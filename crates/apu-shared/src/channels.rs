//! The three channel types.

use crate::{dac_output, Envelope, LengthCounter, Sweep};
use core_common::{Savable, StateError, StateReader, StateWriter};

/// The four selectable duty cycles, as eight-step waveforms.
///
/// Note the 75% pattern is the 25% pattern inverted, so those two sound identical apart from
/// phase — which is why hardware documentation sometimes lists only three distinct timbres.
pub const DUTY_PATTERNS: [[u8; 8]; 4] = [
    [0, 0, 0, 0, 0, 0, 0, 1], // 12.5%
    [1, 0, 0, 0, 0, 0, 0, 1], // 25%
    [1, 0, 0, 0, 0, 1, 1, 1], // 50%
    [0, 1, 1, 1, 1, 1, 1, 0], // 75%
];

/// The highest frequency value; the field is 11 bits.
const MAX_FREQUENCY: u16 = 2047;

/// A square-wave channel, optionally with a frequency sweep.
///
/// Two of the Game Boy's four channels are these; only the first has a sweep.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SquareChannel {
    pub enabled: bool,
    pub dac_enabled: bool,
    /// Index into [`DUTY_PATTERNS`].
    pub duty: u8,
    duty_step: u8,
    /// The 11-bit period field. Higher values mean *higher* pitch, because the period counts
    /// down from `2048 - frequency`.
    pub frequency: u16,
    timer: i32,
    pub length: LengthCounter,
    pub envelope: Envelope,
    pub sweep: Option<Sweep>,
}

impl SquareChannel {
    /// A channel with no sweep unit.
    pub fn new() -> Self {
        Self {
            enabled: false,
            dac_enabled: false,
            duty: 2,
            duty_step: 0,
            frequency: 0,
            timer: 1,
            length: LengthCounter::new(64),
            envelope: Envelope::default(),
            sweep: None,
        }
    }

    /// A channel with a sweep unit, for the first square channel.
    pub fn with_sweep() -> Self {
        Self {
            sweep: Some(Sweep::default()),
            ..Self::new()
        }
    }

    /// T-cycles between duty steps. A frequency of 2047 gives the shortest period.
    #[inline]
    fn period(&self) -> i32 {
        ((MAX_FREQUENCY as i32 + 1) - self.frequency as i32) * 4
    }

    pub fn tick(&mut self, cycles: u32) {
        self.timer -= cycles as i32;
        while self.timer <= 0 {
            self.timer += self.period();
            self.duty_step = (self.duty_step + 1) % 8;
        }
    }

    /// Start the note: reload the timer, envelope, length, and sweep.
    pub fn trigger(&mut self) {
        self.enabled = self.dac_enabled;
        self.timer = self.period();
        self.envelope.trigger();
        self.length.trigger();

        if let Some(sweep) = &mut self.sweep {
            if !sweep.trigger(self.frequency) {
                // The trigger-time overflow check failed, so the note never sounds.
                self.enabled = false;
            }
        }
    }

    pub fn clock_length(&mut self) {
        if self.length.clock() {
            self.enabled = false;
        }
    }

    pub fn clock_envelope(&mut self) {
        self.envelope.clock();
    }

    pub fn clock_sweep(&mut self) {
        let Some(sweep) = &mut self.sweep else {
            return;
        };
        let (new_frequency, alive) = sweep.clock();
        if let Some(frequency) = new_frequency {
            self.frequency = frequency;
        }
        if !alive {
            self.enabled = false;
        }
    }

    /// The 4-bit digital level.
    #[inline]
    pub fn output(&self) -> u8 {
        if !self.enabled || !self.dac_enabled {
            return 0;
        }
        DUTY_PATTERNS[(self.duty & 3) as usize][self.duty_step as usize] * self.envelope.volume
    }

    #[inline]
    pub fn signal(&self) -> f32 {
        dac_output(self.output(), self.enabled && self.dac_enabled)
    }

    /// Apply an `NRx2` envelope write, which can switch the DAC off.
    pub fn write_envelope(&mut self, value: u8) {
        self.envelope.write_register(value);
        self.dac_enabled = self.envelope.dac_enabled();
        if !self.dac_enabled {
            self.enabled = false;
        }
    }
}

