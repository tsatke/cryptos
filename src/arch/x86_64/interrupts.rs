use core::sync::atomic::AtomicU32;

use log::warn;
use raw_cpuid::{CpuId, Hypervisor};
use spin::RwLock;
use x86_64::{
    instructions::interrupts,
    registers::{
        read_rip,
        rflags::{self, RFlags},
        segmentation::{Segment, CS},
    },
    structures::{
        gdt::SegmentSelector,
        idt::{Entry, InterruptStackFrameValue, SelectorErrorCode},
        paging::{PageTableFlags, Size4KiB},
    },
    PrivilegeLevel,
};

use crate::{
    ahci::{get_ahci, get_hba, HbaPortIS},
    apic_impl::{get_active_lapic, get_lapic_ids},
    map_page,
    process::{signal::Signal, State, PTABLE, PTABLE_IDX},
};

use {
    core::sync::atomic::{AtomicU64, Ordering},
    lazy_static::lazy_static,
    log::{debug, error, info},
    x86_64::{
        registers::control::Cr2,
        structures::idt::{InterruptDescriptorTable, InterruptStackFrame, PageFaultErrorCode},
    },
};

/// Initializes the IDT
pub fn init() {
    lazy_static! {
        static ref IDT_CLONE: InterruptDescriptorTable = IDT.read().clone();
    }
    IDT_CLONE.load();
}

pub fn current_privilege_level(frame: InterruptStackFrameValue) -> PrivilegeLevel {
    // TODO: from what end is the SegmentSelector padded with zeros? Ask @phil-opp for advice on this
    let sel = SegmentSelector(frame.code_segment as u16);

    sel.rpl()
}

pub const QEMU_STATUS_FAIL: u32 = 0x11;

pub static INTA_IRQ: AtomicU64 = AtomicU64::new(0);
pub static INTB_IRQ: AtomicU64 = AtomicU64::new(0);
pub static INTC_IRQ: AtomicU64 = AtomicU64::new(0);
pub static INTD_IRQ: AtomicU64 = AtomicU64::new(0);

lazy_static! {
    pub static ref IDT: RwLock<InterruptDescriptorTable> = {
        let mut idt = InterruptDescriptorTable::new();
        unsafe {
            idt.double_fault
                .set_handler_fn(double_fault)
                .set_stack_index(super::exceptions::DOUBLE_FAULT_STACK_INDEX);

            idt.page_fault
                .set_handler_fn(page_fault)
                .set_stack_index(super::exceptions::PAGE_FAULT_STACK_INDEX);
            idt.divide_error
                .set_handler_fn(sigfpe)
                .set_stack_index(super::exceptions::DIV_ERR_STACK_INDEX);
            idt.invalid_tss
                .set_handler_fn(invalid_tss)
                .set_stack_index(super::exceptions::INVALID_TSS_STACK_INDEX);
            idt.segment_not_present
                .set_handler_fn(sigbus)
                .set_stack_index(super::exceptions::SIGBUS_STACK_INDEX);
            idt.stack_segment_fault
                .set_handler_fn(sigsegv)
                .set_stack_index(super::exceptions::SIGSEGV_STACK_INDEX);
            idt.general_protection_fault
                .set_handler_fn(general_protection)
                .set_stack_index(super::exceptions::GPF_STACK_INDEX);
        }

        idt.breakpoint.set_handler_fn(breakpoint);
        idt.bound_range_exceeded
            .set_handler_fn(bound_range_exceeded);
        idt.invalid_opcode.set_handler_fn(invalid_op);
        idt.device_not_available.set_handler_fn(navail);
        idt[IrqIndex::Timer as usize].set_handler_fn(timer);
        idt[IrqIndex::LapicErr as usize].set_handler_fn(lapic_err);
        idt[IrqIndex::Spurious as usize].set_handler_fn(spurious);
        idt[INTA_IRQ.load(Ordering::SeqCst) as usize].set_handler_fn(pin_inta);
        idt[INTB_IRQ.load(Ordering::SeqCst) as usize].set_handler_fn(pin_intb);
        idt[INTC_IRQ.load(Ordering::SeqCst) as usize].set_handler_fn(pin_intc);
        idt[INTD_IRQ.load(Ordering::SeqCst) as usize].set_handler_fn(pin_intd);
        idt[0x80].set_handler_fn(syscall);

        // Vector 100 = IPI_WAKE handler as task scheduler
        // performance is the obvious reason why I'm doing this
        idt[132].set_handler_fn(task_sched);
        idt[139].set_handler_fn(pci);
        idt[0x82].set_handler_fn(spurious);
        idt[151].set_handler_fn(ahci);
        RwLock::new(idt)
    };
}

