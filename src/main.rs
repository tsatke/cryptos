#![no_std]
#![no_main]
#![feature(alloc_error_handler)]
#![feature(abi_x86_interrupt)]
#![feature(maybe_uninit_slice)]
#![allow(unused_imports)]

extern crate alloc;

pub mod acpi_impl;
pub mod ahci;
pub mod apic_impl;
pub mod cralloc;
pub mod exceptions;
pub mod hmfs;
pub mod interrupts;

use ahci::hba::{structs::InterruptError, EIO_DEBUG, EIO_STATUS};
use bootloader_api::{*, config::{Mapping, Mappings, FrameBuffer}, info::FrameBufferInfo};
use acpi::{
    sdt::{Signature, SdtHeader}, AcpiError, AcpiHandler, AcpiTables, HpetInfo, InterruptModel,
    PciConfigRegions, PhysicalMapping, PlatformInfo, RsdpError, fadt::Fadt,
};
use alloc::{
    vec::Vec,
    boxed::Box,
    format,
    sync::Arc, string::String
};
use aml::{
    pci_routing::{Pin, PciRoutingTable},
    LevelType, AmlContext, AmlValue, value::Args, AmlName,
};
use conquer_once::spin::OnceCell;
use crate::{
    acpi_impl::KernelAcpi,
    ahci::Disk, interrupts::IDT,

};
use pcics::header::{Header, InterruptPin, HeaderType};
use printk::LockedPrintk;
use core::{
    alloc::Layout,
    any::TypeId,
    fmt::Write,
    iter::Copied,
    marker::PhantomData,
    mem::MaybeUninit,
    ops::{Add, AddAssign, BitAnd, BitOr, Div, DivAssign, Mul, MulAssign, Not, Sub, SubAssign},
    panic::PanicInfo,
    ptr::{addr_of, addr_of_mut, read_volatile, write_volatile, NonNull},
};
use cralloc::{
    frames::{map_memory, Falloc},
    heap_init,
};
use log::{debug, error, info};
use spin::{Mutex, RwLock};
use x86_64::{
    structures::paging::{FrameAllocator, Mapper, OffsetPageTable, PhysFrame, Page, PageTableFlags, Size4KiB, Size2MiB},
    PhysAddr, VirtAddr,
};
use xmas_elf::ElfFile;

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    error!("Kernel panic -- not syncing: {info}");
    loop {
        unsafe { 
            core::arch::asm!("cli");
            core::arch::asm!("hlt")
        };
    }
}

const MAPPINGS: Mappings = {
    let mut mappings = Mappings::new_default();
    mappings.kernel_stack = Mapping::FixedAddress(0xf000_0000_0000);
    mappings.boot_info = Mapping::Dynamic;
    mappings.framebuffer = Mapping::Dynamic;
    mappings.physical_memory = Some(Mapping::Dynamic);
    mappings.page_table_recursive = None;
    mappings.aslr = false;
    mappings.dynamic_range_start = Some(0);
    mappings.dynamic_range_end = Some(0xffff_ffff_ffff);
    mappings
};

const CONFIG: BootloaderConfig = {
    let mut config = BootloaderConfig::new_default();
    config.mappings = MAPPINGS;
    config.frame_buffer = FrameBuffer::new_default();
    config
};

/// Creates a page-aligned size of something by creating a test page at a given address
///
pub fn page_align(size: u64, addr: u64) -> usize {
    let test = Page::<Size4KiB>::containing_address(VirtAddr::new(addr));
    let test_size = test.size() as usize;

    (((size as usize) - 1) / test_size + 1) * test_size
}

pub static PRINTK: OnceCell<LockedPrintk> = OnceCell::uninit();

// override compiler's pickiness about raw pointers not implementing Send
pub struct SendRawPointer<T>(*mut T);

impl<T> SendRawPointer<T> {
    pub fn new(inner: *mut T) -> Self {
        Self(inner)
    }