impl Default for SquareChannel {
    fn default() -> Self {
        Self::new()
    }
}

impl Savable for SquareChannel {
    fn save(&self, w: &mut StateWriter) {
        w.write_bool(self.enabled);
        w.write_bool(self.dac_enabled);
        w.write_u8(self.duty);
        w.write_u8(self.duty_step);
        w.write_u16(self.frequency);
        w.write_i32(self.timer);
        self.length.save(w);
        self.envelope.save(w);
        w.write_bool(self.sweep.is_some());
        if let Some(sweep) = &self.sweep {
            sweep.save(w);
        }
    }

    fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        self.enabled = r.read_bool()?;
        self.dac_enabled = r.read_bool()?;
        self.duty = r.read_u8()?;
        self.duty_step = r.read_u8()?;
        self.frequency = r.read_u16()?;
        self.timer = r.read_i32()?;
        self.length.load(r)?;
        self.envelope.load(r)?;
        if r.read_bool()? {
            self.sweep.get_or_insert_with(Sweep::default).load(r)?;
        } else {
            self.sweep = None;
        }
        Ok(())
    }
}

/// Bytes of wave RAM: 32 four-bit samples packed two per byte.
pub const WAVE_RAM_BYTES: usize = 16;

/// The custom-waveform channel.
///
/// Unlike the others it has no envelope: its level comes from wave RAM, scaled by a two-bit
/// shift. That makes it the only channel a game can give an arbitrary timbre, and the only
/// one where changing the *data* mid-note is a normal thing to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WaveChannel {
    pub enabled: bool,
    pub dac_enabled: bool,
    pub frequency: u16,
    /// 0 mutes, 1 is full, 2 is half, 3 is a quarter.
    pub volume_shift: u8,
    pub wave_ram: [u8; WAVE_RAM_BYTES],
    position: u8,
    timer: i32,
    /// T-cycles since the channel last advanced onto a new sample.
    ///
    /// Only interesting because of who else wants that memory: see
    /// [`WaveChannel::wave_ram_access`].
    since_fetch: u32,
    pub length: LengthCounter,

    // -- GBA-only fields, additive with Game Boy defaults --------------------------------------
    //
    // A GBA channel 3 is genuinely different hardware: two 16-byte banks rather than one, a
    // 64-sample mode that plays both back to back, and a fourth volume step the Game Boy lacks.
    // Rather than a second, parallel implementation that would drift from this one, these are
    // added as fields that default to exactly what a Game Boy always has — one bank, 32 samples,
    // no 75% step — so a caller that never touches them, `system-gb::apu` included, cannot
    // observe any difference. `wave_channel_defaults_reproduce_game_boy_hardware_exactly` in this
    // module's tests is the check that keeps that true.
    /// The second 16-byte bank. Unused, and always zero, on a Game Boy — which has one.
    pub wave_ram_bank1: [u8; WAVE_RAM_BYTES],
    /// Samples in one waveform cycle: 32 on a Game Boy, or a GBA channel playing one bank; 64 on
    /// a GBA channel playing both banks back to back. `position` walks `0..sample_count` instead
    /// of the fixed `0..32` a Game Boy always has.
    pub sample_count: u8,
    /// GBA only: which bank plays when `sample_count` is 32. Ignored once `sample_count` is 64,
    /// where playback always walks bank 0 then bank 1 in that order. Always `false` (bank 0) on a
    /// Game Boy, which has no second bank to select.
    pub active_bank: bool,
    /// GBA only: force 75% output regardless of `volume_shift` — the fourth volume step the Game
    /// Boy's two-bit field cannot reach. Always `false` on a Game Boy, which has no such bit.
    pub force_75_percent: bool,
}

impl Default for WaveChannel {
    fn default() -> Self {
        Self::new()
    }
}

impl WaveChannel {
    pub fn new() -> Self {
        Self {
            enabled: false,
            dac_enabled: false,
            frequency: 0,
            volume_shift: 0,
            wave_ram: [0; WAVE_RAM_BYTES],
            position: 0,
            timer: 1,
            since_fetch: 0,
            length: LengthCounter::new(256),
            wave_ram_bank1: [0; WAVE_RAM_BYTES],
            sample_count: 32,
            active_bank: false,
            force_75_percent: false,
        }
    }

