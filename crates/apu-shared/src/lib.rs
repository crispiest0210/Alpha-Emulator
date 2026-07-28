//! Shared programmable-sound-generator primitives.
//!
//! The square, wave, and noise channels here are the Game Boy's four sound channels, reused
//! unmodified by the Game Boy Color and reused by the GBA for its backward-compatible sound.
//! The GBA's two PCM channels and the DS's larger mixer are *not* here — they are fed by DMA
//! and share nothing with these.
//!
//! # Sub-units are separate types on purpose
//!
//! [`Envelope`], [`LengthCounter`], and [`Sweep`] are their own structs rather than fields
//! smeared across each channel, because the hardware really does share them: two channels
//! have an envelope, three have a length counter, one has a sweep, and each unit has its own
//! clock rate and its own edge cases. Testing "does the envelope step at the right rate"
//! against a square wave, a noise generator, and a duty cycle all at once is how those edge
//! cases get missed.
//!
//! # Digital output, not audio
//!
//! Each channel produces a 4-bit *digital* level, 0–15. Turning that into a signal is the
//! DAC's job, and the distinction matters: a channel whose DAC is off is silent no matter
//! what its level says, and switching a DAC off is how games mute a channel without losing
//! its state. See [`dac_output`].
//!
//! # Timing
//!
//! Channels advance by t-cycles through [`tick`](SquareChannel::tick). The 512 Hz frame
//! sequencer that clocks lengths, envelopes, and sweeps is *not* here — it lives in the
//! system's scheduler wiring, because it is a timing concern and is shared with things that
//! are not audio at all. This crate exposes `clock_length`, `clock_envelope`, and
//! `clock_sweep` for that sequencer to call.

#![deny(unsafe_code)]

mod channels;

pub use channels::{NoiseChannel, SquareChannel, WaveChannel, DUTY_PATTERNS, WAVE_RAM_BYTES};

use core_common::{AudioSample, Savable, StateError, StateReader, StateWriter};

/// Convert a channel's 4-bit digital level to a signal in `-1.0..=1.0`.
///
/// A disabled DAC produces silence regardless of the level — that is the whole point of
/// having a DAC separate from the channel. Turning it off is also what *disables* a channel
/// on this hardware: writing an envelope of zero volume with a downward direction switches
/// the DAC off, and the channel goes quiet even mid-note.
#[inline]
pub fn dac_output(level: u8, dac_enabled: bool) -> f32 {
    if !dac_enabled {
        return 0.0;
    }
    // 0 maps to +1.0 and 15 to -1.0 on real hardware; the sign is inaudible on its own but
    // matters when channels are summed, so it is kept.
    1.0 - (level.min(15) as f32 / 7.5)
}

/// A volume envelope: a level that steps up or down at a fixed rate.
///
/// Clocked at 64 Hz by the frame sequencer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Envelope {
    /// Current output level, 0–15.
    pub volume: u8,
    /// Level loaded when the channel is triggered.
    pub initial_volume: u8,
    /// True counts up, false counts down.
    pub increasing: bool,
    /// Sequencer steps between changes. Zero disables the envelope entirely.
    pub period: u8,
    timer: u8,
}

impl Envelope {
    /// Load from a Game Boy `NRx2` register: volume in the top nibble, direction in bit 3,
    /// period in the low three bits.
    pub fn write_register(&mut self, value: u8) {
        self.initial_volume = value >> 4;
        self.increasing = value & 0x08 != 0;
        self.period = value & 0x07;
    }

    pub fn read_register(&self) -> u8 {
        (self.initial_volume << 4) | ((self.increasing as u8) << 3) | self.period
    }

    /// Whether this envelope setting leaves the DAC powered.
    ///
    /// Zero volume counting down cannot produce anything, and the hardware responds by
    /// switching the DAC off — which silences the channel immediately rather than at the end
    /// of its length.
    pub fn dac_enabled(&self) -> bool {
        self.initial_volume != 0 || self.increasing
    }

    pub fn trigger(&mut self) {
        self.volume = self.initial_volume;
        self.timer = self.period;
    }

    /// One 64 Hz step.
    pub fn clock(&mut self) {
        if self.period == 0 {
            return; // a zero period means the envelope never moves
        }
        if self.timer > 0 {
            self.timer -= 1;
        }
        if self.timer != 0 {
            return;
        }
        self.timer = self.period;

        // The envelope stops at either end rather than wrapping, which is what makes a
        // decay-to-silence stay silent.
        if self.increasing && self.volume < 15 {
            self.volume += 1;
        } else if !self.increasing && self.volume > 0 {
            self.volume -= 1;
        }
    }
}

