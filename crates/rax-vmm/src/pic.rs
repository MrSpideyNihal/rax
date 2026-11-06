//! The master 8259A interrupt controller.
//!
//! The kernel routes the PIT and the serial port through this. RAX models the
//! master only, plus enough of the slave for the kernel's probe to pass.

const ICW1_INIT: u8 = 0x10;
const ICW1_NEEDS_ICW4: u8 = 0x01;
const OCW3_MARKER: u8 = 0x08;
const OCW3_READ_ISR: u8 = 0x03;
const OCW3_READ_IRR: u8 = 0x02;
const EOI_NON_SPECIFIC: u8 = 0x20;
const EOI_SPECIFIC: u8 = 0x60;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Init {
    Done,
    Base { needs_icw4: bool },
    Cascade { needs_icw4: bool },
    Mode,
}

/// One 8259A.
#[derive(Debug, Clone)]
pub struct Pic {
    init: Init,
    base: u8,
    mask: u8,
    requested: u8,
    in_service: u8,
    read_isr: bool,
}

impl Default for Pic {
    fn default() -> Self {
        Self {
            init: Init::Done,
            base: 0x08,
            mask: 0xff,
            requested: 0,
            in_service: 0,
            read_isr: false,
        }
    }
}

impl Pic {
    /// Handles a write to the command port.
    pub fn write_command(&mut self, value: u8) {
        if value & ICW1_INIT != 0 {
            *self = Self {
                init: Init::Base {
                    needs_icw4: value & ICW1_NEEDS_ICW4 != 0,
                },
                base: self.base,
                mask: 0,
                requested: 0,
                in_service: 0,
                read_isr: false,
            };
            return;
        }
        if value & OCW3_MARKER != 0 {
            match value & 0x03 {
                OCW3_READ_ISR => self.read_isr = true,
                OCW3_READ_IRR => self.read_isr = false,
                _ => {}
            }
            return;
        }
        if value & 0xe0 == EOI_NON_SPECIFIC {
            self.in_service &= self.in_service.wrapping_sub(1);
        } else if value & 0xe0 == EOI_SPECIFIC {
            self.in_service &= !(1 << (value & 0x07));
        }
    }

    /// Handles a write to the data port.
    pub fn write_data(&mut self, value: u8) {
        self.init = match self.init {
            Init::Done => {
                self.mask = value;
                Init::Done
            }
            Init::Base { needs_icw4 } => {
                self.base = value & 0xf8;
                Init::Cascade { needs_icw4 }
            }
            Init::Cascade { needs_icw4: true } => Init::Mode,
            Init::Cascade { needs_icw4: false } | Init::Mode => Init::Done,
        };
    }

    /// Handles a read of the command port.
    #[must_use]
    pub fn read_command(&self) -> u8 {
        if self.read_isr {
            self.in_service
        } else {
            self.requested
        }
    }

    /// Handles a read of the data port.
    #[must_use]
    pub fn read_data(&self) -> u8 {
        self.mask
    }

    /// Latches a request on `irq`.
    pub fn raise(&mut self, irq: u8) {
        self.requested |= 1 << irq;
    }

    /// The vector to deliver next, without acknowledging it.
    #[must_use]
    pub fn pending_vector(&self) -> Option<u8> {
        self.pending_irq().map(|irq| self.base + irq)
    }

    /// The IRQ that would be delivered next.
    #[must_use]
    pub fn pending_irq(&self) -> Option<u8> {
        let ready = self.requested & !self.mask;
        if ready == 0 {
            return None;
        }
        let irq = ready.trailing_zeros() as u8;
        // Lower numbers have priority, so an active lower IRQ blocks this one.
        (self.in_service & ((1 << irq) | ((1 << irq) - 1)) == 0).then_some(irq)
    }

    /// Records that the pending interrupt has been delivered.
    pub fn acknowledge(&mut self) {
        if let Some(irq) = self.pending_irq() {
            self.requested &= !(1 << irq);
            self.in_service |= 1 << irq;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The sequence Linux uses: vectors at 0x30, IRQ2 cascade, 8086 mode.
    fn initialised() -> Pic {
        let mut pic = Pic::default();
        pic.write_command(0x11);
        pic.write_data(0x30);
        pic.write_data(0x04);
        pic.write_data(0x01);
        pic.write_data(0x00);
        pic
    }

    #[test]
    fn mask_reads_back_for_the_kernel_probe() {
        let mut pic = Pic::default();
        pic.write_data(0xfb);
        assert_eq!(pic.read_data(), 0xfb);
    }

    #[test]
    fn initialisation_sets_the_vector_base() {
        let mut pic = initialised();
        pic.raise(0);
        assert_eq!(pic.pending_vector(), Some(0x30));
    }

    #[test]
    fn masked_interrupts_are_held_until_unmasked() {
        let mut pic = initialised();
        pic.write_data(0x01);
        pic.raise(0);
        assert_eq!(pic.pending_vector(), None);
        pic.write_data(0x00);
        assert_eq!(pic.pending_vector(), Some(0x30));
    }

    #[test]
    fn lower_irqs_win() {
        let mut pic = initialised();
        pic.raise(4);
        pic.raise(0);
        assert_eq!(pic.pending_vector(), Some(0x30));
    }

    #[test]
    fn in_service_blocks_equal_and_lower_priority_until_eoi() {
        let mut pic = initialised();
        pic.raise(0);
        pic.acknowledge();
        pic.raise(0);
        pic.raise(4);
        assert_eq!(pic.pending_vector(), None);
        pic.write_command(EOI_NON_SPECIFIC);
        assert_eq!(pic.pending_vector(), Some(0x30));
    }

    #[test]
    fn a_higher_priority_irq_preempts() {
        let mut pic = initialised();
        pic.raise(4);
        pic.acknowledge();
        pic.raise(0);
        assert_eq!(pic.pending_vector(), Some(0x30));
    }

    #[test]
    fn specific_eoi_clears_only_that_irq() {
        let mut pic = initialised();
        pic.raise(0);
        pic.acknowledge();
        pic.raise(4);
        pic.write_command(EOI_SPECIFIC);
        assert_eq!(pic.pending_vector(), Some(0x34));
    }

    #[test]
    fn ocw3_selects_which_register_is_read() {
        let mut pic = initialised();
        pic.raise(1);
        assert_eq!(pic.read_command(), 0x02);
        pic.acknowledge();
        pic.write_command(OCW3_MARKER | OCW3_READ_ISR);
        assert_eq!(pic.read_command(), 0x02);
    }
}
