//! An I/O APIC, which routes device interrupt lines to CPUs.
//!
//! With the hypervisor emulating each CPU's local APIC, this is the only part
//! of interrupt delivery RAX still has to do, and acknowledging an interrupt
//! no longer needs a trip out of the guest. All lines behave as edges, like
//! the 8259 path before them, which suits RAX's shared virtio line.

/// Where the guest finds this device.
pub const IOAPIC_GPA: u64 = 0xfec0_0000;
/// Size of its register window.
pub const IOAPIC_SIZE: u64 = 0x1000;
/// Input lines.
pub const PINS: usize = 24;

const REG_SELECT: u64 = 0x00;
const REG_WINDOW: u64 = 0x10;
const INDEX_ID: u8 = 0x00;
const INDEX_VERSION: u8 = 0x01;
const INDEX_ARBITRATION: u8 = 0x02;
const INDEX_ROUTES: u8 = 0x10;
const VERSION: u32 = 0x11 | ((PINS as u32 - 1) << 16);

const ROUTE_MASKED: u64 = 1 << 16;
const ROUTE_LOGICAL: u64 = 1 << 11;
const DELIVERY_MASK: u64 = 7 << 8;
const DELIVERY_LOWEST: u64 = 1 << 8;
/// Vectors below this are reserved for the processor.
const FIRST_VECTOR: u8 = 16;

/// An interrupt on its way to a local APIC.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Interrupt {
    /// Interrupt vector.
    pub vector: u8,
    /// Destination APIC ID or logical mask.
    pub destination: u8,
    /// Whether `destination` is a logical mask.
    pub logical: bool,
    /// Whether any destination CPU may take it.
    pub lowest_priority: bool,
}

/// The device.
#[derive(Debug, Clone)]
pub struct IoApic {
    id: u32,
    select: u8,
    routes: [u64; PINS],
    /// Edges that arrived while masked.
    waiting: [bool; PINS],
}

impl IoApic {
    /// Creates a device with every line masked.
    #[must_use]
    pub fn new(id: u8) -> Self {
        Self {
            id: u32::from(id) << 24,
            select: 0,
            routes: [ROUTE_MASKED; PINS],
            waiting: [false; PINS],
        }
    }

    fn deliverable(&self, pin: usize) -> Option<Interrupt> {
        let route = self.routes[pin];
        let vector = route as u8;
        if route & ROUTE_MASKED != 0 || vector < FIRST_VECTOR {
            return None;
        }
        Some(Interrupt {
            vector,
            destination: (route >> 56) as u8,
            logical: route & ROUTE_LOGICAL != 0,
            lowest_priority: route & DELIVERY_MASK == DELIVERY_LOWEST,
        })
    }

    /// Signals an edge on `pin`, returning what to deliver, if anything.
    pub fn raise(&mut self, pin: usize) -> Option<Interrupt> {
        if pin >= PINS {
            return None;
        }
        let interrupt = self.deliverable(pin);
        self.waiting[pin] = interrupt.is_none();
        interrupt
    }

    fn route_of(index: u8) -> Option<(usize, bool)> {
        let offset = usize::from(index.checked_sub(INDEX_ROUTES)?);
        (offset / 2 < PINS).then_some((offset / 2, offset % 2 == 1))
    }

    /// Handles a load from the register window.
    #[must_use]
    pub fn read(&self, offset: u64) -> u32 {
        match offset {
            REG_SELECT => u32::from(self.select),
            REG_WINDOW => match self.select {
                INDEX_ID | INDEX_ARBITRATION => self.id,
                INDEX_VERSION => VERSION,
                index => Self::route_of(index).map_or(0, |(pin, high)| {
                    (self.routes[pin] >> if high { 32 } else { 0 }) as u32
                }),
            },
            _ => 0,
        }
    }

