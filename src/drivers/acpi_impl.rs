use core::todo;

use acpi::{
    bgrt::Bgrt,
    fadt::Fadt,
    hpet::HpetTable,
    madt::Madt,
    mcfg::{Mcfg, McfgEntry},
    sdt::SdtHeader,
    AmlTable, Sdt,
};
use alloc::{alloc::Global, collections::BTreeMap};
use aml::{
    pci_routing::{PciRoutingTable, Pin},
    value::Args,
    AmlName, AmlValue,
};
use log::{debug, info};
use pcics::{header::InterruptPin, Header};
use x86_64::{instructions::port::Port, structures::paging::FrameAllocator};

use crate::arch::x86_64::interrupts::{INTA_IRQ, INTB_IRQ, INTC_IRQ, INTD_IRQ};

use {
    crate::{get_phys_offset, map_page},
    acpi::{
        sdt::Signature, AcpiError, AcpiHandler, AcpiTables, HpetInfo, InterruptModel,
        PciConfigRegions, PhysicalMapping, PlatformInfo,
    },
    alloc::boxed::Box,
    alloc::sync::Arc,
    alloc::vec::Vec,
    aml::AmlContext,
    conquer_once::spin::OnceCell,
    core::{
        arch::asm,
        ptr::NonNull,
        sync::atomic::{AtomicU64, Ordering},
    },
    spin::RwLock,
    x86_64::{
        structures::paging::{
            mapper::MapToError, Mapper, OffsetPageTable, Page, PageTableFlags, PhysFrame, Size4KiB,
        },
        PhysAddr, VirtAddr,
    },
};

/// Creates a page-aligned size of something by creating a test page at a given address
///
pub fn page_align(size: u64, addr: u64) -> usize {
    let test = Page::<Size4KiB>::containing_address(VirtAddr::new(addr));
    let test_size = test.size() as usize;

    (((size as usize) - 1) / test_size + 1) * test_size
}

#[derive(Clone)]
pub struct KernelAcpi;

impl AcpiHandler for KernelAcpi {
    unsafe fn map_physical_region<T>(
        &self,
        physical_address: usize,
        size: usize,
    ) -> PhysicalMapping<Self, T> {
        let test = Page::<Size4KiB>::containing_address(VirtAddr::new(
            physical_address as u64 + get_phys_offset(),
        ));
        let virtual_address = test.start_address().as_u64();

        // now that we handle the PageAlreadyMapped and ParentEntryHugePage errors properly, i.e. without panicking
        map_page!(
            physical_address,
            virtual_address,
            Size4KiB,
            PageTableFlags::PRESENT
                | PageTableFlags::WRITABLE
                | PageTableFlags::NO_CACHE
                | PageTableFlags::WRITE_THROUGH
        );

        PhysicalMapping::new(
            physical_address,
            NonNull::new(virtual_address as *mut T).unwrap(), //page must exist
            size,
            page_align(size as u64, virtual_address),
            Self,
        )
    }

    fn unmap_physical_region<T>(_region: &PhysicalMapping<Self, T>) {}
}

impl aml::Handler for KernelAcpi {
    fn read_u8(&self, address: usize) -> u8 {
        let test = Page::<Size4KiB>::containing_address(VirtAddr::new(address as u64));
        let test_start = test.start_address().as_u64();

        map_page!(
            address,
            test_start + get_phys_offset() + test.size(),
            Size4KiB,
            PageTableFlags::PRESENT
                | PageTableFlags::WRITABLE
                | PageTableFlags::NO_CACHE
                | PageTableFlags::WRITE_THROUGH
        );

        unsafe { core::ptr::read_volatile((address + get_phys_offset() as usize) as *const u8) }
    }

    fn read_u16(&self, address: usize) -> u16 {
        let test = Page::<Size4KiB>::containing_address(VirtAddr::new(address as u64));
        let test_start = test.start_address().as_u64();

        map_page!(
            address,
            test_start + get_phys_offset() + test.size(),
            Size4KiB,
            PageTableFlags::PRESENT
                | PageTableFlags::WRITABLE
                | PageTableFlags::NO_CACHE
                | PageTableFlags::WRITE_THROUGH
        );

        unsafe { core::ptr::read_volatile((address + get_phys_offset() as usize) as *const u16) }
    }

