//! A legacy virtio network card over port I/O.
//!
//! Queue 0 carries frames to the guest and queue 1 carries frames from it.
//! Each frame has a 10-byte header in front, which stays zero because no
//! offloads are offered. As with the disk, ring contents are untrusted.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use crate::virtio_blk::{GuestMemory, OutOfRange};

const REG_HOST_FEATURES: u16 = 0x00;
const REG_GUEST_FEATURES: u16 = 0x04;
const REG_QUEUE_PFN: u16 = 0x08;
const REG_QUEUE_SIZE: u16 = 0x0c;
const REG_QUEUE_SELECT: u16 = 0x0e;
const REG_QUEUE_NOTIFY: u16 = 0x10;
const REG_STATUS: u16 = 0x12;
const REG_ISR: u16 = 0x13;
const REG_CONFIG: u16 = 0x14;

const FEATURE_MAC: u32 = 1 << 5;
const FEATURE_STATUS: u32 = 1 << 16;
const LINK_UP: u8 = 1;
const STATUS_FAILED: u8 = 0x80;
const ISR_QUEUE: u8 = 1;

const QUEUE_SIZE: u16 = 256;
const PAGE: u64 = 4096;
const DESCRIPTOR_SIZE: u64 = 16;
const DESCRIPTOR_NEXT: u16 = 1;
const DESCRIPTOR_WRITE: u16 = 2;
const HEADER_SIZE: usize = 10;
/// Largest frame accepted from the guest.
const MAX_FRAME: usize = 65_550;

const RX: usize = 0;
const TX: usize = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RingError;

impl From<OutOfRange> for RingError {
    fn from(_: OutOfRange) -> Self {
        Self
    }
}

#[derive(Debug, Clone, Copy)]
struct Descriptor {
    address: u64,
    length: u32,
    flags: u16,
}

fn read_u16(memory: &dyn GuestMemory, gpa: u64) -> Result<u16, RingError> {
    let mut bytes = [0; 2];
    memory.read(gpa, &mut bytes)?;
    Ok(u16::from_le_bytes(bytes))
}

#[derive(Debug, Clone, Copy, Default)]
struct Queue {
    pfn: u32,
    last_available: u16,
}

impl Queue {
    fn table(self) -> u64 {
        u64::from(self.pfn) * PAGE
    }

    fn available(self) -> u64 {
        self.table() + u64::from(QUEUE_SIZE) * DESCRIPTOR_SIZE
    }

    fn used(self) -> u64 {
        (self.available() + 6 + 2 * u64::from(QUEUE_SIZE)).next_multiple_of(PAGE)
    }

    /// Takes the next buffer the guest has offered.
    fn next_head(&mut self, memory: &dyn GuestMemory) -> Result<Option<u16>, RingError> {
        let offered = read_u16(memory, self.available() + 2)?;
        if offered == self.last_available {
            return Ok(None);
        }
        let slot = u64::from(self.last_available % QUEUE_SIZE);
        let head = read_u16(memory, self.available() + 4 + slot * 2)?;
        self.last_available = self.last_available.wrapping_add(1);
        Ok(Some(head))
    }

    /// Walks a chain. The length limit stops a descriptor loop.
    fn chain(self, memory: &dyn GuestMemory, head: u16) -> Result<Vec<Descriptor>, RingError> {
        let mut chain = Vec::new();
        let mut index = head;
        loop {
            if index >= QUEUE_SIZE {
                return Err(RingError);
            }
            let mut raw = [0_u8; 16];
            memory.read(self.table() + u64::from(index) * DESCRIPTOR_SIZE, &mut raw)?;
            let [
                a0,
                a1,
                a2,
                a3,
                a4,
                a5,
                a6,
                a7,
                l0,
                l1,
                l2,
                l3,
                f0,
                f1,
                n0,
                n1,
            ] = raw;
            let flags = u16::from_le_bytes([f0, f1]);
            chain.push(Descriptor {
                address: u64::from_le_bytes([a0, a1, a2, a3, a4, a5, a6, a7]),
                length: u32::from_le_bytes([l0, l1, l2, l3]),
                flags,
            });
            if flags & DESCRIPTOR_NEXT == 0 {
                return Ok(chain);
            }
            if chain.len() >= usize::from(QUEUE_SIZE) {
                return Err(RingError);
            }
            index = u16::from_le_bytes([n0, n1]);
        }
    }

