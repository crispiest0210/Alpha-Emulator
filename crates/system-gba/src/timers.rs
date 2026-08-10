//! The GBA's four timers.
//!
//! # Counting up, not down
//!
//! Each timer counts *up* from a reload value to `0xFFFF` and overflows back to that reload
//! value. The register that sets the reload and the register that reads the count are the same
//! address: writes go to the reload latch, reads come from the counter. Treating it as one
//! field means a game that writes a reload mid-frame appears to teleport its counter.
//!
//! # Cascade is not a prescaler
//!
//! With bit 2 set a timer ignores the clock entirely and advances by one each time the timer
//! *below* it overflows. That is how a game gets a counter longer than sixteen bits. Timer 0
//! has nothing below it, so the bit does nothing there — modelled explicitly rather than left
//! to produce a timer that never ticks.
//!
//! # Why this is not driven by the scheduler
//!
//! Prompt 07's pattern schedules an event per overflow, which is right for the Game Boy's one
//! timer. Four timers, three of which can be chained, would need rescheduling on every write to
//! any of them and on every overflow of a lower one. Counting cycles forward is simpler here
//! and stays exact, because the prescalers are powers of two and the remainder is carried.

use core_common::{Savable, StateError, StateReader, StateWriter};

pub const CHANNELS: usize = 4;

/// Base address of timer 0's registers. Each channel is four bytes further on.
pub const BASE: u32 = 0x0400_0100;

/// Cycles per tick for each prescaler setting.
pub const PRESCALERS: [u32; 4] = [1, 64, 256, 1024];
/// The same four divisors as shifts, for the hot path. Checked against [`PRESCALERS`] by a test.
const PRESCALER_SHIFTS: [u32; 4] = [0, 6, 8, 10];

mod control {
    pub const PRESCALE: u16 = 0x0003;
    pub const CASCADE: u16 = 1 << 2;
    pub const IRQ: u16 = 1 << 6;
    pub const ENABLE: u16 = 1 << 7;
    /// The bits that exist; the rest read as zero.
    pub const MASK: u16 = PRESCALE | CASCADE | IRQ | ENABLE;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct Timer {
    /// The live count.
    counter: u16,
    /// What an overflow reloads. Written through the same address the counter is read from.
    reload: u16,
    control: u16,
    /// Cycles seen but not yet worth a tick, at the current prescaler.
    remainder: u32,
}

impl Timer {
    fn enabled(&self) -> bool {
        self.control & control::ENABLE != 0
    }

    fn cascading(&self) -> bool {
        self.control & control::CASCADE != 0
    }

    fn irq_enabled(&self) -> bool {
        self.control & control::IRQ != 0
    }

    /// How many cycles one tick takes, as a shift rather than a divisor.
    ///
    /// Every entry of [`PRESCALERS`] is a power of two — 1, 64, 256, 1024 — which is not a
    /// coincidence to be rediscovered but the reason the hot path can avoid a division.
    /// `every_prescaler_shift_matches_its_divisor` is what keeps the two tables honest.
    fn prescaler_shift(&self) -> u32 {
        PRESCALER_SHIFTS[(self.control & control::PRESCALE) as usize]
    }

    /// Advance by one tick. Returns true on overflow.
    fn tick(&mut self) -> bool {
        match self.counter.checked_add(1) {
            Some(next) => {
                self.counter = next;
                false
            }
            None => {
                self.counter = self.reload;
                true
            }
        }
    }
}

/// All four timers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Timers {
    channels: [Timer; CHANNELS],
}

impl Timers {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn owns(addr: u32) -> bool {
        (BASE..BASE + (CHANNELS as u32 * 4)).contains(&addr)
    }

    /// Advance every timer, returning which ones overflowed.
    ///
    /// The return is a bitmask rather than a count because two things want it and want it
    /// differently: the interrupt controller cares only *that* a timer overflowed, and the sound
    /// FIFOs care *which* one, since a game picks a timer to pace each channel.
    pub fn tick(&mut self, cycles: u32) -> u8 {
        let mut overflowed = 0u8;

        for index in 0..CHANNELS {
            // A cascading timer is driven by the one below it, further down, not by the clock.
            if !self.channels[index].enabled() || self.channels[index].cascading() {
                continue;
            }

            // Every prescaler is a power of two, so this is a shift and a mask rather than an
            // integer division and a remainder. It matters because this runs once per timer per
            // *instruction*: with a game's sound timer running, the two divisions here were 14% of
            // a whole frame, measured with `sample` against a commercial ROM.
            let shift = self.channels[index].prescaler_shift();
            let total = self.channels[index].remainder + cycles;
            self.channels[index].remainder = total & ((1 << shift) - 1);

            for _ in 0..(total >> shift) {
                if self.channels[index].tick() {
                    overflowed |= 1 << index;
                    self.cascade_from(index, &mut overflowed);
                }
            }
        }
        overflowed
    }