    fn read_u32(&self, address: usize) -> u32 {
        let test = Page::<Size4KiB>::containing_address(VirtAddr::new(address as u64));
        let test_start = test.start_address().as_u64();

        map_page!(
            address,
            test_start + get_phys_offset() + test.size(),
            Size4KiB,
            PageTableFlags::PRESENT
                | PageTableFlags::WRITABLE
                | PageTableFlags::NO_CACHE
                | PageTableFlags::WRITE_THROUGH
        );

        unsafe { core::ptr::read_volatile((address + get_phys_offset() as usize) as *const u32) }
    }

    fn read_u64(&self, address: usize) -> u64 {
        let test = Page::<Size4KiB>::containing_address(VirtAddr::new(address as u64));
        let test_start = test.start_address().as_u64();

        map_page!(
            address,
            test_start + get_phys_offset() + test.size(),
            Size4KiB,
            PageTableFlags::PRESENT
                | PageTableFlags::WRITABLE
                | PageTableFlags::NO_CACHE
                | PageTableFlags::WRITE_THROUGH
        );

        unsafe { core::ptr::read_volatile((address + get_phys_offset() as usize) as *const u64) }
    }

    fn write_u8(&mut self, address: usize, value: u8) {
        let test = Page::<Size4KiB>::containing_address(VirtAddr::new(address as u64));
        let test_start = test.start_address().as_u64();

        map_page!(
            address,
            test_start + get_phys_offset() + test.size(),
            Size4KiB,
            PageTableFlags::PRESENT
                | PageTableFlags::WRITABLE
                | PageTableFlags::NO_CACHE
                | PageTableFlags::WRITE_THROUGH
        );

        unsafe {
            core::ptr::write_volatile((address + get_phys_offset() as usize) as *mut u8, value)
        }
    }

    fn write_u16(&mut self, address: usize, value: u16) {
        let test = Page::<Size4KiB>::containing_address(VirtAddr::new(address as u64));
        let test_start = test.start_address().as_u64();

        map_page!(
            address,
            test_start + get_phys_offset() + test.size(),
            Size4KiB,
            PageTableFlags::PRESENT
                | PageTableFlags::WRITABLE
                | PageTableFlags::NO_CACHE
                | PageTableFlags::WRITE_THROUGH
        );

        unsafe {
            core::ptr::write_volatile((address + get_phys_offset() as usize) as *mut u16, value)
        }
    }

    fn write_u32(&mut self, address: usize, value: u32) {
        let test = Page::<Size4KiB>::containing_address(VirtAddr::new(address as u64));
        let test_start = test.start_address().as_u64();

        map_page!(
            address,
            test_start + get_phys_offset() + test.size(),
            Size4KiB,
            PageTableFlags::PRESENT
                | PageTableFlags::WRITABLE
                | PageTableFlags::NO_CACHE
                | PageTableFlags::WRITE_THROUGH
        );

        unsafe {
            core::ptr::write_volatile((address + get_phys_offset() as usize) as *mut u32, value)
        }
    }

    fn write_u64(&mut self, address: usize, value: u64) {
        let test = Page::<Size4KiB>::containing_address(VirtAddr::new(address as u64));
        let test_start = test.start_address().as_u64();
        map_page!(
            address,
            test_start + get_phys_offset() + test.size(),
            Size4KiB,
            PageTableFlags::PRESENT
                | PageTableFlags::WRITABLE
                | PageTableFlags::NO_CACHE
                | PageTableFlags::WRITE_THROUGH
        );

        unsafe {
            core::ptr::write_volatile((address + get_phys_offset() as usize) as *mut u64, value)
        }
    }

    fn read_io_u8(&self, port: u16) -> u8 {
        let res: u8;
        unsafe {
            asm!("in al, dx", in("dx") port, out("al") res);
        }
        return res;
    }

    fn read_io_u16(&self, port: u16) -> u16 {
        let res: u16;
        unsafe {
            asm!("in ax, dx", in("dx") port, out("ax") res);
        }
        return res;
    }

    fn read_io_u32(&self, port: u16) -> u32 {
        let res: u32;
        unsafe {
            asm!("in eax, dx", in("dx") port, out("eax") res);
        }
        return res;
    }

    fn write_io_u8(&self, port: u16, value: u8) {
        unsafe {
            asm!("out dx, al", in("dx") port, in("al") value);
        }
    }

