//! A 16550A serial port.

use std::collections::VecDeque;

const REG_DATA: u16 = 0;
const REG_IER: u16 = 1;
const REG_IIR_FCR: u16 = 2;
const REG_LCR: u16 = 3;
const REG_MCR: u16 = 4;
const REG_LSR: u16 = 5;
const REG_MSR: u16 = 6;
const REG_SCRATCH: u16 = 7;

const LCR_DLAB: u8 = 0x80;
const IER_RX: u8 = 0x01;
const IER_THRE: u8 = 0x02;
const IIR_NONE: u8 = 0x01;
const IIR_THRE: u8 = 0x02;
const IIR_RX: u8 = 0x04;
const IIR_FIFO_ENABLED: u8 = 0xc0;
const FCR_ENABLE: u8 = 0x01;
const FCR_CLEAR_RX: u8 = 0x02;
const LSR_DATA_READY: u8 = 0x01;
const LSR_TRANSMITTER_IDLE: u8 = 0x60;
const MCR_LOOPBACK: u8 = 0x10;
const MSR_CARRIER_ASSERTED: u8 = 0xb0;

/// Receives bytes the guest transmits.
pub trait Sink: Send {
    /// Accepts one transmitted byte.
    fn put(&mut self, byte: u8);
}

/// One UART.
pub struct Serial {
    sink: Box<dyn Sink>,
    received: VecDeque<u8>,
    ier: u8,
    lcr: u8,
    mcr: u8,
    fcr: u8,
    scratch: u8,
    divisor: [u8; 2],
    thre_pending: bool,
    irq_requested: bool,
}

impl Serial {
    /// Creates a UART that transmits into `sink`.
    #[must_use]
    pub fn new(sink: Box<dyn Sink>) -> Self {
        Self {
            sink,
            received: VecDeque::new(),
            ier: 0,
            lcr: 0,
            mcr: 0,
            fcr: 0,
            scratch: 0,
            divisor: [12, 0],
            thre_pending: false,
            irq_requested: false,
        }
    }

    fn dlab(&self) -> bool {
        self.lcr & LCR_DLAB != 0
    }

    fn update_irq(&mut self) {
        let rx = self.ier & IER_RX != 0 && !self.received.is_empty();
        let tx = self.ier & IER_THRE != 0 && self.thre_pending;
        if rx || tx {
            self.irq_requested = true;
        }
    }

    /// Returns true once for each interrupt request since the last call.
    pub fn take_irq(&mut self) -> bool {
        std::mem::take(&mut self.irq_requested)
    }

    /// Queues bytes for the guest to read.
    pub fn receive(&mut self, bytes: &[u8]) {
        self.received.extend(bytes);
        self.update_irq();
    }

    /// Handles a guest write to `offset`.
    pub fn write(&mut self, offset: u16, value: u8) {
        match offset {
            REG_DATA if self.dlab() => self.divisor[0] = value,
            REG_DATA => {
                if self.mcr & MCR_LOOPBACK != 0 {
                    self.received.push_back(value);
                } else {
                    self.sink.put(value);
                }
                self.thre_pending = true;
            }
            REG_IER if self.dlab() => self.divisor[1] = value,
            REG_IER => {
                let enabling_thre = value & IER_THRE != 0 && self.ier & IER_THRE == 0;
                self.ier = value & 0x0f;
                if enabling_thre {
                    self.thre_pending = true;
                }
            }
            REG_IIR_FCR => {
                self.fcr = value;
                if value & FCR_CLEAR_RX != 0 {
                    self.received.clear();
                }
            }
            REG_LCR => self.lcr = value,
            REG_MCR => self.mcr = value,
            REG_SCRATCH => self.scratch = value,
            _ => {}
        }
        self.update_irq();
    }

    /// Handles a guest read of `offset`.
    pub fn read(&mut self, offset: u16) -> u8 {
        match offset {
            REG_DATA if self.dlab() => self.divisor[0],
            REG_DATA => {
                let byte = self.received.pop_front().unwrap_or(0);
                self.update_irq();
                byte
            }
            REG_IER if self.dlab() => self.divisor[1],
            REG_IER => self.ier,
            REG_IIR_FCR => self.read_iir(),
            REG_LCR => self.lcr,
            REG_MCR => self.mcr,
            REG_LSR => {
                let ready = !self.received.is_empty();
                LSR_TRANSMITTER_IDLE | if ready { LSR_DATA_READY } else { 0 }
            }
            REG_MSR => MSR_CARRIER_ASSERTED,
            REG_SCRATCH => self.scratch,
            _ => 0,
        }
    }