    pub unsafe fn get(&self) -> *mut T {
        self.0
    }
}

unsafe impl<T> Send for SendRawPointer<T> {}

// Needed to allow page/frame allocation outside of the entry point, by things like the ACPI handler
pub static MAPPER: OnceCell<Mutex<OffsetPageTable>> = OnceCell::uninit();
pub static FRAME_ALLOCATOR: OnceCell<Mutex<Falloc>> = OnceCell::uninit();

pub fn get_next_usable_frame() -> PhysFrame {
    FRAME_ALLOCATOR
        .get()
        .as_ref()
        .unwrap()
        .lock()
        .usable()
        .next()
        .clone()
        .expect("Out of memory")
        .clone()
}

pub static PHYS_OFFSET: OnceCell<u64> = OnceCell::uninit();

/// Convenient wrapper for getting the physical memory offset
///
/// Safety: only call this if you know that the OnceCell holding the physical memory offset has been initialized
pub unsafe fn get_phys_offset() -> u64 {
    PHYS_OFFSET.get().clone().unwrap().clone()
}

pub static INTERRUPT_MODEL: OnceCell<InterruptModel> = OnceCell::uninit();
pub static PCI_CONFIG: OnceCell<Option<PciConfigRegions>> = OnceCell::uninit();

pub fn get_mcfg() -> Option<PciConfigRegions> {
    PCI_CONFIG.get().clone().unwrap().clone()
}

/// Function which retrieves a debug string for handling I/O errors
///
/// TODO: register this function as a system call
pub fn eio_debug() -> (Option<String>, Option<InterruptError>) {
    let msg = match EIO_DEBUG.read().clone() {
        Some(s) => Some(s.clone()),
        None => None,
    };
    let internal = match EIO_STATUS.read().clone() {
        Some(err) => Some(err.clone()),
        None => None,
    };
    (msg, internal)
}

/// Returns an Iterator of all possible `Option<u64>` in the PCIe extended address space
///
/// Use the `.filter(|i| i.is_some())` method of the resulting iterator to get the PCI devices present on the system
pub fn mcfg_brute_force() -> impl Iterator<Item = Option<u64>> {
    (0x0u32..0x00ffffffu32).map(|i: u32| match get_mcfg() {
        Some(mcfg) => match mcfg.physical_address(
            i.to_be_bytes()[0] as u16,
            i.to_be_bytes()[1],
            i.to_be_bytes()[2],
            i.to_be_bytes()[3],
        ) {
            Some(addr) => Some(addr),
            None => None,
        },
        None => None,
    })
}