    fn write_io_u16(&self, port: u16, value: u16) {
        unsafe {
            asm!("out dx, ax", in("dx") port, in("ax") value);
        }
    }

    fn write_io_u32(&self, port: u16, value: u32) {
        unsafe {
            asm!("out dx, eax", in("dx") port, in("eax") value);
        }
    }

    fn read_pci_u8(&self, segment: u16, bus: u8, device: u8, function: u8, offset: u16) -> u8 {
        let seg_bytes = segment.to_be_bytes();
        let offs_bytes = offset.to_be_bytes();
        let rebuilt = u64::from_be_bytes([
            0,
            seg_bytes[0],
            seg_bytes[1],
            bus,
            device,
            function,
            offs_bytes[0],
            offs_bytes[1],
        ]);

        let test = Page::<Size4KiB>::containing_address(VirtAddr::new(rebuilt));
        let test_start = test.start_address().as_u64();

        map_page!(
            rebuilt,
            test_start + get_phys_offset() + test.size(),
            Size4KiB,
            PageTableFlags::PRESENT
                | PageTableFlags::WRITABLE
                | PageTableFlags::NO_CACHE
                | PageTableFlags::WRITE_THROUGH
        );

        unsafe { core::ptr::read_volatile(((rebuilt + get_phys_offset()) as usize) as *const u8) }
    }

    fn read_pci_u16(&self, segment: u16, bus: u8, device: u8, function: u8, offset: u16) -> u16 {
        let seg_bytes = segment.to_be_bytes();
        let offs_bytes = offset.to_be_bytes();
        let rebuilt = u64::from_be_bytes([
            0,
            seg_bytes[0],
            seg_bytes[1],
            bus,
            device,
            function,
            offs_bytes[0],
            offs_bytes[1],
        ]);

        let test = Page::<Size4KiB>::containing_address(VirtAddr::new(rebuilt));
        let test_start = test.start_address().as_u64();

        map_page!(
            rebuilt,
            test_start + get_phys_offset() + test.size(),
            Size4KiB,
            PageTableFlags::PRESENT
                | PageTableFlags::WRITABLE
                | PageTableFlags::NO_CACHE
                | PageTableFlags::WRITE_THROUGH
        );

        unsafe { core::ptr::read_volatile(((rebuilt + get_phys_offset()) as usize) as *const u16) }
    }

    fn read_pci_u32(&self, segment: u16, bus: u8, device: u8, function: u8, offset: u16) -> u32 {
        let seg_bytes = segment.to_be_bytes();
        let offs_bytes = offset.to_be_bytes();
        let rebuilt = u64::from_be_bytes([
            0,
            seg_bytes[0],
            seg_bytes[1],
            bus,
            device,
            function,
            offs_bytes[0],
            offs_bytes[1],
        ]);

        let test = Page::<Size4KiB>::containing_address(VirtAddr::new(rebuilt));
        let test_start = test.start_address().as_u64();

        map_page!(
            rebuilt,
            test_start + get_phys_offset() + test.size(),
            Size4KiB,
            PageTableFlags::PRESENT
                | PageTableFlags::WRITABLE
                | PageTableFlags::NO_CACHE
                | PageTableFlags::WRITE_THROUGH
        );

        unsafe { core::ptr::read_volatile(((rebuilt + get_phys_offset()) as usize) as *const u32) }
    }

    fn write_pci_u8(
        &self,
        segment: u16,
        bus: u8,
        device: u8,
        function: u8,
        offset: u16,
        value: u8,
    ) {
        let seg_bytes = segment.to_be_bytes();
        let offs_bytes = offset.to_be_bytes();
        let rebuilt = u64::from_be_bytes([
            0,
            seg_bytes[0],
            seg_bytes[1],
            bus,
            device,
            function,
            offs_bytes[0],
            offs_bytes[1],
        ]);

        let test = Page::<Size4KiB>::containing_address(VirtAddr::new(rebuilt));
        let test_start = test.start_address().as_u64();

        map_page!(
            rebuilt,
            test_start + get_phys_offset() + test.size(),
            Size4KiB,
            PageTableFlags::PRESENT
                | PageTableFlags::WRITABLE
                | PageTableFlags::NO_CACHE
                | PageTableFlags::WRITE_THROUGH
        );

        unsafe {
            core::ptr::write_volatile(((rebuilt + get_phys_offset()) as usize) as *mut u8, value)
        }
    }

