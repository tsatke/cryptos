use crate::{cralloc::frames::XhciMapper, pci_impl::DeviceKind, FRAME_ALLOCATOR};
use pcics::{header::HeaderType, Header};
use spin::RwLock;
use xhci::Registers;

pub(crate) static MAPPER: RwLock<XhciMapper> = RwLock::new(XhciMapper);

pub fn xhci_init(header: &Header) -> Option<Registers<XhciMapper>> {
    if let DeviceKind::UsbController =
        DeviceKind::new(header.class_code.base as u32, header.class_code.sub as u32)
    {
        if let HeaderType::Normal(header) = header.header_type.clone() {
            let bar0 = header.base_addresses.orig()[0];
            let bar1 = header.base_addresses.orig()[1];

            let full_bar = bar0 as u64 | ((bar1 as u64) << 32);

            let regs = unsafe { Registers::new(full_bar as usize, MAPPER.write().clone()) };

            Some(regs)
        } else {
            None
        }
    } else {
        None
    }
}