    fn read_iir(&mut self) -> u8 {
        let fifo = if self.fcr & FCR_ENABLE != 0 {
            IIR_FIFO_ENABLED
        } else {
            0
        };
        let cause = if self.ier & IER_RX != 0 && !self.received.is_empty() {
            IIR_RX
        } else if self.ier & IER_THRE != 0 && self.thre_pending {
            self.thre_pending = false;
            IIR_THRE
        } else {
            IIR_NONE
        };
        fifo | cause
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

    fn uart() -> (Serial, Capture) {
        let capture = Capture::default();
        (Serial::new(Box::new(capture.clone())), capture)
    }

    #[test]
    fn transmitted_bytes_reach_the_sink() {
        let (mut serial, capture) = uart();
        for byte in b"hi" {
            serial.write(REG_DATA, *byte);
        }
        assert_eq!(*capture.0.lock().unwrap(), b"hi");
    }

    #[test]
    fn transmitter_always_reports_idle() {
        let (mut serial, _) = uart();
        assert_eq!(serial.read(REG_LSR), LSR_TRANSMITTER_IDLE);
    }

    #[test]
    fn dlab_redirects_to_the_divisor() {
        let (mut serial, capture) = uart();
        serial.write(REG_LCR, LCR_DLAB);
        serial.write(REG_DATA, 1);
        serial.write(REG_IER, 2);
        assert_eq!(serial.read(REG_DATA), 1);
        assert_eq!(serial.read(REG_IER), 2);
        serial.write(REG_LCR, 0x03);
        assert_eq!(serial.read(REG_IER), 0);
        assert!(capture.0.lock().unwrap().is_empty());
    }

    #[test]
    fn scratch_register_round_trips() {
        let (mut serial, _) = uart();
        serial.write(REG_SCRATCH, 0x5a);
        assert_eq!(serial.read(REG_SCRATCH), 0x5a);
    }

    #[test]
    fn fifo_identifies_as_16550a() {
        let (mut serial, _) = uart();
        serial.write(REG_IIR_FCR, FCR_ENABLE);
        assert_eq!(
            serial.read(REG_IIR_FCR) & IIR_FIFO_ENABLED,
            IIR_FIFO_ENABLED
        );
    }

    #[test]
    fn received_bytes_set_data_ready_and_raise_an_irq() {
        let (mut serial, _) = uart();
        serial.write(REG_IER, IER_RX);
        serial.receive(b"a");
        assert!(serial.take_irq());
        assert!(!serial.take_irq());
        assert_ne!(serial.read(REG_LSR) & LSR_DATA_READY, 0);
        assert_eq!(serial.read(REG_IIR_FCR) & 0x0f, IIR_RX);
        assert_eq!(serial.read(REG_DATA), b'a');
        assert_eq!(serial.read(REG_LSR) & LSR_DATA_READY, 0);
    }

    #[test]
    fn enabling_thre_interrupts_fires_because_the_transmitter_is_empty() {
        let (mut serial, _) = uart();
        serial.write(REG_IER, IER_THRE);
        assert!(serial.take_irq());
        assert_eq!(serial.read(REG_IIR_FCR) & 0x0f, IIR_THRE);
        assert_eq!(serial.read(REG_IIR_FCR) & 0x0f, IIR_NONE);
    }

    #[test]
    fn each_transmitted_byte_requests_the_next() {
        let (mut serial, _) = uart();
        serial.write(REG_IER, IER_THRE);
        let _ = serial.take_irq();
        let _ = serial.read(REG_IIR_FCR);
        serial.write(REG_DATA, b'x');
        assert!(serial.take_irq());
    }

    #[test]
    fn no_interrupt_without_enable() {
        let (mut serial, _) = uart();
        serial.write(REG_DATA, b'x');
        serial.receive(b"y");
        assert!(!serial.take_irq());
    }

    #[test]
    fn loopback_returns_transmitted_bytes() {
        let (mut serial, capture) = uart();
        serial.write(REG_MCR, MCR_LOOPBACK);
        serial.write(REG_DATA, 0x42);
        assert_eq!(serial.read(REG_DATA), 0x42);
        assert!(capture.0.lock().unwrap().is_empty());
    }

    #[test]
    fn clearing_the_fifo_drops_received_data() {
        let (mut serial, _) = uart();
        serial.receive(b"abc");
        serial.write(REG_IIR_FCR, FCR_CLEAR_RX);
        assert_eq!(serial.read(REG_LSR) & LSR_DATA_READY, 0);
    }
}