pub static TICK_COUNT: AtomicU64 = AtomicU64::new(0);
pub static ACTIVE_LAPIC_ID: AtomicU32 = AtomicU32::new(0);

#[derive(Debug, Clone, Copy)]
#[repr(u8)]
pub enum IrqIndex {
    Timer = 0xfe,     // 254
    LapicErr = 0x31,  // 49
    IpiWake = 0x40,   // 64
    IpiTlb = 0x41,    // 65
    IpiSwitch = 0x42, // 66
    IpiPit = 0x43,    // 67
    Spurious = 0xff,  // 255
}

extern "x86-interrupt" fn timer(_frame: InterruptStackFrame) {
    TICK_COUNT.fetch_add(1, Ordering::Relaxed);
    unsafe { get_active_lapic().end_of_interrupt() };
}

extern "x86-interrupt" fn spurious(_frame: InterruptStackFrame) {
    debug!("Received spurious interrupt");
    unsafe { get_active_lapic().end_of_interrupt() };
}

extern "x86-interrupt" fn lapic_err(_frame: InterruptStackFrame) {
    error!("Local APIC error; check the status for details");
    unsafe { get_active_lapic().end_of_interrupt() };
}

/// Round-robin preemptive scheduler
///
/// Uses an IPI instead of the timer or the loop at the end of maink for optimization reasons:
/// an IPI can send itself to every CPU on the system, making it possible to evenly distribute all that power
extern "x86-interrupt" fn task_sched(_: InterruptStackFrame) {
    // use index of an atomic to ensure that only one process is being run at a time
    if !(PTABLE.read().is_empty()) {
        if PTABLE.read().len() == 1 {
            // always runnable as only one process exists
            (PTABLE.read())[&PTABLE_IDX.load(Ordering::SeqCst)]
                .write()
                .set_state(State::Runnable);

            (PTABLE.read())[&PTABLE_IDX.load(Ordering::SeqCst)]
                .write()
                .run()
                .unwrap(); // TODO: handle error cases using file descriptors
        } else {
            // need to preempt previous process in the table
            (PTABLE.read())[&(PTABLE_IDX.load(Ordering::SeqCst) - 1)]
                .write()
                .set_state(State::Blocked);

            (PTABLE.read())[&PTABLE_IDX.load(Ordering::SeqCst)]
                .write()
                .set_state(State::Runnable);

            (PTABLE.read())[&PTABLE_IDX.load(Ordering::SeqCst)]
                .write()
                .run()
                .unwrap(); // TODO: handle error cases using file descriptors
        }

        if PTABLE_IDX.load(Ordering::SeqCst) < (PTABLE.read().len()) {
            // ensure that the next CPU core runs the next process when it receives this interrupt
            PTABLE_IDX.fetch_add(1, Ordering::SeqCst);
        } else {
            // we've reached the end of PTABLE, so time to reset this
            PTABLE_IDX.store(0, Ordering::SeqCst);
        }
    }

    if ACTIVE_LAPIC_ID.load(Ordering::SeqCst) == 0 {
        // initialize with first LAPIC ID
        ACTIVE_LAPIC_ID.store(get_lapic_ids().next().unwrap(), Ordering::SeqCst);

        // get the ball rolling
        unsafe { get_active_lapic().send_ipi(100, get_lapic_ids().cycle().nth(1).unwrap()) };
    } else {
        // need to store this in a variable in order to ensure that `.next()` matches the correct core ID
        let mut lapic_iter = get_lapic_ids().cycle();

        if lapic_iter.any(|id| id == ACTIVE_LAPIC_ID.load(Ordering::SeqCst)) {
            // find the next LAPIC ID after the current one
            // Note that because we're using `.cycle` this will never be None
            let id = lapic_iter.next().unwrap();

            // update active LAPIC ID to match the next one
            ACTIVE_LAPIC_ID.store(id, Ordering::SeqCst);

            // send the very IPI that this handler handles to the next available CPU core on the system
            unsafe { get_active_lapic().send_ipi(100, ACTIVE_LAPIC_ID.load(Ordering::SeqCst)) };
        } else {
            unreachable!()
        }
    }

    unsafe {
        get_active_lapic().end_of_interrupt();
    };
}