pub fn aml_init(tables: &mut AcpiTables<KernelAcpi>) -> Option<[(u32, InterruptPin); 4]> {
    let mut aml_ctx = AmlContext::new(Box::new(KernelAcpi), aml::DebugVerbosity::Scopes);

    let fadt = unsafe { &mut tables.get_sdt::<Fadt>(Signature::FADT).unwrap().unwrap() };

    // Properly reintroduce the size/length of the header
    let dsdt_addr = fadt.dsdt_address().unwrap();
    info!("DSDT address: {:#x}", dsdt_addr.clone());
    let dsdt_len = tables.dsdt.as_ref().unwrap().length.clone() as usize;

    let aml_test_page =
        Page::<Size4KiB>::containing_address(VirtAddr::new(dsdt_addr.clone() as u64));
    let aml_virt = aml_test_page.start_address().as_u64() + unsafe { get_phys_offset() };

    map_page!(
        dsdt_addr,
        aml_virt,
        Size4KiB,
        PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::NO_CACHE
    );

    let raw_table = unsafe {
        core::slice::from_raw_parts_mut(
            aml_virt as *mut u8,
            dsdt_len.clone() + core::mem::size_of::<SdtHeader>(),
        )
    };

    if let Ok(()) = aml_ctx.initialize_objects() {
        if let Ok(()) =
            aml_ctx.parse_table(&raw_table.split_at_mut(core::mem::size_of::<SdtHeader>()).1)
        {
            let _ = aml_ctx.invoke_method(
                &AmlName::from_str("\\_PIC").unwrap(),
                Args([
                    Some(AmlValue::Integer(1)),
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                ]),
            );

            if let Ok(prt) = PciRoutingTable::from_prt_path(
                &AmlName::from_str("\\_SB.PCI0._PRT").unwrap(),
                &mut aml_ctx,
            ) {
                let mut a: [(u32, InterruptPin); 4] = [
                    (0, InterruptPin::IntA),
                    (0, InterruptPin::IntB),
                    (0, InterruptPin::IntC),
                    (0, InterruptPin::IntD),
                ];
                if let Ok(desc) = prt.route(0x1, 0x6, Pin::IntA, &mut aml_ctx) {
                    debug!("IRQ descriptor A: {:#?}", desc);
                    a[0] = (desc.irq, InterruptPin::IntA);
                }
                if let Ok(desc) = prt.route(0x1, 0x6, Pin::IntB, &mut aml_ctx) {
                    debug!("IRQ descriptor B: {:#?}", desc);
                    a[1] = (desc.irq, InterruptPin::IntB);
                }
                if let Ok(desc) = prt.route(0x1, 0x6, Pin::IntC, &mut aml_ctx) {
                    debug!("IRQ descriptor C: {:#?}", desc);
                    a[2] = (desc.irq, InterruptPin::IntC);
                }
                if let Ok(desc) = prt.route(0x1, 0x6, Pin::IntD, &mut aml_ctx) {
                    debug!("IRQ descriptor D: {:#?}", desc);
                    a[3] = (desc.irq, InterruptPin::IntD);
                }
                return Some(a);
            }
            return None;
        }
        return None;
    }
    None
}

pub fn printk_init(buffer: &'static mut [u8], info: FrameBufferInfo) {
    let p = PRINTK.get_or_init(move || LockedPrintk::new(buffer, info));
    log::set_logger(p).expect("Logger has already been set!");

    // Don't flood users with excessive messages if compiled with "--release"
    if cfg!(opt_level = "0") {
        log::set_max_level(log::LevelFilter::Trace);
    } else {
        log::set_max_level(log::LevelFilter::Info);
    }
    info!("CryptOS v. 0.1.1-alpha");
}

pub static ALL_DISKS: OnceCell<RwLock<Vec<Box<dyn Disk + Send + Sync>>>> = OnceCell::uninit();

entry_point!(maink, config = &CONFIG);