    /// This channel steps twice as fast as a square channel at the same frequency, because it
    /// has 32 samples per cycle rather than 8.
    #[inline]
    fn period(&self) -> i32 {
        ((MAX_FREQUENCY as i32 + 1) - self.frequency as i32) * 2
    }

    pub fn tick(&mut self, cycles: u32) {
        self.timer -= cycles as i32;
        self.since_fetch = self.since_fetch.saturating_add(cycles);
        while self.timer <= 0 {
            self.timer += self.period();
            // `sample_count` rather than a fixed 32: a Game Boy never changes it from that
            // default, so this is exactly the old behaviour there, and a GBA channel in
            // 64-sample mode walks twice as far before wrapping.
            self.position = (self.position + 1) % self.sample_count;
            self.since_fetch = 0;
        }
    }

    /// Which wave-RAM byte the CPU may touch right now, given the one it asked for.
    ///
    /// A stopped channel leaves the memory to the CPU. A playing one does not share it: the
    /// CPU only gets in during the couple of cycles around the channel's own fetch, and then
    /// it sees whichever byte the *channel* just read rather than the one it addressed. Every
    /// other moment reads as 0xFF and swallows writes on a DMG, which is why games load a
    /// waveform with the channel switched off.
    ///
    /// `always_open` selects the CGB's rule, where the access always succeeds.
    pub fn wave_ram_access(&self, requested: usize, always_open: bool) -> Option<usize> {
        if !self.enabled {
            return Some(requested);
        }
        // A CGB never locks the CPU out — but it does not honour the address either. Both
        // machines redirect to the byte the channel just fetched; they differ only in what
        // happens outside that window, where a DMG refuses and a CGB serves the same byte
        // anyway. Free access to the *requested* byte is neither machine's behaviour.
        if always_open {
            return Some((self.position >> 1) as usize);
        }
        (self.since_fetch < 4).then_some((self.position >> 1) as usize)
    }

    pub fn trigger(&mut self) {
        self.enabled = self.dac_enabled;
        self.timer = self.period();
        self.position = 0;
        self.since_fetch = 0;
        self.length.trigger();
    }

    pub fn clock_length(&mut self) {
        if self.length.clock() {
            self.enabled = false;
        }
    }

    #[inline]
    pub fn output(&self) -> u8 {
        if !self.enabled || !self.dac_enabled {
            return 0;
        }
        // In 64-sample mode the first half of the cycle is bank 0 and the second is bank 1,
        // regardless of `active_bank`; in 32-sample mode `active_bank` alone picks which bank
        // plays. A Game Boy always takes the `else` branch of both, with `sample_count` fixed
        // at 32 and `active_bank` fixed at `false` — exactly the single-bank lookup this had
        // before either field existed.
        let bank = if self.sample_count > 32 {
            if self.position < 32 {
                &self.wave_ram
            } else {
                &self.wave_ram_bank1
            }
        } else if self.active_bank {
            &self.wave_ram_bank1
        } else {
            &self.wave_ram
        };
        let index_in_bank = self.position % 32;
        let byte = bank[(index_in_bank / 2) as usize];
        // The high nibble is the *earlier* sample of the pair.
        let sample = if index_in_bank.is_multiple_of(2) {
            byte >> 4
        } else {
            byte & 0x0F
        };
        if self.force_75_percent {
            // GBA only: overrides `volume_shift` entirely rather than becoming a fifth match arm
            // on it, because that is what the hardware bit does — a fourth volume step the Game
            // Boy's two-bit field has no code point for at all.
            (sample * 3) / 4
        } else {
            match self.volume_shift {
                0 => 0,
                1 => sample,
                2 => sample >> 1,
                _ => sample >> 2,
            }
        }
    }

    #[inline]
    pub fn signal(&self) -> f32 {
        dac_output(self.output(), self.enabled && self.dac_enabled)
    }
}