extern "x86-interrupt" fn bound_range_exceeded(frame: InterruptStackFrame) {
    if let PrivilegeLevel::Ring0 = current_privilege_level(*frame) {
        panic!("Bound range exceeded\nStack frame: {:#?}", frame);
    } else {
        (PTABLE.read())[&PTABLE_IDX.load(Ordering::SeqCst)]
            .write()
            .kill(Signal::SIGFPE);
    }
}

extern "x86-interrupt" fn invalid_op(frame: InterruptStackFrame) {
    let offender = unsafe { *((frame.instruction_pointer.as_u64()) as *const u32) };

    if let PrivilegeLevel::Ring0 = current_privilege_level(*frame) {
        panic!(
            "Invalid opcode\nOffending instruction: {:#x?}\nStack frame: {:#?}",
            offender.to_be_bytes(),
            frame
        );
    } else {
        (PTABLE.read())[&PTABLE_IDX.load(Ordering::SeqCst)]
            .write()
            .kill(Signal::SIGILL);
    }
}

extern "x86-interrupt" fn navail(frame: InterruptStackFrame) {
    if let PrivilegeLevel::Ring0 = current_privilege_level(*frame) {
        panic!("Device not available\nStack frame: {:#?}", frame);
    } else {
        (PTABLE.read())[&PTABLE_IDX.load(Ordering::SeqCst)]
            .write()
            .kill(Signal::SIGSYS);
    }
}

extern "x86-interrupt" fn breakpoint(_frame: InterruptStackFrame) {
    debug!("Reached breakpoint; waiting for debugger to give the all-clear");
    loop {
        let int3_ip = read_rip().as_u64();

        // 0xcc is the INT3 opcode
        if unsafe { *(int3_ip as *const u8) != 0xcc } {
            debug!("Resuming");
            break;
        }
    }
}

extern "x86-interrupt" fn double_fault(frame: InterruptStackFrame, _code: u64) -> ! {
    panic!(
        "Double fault at address {:#x}\nBacktrace: {:#?}",
        frame.instruction_pointer.as_u64(),
        frame
    );
}

extern "x86-interrupt" fn page_fault(frame: InterruptStackFrame, code: PageFaultErrorCode) {
    if code.is_empty() {
        // Create and map the nonexistent page and try again
        let virt = Cr2::read().as_u64();
        let phys = Cr2::read().as_u64();

        map_page!(
            phys,
            virt,
            Size4KiB,
            PageTableFlags::PRESENT
                | PageTableFlags::WRITABLE
                | PageTableFlags::NO_CACHE
                | PageTableFlags::WRITE_THROUGH
        );

        unsafe { frame.iretq() }
    } else if let PrivilegeLevel::Ring0 = CS::get_reg().rpl() {
        // kernel mode
        panic!(
            "Page fault: Attempt to access address {:#x} returned a {:#?} error\n Backtrace: {:#?}",
            Cr2::read(),
            code,
            frame
        );
    } else {
        // user mode
        (PTABLE.read())[&PTABLE_IDX.load(Ordering::SeqCst)]
            .write()
            .kill(Signal::SIGSEGV);
    }
}