    fn write_pci_u16(
        &self,
        segment: u16,
        bus: u8,
        device: u8,
        function: u8,
        offset: u16,
        value: u16,
    ) {
        let seg_bytes = segment.to_be_bytes();
        let offs_bytes = offset.to_be_bytes();
        let rebuilt = u64::from_be_bytes([
            0,
            seg_bytes[0],
            seg_bytes[1],
            bus,
            device,
            function,
            offs_bytes[0],
            offs_bytes[1],
        ]);

        let test = Page::<Size4KiB>::containing_address(VirtAddr::new(rebuilt));
        let test_start = test.start_address().as_u64();

        map_page!(
            rebuilt,
            test_start + get_phys_offset() + test.size(),
            Size4KiB,
            PageTableFlags::PRESENT
                | PageTableFlags::WRITABLE
                | PageTableFlags::NO_CACHE
                | PageTableFlags::WRITE_THROUGH
        );

        unsafe {
            core::ptr::write_volatile(((rebuilt + get_phys_offset()) as usize) as *mut u16, value)
        }
    }

    fn write_pci_u32(
        &self,
        segment: u16,
        bus: u8,
        device: u8,
        function: u8,
        offset: u16,
        value: u32,
    ) {
        let seg_bytes = segment.to_be_bytes();
        let offs_bytes = offset.to_be_bytes();
        let rebuilt = u64::from_be_bytes([
            0,
            seg_bytes[0],
            seg_bytes[1],
            bus,
            device,
            function,
            offs_bytes[0],
            offs_bytes[1],
        ]);

        let test = Page::<Size4KiB>::containing_address(VirtAddr::new(rebuilt));
        let test_start = test.start_address().as_u64();

        map_page!(
            rebuilt,
            test_start + get_phys_offset() + test.size(),
            Size4KiB,
            PageTableFlags::PRESENT
                | PageTableFlags::WRITABLE
                | PageTableFlags::NO_CACHE
                | PageTableFlags::WRITE_THROUGH
        );

        unsafe {
            core::ptr::write_volatile(((rebuilt + get_phys_offset()) as usize) as *mut u32, value)
        }
    }

    fn stall(&self, _microseconds: u64) {
        unimplemented!()
    }

    fn sleep(&self, _milliseconds: u64) {
        unimplemented!()
    }
}

unsafe impl Send for KernelAcpi {}
unsafe impl Sync for KernelAcpi {}

pub(crate) static AML_CONTEXT: OnceCell<Arc<RwLock<AmlContext>>> = OnceCell::uninit();
pub(crate) static DSDT_MAPPED: AtomicU64 = AtomicU64::new(0);
pub(crate) static FADT: OnceCell<Arc<RwLock<Fadt>>> = OnceCell::uninit();