impl Savable for WaveChannel {
    fn save(&self, w: &mut StateWriter) {
        w.write_bool(self.enabled);
        w.write_bool(self.dac_enabled);
        w.write_u16(self.frequency);
        w.write_u8(self.volume_shift);
        w.write_bytes(&self.wave_ram);
        w.write_u8(self.position);
        w.write_i32(self.timer);
        w.write_u32(self.since_fetch);
        self.length.save(w);
        // Added for the GBA's channel 3. Written even for a Game Boy, at their fixed defaults,
        // rather than skipped: a caller-conditional field layout is exactly the kind of thing
        // that silently drifts between a struct's shape and what it actually serializes. Every
        // caller of this type must bump its own top-level state version alongside this change,
        // since the byte layout changed for all of them — `system-gb::system::STATE_VERSION`'s
        // history is the precedent to follow.
        w.write_bytes(&self.wave_ram_bank1);
        w.write_u8(self.sample_count);
        w.write_bool(self.active_bank);
        w.write_bool(self.force_75_percent);
    }
    fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        self.enabled = r.read_bool()?;
        self.dac_enabled = r.read_bool()?;
        self.frequency = r.read_u16()?;
        self.volume_shift = r.read_u8()?;
        r.read_bytes(&mut self.wave_ram)?;
        self.position = r.read_u8()?;
        self.timer = r.read_i32()?;
        self.since_fetch = r.read_u32()?;
        self.length.load(r)?;
        r.read_bytes(&mut self.wave_ram_bank1)?;
        self.sample_count = r.read_u8()?;
        self.active_bank = r.read_bool()?;
        self.force_75_percent = r.read_bool()?;
        Ok(())
    }
}

/// Base divisors for the noise channel's clock, indexed by the low three bits of `NR43`.
///
/// The first entry is 8 rather than 0 because a divisor of zero would be a division by zero;
/// the hardware substitutes half of the next step.
const NOISE_DIVISORS: [u32; 8] = [8, 16, 32, 48, 64, 80, 96, 112];

/// A pseudo-random noise channel, driven by a linear-feedback shift register.
///
/// The register can be run in 15-bit or 7-bit mode. The short mode repeats after 127 steps,
/// which is short enough to be heard as a buzzy pitch rather than as noise — games use it for
/// metallic percussion deliberately.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoiseChannel {
    pub enabled: bool,
    pub dac_enabled: bool,
    /// The shift register. Reset to all ones on trigger.
    lfsr: u16,
    /// True selects the 7-bit sequence.
    pub short_mode: bool,
    pub clock_shift: u8,
    pub divisor_code: u8,
    timer: i32,
    pub length: LengthCounter,
    pub envelope: Envelope,
}

impl Default for NoiseChannel {
    fn default() -> Self {
        Self::new()
    }
}

impl NoiseChannel {
    pub fn new() -> Self {
        Self {
            enabled: false,
            dac_enabled: false,
            lfsr: 0x7FFF,
            short_mode: false,
            clock_shift: 0,
            divisor_code: 0,
            timer: 1,
            length: LengthCounter::new(64),
            envelope: Envelope::default(),
        }
    }

    #[inline]
    fn period(&self) -> i32 {
        (NOISE_DIVISORS[(self.divisor_code & 7) as usize] << self.clock_shift.min(15)) as i32
    }

    pub fn tick(&mut self, cycles: u32) {
        self.timer -= cycles as i32;
        while self.timer <= 0 {
            self.timer += self.period().max(1);
            self.step_lfsr();
        }
    }

    /// Advance the shift register one step.
    ///
    /// The new bit is the XOR of the low two bits, fed back into bit 14 — and, in short mode,
    /// into bit 6 as well, which is what collapses the sequence from 32767 steps to 127.
    fn step_lfsr(&mut self) {
        let feedback = (self.lfsr & 1) ^ ((self.lfsr >> 1) & 1);
        self.lfsr >>= 1;
        self.lfsr |= feedback << 14;
        if self.short_mode {
            self.lfsr = (self.lfsr & !(1 << 6)) | (feedback << 6);
        }
    }

    pub fn trigger(&mut self) {
        self.enabled = self.dac_enabled;
        self.timer = self.period().max(1);
        // All ones, so the first output is silence rather than an arbitrary click.
        self.lfsr = 0x7FFF;
        self.envelope.trigger();
        self.length.trigger();
    }