extern "x86-interrupt" fn sigfpe(frame: InterruptStackFrame) {
    if let PrivilegeLevel::Ring0 = current_privilege_level(*frame) {
        panic!("Attempt to divide by zero\nBacktrace: {:#?}", frame);
    } else {
        (PTABLE.read())[&PTABLE_IDX.load(Ordering::SeqCst)]
            .write()
            .kill(Signal::SIGFPE);
    }
}
extern "x86-interrupt" fn invalid_tss(frame: InterruptStackFrame, code: u64) {
    error!(
        "Invalid TSS at segment selector {:#?}\nBacktrace: {:#?}",
        code, frame
    );
}

extern "x86-interrupt" fn sigbus(frame: InterruptStackFrame, code: u64) {
    let selector = SelectorErrorCode::new_truncate(code);

    if let PrivilegeLevel::Ring0 = current_privilege_level(*frame) {
        panic!(
            "Segment selector at index {:#?} is not present\n\
            Descriptor table involved: {:#?}\n\
            Is external? {}\n\
            Is null? {}\n\
            Backtrace: {:#?}",
            match CpuId::new().get_hypervisor_info() {
                Some(hypervisor) =>
                    if let Hypervisor::QEMU = hypervisor.identify() {
                        selector.index() / 2
                    } else {
                        selector.index()
                    },
                None => selector.index(),
            },
            selector.descriptor_table(),
            match selector.external() {
                true => "Yes",
                false => "No",
            },
            match selector.is_null() {
                true => "Yes",
                false => "No",
            },
            frame
        );
    } else {
        (PTABLE.read())[&PTABLE_IDX.load(Ordering::SeqCst)]
            .write()
            .kill(Signal::SIGBUS);
    }
}

extern "x86-interrupt" fn sigsegv(frame: InterruptStackFrame, code: u64) {
    let is_caused_by_np = match code {
        0 => None,
        sel => Some(sel),
    };

    if let PrivilegeLevel::Ring0 = current_privilege_level(*frame) {
        if let Some(code) = is_caused_by_np {
            let selector = SelectorErrorCode::new_truncate(code);
            panic!(
                "Segment selector at index {:#?} caused a stack segment fault\nBacktrace: {:#?}",
                match CpuId::new().get_hypervisor_info() {
                    Some(hypervisor) =>
                        if let Hypervisor::QEMU = hypervisor.identify() {
                            selector.index() / 2
                        } else {
                            selector.index()
                        },
                    None => selector.index(),
                },
                frame
            );
        } else {
            panic!(
                "Stack segment fault of unknown cause\nBacktrace: {:#?}",
                frame
            );
        }
    } else {
        (PTABLE.read())[&PTABLE_IDX.load(Ordering::SeqCst)]
            .write()
            .kill(Signal::SIGSEGV);
    }
}

extern "x86-interrupt" fn general_protection(frame: InterruptStackFrame, code: u64) {
    let is_seg_related = match code {
        0 => None,
        sel => Some(sel),
    };

    if let PrivilegeLevel::Ring0 = current_privilege_level(*frame) {
        if let Some(code) = is_seg_related {
            let selector = SelectorErrorCode::new_truncate(code);
            panic!(
                "Segment selector at index {:#?} caused a general protection fault\n\
                Descriptor table involved: {:#?}\n\
                Is external? {}\n\
                Is null? {}\n\
                Backtrace: {:#?}",
                match CpuId::new().get_hypervisor_info() {
                    Some(hypervisor) =>
                        if let Hypervisor::QEMU = hypervisor.identify() {
                            selector.index() / 2
                        } else {
                            selector.index()
                        },
                    None => selector.index(),
                },
                selector.descriptor_table(),
                match selector.external() {
                    true => "Yes",
                    false => "No",
                },
                match selector.is_null() {
                    true => "Yes",
                    false => "No",
                },
                frame
            );
        } else {
            panic!(
                "General protection fault of unknown cause\nBacktrace: {:#?}",
                frame
            )
        }
    } else {
        (PTABLE.read())[&PTABLE_IDX.load(Ordering::SeqCst)]
            .write()
            .kill(Signal::SIGABRT);
    }
}

