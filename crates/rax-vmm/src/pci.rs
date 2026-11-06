//! A minimal PCI bus using configuration mechanism #1.
//!
//! There is a host bridge at device 0 and one virtio block device at device
//! 1. Each has a 256-byte configuration header.

const ENABLE_BIT: u32 = 0x8000_0000;
const HEADER_SIZE: usize = 256;

const REG_VENDOR: usize = 0x00;
const REG_DEVICE: usize = 0x02;
const REG_COMMAND: usize = 0x04;
const REG_REVISION: usize = 0x08;
const REG_CLASS: usize = 0x0a;
const REG_BAR0: usize = 0x10;
const REG_SUBSYSTEM_VENDOR: usize = 0x2c;
const REG_SUBSYSTEM: usize = 0x2e;
const REG_INTERRUPT_LINE: usize = 0x3c;
const REG_INTERRUPT_PIN: usize = 0x3d;

const VENDOR_REDHAT: u16 = 0x1b36;
const DEVICE_HOST_BRIDGE: u16 = 0x0008;
const CLASS_HOST_BRIDGE: u16 = 0x0600;

const VENDOR_VIRTIO: u16 = 0x1af4;
/// Transitional virtio block device, which exposes the legacy interface.
const DEVICE_VIRTIO_BLK: u16 = 0x1001;
const SUBSYSTEM_VIRTIO_BLK: u16 = 2;
const CLASS_MASS_STORAGE: u16 = 0x0100;
/// Transitional virtio network device.
const DEVICE_VIRTIO_NET: u16 = 0x1000;
const SUBSYSTEM_VIRTIO_NET: u16 = 1;
const CLASS_ETHERNET: u16 = 0x0200;
/// Legacy virtio requires revision 0.
const REVISION_LEGACY: u8 = 0;

const COMMAND_IO_ENABLE: u16 = 0x0001;
const BAR_IO_SPACE: u32 = 0x0001;
/// Size of the virtio register window, in I/O ports.
const VIRTIO_BAR_SIZE: u32 = 0x40;
const VIRTIO_BAR_DEFAULT: u32 = 0xc040;
const INTERRUPT_PIN_A: u8 = 1;

/// The IRQ wired to the virtio device.
pub const VIRTIO_IRQ: u8 = 5;

fn put_u16(header: &mut [u8; HEADER_SIZE], at: usize, value: u16) {
    header[at..at + 2].copy_from_slice(&value.to_le_bytes());
}

