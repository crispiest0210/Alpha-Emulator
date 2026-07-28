//! Cartridge real-time clocks.
//!
//! Two of them, and they are genuinely different devices:
//!
//! - [`Mbc3Rtc`], on Game Boy MBC3 cartridges, counts in **binary** — seconds 0–59, minutes
//!   0–59, hours 0–23, and a 9-bit day counter. It is often described as BCD; it is not, and
//!   implementing it that way makes every date in the game wrong past the ninth of anything.
//! - [`GbaGpioRtc`], the S-3511 on GBA cartridges reached through cartridge GPIO, counts in
//!   **BCD** and tracks a real calendar with a year, month, and weekday.
//!
//! # Driven by emulated cycles, not wall-clock time
//!
//! Both advance from the emulated cycle count rather than the host clock. That makes save
//! states replay identically, keeps the accuracy harness deterministic, and means
//! fast-forward and rewind move the cartridge clock at the same rate as the game — which is
//! what a player expects, and what reading the host clock would break.

use core_common::{Savable, StateError, StateReader, StateWriter};

/// A wall-clock reading, in whatever units the owning device counts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RtcTime {
    pub seconds: u8,
    pub minutes: u8,
    pub hours: u8,
    /// 0–511 on MBC3; day-of-month on the GBA's S-3511.
    pub days: u16,
}

impl RtcTime {
    fn save(&self, w: &mut StateWriter) {
        w.write_u8(self.seconds);
        w.write_u8(self.minutes);
        w.write_u8(self.hours);
        w.write_u16(self.days);
    }

    fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        self.seconds = r.read_u8()?;
        self.minutes = r.read_u8()?;
        self.hours = r.read_u8()?;
        self.days = r.read_u16()?;
        Ok(())
    }
}

/// The MBC3 real-time clock.
///
/// # Latching
///
/// The CPU never reads the running counter directly. Writing `0x00` then `0x01` to the latch
/// register copies the live time into a snapshot, and reads return that snapshot until the
/// next latch. Without this a multi-register read could straddle a tick and produce a time
/// that never existed — 12:59:59 read as 12:00:59 — which is exactly the bug the hardware
/// latch exists to prevent, so emulating the latch is not optional.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Mbc3Rtc {
    /// The freely running counter.
    live: RtcTime,
    /// What the CPU sees, updated only on latch.
    latched: RtcTime,
    /// Set when the day counter passes 511 and stays set until software clears it.
    day_carry: bool,
    latched_day_carry: bool,
    /// When set, the clock stops counting.
    halted: bool,
    latched_halted: bool,
    /// Tracks the `0x00` then `0x01` latch sequence.
    latch_armed: bool,
    /// Emulated cycles accumulated toward the next second.
    cycle_accumulator: u64,
}

/// The registers an MBC3 maps into the cartridge RAM window when an RTC bank is selected.
pub mod mbc3_register {
    pub const SECONDS: u8 = 0x08;
    pub const MINUTES: u8 = 0x09;
    pub const HOURS: u8 = 0x0A;
    /// Low eight bits of the day counter.
    pub const DAY_LOW: u8 = 0x0B;
    /// Bit 0 is the ninth day bit, bit 6 halts the clock, bit 7 is the overflow carry.
    pub const DAY_HIGH: u8 = 0x0C;

    pub const RANGE: std::ops::RangeInclusive<u8> = SECONDS..=DAY_HIGH;
}

impl Mbc3Rtc {
    pub fn new() -> Self {
        Self::default()
    }

    /// Advance the clock by `cycles` at the given clock rate.
    pub fn tick(&mut self, cycles: u64, cycles_per_second: u64) {
        if self.halted || cycles_per_second == 0 {
            return;
        }
        self.cycle_accumulator += cycles;
        while self.cycle_accumulator >= cycles_per_second {
            self.cycle_accumulator -= cycles_per_second;
            self.advance_one_second();
        }
    }