    /// Returns a finished buffer to the guest.
    fn complete(self, memory: &dyn GuestMemory, head: u16, length: u32) -> Result<(), RingError> {
        let used_index = read_u16(memory, self.used() + 2)?;
        let entry = self.used() + 4 + u64::from(used_index % QUEUE_SIZE) * 8;
        memory.write(entry, &u32::from(head).to_le_bytes())?;
        memory.write(entry + 4, &length.to_le_bytes())?;
        memory.write(self.used() + 2, &used_index.wrapping_add(1).to_le_bytes())?;
        Ok(())
    }
}

/// Where frames from the guest go.
pub type Transmit = Box<dyn FnMut(Vec<u8>) + Send>;

/// One network card.
pub struct VirtioNet {
    mac: [u8; 6],
    transmit: Transmit,
    receive: Arc<Mutex<VecDeque<Vec<u8>>>>,
    queues: [Queue; 2],
    selected: usize,
    guest_features: u32,
    status: u8,
    isr: u8,
    transmit_notified: bool,
    irq_requested: bool,
    received: u64,
    transmitted: u64,
}

impl VirtioNet {
    /// Creates a card. Frames in `receive` are delivered when the guest has
    /// buffers, and frames the guest sends go to `transmit`.
    #[must_use]
    pub fn new(mac: [u8; 6], transmit: Transmit, receive: Arc<Mutex<VecDeque<Vec<u8>>>>) -> Self {
        Self {
            mac,
            transmit,
            receive,
            queues: [Queue::default(); 2],
            selected: 0,
            guest_features: 0,
            status: 0,
            isr: 0,
            transmit_notified: false,
            irq_requested: false,
            received: 0,
            transmitted: 0,
        }
    }

    /// Frames delivered to and taken from the guest.
    #[must_use]
    pub fn frame_counts(&self) -> (u64, u64) {
        (self.received, self.transmitted)
    }

    /// Returns true once for each interrupt since the last call.
    pub fn take_irq(&mut self) -> bool {
        std::mem::take(&mut self.irq_requested)
    }

    fn config_byte(&self, offset: u16) -> u8 {
        match offset {
            0..=5 => self.mac[usize::from(offset)],
            6 => LINK_UP,
            _ => 0,
        }
    }

    /// Handles a read of the register window.
    pub fn read(&mut self, offset: u16, size: u8) -> u32 {
        match offset {
            REG_HOST_FEATURES => FEATURE_MAC | FEATURE_STATUS,
            REG_GUEST_FEATURES => self.guest_features,
            REG_QUEUE_PFN => self.queues.get(self.selected).map_or(0, |queue| queue.pfn),
            REG_QUEUE_SIZE if self.selected < self.queues.len() => u32::from(QUEUE_SIZE),
            REG_QUEUE_SIZE => 0,
            REG_STATUS => u32::from(self.status),
            REG_ISR => u32::from(std::mem::take(&mut self.isr)),
            REG_CONFIG.. => {
                let mut bytes = [0_u8; 4];
                for (index, byte) in bytes.iter_mut().take(usize::from(size)).enumerate() {
                    *byte = self.config_byte(offset - REG_CONFIG + index as u16);
                }
                u32::from_le_bytes(bytes)
            }
            _ => 0,
        }
    }

    /// Handles a write to the register window.
    pub fn write(&mut self, offset: u16, value: u32) {
        match offset {
            REG_GUEST_FEATURES => self.guest_features = value,
            REG_QUEUE_PFN => {
                if let Some(queue) = self.queues.get_mut(self.selected) {
                    *queue = Queue {
                        pfn: value,
                        last_available: 0,
                    };
                }
            }
            REG_QUEUE_SELECT => self.selected = value as usize,
            REG_QUEUE_NOTIFY if value as usize == TX => self.transmit_notified = true,
            REG_STATUS if value as u8 == 0 => {
                self.queues = [Queue::default(); 2];
                self.guest_features = 0;
                self.status = 0;
                self.isr = 0;
                self.transmit_notified = false;
            }
            REG_STATUS => self.status = value as u8,
            _ => {}
        }
    }