impl Savable for Envelope {
    fn save(&self, w: &mut StateWriter) {
        w.write_u8(self.volume);
        w.write_u8(self.initial_volume);
        w.write_bool(self.increasing);
        w.write_u8(self.period);
        w.write_u8(self.timer);
    }
    fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        self.volume = r.read_u8()?;
        self.initial_volume = r.read_u8()?;
        self.increasing = r.read_bool()?;
        self.period = r.read_u8()?;
        self.timer = r.read_u8()?;
        Ok(())
    }
}

/// Counts down to silence, clocked at 256 Hz.
///
/// The maximum differs per channel: 64 for the square and noise channels, 256 for the wave
/// channel, which is why the maximum is a field rather than a constant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LengthCounter {
    pub counter: u16,
    pub max: u16,
    /// When false the counter runs but never silences anything.
    pub enabled: bool,
}

impl LengthCounter {
    pub const fn new(max: u16) -> Self {
        Self {
            counter: 0,
            max,
            enabled: false,
        }
    }

    /// A `NRx1` length write: the register holds the value to *count up from*, so the counter
    /// is loaded with the remaining steps.
    pub fn write_length(&mut self, value: u16) {
        self.counter = self.max - (value % self.max);
    }

    /// One 256 Hz step. Returns true when the channel should be switched off.
    pub fn clock(&mut self) -> bool {
        if !self.enabled || self.counter == 0 {
            return false;
        }
        self.counter -= 1;
        self.counter == 0
    }

    /// A trigger reloads a counter that has run out, so re-triggering a finished note plays
    /// it again rather than staying silent.
    pub fn trigger(&mut self) {
        if self.counter == 0 {
            self.counter = self.max;
        }
    }
}

impl Savable for LengthCounter {
    fn save(&self, w: &mut StateWriter) {
        w.write_u16(self.counter);
        w.write_u16(self.max);
        w.write_bool(self.enabled);
    }
    fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        self.counter = r.read_u16()?;
        self.max = r.read_u16()?;
        self.enabled = r.read_bool()?;
        Ok(())
    }
}

/// Frequency sweep, clocked at 128 Hz. Only the first square channel has one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Sweep {
    /// Sequencer steps between sweeps. Zero disables sweeping.
    pub period: u8,
    /// True sweeps down in frequency.
    pub decreasing: bool,
    /// How far to shift the frequency when computing the delta.
    pub shift: u8,
    timer: u8,
    /// The sweep's own copy of the frequency, taken at trigger.
    shadow_frequency: u16,
    active: bool,
}

impl Sweep {
    pub fn write_register(&mut self, value: u8) {
        self.period = (value >> 4) & 0x07;
        self.decreasing = value & 0x08 != 0;
        self.shift = value & 0x07;
    }

    pub fn read_register(&self) -> u8 {
        (self.period << 4) | ((self.decreasing as u8) << 3) | self.shift
    }

    /// Arm the sweep from the channel's current frequency.
    ///
    /// Returns false when the channel must be switched off immediately: an enabled shift
    /// performs an overflow check *at trigger time*, before any sweep has elapsed, and a
    /// frequency that would overflow kills the channel right then.
    pub fn trigger(&mut self, frequency: u16) -> bool {
        self.shadow_frequency = frequency;
        self.timer = if self.period == 0 { 8 } else { self.period };
        self.active = self.period != 0 || self.shift != 0;

        if self.shift != 0 {
            return self.next_frequency() <= 2047;
        }
        true
    }

    /// The frequency one sweep step away, which may exceed the 11-bit field.
    fn next_frequency(&self) -> u16 {
        let delta = self.shadow_frequency >> self.shift;
        if self.decreasing {
            self.shadow_frequency.saturating_sub(delta)
        } else {
            // Deliberately not saturating: overflow past 2047 is the signal that disables the
            // channel, so it has to be observable.
            self.shadow_frequency + delta
        }
    }