    /// Handles a store to the register window. Unmasking a line can release
    /// an edge that arrived earlier.
    pub fn write(&mut self, offset: u64, value: u32) -> Option<Interrupt> {
        match offset {
            REG_SELECT => self.select = value as u8,
            REG_WINDOW if self.select == INDEX_ID => self.id = value & 0x0f00_0000,
            REG_WINDOW => {
                let (pin, high) = Self::route_of(self.select)?;
                let route = &mut self.routes[pin];
                *route = if high {
                    (*route & 0xffff_ffff) | u64::from(value) << 32
                } else {
                    (*route & !0xffff_ffff) | u64::from(value)
                };
                if self.waiting[pin] {
                    let interrupt = self.deliverable(pin);
                    self.waiting[pin] = interrupt.is_none();
                    return interrupt;
                }
            }
            _ => {}
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route(apic: &mut IoApic, pin: u8, low: u32, high: u32) {
        apic.write(REG_SELECT, u32::from(INDEX_ROUTES + pin * 2 + 1));
        apic.write(REG_WINDOW, high);
        apic.write(REG_SELECT, u32::from(INDEX_ROUTES + pin * 2));
        apic.write(REG_WINDOW, low);
    }

    #[test]
    fn linux_can_read_the_version_and_line_count() {
        let mut apic = IoApic::new(1);
        apic.write(REG_SELECT, u32::from(INDEX_VERSION));
        let version = apic.read(REG_WINDOW);
        assert_eq!(version & 0xff, 0x11);
        assert_eq!((version >> 16) as usize + 1, PINS);
    }

    #[test]
    fn the_id_sits_in_the_top_byte() {
        let mut apic = IoApic::new(3);
        apic.write(REG_SELECT, u32::from(INDEX_ID));
        assert_eq!(apic.read(REG_WINDOW), 3 << 24);
    }

    #[test]
    fn every_line_starts_masked() {
        let mut apic = IoApic::new(1);
        assert_eq!(apic.raise(5), None);
        apic.write(REG_SELECT, u32::from(INDEX_ROUTES));
        assert_ne!(u64::from(apic.read(REG_WINDOW)) & ROUTE_MASKED, 0);
    }

    #[test]
    fn a_route_reads_back_in_two_halves() {
        let mut apic = IoApic::new(1);
        route(&mut apic, 2, 0x31, 0x0100_0000);
        apic.write(REG_SELECT, u32::from(INDEX_ROUTES + 4));
        assert_eq!(apic.read(REG_WINDOW), 0x31);
        apic.write(REG_SELECT, u32::from(INDEX_ROUTES + 5));
        assert_eq!(apic.read(REG_WINDOW), 0x0100_0000);
    }

    #[test]
    fn an_unmasked_line_delivers_its_route() {
        let mut apic = IoApic::new(1);
        route(&mut apic, 5, 0x41, 0x0200_0000);
        assert_eq!(
            apic.raise(5),
            Some(Interrupt {
                vector: 0x41,
                destination: 2,
                logical: false,
                lowest_priority: false,
            })
        );
    }

    #[test]
    fn logical_and_lowest_priority_are_reported() {
        let mut apic = IoApic::new(1);
        route(&mut apic, 5, 0x41 | 1 << 8 | 1 << 11, 0xff00_0000);
        let interrupt = apic.raise(5).unwrap();
        assert!(interrupt.logical && interrupt.lowest_priority);
        assert_eq!(interrupt.destination, 0xff);
    }

    #[test]
    fn an_edge_on_a_masked_line_waits_for_unmasking() {
        let mut apic = IoApic::new(1);
        route(&mut apic, 5, 0x41 | ROUTE_MASKED as u32, 0);
        assert_eq!(apic.raise(5), None);
        apic.write(REG_SELECT, u32::from(INDEX_ROUTES + 10));
        assert_eq!(apic.write(REG_WINDOW, 0x41).map(|i| i.vector), Some(0x41));
        assert_eq!(apic.write(REG_WINDOW, 0x41), None, "delivered only once");
    }

    #[test]
    fn reserved_vectors_are_never_delivered() {
        let mut apic = IoApic::new(1);
        route(&mut apic, 5, 0x05, 0);
        assert_eq!(apic.raise(5), None);
    }

    #[test]
    fn indexes_past_the_last_line_are_ignored() {
        let mut apic = IoApic::new(1);
        apic.write(REG_SELECT, 0xff);
        assert_eq!(apic.read(REG_WINDOW), 0);
        assert_eq!(apic.write(REG_WINDOW, 1), None);
        assert_eq!(apic.raise(PINS), None);
    }
}
