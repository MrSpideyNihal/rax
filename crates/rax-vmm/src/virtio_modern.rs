//! What every modern virtio device shares: the common configuration block
//! and the split virtqueue. Ring contents are untrusted.

use crate::virtio_blk::{GuestMemory, OutOfRange};

/// The driver gave up on the device, or the device gave up on the driver.
pub const STATUS_FAILED: u8 = 0x80;
/// `VIRTIO_F_VERSION_1`.
pub const VERSION_1: u64 = 1 << 32;

const ISR_QUEUE: u8 = 1;
const DESCRIPTOR_SIZE: u64 = 16;
const DESCRIPTOR_NEXT: u16 = 1;
const DESCRIPTOR_WRITE: u16 = 2;

/// One split virtqueue.
#[derive(Debug, Clone, Copy, Default)]
pub struct Queue {
    /// Entries in the ring.
    pub size: u16,
    /// Whether the driver has finished setting it up.
    pub enabled: bool,
    /// Guest address of the descriptor table.
    pub descriptors: u64,
    /// Guest address of the available ring.
    pub available: u64,
    /// Guest address of the used ring.
    pub used: u64,
    /// Next available entry to look at.
    pub last_available: u16,
}

/// A ring the driver has written something impossible into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RingError;

impl From<OutOfRange> for RingError {
    fn from(_: OutOfRange) -> Self {
        Self
    }
}

/// One request: the guest buffers the device reads, then those it writes.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Chain {
    /// First descriptor, which identifies the request when it completes.
    pub head: u16,
    /// Buffers the device reads, as `(address, length)`.
    pub readable: Vec<(u64, u32)>,
    /// Buffers the device writes.
    pub writable: Vec<(u64, u32)>,
}

fn read_u16(memory: &dyn GuestMemory, gpa: u64) -> Result<u16, RingError> {
    let mut bytes = [0; 2];
    memory.read(gpa, &mut bytes)?;
    Ok(u16::from_le_bytes(bytes))
}

// Guest-chosen queue addresses can sit anywhere, so ring arithmetic wraps and
// the bounds check in `GuestMemory` then refuses the access.

/// Takes the next request off the available ring, if there is one.
pub fn next_chain(memory: &dyn GuestMemory, queue: &mut Queue) -> Result<Option<Chain>, RingError> {
    if !queue.enabled || queue.size == 0 {
        return Ok(None);
    }
    let offered = read_u16(memory, queue.available.wrapping_add(2))?;
    if offered == queue.last_available {
        return Ok(None);
    }
    let slot = u64::from(queue.last_available % queue.size);
    let head = read_u16(memory, queue.available.wrapping_add(4 + slot * 2))?;
    let mut chain = Chain {
        head,
        ..Chain::default()
    };
    let mut index = head;
    // A chain can't be longer than the table, so a loop in it is caught here.
    for _ in 0..queue.size {
        if index >= queue.size {
            return Err(RingError);
        }
        let mut raw = [0_u8; 16];
        let at = queue
            .descriptors
            .wrapping_add(u64::from(index) * DESCRIPTOR_SIZE);
        memory.read(at, &mut raw)?;
        let address = u64::from_le_bytes(raw[0..8].try_into().map_err(|_| RingError)?);
        let length = u32::from_le_bytes(raw[8..12].try_into().map_err(|_| RingError)?);
        let flags = u16::from_le_bytes([raw[12], raw[13]]);
        if flags & DESCRIPTOR_WRITE != 0 {
            chain.writable.push((address, length));
        } else {
            chain.readable.push((address, length));
        }
        if flags & DESCRIPTOR_NEXT == 0 {
            queue.last_available = queue.last_available.wrapping_add(1);
            return Ok(Some(chain));
        }
        index = u16::from_le_bytes([raw[14], raw[15]]);
    }
    Err(RingError)
}

/// Returns a request to the driver, saying how many bytes were written.
pub fn complete(
    memory: &dyn GuestMemory,
    queue: &Queue,
    head: u16,
    written: u32,
) -> Result<(), RingError> {
    let used_index = read_u16(memory, queue.used.wrapping_add(2))?;
    let entry = queue
        .used
        .wrapping_add(4 + u64::from(used_index % queue.size) * 8);
    memory.write(entry, &u32::from(head).to_le_bytes())?;
    memory.write(entry.wrapping_add(4), &written.to_le_bytes())?;
    memory.write(
        queue.used.wrapping_add(2),
        &used_index.wrapping_add(1).to_le_bytes(),
    )?;
    Ok(())
}

fn set_low(value: &mut u64, low: u32) {
    *value = (*value & !0xffff_ffff) | u64::from(low);
}

fn set_high(value: &mut u64, high: u32) {
    *value = (*value & 0xffff_ffff) | u64::from(high) << 32;
}

fn low(value: u64) -> u32 {
    value as u32
}

fn high(value: u64) -> u32 {
    (value >> 32) as u32
}