    fn advance_one_second(&mut self) {
        self.live.seconds += 1;
        if self.live.seconds < 60 {
            return;
        }
        self.live.seconds = 0;

        self.live.minutes += 1;
        if self.live.minutes < 60 {
            return;
        }
        self.live.minutes = 0;

        self.live.hours += 1;
        if self.live.hours < 24 {
            return;
        }
        self.live.hours = 0;

        self.live.days += 1;
        if self.live.days > 0x1FF {
            // The counter is nine bits; overflow sets a sticky carry the game can notice.
            self.live.days = 0;
            self.day_carry = true;
        }
    }

    /// Handle a write to the latch register at `0x6000`–`0x7FFF`.
    pub fn write_latch(&mut self, value: u8) {
        if value == 0x00 {
            self.latch_armed = true;
        } else if value == 0x01 && self.latch_armed {
            self.latch_armed = false;
            self.latched = self.live;
            self.latched_day_carry = self.day_carry;
            self.latched_halted = self.halted;
        } else {
            self.latch_armed = false;
        }
    }

    /// Read one of the five RTC registers. Anything else reads as open bus.
    pub fn read_register(&self, register: u8) -> u8 {
        match register {
            mbc3_register::SECONDS => self.latched.seconds,
            mbc3_register::MINUTES => self.latched.minutes,
            mbc3_register::HOURS => self.latched.hours,
            mbc3_register::DAY_LOW => self.latched.days as u8,
            mbc3_register::DAY_HIGH => {
                ((self.latched.days >> 8) & 1) as u8
                    | ((self.latched_halted as u8) << 6)
                    | ((self.latched_day_carry as u8) << 7)
            }
            _ => 0xFF,
        }
    }

    /// Write one of the five RTC registers.
    ///
    /// Writes go to the *live* counter as well as the latch, because setting the clock has to
    /// take effect rather than being overwritten by the next latch.
    pub fn write_register(&mut self, register: u8, value: u8) {
        match register {
            mbc3_register::SECONDS => {
                // Writing seconds also resets the sub-second divider on hardware.
                self.live.seconds = value % 60;
                self.cycle_accumulator = 0;
            }
            mbc3_register::MINUTES => self.live.minutes = value % 60,
            mbc3_register::HOURS => self.live.hours = value % 24,
            mbc3_register::DAY_LOW => {
                self.live.days = (self.live.days & 0x100) | value as u16;
            }
            mbc3_register::DAY_HIGH => {
                self.live.days = (self.live.days & 0xFF) | (((value & 1) as u16) << 8);
                self.halted = value & 0x40 != 0;
                self.day_carry = value & 0x80 != 0;
            }
            _ => return,
        }
        self.latched = self.live;
        self.latched_halted = self.halted;
        self.latched_day_carry = self.day_carry;
    }

    pub fn is_halted(&self) -> bool {
        self.halted
    }

    /// The live counter, for the UI and tests.
    pub fn now(&self) -> RtcTime {
        self.live
    }
}

impl Savable for Mbc3Rtc {
    fn save(&self, w: &mut StateWriter) {
        self.live.save(w);
        self.latched.save(w);
        w.write_bool(self.day_carry);
        w.write_bool(self.latched_day_carry);
        w.write_bool(self.halted);
        w.write_bool(self.latched_halted);
        w.write_bool(self.latch_armed);
        w.write_u64(self.cycle_accumulator);
    }

    fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        self.live.load(r)?;
        self.latched.load(r)?;
        self.day_carry = r.read_bool()?;
        self.latched_day_carry = r.read_bool()?;
        self.halted = r.read_bool()?;
        self.latched_halted = r.read_bool()?;
        self.latch_armed = r.read_bool()?;
        self.cycle_accumulator = r.read_u64()?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// GBA GPIO RTC
// ---------------------------------------------------------------------------

#[inline]
fn to_bcd(value: u8) -> u8 {
    ((value / 10) << 4) | (value % 10)
}

#[inline]
fn from_bcd(value: u8) -> u8 {
    ((value >> 4) & 0xF) * 10 + (value & 0xF)
}

/// The S-3511 real-time clock on GBA cartridges, reached through cartridge GPIO.
///
/// Unlike the MBC3's, this one is a genuine calendar in BCD and is talked to over a
/// three-wire serial link rather than being memory-mapped. Only the parts games actually use
/// are implemented: reading the date and time, and the status register.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GbaGpioRtc {
    pub year: u8,
    pub month: u8,
    pub day: u8,
    pub weekday: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
    /// Bit 6 selects 24-hour mode, which every game uses.
    status: u8,
    cycle_accumulator: u64,
}