fn put_u32(header: &mut [u8; HEADER_SIZE], at: usize, value: u32) {
    header[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

fn host_bridge_header() -> [u8; HEADER_SIZE] {
    let mut header = [0; HEADER_SIZE];
    put_u16(&mut header, REG_VENDOR, VENDOR_REDHAT);
    put_u16(&mut header, REG_DEVICE, DEVICE_HOST_BRIDGE);
    put_u16(&mut header, REG_CLASS, CLASS_HOST_BRIDGE);
    header
}

/// Which virtio device a slot holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Block,
    Net,
}

fn virtio_header(index: usize, kind: Kind) -> [u8; HEADER_SIZE] {
    let (device, subsystem, class) = match kind {
        Kind::Block => (DEVICE_VIRTIO_BLK, SUBSYSTEM_VIRTIO_BLK, CLASS_MASS_STORAGE),
        Kind::Net => (DEVICE_VIRTIO_NET, SUBSYSTEM_VIRTIO_NET, CLASS_ETHERNET),
    };
    let mut header = [0; HEADER_SIZE];
    put_u16(&mut header, REG_VENDOR, VENDOR_VIRTIO);
    put_u16(&mut header, REG_DEVICE, device);
    header[REG_REVISION] = REVISION_LEGACY;
    put_u16(&mut header, REG_CLASS, class);
    let bar = VIRTIO_BAR_DEFAULT + index as u32 * VIRTIO_BAR_SIZE;
    put_u32(&mut header, REG_BAR0, bar | BAR_IO_SPACE);
    put_u16(&mut header, REG_SUBSYSTEM_VENDOR, VENDOR_VIRTIO);
    put_u16(&mut header, REG_SUBSYSTEM, subsystem);
    header[REG_INTERRUPT_LINE] = VIRTIO_IRQ;
    header[REG_INTERRUPT_PIN] = INTERRUPT_PIN_A;
    header
}

/// The PCI configuration space.
#[derive(Debug, Clone)]
pub struct Pci {
    address: u32,
    host_bridge: [u8; HEADER_SIZE],
    /// One header per disk, at devices 1 and up.
    virtio: Vec<[u8; HEADER_SIZE]>,
}

impl Pci {
    /// Creates the bus with one virtio device per disk, then the network
    /// card if there is one.
    #[must_use]
    pub fn new(disks: usize, network: bool) -> Self {
        let kinds = (0..disks)
            .map(|_| Kind::Block)
            .chain(network.then_some(Kind::Net));
        Self {
            address: 0,
            host_bridge: host_bridge_header(),
            virtio: kinds
                .enumerate()
                .map(|(index, kind)| virtio_header(index, kind))
                .collect(),
        }
    }

    /// Handles a write to port 0xCF8.
    pub fn write_address(&mut self, value: u32) {
        self.address = value;
    }

    /// Handles a read of port 0xCF8.
    #[must_use]
    pub fn read_address(&self) -> u32 {
        self.address
    }

    /// Splits the latched address into `(device, register)` on bus 0,
    /// function 0. Anything else addresses nothing.
    fn target(&self, port_offset: u16) -> Option<(u32, usize)> {
        let bus = (self.address >> 16) & 0xff;
        let function = (self.address >> 8) & 0x07;
        if self.address & ENABLE_BIT == 0 || bus != 0 || function != 0 {
            return None;
        }
        let device = (self.address >> 11) & 0x1f;
        let register = (self.address & 0xfc) as usize + usize::from(port_offset & 3);
        Some((device, register))
    }

    fn header(&self, device: u32) -> Option<&[u8; HEADER_SIZE]> {
        match device {
            0 => Some(&self.host_bridge),
            device => self.virtio.get(device as usize - 1),
        }
    }

    /// Handles a read of the data ports. `port_offset` is the port minus 0xCFC.
    #[must_use]
    pub fn read_data(&self, port_offset: u16, size: u8) -> u32 {
        let found = self
            .target(port_offset)
            .and_then(|(device, register)| Some((self.header(device)?, register)));
        let Some((header, register)) = found else {
            return u32::MAX;
        };
        let mut bytes = [0_u8; 4];
        for (index, byte) in bytes.iter_mut().take(usize::from(size)).enumerate() {
            *byte = header.get(register + index).copied().unwrap_or(0xff);
        }
        u32::from_le_bytes(bytes)
    }

    /// Handles a write to the data ports. Only the virtio device's command
    /// register, BAR0 and interrupt line are writable.
    pub fn write_data(&mut self, port_offset: u16, size: u8, value: u32) {
        let Some((device @ 1.., register)) = self.target(port_offset) else {
            return;
        };
        let Some(header) = self.virtio.get_mut(device as usize - 1) else {
            return;
        };
        match (register, size) {
            (REG_COMMAND, 2 | 4) => put_u16(header, REG_COMMAND, value as u16),
            (REG_BAR0, 4) => {
                let address = value & !(VIRTIO_BAR_SIZE - 1);
                put_u32(header, REG_BAR0, address | BAR_IO_SPACE);
            }
            (REG_INTERRUPT_LINE, 1 | 2 | 4) => header[REG_INTERRUPT_LINE] = value as u8,
            _ => {}
        }
    }

    /// Finds the disk and register a port belongs to. Ports decode only
    /// after the guest enables I/O on that device.
    #[must_use]
    pub fn virtio_offset(&self, port: u16) -> Option<(usize, u16)> {
        self.virtio.iter().enumerate().find_map(|(index, header)| {
            let command = u16::from_le_bytes([header[REG_COMMAND], header[REG_COMMAND + 1]]);
            if command & COMMAND_IO_ENABLE == 0 {
                return None;
            }
            let bar = u32::from_le_bytes([
                header[REG_BAR0],
                header[REG_BAR0 + 1],
                header[REG_BAR0 + 2],
                header[REG_BAR0 + 3],
            ]);
            let base = bar & !(VIRTIO_BAR_SIZE - 1);
            let offset = u32::from(port).checked_sub(base)?;
            (offset < VIRTIO_BAR_SIZE).then_some((index, offset as u16))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn address(device: u32, register: u32) -> u32 {
        ENABLE_BIT | device << 11 | register
    }

    fn read(pci: &mut Pci, device: u32, register: u32, size: u8) -> u32 {
        pci.write_address(address(device, register & 0xfc));
        pci.read_data((register & 3) as u16, size)
    }

    fn write(pci: &mut Pci, device: u32, register: u32, size: u8, value: u32) {
        pci.write_address(address(device, register & 0xfc));
        pci.write_data((register & 3) as u16, size, value);
    }

    #[test]
    fn address_port_reads_back_for_the_kernel_probe() {
        let mut pci = Pci::new(1, false);
        pci.write_address(0x8000_0000);
        assert_eq!(pci.read_address(), 0x8000_0000);
    }

    #[test]
    fn the_host_bridge_identifies_itself() {
        let mut pci = Pci::new(0, false);
        assert_eq!(read(&mut pci, 0, 0, 2), u32::from(VENDOR_REDHAT));
        assert_eq!(read(&mut pci, 0, 0x0a, 2), u32::from(CLASS_HOST_BRIDGE));
    }

    #[test]
    fn virtio_identifies_as_a_legacy_block_device() {
        let mut pci = Pci::new(1, false);
        assert_eq!(
            read(&mut pci, 1, 0, 4),
            u32::from(DEVICE_VIRTIO_BLK) << 16 | u32::from(VENDOR_VIRTIO)
        );
        assert_eq!(read(&mut pci, 1, 0x2e, 2), u32::from(SUBSYSTEM_VIRTIO_BLK));
        assert_eq!(read(&mut pci, 1, 0x08, 1), 0);
        assert_eq!(read(&mut pci, 1, 0x3c, 1), u32::from(VIRTIO_IRQ));
        assert_eq!(read(&mut pci, 1, 0x3d, 1), 1);
    }

    #[test]
    fn empty_slots_read_all_ones() {
        let mut pci = Pci::new(1, false);
        assert_eq!(read(&mut pci, 2, 0, 4), u32::MAX);
        let mut without_disk = Pci::new(0, false);
        assert_eq!(read(&mut without_disk, 1, 0, 4), u32::MAX);
    }

    #[test]
    fn other_buses_and_functions_are_empty() {
        let mut pci = Pci::new(1, false);
        pci.write_address(ENABLE_BIT | 1 << 16);
        assert_eq!(pci.read_data(0, 4), u32::MAX);
        pci.write_address(ENABLE_BIT | 1 << 8);
        assert_eq!(pci.read_data(0, 4), u32::MAX);
    }

    #[test]
    fn a_disabled_address_reads_nothing() {
        let mut pci = Pci::new(1, false);
        pci.write_address(0);
        assert_eq!(pci.read_data(0, 4), u32::MAX);
    }

    #[test]
    fn bar_sizing_reports_the_window_size() {
        let mut pci = Pci::new(1, false);
        write(&mut pci, 1, 0x10, 4, u32::MAX);
        assert_eq!(read(&mut pci, 1, 0x10, 4), 0xffff_ffc1);
    }

    #[test]
    fn registers_decode_only_after_io_is_enabled() {
        let mut pci = Pci::new(1, false);
        assert_eq!(pci.virtio_offset(0xc040), None);
        write(&mut pci, 1, 0x04, 2, 1);
        assert_eq!(pci.virtio_offset(0xc040), Some((0, 0)));
        assert_eq!(pci.virtio_offset(0xc07f), Some((0, 0x3f)));
        assert_eq!(pci.virtio_offset(0xc080), None);
        assert_eq!(pci.virtio_offset(0xc03f), None);
    }

    #[test]
    fn a_moved_bar_moves_the_window() {
        let mut pci = Pci::new(1, false);
        write(&mut pci, 1, 0x04, 2, 1);
        write(&mut pci, 1, 0x10, 4, 0xd000);
        assert_eq!(pci.virtio_offset(0xd010), Some((0, 0x10)));
        assert_eq!(pci.virtio_offset(0xc040), None);
    }

    #[test]
    fn each_disk_gets_its_own_slot_and_window() {
        let mut pci = Pci::new(2, false);
        assert_eq!(read(&mut pci, 2, 0, 2), u32::from(VENDOR_VIRTIO));
        assert_eq!(read(&mut pci, 3, 0, 4), u32::MAX);
        write(&mut pci, 1, 0x04, 2, 1);
        write(&mut pci, 2, 0x04, 2, 1);
        assert_eq!(pci.virtio_offset(0xc040), Some((0, 0)));
        assert_eq!(pci.virtio_offset(0xc080), Some((1, 0)));
    }

    #[test]
    fn the_network_card_follows_the_disks() {
        let mut pci = Pci::new(1, true);
        assert_eq!(read(&mut pci, 2, 0x02, 2), u32::from(DEVICE_VIRTIO_NET));
        assert_eq!(read(&mut pci, 2, 0x0a, 2), u32::from(CLASS_ETHERNET));
        assert_eq!(read(&mut pci, 2, 0x2e, 2), u32::from(SUBSYSTEM_VIRTIO_NET));
    }

    #[test]
    fn identity_registers_cannot_be_overwritten() {
        let mut pci = Pci::new(1, false);
        write(&mut pci, 1, 0, 4, 0);
        assert_eq!(read(&mut pci, 1, 0, 2), u32::from(VENDOR_VIRTIO));
    }
}