/// The common configuration block, and the interrupt status beside it.
#[derive(Debug, Clone)]
pub struct Common {
    queues: Vec<Queue>,
    queue_limit: u16,
    features: u64,
    selected: usize,
    feature_select: u32,
    driver_feature_select: u32,
    driver_features: [u32; 2],
    status: u8,
    isr: u8,
}

impl Common {
    /// Creates a device with `queues` rings of up to `queue_limit` entries.
    #[must_use]
    pub fn new(queues: usize, queue_limit: u16, features: u64) -> Self {
        Self {
            queues: vec![Queue::default(); queues],
            queue_limit,
            features,
            selected: 0,
            feature_select: 0,
            driver_feature_select: 0,
            driver_features: [0; 2],
            status: 0,
            isr: 0,
        }
    }

    /// A copy of queue `index`, which must exist.
    #[must_use]
    pub fn queue(&self, index: usize) -> Queue {
        self.queues[index]
    }

    /// Stores a queue back after working on a copy.
    pub fn set_queue(&mut self, index: usize, queue: Queue) {
        self.queues[index] = queue;
    }

    /// Marks the device as broken.
    pub fn fail(&mut self) {
        self.status |= STATUS_FAILED;
    }

    /// Records that a queue has news for the driver.
    pub fn signal_queue(&mut self) {
        self.isr |= ISR_QUEUE;
    }

    /// Reads and clears the interrupt status.
    pub fn take_isr(&mut self) -> u8 {
        std::mem::take(&mut self.isr)
    }

    fn register(&self, offset: u64) -> u32 {
        let selected = self.queues.get(self.selected);
        let queue = selected.copied().unwrap_or_default();
        match offset {
            0x00 => self.feature_select,
            0x04 => match self.feature_select {
                0 => low(self.features),
                1 => high(self.features),
                _ => 0,
            },
            0x08 => self.driver_feature_select,
            0x0c => self.driver_features[(self.driver_feature_select & 1) as usize],
            0x20 => low(queue.descriptors),
            0x24 => high(queue.descriptors),
            0x28 => low(queue.available),
            0x2c => high(queue.available),
            0x30 => low(queue.used),
            0x34 => high(queue.used),
            _ => 0,
        }
    }

    /// Handles a read of the block.
    #[must_use]
    pub fn read(&self, offset: u64, size: u8) -> u32 {
        let aligned = offset & !3;
        let shift = (offset - aligned) * 8;
        let mask = match size {
            1 => 0xff,
            2 => 0xffff,
            _ => u32::MAX,
        };
        let selected = self.queues.get(self.selected);
        // Byte and word registers share a dword in this layout.
        let word = match aligned {
            // No MSI-X, so interrupts use the legacy line.
            0x10 => 0xffff | (self.queues.len() as u32) << 16,
            0x14 => u32::from(self.status) | (self.selected as u32) << 16,
            0x18 => selected.map_or(0, |_| u32::from(self.queue_limit)) | 0xffff << 16,
            0x1c => selected.map_or(0, |queue| u32::from(queue.enabled)),
            _ => self.register(aligned),
        };
        (word >> shift) & mask
    }

    /// Handles a write to the block. Returns true if the driver reset the
    /// device, which the caller must then do to its own state.
    pub fn write(&mut self, offset: u64, value: u32) -> bool {
        match offset {
            0x00 => self.feature_select = value,
            0x08 => self.driver_feature_select = value,
            0x0c => self.driver_features[(self.driver_feature_select & 1) as usize] = value,
            0x14 if value == 0 => {
                *self = Self::new(self.queues.len(), self.queue_limit, self.features);
                return true;
            }
            0x14 => self.status = value as u8,
            0x16 => self.selected = value as usize,
            _ => {
                let limit = self.queue_limit;
                if let Some(queue) = self.queues.get_mut(self.selected) {
                    match offset {
                        0x18 => queue.size = (value as u16).min(limit),
                        0x1c => queue.enabled = value & 1 != 0,
                        0x20 => set_low(&mut queue.descriptors, value),
                        0x24 => set_high(&mut queue.descriptors, value),
                        0x28 => set_low(&mut queue.available, value),
                        0x2c => set_high(&mut queue.available, value),
                        0x30 => set_low(&mut queue.used, value),
                        0x34 => set_high(&mut queue.used, value),
                        _ => {}
                    }
                }
            }
        }
        false
    }
}

#[cfg(test)]
pub(crate) mod testing {
    use super::*;
    use std::cell::RefCell;

    pub const DESCRIPTORS: u64 = 0x1_0000;
    pub const AVAILABLE: u64 = 0x2_0000;
    pub const USED: u64 = 0x3_0000;

    /// Guest RAM for tests.
    pub struct Ram(pub RefCell<Vec<u8>>);

    impl Ram {
        pub fn new(size: usize) -> Self {
            Self(RefCell::new(vec![0; size]))
        }