    pub fn clock_length(&mut self) {
        if self.length.clock() {
            self.enabled = false;
        }
    }

    pub fn clock_envelope(&mut self) {
        self.envelope.clock();
    }

    /// The output is the *inverse* of the register's low bit.
    #[inline]
    pub fn output(&self) -> u8 {
        if !self.enabled || !self.dac_enabled {
            return 0;
        }
        if self.lfsr & 1 == 0 {
            self.envelope.volume
        } else {
            0
        }
    }

    #[inline]
    pub fn signal(&self) -> f32 {
        dac_output(self.output(), self.enabled && self.dac_enabled)
    }

    pub fn write_envelope(&mut self, value: u8) {
        self.envelope.write_register(value);
        self.dac_enabled = self.envelope.dac_enabled();
        if !self.dac_enabled {
            self.enabled = false;
        }
    }

    /// Exposed for tests and the debugger.
    pub fn lfsr(&self) -> u16 {
        self.lfsr
    }
}

impl Savable for NoiseChannel {
    fn save(&self, w: &mut StateWriter) {
        w.write_bool(self.enabled);
        w.write_bool(self.dac_enabled);
        w.write_u16(self.lfsr);
        w.write_bool(self.short_mode);
        w.write_u8(self.clock_shift);
        w.write_u8(self.divisor_code);
        w.write_i32(self.timer);
        self.length.save(w);
        self.envelope.save(w);
    }
    fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        self.enabled = r.read_bool()?;
        self.dac_enabled = r.read_bool()?;
        self.lfsr = r.read_u16()?;
        self.short_mode = r.read_bool()?;
        self.clock_shift = r.read_u8()?;
        self.divisor_code = r.read_u8()?;
        self.timer = r.read_i32()?;
        self.length.load(r)?;
        self.envelope.load(r)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn armed_square() -> SquareChannel {
        let mut ch = SquareChannel::new();
        ch.write_envelope(0xF0); // full volume, no envelope movement
        ch.frequency = 2044; // a short period, so a few ticks step the duty
        ch.trigger();
        ch
    }

    /// Collect one full duty cycle of output levels.
    fn duty_cycle(channel: &mut SquareChannel) -> Vec<u8> {
        let period = (2048 - channel.frequency as u32) * 4;
        let mut levels = vec![channel.output()];
        for _ in 0..7 {
            channel.tick(period);
            levels.push(channel.output());
        }
        levels
    }

    #[test]
    fn each_duty_setting_produces_its_documented_waveform() {
        for duty in 0..4u8 {
            let mut ch = armed_square();
            ch.duty = duty;
            let levels = duty_cycle(&mut ch);
            let expected: Vec<u8> = DUTY_PATTERNS[duty as usize]
                .iter()
                .map(|&on| on * 15)
                .collect();
            assert_eq!(levels, expected, "duty {duty}");
        }
    }

    #[test]
    fn the_high_and_low_duty_cycles_are_inverses_of_each_other() {
        // 25% and 75% differ only by inversion, which is why they sound the same.
        let inverted: Vec<u8> = DUTY_PATTERNS[1].iter().map(|&b| 1 - b).collect();
        assert_eq!(inverted, DUTY_PATTERNS[3].to_vec());
    }

    #[test]
    fn a_higher_frequency_value_means_a_shorter_period() {
        let mut low = SquareChannel::new();
        low.frequency = 0;
        let mut high = SquareChannel::new();
        high.frequency = 2000;
        assert!(high.period() < low.period());
        assert_eq!(low.period(), 2048 * 4);
    }

    #[test]
    fn a_square_channel_with_no_sweep_ignores_sweep_clocks() {
        let mut ch = armed_square();
        let frequency = ch.frequency;
        for _ in 0..10 {
            ch.clock_sweep();
        }
        assert_eq!(ch.frequency, frequency);
        assert!(ch.enabled);
    }

    #[test]
    fn a_sweeping_channel_updates_its_own_frequency() {
        let mut ch = SquareChannel::with_sweep();
        ch.write_envelope(0xF0);
        ch.frequency = 1000;
        if let Some(sweep) = &mut ch.sweep {
            assert!(sweep.write_register(0x11)); // period 1, increasing, shift 1
        }
        ch.trigger();

        ch.clock_sweep();
        assert_eq!(ch.frequency, 1500);
    }