pub fn aml_init(tables: &mut AcpiTables<KernelAcpi>) {
    info!("Parsing AML");
    let mut aml_ctx = AmlContext::new(Box::new(KernelAcpi), aml::DebugVerbosity::Scopes);

    let fadt = &mut tables.find_table::<Fadt>().unwrap();

    // borrow checker
    let clone = fadt.clone();
    FADT.get_or_init(move || Arc::new(RwLock::new(clone)));

    // Properly reintroduce the size/length of the header
    let dsdt_addr = fadt.dsdt_address().unwrap();
    info!("DSDT address: {:#x}", dsdt_addr.clone());
    let dsdt_len = tables.dsdt().as_ref().unwrap().length.clone() as usize;

    let aml_test_page =
        Page::<Size4KiB>::containing_address(VirtAddr::new(dsdt_addr.clone() as u64));
    let aml_virt = aml_test_page.start_address().as_u64() + get_phys_offset();

    info!("Virtual DSDT address: {:#x}", &aml_virt);

    map_page!(
        dsdt_addr,
        aml_virt,
        Size4KiB,
        PageTableFlags::PRESENT
            | PageTableFlags::WRITABLE
            | PageTableFlags::NO_CACHE
            | PageTableFlags::WRITE_THROUGH
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
            // Make sure AML knows that the APIC, not the legacy PIC, is what's being used
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

            // Check the SMI command port
            let smi_cmd = fadt.smi_cmd_port;
            let acpi_en = fadt.acpi_enable;

            info!("SMI command port: {:#x?}", smi_cmd);
            info!("ACPI enable value: {:?}", acpi_en);

            let pm1a_block = match fadt.pm1a_control_block() {
                Ok(block) => Some(block.address),
                Err(_) => None,
            };

            if let Some(addr) = pm1a_block {
                info!("Found PM1a control address: {:#x?}", addr);
            }

            let pm1b_block = match fadt.pm1b_control_block() {
                Ok(block_opt) => {
                    if let Some(block) = block_opt {
                        Some(block.address)
                    } else {
                        None
                    }
                }
                Err(_) => None,
            };

            if let Some(addr) = pm1b_block {
                info!("Found PM1b control address: {:?}", addr);
            }

            let no_value = [None, None, None, None, None, None, None];

            if let Ok(pkg) = aml_ctx.invoke_method(
                &AmlName::from_str("\\_S5")
                    .unwrap_or_else(|e| panic!("Failed to execute method: {:?}", e)),
                Args(no_value),
            ) {
                info!("S5 Controls: {:#?}", pkg)
            };

            let mut smi_port = Port::new(smi_cmd as u16);
            unsafe { smi_port.write(acpi_en) };

            AML_CONTEXT.get_or_init(move || Arc::new(RwLock::new(aml_ctx)));
            DSDT_MAPPED.store(aml_virt, Ordering::SeqCst);
        }
    }
}

pub fn aml_route(header: &mut Header) -> Option<[(u32, InterruptPin); 4]> {
    let aml_clone = Arc::clone(AML_CONTEXT.get().expect("AML context failed to initialize"));
    let mut aml_ctx = aml_clone.write();

    if let Ok(prt) =
        PciRoutingTable::from_prt_path(&AmlName::from_str("\\_SB.PCI0._PRT").unwrap(), &mut aml_ctx)
    {
        let mut a: [(u32, InterruptPin); 4] = [
            (0, InterruptPin::IntA),
            (0, InterruptPin::IntB),
            (0, InterruptPin::IntC),
            (0, InterruptPin::IntD),
        ];
        if let Ok(desc) = prt.route(
            header.class_code.base as u16,
            header.class_code.base as u16,
            Pin::IntA,
            &mut aml_ctx,
        ) {
            debug!("IntA IRQ number: {:#?}", desc.irq.clone());
            INTA_IRQ.store((desc.irq.clone() + 32) as u64, Ordering::SeqCst);
            a[0] = (desc.irq, InterruptPin::IntA);
        }
        if let Ok(desc) = prt.route(
            header.class_code.base as u16,
            header.class_code.base as u16,
            Pin::IntB,
            &mut aml_ctx,
        ) {
            debug!("IntB IRQ number: {:#?}", desc.irq.clone());
            INTB_IRQ.store((desc.irq.clone() + 32) as u64, Ordering::SeqCst);
            a[1] = (desc.irq, InterruptPin::IntB);
        }
        if let Ok(desc) = prt.route(
            header.class_code.base as u16,
            header.class_code.base as u16,
            Pin::IntC,
            &mut aml_ctx,
        ) {
            debug!("IntC IRQ number: {:#?}", desc.irq.clone());
            INTC_IRQ.store((desc.irq.clone() + 32) as u64, Ordering::SeqCst);
            a[2] = (desc.irq, InterruptPin::IntC);
        }
        if let Ok(desc) = prt.route(
            header.class_code.base as u16,
            header.class_code.base as u16,
            Pin::IntD,
            &mut aml_ctx,
        ) {
            debug!("IntD IRQ number: {:#?}", desc.irq.clone());
            INTD_IRQ.store((desc.irq.clone() + 32) as u64, Ordering::SeqCst);
            a[3] = (desc.irq, InterruptPin::IntD);
        }
        return Some(a);
    }
    return None;
}

// Needed for cloning the ACPI tables into an abstraction for usermode use
pub struct UserAcpi {
    pub bgrt: Bgrt,
    pub fadt: Fadt,
    pub hpet: HpetTable,
    pub madt: Madt,
    pub mcfg: Vec<McfgEntry>,
    pub dsdt: Option<AmlTable>,
    pub ssdts: Vec<AmlTable>,
}