        /// Writes a descriptor. `next` of `None` ends the chain.
        pub fn descriptor(
            &self,
            index: u16,
            address: u64,
            length: u32,
            write: bool,
            next: Option<u16>,
        ) {
            let mut raw = [0_u8; 16];
            raw[0..8].copy_from_slice(&address.to_le_bytes());
            raw[8..12].copy_from_slice(&length.to_le_bytes());
            let flags = (u16::from(next.is_some()) * DESCRIPTOR_NEXT)
                | (u16::from(write) * DESCRIPTOR_WRITE);
            raw[12..14].copy_from_slice(&flags.to_le_bytes());
            raw[14..16].copy_from_slice(&next.unwrap_or(0).to_le_bytes());
            let at = DESCRIPTORS + u64::from(index) * DESCRIPTOR_SIZE;
            self.write(at, &raw).unwrap();
        }

        /// Offers `head` as available entry `position`.
        pub fn offer(&self, position: u16, head: u16) {
            self.write(AVAILABLE + 4 + u64::from(position) * 2, &head.to_le_bytes())
                .unwrap();
            self.write(AVAILABLE + 2, &(position + 1).to_le_bytes())
                .unwrap();
        }

        pub fn used_count(&self) -> u16 {
            read_u16(self, USED + 2).unwrap()
        }
    }

    impl GuestMemory for Ram {
        fn read(&self, gpa: u64, buffer: &mut [u8]) -> Result<(), OutOfRange> {
            let start = usize::try_from(gpa).map_err(|_| OutOfRange)?;
            let end = start.checked_add(buffer.len()).ok_or(OutOfRange)?;
            let ram = self.0.borrow();
            buffer.copy_from_slice(ram.get(start..end).ok_or(OutOfRange)?);
            Ok(())
        }

        fn write(&self, gpa: u64, bytes: &[u8]) -> Result<(), OutOfRange> {
            let start = usize::try_from(gpa).map_err(|_| OutOfRange)?;
            let end = start.checked_add(bytes.len()).ok_or(OutOfRange)?;
            let mut ram = self.0.borrow_mut();
            ram.get_mut(start..end)
                .ok_or(OutOfRange)?
                .copy_from_slice(bytes);
            Ok(())
        }
    }

    pub fn ready_queue(size: u16) -> Queue {
        Queue {
            size,
            enabled: true,
            descriptors: DESCRIPTORS,
            available: AVAILABLE,
            used: USED,
            last_available: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;

    #[test]
    fn a_chain_is_split_into_read_and_write_buffers() {
        let ram = Ram::new(0x4_0000);
        ram.descriptor(0, 0x100, 8, false, Some(1));
        ram.descriptor(1, 0x200, 16, true, None);
        ram.offer(0, 0);
        let mut queue = ready_queue(8);
        let chain = next_chain(&ram, &mut queue).unwrap().unwrap();
        assert_eq!(chain.readable, [(0x100, 8)]);
        assert_eq!(chain.writable, [(0x200, 16)]);
        assert_eq!(next_chain(&ram, &mut queue).unwrap(), None);
    }

    #[test]
    fn a_looping_chain_is_refused() {
        let ram = Ram::new(0x4_0000);
        ram.descriptor(0, 0x100, 8, false, Some(0));
        ram.offer(0, 0);
        assert_eq!(next_chain(&ram, &mut ready_queue(8)), Err(RingError));
    }

    #[test]
    fn a_link_past_the_table_is_refused() {
        let ram = Ram::new(0x4_0000);
        ram.descriptor(0, 0x100, 8, false, Some(9));
        ram.offer(0, 0);
        assert_eq!(next_chain(&ram, &mut ready_queue(8)), Err(RingError));
    }

    #[test]
    fn completing_advances_the_used_ring() {
        let ram = Ram::new(0x4_0000);
        complete(&ram, &ready_queue(8), 3, 24).unwrap();
        assert_eq!(ram.used_count(), 1);
        let mut entry = [0; 8];
        ram.read(USED + 4, &mut entry).unwrap();
        assert_eq!(entry, [3, 0, 0, 0, 24, 0, 0, 0]);
    }

    #[test]
    fn resetting_keeps_the_device_shape() {
        let mut common = Common::new(2, 64, VERSION_1);
        common.write(0x16, 1);
        common.write(0x1c, 1);
        assert!(common.write(0x14, 0));
        assert_eq!(common.read(0x16, 2), 0);
        assert_eq!(common.read(0x12, 2), 2);
        assert_eq!(common.read(0x18, 2), 64);
    }

    #[test]
    fn low_feature_bits_are_offered_too() {
        let mut common = Common::new(1, 8, VERSION_1 | 1);
        assert_eq!(common.read(0x04, 4), 1);
        common.write(0x00, 1);
        assert_eq!(common.read(0x04, 4), 1);
        common.write(0x00, 2);
        assert_eq!(common.read(0x04, 4), 0);
    }
}
