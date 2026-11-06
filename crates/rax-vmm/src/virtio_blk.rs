//! A legacy virtio block device over port I/O.
//!
//! The guest is untrusted. Every address, length and descriptor index it
//! supplies is checked here or by [`GuestMemory`], and a malformed request
//! ends with an error status or a stopped queue, never a panic.

use std::io::{Read, Seek, SeekFrom, Write};

const REG_HOST_FEATURES: u16 = 0x00;
const REG_GUEST_FEATURES: u16 = 0x04;
const REG_QUEUE_PFN: u16 = 0x08;
const REG_QUEUE_SIZE: u16 = 0x0c;
const REG_QUEUE_NOTIFY: u16 = 0x10;
const REG_STATUS: u16 = 0x12;
const REG_ISR: u16 = 0x13;
const REG_CONFIG: u16 = 0x14;

const FEATURE_SEG_MAX: u32 = 1 << 2;
const FEATURE_FLUSH: u32 = 1 << 9;

const STATUS_FAILED: u8 = 0x80;
const ISR_QUEUE: u8 = 1;

const QUEUE_SIZE: u16 = 256;
const PAGE: u64 = 4096;
const DESCRIPTOR_SIZE: u64 = 16;
const DESCRIPTOR_NEXT: u16 = 1;
const DESCRIPTOR_WRITE: u16 = 2;

/// Segments per request. Two descriptors carry the header and the status.
const SEG_MAX: u32 = QUEUE_SIZE as u32 - 2;
const SECTOR: u64 = 512;
const HEADER_SIZE: usize = 16;

const REQUEST_IN: u32 = 0;
const REQUEST_OUT: u32 = 1;
const REQUEST_FLUSH: u32 = 4;

const RESULT_OK: u8 = 0;
const RESULT_IO_ERROR: u8 = 1;
const RESULT_UNSUPPORTED: u8 = 2;

/// An access outside guest RAM.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutOfRange;

/// Guest physical memory, seen while the vCPU is stopped.
pub trait GuestMemory {
    /// Fills `buffer` from `gpa`.
    fn read(&self, gpa: u64, buffer: &mut [u8]) -> Result<(), OutOfRange>;
    /// Copies `bytes` to `gpa`.
    fn write(&self, gpa: u64, bytes: &[u8]) -> Result<(), OutOfRange>;
}

/// Storage behind the device.
pub trait Backing: Read + Write + Seek + Send {
    /// Commits written data to stable storage.
    fn sync(&mut self) -> std::io::Result<()>;
}

impl Backing for std::fs::File {
    fn sync(&mut self) -> std::io::Result<()> {
        self.sync_data()
    }
}

#[derive(Debug, Clone, Copy)]
struct Descriptor {
    address: u64,
    length: u32,
    flags: u16,
    next: u16,
}

fn read_u16(memory: &dyn GuestMemory, gpa: u64) -> Result<u16, OutOfRange> {
    let mut bytes = [0; 2];
    memory.read(gpa, &mut bytes)?;
    Ok(u16::from_le_bytes(bytes))
}

/// The driver broke the ring protocol. Unrecoverable for this queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RingError;

impl From<OutOfRange> for RingError {
    fn from(_: OutOfRange) -> Self {
        Self
    }
}

/// One virtio block device.
pub struct VirtioBlk {
    backing: Box<dyn Backing>,
    sectors: u64,
    guest_features: u32,
    queue_pfn: u32,
    status: u8,
    isr: u8,
    last_available: u16,
    notified: bool,
    irq_requested: bool,
    requests: u64,
}

impl VirtioBlk {
    /// Creates a device over `backing`, which holds `size_bytes`.
    pub fn new(backing: Box<dyn Backing>, size_bytes: u64) -> Self {
        Self {
            backing,
            sectors: size_bytes / SECTOR,
            guest_features: 0,
            queue_pfn: 0,
            status: 0,
            isr: 0,
            last_available: 0,
            notified: false,
            irq_requested: false,
            requests: 0,
        }
    }

