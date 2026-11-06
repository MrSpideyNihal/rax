//! A modern virtio multi-touch screen.
//!
//! This is what lets a key press become a finger on the screen. Unlike the
//! disk and network card, virtio-input has no legacy form, so it speaks the
//! modern memory-mapped interface. Ring contents are untrusted.

use std::collections::VecDeque;

use rax_input::{Phase, Touch};

use crate::virtio_blk::{GuestMemory, OutOfRange};

/// Offset of the common configuration block in the device's memory.
pub const COMMON_OFFSET: u64 = 0x0000;
/// Offset of the notification register.
pub const NOTIFY_OFFSET: u64 = 0x1000;
/// Offset of the interrupt status register.
pub const ISR_OFFSET: u64 = 0x2000;
/// Offset of the device-specific configuration.
pub const DEVICE_OFFSET: u64 = 0x3000;
/// Size of the device's memory.
pub const BAR_SIZE: u32 = 0x4000;
/// Length of the common configuration block.
pub const COMMON_LENGTH: u32 = 0x38;
/// Length of the device-specific configuration.
pub const DEVICE_LENGTH: u32 = 0x88;

const QUEUE_SIZE: u16 = 64;
const EVENT_QUEUE: usize = 0;
const EVENT_BYTES: u32 = 8;
const DESCRIPTOR_SIZE: u64 = 16;
const DESCRIPTOR_WRITE: u16 = 2;
const STATUS_FAILED: u8 = 0x80;
const ISR_QUEUE: u8 = 1;
const VERSION_1_HIGH: u32 = 1;
/// Events kept while the guest has no buffers.
const EVENT_LIMIT: usize = 4096;

const EV_SYN: u16 = 0;
const EV_KEY: u16 = 1;
const EV_ABS: u16 = 3;
const BTN_TOUCH: u16 = 0x14a;
const ABS_X: u16 = 0x00;
const ABS_Y: u16 = 0x01;
const ABS_MT_SLOT: u16 = 0x2f;
const ABS_MT_POSITION_X: u16 = 0x35;
const ABS_MT_POSITION_Y: u16 = 0x36;
const ABS_MT_TRACKING_ID: u16 = 0x39;
const INPUT_PROP_DIRECT: u8 = 1;

/// Touch coordinates run from zero to this.
const AXIS_MAX: u32 = 32767;
const SLOTS: u32 = 10;

const CONFIG_ID_NAME: u8 = 0x01;
const CONFIG_ID_DEVIDS: u8 = 0x03;
const CONFIG_PROP_BITS: u8 = 0x10;
const CONFIG_EV_BITS: u8 = 0x11;
const CONFIG_ABS_INFO: u8 = 0x12;
const DEVICE_NAME: &[u8] = b"RAX Touchscreen";