    /// Moves frames in both directions. Call while the vCPU is stopped.
    pub fn service(&mut self, memory: &dyn GuestMemory) {
        let sent = if std::mem::take(&mut self.transmit_notified) {
            self.transmit_frames(memory)
        } else {
            Ok(false)
        };
        let result = sent.and_then(|sent| Ok(self.receive_frames(memory)? || sent));
        match result {
            Ok(true) => {
                self.isr |= ISR_QUEUE;
                self.irq_requested = true;
            }
            Ok(false) => {}
            Err(RingError) => self.status |= STATUS_FAILED,
        }
    }

    fn transmit_frames(&mut self, memory: &dyn GuestMemory) -> Result<bool, RingError> {
        let mut queue = self.queues[TX];
        if queue.pfn == 0 {
            return Ok(false);
        }
        let mut completed = false;
        while let Some(head) = queue.next_head(memory)? {
            let chain = queue.chain(memory, head)?;
            let total: usize = chain.iter().map(|part| part.length as usize).sum();
            if (HEADER_SIZE..=MAX_FRAME + HEADER_SIZE).contains(&total) {
                let mut packet = vec![0_u8; total];
                let mut filled = 0;
                for part in &chain {
                    let end = filled + part.length as usize;
                    memory.read(part.address, &mut packet[filled..end])?;
                    filled = end;
                }
                packet.drain(..HEADER_SIZE);
                (self.transmit)(packet);
                self.transmitted += 1;
            }
            queue.complete(memory, head, 0)?;
            completed = true;
        }
        self.queues[TX] = queue;
        Ok(completed)
    }

    fn next_frame(&self) -> Option<Vec<u8>> {
        self.receive.lock().ok()?.pop_front()
    }

    fn receive_frames(&mut self, memory: &dyn GuestMemory) -> Result<bool, RingError> {
        let mut queue = self.queues[RX];
        if queue.pfn == 0 {
            return Ok(false);
        }
        let mut completed = false;
        loop {
            let empty = self.receive.lock().map_or(true, |frames| frames.is_empty());
            if empty {
                break;
            }
            // No buffer means the frame simply waits, so nothing is lost.
            let Some(head) = queue.next_head(memory)? else {
                break;
            };
            let chain = queue.chain(memory, head)?;
            let Some(frame) = self.next_frame() else {
                break;
            };
            let written = write_frame(memory, &chain, &frame)?;
            queue.complete(memory, head, written)?;
            self.received += 1;
            completed = true;
        }
        self.queues[RX] = queue;
        Ok(completed)
    }
}

