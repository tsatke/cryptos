// SPDX-License-Identifier: GPL-3.0-or-later
// Port of https://github.com/Andy-Python-Programmer/aero/raw/master/src/aero_kernel/src/drivers/block/ahci.rs

#![allow(unused)]

use core::sync::atomic::Ordering;

use acpi::AcpiTables;
use conquer_once::spin::OnceCell;
use pcics::header::{HeaderType, InterruptPin};
use spin::{RwLock, RwLockReadGuard, RwLockWriteGuard};
use x86_64::{
    instructions::interrupts::without_interrupts, registers::control::Cr3,
    structures::paging::FrameAllocator,
};

use crate::{
    acpi_impl::{aml_route, KernelAcpi},
    arch::x86_64::interrupts::{self, IDT},
    cralloc::frames::safe_active_pml4,
    get_phys_offset, map_page, MAPPER,
};

use self::util::sync::MutexGuard;

pub mod util;

use {
    crate::{pci_impl::*, FRAME_ALLOCATOR},
    alloc::{sync::Arc, vec::Vec},
    bit_field::BitField,
    log::*,
    spin::Once,
    util::{sync::Mutex, CeilDiv, VolatileCell},
    x86_64::{
        structures::paging::{
            mapper::MapToError, Mapper, OffsetPageTable, Page, PageTableFlags, PhysFrame, Size2MiB,
            Size4KiB,
        },
        PhysAddr, VirtAddr,
    },
};

static DRIVER: Once<Arc<AhciDriver>> = Once::new();
static BUDDY_SIZE: [u64; 3] = [
    Page::<Size4KiB>::SIZE,
    Page::<Size4KiB>::SIZE * 4,
    Page::<Size2MiB>::SIZE,
];

pub static ABAR: OnceCell<u64> = OnceCell::uninit();

#[repr(usize)]
#[derive(Clone)]
pub enum BuddyOrdering {
    Size4KiB = 0,
    Size8KiB = 2,
}

pub fn pmm_alloc(order: BuddyOrdering) -> PhysAddr {
    // borrow checker
    let copied_order = order.clone();

    let raw_order = order as usize;
    debug_assert!(raw_order <= BUDDY_SIZE.len());

    let frame = FRAME_ALLOCATOR
        .get()
        .expect("Frame allocator not initialized")
        .write()
        .allocate_frame()
        .expect("Out of memory");

    let phys = frame.start_address().as_u64();
    let virt = unsafe { phys + get_phys_offset() };

    if let BuddyOrdering::Size4KiB = copied_order {
        map_page!(
            phys,
            virt,
            Size4KiB,
            PageTableFlags::PRESENT
                | PageTableFlags::WRITABLE
                | PageTableFlags::NO_CACHE
                | PageTableFlags::WRITE_THROUGH
        );
    } else {
        // map 2 pages to take the 8KiB size into account
        map_page!(
            phys,
            virt,
            Size4KiB,
            PageTableFlags::PRESENT
                | PageTableFlags::WRITABLE
                | PageTableFlags::NO_CACHE
                | PageTableFlags::WRITE_THROUGH
        );
        map_page!(
            phys + 4096,
            virt + 4096,
            Size4KiB,
            PageTableFlags::PRESENT
                | PageTableFlags::WRITABLE
                | PageTableFlags::NO_CACHE
                | PageTableFlags::WRITE_THROUGH
        );
    }

    let fill_size = BUDDY_SIZE[raw_order] as usize;
    let slice = unsafe { core::slice::from_raw_parts_mut(virt as *mut u8, fill_size) };

    // We always zero out memory for security reasons.
    slice.fill(0x00);

    PhysAddr::new(phys)
}

bitflags::bitflags! {
    struct HbaEnclosureCtrl: u32 {
        const STS_MR =      1 << 0;  // Message Received
        const CTL_TM =      1 << 8;  // Transmit Message
        const CTL_RST =     1 << 9;  // Reset
        const SUPP_LED =    1 << 16; // LED Message Types
        const SUPP_SAFTE =  1 << 17; // SAF-TE Enclosure Management Messages
        const SUPP_SES2 =   1 << 18; // SES-2 Enclosure Management Messages
        const SUPP_SGPIO =  1 << 19; // SGPIO Enclosure Management Messages
        const ATTR_SMB =    1 << 24; // Single Message Buffer
        const ATTR_XMT =    1 << 25; // Transmit Only
        const ATTR_ALHD =   1 << 26; // Activity LED Hardware Driven
        const ATTR_PM =     1 << 27; // Port Multiplier Support
    }
}

bitflags::bitflags! {
    struct HbaCapabilities2: u32 {
        const BOH   = 1 << 0; // BIOS/OS Handoff
        const NVMP  = 1 << 1; // NVMHCI Present
        const APST  = 1 << 2; // Automatic Partial to Slumber Transitions
        const SDS   = 1 << 3; // Supports Device Sleep
        const SADM  = 1 << 4; // Supports Aggressive Device Sleep management
        const DESO  = 1 << 5; // DevSleep Entrance from Slumber Only
    }
}

