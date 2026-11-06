//! Routes guest port I/O to the devices.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::pci::{Pci, VIRTIO_IRQ};
use crate::pic::Pic;
use crate::pit::Pit;
use crate::ps2::{GuestInput, Ps2, Ps2Irq};
use crate::rtc::Rtc;
use crate::serial::{Serial, Sink};
use crate::virtio_balloon::VirtioBalloon;
use crate::virtio_blk::{GuestMemory, VirtioBlk};
use crate::virtio_input::VirtioInput;
use crate::virtio_net::VirtioNet;
use rax_input::Touch;

const IRQ_TIMER: u8 = 0;
const IRQ_KEYBOARD: u8 = 1;
const IRQ_CASCADE: u8 = 2;
const IRQ_SERIAL: u8 = 4;
/// The mouse line on the slave PIC, IRQ 12 overall.
const SLAVE_IRQ_MOUSE: u8 = 4;

const PIC1_COMMAND: u16 = 0x20;
const PIC1_DATA: u16 = 0x21;
const PIT_CHANNEL0: u16 = 0x40;
const PIT_CHANNEL2: u16 = 0x42;
const PIT_CONTROL: u16 = 0x43;
const SPEAKER_CONTROL: u16 = 0x61;
const CMOS_INDEX: u16 = 0x70;
const CMOS_DATA: u16 = 0x71;
const KEYBOARD_DATA: u16 = 0x60;
const KEYBOARD_STATUS: u16 = 0x64;
const PIC2_COMMAND: u16 = 0xa0;
const PIC2_DATA: u16 = 0xa1;
const COM1_START: u16 = 0x3f8;
const COM1_END: u16 = 0x3ff;
const PCI_ADDRESS: u16 = 0xcf8;
const PCI_DATA_START: u16 = 0xcfc;
const PCI_DATA_END: u16 = 0xcff;

const KEYBOARD_PULSE_RESET: u8 = 0xfe;
/// What an unclaimed ISA port returns.
const FLOATING_BUS: u32 = 0xffff_ffff;

/// Counts of guest activity, for reports.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct IoStats {
    /// Accesses to the serial port.
    pub serial: u64,
    /// Accesses to the PIT.
    pub timer: u64,
    /// Accesses to the real-time clock.
    pub clock: u64,
    /// Accesses to the PICs.
    pub interrupt_controller: u64,
    /// Accesses to PCI configuration space.
    pub pci: u64,
    /// Accesses to the disk registers.
    pub disk: u64,
    /// Disk requests completed.
    pub disk_requests: u64,
    /// Frames delivered to the guest.
    pub frames_in: u64,
    /// Frames sent by the guest.
    pub frames_out: u64,
    /// Pages the balloon holds at the end of the run.
    pub balloon_pages: u64,
    /// Accesses to ports with no device.
    pub unclaimed: u64,
}

/// All devices on the machine.
pub struct Bus {
    pic: Pic,
    slave_pic: Pic,
    pit: Pit,
    rtc: Rtc,
    wall_clock: fn() -> u64,
    serial: Serial,
    ps2: Ps2,
    pci: Pci,
    disks: Vec<VirtioBlk>,
    network: Option<VirtioNet>,
    touch: Option<VirtioInput>,
    balloon: Option<VirtioBalloon>,
    reset_requested: bool,
    stats: IoStats,
    unclaimed_ports: BTreeMap<u16, u64>,
}

impl Bus {
    /// Creates the machine's devices.
    #[must_use]
    pub fn new(
        console: Box<dyn Sink>,
        wall_clock: fn() -> u64,
        disks: Vec<VirtioBlk>,
        network: Option<VirtioNet>,
        touch: Option<VirtioInput>,
        gpu: bool,
        balloon: Option<VirtioBalloon>,
    ) -> Self {
        Self {
            pic: Pic::default(),
            slave_pic: Pic::default(),
            pit: Pit::default(),
            rtc: Rtc::default(),
            wall_clock,
            serial: Serial::new(console),
            ps2: Ps2::default(),
            pci: Pci::new(
                disks.len(),
                network.is_some(),
                touch.is_some(),
                gpu,
                balloon.is_some(),
            ),
            disks,
            network,
            touch,
            balloon,
            reset_requested: false,
            stats: IoStats::default(),
            unclaimed_ports: BTreeMap::new(),
        }
    }

