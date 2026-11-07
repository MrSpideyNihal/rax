//! A memory balloon, which gives Android's unused RAM back to Windows.
//!
//! RAM a guest has touched stays with the host forever otherwise, however
//! little the guest is using. RAX reads how much memory Android has to spare
//! and asks it to hand over everything above a safety margin. Android
//! "inflates" by giving page numbers, and those pages are discarded on the
//! host. If Android suddenly needs more, it takes pages back by itself, which
//! `DEFLATE_ON_OOM` allows without waiting for RAX. Rings are untrusted.
//!
//! Experimental: the app leaves this off. See `docs/BENCHMARKS.md`. No block
//! size tried yet both keeps the guest responsive and returns memory.

use std::collections::HashMap;

use crate::virtio_blk::GuestMemory;
use crate::virtio_input::{DEVICE_OFFSET, ISR_OFFSET, NOTIFY_OFFSET};
use crate::virtio_modern::{Chain, Common, RingError, VERSION_1, complete, next_chain};

const FEATURE_STATS: u64 = 1 << 1;
const FEATURE_DEFLATE_ON_OOM: u64 = 1 << 2;
const QUEUE_SIZE: u16 = 128;
const INFLATE_QUEUE: usize = 0;
const DEFLATE_QUEUE: usize = 1;
const STATS_QUEUE: usize = 2;
const QUEUES: usize = 3;
const ISR_CONFIG: u8 = 2;

/// The size of a balloon page, whatever the guest's own page size.
const PAGE_BYTES: u64 = 4096;
const MIB: u64 = 1024 * 1024;
/// RAM goes back to Windows in whole blocks of this size. Android hands over
/// scattered 4 KiB pages, and releasing those one by one starved the guest:
/// measured, its 120 s sleep had not ended after 6 minutes.
const BLOCK_BYTES: u64 = 2 * MIB;
const PAGES_PER_BLOCK: u16 = (BLOCK_BYTES / PAGE_BYTES) as u16;
/// Longest buffer read from the guest. Linux sends 256 page numbers at most.
const MAX_BUFFER: usize = 64 * 1024;
const STAT_AVAILABLE: u16 = 6;
const STAT_BYTES: usize = 10;

/// Memory left for Android to use at once. Its low-memory killer starts well
/// below this, so inflating never pushes apps out by itself.
const MARGIN_BYTES: u64 = 512 * MIB;
/// Spare memory ignored, so the balloon doesn't chase small changes.
const DEAD_ZONE_BYTES: u64 = 32 * MIB;
/// The most taken from Android at each poll, so RAM is given back smoothly.
const STEP_BYTES: u64 = 64 * MIB;
/// Time before the first request. Android uses plenty of memory briefly
/// while it boots, and taking memory then only slows it down.
const SETTLE_NS: u64 = 90_000_000_000;
/// How often Android's spare memory is checked.
const POLL_NS: u64 = 3_000_000_000;

/// Decides how many pages Android should hand over.
fn next_target(current: u32, available_bytes: u64) -> u32 {
    let page = |bytes: u64| (bytes / PAGE_BYTES) as u32;
    if available_bytes < MARGIN_BYTES {
        current.saturating_sub(page(MARGIN_BYTES - available_bytes))
    } else if available_bytes > MARGIN_BYTES + DEAD_ZONE_BYTES {
        let spare = available_bytes - MARGIN_BYTES;
        current.saturating_add(page(spare.min(STEP_BYTES)))
    } else {
        current
    }
}

/// Reads little-endian page numbers.
fn page_numbers(bytes: &[u8]) -> impl Iterator<Item = u32> + '_ {
    bytes
        .as_chunks::<4>()
        .0
        .iter()
        .copied()
        .map(u32::from_le_bytes)
}

/// Turns block numbers into `(address, bytes)` runs, so neighbouring blocks
/// cost a single call to Windows.
fn block_runs(blocks: &mut [u32]) -> Vec<(u64, u64)> {
    blocks.sort_unstable();
    let mut runs: Vec<(u64, u64)> = Vec::new();
    for &block in blocks.iter() {
        let address = u64::from(block) * BLOCK_BYTES;
        match runs.last_mut() {
            Some((start, length)) if *start + *length == address => *length += BLOCK_BYTES,
            Some((start, length)) if *start + *length > address => {}
            _ => runs.push((address, BLOCK_BYTES)),
        }
    }
    runs
}