#[derive(Debug, Clone, Copy, Default)]
struct Queue {
    size: u16,
    enabled: bool,
    descriptors: u64,
    available: u64,
    used: u64,
    last_available: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RingError;

impl From<OutOfRange> for RingError {
    fn from(_: OutOfRange) -> Self {
        Self
    }
}

fn read_u16(memory: &dyn GuestMemory, gpa: u64) -> Result<u16, RingError> {
    let mut bytes = [0; 2];
    memory.read(gpa, &mut bytes)?;
    Ok(u16::from_le_bytes(bytes))
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

/// The touchscreen.
pub struct VirtioInput {
    queues: [Queue; 2],
    selected: usize,
    feature_select: u32,
    driver_feature_select: u32,
    driver_features: [u32; 2],
    status: u8,
    isr: u8,
    config_select: u8,
    config_subselect: u8,
    /// Encoded events waiting for guest buffers.
    events: VecDeque<[u8; 8]>,
    /// Tracking number of the finger in each slot.
    fingers: [Option<i32>; SLOTS as usize],
    next_tracking_id: i32,
    irq_requested: bool,
}

impl Default for VirtioInput {
    fn default() -> Self {
        Self {
            queues: [Queue::default(); 2],
            selected: 0,
            feature_select: 0,
            driver_feature_select: 0,
            driver_features: [0; 2],
            status: 0,
            isr: 0,
            config_select: 0,
            config_subselect: 0,
            events: VecDeque::new(),
            fingers: [None; SLOTS as usize],
            next_tracking_id: 0,
            irq_requested: false,
        }
    }
}

fn scale(position: f32) -> i32 {
    (position.clamp(0.0, 1.0) * AXIS_MAX as f32) as i32
}

impl VirtioInput {
    fn push(&mut self, kind: u16, code: u16, value: i32) {
        if self.events.len() >= EVENT_LIMIT {
            return;
        }
        let mut event = [0_u8; 8];
        event[0..2].copy_from_slice(&kind.to_le_bytes());
        event[2..4].copy_from_slice(&code.to_le_bytes());
        event[4..8].copy_from_slice(&value.to_le_bytes());
        self.events.push_back(event);
    }

    /// Queues a finger change, as multi-touch protocol B.
    pub fn touch(&mut self, touch: Touch) {
        let slot = usize::from(touch.slot);
        if slot >= self.fingers.len() {
            return;
        }
        let was_active = self.fingers.iter().any(Option::is_some);
        self.push(EV_ABS, ABS_MT_SLOT, i32::from(touch.slot));
        match touch.phase {
            Phase::Down => {
                let id = self.next_tracking_id;
                self.next_tracking_id = (id + 1) & 0xffff;
                self.fingers[slot] = Some(id);
                self.push(EV_ABS, ABS_MT_TRACKING_ID, id);
                self.push_position(touch);
                if !was_active {
                    self.push(EV_KEY, BTN_TOUCH, 1);
                }
            }
            Phase::Move => self.push_position(touch),
            Phase::Up => {
                self.fingers[slot] = None;
                self.push(EV_ABS, ABS_MT_TRACKING_ID, -1);
                if !self.fingers.iter().any(Option::is_some) {
                    self.push(EV_KEY, BTN_TOUCH, 0);
                }
            }
        }
        self.push(EV_SYN, 0, 0);
    }

    fn push_position(&mut self, touch: Touch) {
        let (x, y) = (scale(touch.at.x), scale(touch.at.y));
        self.push(EV_ABS, ABS_MT_POSITION_X, x);
        self.push(EV_ABS, ABS_MT_POSITION_Y, y);
        self.push(EV_ABS, ABS_X, x);
        self.push(EV_ABS, ABS_Y, y);
    }

    /// Returns true once for each interrupt since the last call.
    pub fn take_irq(&mut self) -> bool {
        std::mem::take(&mut self.irq_requested)
    }

    fn queue(&mut self) -> Option<&mut Queue> {
        self.queues.get_mut(self.selected)
    }

    // Guest-chosen queue addresses can sit anywhere, so ring arithmetic wraps
    // and the bounds check in `GuestMemory` then refuses the access.

    fn reset(&mut self) {
        let events = std::mem::take(&mut self.events);
        *self = Self {
            next_tracking_id: self.next_tracking_id,
            events,
            ..Self::default()
        };
        self.events.clear();
    }

    fn common_read(&self, offset: u64) -> u32 {
        let selected = self.queues.get(self.selected);
        let queue = selected.copied().unwrap_or_default();
        match offset {
            0x00 => self.feature_select,
            0x04 => {
                if self.feature_select == 1 {
                    VERSION_1_HIGH
                } else {
                    0
                }
            }
            0x08 => self.driver_feature_select,
            0x0c => self.driver_features[(self.driver_feature_select & 1) as usize],
            0x10 | 0x1a => 0xffff, // No MSI-X, so interrupts use the legacy line.
            0x12 => self.queues.len() as u32,
            0x14 => u32::from(self.status),
            0x15 => 0,
            0x16 => self.selected as u32,
            0x18 => selected.map_or(0, |_| u32::from(QUEUE_SIZE)),
            0x1c => u32::from(queue.enabled),
            0x1e => 0,
            0x20 => low(queue.descriptors),
            0x24 => high(queue.descriptors),
            0x28 => low(queue.available),
            0x2c => high(queue.available),
            0x30 => low(queue.used),
            0x34 => high(queue.used),
            _ => 0,
        }
    }

    fn common_write(&mut self, offset: u64, value: u32) {
        match offset {
            0x00 => self.feature_select = value,
            0x08 => self.driver_feature_select = value,
            0x0c => self.driver_features[(self.driver_feature_select & 1) as usize] = value,
            0x14 if value == 0 => self.reset(),
            0x14 => self.status = value as u8,
            0x16 => self.selected = value as usize,
            _ => {
                if let Some(queue) = self.queue() {
                    match offset {
                        0x18 => queue.size = (value as u16).min(QUEUE_SIZE),
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
    }

    /// The bytes of the current configuration selection.
    fn config_payload(&self) -> Vec<u8> {
        let abs_max = |maximum: u32| {
            let mut info = vec![0_u8; 20];
            info[4..8].copy_from_slice(&maximum.to_le_bytes());
            info
        };
        match (self.config_select, u16::from(self.config_subselect)) {
            (CONFIG_ID_NAME, 0) => DEVICE_NAME.to_vec(),
            (CONFIG_ID_DEVIDS, 0) => {
                let ids: [u16; 4] = [0x06, 0x1af4, 0x0001, 0x0001];
                ids.iter().flat_map(|id| id.to_le_bytes()).collect()
            }
            (CONFIG_PROP_BITS, 0) => vec![1 << INPUT_PROP_DIRECT],
            (CONFIG_EV_BITS, EV_KEY) => bitmap(&[BTN_TOUCH]),
            (CONFIG_EV_BITS, EV_ABS) => bitmap(&[
                ABS_X,
                ABS_Y,
                ABS_MT_SLOT,
                ABS_MT_POSITION_X,
                ABS_MT_POSITION_Y,
                ABS_MT_TRACKING_ID,
            ]),
            (CONFIG_ABS_INFO, ABS_MT_SLOT) => abs_max(SLOTS - 1),
            (CONFIG_ABS_INFO, ABS_MT_TRACKING_ID) => abs_max(0xffff),
            (CONFIG_ABS_INFO, ABS_X | ABS_Y | ABS_MT_POSITION_X | ABS_MT_POSITION_Y) => {
                abs_max(AXIS_MAX)
            }
            _ => Vec::new(),
        }
    }

    fn config_read(&self, offset: u64, size: u8) -> u32 {
        let payload = self.config_payload();
        let byte = |at: u64| -> u8 {
            match at {
                0 => self.config_select,
                1 => self.config_subselect,
                2 => payload.len().min(128) as u8,
                8.. => payload.get((at - 8) as usize).copied().unwrap_or(0),
                _ => 0,
            }
        };
        (0..u64::from(size)).fold(0, |value, index| {
            value | u32::from(byte(offset + index)) << (index * 8)
        })
    }

    /// Handles a read of the device's memory.
    pub fn read(&mut self, offset: u64, size: u8) -> u32 {
        match offset {
            ISR_OFFSET => u32::from(std::mem::take(&mut self.isr)),
            DEVICE_OFFSET.. => self.config_read(offset - DEVICE_OFFSET, size),
            NOTIFY_OFFSET..ISR_OFFSET => 0,
            _ => {
                let aligned = offset & !3;
                let shift = (offset - aligned) * 8;
                let mask = match size {
                    1 => 0xff,
                    2 => 0xffff,
                    _ => u32::MAX,
                };
                // Byte and word registers share a dword in this layout.
                let word = match aligned {
                    0x14 => self.common_read(0x14) | u32::from(self.selected as u16) << 16,
                    0x10 => 0xffff | (self.queues.len() as u32) << 16,
                    0x18 => self.common_read(0x18) | 0xffff << 16,
                    0x1c => self.common_read(0x1c),
                    _ => self.common_read(aligned),
                };
                (word >> shift) & mask
            }
        }
    }

    /// Handles a write to the device's memory.
    pub fn write(&mut self, offset: u64, value: u32) {
        match offset {
            NOTIFY_OFFSET => {}
            DEVICE_OFFSET => self.config_select = value as u8,
            0x3001 => self.config_subselect = value as u8,
            _ if offset < NOTIFY_OFFSET => self.common_write(offset, value),
            _ => {}
        }
    }

    /// Delivers waiting events. Call while the vCPU is stopped.
    pub fn service(&mut self, memory: &dyn GuestMemory) {
        match self.deliver(memory) {
            Ok(true) => {
                self.isr |= ISR_QUEUE;
                self.irq_requested = true;
            }
            Ok(false) => {}
            Err(RingError) => self.status |= STATUS_FAILED,
        }
    }

    fn deliver(&mut self, memory: &dyn GuestMemory) -> Result<bool, RingError> {
        let mut queue = self.queues[EVENT_QUEUE];
        if !queue.enabled || queue.size == 0 {
            return Ok(false);
        }
        let mut delivered = false;
        while !self.events.is_empty() {
            let offered = read_u16(memory, queue.available.wrapping_add(2))?;
            if offered == queue.last_available {
                break;
            }
            let slot = u64::from(queue.last_available % queue.size);
            let head = read_u16(memory, queue.available.wrapping_add(4 + slot * 2))?;
            if head >= queue.size {
                return Err(RingError);
            }
            let mut raw = [0_u8; 16];
            memory.read(
                queue
                    .descriptors
                    .wrapping_add(u64::from(head) * DESCRIPTOR_SIZE),
                &mut raw,
            )?;
            let address = u64::from_le_bytes(raw[0..8].try_into().map_err(|_| RingError)?);
            let length = u32::from_le_bytes(raw[8..12].try_into().map_err(|_| RingError)?);
            let flags = u16::from_le_bytes(raw[12..14].try_into().map_err(|_| RingError)?);
            if flags & DESCRIPTOR_WRITE == 0 || length < EVENT_BYTES {
                return Err(RingError);
            }
            if let Some(event) = self.events.pop_front() {
                memory.write(address, &event)?;
            }
            let used_index = read_u16(memory, queue.used.wrapping_add(2))?;
            let entry = queue
                .used
                .wrapping_add(4 + u64::from(used_index % queue.size) * 8);
            memory.write(entry, &u32::from(head).to_le_bytes())?;
            memory.write(entry.wrapping_add(4), &EVENT_BYTES.to_le_bytes())?;
            memory.write(
                queue.used.wrapping_add(2),
                &used_index.wrapping_add(1).to_le_bytes(),
            )?;
            queue.last_available = queue.last_available.wrapping_add(1);
            delivered = true;
        }
        self.queues[EVENT_QUEUE] = queue;
        Ok(delivered)
    }
}

/// Builds a little-endian bitmap with `bits` set.
fn bitmap(bits: &[u16]) -> Vec<u8> {
    let top = bits.iter().copied().max().map_or(0, usize::from);
    let mut map = vec![0_u8; top / 8 + 1];
    for &bit in bits {
        map[usize::from(bit) / 8] |= 1 << (bit % 8);
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;
    use rax_input::Point;
    use std::cell::RefCell;

    const AT: Point = Point { x: 0.5, y: 0.25 };
    const DESCRIPTORS: u64 = 0x1_0000;
    const AVAILABLE: u64 = 0x2_0000;
    const USED: u64 = 0x3_0000;
    const BUFFERS: u64 = 0x4_0000;

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

    fn touch(slot: u8, phase: Phase) -> Touch {
        Touch {
            slot,
            phase,
            at: AT,
        }
    }

    fn decoded(device: &VirtioInput) -> Vec<(u16, u16, i32)> {
        device
            .events
            .iter()
            .map(|event| {
                (
                    u16::from_le_bytes([event[0], event[1]]),
                    u16::from_le_bytes([event[2], event[3]]),
                    i32::from_le_bytes([event[4], event[5], event[6], event[7]]),
                )
            })
            .collect()
    }

    fn ready() -> (VirtioInput, Ram) {
        let mut device = VirtioInput::default();
        device.write(0x16, 0);
        device.write(0x18, 8);
        device.write(0x20, DESCRIPTORS as u32);
        device.write(0x28, AVAILABLE as u32);
        device.write(0x30, USED as u32);
        device.write(0x1c, 1);
        (device, Ram(RefCell::new(vec![0; 0x8_0000])))
    }

    fn offer_buffers(ram: &Ram, count: u16) {
        for index in 0..count {
            let at = DESCRIPTORS + u64::from(index) * DESCRIPTOR_SIZE;
            let mut raw = [0_u8; 16];
            let address = BUFFERS + u64::from(index) * 8;
            raw[0..8].copy_from_slice(&address.to_le_bytes());
            raw[8..12].copy_from_slice(&8_u32.to_le_bytes());
            raw[12..14].copy_from_slice(&DESCRIPTOR_WRITE.to_le_bytes());
            ram.write(at, &raw).unwrap();
            ram.write(AVAILABLE + 4 + u64::from(index) * 2, &index.to_le_bytes())
                .unwrap();
        }
        ram.write(AVAILABLE + 2, &count.to_le_bytes()).unwrap();
    }

    fn config(device: &mut VirtioInput, select: u8, subselect: u8) -> (u8, Vec<u8>) {
        device.write(DEVICE_OFFSET, u32::from(select));
        device.write(DEVICE_OFFSET + 1, u32::from(subselect));
        let size = device.read(DEVICE_OFFSET + 2, 1) as u8;
        let bytes = (0..u64::from(size))
            .map(|index| device.read(DEVICE_OFFSET + 8 + index, 1) as u8)
            .collect();
        (size, bytes)
    }

    #[test]
    fn it_offers_only_version_1() {
        let mut device = VirtioInput::default();
        device.write(0x00, 1);
        assert_eq!(device.read(0x04, 4), VERSION_1_HIGH);
        device.write(0x00, 0);
        assert_eq!(device.read(0x04, 4), 0);
    }

    #[test]
    fn driver_features_read_back() {
        let mut device = VirtioInput::default();
        device.write(0x08, 1);
        device.write(0x0c, 1);
        assert_eq!(device.read(0x0c, 4), 1);
    }

    #[test]
    fn it_has_two_queues_and_no_msix() {
        let mut device = VirtioInput::default();
        assert_eq!(device.read(0x12, 2), 2);
        assert_eq!(device.read(0x10, 2), 0xffff);
        assert_eq!(device.read(0x1a, 2), 0xffff);
    }

    #[test]
    fn narrow_registers_read_correctly() {
        let mut device = VirtioInput::default();
        device.write(0x14, 7);
        device.write(0x16, 1);
        assert_eq!(device.read(0x14, 1), 7);
        assert_eq!(device.read(0x16, 2), 1);
        assert_eq!(device.read(0x18, 2), u32::from(QUEUE_SIZE));
    }

    #[test]
    fn a_missing_queue_reports_size_zero() {
        let mut device = VirtioInput::default();
        device.write(0x16, 5);
        assert_eq!(device.read(0x18, 2), 0);
    }

    #[test]
    fn queue_addresses_read_back_in_two_halves() {
        let mut device = VirtioInput::default();
        device.write(0x20, 0x1234_5678);
        device.write(0x24, 0x9);
        assert_eq!(device.read(0x20, 4), 0x1234_5678);
        assert_eq!(device.read(0x24, 4), 0x9);
    }

    #[test]
    fn zero_status_resets() {
        let (mut device, _) = ready();
        device.write(0x14, 0);
        assert_eq!(device.read(0x1c, 2), 0);
    }

    #[test]
    fn the_name_is_reported() {
        let mut device = VirtioInput::default();
        let (size, bytes) = config(&mut device, CONFIG_ID_NAME, 0);
        assert_eq!(usize::from(size), DEVICE_NAME.len());
        assert_eq!(bytes, DEVICE_NAME);
    }

    #[test]
    fn it_is_a_direct_touch_device() {
        let mut device = VirtioInput::default();
        let (_, bytes) = config(&mut device, CONFIG_PROP_BITS, 0);
        assert_eq!(bytes[0], 1 << INPUT_PROP_DIRECT);
    }

    #[test]
    fn multitouch_axes_are_advertised() {
        let mut device = VirtioInput::default();
        let (_, bytes) = config(&mut device, CONFIG_EV_BITS, EV_ABS as u8);
        let has = |bit: u16| bytes[usize::from(bit) / 8] & (1 << (bit % 8)) != 0;
        assert!(has(ABS_MT_SLOT) && has(ABS_MT_POSITION_X) && has(ABS_MT_TRACKING_ID));
        assert!(!has(0x10));
    }

    #[test]
    fn axis_limits_are_reported() {
        let mut device = VirtioInput::default();
        let (size, bytes) = config(&mut device, CONFIG_ABS_INFO, ABS_MT_SLOT as u8);
        assert_eq!(size, 20);
        assert_eq!(bytes[4], (SLOTS - 1) as u8);
    }

    #[test]
    fn unknown_selections_are_empty() {
        let mut device = VirtioInput::default();
        assert_eq!(config(&mut device, 0x7f, 0).0, 0);
    }

    #[test]
    fn a_first_finger_uses_protocol_b() {
        let mut device = VirtioInput::default();
        device.touch(touch(2, Phase::Down));
        assert_eq!(
            decoded(&device),
            [
                (EV_ABS, ABS_MT_SLOT, 2),
                (EV_ABS, ABS_MT_TRACKING_ID, 0),
                (EV_ABS, ABS_MT_POSITION_X, 16383),
                (EV_ABS, ABS_MT_POSITION_Y, 8191),
                (EV_ABS, ABS_X, 16383),
                (EV_ABS, ABS_Y, 8191),
                (EV_KEY, BTN_TOUCH, 1),
                (EV_SYN, 0, 0),
            ]
        );
    }

    #[test]
    fn btn_touch_tracks_the_first_and_last_finger_only() {
        let mut device = VirtioInput::default();
        device.touch(touch(0, Phase::Down));
        device.touch(touch(1, Phase::Down));
        device.touch(touch(0, Phase::Up));
        let presses = decoded(&device)
            .iter()
            .filter(|event| event.1 == BTN_TOUCH)
            .count();
        assert_eq!(presses, 1, "the second finger is not a new press");
        device.touch(touch(1, Phase::Up));
        assert!(decoded(&device).contains(&(EV_KEY, BTN_TOUCH, 0)));
    }

    #[test]
    fn each_press_gets_a_new_tracking_id() {
        let mut device = VirtioInput::default();
        device.touch(touch(0, Phase::Down));
        device.touch(touch(0, Phase::Up));
        device.touch(touch(0, Phase::Down));
        assert!(decoded(&device).contains(&(EV_ABS, ABS_MT_TRACKING_ID, 1)));
    }

    #[test]
    fn releasing_sends_a_tracking_id_of_minus_one() {
        let mut device = VirtioInput::default();
        device.touch(touch(3, Phase::Up));
        assert!(decoded(&device).contains(&(EV_ABS, ABS_MT_TRACKING_ID, -1)));
    }

    #[test]
    fn slots_beyond_the_device_are_ignored() {
        let mut device = VirtioInput::default();
        device.touch(touch(SLOTS as u8, Phase::Down));
        assert!(device.events.is_empty());
    }

    #[test]
    fn events_reach_guest_buffers_and_raise_an_interrupt() {
        let (mut device, ram) = ready();
        offer_buffers(&ram, 8);
        device.touch(touch(0, Phase::Up));
        device.service(&ram);
        assert!(device.events.is_empty());
        assert!(device.take_irq());
        assert_eq!(read_u16(&ram, USED + 2).unwrap(), 4);
        let mut first = [0; 8];
        ram.read(BUFFERS, &mut first).unwrap();
        assert_eq!(&first[..2], &EV_ABS.to_le_bytes());
        assert_eq!(device.read(ISR_OFFSET, 1), 1);
        assert_eq!(device.read(ISR_OFFSET, 1), 0);
    }

    #[test]
    fn events_wait_until_the_guest_has_buffers() {
        let (mut device, ram) = ready();
        device.touch(touch(0, Phase::Up));
        let waiting = device.events.len();
        device.service(&ram);
        assert_eq!(device.events.len(), waiting);
        assert!(!device.take_irq());
    }

    #[test]
    fn a_disabled_queue_does_not_read_guest_memory() {
        let mut device = VirtioInput::default();
        device.touch(touch(0, Phase::Up));
        device.service(&Ram(RefCell::new(Vec::new())));
        assert_eq!(device.read(0x14, 1) as u8 & STATUS_FAILED, 0);
    }

    #[test]
    fn read_only_buffers_stop_the_device() {
        let (mut device, ram) = ready();
        offer_buffers(&ram, 1);
        ram.write(DESCRIPTORS + 12, &[0, 0]).unwrap();
        device.touch(touch(0, Phase::Up));
        device.service(&ram);
        assert_ne!(device.read(0x14, 1) as u8 & STATUS_FAILED, 0);
    }

    #[test]
    fn a_bad_head_stops_the_device_without_panicking() {
        let (mut device, ram) = ready();
        ram.write(AVAILABLE + 2, &1_u16.to_le_bytes()).unwrap();
        ram.write(AVAILABLE + 4, &9_u16.to_le_bytes()).unwrap();
        device.touch(touch(0, Phase::Up));
        device.service(&ram);
        assert_ne!(device.read(0x14, 1) as u8 & STATUS_FAILED, 0);
    }

    #[test]
    fn a_queue_outside_ram_stops_the_device_without_panicking() {
        let (mut device, ram) = ready();
        device.write(0x28, 0xffff_0000);
        device.touch(touch(0, Phase::Up));
        device.service(&ram);
        assert_ne!(device.read(0x14, 1) as u8 & STATUS_FAILED, 0);
    }

    #[test]
    fn a_queue_at_the_top_of_memory_cannot_overflow() {
        let (mut device, ram) = ready();
        device.write(0x28, u32::MAX);
        device.write(0x2c, u32::MAX);
        device.touch(touch(0, Phase::Up));
        // Wrapping lands in the guest's own memory, which is harmless. What
        // matters is that the arithmetic does not panic.
        device.service(&ram);
    }

    #[test]
    fn the_queue_is_bounded() {
        let mut device = VirtioInput::default();
        for _ in 0..EVENT_LIMIT {
            device.touch(touch(0, Phase::Move));
        }
        assert_eq!(device.events.len(), EVENT_LIMIT);
    }

    #[test]
    fn bitmaps_set_the_right_bits() {
        assert_eq!(bitmap(&[0, 9]), [0x01, 0x02]);
    }
}