    fn reset(&mut self) {
        self.guest_features = 0;
        self.queue_pfn = 0;
        self.status = 0;
        self.isr = 0;
        self.last_available = 0;
        self.notified = false;
    }

    /// Requests handled so far.
    #[must_use]
    pub fn requests(&self) -> u64 {
        self.requests
    }

    /// Returns true once for each interrupt since the last call.
    pub fn take_irq(&mut self) -> bool {
        std::mem::take(&mut self.irq_requested)
    }

    fn config_byte(&self, offset: u16) -> u8 {
        let mut config = [0_u8; 24];
        config[0..8].copy_from_slice(&self.sectors.to_le_bytes());
        config[12..16].copy_from_slice(&SEG_MAX.to_le_bytes());
        config.get(usize::from(offset)).copied().unwrap_or(0)
    }

    /// Handles a read of the register window.
    pub fn read(&mut self, offset: u16, size: u8) -> u32 {
        match offset {
            REG_HOST_FEATURES => FEATURE_SEG_MAX | FEATURE_FLUSH,
            REG_GUEST_FEATURES => self.guest_features,
            REG_QUEUE_PFN => self.queue_pfn,
            REG_QUEUE_SIZE => u32::from(QUEUE_SIZE),
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
                self.queue_pfn = value;
                self.last_available = 0;
            }
            REG_QUEUE_NOTIFY => self.notified = true,
            REG_STATUS if value as u8 == 0 => self.reset(),
            REG_STATUS => self.status = value as u8,
            // This includes queue select: queue 0 is the only queue.
            _ => {}
        }
    }

    /// Handles requests if the guest has notified the queue.
    pub fn service(&mut self, memory: &dyn GuestMemory) {
        if !std::mem::take(&mut self.notified) || self.queue_pfn == 0 {
            return;
        }
        if self.process_queue(memory).is_err() {
            self.status |= STATUS_FAILED;
        }
    }

    fn process_queue(&mut self, memory: &dyn GuestMemory) -> Result<(), RingError> {
        let size = u64::from(QUEUE_SIZE);
        let table = u64::from(self.queue_pfn) * PAGE;
        let available = table + size * DESCRIPTOR_SIZE;
        let used = (available + 6 + 2 * size).next_multiple_of(PAGE);

        let available_index = read_u16(memory, available + 2)?;
        let mut completed = false;
        while self.last_available != available_index {
            let slot = u64::from(self.last_available % QUEUE_SIZE);
            let head = read_u16(memory, available + 4 + slot * 2)?;
            let written = self.handle_request(memory, table, head)?;

            let used_index = read_u16(memory, used + 2)?;
            let entry = used + 4 + u64::from(used_index % QUEUE_SIZE) * 8;
            memory.write(entry, &u32::from(head).to_le_bytes())?;
            memory.write(entry + 4, &written.to_le_bytes())?;
            memory.write(used + 2, &used_index.wrapping_add(1).to_le_bytes())?;

            self.last_available = self.last_available.wrapping_add(1);
            self.requests += 1;
            completed = true;
        }
        if completed {
            self.isr |= ISR_QUEUE;
            self.irq_requested = true;
        }
        Ok(())
    }

    fn descriptor(
        memory: &dyn GuestMemory,
        table: u64,
        index: u16,
    ) -> Result<Descriptor, RingError> {
        if index >= QUEUE_SIZE {
            return Err(RingError);
        }
        let mut raw = [0_u8; DESCRIPTOR_SIZE as usize];
        memory.read(table + u64::from(index) * DESCRIPTOR_SIZE, &mut raw)?;
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
        Ok(Descriptor {
            address: u64::from_le_bytes([a0, a1, a2, a3, a4, a5, a6, a7]),
            length: u32::from_le_bytes([l0, l1, l2, l3]),
            flags: u16::from_le_bytes([f0, f1]),
            next: u16::from_le_bytes([n0, n1]),
        })
    }

    /// Walks a chain. The length limit stops a descriptor loop from a
    /// malicious driver.
    fn chain(
        memory: &dyn GuestMemory,
        table: u64,
        head: u16,
    ) -> Result<Vec<Descriptor>, RingError> {
        let mut chain = Vec::new();
        let mut index = head;
        loop {
            let descriptor = Self::descriptor(memory, table, index)?;
            chain.push(descriptor);
            if descriptor.flags & DESCRIPTOR_NEXT == 0 {
                return Ok(chain);
            }
            if chain.len() >= usize::from(QUEUE_SIZE) {
                return Err(RingError);
            }
            index = descriptor.next;
        }
    }

    /// Runs one request, returning the bytes written for the guest.
    fn handle_request(
        &mut self,
        memory: &dyn GuestMemory,
        table: u64,
        head: u16,
    ) -> Result<u32, RingError> {
        let chain = Self::chain(memory, table, head)?;
        let [header, data @ .., status] = chain.as_slice() else {
            return Err(RingError);
        };
        if header.length < HEADER_SIZE as u32 || status.length < 1 {
            return Err(RingError);
        }
        if status.flags & DESCRIPTOR_WRITE == 0 {
            return Err(RingError);
        }
        let mut raw = [0_u8; HEADER_SIZE];
        memory.read(header.address, &mut raw)?;
        let [t0, t1, t2, t3, _, _, _, _, s0, s1, s2, s3, s4, s5, s6, s7] = raw;
        let kind = u32::from_le_bytes([t0, t1, t2, t3]);
        let sector = u64::from_le_bytes([s0, s1, s2, s3, s4, s5, s6, s7]);

        let (result, written) = match kind {
            REQUEST_IN => self.transfer(memory, data, sector, true),
            REQUEST_OUT => self.transfer(memory, data, sector, false),
            REQUEST_FLUSH => match self.backing.sync() {
                Ok(()) => (RESULT_OK, 0),
                Err(_) => (RESULT_IO_ERROR, 0),
            },
            _ => (RESULT_UNSUPPORTED, 0),
        };
        memory.write(status.address, &[result])?;
        Ok(written + 1)
    }

    /// Copies between the backing store and guest buffers.
    fn transfer(
        &mut self,
        memory: &dyn GuestMemory,
        data: &[Descriptor],
        sector: u64,
        to_guest: bool,
    ) -> (u8, u32) {
        let total: u64 = data.iter().map(|d| u64::from(d.length)).sum();
        let in_bounds = sector
            .checked_mul(SECTOR)
            .and_then(|start| start.checked_add(total).map(|end| (start, end)))
            .filter(|&(_, end)| end <= self.sectors * SECTOR);
        // Checking the total first bounds every allocation below by the disk
        // size, whatever lengths the guest claims.
        let Some((start, _)) = in_bounds else {
            return (RESULT_IO_ERROR, 0);
        };
        if self.backing.seek(SeekFrom::Start(start)).is_err() {
            return (RESULT_IO_ERROR, 0);
        }
        let mut written = 0_u32;
        for descriptor in data {
            let wants_write = descriptor.flags & DESCRIPTOR_WRITE != 0;
            if wants_write != to_guest {
                return (RESULT_IO_ERROR, written);
            }
            let mut buffer = vec![0_u8; descriptor.length as usize];
            let moved = if to_guest {
                self.backing.read_exact(&mut buffer).is_ok()
                    && memory.write(descriptor.address, &buffer).is_ok()
            } else {
                memory.read(descriptor.address, &mut buffer).is_ok()
                    && self.backing.write_all(&buffer).is_ok()
            };
            if !moved {
                return (RESULT_IO_ERROR, written);
            }
            if to_guest {
                written += descriptor.length;
            }
        }
        (RESULT_OK, written)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::io::Cursor;
    use std::sync::{Arc, Mutex};

    const RAM: usize = 0x20_0000;
    const PFN: u32 = 0x10;
    const TABLE: u64 = PFN as u64 * PAGE;
    const AVAILABLE: u64 = TABLE + QUEUE_SIZE as u64 * DESCRIPTOR_SIZE;
    const USED: u64 = (AVAILABLE + 6 + 2 * QUEUE_SIZE as u64).next_multiple_of(PAGE);
    const HEADER: u64 = 0x10_0000;
    const DATA: u64 = 0x11_0000;
    const STATUS: u64 = 0x12_0000;

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

    /// A disk the test can inspect after handing it to the device.
    #[derive(Clone)]
    struct SharedDisk {
        data: Arc<Mutex<Cursor<Vec<u8>>>>,
        syncs: Arc<Mutex<u32>>,
    }

    impl Read for SharedDisk {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            self.data.lock().unwrap().read(buffer)
        }
    }

    impl Write for SharedDisk {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.data.lock().unwrap().write(bytes)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Seek for SharedDisk {
        fn seek(&mut self, position: SeekFrom) -> std::io::Result<u64> {
            self.data.lock().unwrap().seek(position)
        }
    }

    impl Backing for SharedDisk {
        fn sync(&mut self) -> std::io::Result<()> {
            *self.syncs.lock().unwrap() += 1;
            Ok(())
        }
    }

    struct Rig {
        device: VirtioBlk,
        ram: Ram,
        disk: SharedDisk,
    }

    fn rig() -> Rig {
        let mut contents = vec![0_u8; 4096];
        for (index, byte) in contents.iter_mut().enumerate() {
            *byte = (index / 512) as u8 + 1;
        }
        let disk = SharedDisk {
            data: Arc::new(Mutex::new(Cursor::new(contents))),
            syncs: Arc::new(Mutex::new(0)),
        };
        let mut device = VirtioBlk::new(Box::new(disk.clone()), 4096);
        device.write(REG_QUEUE_PFN, PFN);
        Rig {
            device,
            ram: Ram(RefCell::new(vec![0; RAM])),
            disk,
        }
    }

    impl Rig {
        fn put_descriptor(&self, index: u16, address: u64, length: u32, flags: u16, next: u16) {
            let mut raw = [0_u8; 16];
            raw[0..8].copy_from_slice(&address.to_le_bytes());
            raw[8..12].copy_from_slice(&length.to_le_bytes());
            raw[12..14].copy_from_slice(&flags.to_le_bytes());
            raw[14..16].copy_from_slice(&next.to_le_bytes());
            self.ram
                .write(TABLE + u64::from(index) * DESCRIPTOR_SIZE, &raw)
                .unwrap();
        }

        fn put_header(&self, kind: u32, sector: u64) {
            let mut raw = [0_u8; HEADER_SIZE];
            raw[0..4].copy_from_slice(&kind.to_le_bytes());
            raw[8..16].copy_from_slice(&sector.to_le_bytes());
            self.ram.write(HEADER, &raw).unwrap();
        }

        /// Three-descriptor request: header, one data buffer, status.
        fn request(&mut self, kind: u32, sector: u64, length: u32, data_flags: u16) {
            self.put_header(kind, sector);
            self.put_descriptor(0, HEADER, HEADER_SIZE as u32, DESCRIPTOR_NEXT, 1);
            self.put_descriptor(1, DATA, length, DESCRIPTOR_NEXT | data_flags, 2);
            self.put_descriptor(2, STATUS, 1, DESCRIPTOR_WRITE, 0);
            self.submit(0);
        }

        fn submit(&mut self, head: u16) {
            let index = read_u16(&self.ram, AVAILABLE + 2).unwrap();
            let slot = u64::from(index % QUEUE_SIZE);
            self.ram
                .write(AVAILABLE + 4 + slot * 2, &head.to_le_bytes())
                .unwrap();
            self.ram
                .write(AVAILABLE + 2, &index.wrapping_add(1).to_le_bytes())
                .unwrap();
            self.device.write(REG_QUEUE_NOTIFY, 0);
            self.device.service(&self.ram);
        }

        fn result(&self) -> u8 {
            let mut status = [0xee];
            self.ram.read(STATUS, &mut status).unwrap();
            status[0]
        }

        fn used_index(&self) -> u16 {
            read_u16(&self.ram, USED + 2).unwrap()
        }

        fn used_length(&self, slot: u64) -> u32 {
            let mut raw = [0; 4];
            self.ram.read(USED + 8 + slot * 8, &mut raw).unwrap();
            u32::from_le_bytes(raw)
        }
    }

    #[test]
    fn config_reports_capacity_in_sectors() {
        let mut rig = rig();
        assert_eq!(rig.device.read(REG_CONFIG, 4), 8);
        assert_eq!(rig.device.read(REG_CONFIG + 4, 4), 0);
        assert_eq!(rig.device.read(REG_CONFIG + 12, 4), SEG_MAX);
    }

    #[test]
    fn reads_copy_disk_data_into_the_guest() {
        let mut rig = rig();
        rig.request(REQUEST_IN, 2, 512, DESCRIPTOR_WRITE);
        assert_eq!(rig.result(), RESULT_OK);
        let mut data = [0; 512];
        rig.ram.read(DATA, &mut data).unwrap();
        assert!(data.iter().all(|&byte| byte == 3));
        assert_eq!(rig.used_index(), 1);
        assert_eq!(rig.used_length(0), 513, "data plus the status byte");
    }

    #[test]
    fn writes_reach_the_disk() {
        let mut rig = rig();
        rig.ram.write(DATA, &[0xab; 512]).unwrap();
        rig.request(REQUEST_OUT, 1, 512, 0);
        assert_eq!(rig.result(), RESULT_OK);
        let disk = rig.disk.data.lock().unwrap();
        assert!(disk.get_ref()[512..1024].iter().all(|&b| b == 0xab));
        assert!(disk.get_ref()[..512].iter().all(|&b| b == 1));
        assert_eq!(rig.used_length(0), 1, "a write only returns the status");
    }

    #[test]
    fn completion_raises_an_interrupt_and_isr_clears_on_read() {
        let mut rig = rig();
        rig.request(REQUEST_IN, 0, 512, DESCRIPTOR_WRITE);
        assert!(rig.device.take_irq());
        assert!(!rig.device.take_irq());
        assert_eq!(rig.device.read(REG_ISR, 1), 1);
        assert_eq!(rig.device.read(REG_ISR, 1), 0);
    }

    #[test]
    fn flush_syncs_the_backing_store() {
        let mut rig = rig();
        rig.put_header(REQUEST_FLUSH, 0);
        rig.put_descriptor(0, HEADER, HEADER_SIZE as u32, DESCRIPTOR_NEXT, 1);
        rig.put_descriptor(1, STATUS, 1, DESCRIPTOR_WRITE, 0);
        rig.submit(0);
        assert_eq!(rig.result(), RESULT_OK);
        assert_eq!(*rig.disk.syncs.lock().unwrap(), 1);
    }

    #[test]
    fn scattered_buffers_are_filled_in_order() {
        let mut rig = rig();
        rig.put_header(REQUEST_IN, 0);
        rig.put_descriptor(0, HEADER, HEADER_SIZE as u32, DESCRIPTOR_NEXT, 1);
        rig.put_descriptor(1, DATA, 512, DESCRIPTOR_NEXT | DESCRIPTOR_WRITE, 2);
        rig.put_descriptor(2, DATA + 0x2000, 512, DESCRIPTOR_NEXT | DESCRIPTOR_WRITE, 3);
        rig.put_descriptor(3, STATUS, 1, DESCRIPTOR_WRITE, 0);
        rig.submit(0);
        let (mut first, mut second) = ([0], [0]);
        rig.ram.read(DATA, &mut first).unwrap();
        rig.ram.read(DATA + 0x2000, &mut second).unwrap();
        assert_eq!((first[0], second[0]), (1, 2));
    }

    #[test]
    fn reads_past_the_end_fail_cleanly() {
        let mut rig = rig();
        rig.request(REQUEST_IN, 8, 512, DESCRIPTOR_WRITE);
        assert_eq!(rig.result(), RESULT_IO_ERROR);
        assert_eq!(rig.used_index(), 1, "the request still completes");
    }

    #[test]
    fn a_huge_sector_cannot_overflow_the_bounds_check() {
        let mut rig = rig();
        rig.request(REQUEST_IN, u64::MAX, 512, DESCRIPTOR_WRITE);
        assert_eq!(rig.result(), RESULT_IO_ERROR);
    }

    #[test]
    fn a_huge_length_is_refused_before_allocating() {
        let mut rig = rig();
        rig.request(REQUEST_IN, 0, u32::MAX, DESCRIPTOR_WRITE);
        assert_eq!(rig.result(), RESULT_IO_ERROR);
    }

    #[test]
    fn buffers_outside_ram_fail_without_touching_the_disk() {
        let mut rig = rig();
        rig.put_header(REQUEST_OUT, 0);
        rig.put_descriptor(0, HEADER, HEADER_SIZE as u32, DESCRIPTOR_NEXT, 1);
        rig.put_descriptor(1, u64::MAX - 8, 512, DESCRIPTOR_NEXT, 2);
        rig.put_descriptor(2, STATUS, 1, DESCRIPTOR_WRITE, 0);
        rig.submit(0);
        assert_eq!(rig.result(), RESULT_IO_ERROR);
        assert_eq!(rig.disk.data.lock().unwrap().get_ref()[0], 1);
    }

    #[test]
    fn a_read_into_a_read_only_buffer_is_rejected() {
        let mut rig = rig();
        rig.request(REQUEST_IN, 0, 512, 0);
        assert_eq!(rig.result(), RESULT_IO_ERROR);
    }

    #[test]
    fn unknown_request_types_are_reported_as_unsupported() {
        let mut rig = rig();
        rig.request(99, 0, 512, DESCRIPTOR_WRITE);
        assert_eq!(rig.result(), RESULT_UNSUPPORTED);
    }

    #[test]
    fn a_descriptor_loop_stops_the_queue_instead_of_hanging() {
        let mut rig = rig();
        rig.put_descriptor(0, HEADER, 16, DESCRIPTOR_NEXT, 1);
        rig.put_descriptor(1, DATA, 16, DESCRIPTOR_NEXT, 0);
        rig.submit(0);
        assert_ne!(rig.device.read(REG_STATUS, 1) as u8 & STATUS_FAILED, 0);
    }

    #[test]
    fn an_out_of_range_descriptor_index_stops_the_queue() {
        let mut rig = rig();
        rig.submit(QUEUE_SIZE);
        assert_ne!(rig.device.read(REG_STATUS, 1) as u8 & STATUS_FAILED, 0);
    }

    #[test]
    fn a_chain_without_a_status_descriptor_stops_the_queue() {
        let mut rig = rig();
        rig.put_descriptor(0, HEADER, 16, 0, 0);
        rig.submit(0);
        assert_ne!(rig.device.read(REG_STATUS, 1) as u8 & STATUS_FAILED, 0);
    }

    #[test]
    fn a_queue_outside_ram_stops_the_device_without_panicking() {
        let mut rig = rig();
        rig.device.write(REG_QUEUE_PFN, u32::MAX);
        rig.device.write(REG_QUEUE_NOTIFY, 0);
        rig.device.service(&rig.ram);
        assert_ne!(rig.device.read(REG_STATUS, 1) as u8 & STATUS_FAILED, 0);
    }

    #[test]
    fn nothing_happens_without_a_notification() {
        let mut rig = rig();
        rig.device.service(&rig.ram);
        assert_eq!(rig.device.requests(), 0);
    }

    #[test]
    fn writing_zero_status_resets_the_device() {
        let mut rig = rig();
        rig.device.write(REG_STATUS, 7);
        rig.device.write(REG_STATUS, 0);
        assert_eq!(rig.device.read(REG_STATUS, 1), 0);
        assert_eq!(rig.device.read(REG_QUEUE_PFN, 4), 0);
    }

    #[test]
    fn indexes_wrap_past_the_ring_size() {
        let mut rig = rig();
        for _ in 0..300 {
            rig.request(REQUEST_IN, 0, 512, DESCRIPTOR_WRITE);
        }
        assert_eq!(rig.device.requests(), 300);
        assert_eq!(rig.used_index(), 300);
    }
}
