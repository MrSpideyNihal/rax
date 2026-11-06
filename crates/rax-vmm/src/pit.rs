//! The 8254 programmable interval timer.
//!
//! Time is passed in, in nanoseconds, so the model is deterministic in tests.

const PIT_HZ: u128 = 1_193_182;
const NANOS_PER_SECOND: u128 = 1_000_000_000;

const fn ns_to_ticks(ns: u64) -> u64 {
    (ns as u128 * PIT_HZ / NANOS_PER_SECOND) as u64
}

const fn ticks_to_ns(ticks: u64) -> u64 {
    (ticks as u128 * NANOS_PER_SECOND / PIT_HZ) as u64
}

#[derive(Debug, Clone, Default)]
struct Channel {
    mode: u8,
    access: u8,
    reload: u32,
    started_ns: u64,
    pending_low: Option<u8>,
    latched: Option<u16>,
    read_high: bool,
    programmed: bool,
}

impl Channel {
    fn is_periodic(&self) -> bool {
        matches!(self.mode, 2 | 3)
    }

    fn count(&self, now_ns: u64) -> u16 {
        if self.reload == 0 {
            return 0;
        }
        let ticks = ns_to_ticks(now_ns.saturating_sub(self.started_ns));
        let reload = u64::from(self.reload);
        let remaining = if self.is_periodic() {
            reload - ticks % reload
        } else {
            reload.saturating_sub(ticks)
        };
        remaining as u16
    }

    fn output_high(&self, now_ns: u64) -> bool {
        ns_to_ticks(now_ns.saturating_sub(self.started_ns)) >= u64::from(self.reload)
    }

    fn read(&mut self, now_ns: u64) -> u8 {
        let value = self.latched.unwrap_or_else(|| self.count(now_ns));
        let [low, high] = value.to_le_bytes();
        match self.access {
            1 => {
                self.latched = None;
                low
            }
            2 => {
                self.latched = None;
                high
            }
            _ if self.read_high => {
                self.read_high = false;
                self.latched = None;
                high
            }
            _ => {
                self.read_high = true;
                self.latched = Some(value);
                low
            }
        }
    }

    /// Returns true when a full count has been written.
    fn write(&mut self, value: u8, now_ns: u64) -> bool {
        let count = match self.access {
            1 => u16::from(value),
            2 => u16::from(value) << 8,
            _ => match self.pending_low.take() {
                Some(low) => u16::from_le_bytes([low, value]),
                None => {
                    self.pending_low = Some(value);
                    return false;
                }
            },
        };
        self.reload = if count == 0 {
            0x1_0000
        } else {
            u32::from(count)
        };
        self.started_ns = now_ns;
        self.programmed = true;
        true
    }
}

/// Channels 0 and 2 of an 8254, plus the gate on port 0x61.
#[derive(Debug, Clone, Default)]
pub struct Pit {
    channels: [Channel; 3],
    next_irq_ns: Option<u64>,
    speaker_control: u8,
}

impl Pit {
    /// Handles a write to the mode port.
    pub fn write_control(&mut self, value: u8, now_ns: u64) {
        let select = usize::from(value >> 6);
        if select == 3 {
            return; // Read-back is not used by Linux.
        }
        let channel = &mut self.channels[select];
        let access = (value >> 4) & 0x03;
        if access == 0 {
            channel.latched = Some(channel.count(now_ns));
            channel.read_high = false;
            return;
        }
        let mode = (value >> 1) & 0x07;
        channel.access = access;
        channel.mode = if mode > 5 { mode & 0x03 } else { mode };
        channel.pending_low = None;
        channel.read_high = false;
        channel.programmed = false;
        if select == 0 {
            self.next_irq_ns = None;
        }
    }

    /// Handles a write to a channel's data port.
    pub fn write_channel(&mut self, index: usize, value: u8, now_ns: u64) {
        let channel = &mut self.channels[index];
        if channel.write(value, now_ns) && index == 0 {
            self.next_irq_ns = Some(now_ns + ticks_to_ns(u64::from(channel.reload)));
        }
    }

    /// Handles a read of a channel's data port.
    pub fn read_channel(&mut self, index: usize, now_ns: u64) -> u8 {
        self.channels[index].read(now_ns)
    }

    /// Handles a write to port 0x61. Raising the gate restarts channel 2.
    pub fn write_speaker_control(&mut self, value: u8, now_ns: u64) {
        if value & 1 != 0 && self.speaker_control & 1 == 0 {
            self.channels[2].started_ns = now_ns;
        }
        self.speaker_control = value & 0x03;
    }