    #[test]
    fn an_overflowing_sweep_disables_the_channel_it_belongs_to() {
        let mut ch = SquareChannel::with_sweep();
        ch.write_envelope(0xF0);
        ch.frequency = 2000;
        if let Some(sweep) = &mut ch.sweep {
            assert!(sweep.write_register(0x11));
        }
        ch.trigger();
        assert!(!ch.enabled, "the trigger-time check already killed it");
    }

    #[test]
    fn switching_the_dac_off_silences_a_playing_channel_immediately() {
        let mut ch = armed_square();
        assert!(ch.enabled);
        assert_ne!(ch.output(), 0);

        ch.write_envelope(0x00); // zero volume, decreasing
        assert!(!ch.dac_enabled);
        assert!(!ch.enabled);
        assert_eq!(ch.output(), 0);
    }

    #[test]
    fn a_length_expiry_stops_the_channel() {
        let mut ch = armed_square();
        ch.length.write_length(62);
        ch.length.enabled = true;
        ch.clock_length();
        assert!(ch.enabled);
        ch.clock_length();
        assert!(!ch.enabled);
        assert_eq!(ch.output(), 0);
    }

    // -- Wave ----------------------------------------------------------------

    fn armed_wave() -> WaveChannel {
        let mut ch = WaveChannel::new();
        ch.dac_enabled = true;
        ch.volume_shift = 1;
        // Sample n holds value n % 16.
        for (i, byte) in ch.wave_ram.iter_mut().enumerate() {
            let high = (i * 2) % 16;
            let low = (i * 2 + 1) % 16;
            *byte = ((high as u8) << 4) | low as u8;
        }
        ch.frequency = 2046;
        ch.trigger();
        ch
    }

    #[test]
    fn wave_ram_is_read_high_nibble_first() {
        let ch = armed_wave();
        assert_eq!(ch.output(), 0, "sample 0 is the high nibble of byte 0");

        let mut ch = ch;
        ch.tick((2048 - 2046) * 2);
        assert_eq!(ch.output(), 1, "sample 1 is the low nibble");
        ch.tick((2048 - 2046) * 2);
        assert_eq!(ch.output(), 2, "sample 2 is the high nibble of byte 1");
    }

    #[test]
    fn the_volume_shift_attenuates_by_powers_of_two() {
        let mut ch = armed_wave();
        // Move to a sample with a large value.
        for _ in 0..15 {
            ch.tick((2048 - 2046) * 2);
        }
        let full = ch.output();
        assert_eq!(full, 15);

        ch.volume_shift = 2;
        assert_eq!(ch.output(), 7);
        ch.volume_shift = 3;
        assert_eq!(ch.output(), 3);
        ch.volume_shift = 0;
        assert_eq!(ch.output(), 0, "shift 0 mutes entirely");
    }

    #[test]
    fn the_wave_channel_wraps_after_thirty_two_samples() {
        let mut ch = armed_wave();
        let first = ch.output();
        for _ in 0..32 {
            ch.tick((2048 - 2046) * 2);
        }
        assert_eq!(ch.output(), first);
    }

    #[test]
    fn the_wave_channel_uses_the_longer_length_counter() {
        let mut ch = WaveChannel::new();
        ch.length.write_length(0);
        assert_eq!(ch.length.counter, 256);
    }