/// Reads the guest's available memory from a statistics buffer.
fn available_memory(bytes: &[u8]) -> Option<u64> {
    let (entries, _) = bytes.as_chunks::<STAT_BYTES>();
    entries.iter().find_map(|entry| {
        let tag = u16::from_le_bytes([entry[0], entry[1]]);
        let value = u64::from_le_bytes(entry[2..10].try_into().ok()?);
        (tag == STAT_AVAILABLE).then_some(value)
    })
}

fn gather(memory: &dyn GuestMemory, chain: &Chain) -> Result<Vec<u8>, RingError> {
    let mut bytes = Vec::new();
    for &(address, length) in &chain.readable {
        let length = (length as usize).min(MAX_BUFFER.saturating_sub(bytes.len()));
        let start = bytes.len();
        bytes.resize(start + length, 0);
        memory.read(address, &mut bytes[start..])?;
    }
    Ok(bytes)
}

/// The balloon.
pub struct VirtioBalloon {
    common: Common,
    /// Pages RAX wants Android to hold back.
    target: u32,
    /// Pages Android says it holds back.
    actual: u32,
    /// Pages held in each 2 MiB block. Only full blocks go to Windows.
    held: HashMap<u32, u16>,
    /// Full blocks not yet given to Windows.
    pending: Vec<u32>,
    /// The guest's statistics buffer, held until new figures are wanted.
    stats_buffer: Option<u16>,
    next_poll_ns: u64,
    irq_requested: bool,
}

impl Default for VirtioBalloon {
    fn default() -> Self {
        Self {
            common: Common::new(
                QUEUES,
                QUEUE_SIZE,
                VERSION_1 | FEATURE_STATS | FEATURE_DEFLATE_ON_OOM,
            ),
            target: 0,
            actual: 0,
            held: HashMap::new(),
            pending: Vec::new(),
            stats_buffer: None,
            next_poll_ns: SETTLE_NS,
            irq_requested: false,
        }
    }
}

impl VirtioBalloon {
    /// Returns true once for each interrupt since the last call.
    pub fn take_irq(&mut self) -> bool {
        std::mem::take(&mut self.irq_requested)
    }

    /// Pages currently given back to the host.
    #[must_use]
    pub fn pages_returned(&self) -> u32 {
        self.actual
    }

    /// Handles a read of the device's memory.
    pub fn read(&mut self, offset: u64, size: u8) -> u32 {
        match offset {
            ISR_OFFSET => u32::from(self.common.take_isr()),
            NOTIFY_OFFSET..ISR_OFFSET => 0,
            DEVICE_OFFSET.. => match offset - DEVICE_OFFSET {
                0 => self.target,
                4 => self.actual,
                _ => 0,
            },
            _ => self.common.read(offset, size),
        }
    }

    /// Handles a write to the device's memory.
    pub fn write(&mut self, offset: u64, value: u32) {
        if offset == DEVICE_OFFSET + 4 {
            self.actual = value;
        } else if offset < NOTIFY_OFFSET && self.common.write(offset, value) {
            *self = Self::default();
        }
    }

    /// Runs anything the guest has queued. Call while the vCPU is stopped.
    pub fn service(&mut self, memory: &dyn GuestMemory) {
        match self.drain(memory) {
            Ok(true) => {
                self.common.signal_queue();
                self.irq_requested = true;
            }
            Ok(false) => {}
            Err(RingError) => self.common.fail(),
        }
    }

    fn drain(&mut self, memory: &dyn GuestMemory) -> Result<bool, RingError> {
        let mut used = false;
        let mut queue = self.common.queue(INFLATE_QUEUE);
        while let Some(chain) = next_chain(memory, &mut queue)? {
            for page in page_numbers(&gather(memory, &chain)?) {
                self.hold(page);
            }
            complete(memory, &queue, chain.head, 0)?;
            self.common.set_queue(INFLATE_QUEUE, queue);
            used = true;
        }
        // Android waits for this answer before touching those pages, so
        // anything still pending must go first or it would wipe their new
        // contents. After that, the first touch brings a page back.
        let mut queue = self.common.queue(DEFLATE_QUEUE);
        while let Some(chain) = next_chain(memory, &mut queue)? {
            self.flush(memory);
            memory.finish_discards();
            for page in page_numbers(&gather(memory, &chain)?) {
                self.give_back(page);
            }
            complete(memory, &queue, chain.head, 0)?;
            self.common.set_queue(DEFLATE_QUEUE, queue);
            used = true;
        }
        let mut queue = self.common.queue(STATS_QUEUE);
        while let Some(chain) = next_chain(memory, &mut queue)? {
            self.common.set_queue(STATS_QUEUE, queue);
            if let Some(available) = available_memory(&gather(memory, &chain)?) {
                self.set_target(next_target(self.actual, available));
            }
            self.stats_buffer = Some(chain.head);
        }
        Ok(used)
    }