impl Default for GbaGpioRtc {
    fn default() -> Self {
        Self::new()
    }
}

impl GbaGpioRtc {
    pub fn new() -> Self {
        Self {
            // A plausible fixed date rather than the host's: deterministic, and games only
            // care about elapsed time and time-of-day, not the absolute year.
            year: 0,
            month: 1,
            day: 1,
            weekday: 0,
            hour: 0,
            minute: 0,
            second: 0,
            status: 0x40, // 24-hour mode
            cycle_accumulator: 0,
        }
    }

    pub fn tick(&mut self, cycles: u64, cycles_per_second: u64) {
        if cycles_per_second == 0 {
            return;
        }
        self.cycle_accumulator += cycles;
        while self.cycle_accumulator >= cycles_per_second {
            self.cycle_accumulator -= cycles_per_second;
            self.advance_one_second();
        }
    }

    fn advance_one_second(&mut self) {
        self.second += 1;
        if self.second < 60 {
            return;
        }
        self.second = 0;
        self.minute += 1;
        if self.minute < 60 {
            return;
        }
        self.minute = 0;
        self.hour += 1;
        if self.hour < 24 {
            return;
        }
        self.hour = 0;
        self.weekday = (self.weekday + 1) % 7;
        self.day += 1;
        if self.day <= Self::days_in_month(self.year, self.month) {
            return;
        }
        self.day = 1;
        self.month += 1;
        if self.month <= 12 {
            return;
        }
        self.month = 1;
        self.year = (self.year + 1) % 100;
    }

    fn days_in_month(year: u8, month: u8) -> u8 {
        match month {
            1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
            4 | 6 | 9 | 11 => 30,
            // The chip stores a two-digit year, so the century rule never applies: every
            // year divisible by four in range is a leap year.
            2 if year.is_multiple_of(4) => 29,
            2 => 28,
            _ => 30,
        }
    }

    /// The seven-byte date-and-time response, in BCD.
    pub fn date_time_bytes(&self) -> [u8; 7] {
        [
            to_bcd(self.year),
            to_bcd(self.month),
            to_bcd(self.day),
            self.weekday & 7,
            to_bcd(self.hour) | if self.hour >= 12 { 0x80 } else { 0 },
            to_bcd(self.minute),
            to_bcd(self.second),
        ]
    }

    /// The three-byte time-only response.
    pub fn time_bytes(&self) -> [u8; 3] {
        [
            to_bcd(self.hour) | if self.hour >= 12 { 0x80 } else { 0 },
            to_bcd(self.minute),
            to_bcd(self.second),
        ]
    }

    pub fn set_date_time_bytes(&mut self, bytes: [u8; 7]) {
        self.year = from_bcd(bytes[0]) % 100;
        self.month = from_bcd(bytes[1]).clamp(1, 12);
        self.day = from_bcd(bytes[2]).clamp(1, 31);
        self.weekday = bytes[3] & 7;
        self.hour = from_bcd(bytes[4] & 0x7F) % 24;
        self.minute = from_bcd(bytes[5]) % 60;
        self.second = from_bcd(bytes[6]) % 60;
    }

    pub fn status(&self) -> u8 {
        self.status
    }

    pub fn set_status(&mut self, value: u8) {
        self.status = value;
    }
}

impl Savable for GbaGpioRtc {
    fn save(&self, w: &mut StateWriter) {
        for v in [
            self.year,
            self.month,
            self.day,
            self.weekday,
            self.hour,
            self.minute,
            self.second,
            self.status,
        ] {
            w.write_u8(v);
        }
        w.write_u64(self.cycle_accumulator);
    }

    fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        self.year = r.read_u8()?;
        self.month = r.read_u8()?;
        self.day = r.read_u8()?;
        self.weekday = r.read_u8()?;
        self.hour = r.read_u8()?;
        self.minute = r.read_u8()?;
        self.second = r.read_u8()?;
        self.status = r.read_u8()?;
        self.cycle_accumulator = r.read_u64()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The Game Boy's clock rate, which is what an MBC3 counts against.
    const GB_HZ: u64 = 4_194_304;