    /// One 128 Hz step.
    ///
    /// Returns the new frequency to write back, or `None` if nothing changed. The `bool` is
    /// false when the channel must be switched off.
    pub fn clock(&mut self) -> (Option<u16>, bool) {
        if !self.active {
            return (None, true);
        }
        if self.timer > 0 {
            self.timer -= 1;
        }
        if self.timer != 0 {
            return (None, true);
        }
        // A period of zero reloads as 8 but performs no sweep, which is how a sweep register
        // of zero parks the unit without disabling it.
        self.timer = if self.period == 0 { 8 } else { self.period };
        if self.period == 0 {
            return (None, true);
        }

        let new_frequency = self.next_frequency();
        if new_frequency > 2047 {
            return (None, false);
        }
        if self.shift == 0 {
            return (None, true);
        }
        self.shadow_frequency = new_frequency;

        // The documented quirk: a *second* overflow check runs on the frequency after this
        // one, and it can disable the channel even though the value just written back was
        // perfectly valid. Skipping it makes sweeping notes run away instead of cutting out.
        let overflows = self.next_frequency() > 2047;
        (Some(new_frequency), !overflows)
    }
}

impl Savable for Sweep {
    fn save(&self, w: &mut StateWriter) {
        w.write_u8(self.period);
        w.write_bool(self.decreasing);
        w.write_u8(self.shift);
        w.write_u8(self.timer);
        w.write_u16(self.shadow_frequency);
        w.write_bool(self.active);
    }
    fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        self.period = r.read_u8()?;
        self.decreasing = r.read_bool()?;
        self.shift = r.read_u8()?;
        self.timer = r.read_u8()?;
        self.shadow_frequency = r.read_u16()?;
        self.active = r.read_bool()?;
        Ok(())
    }
}

/// Per-channel stereo panning and master volume: the Game Boy's `NR50` and `NR51`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Mixer {
    /// Master volume 0–7 per side. Note that 0 is *not* silence on this hardware — it is the
    /// quietest of eight steps, and a game that expects a faint sound gets one.
    pub left_volume: u8,
    pub right_volume: u8,
    /// Bit per channel, low four bits for the right side and high four for the left.
    pub panning: u8,
}

impl Mixer {
    pub fn write_nr50(&mut self, value: u8) {
        self.right_volume = value & 0x07;
        self.left_volume = (value >> 4) & 0x07;
    }

    pub fn read_nr50(&self) -> u8 {
        (self.left_volume << 4) | self.right_volume
    }

    pub fn write_nr51(&mut self, value: u8) {
        self.panning = value;
    }

    pub fn read_nr51(&self) -> u8 {
        self.panning
    }

    /// Mix four channel signals into one stereo sample.
    ///
    /// Channels are summed and divided by four so a full mix cannot clip, then scaled by the
    /// master volume. Clipping in the core would be a decision the frontend could not undo.
    pub fn mix(&self, channels: [f32; 4]) -> AudioSample {
        let mut left = 0.0;
        let mut right = 0.0;
        for (index, signal) in channels.iter().enumerate() {
            if self.panning & (1 << (index + 4)) != 0 {
                left += signal;
            }
            if self.panning & (1 << index) != 0 {
                right += signal;
            }
        }
        // The volume field is 0-7 and represents eight steps, so it scales by (v+1)/8.
        let scale = |sum: f32, volume: u8| sum / 4.0 * ((volume as f32 + 1.0) / 8.0);
        AudioSample::stereo(
            scale(left, self.left_volume),
            scale(right, self.right_volume),
        )
    }
}

impl Savable for Mixer {
    fn save(&self, w: &mut StateWriter) {
        w.write_u8(self.left_volume);
        w.write_u8(self.right_volume);
        w.write_u8(self.panning);
    }
    fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        self.left_volume = r.read_u8()?;
        self.right_volume = r.read_u8()?;
        self.panning = r.read_u8()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_disabled_dac_is_silent_whatever_the_level() {
        assert_eq!(dac_output(15, false), 0.0);
        assert_eq!(dac_output(0, false), 0.0);
        // Level 0 through a live DAC is *not* silence: it is one end of the swing.
        assert!(dac_output(0, true) > 0.9);
        assert!(dac_output(15, true) < -0.9);
    }

    // -- Envelope ------------------------------------------------------------

    #[test]
    fn the_envelope_steps_at_its_period() {
        let mut e = Envelope::default();
        e.write_register(0xF3); // volume 15, decreasing, period 3
        e.trigger();
        assert_eq!(e.volume, 15);

        e.clock();
        e.clock();
        assert_eq!(e.volume, 15, "not yet");
        e.clock();
        assert_eq!(e.volume, 14, "three clocks make one step");
    }