    /// Handles `in`.
    pub fn read(&mut self, port: u16, size: u8, now_ns: u64) -> u32 {
        let value = match port {
            PIC1_COMMAND => {
                self.stats.interrupt_controller += 1;
                u32::from(self.pic.read_command())
            }
            PIC1_DATA => {
                self.stats.interrupt_controller += 1;
                u32::from(self.pic.read_data())
            }
            PIC2_COMMAND => {
                self.stats.interrupt_controller += 1;
                u32::from(self.slave_pic.read_command())
            }
            PIC2_DATA => {
                self.stats.interrupt_controller += 1;
                u32::from(self.slave_pic.read_data())
            }
            PIT_CHANNEL0 | PIT_CHANNEL2 => {
                self.stats.timer += 1;
                u32::from(
                    self.pit
                        .read_channel(usize::from(port - PIT_CHANNEL0), now_ns),
                )
            }
            SPEAKER_CONTROL => {
                self.stats.timer += 1;
                u32::from(self.pit.read_speaker_control(now_ns))
            }
            CMOS_DATA => {
                self.stats.clock += 1;
                u32::from(self.rtc.read_data((self.wall_clock)()))
            }
            KEYBOARD_DATA => {
                let byte = self.ps2.read_data();
                self.collect_ps2_irq();
                u32::from(byte)
            }
            KEYBOARD_STATUS => u32::from(self.ps2.read_status()),
            COM1_START..=COM1_END => {
                self.stats.serial += 1;
                let byte = self.serial.read(port - COM1_START);
                self.collect_serial_irq();
                u32::from(byte)
            }
            PCI_ADDRESS => {
                self.stats.pci += 1;
                self.pci.read_address()
            }
            PCI_DATA_START..=PCI_DATA_END => {
                self.stats.pci += 1;
                self.pci.read_data(port - PCI_DATA_START, size)
            }
            _ => match self.pci.virtio_offset(port) {
                Some((index, offset)) => match self.disks.get_mut(index) {
                    Some(disk) => {
                        self.stats.disk += 1;
                        disk.read(offset, size)
                    }
                    None => self
                        .network
                        .as_mut()
                        .map_or(FLOATING_BUS, |card| card.read(offset, size)),
                },
                None => {
                    self.count_unclaimed(port);
                    FLOATING_BUS
                }
            },
        };
        match size {
            1 => value & 0xff,
            2 => value & 0xffff,
            _ => value,
        }
    }

    /// Handles `out`.
    pub fn write(&mut self, port: u16, size: u8, value: u32, now_ns: u64) {
        let byte = value as u8;
        match port {
            PIC1_COMMAND => {
                self.stats.interrupt_controller += 1;
                self.pic.write_command(byte);
            }
            PIC1_DATA => {
                self.stats.interrupt_controller += 1;
                self.pic.write_data(byte);
            }
            PIC2_COMMAND => {
                self.stats.interrupt_controller += 1;
                self.slave_pic.write_command(byte);
            }
            PIC2_DATA => {
                self.stats.interrupt_controller += 1;
                self.slave_pic.write_data(byte);
            }
            PIT_CHANNEL0 | PIT_CHANNEL2 => {
                self.stats.timer += 1;
                self.pit
                    .write_channel(usize::from(port - PIT_CHANNEL0), byte, now_ns);
            }
            PIT_CONTROL => {
                self.stats.timer += 1;
                self.pit.write_control(byte, now_ns);
            }
            SPEAKER_CONTROL => {
                self.stats.timer += 1;
                self.pit.write_speaker_control(byte, now_ns);
            }
            CMOS_INDEX => {
                self.stats.clock += 1;
                self.rtc.write_index(byte);
            }
            KEYBOARD_STATUS if byte == KEYBOARD_PULSE_RESET => self.reset_requested = true,
            KEYBOARD_STATUS => {
                self.ps2.write_command(byte);
                self.collect_ps2_irq();
            }
            KEYBOARD_DATA => {
                self.ps2.write_data(byte);
                self.collect_ps2_irq();
            }
            COM1_START..=COM1_END => {
                self.stats.serial += 1;
                self.serial.write(port - COM1_START, byte);
                self.collect_serial_irq();
            }
            PCI_ADDRESS => {
                self.stats.pci += 1;
                self.pci.write_address(value);
            }
            PCI_DATA_START..=PCI_DATA_END => {
                self.stats.pci += 1;
                self.pci.write_data(port - PCI_DATA_START, size, value);
            }
            _ => match self.pci.virtio_offset(port) {
                Some((index, offset)) => match self.disks.get_mut(index) {
                    Some(disk) => {
                        self.stats.disk += 1;
                        disk.write(offset, value);
                    }
                    None => {
                        if let Some(card) = self.network.as_mut() {
                            card.write(offset, value);
                        }
                    }
                },
                None => self.count_unclaimed(port),
            },
        }
    }