bitflags::bitflags! {
    #[derive(Clone, Copy)]
    struct HbaBohc: u32 {
        const BOS =     1 << 0; // BIOS Owned Semaphore
        const OOS =     1 << 1; // OS Owned Semaphore
        const SOOE =    1 << 2; // SMI on OS Ownership Change Enable
        const OOC =     1 << 3; // OS Ownership Change
        const BB =      1 << 4; // BIOS Busy
    }
}

bitflags::bitflags! {
    struct HbaCapabilities: u32 {
        const SXS           = 1 << 5;  // Supports External SATA
        const EMS           = 1 << 6;  // Enclosure Management Supported
        const CCCS          = 1 << 7;  // Command Completion Coalescing Supported
        const PSC           = 1 << 13; // Partial State Capable
        const SSC           = 1 << 14; // Slumber State Capable
        const PMD           = 1 << 15; // PIO Multiple DRQ Block
        const FBSS          = 1 << 16; // FIS-based Switching Supported
        const SPM           = 1 << 17; // Supports Port Multiplier
        const SAM           = 1 << 18; // Supports AHCI mode only
        const SCLO          = 1 << 24; // Supports Command List Override
        const SAL           = 1 << 25; // Supports Activity LED
        const SALP          = 1 << 26; // Supports Aggressive Link Power Mgmt
        const SSS           = 1 << 27; // Supports Staggered Spin-up
        const SMPS          = 1 << 28; // Supports Mechanical Presence Switch
        const SSNTF         = 1 << 29; // Supports SNotification Register
        const SNCQ          = 1 << 30; // Supports Native Command Queuing
        const S64A          = 1 << 31; // Supports 64-bit Addressing
    }
}

bitflags::bitflags! {
    #[derive(Clone, Copy)]
    struct HbaHostCont: u32 {
        const HR =   1 << 0;  // HBA Reset
        const IE =   1 << 1;  // Interrupt Enable
        const MRSM = 1 << 2;  // MSI Revert to Single Message
        const AE =   1 << 31; // AHCI Enable
    }
}

bitflags::bitflags! {
    #[derive(Clone, Copy)]
    pub struct HbaPortIS: u32 {
        const DHRS = 1 << 0; // Device to Host Register FIS Interrupt
        const PSS = 1 << 1; // PIO Setup FIS Interrupt
        const DSS = 1 << 2; // DMA Setup FIS Interrupt
        const SDBS = 1 << 3; // Set Device Bits Interrupt
        const UFS = 1 << 4; // Unknown FIS Interrupt
        const DPS = 1 << 5; // Descriptor Processed
        const PCS = 1 << 6; // Port Connect Change Status
        const DMPS = 1 << 7; // Device Mechanical Presence Status
        const PRCS = 1 << 22; // PhyRdy Change Status
        const IPMS = 1 << 23; // Incorrect Port Multiplier Status
        const OFS = 1 << 24; // Overflow Status
        const INFS = 1 << 26; // Interface Not-fatal Error Status
        const IFS = 1 << 27; // Interface Fatal Error Status
        const HBDS = 1 << 28; // Host Bus Data Error Status
        const HBFS = 1 << 29; // Host Bus Fatal Error Status
        const TFES = 1 << 30; // Task File Error Status
        const CPDS = 1 << 31; // Cold Port Detect Status
    }
}

bitflags::bitflags! {
    #[derive(Clone, Copy)]
    struct HbaPortIE: u32 {
        const DHRE = 1 << 0; // Device to Host Register FIS Interrupt
        const PSE = 1 << 1; // PIO Setup FIS Interrupt
        const DSE = 1 << 2; // DMA Setup FIS Interrupt
        const SDBE = 1 << 3; // Set Device Bits Interrupt
        const UFE = 1 << 4; // Unknown FIS Interrupt
        const DPE = 1 << 5; // Descriptor Processed
        const PCE = 1 << 6; // Port Connect Change Status
        const DMPE = 1 << 7; // Device Mechanical Presence Status
        const PRCE = 1 << 22; // PhyRdy Change Status
        const IPME = 1 << 23; // Incorrect Port Multiplier Status
        const OFE= 1 << 24; // Overflow Status
        const INFE = 1 << 26; // Interface Not-fatal Error Status
        const IFE = 1 << 27; // Interface Fatal Error Status
        const HBDE = 1 << 28; // Host Bus Data Error Status
        const HBFE = 1 << 29; // Host Bus Fatal Error Status
        const TFEE = 1 << 30; // Task File Error Status
        const CPDE = 1 << 31; // Cold Port Detect Status
    }
}