    #[test]
    fn a_zero_period_freezes_the_envelope() {
        let mut e = Envelope::default();
        e.write_register(0x80); // volume 8, period 0
        e.trigger();
        for _ in 0..100 {
            e.clock();
        }
        assert_eq!(e.volume, 8);
    }

    #[test]
    fn the_envelope_stops_at_the_ends_rather_than_wrapping() {
        let mut e = Envelope::default();
        e.write_register(0x11); // volume 1, decreasing, period 1
        e.trigger();
        e.clock();
        assert_eq!(e.volume, 0);
        e.clock();
        assert_eq!(e.volume, 0, "a decayed envelope stays silent");

        e.write_register(0xE9); // volume 14, increasing, period 1
        e.trigger();
        e.clock();
        assert_eq!(e.volume, 15);
        e.clock();
        assert_eq!(e.volume, 15);
    }

    #[test]
    fn zero_volume_counting_down_switches_the_dac_off() {
        // This is how a game silences a channel immediately rather than waiting out its
        // length counter.
        let mut e = Envelope::default();
        e.write_register(0x00);
        assert!(!e.dac_enabled());

        e.write_register(0x08); // zero volume but increasing, so still powered
        assert!(e.dac_enabled());
        e.write_register(0x10);
        assert!(e.dac_enabled());
    }

    #[test]
    fn the_envelope_register_round_trips() {
        let mut e = Envelope::default();
        e.write_register(0xA5);
        assert_eq!(e.read_register(), 0xA5);
    }

    // -- Length --------------------------------------------------------------

    #[test]
    fn the_length_counter_silences_the_channel_when_it_expires() {
        let mut l = LengthCounter::new(64);
        l.write_length(61); // three steps remain
        l.enabled = true;

        assert!(!l.clock());
        assert!(!l.clock());
        assert!(l.clock(), "the third clock expires it");
        assert!(!l.clock(), "and it stays expired");
    }

    #[test]
    fn a_disabled_length_counter_never_silences_anything() {
        let mut l = LengthCounter::new(64);
        l.write_length(63);
        l.enabled = false;
        for _ in 0..200 {
            assert!(!l.clock());
        }
    }

    #[test]
    fn triggering_reloads_an_expired_counter() {
        let mut l = LengthCounter::new(64);
        l.write_length(63);
        l.enabled = true;
        assert!(l.clock());

        l.trigger();
        assert_eq!(l.counter, 64, "a re-trigger plays the note again");
    }

    #[test]
    fn the_wave_channel_has_a_longer_counter() {
        let mut l = LengthCounter::new(256);
        l.write_length(0);
        assert_eq!(l.counter, 256);
    }

    // -- Sweep ---------------------------------------------------------------

    #[test]
    fn sweeping_up_raises_the_frequency_by_a_shifted_fraction() {
        // A gentle shift, so the post-write overflow check below does not fire.
        let mut s = Sweep::default();
        s.write_register(0x14); // period 1, increasing, shift 4
        assert!(s.trigger(500));

        let (frequency, alive) = s.clock();
        assert!(alive);
        assert_eq!(frequency, Some(531), "500 + 500>>4");
    }

    #[test]
    fn the_second_overflow_check_kills_the_channel_after_a_valid_write_back() {
        // The quirk worth having: hardware runs *another* overflow calculation on the value
        // it just wrote, and disables the channel if that one overflows — even though the
        // frequency actually written was perfectly in range. Skipping this makes rising
        // notes run away instead of cutting out.
        let mut s = Sweep::default();
        s.write_register(0x11); // period 1, increasing, shift 1
        assert!(s.trigger(1000));

        let (frequency, alive) = s.clock();
        assert_eq!(frequency, Some(1500), "the write-back happened");
        assert!(!alive, "but 1500 + 750 would overflow, so the channel dies");
    }

    #[test]
    fn sweeping_down_lowers_it() {
        let mut s = Sweep::default();
        s.write_register(0x1A); // period 1, decreasing, shift 2
        assert!(s.trigger(1000));

        let (frequency, alive) = s.clock();
        assert!(alive);
        assert_eq!(frequency, Some(750), "1000 - 1000>>2");
    }