    fn count_unclaimed(&mut self, port: u16) {
        self.stats.unclaimed += 1;
        *self.unclaimed_ports.entry(port).or_default() += 1;
    }

    /// The most-used ports with no device, busiest first.
    #[must_use]
    pub fn busiest_unclaimed_ports(&self, limit: usize) -> Vec<(u16, u64)> {
        let mut ports: Vec<_> = self.unclaimed_ports.iter().map(|(&p, &n)| (p, n)).collect();
        ports.sort_by_key(|&(_, count)| std::cmp::Reverse(count));
        ports.truncate(limit);
        ports
    }

    /// Runs anything the guest has queued for the disks or network. Call
    /// while the vCPU is stopped.
    pub fn service_devices(&mut self, memory: &dyn GuestMemory) {
        let mut raise = false;
        for disk in &mut self.disks {
            disk.service(memory);
            raise |= disk.take_irq();
        }
        self.stats.disk_requests = self.disks.iter().map(VirtioBlk::requests).sum();
        if let Some(card) = self.network.as_mut() {
            card.service(memory);
            raise |= card.take_irq();
            (self.stats.frames_in, self.stats.frames_out) = card.frame_counts();
        }
        if let Some(screen) = self.touch.as_mut() {
            screen.service(memory);
            raise |= screen.take_irq();
        }
        if let Some(balloon) = self.balloon.as_mut() {
            balloon.service(memory);
            raise |= balloon.take_irq();
            self.stats.balloon_pages = u64::from(balloon.pages_returned());
        }
        if raise {
            self.pic.raise(VIRTIO_IRQ);
        }
    }

    /// Lets the balloon check the guest's spare memory. Call while the vCPU
    /// is stopped.
    pub fn poll_balloon(&mut self, now_ns: u64, memory: &dyn GuestMemory) {
        if let Some(balloon) = self.balloon.as_mut() {
            balloon.poll(now_ns, memory);
            if balloon.take_irq() {
                self.pic.raise(VIRTIO_IRQ);
            }
        }
    }

    /// Whether a memory address belongs to a device.
    #[must_use]
    pub fn owns_memory(&self, gpa: u64) -> bool {
        self.pci.input_offset(gpa).is_some()
            || self.pci.gpu_offset(gpa).is_some()
            || self.pci.balloon_offset(gpa).is_some()
    }

    /// Finds the GPU register a memory address belongs to. The GPU itself
    /// lives on the vCPU thread, because OpenGL contexts cannot move.
    #[must_use]
    pub fn gpu_offset(&self, gpa: u64) -> Option<u64> {
        self.pci.gpu_offset(gpa)
    }

    /// Raises the interrupt line the virtio devices share.
    pub fn raise_virtio(&mut self) {
        self.pic.raise(VIRTIO_IRQ);
    }

    /// Handles a device memory load.
    pub fn memory_read(&mut self, gpa: u64, size: u8) -> u32 {
        if let (Some(offset), Some(balloon)) = (self.pci.balloon_offset(gpa), self.balloon.as_mut())
        {
            return balloon.read(offset, size);
        }
        match (self.pci.input_offset(gpa), self.touch.as_mut()) {
            (Some(offset), Some(screen)) => screen.read(offset, size),
            _ => FLOATING_BUS,
        }
    }