bitflags::bitflags! {
    #[derive(Clone, Copy)]
    struct HbaPortCmd: u32 {
        const ST = 1 << 0; // Start
        const SUD = 1 << 1; // Spin-Up Device
        const POD = 1 << 2; // Power On Device
        const CLO = 1 << 3; // Command List Override
        const FRE = 1 << 4; // FIS Receive Enable
        const MPSS = 1 << 13; // Mechanical Presence Switch State
        const FR = 1 << 14; // FIS Receive Running
        const CR = 1 << 15; // Command List Running
        const CPS = 1 << 16; // Cold Presence State
        const PMA = 1 << 17; // Port Multiplier Attached
        const HPCP = 1 << 18; // Hot Plug Capable Port
        const MSPC = 1 << 19; // Mechanical Presence Switch Attached to Port
        const CPD = 1 << 20; // Cold Presence Detection
        const ESP = 1 << 21; // External SATA Port
        const FBSCP = 1 << 22; // FIS-based Switching Capable Port
        const APSTE = 1 << 23; // Automatic Partial to Slumber Transition Enabled
        const ATAPI = 1 << 24; // Device is ATAPI
        const DLAE = 1 << 25; // Drive LED on ATAPI Enable
        const ALPE = 1 << 26; // Aggressive Link Power Management Enable
        const ASP = 1 << 27; // Aggressive Slumber / Partial
    }
}

bitflags::bitflags! {
    #[derive(Clone, Copy)]
    struct HbaCmdHeaderFlags: u16 {
        const A = 1 << 5; // ATAPI
        const W = 1 << 6; // Write
        const P = 1 << 7; // Prefetchable
        const R = 1 << 8; // Reset
        const B = 1 << 9; // Bist
        const C = 1 << 10; // Clear Busy upon R_OK
    }
}

impl HbaCmdHeaderFlags {
    /// Sets the length of the command FIS. The HBA uses this field to know the
    /// length of the FIS it shall send to the device.
    #[inline]
    fn set_command_fis_size(&mut self, size: usize) {
        self.bits().set_bits(0..=4, size as _);
    }
}

#[derive(Debug)]
enum DmaCommand {
    Read,
}

#[derive(Debug)]
pub struct DmaBuffer {
    /// The start address of the DMA buffer.
    start: PhysAddr,
    /// The data size of the DMA buffer.
    data_size: usize,
}

impl DmaBuffer {
    pub fn sectors(&self) -> usize {
        self.data_size.ceil_div(512)
    }

    pub fn start(&self) -> PhysAddr {
        self.start
    }

    pub fn data_size(&self) -> usize {
        self.data_size
    }
}

#[derive(Debug)]
pub struct DmaRequest {
    sector: usize,
    pub count: usize,
    buffer: Vec<DmaBuffer>,
    command: DmaCommand,
}

impl DmaRequest {
    /// Creates a new DMA request for the given sector and count.
    pub fn new(sector: usize, count: usize) -> Self {
        let mut size = count * 512;
        let mut buffer = Vec::<DmaBuffer>::new();

        while size > 0 {
            let data_size = core::cmp::min(size, 0x2000);
            let ordering = if size > 0x1000 {
                BuddyOrdering::Size8KiB
            } else {
                BuddyOrdering::Size4KiB
            };

            let start = pmm_alloc(ordering);

            buffer.push(DmaBuffer { start, data_size });
            size -= data_size; // Subtract the data size from the total size.
        }

        Self {
            sector,
            count,
            buffer,
            command: DmaCommand::Read,
        }
    }

    pub fn sector(&self) -> usize {
        self.sector
    }

    /// Copys the data from the DMA buffer into the given buffer.
    pub fn copy_into(&self, into: &mut [u8]) {
        let mut offset = 0x00; // Keep track of the offset
        let mut remaning = into.len(); // Keep track of the remaining data

        for buffer in self.buffer.iter() {
            let count = core::cmp::min(remaning, 0x2000);

            let buffer_phys = buffer.start();
            let buffer_virt = VirtAddr::new(buffer_phys.as_u64() + get_phys_offset());

            let buffer_pointer = buffer_virt.as_ptr();
            let buffer = unsafe { core::slice::from_raw_parts::<u8>(buffer_pointer, count) };

            // Copy the data from the buffer into the given buffer with the
            // calculated offset.
            into[offset..offset + count].copy_from_slice(buffer);

            remaning -= count; // Subtract the size from the remaining size.
            offset += count; // Add the size to the offset.
        }
    }

    pub(crate) fn as_command(&self) -> AtaCommand {
        let lba48 = self.sector > 0x0FFF_FFFF;

        match self.command {
            DmaCommand::Read => {
                if lba48 {
                    AtaCommand::ReadDmaExt
                } else {
                    AtaCommand::ReadDma
                }
            }
        }
    }