    #[test]
    fn an_overflowing_sweep_disables_the_channel_rather_than_clamping() {
        // A note sweeping upward is supposed to cut out, not sit at the top of the range.
        // This frequency survives the trigger check but overflows on the first sweep.
        let mut s = Sweep::default();
        s.write_register(0x15); // period 1, increasing, shift 5
        assert!(s.trigger(1900), "1900 + 59 stays within the field");

        // Step until the accumulating sweep pushes it past the 11-bit field.
        let mut died = false;
        for _ in 0..64 {
            let (_, alive) = s.clock();
            if !alive {
                died = true;
                break;
            }
        }
        assert!(died, "a rising sweep must eventually cut the channel off");
    }

    #[test]
    fn the_trigger_time_overflow_check_can_kill_the_channel_before_any_sweep() {
        // With a shift set, the overflow check runs at trigger, before a single sweep step.
        let mut s = Sweep::default();
        s.write_register(0x01); // period 0, increasing, shift 1
        assert!(!s.trigger(2000), "killed at trigger");

        // Without a shift there is no check, so a high frequency survives.
        let mut s = Sweep::default();
        s.write_register(0x10); // period 1, no shift
        assert!(s.trigger(2000));
    }

    #[test]
    fn a_sweep_with_no_period_never_steps() {
        let mut s = Sweep::default();
        s.write_register(0x01); // period 0, shift 1
        s.trigger(500);
        for _ in 0..20 {
            let (frequency, alive) = s.clock();
            assert_eq!(frequency, None);
            assert!(alive);
        }
    }

    #[test]
    fn the_sweep_register_round_trips() {
        let mut s = Sweep::default();
        s.write_register(0x5D);
        assert_eq!(s.read_register(), 0x5D);
    }

    // -- Mixer ---------------------------------------------------------------

    #[test]
    fn panning_routes_each_channel_to_the_sides_that_selected_it() {
        let mut m = Mixer::default();
        m.write_nr50(0x77); // full volume both sides
        m.write_nr51(0b0001_0010); // channel 0 left, channel 1 right

        let sample = m.mix([1.0, 1.0, 1.0, 1.0]);
        assert!(sample.left > 0.0);
        assert!(sample.right > 0.0);

        // Only channel 0 on the left, only channel 1 on the right, so they are equal.
        assert!((sample.left - sample.right).abs() < 1e-6);

        m.write_nr51(0b0000_0001); // channel 0 right only
        let sample = m.mix([1.0, 0.0, 0.0, 0.0]);
        assert_eq!(sample.left, 0.0);
        assert!(sample.right > 0.0);
    }

    #[test]
    fn master_volume_zero_is_quiet_but_not_silent() {
        // Volume is eight steps and zero is the quietest, not off. A game fading to volume 0
        // still expects to hear something faint.
        let mut m = Mixer::default();
        m.write_nr50(0x00);
        m.write_nr51(0xFF);
        let sample = m.mix([1.0, 1.0, 1.0, 1.0]);
        assert!(sample.left > 0.0, "not silence");
        assert!(sample.left < 0.2, "but much quieter than full");
    }

    #[test]
    fn a_full_mix_does_not_exceed_unity() {
        let mut m = Mixer::default();
        m.write_nr50(0x77);
        m.write_nr51(0xFF);
        let sample = m.mix([1.0, 1.0, 1.0, 1.0]);
        assert!(sample.left <= 1.0, "{}", sample.left);
        assert!(sample.right <= 1.0);

        let sample = m.mix([-1.0, -1.0, -1.0, -1.0]);
        assert!(sample.left >= -1.0);
    }

    #[test]
    fn the_mixer_registers_round_trip() {
        let mut m = Mixer::default();
        m.write_nr50(0x53);
        m.write_nr51(0xA9);
        assert_eq!(m.read_nr50(), 0x53);
        assert_eq!(m.read_nr51(), 0xA9);
    }

    #[test]
    fn every_sub_unit_round_trips_through_a_save_state() {
        let mut envelope = Envelope::default();
        envelope.write_register(0xC5);
        envelope.trigger();
        envelope.clock();

        let mut w = StateWriter::new();
        envelope.save(&mut w);
        let blob = w.into_inner();
        let mut restored = Envelope::default();
        restored.load(&mut StateReader::new(&blob)).unwrap();
        assert_eq!(restored, envelope);

        let mut sweep = Sweep::default();
        sweep.write_register(0x33);
        sweep.trigger(700);
        sweep.clock();
        let mut w = StateWriter::new();
        sweep.save(&mut w);
        let blob = w.into_inner();
        let mut restored = Sweep::default();
        restored.load(&mut StateReader::new(&blob)).unwrap();
        assert_eq!(restored, sweep);
    }
}