    /// Handles a read of port 0x61. Bit 5 is channel 2's output.
    #[must_use]
    pub fn read_speaker_control(&self, now_ns: u64) -> u8 {
        let output = u8::from(self.channels[2].output_high(now_ns)) << 5;
        self.speaker_control | output
    }

    /// When channel 0 next fires.
    #[must_use]
    pub fn next_irq_ns(&self) -> Option<u64> {
        self.next_irq_ns
    }

    /// Returns true if channel 0 fired by `now_ns`, and schedules the next.
    pub fn poll(&mut self, now_ns: u64) -> bool {
        let Some(due) = self.next_irq_ns else {
            return false;
        };
        if now_ns < due {
            return false;
        }
        let channel = &self.channels[0];
        self.next_irq_ns = channel.is_periodic().then(|| {
            let period = ticks_to_ns(u64::from(channel.reload)).max(1);
            // Ticks missed while the host was busy are dropped, not replayed.
            due + period * ((now_ns - due) / period + 1)
        });
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MODE_2_LOHI_CH0: u8 = 0b0011_0100;
    const MODE_0_LOHI_CH0: u8 = 0b0011_0000;
    const MODE_0_LOHI_CH2: u8 = 0b1011_0000;
    const MS: u64 = 1_000_000;

    fn program(pit: &mut Pit, control: u8, index: usize, count: u16, now_ns: u64) {
        let [low, high] = count.to_le_bytes();
        pit.write_control(control, now_ns);
        pit.write_channel(index, low, now_ns);
        pit.write_channel(index, high, now_ns);
    }

    #[test]
    fn nothing_fires_until_programmed() {
        let mut pit = Pit::default();
        assert!(!pit.poll(u64::MAX));
    }

    #[test]
    fn periodic_mode_keeps_firing() {
        let mut pit = Pit::default();
        program(&mut pit, MODE_2_LOHI_CH0, 0, 11932, 0); // About 10 ms.
        assert!(!pit.poll(9 * MS));
        assert!(pit.poll(11 * MS));
        assert!(!pit.poll(11 * MS));
        assert!(pit.poll(21 * MS));
    }

    #[test]
    fn missed_ticks_are_not_replayed() {
        let mut pit = Pit::default();
        program(&mut pit, MODE_2_LOHI_CH0, 0, 11932, 0);
        assert!(pit.poll(1000 * MS));
        assert!(!pit.poll(1000 * MS));
        assert!(pit.next_irq_ns().unwrap() > 1000 * MS);
    }

    #[test]
    fn one_shot_mode_fires_once() {
        let mut pit = Pit::default();
        program(&mut pit, MODE_0_LOHI_CH0, 0, 1193, 0); // About 1 ms.
        assert!(pit.poll(2 * MS));
        assert!(!pit.poll(100 * MS));
    }

    #[test]
    fn a_zero_count_means_65536() {
        let mut pit = Pit::default();
        program(&mut pit, MODE_2_LOHI_CH0, 0, 0, 0);
        assert_eq!(pit.next_irq_ns(), Some(ticks_to_ns(0x1_0000)));
    }

    #[test]
    fn reprogramming_control_stops_the_pending_tick() {
        let mut pit = Pit::default();
        program(&mut pit, MODE_2_LOHI_CH0, 0, 1000, 0);
        pit.write_control(MODE_0_LOHI_CH0, 0);
        assert_eq!(pit.next_irq_ns(), None);
    }

    #[test]
    fn latched_count_survives_time_passing() {
        let mut pit = Pit::default();
        program(&mut pit, MODE_2_LOHI_CH0, 0, 10000, 0);
        pit.write_control(0b0000_0000, MS); // Latch channel 0.
        let low = pit.read_channel(0, 5 * MS);
        let high = pit.read_channel(0, 5 * MS);
        let count = u16::from_le_bytes([low, high]);
        assert_eq!(count, 10000 - 1193);
    }

    #[test]
    fn channel_two_output_rises_after_its_count() {
        let mut pit = Pit::default();
        pit.write_speaker_control(0, 0);
        program(&mut pit, MODE_0_LOHI_CH2, 2, 1193, 0);
        pit.write_speaker_control(1, 10 * MS);
        assert_eq!(pit.read_speaker_control(10 * MS) & 0x20, 0);
        assert_eq!(pit.read_speaker_control(12 * MS) & 0x20, 0x20);
    }

    #[test]
    fn speaker_control_reads_back_its_low_bits() {
        let mut pit = Pit::default();
        pit.write_speaker_control(0b0000_0011, 0);
        assert_eq!(pit.read_speaker_control(0) & 0x03, 0x03);
    }
}