    pub fn at_offset(&self, offset: usize) -> &[DmaBuffer] {
        &self.buffer[offset / 16..]
    }
}

#[allow(unused)]
#[derive(Debug, PartialEq, Copy, Clone)]
#[repr(u8)]
pub(crate) enum AtaCommand {
    WriteDma = 0xCA,
    WriteDmaQueued = 0xCC,
    WriteMultiple = 0xC5,
    WriteSectors = 0x30,

    ReadDma = 0xC8,
    ReadDmaQueued = 0xC7,
    ReadMultiple = 0xC4,
    ReadSectors = 0x20,

    WriteDmaExt = 0x35,
    WriteDmaQueuedExt = 0x36,
    WriteMultipleExt = 0x39,
    WriteSectorsExt = 0x34,

    ReadDmaExt = 0x25,
    ReadDmaQueuedExt = 0x26,
    ReadMultipleExt = 0x29,
    ReadSectorsExt = 0x24,

    Packet = 0xA0,
    DeviceReset = 0x08,

    Service = 0xA2,
    Nop = 0,
    NopNopAutopoll = 1,

    GetMediaStatus = 0xDA,

    FlushCache = 0xE7,
    FlushCacheExt = 0xEA,

    DataSetManagement = 0x06,

    MediaEject = 0xED,

    IdentifyPacketDevice = 0xA1,
    IdentifyDevice = 0xEC,

    SetFeatures = 0xEF,
    SetFeaturesEnableReleaseInt = 0x5D,
    SetFeaturesEnableServiceInt = 0x5E,
    SetFeaturesDisableReleaseInt = 0xDD,
    SetFeaturesDisableServiceInt = 0xDE,
}

impl AtaCommand {
    #[allow(dead_code)] // will use this function later
    pub(crate) fn is_lba48(&self) -> bool {
        matches!(self, AtaCommand::ReadDmaExt | AtaCommand::WriteDmaExt)
    }

    #[allow(dead_code)] // will use this function later
    pub(crate) fn is_write(&self) -> bool {
        matches!(self, AtaCommand::WriteDmaExt | AtaCommand::WriteDma)
    }
}

#[repr(C)]
pub(crate) struct HbaMemory {
    host_capability: VolatileCell<HbaCapabilities>,
    global_host_control: VolatileCell<HbaHostCont>,
    pub(crate) interrupt_status: VolatileCell<u32>,
    ports_implemented: VolatileCell<u32>,
    version: VolatileCell<u32>,
    ccc_control: VolatileCell<u32>,
    ccc_ports: VolatileCell<u32>,
    enclosure_management_location: VolatileCell<u32>,
    enclosure_management_control: VolatileCell<HbaEnclosureCtrl>,
    host_capabilities_extended: VolatileCell<HbaCapabilities2>,
    bios_handoff_ctrl_sts: VolatileCell<HbaBohc>,
    _reserved: [u8; 0xa0 - 0x2c],
    vendor: [u8; 0x100 - 0xa0],
}

#[repr(C)]
struct FisRegH2D {
    fis_type: VolatileCell<FisType>,
    flags: VolatileCell<u8>,
    command: VolatileCell<AtaCommand>,
    featurel: VolatileCell<u8>,

    lba0: VolatileCell<u8>,
    lba1: VolatileCell<u8>,
    lba2: VolatileCell<u8>,
    device: VolatileCell<u8>,

    lba3: VolatileCell<u8>,
    lba4: VolatileCell<u8>,
    lba5: VolatileCell<u8>,
    featureh: VolatileCell<u8>,

    count: VolatileCell<u16>,
    icc: VolatileCell<u8>,
    control: VolatileCell<u8>,

    _reserved: [u8; 4],
}

#[repr(C)]
#[allow(dead_code)] //future-proof
struct FisRegD2H {
    fis_type: VolatileCell<FisType>,
    pm: VolatileCell<u8>,
    pub status: VolatileCell<u8>,
    pub err: VolatileCell<u8>,
    pub lba0: VolatileCell<u8>,
    pub lba1: VolatileCell<u8>,
    pub lba2: VolatileCell<u8>,
    pub dev: VolatileCell<u8>,
    pub lba3: VolatileCell<u8>,
    pub lba4: VolatileCell<u8>,
    pub lba5: VolatileCell<u8>,
    pub _rsvd0: VolatileCell<u8>,
    pub count_low: VolatileCell<u8>,
    pub count_high: VolatileCell<u8>,
    pub _rsvd1: [VolatileCell<u8>; 6],
}

impl FisRegH2D {
    fn set_lba(&mut self, lba: usize) {
        debug_assert!(lba < 1 << 48, "LBA is limited to 48 bits");

        self.lba0.set(lba as _);
        self.lba1.set((lba >> 8) as _);
        self.lba2.set((lba >> 16) as _);
        self.lba3.set((lba >> 24) as _);
        self.lba4.set((lba >> 32) as _);
        self.lba5.set((lba >> 40) as _);
    }