pub extern "x86-interrupt" fn pin_inta(_frame: InterruptStackFrame) {
    info!("Received IntA interrupt");
    unsafe { get_active_lapic().end_of_interrupt() };
}

pub extern "x86-interrupt" fn pin_intb(_frame: InterruptStackFrame) {
    info!("Received IntB interrupt");
    unsafe { get_active_lapic().end_of_interrupt() };
}

pub extern "x86-interrupt" fn pin_intc(_frame: InterruptStackFrame) {
    info!("Received IntC interrupt");
    unsafe { get_active_lapic().end_of_interrupt() };
}

pub extern "x86-interrupt" fn pin_intd(_frame: InterruptStackFrame) {
    info!("Received IntD interrupt");
    unsafe { get_active_lapic().end_of_interrupt() };
}

pub extern "x86-interrupt" fn pci(frame: InterruptStackFrame) {
    debug!("Received PCI interrupt: {:#?}", &frame);
    unsafe { get_active_lapic().end_of_interrupt() };
}

pub extern "x86-interrupt" fn ahci(frame: InterruptStackFrame) {
    info!("Received AHCI interrupt: {:#?}", &frame);

    // Source: https://wiki.osdev.org/AHCI#IRQ_handler

    // Read and write back global HBA interrupt status
    let status = get_hba().interrupt_status.get();
    get_hba().interrupt_status.set(status);

    // Read and write back port interrupt status
    for port in get_ahci().write().ports.as_mut().iter_mut().flatten() {
        let port_status = port.inner.write().hba_port().is.get();

        // Check error bit and debug if set
        if port_status.contains(HbaPortIS::HBDS) {
            warn!("AHCI: Host bus data error");
        } else if port_status.contains(HbaPortIS::HBFS) {
            warn!("AHCI: Host bus file error");
        } else if port_status.contains(HbaPortIS::TFES) {
            warn!("AHCI: Task file error");
        } else if port_status.contains(HbaPortIS::CPDS) {
            warn!("AHCI: Cold port detected");
        }

        port.inner.write().hba_port().is.set(port_status);
    }

    unsafe { get_active_lapic().end_of_interrupt() };
}

pub extern "x86-interrupt" fn syscall(_: InterruptStackFrame) {
    todo!("Syscall handler");
}

#[inline(always)]
pub fn is_enabled() -> bool {
    rflags::read().contains(RFlags::INTERRUPT_FLAG)
}

#[inline(always)]
pub(crate) unsafe fn disable_interrupts() {
    interrupts::disable();
}

#[inline(always)]
pub(crate) unsafe fn enable_interrupts() {
    interrupts::enable();
}

/// Function that searches for an open entry in the IDT to map a new handler to
pub fn irqalloc() -> u8 {
    for i in 32..=255 {
        if IDT.read()[i] == Entry::missing() {
            return i as u8;
        }
    }
    // we've exhausted all entries at this point
    panic!("Out of IRQs!")
}

/// Indexes a new handler at a new IDT entry created by `irqalloc()` fn
pub fn register_handler(irq: u8, handler: extern "x86-interrupt" fn(InterruptStackFrame)) {
    IDT.write()[irq as usize].set_handler_fn(handler);
    init();
}