    /// Handles a device memory store.
    pub fn memory_write(&mut self, gpa: u64, value: u32) {
        if let (Some(offset), Some(balloon)) = (self.pci.balloon_offset(gpa), self.balloon.as_mut())
        {
            balloon.write(offset, value);
            return;
        }
        if let (Some(offset), Some(screen)) = (self.pci.input_offset(gpa), self.touch.as_mut()) {
            screen.write(offset, value);
        }
    }

    /// Queues finger changes for the guest.
    pub fn touch_input(&mut self, touch: Touch) {
        if let Some(screen) = self.touch.as_mut() {
            screen.touch(touch);
        }
    }

    fn collect_ps2_irq(&mut self) {
        match self.ps2.take_irq() {
            Some(Ps2Irq::Keyboard) => self.pic.raise(IRQ_KEYBOARD),
            Some(Ps2Irq::Mouse) => {
                self.slave_pic.raise(SLAVE_IRQ_MOUSE);
                self.pic.raise(IRQ_CASCADE);
            }
            None => {}
        }
    }

    /// Passes a physical keyboard or mouse event to the guest.
    pub fn guest_input(&mut self, input: GuestInput) {
        self.ps2.input(input);
        self.collect_ps2_irq();
    }

    fn collect_serial_irq(&mut self) {
        if self.serial.take_irq() {
            self.pic.raise(IRQ_SERIAL);
        }
    }

    /// Advances time. Returns true if an interrupt is now waiting.
    pub fn tick(&mut self, now_ns: u64) -> bool {
        if self.pit.poll(now_ns) {
            self.pic.raise(IRQ_TIMER);
        }
        self.pic.pending_vector().is_some()
    }

    /// When the timer next needs attention.
    #[must_use]
    pub fn next_timer_ns(&self) -> Option<u64> {
        self.pit.next_irq_ns()
    }

    /// Passes host input to the serial port.
    pub fn console_input(&mut self, bytes: &[u8]) {
        self.serial.receive(bytes);
        self.collect_serial_irq();
    }

    /// The vector to deliver next.
    #[must_use]
    pub fn pending_vector(&self) -> Option<u8> {
        let vector = self.pic.pending_vector()?;
        if self.pic.pending_irq() == Some(IRQ_CASCADE) {
            self.slave_pic.pending_vector()
        } else {
            Some(vector)
        }
    }

    /// Records that the pending vector was delivered.
    pub fn acknowledge_interrupt(&mut self) {
        if self.pic.pending_irq() == Some(IRQ_CASCADE) {
            self.slave_pic.acknowledge();
        }
        self.pic.acknowledge();
    }

    /// Whether the guest asked for a reset through the keyboard controller.
    #[must_use]
    pub fn reset_requested(&self) -> bool {
        self.reset_requested
    }

    /// Activity counters.
    #[must_use]
    pub fn stats(&self) -> IoStats {
        self.stats
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<u8>>>);

    impl Sink for Capture {
        fn put(&mut self, byte: u8) {
            self.0.lock().unwrap().push(byte);
        }
    }

    fn bus() -> (Bus, Capture) {
        let capture = Capture::default();
        (
            Bus::new(
                Box::new(capture.clone()),
                || 0,
                Vec::new(),
                None,
                None,
                false,
                None,
            ),
            capture,
        )
    }

    fn init_pic(bus: &mut Bus) {
        for (port, value) in [
            (0x20, 0x11),
            (0x21, 0x30),
            (0x21, 0x04),
            (0x21, 0x01),
            (0x21, 0x00),
        ] {
            bus.write(port, 1, value, 0);
        }
    }

    #[test]
    fn serial_output_reaches_the_console() {
        let (mut bus, capture) = bus();
        bus.write(0x3f8, 1, u32::from(b'A'), 0);
        assert_eq!(*capture.0.lock().unwrap(), b"A");
        assert_eq!(bus.stats().serial, 1);
    }