    #[test]
    fn the_clock_advances_one_second_per_clock_rate_of_cycles() {
        let mut rtc = Mbc3Rtc::new();
        rtc.tick(GB_HZ, GB_HZ);
        assert_eq!(rtc.now().seconds, 1);

        // Sub-second remainders accumulate rather than being lost, so ten half-seconds are
        // five seconds and not zero.
        let mut rtc = Mbc3Rtc::new();
        for _ in 0..10 {
            rtc.tick(GB_HZ / 2, GB_HZ);
        }
        assert_eq!(rtc.now().seconds, 5);
    }

    #[test]
    fn fields_carry_in_binary_not_bcd() {
        // The classic bug: treating these as BCD makes every value past 9 wrong.
        let mut rtc = Mbc3Rtc::new();
        rtc.tick(GB_HZ * 59, GB_HZ);
        assert_eq!(rtc.now().seconds, 59, "59 is a legal binary second");

        rtc.tick(GB_HZ, GB_HZ);
        assert_eq!(rtc.now().seconds, 0);
        assert_eq!(rtc.now().minutes, 1);
    }

    #[test]
    fn every_field_rolls_over_at_the_right_boundary() {
        let mut rtc = Mbc3Rtc::new();
        rtc.tick(GB_HZ * 60 * 60 * 24, GB_HZ);
        let now = rtc.now();
        assert_eq!((now.seconds, now.minutes, now.hours), (0, 0, 0));
        assert_eq!(now.days, 1);
    }

    #[test]
    fn the_day_counter_is_nine_bits_with_a_sticky_carry() {
        let mut rtc = Mbc3Rtc::new();
        rtc.write_register(mbc3_register::DAY_LOW, 0xFF);
        rtc.write_register(mbc3_register::DAY_HIGH, 0x01); // day 511
        rtc.write_register(mbc3_register::HOURS, 23);
        rtc.write_register(mbc3_register::MINUTES, 59);
        rtc.write_register(mbc3_register::SECONDS, 59);

        rtc.tick(GB_HZ, GB_HZ);
        rtc.write_latch(0x00);
        rtc.write_latch(0x01);

        assert_eq!(rtc.read_register(mbc3_register::DAY_LOW), 0);
        let high = rtc.read_register(mbc3_register::DAY_HIGH);
        assert_eq!(high & 1, 0, "the day counter wrapped");
        assert_eq!(high & 0x80, 0x80, "and set the carry bit");
    }

    #[test]
    fn reads_return_the_latched_snapshot_not_the_live_counter() {
        // This is the whole point of the latch: a multi-register read must see one consistent
        // instant, never a time that straddles a tick.
        let mut rtc = Mbc3Rtc::new();
        rtc.write_latch(0x00);
        rtc.write_latch(0x01);
        assert_eq!(rtc.read_register(mbc3_register::SECONDS), 0);

        rtc.tick(GB_HZ * 30, GB_HZ);
        assert_eq!(
            rtc.read_register(mbc3_register::SECONDS),
            0,
            "the snapshot is stale until the next latch"
        );
        assert_eq!(rtc.now().seconds, 30, "but the counter did advance");

        rtc.write_latch(0x00);
        rtc.write_latch(0x01);
        assert_eq!(rtc.read_register(mbc3_register::SECONDS), 30);
    }

    #[test]
    fn the_latch_needs_the_full_zero_then_one_sequence() {
        let mut rtc = Mbc3Rtc::new();
        rtc.tick(GB_HZ * 5, GB_HZ);

        rtc.write_latch(0x01); // a lone 1 does nothing
        assert_eq!(rtc.read_register(mbc3_register::SECONDS), 0);

        rtc.write_latch(0x00);
        rtc.write_latch(0xFF); // anything but 1 cancels
        assert_eq!(rtc.read_register(mbc3_register::SECONDS), 0);

        rtc.write_latch(0x00);
        rtc.write_latch(0x01);
        assert_eq!(rtc.read_register(mbc3_register::SECONDS), 5);
    }