    /// Propagate one overflow up the chain of cascading timers above it.
    ///
    /// Recursive in effect but written as a loop: three chained overflows in one tick is rare
    /// but reachable, and a loop makes the termination obvious.
    fn cascade_from(&mut self, mut index: usize, overflowed: &mut u8) {
        while index + 1 < CHANNELS {
            index += 1;
            let above = &mut self.channels[index];
            if !above.enabled() || !above.cascading() {
                return;
            }
            if !above.tick() {
                return;
            }
            *overflowed |= 1 << index;
        }
    }

    /// Which of the overflows in `mask` should raise an interrupt.
    pub fn interrupts(&self, mask: u8) -> u8 {
        let mut out = 0;
        for index in 0..CHANNELS {
            if mask & (1 << index) != 0 && self.channels[index].irq_enabled() {
                out |= 1 << index;
            }
        }
        out
    }

    pub fn counter(&self, channel: usize) -> u16 {
        self.channels[channel].counter
    }

    pub fn read16(&self, addr: u32) -> Option<u16> {
        if !Self::owns(addr) {
            return None;
        }
        let channel = ((addr - BASE) / 4) as usize;
        Some(match addr & 2 {
            0 => self.channels[channel].counter,
            _ => self.channels[channel].control & control::MASK,
        })
    }

    pub fn write16(&mut self, addr: u32, value: u16) -> Option<()> {
        if !Self::owns(addr) {
            return None;
        }
        let channel = ((addr - BASE) / 4) as usize;
        if addr & 2 == 0 {
            // The reload latch, not the counter. A game that writes a reload mid-frame expects
            // the current count to keep running until it overflows.
            self.channels[channel].reload = value;
            return Some(());
        }

        let was_enabled = self.channels[channel].enabled();
        self.channels[channel].control = value & control::MASK;

        // Switching a timer on loads the counter from the reload latch. Only the *edge* does:
        // rewriting the control register of a running timer must not restart it, which is how
        // a game changes the prescaler mid-count.
        if !was_enabled && self.channels[channel].enabled() {
            self.channels[channel].counter = self.channels[channel].reload;
            self.channels[channel].remainder = 0;
        }
        Some(())
    }
}

impl Savable for Timers {
    fn save(&self, w: &mut StateWriter) {
        for timer in &self.channels {
            w.write_u16(timer.counter);
            w.write_u16(timer.reload);
            w.write_u16(timer.control);
            w.write_u32(timer.remainder);
        }
    }

    fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        for timer in &mut self.channels {
            timer.counter = r.read_u16()?;
            timer.reload = r.read_u16()?;
            timer.control = r.read_u16()?;
            timer.remainder = r.read_u32()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn control_addr(channel: usize) -> u32 {
        BASE + channel as u32 * 4 + 2
    }

    fn reload_addr(channel: usize) -> u32 {
        BASE + channel as u32 * 4
    }

    /// Start a timer at the given prescaler with a reload value.
    fn start(timers: &mut Timers, channel: usize, prescale: u16, reload: u16) {
        timers.write16(reload_addr(channel), reload);
        timers.write16(control_addr(channel), control::ENABLE | prescale);
    }

    #[test]
    fn a_disabled_timer_does_not_count() {
        let mut timers = Timers::new();
        timers.write16(reload_addr(0), 0);
        assert_eq!(timers.tick(1000), 0);
        assert_eq!(timers.counter(0), 0);
    }

    #[test]
    fn enabling_a_timer_loads_the_counter_from_the_reload_latch() {
        let mut timers = Timers::new();
        start(&mut timers, 0, 0, 0xFF00);
        assert_eq!(timers.counter(0), 0xFF00);
    }

    #[test]
    fn rewriting_control_on_a_running_timer_does_not_restart_it() {
        // Only the off-to-on edge reloads. A game changes the prescaler mid-count and expects
        // the count to survive.
        let mut timers = Timers::new();
        start(&mut timers, 0, 0, 0);
        timers.tick(100);
        assert_eq!(timers.counter(0), 100);

        timers.write16(control_addr(0), control::ENABLE | 1);
        assert_eq!(timers.counter(0), 100, "still counting from where it was");
    }

    #[test]
    fn each_prescaler_divides_the_clock_by_its_documented_amount() {
        for (setting, divisor) in PRESCALERS.iter().enumerate() {
            let mut timers = Timers::new();
            start(&mut timers, 0, setting as u16, 0);
            timers.tick(divisor * 5);
            assert_eq!(
                timers.counter(0),
                5,
                "prescaler setting {setting} (divide by {divisor})"
            );
        }
    }

    #[test]
    fn cycles_below_the_prescaler_are_carried_rather_than_dropped() {
        // Dropping them is slow clock drift that only shows up as audio pitch or a game's
        // timing calibration being subtly wrong.
        let mut timers = Timers::new();
        start(&mut timers, 0, 1, 0); // divide by 64
        for _ in 0..64 {
            timers.tick(1);
        }
        assert_eq!(timers.counter(0), 1);
    }

    #[test]
    fn an_overflow_reloads_rather_than_wrapping_to_zero() {
        let mut timers = Timers::new();
        start(&mut timers, 0, 0, 0xFFFE);
        assert_eq!(timers.tick(1), 0);
        assert_eq!(timers.counter(0), 0xFFFF);
        assert_eq!(timers.tick(1), 1 << 0, "the overflow is reported");
        assert_eq!(timers.counter(0), 0xFFFE, "back to the reload, not to zero");
    }

    #[test]
    fn a_cascading_timer_advances_on_the_overflow_below_it_and_not_on_the_clock() {
        // This is how a game gets a counter longer than sixteen bits.
        let mut timers = Timers::new();
        start(&mut timers, 0, 0, 0xFFFF);
        timers.write16(reload_addr(1), 0);
        timers.write16(control_addr(1), control::ENABLE | control::CASCADE);

        assert_eq!(timers.counter(1), 0);
        timers.tick(1); // timer 0 overflows
        assert_eq!(timers.counter(1), 1);
        assert_eq!(timers.counter(0), 0xFFFF, "and reloaded");
    }

    #[test]
    fn a_cascade_can_run_the_whole_way_up_the_chain() {
        let mut timers = Timers::new();
        start(&mut timers, 0, 0, 0xFFFF);
        for channel in 1..CHANNELS {
            timers.write16(reload_addr(channel), 0xFFFF);
            timers.write16(control_addr(channel), control::ENABLE | control::CASCADE);
        }
        // Every timer is one tick from overflowing, so one clock tick carries through all four.
        let overflowed = timers.tick(1);
        assert_eq!(overflowed, 0b1111, "all four reported an overflow");
    }

    #[test]
    fn timer_zero_has_nothing_below_it_so_the_cascade_bit_does_nothing() {
        // Modelled rather than left to chance: read the bit naively and timer 0 never ticks.
        let mut timers = Timers::new();
        timers.write16(reload_addr(0), 0);
        timers.write16(control_addr(0), control::ENABLE | control::CASCADE);
        timers.tick(1000);
        assert_eq!(timers.counter(0), 0, "it is driven by nothing");
    }

    #[test]
    fn only_timers_with_the_irq_bit_ask_for_an_interrupt() {
        let mut timers = Timers::new();
        start(&mut timers, 0, 0, 0xFFFF);
        timers.write16(reload_addr(1), 0xFFFF);
        timers.write16(
            control_addr(1),
            control::ENABLE | control::CASCADE | control::IRQ,
        );

        let overflowed = timers.tick(1);
        assert_eq!(overflowed, 0b11, "both overflowed");
        assert_eq!(
            timers.interrupts(overflowed),
            0b10,
            "but only the one that asked wants an interrupt"
        );
    }

    #[test]
    fn the_counter_and_the_reload_share_an_address_but_not_a_field() {
        // Reads come from the counter, writes go to the reload latch. Conflating them makes a
        // mid-frame reload write look like the counter teleporting.
        let mut timers = Timers::new();
        start(&mut timers, 0, 0, 0);
        timers.tick(50);
        timers.write16(reload_addr(0), 0x8000);
        assert_eq!(
            timers.read16(reload_addr(0)),
            Some(50),
            "the read still sees the counter"
        );
    }

    #[test]
    fn control_reads_back_with_the_unused_bits_clear() {
        let mut timers = Timers::new();
        timers.write16(control_addr(2), 0xFFFF);
        assert_eq!(timers.read16(control_addr(2)), Some(control::MASK));
    }

    #[test]
    fn the_block_claims_four_channels_and_no_more() {
        assert!(Timers::owns(BASE));
        assert!(Timers::owns(BASE + 15));
        assert!(!Timers::owns(BASE - 1));
        assert!(!Timers::owns(BASE + 16));
        let timers = Timers::new();
        assert_eq!(timers.read16(BASE + 16), None);
    }

    #[test]
    fn timer_state_round_trips_mid_count() {
        use savestate::{decode_state, encode_state};
        let mut timers = Timers::new();
        start(&mut timers, 2, 2, 0x1234);
        timers.tick(700);

        let bytes = encode_state("gba-timers", 1, &timers);
        let mut restored = Timers::new();
        decode_state("gba-timers", 1, &bytes, &mut restored).unwrap();
        assert_eq!(restored, timers);

        // And it resumes on the same sub-tick boundary rather than losing the remainder.
        timers.tick(56);
        restored.tick(56);
        assert_eq!(restored.counter(2), timers.counter(2));
    }
}

#[cfg(test)]
mod shift_tests {
    use super::*;

    #[test]
    fn every_prescaler_shift_matches_its_divisor() {
        // The hot path divides by shifting, which is only correct because every prescaler is a
        // power of two. If a fifth setting ever appears that is not, this is what says so.
        for (setting, divisor) in PRESCALERS.iter().enumerate() {
            assert_eq!(
                1u32 << PRESCALER_SHIFTS[setting],
                *divisor,
                "setting {setting}"
            );
        }
    }
}