pub fn maink(boot_info: &'static mut BootInfo) -> ! {
    // set up heap allocation ASAP
    let offset = VirtAddr::new(
        boot_info
            .physical_memory_offset
            .clone()
            .into_option()
            .unwrap(),
    );
    let map = unsafe { map_memory(offset) };
    let falloc = unsafe { Falloc::new(&boot_info.memory_regions) };

    MAPPER.get_or_init(move || Mutex::new(map));
    FRAME_ALLOCATOR.get_or_init(move || Mutex::new(falloc));

    heap_init(
        &mut *MAPPER.get().unwrap().lock(),
        &mut *FRAME_ALLOCATOR.get().unwrap().lock(),
    )
    .unwrap_or_else(|e| panic!("Failed to initialize heap: {:#?}", e));

    // clone the physical memory offset into a static ASAP
    // so it doesn't need to be hardcoded everywhere it's needed
    let cloned_offset = boot_info
        .physical_memory_offset
        .clone()
        .into_option()
        .unwrap();
    PHYS_OFFSET.get_or_init(move || cloned_offset);

    let buffer_optional = &mut boot_info.framebuffer;
    let buffer_option = buffer_optional.as_mut();
    let buffer = buffer_option.unwrap();
    let bi = buffer.info().clone();
    let raw_buffer = buffer.buffer_mut();

    let rsdp = boot_info.rsdp_addr.clone().into_option().unwrap();
    printk_init(raw_buffer, bi);
    info!(
        "Using version {}.{}.{} of crates.io/crates/bootloader",
        boot_info.api_version.version_major(),
        boot_info.api_version.version_minor(),
        boot_info.api_version.version_patch()
    );

    info!("RSDP address: {:#x}", rsdp.clone());
    info!(
        "Memory region start address: {:#x}",
        &boot_info.memory_regions.first().unwrap() as *const _ as usize
    );

    let mut tables = unsafe { AcpiTables::from_rsdp(KernelAcpi, rsdp.clone() as usize).unwrap() };
    let mcfg = match PciConfigRegions::new(&tables) {
        Ok(mcfg) => Some(mcfg),
        Err(_) => None,
    };

    let interrupts = tables.platform_info().unwrap().interrupt_model;

    INTERRUPT_MODEL.get_or_init(move || interrupts);
    PCI_CONFIG.get_or_init(move || mcfg.clone());

    debug!("Interrupt model: {:#?}", INTERRUPT_MODEL.get().unwrap());

    debug!("TLS template: {:#?}", boot_info.tls_template);
    debug!("PCI Configuration Regions: {:#x?}", get_mcfg());

    for dev in mcfg_brute_force()
        .filter(|i| i.is_some())
        .map(|i| i.unwrap())
    {
        let test_page = Page::<Size4KiB>::containing_address(VirtAddr::new(dev));

        let virt = test_page.start_address().as_u64() + unsafe { get_phys_offset() };

        map_page!(
            dev,
            virt,
            Size4KiB,
            PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::NO_CACHE
        );

        let raw_header = unsafe { *(virt as *const [u8; 64]) };
        let header = Header::from(raw_header);

        if header.class_code.base == 0x01 && header.class_code.sub == 0x06 {
            info!(
                "Found AHCI controller {:x}:{:x} at {:#x}",
                header.vendor_id, header.device_id, dev
            );
            info!("Class Code: {:#x?}", header.class_code);

            let arr = aml_init(&mut tables);

            if arr.is_some() {
                match header.interrupt_pin {
                    InterruptPin::IntA => {
                        IDT.lock()[32 + arr.unwrap()[0].0 as usize].set_handler_fn(interrupts::ahci)
                    }
                    InterruptPin::IntB => {
                        IDT.lock()[32 + arr.unwrap()[1].0 as usize].set_handler_fn(interrupts::ahci)
                    }
                    InterruptPin::IntC => {
                        IDT.lock()[32 + arr.unwrap()[2].0 as usize].set_handler_fn(interrupts::ahci)
                    }
                    InterruptPin::IntD => {
                        IDT.lock()[32 + arr.unwrap()[3].0 as usize].set_handler_fn(interrupts::ahci)
                    }
                    _ => panic!("Invalid interrupt pin"),
                };
            }

            crate::interrupts::init();

            info!("Interrupt pin: {:#?}", header.interrupt_pin);

            if let HeaderType::Normal(normal_header) = header.header_type {
                let abar = normal_header.base_addresses.orig()[5];
                let abar_test_page =
                    Page::<Size2MiB>::containing_address(VirtAddr::new(abar as u64));
                let abar_virt =
                    abar_test_page.start_address().as_u64() + unsafe { get_phys_offset() };

                map_page!(
                    abar,
                    abar_virt,
                    Size4KiB,
                    PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::NO_CACHE
                );

                let (_, disks) = ahci::all_disks(abar_virt as usize);
                info!("Found {:#?} disks", disks.len());
                ALL_DISKS.get_or_init(move || RwLock::new(disks));
            }

            break;
        }
    }

    loop {
        unsafe { 
            core::arch::asm!("cli");
            core::arch::asm!("hlt");
        };
    }
}

#[alloc_error_handler]
fn alloc_err(_layout: Layout) -> ! {
    panic!("Out of memory!")
}