    #[test]
    fn unclaimed_ports_float_high_and_are_counted() {
        let (mut bus, _) = bus();
        assert_eq!(bus.read(0x2f8, 1, 0), 0xff);
        assert_eq!(bus.read(0xe000, 4, 0), 0xffff_ffff);
        bus.write(0x80, 1, 0, 0);
        assert_eq!(bus.stats().unclaimed, 3);
    }

    #[test]
    fn busiest_unclaimed_ports_are_ranked() {
        let (mut bus, _) = bus();
        bus.write(0x80, 1, 0, 0);
        bus.write(0x80, 1, 0, 0);
        bus.read(0x2f8, 1, 0);
        assert_eq!(
            bus.busiest_unclaimed_ports(1),
            vec![(0x80, 2)],
            "the limit keeps the busiest"
        );
    }

    #[test]
    fn timer_interrupts_flow_through_the_pic() {
        let (mut bus, _) = bus();
        init_pic(&mut bus);
        bus.write(0x43, 1, 0b0011_0100, 0);
        bus.write(0x40, 1, 0x9c, 0);
        bus.write(0x40, 1, 0x2e, 0); // 11932 ticks, about 10 ms.
        assert!(!bus.tick(1_000_000));
        assert!(bus.tick(11_000_000));
        assert_eq!(bus.pending_vector(), Some(0x30));
        bus.acknowledge_interrupt();
        assert_eq!(bus.pending_vector(), None);
    }

    #[test]
    fn console_input_raises_the_serial_irq() {
        let (mut bus, _) = bus();
        init_pic(&mut bus);
        bus.write(0x3f9, 1, 0x01, 0); // Enable RX interrupts.
        bus.console_input(b"a");
        assert_eq!(bus.pending_vector(), Some(0x34));
        assert_eq!(bus.read(0x3f8, 1, 0), u32::from(b'a'));
    }

    #[test]
    fn the_clock_never_looks_busy() {
        let (mut bus, _) = bus();
        bus.write(0x70, 1, 0x0a, 0);
        assert_eq!(bus.read(0x71, 1, 0) & 0x80, 0);
        assert_eq!(bus.stats().unclaimed, 0);
    }

    fn init_both_pics(bus: &mut Bus) {
        init_pic(bus);
        for (port, value) in [
            (0xa0, 0x11),
            (0xa1, 0x38),
            (0xa1, 0x02),
            (0xa1, 0x01),
            (0xa1, 0x00),
        ] {
            bus.write(port, 1, value, 0);
        }
    }

    #[test]
    fn keyboard_input_reaches_port_0x60_with_irq_1() {
        let (mut bus, _) = bus();
        init_pic(&mut bus);
        bus.guest_input(GuestInput::Key {
            scancode: 0x1e,
            extended: false,
            down: true,
        });
        assert_eq!(bus.pending_vector(), Some(0x31));
        assert_eq!(bus.read(0x64, 1, 0) & 1, 1);
        assert_eq!(bus.read(0x60, 1, 0), 0x1e);
    }

    #[test]
    fn mouse_interrupts_cascade_through_the_slave() {
        let (mut bus, _) = bus();
        init_both_pics(&mut bus);
        bus.write(0x64, 1, 0x60, 0); // Write config.
        bus.write(0x60, 1, 0x03, 0); // Enable both interrupts.
        bus.write(0x64, 1, 0xd4, 0); // Next byte goes to the mouse.
        bus.write(0x60, 1, 0xf4, 0); // Enable reporting.
        assert_eq!(bus.pending_vector(), Some(0x3c), "the acknowledgement");
        bus.acknowledge_interrupt();
        assert_eq!(bus.read(0x60, 1, 0), 0xfa);
        bus.write(0xa0, 1, 0x64, 0); // Slave EOI for its line 4.
        bus.write(0x20, 1, 0x62, 0); // Master EOI for the cascade.
        assert_eq!(bus.pending_vector(), None);
    }

    #[test]
    fn keyboard_controller_reset_is_reported() {
        let (mut bus, _) = bus();
        bus.write(0x64, 1, 0xaa, 0);
        assert!(!bus.reset_requested());
        bus.write(0x64, 1, 0xfe, 0);
        assert!(bus.reset_requested());
    }
}