impl UserAcpi {
    pub fn new(tables: &mut AcpiTables<KernelAcpi>) -> Self {
        Self {
            bgrt: tables
                .find_table::<Bgrt>()
                .unwrap_or_else(|e| panic!("Failed to find BGRT table: {:#?}", e))
                .clone(),
            fadt: tables
                .find_table::<Fadt>()
                .unwrap_or_else(|e| panic!("Failed to find FADT table: {:#?}", e))
                .clone(),
            hpet: tables
                .find_table::<HpetTable>()
                .unwrap_or_else(|e| panic!("Failed to find HPET table: {:#?}", e))
                .clone(),
            madt: tables
                .find_table::<Madt>()
                .unwrap_or_else(|e| panic!("Failed to find MADT table: {:#?}", e))
                .clone(),
            mcfg: tables
                .find_table::<Mcfg>()
                .unwrap_or_else(|e| panic!("Failed to find MCFG table: {:#?}", e))
                .entries()
                .iter()
                .map(|&entry| entry.clone())
                .collect::<Vec<_>>(),
            dsdt: if let Ok(dsdt) = tables.dsdt() {
                Some(AmlTable {
                    address: dsdt.address,
                    length: dsdt.length,
                })
            } else {
                None
            },
            ssdts: {
                let mut v = Vec::new();
                for ssdt in tables.ssdts() {
                    v.push(AmlTable {
                        address: ssdt.address,
                        length: ssdt.length,
                    });
                }
                v
            },
        }
    }
}

impl Clone for UserAcpi {
    fn clone(&self) -> Self {
        Self {
            bgrt: self.bgrt.clone(),
            fadt: self.fadt.clone(),
            hpet: self.hpet.clone(),
            madt: self.madt.clone(),
            mcfg: self.mcfg.clone(),
            dsdt: if let Some(dsdt) = &self.dsdt {
                Some(AmlTable {
                    address: dsdt.address,
                    length: dsdt.length,
                })
            } else {
                None
            },
            ssdts: {
                let mut v = Vec::new();
                for table in self.ssdts.iter() {
                    v.push(AmlTable {
                        address: table.address,
                        length: table.length,
                    });
                }
                v
            },
        }
    }
}

unsafe impl Send for UserAcpi {}
unsafe impl Sync for UserAcpi {}

/// Invokes the ACPI shutdown command
pub fn system_shutdown() -> ! {
    let aml_clone = Arc::clone(AML_CONTEXT.get().expect("AML context failed to initialize"));
    let mut aml_ctx = aml_clone.write();

    let _ = aml_ctx.invoke_method(
        &AmlName::from_str("\\_PTS").unwrap(),
        Args([
            Some(AmlValue::Integer(5)),
            None,
            None,
            None,
            None,
            None,
            None,
        ]),
    );

    let fadt_lock = Arc::clone(&FADT.get().unwrap());
    let fadt = fadt_lock.write();

    let pm1a_block = match fadt.pm1a_control_block() {
        Ok(block) => Some(block.address),
        Err(_) => None,
    };

    let pm1b_block = match fadt.pm1b_control_block() {
        Ok(block_opt) => {
            if let Some(block) = block_opt {
                Some(block.address)
            } else {
                None
            }
        }
        Err(_) => None,
    };

    let no_value = [None, None, None, None, None, None, None];

    if let Ok(pkg) = aml_ctx.invoke_method(
        &AmlName::from_str("\\_S5").unwrap_or_else(|e| panic!("Failed to execute method: {:?}", e)),
        Args(no_value),
    ) {
        if let AmlValue::Package(pkg) = pkg {
            if let Some(pm1a) = pm1a_block {
                let mut p = Port::new(pm1a as u16);

                if let AmlValue::Integer(value) = pkg[0] {
                    let sleep_a = value;
                    let out = (sleep_a | 1 << 13) as u16;

                    unsafe { p.write(out) };
                }

                if let Some(pm1b) = pm1b_block {
                    let mut p = Port::new(pm1b as u16);

                    if let AmlValue::Integer(value) = pkg[1] {
                        let sleep_b = value;
                        let out = (sleep_b | 1 << 13) as u16;

                        unsafe { p.write(out) };
                    }
                }
            }
        } else {
            unreachable!()
        }
    };

    unreachable!()
}
