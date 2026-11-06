//! The CMOS real-time clock on ports 0x70 and 0x71.
//!
//! Linux polls the "update in progress" bit before reading the time. Without
//! this device that bit reads as set, and boot wastes tens of thousands of
//! exits spinning on it.

const REG_SECONDS: u8 = 0x00;
const REG_MINUTES: u8 = 0x02;
const REG_HOURS: u8 = 0x04;
const REG_WEEKDAY: u8 = 0x06;
const REG_DAY: u8 = 0x07;
const REG_MONTH: u8 = 0x08;
const REG_YEAR: u8 = 0x09;
const REG_STATUS_A: u8 = 0x0a;
const REG_STATUS_B: u8 = 0x0b;
const REG_STATUS_D: u8 = 0x0d;
const REG_CENTURY: u8 = 0x32;

/// Divider running, update-in-progress clear.
const STATUS_A_IDLE: u8 = 0x26;
/// 24-hour mode with BCD values.
const STATUS_B_24_HOUR: u8 = 0x02;
/// Battery good.
const STATUS_D_VALID: u8 = 0x80;

const SECONDS_PER_DAY: u64 = 86_400;

/// A CMOS RTC whose time always comes from the caller.
#[derive(Debug, Clone, Default)]
pub struct Rtc {
    index: u8,
}

const fn bcd(value: u64) -> u8 {
    (((value / 10) << 4) | (value % 10)) as u8
}

/// Converts days since 1970-01-01 to `(year, month, day)`.
///
/// This is Howard Hinnant's `civil_from_days` for the proleptic Gregorian
/// calendar, restricted to dates after 1970.
const fn civil_from_days(days: u64) -> (u64, u64, u64) {
    let shifted = days + 719_468;
    let era = shifted / 146_097;
    let day_of_era = shifted % 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let (month, carry) = if month_index < 10 {
        (month_index + 3, 0)
    } else {
        (month_index - 9, 1)
    };
    (year_of_era + era * 400 + carry, month, day)
}

impl Rtc {
    /// Handles a write to the index port. Bit 7 is the NMI mask.
    pub fn write_index(&mut self, value: u8) {
        self.index = value & 0x7f;
    }

    /// Handles a read of the data port at `unix_seconds`.
    #[must_use]
    pub fn read_data(&self, unix_seconds: u64) -> u8 {
        let time_of_day = unix_seconds % SECONDS_PER_DAY;
        let days = unix_seconds / SECONDS_PER_DAY;
        let (year, month, day) = civil_from_days(days);
        match self.index {
            REG_SECONDS => bcd(time_of_day % 60),
            REG_MINUTES => bcd(time_of_day / 60 % 60),
            REG_HOURS => bcd(time_of_day / 3600),
            // 1970-01-01 was a Thursday. The register counts Sunday as 1.
            REG_WEEKDAY => bcd((days + 4) % 7 + 1),
            REG_DAY => bcd(day),
            REG_MONTH => bcd(month),
            REG_YEAR => bcd(year % 100),
            REG_CENTURY => bcd(year / 100),
            REG_STATUS_A => STATUS_A_IDLE,
            REG_STATUS_B => STATUS_B_24_HOUR,
            REG_STATUS_D => STATUS_D_VALID,
            _ => 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-11-06 09:29:21 UTC, a Friday.
    const SAMPLE: u64 = 1_793_957_361;

    fn register(index: u8) -> u8 {
        let mut rtc = Rtc::default();
        rtc.write_index(index);
        rtc.read_data(SAMPLE)
    }

    #[test]
    fn update_in_progress_is_never_set() {
        assert_eq!(register(REG_STATUS_A) & 0x80, 0);
    }

    #[test]
    fn time_is_bcd() {
        assert_eq!(register(REG_SECONDS), 0x21);
        assert_eq!(register(REG_MINUTES), 0x29);
        assert_eq!(register(REG_HOURS), 0x09);
    }

    #[test]
    fn date_is_bcd() {
        assert_eq!(register(REG_DAY), 0x06);
        assert_eq!(register(REG_MONTH), 0x11);
        assert_eq!(register(REG_YEAR), 0x26);
        assert_eq!(register(REG_CENTURY), 0x20);
        assert_eq!(register(REG_WEEKDAY), 0x06);
    }

    #[test]
    fn nmi_bit_is_not_part_of_the_index() {
        let mut rtc = Rtc::default();
        rtc.write_index(0x80 | REG_STATUS_D);
        assert_eq!(rtc.read_data(SAMPLE), STATUS_D_VALID);
    }

    #[test]
    fn calendar_handles_the_epoch_and_a_leap_day() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        // 2024-02-29 is 19782 days after the epoch.
        assert_eq!(civil_from_days(19_782), (2024, 2, 29));
        assert_eq!(civil_from_days(19_783), (2024, 3, 1));
    }

    #[test]
    fn unknown_registers_read_zero() {
        assert_eq!(register(0x20), 0);
    }
}