    fn set_command(&mut self, yes: bool) {
        let mut old_flags = self.flags.get();
        old_flags.set_bit(7, yes);

        self.flags.set(old_flags);
    }
}

#[repr(C)]
struct HbaCmdTbl {
    cfis: [u8; 64],
    acmd: [u8; 16],
    _reserved: [u8; 48],

    prdt_entry: [HbaPrdtEntry; 1],
}

impl HbaCmdTbl {
    fn cfis_as_h2d_mut(&mut self) -> &mut FisRegH2D {
        unsafe { &mut *(self.cfis.as_mut_ptr() as *mut FisRegH2D) }
    }

    fn prdt_entry_mut(&mut self, i: usize) -> &mut HbaPrdtEntry {
        unsafe { &mut *self.prdt_entry.as_mut_ptr().add(i) }
    }
}

#[repr(C)]
struct HbaPrdtEntry {
    dba: VolatileCell<PhysAddr>,
    _reserved: u32,
    flags: VolatileCell<u32>,
}

impl HbaPrdtEntry {
    /// Sets the data byte count that indicates the length in bytes of the data block.
    #[inline]
    fn set_data_byte_count(&mut self, count: usize) {
        let mut old_flags = self.flags.get();
        old_flags.set_bits(0..21, count as _);

        self.flags.set(old_flags);
    }

    /// Sets the interrupt on completion bit which indicates that hardware should
    /// assert an interrupt when the data block for this entry has transferred. The
    /// HBA shall set the PxIS.DPS bit after completing the data transfer and if enabled,
    /// generate an interrupt.
    #[inline]
    fn set_interrupt_on_completion(&mut self, yes: bool) {
        let mut old_flags = self.flags.get();
        old_flags.set_bit(31, yes);

        self.flags.set(old_flags);
    }
}

#[repr(u8)]
#[derive(Copy, Clone)]
#[allow(dead_code)] //future-proof
enum FisType {
    RegH2D = 0x27,
    RegD2H = 0x34,
    DmaInit = 0x39,
    DmaSetup = 0x41,
    Data = 0x46,
    Bist = 0x58,
    PioSetup = 0x5f,
    DevBits = 0xa1,
}

enum HbaPortDd {
    None = 0,
    PresentNotE = 1,
    PresentAndE = 3,
    Offline = 4,
}

enum HbaPortIpm {
    None = 0,
    Active = 1,
    Partial = 2,
    Slumber = 6,
    DevSleep = 8,
}

#[repr(transparent)]
#[derive(Clone, Copy)]
struct HbaSataStatus(u64);

impl HbaSataStatus {
    pub(crate) fn device_detection(&self) -> HbaPortDd {
        match self.0.get_bits(0..=3) {
            0 => HbaPortDd::None,
            1 => HbaPortDd::PresentNotE,
            3 => HbaPortDd::PresentAndE,
            4 => HbaPortDd::Offline,
            v => panic!("Invalid HbaPortSstsRegDet {}", v),
        }
    }

    fn interface_power_management(&self) -> HbaPortIpm {
        match self.0.get_bits(8..=11) {
            0 => HbaPortIpm::None,
            1 => HbaPortIpm::Active,
            2 => HbaPortIpm::Partial,
            6 => HbaPortIpm::Slumber,
            8 => HbaPortIpm::DevSleep,
            v => panic!("Invalid HbaPortSstsRegIpm {}", v),
        }
    }
}

#[repr(C)]
pub(crate) struct HbaPort {
    clb: VolatileCell<PhysAddr>,
    fb: VolatileCell<PhysAddr>,
    pub is: VolatileCell<HbaPortIS>,
    ie: VolatileCell<HbaPortIE>,
    cmd: VolatileCell<HbaPortCmd>,
    _reserved: u32,
    tfd: VolatileCell<u32>,
    sig: VolatileCell<u32>,
    ssts: VolatileCell<HbaSataStatus>,
    sctl: VolatileCell<u32>,
    serr: VolatileCell<u32>,
    sact: VolatileCell<u32>,
    ci: VolatileCell<u32>,
    sntf: VolatileCell<u32>,
    fbs: VolatileCell<u32>,
    devslp: VolatileCell<u32>,
    _reserved_1: [u32; 10],
    vendor: [u32; 4],
}

#[repr(C)]
struct HbaCmdHeader {
    flags: VolatileCell<HbaCmdHeaderFlags>,
    prdtl: VolatileCell<u16>,
    prdbc: VolatileCell<u32>,
    ctb: VolatileCell<PhysAddr>,
    _reserved: [u32; 4],
}