    #[test]
    fn wave_channel_defaults_reproduce_game_boy_hardware_exactly() {
        // The check the GBA-only fields' whole design rests on: `sample_count`, `active_bank`,
        // and `force_75_percent` at their `WaveChannel::new` defaults must be provably invisible
        // to a Game Boy caller, not merely "probably fine because the other wave tests still
        // pass" — those tests exercise the same code paths the defaults do and would not catch a
        // default that quietly changed, only one that broke outright. This pins the full
        // thirty-two-sample cycle against a hand-computed reference built the same way
        // `armed_wave` is, independently of any of `output`'s own branches, so a mistake in the
        // bank-selection logic that happened to leave a Game Boy's *simple* cases unaffected
        // still shows up here.
        let mut ch = armed_wave();
        assert_eq!(ch.sample_count, 32, "the Game Boy's only mode");
        assert!(!ch.active_bank, "there is no second bank to select");
        assert!(!ch.force_75_percent, "no such bit exists on this hardware");

        // Reference built from the raw wave-RAM byte pattern `armed_wave` documents (sample n
        // holds value n % 16), independently of `output()` itself.
        let mut levels = Vec::with_capacity(32);
        let period = (2048 - ch.frequency as u32) * 2;
        levels.push(ch.output());
        for _ in 0..31 {
            ch.tick(period);
            levels.push(ch.output());
        }
        let full_volume: Vec<u8> = (0..32).map(|i: usize| (i % 16) as u8).collect();
        assert_eq!(levels, full_volume, "volume_shift 1 is full, unattenuated");

        ch.volume_shift = 2;
        ch.trigger();
        let mut halved = Vec::with_capacity(32);
        halved.push(ch.output());
        for _ in 0..31 {
            ch.tick(period);
            halved.push(ch.output());
        }
        let expected_halved: Vec<u8> = full_volume.iter().map(|&v| v >> 1).collect();
        assert_eq!(
            halved, expected_halved,
            "volume_shift 2 halves every sample"
        );
    }

    #[test]
    fn a_gba_channel_in_sixty_four_sample_mode_plays_bank_zero_then_bank_one() {
        let mut ch = WaveChannel::new();
        ch.dac_enabled = true;
        ch.volume_shift = 1;
        ch.sample_count = 64;
        ch.wave_ram = [0x11; WAVE_RAM_BYTES]; // every bank-0 sample is 1
        ch.wave_ram_bank1 = [0x22; WAVE_RAM_BYTES]; // every bank-1 sample is 2
        ch.frequency = 2046;
        ch.trigger();

        let period = (2048 - ch.frequency as u32) * 2;
        assert_eq!(ch.output(), 1, "sample 0 is bank 0");
        for _ in 0..31 {
            ch.tick(period);
        }
        assert_eq!(ch.output(), 1, "sample 31 is still bank 0");
        ch.tick(period);
        assert_eq!(ch.output(), 2, "sample 32 crosses into bank 1");
        for _ in 0..31 {
            ch.tick(period);
        }
        assert_eq!(ch.output(), 2, "sample 63 is still bank 1");
        ch.tick(period);
        assert_eq!(ch.output(), 1, "sample 64 wraps back to bank 0");
    }

    #[test]
    fn active_bank_only_matters_in_thirty_two_sample_mode() {
        let mut ch = WaveChannel::new();
        ch.dac_enabled = true;
        ch.volume_shift = 1;
        ch.wave_ram = [0x11; WAVE_RAM_BYTES];
        ch.wave_ram_bank1 = [0x22; WAVE_RAM_BYTES];
        ch.frequency = 2046;

        ch.active_bank = true;
        ch.trigger();
        assert_eq!(ch.output(), 2, "32-sample mode: active_bank picks bank 1");

        ch.sample_count = 64;
        ch.trigger();
        assert_eq!(
            ch.output(),
            1,
            "64-sample mode always starts from bank 0, active_bank or not"
        );
    }

    #[test]
    fn force_75_percent_overrides_volume_shift_entirely() {
        let mut ch = WaveChannel::new();
        ch.dac_enabled = true;
        ch.wave_ram = [0xFF; WAVE_RAM_BYTES]; // every sample is 15, the loudest nibble
        ch.trigger();

        for shift in [0u8, 1, 2, 3] {
            ch.volume_shift = shift;
            ch.force_75_percent = true;
            assert_eq!(
                ch.output(),
                11,
                "shift {shift}: 15 * 3 / 4 regardless of the two-bit field"
            );
        }

        ch.force_75_percent = false;
        assert_eq!(
            ch.output(),
            3,
            "shift 3 (quarter) applies once the override is off"
        );
    }

    // -- Noise ---------------------------------------------------------------

    fn armed_noise(short_mode: bool) -> NoiseChannel {
        let mut ch = NoiseChannel::new();
        ch.write_envelope(0xF0);
        ch.short_mode = short_mode;
        ch.divisor_code = 0;
        ch.clock_shift = 0;
        ch.trigger();
        ch
    }