/// Writes the header and frame across a chain of writable buffers.
fn write_frame(
    memory: &dyn GuestMemory,
    chain: &[Descriptor],
    frame: &[u8],
) -> Result<u32, RingError> {
    let mut packet = vec![0_u8; HEADER_SIZE];
    packet.extend_from_slice(frame);
    let mut remaining = packet.as_slice();
    for part in chain {
        if part.flags & DESCRIPTOR_WRITE == 0 {
            return Err(RingError);
        }
        let take = remaining.len().min(part.length as usize);
        memory.write(part.address, &remaining[..take])?;
        remaining = &remaining[take..];
    }
    // A frame too large for the buffer is cut short, and the guest's own
    // length checks reject it, which matches a real card.
    Ok((packet.len() - remaining.len()) as u32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    const RAM: usize = 0x20_0000;
    const RX_PFN: u32 = 0x10;
    const TX_PFN: u32 = 0x20;
    const BUFFER: u64 = 0x10_0000;
    const MAC: [u8; 6] = [1, 2, 3, 4, 5, 6];

    struct Ram(RefCell<Vec<u8>>);

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

    struct Rig {
        card: VirtioNet,
        ram: Ram,
        sent: Arc<Mutex<Vec<Vec<u8>>>>,
        incoming: Arc<Mutex<VecDeque<Vec<u8>>>>,
    }

    fn rig() -> Rig {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let incoming = Arc::new(Mutex::new(VecDeque::new()));
        let capture = Arc::clone(&sent);
        let mut card = VirtioNet::new(
            MAC,
            Box::new(move |frame| capture.lock().unwrap().push(frame)),
            Arc::clone(&incoming),
        );
        card.write(REG_QUEUE_SELECT, 0);
        card.write(REG_QUEUE_PFN, RX_PFN);
        card.write(REG_QUEUE_SELECT, 1);
        card.write(REG_QUEUE_PFN, TX_PFN);
        Rig {
            card,
            ram: Ram(RefCell::new(vec![0; RAM])),
            sent,
            incoming,
        }
    }

    impl Rig {
        fn queue(pfn: u32) -> Queue {
            Queue {
                pfn,
                last_available: 0,
            }
        }

        fn put_descriptor(
            &self,
            pfn: u32,
            index: u16,
            address: u64,
            length: u32,
            flags: u16,
            next: u16,
        ) {
            let mut raw = [0_u8; 16];
            raw[0..8].copy_from_slice(&address.to_le_bytes());
            raw[8..12].copy_from_slice(&length.to_le_bytes());
            raw[12..14].copy_from_slice(&flags.to_le_bytes());
            raw[14..16].copy_from_slice(&next.to_le_bytes());
            let gpa = Self::queue(pfn).table() + u64::from(index) * DESCRIPTOR_SIZE;
            self.ram.write(gpa, &raw).unwrap();
        }

        fn offer(&self, pfn: u32, head: u16) {
            let available = Self::queue(pfn).available();
            let index = read_u16(&self.ram, available + 2).unwrap();
            let slot = u64::from(index % QUEUE_SIZE);
            self.ram
                .write(available + 4 + slot * 2, &head.to_le_bytes())
                .unwrap();
            self.ram
                .write(available + 2, &index.wrapping_add(1).to_le_bytes())
                .unwrap();
        }

        fn used_index(&self, pfn: u32) -> u16 {
            read_u16(&self.ram, Self::queue(pfn).used() + 2).unwrap()
        }

        fn offer_rx_buffer(&self, index: u16, length: u32) {
            self.put_descriptor(
                RX_PFN,
                index,
                BUFFER + u64::from(index) * 0x1000,
                length,
                DESCRIPTOR_WRITE,
                0,
            );
            self.offer(RX_PFN, index);
        }

        fn send_frame(&mut self, frame: &[u8]) {
            let mut data = vec![0_u8; HEADER_SIZE];
            data.extend_from_slice(frame);
            self.ram.write(BUFFER, &data).unwrap();
            self.put_descriptor(TX_PFN, 0, BUFFER, data.len() as u32, 0, 0);
            self.offer(TX_PFN, 0);
            self.card.write(REG_QUEUE_NOTIFY, TX as u32);
            self.card.service(&self.ram);
        }

        fn failed(&mut self) -> bool {
            self.card.read(REG_STATUS, 1) as u8 & STATUS_FAILED != 0
        }
    }

    #[test]
    fn config_exposes_the_mac_and_link() {
        let mut rig = rig();
        assert_eq!(rig.card.read(REG_CONFIG, 1), 1);
        assert_eq!(rig.card.read(REG_CONFIG + 5, 1), 6);
        assert_eq!(rig.card.read(REG_CONFIG + 6, 2), u32::from(LINK_UP));
    }

    #[test]
    fn features_offer_no_offloads() {
        let mut rig = rig();
        assert_eq!(
            rig.card.read(REG_HOST_FEATURES, 4),
            FEATURE_MAC | FEATURE_STATUS
        );
    }

    #[test]
    fn each_queue_keeps_its_own_address() {
        let mut rig = rig();
        rig.card.write(REG_QUEUE_SELECT, 0);
        assert_eq!(rig.card.read(REG_QUEUE_PFN, 4), RX_PFN);
        rig.card.write(REG_QUEUE_SELECT, 1);
        assert_eq!(rig.card.read(REG_QUEUE_PFN, 4), TX_PFN);
    }

    #[test]
    fn a_third_queue_does_not_exist() {
        let mut rig = rig();
        rig.card.write(REG_QUEUE_SELECT, 2);
        assert_eq!(rig.card.read(REG_QUEUE_SIZE, 2), 0);
    }

    #[test]
    fn sent_frames_lose_their_header() {
        let mut rig = rig();
        rig.send_frame(b"hello");
        assert_eq!(*rig.sent.lock().unwrap(), [b"hello".to_vec()]);
        assert_eq!(rig.used_index(TX_PFN), 1);
    }

    #[test]
    fn sending_raises_an_interrupt() {
        let mut rig = rig();
        rig.send_frame(b"x");
        assert!(rig.card.take_irq());
        assert_eq!(rig.card.read(REG_ISR, 1), 1);
        assert_eq!(rig.card.read(REG_ISR, 1), 0);
    }

    #[test]
    fn received_frames_gain_a_zero_header() {
        let mut rig = rig();
        rig.offer_rx_buffer(0, 2048);
        rig.incoming.lock().unwrap().push_back(b"data".to_vec());
        rig.card.service(&rig.ram);
        let mut bytes = [0xee; 14];
        rig.ram.read(BUFFER, &mut bytes).unwrap();
        assert_eq!(&bytes[..10], &[0; 10]);
        assert_eq!(&bytes[10..], b"data");
        assert_eq!(rig.used_index(RX_PFN), 1);
        assert!(rig.card.take_irq());
    }

    #[test]
    fn frames_wait_until_the_guest_has_buffers() {
        let mut rig = rig();
        rig.incoming.lock().unwrap().push_back(b"later".to_vec());
        rig.card.service(&rig.ram);
        assert_eq!(rig.incoming.lock().unwrap().len(), 1, "nothing was lost");
        rig.offer_rx_buffer(0, 2048);
        rig.card.service(&rig.ram);
        assert!(rig.incoming.lock().unwrap().is_empty());
        assert_eq!(rig.card.frame_counts(), (1, 0));
    }

    #[test]
    fn a_short_buffer_cuts_the_frame_without_overrunning() {
        let mut rig = rig();
        rig.offer_rx_buffer(0, 12);
        rig.ram.write(BUFFER + 12, &[0xaa]).unwrap();
        rig.incoming.lock().unwrap().push_back(vec![1; 100]);
        rig.card.service(&rig.ram);
        let mut after = [0];
        rig.ram.read(BUFFER + 12, &mut after).unwrap();
        assert_eq!(after, [0xaa]);
    }

    #[test]
    fn read_only_receive_buffers_stop_the_card() {
        let mut rig = rig();
        rig.put_descriptor(RX_PFN, 0, BUFFER, 2048, 0, 0);
        rig.offer(RX_PFN, 0);
        rig.incoming.lock().unwrap().push_back(vec![1; 10]);
        rig.card.service(&rig.ram);
        assert!(rig.failed());
    }

    #[test]
    fn a_descriptor_loop_stops_the_card() {
        let mut rig = rig();
        rig.put_descriptor(TX_PFN, 0, BUFFER, 16, DESCRIPTOR_NEXT, 1);
        rig.put_descriptor(TX_PFN, 1, BUFFER, 16, DESCRIPTOR_NEXT, 0);
        rig.offer(TX_PFN, 0);
        rig.card.write(REG_QUEUE_NOTIFY, TX as u32);
        rig.card.service(&rig.ram);
        assert!(rig.failed());
    }

    #[test]
    fn an_out_of_range_buffer_stops_the_card_without_panicking() {
        let mut rig = rig();
        rig.put_descriptor(TX_PFN, 0, u64::MAX - 4, 100, 0, 0);
        rig.offer(TX_PFN, 0);
        rig.card.write(REG_QUEUE_NOTIFY, TX as u32);
        rig.card.service(&rig.ram);
        assert!(rig.failed());
    }

    #[test]
    fn an_oversized_transmit_is_dropped_before_allocating() {
        let mut rig = rig();
        rig.put_descriptor(TX_PFN, 0, BUFFER, u32::MAX, 0, 0);
        rig.offer(TX_PFN, 0);
        rig.card.write(REG_QUEUE_NOTIFY, TX as u32);
        rig.card.service(&rig.ram);
        assert!(rig.sent.lock().unwrap().is_empty());
        assert_eq!(rig.used_index(TX_PFN), 1, "the buffer is still returned");
    }

    #[test]
    fn resetting_clears_the_queues() {
        let mut rig = rig();
        rig.card.write(REG_STATUS, 7);
        rig.card.write(REG_STATUS, 0);
        rig.card.write(REG_QUEUE_SELECT, 0);
        assert_eq!(rig.card.read(REG_QUEUE_PFN, 4), 0);
    }

    #[test]
    fn indexes_wrap_past_the_ring_size() {
        let mut rig = rig();
        for _ in 0..300 {
            rig.send_frame(b"x");
        }
        assert_eq!(rig.sent.lock().unwrap().len(), 300);
    }
}