impl HbaPort {
    fn cmd_header_at(&mut self, index: usize) -> &mut HbaCmdHeader {
        // Since the CLB holds the physical address, we make the address mapped
        // before reading it.
        let clb_mapped = VirtAddr::new(self.clb.get().as_u64() + get_phys_offset());
        // Get the address of the command header at `index`.
        let clb_addr = clb_mapped + core::mem::size_of::<HbaCmdHeader>() * index;

        // Cast it as [`HbaCmdHeader`] and return a mutable reference to it.
        unsafe { &mut *(clb_addr.as_mut_ptr::<HbaCmdHeader>()) }
    }

    /// This function is responsible for allocating space for command lists,
    /// tables, etc.. for a given this instance of HBA port.
    fn start(&mut self) {
        self.stop_cmd(); // Stop the command engine before starting the port

        /*
         * size = sizeof(CTB) * 32 == 4KiB * 2 (so we need to allocate
         * two 4KiB size frames).
         */
        let frame_addr = pmm_alloc(BuddyOrdering::Size8KiB);
        let page_addr = get_phys_offset() + frame_addr.as_u64();

        for size in (0..0x2000u64).step_by(0x1000) {
            map_page!(
                VirtAddr::new(page_addr + size).as_u64(),
                (frame_addr + size).as_u64(),
                Size4KiB,
                PageTableFlags::PRESENT
                    | PageTableFlags::WRITABLE
                    | PageTableFlags::WRITE_THROUGH
                    | PageTableFlags::NO_CACHE
            );
        }

        for i in 0..32 {
            let command_header = self.cmd_header_at(i);

            // 8 prdt entries per command table
            // 256 bytes per command table, 64 + 16 + 48 + 16 * 8
            command_header.prdtl.set(8);
            command_header.prdbc.set(0);
            command_header.ctb.set(PhysAddr::new(
                (frame_addr.as_u64() as usize + 256 * i) as u64,
            ));
        }

        // Read and write back command list base
        let clb = self.clb.get();
        self.clb.set(clb);

        // Read and write back FIS Base
        let fb = self.fb.get();
        self.fb.set(fb);

        // Read and write back interrupt status
        let is = self.is.get();
        self.is.set(is);

        // Enable interrupts
        self.ie.set(HbaPortIE::all());

        // Disable power management (for now)
        let sctl = self.sctl.get();
        self.sctl.set(sctl | 7 << 8);

        // Power on and spin up
        self.cmd.set(HbaPortCmd::POD | HbaPortCmd::SUD);

        // Start the command engine
        self.start_cmd();
    }

    fn start_cmd(&mut self) {
        while self.cmd.get().contains(HbaPortCmd::CR) {
            core::hint::spin_loop();
        }

        let value = self.cmd.get() | (HbaPortCmd::FRE | HbaPortCmd::ST);
        self.cmd.set(value);
    }

    fn stop_cmd(&mut self) {
        let mut cmd = self.cmd.get();
        cmd.remove(HbaPortCmd::FRE | HbaPortCmd::ST);

        self.cmd.set(cmd);

        while self.cmd.get().intersects(HbaPortCmd::FR | HbaPortCmd::CR) {
            core::hint::spin_loop();
        }
    }

    fn probe(&mut self, port: usize) -> bool {
        let status = self.ssts.get();

        let ipm = status.interface_power_management();
        let dd = status.device_detection();

        // Check if the port is active and is present. If thats the case
        // we can start the AHCI port.
        if let (HbaPortDd::PresentAndE, HbaPortIpm::Active) = (dd, ipm) {
            if PCI_DRIVER_COUNT.load(Ordering::SeqCst) == 1 {
                debug!("AHCI: enabling port {}", port);
            }

            self.start();
            true
        } else {
            // Else we can't enable the port.
            false
        }
    }