    #[test]
    fn halting_stops_the_clock() {
        let mut rtc = Mbc3Rtc::new();
        rtc.write_register(mbc3_register::DAY_HIGH, 0x40);
        assert!(rtc.is_halted());

        rtc.tick(GB_HZ * 100, GB_HZ);
        assert_eq!(rtc.now().seconds, 0, "a halted clock does not count");

        rtc.write_register(mbc3_register::DAY_HIGH, 0x00);
        rtc.tick(GB_HZ * 3, GB_HZ);
        assert_eq!(rtc.now().seconds, 3);
    }

    #[test]
    fn writing_a_register_sets_the_clock_immediately() {
        let mut rtc = Mbc3Rtc::new();
        rtc.write_register(mbc3_register::HOURS, 13);
        // The write must be visible without a latch, or setting the clock would appear to do
        // nothing until the game happened to latch again.
        assert_eq!(rtc.read_register(mbc3_register::HOURS), 13);
        assert_eq!(rtc.now().hours, 13);
    }

    #[test]
    fn the_rtc_round_trips_through_a_save_state() {
        let mut rtc = Mbc3Rtc::new();
        rtc.tick(GB_HZ * 3661 + GB_HZ / 3, GB_HZ);
        rtc.write_latch(0x00);
        rtc.write_latch(0x01);

        let mut w = StateWriter::new();
        rtc.save(&mut w);
        let blob = w.into_inner();

        let mut restored = Mbc3Rtc::new();
        restored.load(&mut StateReader::new(&blob)).unwrap();
        assert_eq!(restored, rtc);
        // The sub-second accumulator survives too, so the clock does not drift across a load.
        restored.tick(GB_HZ * 2 / 3, GB_HZ);
        rtc.tick(GB_HZ * 2 / 3, GB_HZ);
        assert_eq!(restored.now(), rtc.now());
    }

    // -- GBA -----------------------------------------------------------------

    const GBA_HZ: u64 = 16_777_216;

    #[test]
    fn the_gba_clock_reports_bcd() {
        let mut rtc = GbaGpioRtc::new();
        rtc.tick(GBA_HZ * (12 * 3600 + 34 * 60 + 56), GBA_HZ);

        let bytes = rtc.time_bytes();
        assert_eq!(bytes[0] & 0x7F, 0x12, "hour 12 in BCD");
        assert_eq!(bytes[1], 0x34);
        assert_eq!(bytes[2], 0x56);
        assert_eq!(bytes[0] & 0x80, 0x80, "the PM flag is set");
    }

    #[test]
    fn the_gba_clock_keeps_a_real_calendar() {
        let mut rtc = GbaGpioRtc::new();
        rtc.set_date_time_bytes([0x24, 0x02, 0x28, 3, 0x23, 0x59, 0x59]);
        rtc.tick(GBA_HZ, GBA_HZ);

        // Year 24 is divisible by four, so February has 29 days.
        assert_eq!((rtc.month, rtc.day), (2, 29));

        rtc.set_date_time_bytes([0x23, 0x02, 0x28, 3, 0x23, 0x59, 0x59]);
        rtc.tick(GBA_HZ, GBA_HZ);
        assert_eq!((rtc.month, rtc.day), (3, 1), "year 23 is not a leap year");
    }

    #[test]
    fn the_gba_clock_rolls_over_the_year() {
        let mut rtc = GbaGpioRtc::new();
        rtc.set_date_time_bytes([0x99, 0x12, 0x31, 0, 0x23, 0x59, 0x59]);
        rtc.tick(GBA_HZ, GBA_HZ);
        assert_eq!((rtc.year, rtc.month, rtc.day), (0, 1, 1));
    }

    #[test]
    fn the_gba_clock_round_trips_its_fields_and_state() {
        let mut rtc = GbaGpioRtc::new();
        rtc.set_date_time_bytes([0x24, 0x06, 0x15, 5, 0x10, 0x20, 0x30]);
        let bytes = rtc.date_time_bytes();
        assert_eq!(bytes, [0x24, 0x06, 0x15, 5, 0x10, 0x20, 0x30]);

        let mut w = StateWriter::new();
        rtc.save(&mut w);
        let blob = w.into_inner();
        let mut restored = GbaGpioRtc::new();
        restored.load(&mut StateReader::new(&blob)).unwrap();
        assert_eq!(restored, rtc);
    }
}