    #[test]
    fn the_shift_register_starts_full_so_the_first_output_is_silent() {
        let ch = armed_noise(false);
        assert_eq!(ch.lfsr(), 0x7FFF);
        assert_eq!(ch.output(), 0, "bit 0 is set, and the output is inverted");
    }

    #[test]
    fn the_long_sequence_repeats_after_32767_steps() {
        let mut ch = armed_noise(false);
        let period = ch.period() as u32;
        let start = ch.lfsr();

        let mut steps = 0u32;
        loop {
            ch.tick(period);
            steps += 1;
            if ch.lfsr() == start {
                break;
            }
            assert!(steps < 40_000, "the sequence never repeated");
        }
        assert_eq!(steps, 32767);
    }

    #[test]
    fn the_short_sequence_repeats_after_127_steps() {
        // Short enough to hear as a pitch rather than as noise, which games use deliberately.
        //
        // Only the low seven bits form the short sequence: the upper bits keep shifting and
        // do not return to their starting value on the same cycle, so comparing the whole
        // register would never find the repeat.
        let mut ch = armed_noise(true);
        let period = ch.period() as u32;
        let start = ch.lfsr() & 0x7F;

        let mut steps = 0u32;
        loop {
            ch.tick(period);
            steps += 1;
            if ch.lfsr() & 0x7F == start {
                break;
            }
            assert!(steps < 1000, "the sequence never repeated");
        }
        assert_eq!(steps, 127);
    }

    #[test]
    fn the_divisor_and_shift_together_set_the_clock_rate() {
        let mut ch = NoiseChannel::new();
        ch.divisor_code = 0;
        ch.clock_shift = 0;
        assert_eq!(ch.period(), 8, "divisor code 0 is 8, not 0");

        ch.divisor_code = 3;
        assert_eq!(ch.period(), 48);
        ch.clock_shift = 2;
        assert_eq!(ch.period(), 48 * 4);
    }

    #[test]
    fn noise_output_follows_the_envelope_volume() {
        let mut ch = armed_noise(false);
        let period = ch.period() as u32;
        // Step until the register's low bit clears, which is when the channel is audible.
        for _ in 0..100 {
            ch.tick(period);
            if ch.lfsr() & 1 == 0 {
                break;
            }
        }
        assert_eq!(ch.output(), 15);

        ch.envelope.volume = 4;
        assert_eq!(ch.output(), 4);
    }

    // -- State ---------------------------------------------------------------

    #[test]
    fn every_channel_round_trips_through_a_save_state() {
        let mut square = SquareChannel::with_sweep();
        square.write_envelope(0xA3);
        square.frequency = 1234;
        square.trigger();
        square.tick(100);

        let mut w = StateWriter::new();
        square.save(&mut w);
        let blob = w.into_inner();
        let mut restored = SquareChannel::with_sweep();
        restored.load(&mut StateReader::new(&blob)).unwrap();
        assert_eq!(restored, square);

        let mut wave = armed_wave();
        wave.tick(500);
        let mut w = StateWriter::new();
        wave.save(&mut w);
        let blob = w.into_inner();
        let mut restored = WaveChannel::new();
        restored.load(&mut StateReader::new(&blob)).unwrap();
        assert_eq!(restored, wave);

        let mut noise = armed_noise(true);
        noise.tick(1000);
        let mut w = StateWriter::new();
        noise.save(&mut w);
        let blob = w.into_inner();
        let mut restored = NoiseChannel::new();
        restored.load(&mut StateReader::new(&blob)).unwrap();
        assert_eq!(restored, noise);
    }

    #[test]
    fn a_restored_channel_continues_the_same_waveform() {
        let mut square = armed_square();
        square.tick(37);

        let mut w = StateWriter::new();
        square.save(&mut w);
        let blob = w.into_inner();
        let mut restored = SquareChannel::new();
        restored.load(&mut StateReader::new(&blob)).unwrap();

        for _ in 0..20 {
            square.tick(13);
            restored.tick(13);
            assert_eq!(restored.output(), square.output());
        }
    }
}