    fn run_command(
        &mut self,
        command: AtaCommand,
        sector: usize,
        count: usize,
        slot: usize,
        buffer: &[DmaBuffer],
    ) {
        let header = self.cmd_header_at(slot);
        let mut flags = header.flags.get();

        if command == AtaCommand::WriteDmaExt || command == AtaCommand::WriteDma {
            flags.insert(HbaCmdHeaderFlags::W); // If its a write command add the write flag.
        } else {
            flags.remove(HbaCmdHeaderFlags::W); // If its a read command remove the write flag.
        }

        flags.insert(HbaCmdHeaderFlags::P | HbaCmdHeaderFlags::C);
        flags.set_command_fis_size(core::mem::size_of::<FisRegH2D>() / 4);

        header.flags.set(flags); // Update command header flags.

        let length = ((count - 1) >> 4) + 1;
        header.prdtl.set(length as _); // Update the number of PRD entries.

        let command_table_addr = VirtAddr::new(get_phys_offset() + header.ctb.get().as_u64());
        let command_table = unsafe { &mut *(command_table_addr).as_mut_ptr::<HbaCmdTbl>() };

        for (pri, dma) in buffer.iter().enumerate().take(length) {
            let prdt = command_table.prdt_entry_mut(pri);

            prdt.dba.set(buffer[pri].start);
            prdt.set_data_byte_count(buffer[pri].data_size - 1);
            prdt.set_interrupt_on_completion(pri == length - 1);
        }

        let fis = command_table.cfis_as_h2d_mut();

        fis.control.set(0x00);
        fis.icc.set(0x00);
        fis.featurel.set(0x00);
        fis.featureh.set(0x00);
        fis._reserved.fill(0x00);

        fis.fis_type.set(FisType::RegH2D);
        fis.device.set(1 << 6);
        fis.command.set(command);
        fis.count.set(count as _);

        fis.set_lba(sector);
        fis.set_command(true);

        // Issue the command!
        self.ci.set(1 << slot);

        let mut spin = 100;

        // Make sure the port is not busy.
        // Also, thanks Clippy for helping me find an upstream Aero bug!
        while (self.tfd.get() == 0x80 || self.tfd.get() == 0x08) && spin > 0 {
            core::hint::spin_loop();
            spin -= 1;
        }

        if spin == 0 {
            warn!("AHCI: port hung");
            return;
        }

        // Wait for the command to complete.
        while self.ci.get() & (1 << slot) == 1 {
            if self.is.get().contains(HbaPortIS::TFES) {
                warn!("AHCI: disk error (serr={:#x})", self.serr.get());
                break;
            }
        }
    }
}

impl HbaMemory {
    pub(crate) fn port_mut(&mut self, port: usize) -> &mut HbaPort {
        unsafe { &mut *((self as *mut Self).offset(1) as *mut HbaPort).add(port) }
    }
}

#[allow(dead_code)] // need this for later
#[derive(Debug)]
struct AhciCommand {
    request: Arc<DmaRequest>,
}

#[derive(Debug)]
pub(crate) struct AhciPortProtected {
    address: VirtAddr,
    cmds: [Option<AhciCommand>; 32],
    free_cmds: usize,
}

impl AhciPortProtected {
    pub(crate) fn hba_port(&mut self) -> &mut HbaPort {
        unsafe { &mut *(self.address.as_mut_ptr::<HbaPort>()) }
    }

    fn run_request(&mut self, request: Arc<DmaRequest>, mut offset: usize) -> usize {
        let mut remaining = request.count - offset;

        while remaining > 0 {
            let slot = {
                let command =
                    self.cmds
                        .iter()
                        .enumerate()
                        .find_map(|(i, e)| if e.is_none() { Some(i) } else { None });

                if let Some(i) = command {
                    let hba = self.hba_port();
                    let count = core::cmp::min(remaining, 128);

                    hba.run_command(
                        request.as_command(),
                        request.sector + offset,
                        count,
                        i,
                        request.at_offset(offset),
                    );

                    remaining -= count;
                    offset += count;

                    i
                } else {
                    return offset;
                }
            };

            self.cmds[slot] = Some(AhciCommand {
                request: request.clone(),
            });

            self.free_cmds -= 1;
        }

        offset
    }
}

#[derive(Debug)]
pub(crate) struct AhciPort {
    pub(crate) inner: RwLock<AhciPortProtected>,
}

impl AhciPort {
    #[inline]
    fn new(address: VirtAddr) -> Self {
        const EMPTY: Option<AhciCommand> = None;

        Self {
            inner: RwLock::new(AhciPortProtected {
                address,
                cmds: [EMPTY; 32],
                free_cmds: 32,
            }),
        }
    }

    fn run_request(&self, request: Arc<DmaRequest>) -> Option<usize> {
        let mut offset = 0x00;

        // Run request and wait for it to complete.
        while offset < request.count {
            offset = self.inner.write().run_request(request.clone(), offset);
        }

        Some(request.count * 512)
    }

    pub(crate) fn read(&self, sector: usize, buffer: &mut [u8]) -> Option<usize> {
        let count = (buffer.len() + 512 - 1) / 512;
        let request = Arc::new(DmaRequest::new(sector, count));

        let result = self.run_request(request.clone()); // Perform the DMA request.

        if result.is_some() {
            request.copy_into(buffer); // Copy the result into the provided buffer.
        }

        result
    }
}

pub(crate) struct AhciProtected {
    pub(crate) ports: [Option<Arc<AhciPort>>; 32],
    hba: VirtAddr,
}

impl Clone for AhciProtected {
    fn clone(&self) -> Self {
        Self {
            ports: self.ports.clone(),
            hba: self.hba,
        }
    }
}

impl AhciProtected {
    #[inline]
    pub(crate) fn hba_mem<'a>(&self) -> &'a mut HbaMemory {
        unsafe { &mut *(self.hba.as_u64() as *mut HbaMemory) }
    }