    fn hold(&mut self, page: u32) {
        let block = page / u32::from(PAGES_PER_BLOCK);
        let count = self.held.entry(block).or_default();
        *count = count.saturating_add(1);
        if *count == PAGES_PER_BLOCK {
            self.pending.push(block);
        }
    }

    fn give_back(&mut self, page: u32) {
        let block = page / u32::from(PAGES_PER_BLOCK);
        if let Some(count) = self.held.get_mut(&block) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                self.held.remove(&block);
            }
        }
    }

    /// Gives full blocks to Windows in as few calls as possible.
    fn flush(&mut self, memory: &dyn GuestMemory) {
        for (address, length) in block_runs(&mut self.pending) {
            memory.discard(address, length);
        }
        self.pending.clear();
    }

    fn set_target(&mut self, target: u32) {
        if target != self.target {
            self.target = target;
            self.common.signal(ISR_CONFIG);
            self.irq_requested = true;
        }
    }

    /// Gives waiting pages back, and asks for fresh figures when they are
    /// due. Android answers by putting its buffer back, which
    /// [`Self::service`] then reads. Call often, while the vCPU is stopped.
    pub fn poll(&mut self, now_ns: u64, memory: &dyn GuestMemory) {
        self.flush(memory);
        if now_ns < self.next_poll_ns {
            return;
        }
        let Some(head) = self.stats_buffer.take() else {
            return;
        };
        self.next_poll_ns = now_ns + POLL_NS;
        if complete(memory, &self.common.queue(STATS_QUEUE), head, 0).is_err() {
            self.common.fail();
            return;
        }
        self.common.signal_queue();
        self.irq_requested = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::virtio_modern::testing::{AVAILABLE, DESCRIPTORS, Ram, USED};
    use std::cell::RefCell;

    const BUFFER: u64 = 0x4_0000;

    struct Tracking {
        ram: Ram,
        discarded: RefCell<Vec<(u64, u64)>>,
        finished: RefCell<u32>,
    }

    impl GuestMemory for Tracking {
        fn read(&self, gpa: u64, buffer: &mut [u8]) -> Result<(), crate::virtio_blk::OutOfRange> {
            self.ram.read(gpa, buffer)
        }

        fn write(&self, gpa: u64, bytes: &[u8]) -> Result<(), crate::virtio_blk::OutOfRange> {
            self.ram.write(gpa, bytes)
        }

        fn discard(&self, gpa: u64, length: u64) {
            self.discarded.borrow_mut().push((gpa, length));
        }

        fn finish_discards(&self) {
            *self.finished.borrow_mut() += 1;
        }
    }

    fn memory() -> Tracking {
        Tracking {
            ram: Ram::new(0x8_0000),
            discarded: RefCell::new(Vec::new()),
            finished: RefCell::new(0),
        }
    }

    fn device(queue: u32) -> VirtioBalloon {
        let mut balloon = VirtioBalloon::default();
        balloon.write(0x16, queue);
        balloon.write(0x18, 8);
        balloon.write(0x20, DESCRIPTORS as u32);
        balloon.write(0x28, AVAILABLE as u32);
        balloon.write(0x30, USED as u32);
        balloon.write(0x1c, 1);
        balloon
    }

    fn stats(ram: &Ram, available: u64) {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&4_u16.to_le_bytes());
        bytes.extend_from_slice(&1_u64.to_le_bytes());
        bytes.extend_from_slice(&STAT_AVAILABLE.to_le_bytes());
        bytes.extend_from_slice(&available.to_le_bytes());
        ram.write(BUFFER, &bytes).unwrap();
        ram.descriptor(0, BUFFER, bytes.len() as u32, false, None);
    }

    #[test]
    fn it_offers_statistics_and_safe_deflation() {
        let mut balloon = VirtioBalloon::default();
        assert_eq!(balloon.read(0x04, 4), 0b110);
        balloon.write(0x00, 1);
        assert_eq!(balloon.read(0x04, 4), 1);
        assert_eq!(balloon.read(0x12, 2), 3);
    }

    #[test]
    fn spare_memory_grows_the_balloon_one_step_at_a_time() {
        let step = (STEP_BYTES / PAGE_BYTES) as u32;
        assert_eq!(next_target(0, 4096 * MIB), step);
        assert_eq!(next_target(step, 4096 * MIB), step * 2);
    }

    #[test]
    fn the_margin_is_never_given_away() {
        let spare = 50 * MIB;
        let target = next_target(0, MARGIN_BYTES + spare);
        assert_eq!(u64::from(target) * PAGE_BYTES, spare);
    }

    #[test]
    fn a_small_surplus_changes_nothing() {
        assert_eq!(next_target(500, MARGIN_BYTES + DEAD_ZONE_BYTES), 500);
    }

    #[test]
    fn pressure_returns_the_whole_shortfall_at_once() {
        let pages = (MIB / PAGE_BYTES) as u32;
        assert_eq!(
            next_target(pages * 300, MARGIN_BYTES - 100 * MIB),
            pages * 200
        );
    }

    #[test]
    fn deflating_stops_at_zero() {
        assert_eq!(next_target(5, 0), 0);
    }

    #[test]
    fn neighbouring_blocks_become_one_run() {
        assert_eq!(
            block_runs(&mut [3, 1, 2, 9]),
            [(2 * MIB, 6 * MIB), (18 * MIB, 2 * MIB)]
        );
    }

    #[test]
    fn a_repeated_block_is_not_counted_twice() {
        assert_eq!(block_runs(&mut [5, 5, 6]), [(10 * MIB, 4 * MIB)]);
    }

    #[test]
    fn the_top_of_a_32_bit_page_number_cannot_overflow() {
        let mut balloon = VirtioBalloon::default();
        balloon.hold(u32::MAX);
        assert_eq!(block_runs(&mut [u32::MAX]).len(), 1);
    }

    #[test]
    fn a_block_is_released_only_when_every_page_is_held() {
        let mut balloon = VirtioBalloon::default();
        for page in 0..u32::from(PAGES_PER_BLOCK) - 1 {
            balloon.hold(page);
        }
        assert!(balloon.pending.is_empty());
        balloon.hold(u32::from(PAGES_PER_BLOCK) - 1);
        assert_eq!(balloon.pending, [0]);
    }

    #[test]
    fn a_page_given_back_keeps_its_block_from_completing() {
        let mut balloon = VirtioBalloon::default();
        for page in 0..u32::from(PAGES_PER_BLOCK) - 1 {
            balloon.hold(page);
        }
        balloon.give_back(0);
        balloon.hold(u32::from(PAGES_PER_BLOCK) - 1);
        assert!(balloon.pending.is_empty());
    }

    #[test]
    fn a_page_the_device_never_held_is_ignored() {
        let mut balloon = VirtioBalloon::default();
        balloon.give_back(5);
        assert!(balloon.held.is_empty());
    }

    #[test]
    fn a_trailing_partial_number_is_ignored() {
        assert_eq!(page_numbers(&[1, 0, 0]).count(), 0);
    }

    #[test]
    fn available_memory_is_found_among_other_figures() {
        let ram = Ram::new(0x8_0000);
        stats(&ram, 77);
        let mut bytes = [0; 20];
        ram.read(BUFFER, &mut bytes).unwrap();
        assert_eq!(available_memory(&bytes), Some(77));
        assert_eq!(available_memory(&bytes[..10]), None);
    }

    fn offer_page(memory: &Tracking, page: u32) {
        memory.ram.write(BUFFER, &page.to_le_bytes()).unwrap();
        memory.ram.descriptor(0, BUFFER, 4, false, None);
        memory.ram.offer(0, 0);
    }

    #[test]
    fn full_blocks_wait_for_the_next_poll() {
        let (mut balloon, memory) = (device(0), memory());
        balloon.pending.push(3);
        offer_page(&memory, 7);
        balloon.service(&memory);
        assert_eq!(memory.ram.used_count(), 1);
        assert!(balloon.take_irq());
        assert!(memory.discarded.borrow().is_empty());
        balloon.poll(0, &memory);
        assert_eq!(*memory.discarded.borrow(), [(6 * MIB, 2 * MIB)]);
        balloon.poll(1, &memory);
        assert_eq!(memory.discarded.borrow().len(), 1, "discarded only once");
    }

    #[test]
    fn pending_blocks_go_before_any_page_comes_back() {
        // Otherwise a late discard would wipe a page Android is using again.
        let (mut balloon, memory) = (device(1), memory());
        balloon.pending.push(3);
        offer_page(&memory, 7);
        balloon.service(&memory);
        assert_eq!(*memory.discarded.borrow(), [(6 * MIB, 2 * MIB)]);
        assert_eq!(*memory.finished.borrow(), 1, "and they finish first");
        assert_eq!(memory.ram.used_count(), 1);
        assert!(balloon.pending.is_empty());
    }

    #[test]
    fn deflating_alone_discards_nothing() {
        let (mut balloon, memory) = (device(1), memory());
        offer_page(&memory, 7);
        balloon.service(&memory);
        assert!(memory.discarded.borrow().is_empty());
        assert_eq!(memory.ram.used_count(), 1);
    }

    #[test]
    fn statistics_set_a_target_and_tell_the_guest() {
        let (mut balloon, memory) = (device(2), memory());
        stats(&memory.ram, 2048 * MIB);
        memory.ram.offer(0, 0);
        balloon.service(&memory);
        assert_eq!(
            balloon.read(DEVICE_OFFSET, 4),
            (STEP_BYTES / PAGE_BYTES) as u32
        );
        assert!(balloon.take_irq());
        assert_eq!(balloon.read(ISR_OFFSET, 1), u32::from(ISR_CONFIG));
    }

    #[test]
    fn the_statistics_buffer_is_held_until_a_poll_is_due() {
        let (mut balloon, memory) = (device(2), memory());
        stats(&memory.ram, 0);
        memory.ram.offer(0, 0);
        balloon.service(&memory);
        assert_eq!(memory.ram.used_count(), 0);
        balloon.poll(SETTLE_NS, &memory);
        assert_eq!(memory.ram.used_count(), 1);
        assert!(balloon.take_irq());
    }

    #[test]
    fn nothing_is_asked_for_while_android_boots() {
        let (mut balloon, memory) = (device(2), memory());
        balloon.stats_buffer = Some(0);
        balloon.poll(SETTLE_NS - 1, &memory);
        assert_eq!(memory.ram.used_count(), 0);
    }

    #[test]
    fn polls_wait_for_the_interval() {
        let (mut balloon, memory) = (device(2), memory());
        balloon.stats_buffer = Some(0);
        balloon.poll(SETTLE_NS, &memory);
        balloon.stats_buffer = Some(0);
        balloon.poll(SETTLE_NS + POLL_NS - 1, &memory);
        assert_eq!(memory.ram.used_count(), 1);
        balloon.poll(SETTLE_NS + POLL_NS, &memory);
        assert_eq!(memory.ram.used_count(), 2);
    }

    #[test]
    fn a_poll_without_a_buffer_does_nothing() {
        let (mut balloon, memory) = (device(2), memory());
        balloon.poll(0, &memory);
        assert!(!balloon.take_irq());
    }

    #[test]
    fn the_guest_reports_what_it_holds() {
        let mut balloon = VirtioBalloon::default();
        balloon.write(DEVICE_OFFSET + 4, 42);
        assert_eq!(balloon.read(DEVICE_OFFSET + 4, 4), 42);
        assert_eq!(balloon.pages_returned(), 42);
    }

    #[test]
    fn a_reset_empties_the_balloon() {
        let mut balloon = VirtioBalloon::default();
        balloon.write(DEVICE_OFFSET + 4, 42);
        balloon.write(0x14, 0);
        assert_eq!(balloon.pages_returned(), 0);
    }

    #[test]
    fn a_huge_buffer_is_cut_short_not_read_whole() {
        let (mut balloon, memory) = (device(0), memory());
        memory.ram.descriptor(0, BUFFER, u32::MAX, false, None);
        memory.ram.offer(0, 0);
        balloon.service(&memory);
        assert_eq!(memory.ram.used_count(), 1);
    }

    #[test]
    fn a_ring_outside_ram_stops_the_device() {
        let (mut balloon, memory) = (device(0), memory());
        memory.ram.descriptor(0, 0xffff_0000, 4, false, None);
        memory.ram.offer(0, 0);
        balloon.service(&memory);
        assert_ne!(balloon.read(0x14, 1) & 0x80, 0);
    }
}