    fn start_hba(&mut self) {
        let mut hba = self.hba_mem();

        // Take back control from the firmware
        hba.bios_handoff_ctrl_sts.set(HbaBohc::OOC | HbaBohc::OOS);

        // Enable interrupts
        let current_flags = hba.global_host_control.get();
        hba.global_host_control.set(current_flags | HbaHostCont::IE);

        let version = hba.version.get();
        let major_version = version >> 16 & 0xffff;
        let minor_version = version & 0xffff;

        debug!(
            "AHCI: controller version {}.{}",
            major_version, minor_version
        );

        let pi = hba.ports_implemented.get();

        for i in 0..32 {
            if pi.get_bit(i) {
                let port = hba.port_mut(i);

                if port.probe(i) {
                    // Get the address of the HBA port.
                    let address = VirtAddr::new(port as *const _ as _);

                    debug!("AHCI: Port {:#?} address: {:#x}", i, address.as_u64());

                    let port = Arc::new(AhciPort::new(address));

                    // Add the port to the ports array.
                    self.ports[i] = Some(port);

                    // Workaround to get access to the HBA and still satify the
                    // borrow checker.
                    hba = self.hba_mem();
                }
            }
        }
    }

    /// This function is responsible for enabling bus mastering and add AHCI
    /// IRQ handler.
    fn enable_interrupts(&mut self, header: &mut pcics::Header) {
        header.command.io_space = false;
        header.command.memory_space = true;
        header.command.bus_master = true;
    }

    /// This function is responsible for initializing and starting the AHCI driver.
    fn start_driver(&mut self, header: &mut pcics::Header) {
        if let HeaderType::Normal(normal_header) = header.header_type.clone() {
            let abar = normal_header.base_addresses.orig()[5] as u64;

            debug!("ABAR: {:#x}", &abar);

            let abar_test_page = Page::<Size4KiB>::containing_address(VirtAddr::new(abar));
            let abar_virt = abar_test_page.start_address().as_u64() + get_phys_offset();

            map_page!(
                abar,
                abar_virt,
                Size4KiB,
                PageTableFlags::PRESENT
                    | PageTableFlags::WRITABLE
                    | PageTableFlags::NO_CACHE
                    | PageTableFlags::WRITE_THROUGH
            );

            self.hba = VirtAddr::new(abar_virt);

            without_interrupts(|| {
                self.start_hba();
                self.enable_interrupts(header);
            });

            // Test code: can confirm this actually works
            // info!("Powering off");
            // unsafe { super::acpi_impl::system_shutdown() };

            // Test code
            for port in self.ports.iter().filter(|p| p.is_some()) {
                if let Some(port) = port {
                    let buffer = &mut [0u8; 512];
                    let sector = 0;

                    if port.read(sector, buffer).is_some() {
                        debug!("Read sector {:?}: {:?}", sector, buffer);
                    } else {
                        warn!("Couldn't read any data")
                    }
                } else {
                    unreachable!()
                }
            }
        } else {
            panic!("AHCI: Not a normal header")
        }
    }
}

/// Structure representing the ACHI driver.
pub struct AhciDriver {
    inner: RwLock<AhciProtected>,
}

impl AhciDriver {
    pub(crate) fn read(&self) -> RwLockReadGuard<AhciProtected> {
        self.inner.read()
    }

    pub(crate) fn write(&self) -> RwLockWriteGuard<AhciProtected> {
        self.inner.write()
    }
}

impl FOSSPciDeviceHandle for AhciDriver {
    fn handles(&self, vendor_id: Vendor, device_id: DeviceKind) -> bool {
        matches!((vendor_id, device_id), (_, DeviceKind::SataController))
    }

    fn start(&self, header: &mut pcics::Header) {
        // keeps restarting endlessly if I don't put this check in here
        if PCI_DRIVER_COUNT.load(Ordering::Relaxed) == 1 {
            debug!("AHCI: Initializing");
            get_ahci().write().start_driver(header);
        }
    }
}

pub(crate) fn get_hba<'a>() -> &'a mut HbaMemory {
    get_ahci().read().hba_mem()
}

/// Returns a reference-counting pointer to the AHCI driver.
pub(crate) fn get_ahci<'a>() -> &'a Arc<AhciDriver> {
    DRIVER
        .get()
        .expect("Attempted to get the AHCI driver before it was initialized")
}

pub(crate) fn ahci_init() {
    // Initialize the AHCI driver instance.
    DRIVER.call_once(|| {
        const EMPTY: Option<Arc<AhciPort>> = None; // To satisfy the Copy trait bound when the AHCI creating data.

        Arc::new(AhciDriver {
            inner: RwLock::new(AhciProtected {
                ports: [EMPTY; 32],    // Initialize the AHCI ports to an empty slice.
                hba: VirtAddr::zero(), // Initialize the AHCI HBA address to zero.
            }),
        })
    });

    // Now register the AHCI driver with the PCI subsystem.
    register_device_driver(get_ahci().clone());
}
