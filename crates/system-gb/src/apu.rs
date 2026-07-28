//! The Game Boy APU: four channels behind the `NR1x`–`NR5x` registers.
//!
//! The channels themselves live in `apu-shared`. This module is the register layer — decoding
//! writes, applying the read masks, and generating output samples at a fixed rate.
//!
//! # Two clocks, deliberately separate
//!
//! Channel *waveforms* advance with the CPU clock, through [`GbApu::tick`]. The length,
//! envelope, and sweep units advance on the 512 Hz frame sequencer, which the scheduler owns
//! and delivers through [`TimingOutput`](crate::timing::TimingOutput). Output *samples* are
//! generated at a third rate entirely. Conflating any two of those produces audio that is
//! subtly the wrong speed.

use apu_shared::{Mixer, NoiseChannel, SquareChannel, WaveChannel, WAVE_RAM_BYTES};
use core_common::{AudioSample, Savable, StateError, StateReader, StateWriter, AUDIO_SAMPLE_RATE};

use crate::timing::{TimingOutput, CLOCK_HZ};

/// APU register addresses.
pub mod reg {
    pub const NR10: u16 = 0xFF10;
    pub const NR11: u16 = 0xFF11;
    pub const NR12: u16 = 0xFF12;
    pub const NR13: u16 = 0xFF13;
    pub const NR14: u16 = 0xFF14;
    pub const NR21: u16 = 0xFF16;
    pub const NR22: u16 = 0xFF17;
    pub const NR23: u16 = 0xFF18;
    pub const NR24: u16 = 0xFF19;
    pub const NR30: u16 = 0xFF1A;
    pub const NR31: u16 = 0xFF1B;
    pub const NR32: u16 = 0xFF1C;
    pub const NR33: u16 = 0xFF1D;
    pub const NR34: u16 = 0xFF1E;
    pub const NR41: u16 = 0xFF20;
    pub const NR42: u16 = 0xFF21;
    pub const NR43: u16 = 0xFF22;
    pub const NR44: u16 = 0xFF23;
    pub const NR50: u16 = 0xFF24;
    pub const NR51: u16 = 0xFF25;
    pub const NR52: u16 = 0xFF26;

    pub const WAVE_RAM_START: u16 = 0xFF30;
    pub const WAVE_RAM_END: u16 = 0xFF3F;

    pub const RANGE_START: u16 = NR10;
    pub const RANGE_END: u16 = WAVE_RAM_END;
}

/// Bits that read as one because the underlying field is write-only.
///
/// Most `NRxx` registers are partly write-only, and reading returns ones in those positions
/// rather than the value written. Games do read these registers back, so returning the raw
/// value is a visible difference — not a harmless simplification.
fn read_mask(addr: u16) -> u8 {
    match addr {
        reg::NR10 => 0x80,
        reg::NR11 | reg::NR21 => 0x3F,
        reg::NR12 | reg::NR22 | reg::NR42 | reg::NR43 | reg::NR50 | reg::NR51 => 0x00,
        reg::NR13 | reg::NR23 | reg::NR31 | reg::NR33 | reg::NR41 => 0xFF,
        reg::NR14 | reg::NR24 | reg::NR34 | reg::NR44 => 0xBF,
        reg::NR30 => 0x7F,
        reg::NR32 => 0x9F,
        reg::NR52 => 0x70,
        _ => 0x00,
    }
}

/// The Game Boy sound hardware.
pub struct GbApu {
    pub ch1: SquareChannel,
    pub ch2: SquareChannel,
    pub ch3: WaveChannel,
    pub ch4: NoiseChannel,
    pub mixer: Mixer,

    /// `NR52` bit 7. Clearing it resets every other register and ignores writes to them.
    powered: bool,

    /// The raw value last written to each register, for the masked read-back.
    written: [u8; 0x30],

    /// Fixed-point accumulator for sample generation, counting emulated cycles scaled by the
    /// output rate. Integer arithmetic keeps it exactly periodic, where a float accumulator
    /// would drift.
    sample_accumulator: u64,

    /// Samples produced since the last drain.
    samples: Vec<AudioSample>,
    /// Handed out by `take_samples`, swapped with `samples` so nothing allocates per frame.
    drained: Vec<AudioSample>,
}

impl Default for GbApu {
    fn default() -> Self {
        Self::new()
    }
}

impl GbApu {
    pub fn new() -> Self {
        Self {
            ch1: SquareChannel::with_sweep(),
            ch2: SquareChannel::new(),
            ch3: WaveChannel::new(),
            ch4: NoiseChannel::new(),
            mixer: Mixer::default(),
            powered: true,
            written: [0; 0x30],
            sample_accumulator: 0,
            samples: Vec::with_capacity(1024),
            drained: Vec::with_capacity(1024),
        }
    }

    pub fn reset(&mut self) {
        let drained = std::mem::take(&mut self.drained);
        let samples = std::mem::take(&mut self.samples);
        *self = Self::new();
        self.drained = drained;
        self.samples = samples;
        self.drained.clear();
        self.samples.clear();
    }

    /// Advance the channel waveforms and generate output samples.
    pub fn tick(&mut self, cycles: u64) {
        if self.powered {
            self.ch1.tick(cycles as u32);
            self.ch2.tick(cycles as u32);
            self.ch3.tick(cycles as u32);
            self.ch4.tick(cycles as u32);
        }

        // Emit one sample every CLOCK_HZ / AUDIO_SAMPLE_RATE cycles, tracked in fixed point so
        // the rate stays exact rather than accumulating rounding error.
        self.sample_accumulator += cycles * AUDIO_SAMPLE_RATE as u64;
        while self.sample_accumulator >= CLOCK_HZ {
            self.sample_accumulator -= CLOCK_HZ;
            self.samples.push(self.current_sample());
        }
    }

    fn current_sample(&self) -> AudioSample {
        if !self.powered {
            return AudioSample::SILENCE;
        }
        self.mixer.mix([
            self.ch1.signal(),
            self.ch2.signal(),
            self.ch3.signal(),
            self.ch4.signal(),
        ])
    }

    /// Apply the frame-sequencer clocks the scheduler reported.
    ///
    /// Counts rather than flags, so a caller that advanced by a long jump still applies each
    /// one instead of collapsing several into a single step.
    pub fn apply_sequencer(&mut self, timing: &TimingOutput) {
        if !self.powered {
            return;
        }
        for _ in 0..timing.apu_length_clocks {
            self.ch1.clock_length();
            self.ch2.clock_length();
            self.ch3.clock_length();
            self.ch4.clock_length();
        }
        for _ in 0..timing.apu_sweep_clocks {
            self.ch1.clock_sweep();
        }
        for _ in 0..timing.apu_envelope_clocks {
            self.ch1.clock_envelope();
            self.ch2.clock_envelope();
            self.ch4.clock_envelope();
        }
    }

    /// Samples produced since the previous call.
    pub fn take_samples(&mut self) -> &[AudioSample] {
        std::mem::swap(&mut self.samples, &mut self.drained);
        self.samples.clear();
        &self.drained
    }

    pub fn is_powered(&self) -> bool {
        self.powered
    }

    /// Whether this address belongs to the APU.
    pub fn owns(addr: u16) -> bool {
        (reg::RANGE_START..=reg::RANGE_END).contains(&addr)
    }

    pub fn read_register(&self, addr: u16) -> Option<u8> {
        if !Self::owns(addr) {
            return None;
        }
        if (reg::WAVE_RAM_START..=reg::WAVE_RAM_END).contains(&addr) {
            return Some(self.ch3.wave_ram[(addr - reg::WAVE_RAM_START) as usize]);
        }
        if addr == reg::NR52 {
            // The low four bits report which channels are still sounding, and are read-only.
            let status = (self.ch1.enabled as u8)
                | ((self.ch2.enabled as u8) << 1)
                | ((self.ch3.enabled as u8) << 2)
                | ((self.ch4.enabled as u8) << 3);
            return Some(((self.powered as u8) << 7) | read_mask(addr) | status);
        }
        let stored = self.written[(addr - reg::RANGE_START) as usize];
        Some(stored | read_mask(addr))
    }

    pub fn write_register(&mut self, addr: u16, value: u8) -> Option<()> {
        if !Self::owns(addr) {
            return None;
        }

        // Wave RAM stays accessible with the APU powered down, unlike everything else.
        if (reg::WAVE_RAM_START..=reg::WAVE_RAM_END).contains(&addr) {
            self.ch3.wave_ram[(addr - reg::WAVE_RAM_START) as usize] = value;
            return Some(());
        }

        if addr == reg::NR52 {
            self.set_power(value & 0x80 != 0);
            self.written[(addr - reg::RANGE_START) as usize] = value & 0x80;
            return Some(());
        }

        // With the APU off, every other register write is discarded.
        if !self.powered {
            return Some(());
        }
        self.written[(addr - reg::RANGE_START) as usize] = value;

        match addr {
            reg::NR10 => {
                if let Some(sweep) = &mut self.ch1.sweep {
                    sweep.write_register(value);
                }
            }
            reg::NR11 => {
                self.ch1.duty = value >> 6;
                self.ch1.length.write_length((value & 0x3F) as u16);
            }
            reg::NR12 => self.ch1.write_envelope(value),
            reg::NR13 => {
                self.ch1.frequency = (self.ch1.frequency & 0x700) | value as u16;
            }
            reg::NR14 => {
                self.ch1.frequency = (self.ch1.frequency & 0x00FF) | (((value & 0x07) as u16) << 8);
                self.ch1.length.enabled = value & 0x40 != 0;
                if value & 0x80 != 0 {
                    self.ch1.trigger();
                }
            }

            reg::NR21 => {
                self.ch2.duty = value >> 6;
                self.ch2.length.write_length((value & 0x3F) as u16);
            }
            reg::NR22 => self.ch2.write_envelope(value),
            reg::NR23 => {
                self.ch2.frequency = (self.ch2.frequency & 0x700) | value as u16;
            }
            reg::NR24 => {
                self.ch2.frequency = (self.ch2.frequency & 0x00FF) | (((value & 0x07) as u16) << 8);
                self.ch2.length.enabled = value & 0x40 != 0;
                if value & 0x80 != 0 {
                    self.ch2.trigger();
                }
            }

            reg::NR30 => {
                // The wave channel's DAC has its own bit rather than being implied by an
                // envelope, because it has no envelope.
                self.ch3.dac_enabled = value & 0x80 != 0;
                if !self.ch3.dac_enabled {
                    self.ch3.enabled = false;
                }
            }
            reg::NR31 => self.ch3.length.write_length(value as u16),
            reg::NR32 => self.ch3.volume_shift = (value >> 5) & 0x03,
            reg::NR33 => {
                self.ch3.frequency = (self.ch3.frequency & 0x700) | value as u16;
            }
            reg::NR34 => {
                self.ch3.frequency = (self.ch3.frequency & 0x00FF) | (((value & 0x07) as u16) << 8);
                self.ch3.length.enabled = value & 0x40 != 0;
                if value & 0x80 != 0 {
                    self.ch3.trigger();
                }
            }

            reg::NR41 => self.ch4.length.write_length((value & 0x3F) as u16),
            reg::NR42 => self.ch4.write_envelope(value),
            reg::NR43 => {
                self.ch4.clock_shift = value >> 4;
                self.ch4.short_mode = value & 0x08 != 0;
                self.ch4.divisor_code = value & 0x07;
            }
            reg::NR44 => {
                self.ch4.length.enabled = value & 0x40 != 0;
                if value & 0x80 != 0 {
                    self.ch4.trigger();
                }
            }

            reg::NR50 => self.mixer.write_nr50(value),
            reg::NR51 => self.mixer.write_nr51(value),
            _ => {}
        }
        Some(())
    }

    /// `NR52` bit 7.
    ///
    /// Powering down clears every register and silences everything; powering back up starts
    /// from a blank slate. Wave RAM is the exception and survives, which is why games load a
    /// waveform before switching the APU on.
    fn set_power(&mut self, on: bool) {
        if on == self.powered {
            return;
        }
        self.powered = on;
        if on {
            return;
        }

        let wave_ram = self.ch3.wave_ram;
        self.ch1 = SquareChannel::with_sweep();
        self.ch2 = SquareChannel::new();
        self.ch3 = WaveChannel::new();
        self.ch3.wave_ram = wave_ram;
        self.ch4 = NoiseChannel::new();
        self.mixer = Mixer::default();
        self.written = [0; 0x30];
    }
}

impl Savable for GbApu {
    fn save(&self, w: &mut StateWriter) {
        self.ch1.save(w);
        self.ch2.save(w);
        self.ch3.save(w);
        self.ch4.save(w);
        self.mixer.save(w);
        w.write_bool(self.powered);
        w.write_bytes(&self.written);
        w.write_u64(self.sample_accumulator);
        // The staging buffers are output in flight, not emulated state: saving them would
        // duplicate samples the frontend has already consumed.
    }

    fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        self.ch1.load(r)?;
        self.ch2.load(r)?;
        self.ch3.load(r)?;
        self.ch4.load(r)?;
        self.mixer.load(r)?;
        self.powered = r.read_bool()?;
        r.read_bytes(&mut self.written)?;
        self.sample_accumulator = r.read_u64()?;
        self.samples.clear();
        self.drained.clear();
        Ok(())
    }
}

/// Re-exported so callers can size buffers without depending on `apu-shared` directly.
pub const WAVE_RAM_SIZE: usize = WAVE_RAM_BYTES;

#[cfg(test)]
mod tests {
    use super::*;

    fn apu() -> GbApu {
        GbApu::new()
    }

    /// Start channel 1 at full volume with a short period.
    fn start_ch1(apu: &mut GbApu) {
        apu.write_register(reg::NR12, 0xF0); // full volume, no envelope movement
        apu.write_register(reg::NR13, 0xFF);
        apu.write_register(reg::NR14, 0x87); // trigger, frequency high bits
    }

    #[test]
    fn a_trigger_starts_the_channel_and_nr52_reports_it() {
        let mut a = apu();
        assert_eq!(a.read_register(reg::NR52).unwrap() & 0x01, 0);

        start_ch1(&mut a);
        assert!(a.ch1.enabled);
        assert_eq!(a.read_register(reg::NR52).unwrap() & 0x01, 0x01);
    }

    #[test]
    fn write_only_bits_read_back_as_ones() {
        // Games do read these back, so returning the raw value is a visible difference.
        let mut a = apu();
        a.write_register(reg::NR11, 0x00);
        assert_eq!(
            a.read_register(reg::NR11),
            Some(0x3F),
            "only the duty bits are readable"
        );

        a.write_register(reg::NR13, 0x55);
        assert_eq!(a.read_register(reg::NR13), Some(0xFF), "wholly write-only");

        a.write_register(reg::NR12, 0xA5);
        assert_eq!(a.read_register(reg::NR12), Some(0xA5), "fully readable");
    }

    #[test]
    fn the_frequency_is_assembled_from_two_registers() {
        let mut a = apu();
        a.write_register(reg::NR13, 0x34);
        a.write_register(reg::NR14, 0x05);
        assert_eq!(a.ch1.frequency, 0x534);

        // Writing the low byte must not disturb the high bits.
        a.write_register(reg::NR13, 0x78);
        assert_eq!(a.ch1.frequency, 0x578);
    }

    #[test]
    fn each_channels_registers_reach_the_right_channel() {
        let mut a = apu();
        a.write_register(reg::NR11, 0xC0); // duty 3
        a.write_register(reg::NR21, 0x40); // duty 1
        assert_eq!(a.ch1.duty, 3);
        assert_eq!(a.ch2.duty, 1);

        a.write_register(reg::NR32, 0x40); // volume shift 2
        assert_eq!(a.ch3.volume_shift, 2);

        a.write_register(reg::NR43, 0x5B); // shift 5, short mode, divisor 3
        assert_eq!(a.ch4.clock_shift, 5);
        assert!(a.ch4.short_mode);
        assert_eq!(a.ch4.divisor_code, 3);
    }

    #[test]
    fn only_channel_one_has_a_sweep() {
        let a = apu();
        assert!(a.ch1.sweep.is_some());
        assert!(a.ch2.sweep.is_none());
    }

    #[test]
    fn the_wave_channel_dac_has_its_own_bit() {
        // It has no envelope, so the usual "zero volume counting down" rule cannot apply.
        let mut a = apu();
        a.write_register(reg::NR30, 0x80);
        assert!(a.ch3.dac_enabled);
        a.write_register(reg::NR34, 0x80); // trigger
        assert!(a.ch3.enabled);

        a.write_register(reg::NR30, 0x00);
        assert!(!a.ch3.enabled, "clearing the DAC bit silences it at once");
    }

    #[test]
    fn powering_down_clears_the_registers_and_ignores_writes() {
        let mut a = apu();
        start_ch1(&mut a);
        a.write_register(reg::NR50, 0x77);

        a.write_register(reg::NR52, 0x00);
        assert!(!a.is_powered());
        assert!(!a.ch1.enabled);
        assert_eq!(a.read_register(reg::NR50), Some(0x00), "registers cleared");

        // Writes are discarded while powered down.
        a.write_register(reg::NR12, 0xF0);
        a.write_register(reg::NR14, 0x80);
        assert!(!a.ch1.enabled);

        a.write_register(reg::NR52, 0x80);
        assert!(a.is_powered());
    }

    #[test]
    fn wave_ram_survives_a_power_cycle() {
        // Which is why games load a waveform before switching the APU on.
        let mut a = apu();
        a.write_register(reg::WAVE_RAM_START, 0xAB);
        a.write_register(reg::NR52, 0x00);
        assert_eq!(a.read_register(reg::WAVE_RAM_START), Some(0xAB));

        // And it is writable while powered down.
        a.write_register(reg::WAVE_RAM_START + 1, 0xCD);
        a.write_register(reg::NR52, 0x80);
        assert_eq!(a.read_register(reg::WAVE_RAM_START + 1), Some(0xCD));
    }

    #[test]
    fn a_powered_down_apu_produces_silence() {
        let mut a = apu();
        start_ch1(&mut a);
        a.write_register(reg::NR50, 0x77);
        a.write_register(reg::NR51, 0xFF);
        a.tick(1000);
        assert!(a.take_samples().iter().any(|s| s.left != 0.0));

        a.write_register(reg::NR52, 0x00);
        a.tick(1000);
        assert!(a
            .take_samples()
            .iter()
            .all(|s| s.left == 0.0 && s.right == 0.0));
    }

    #[test]
    fn samples_are_generated_at_the_output_rate() {
        let mut a = apu();
        // One second of emulated time should yield one second of audio.
        a.tick(CLOCK_HZ);
        let produced = a.take_samples().len();
        assert!(
            (produced as i64 - AUDIO_SAMPLE_RATE as i64).abs() <= 1,
            "expected about {AUDIO_SAMPLE_RATE}, got {produced}"
        );
    }

    #[test]
    fn the_sample_rate_does_not_drift_across_many_small_ticks() {
        // A float accumulator would lose a fraction every tick; the fixed-point one does not.
        let mut a = apu();
        let mut total = 0usize;
        for _ in 0..(CLOCK_HZ / 100) {
            a.tick(100);
            total += a.take_samples().len();
        }
        assert!(
            (total as i64 - AUDIO_SAMPLE_RATE as i64).abs() <= 2,
            "expected about {AUDIO_SAMPLE_RATE}, got {total}"
        );
    }

    #[test]
    fn samples_are_drained_exactly_once() {
        let mut a = apu();
        a.tick(10_000);
        assert!(!a.take_samples().is_empty());
        assert!(a.take_samples().is_empty(), "and not handed out twice");
    }

    #[test]
    fn the_sequencer_clocks_reach_the_units_that_have_them() {
        let mut a = apu();
        a.write_register(reg::NR12, 0xF1); // volume 15, decreasing, period 1
        a.write_register(reg::NR14, 0x80);
        assert_eq!(a.ch1.envelope.volume, 15);

        a.apply_sequencer(&TimingOutput {
            apu_envelope_clocks: 1,
            ..Default::default()
        });
        assert_eq!(a.ch1.envelope.volume, 14);

        // Counts, not flags: a long jump applies each clock rather than collapsing them.
        a.apply_sequencer(&TimingOutput {
            apu_envelope_clocks: 3,
            ..Default::default()
        });
        assert_eq!(a.ch1.envelope.volume, 11);
    }

    #[test]
    fn a_length_clock_can_silence_a_channel() {
        let mut a = apu();
        a.write_register(reg::NR12, 0xF0);
        a.write_register(reg::NR11, 0x3F); // one step of length remains
        a.write_register(reg::NR14, 0xC0); // trigger with length enabled
        assert!(a.ch1.enabled);

        a.apply_sequencer(&TimingOutput {
            apu_length_clocks: 1,
            ..Default::default()
        });
        assert!(!a.ch1.enabled);
        assert_eq!(a.read_register(reg::NR52).unwrap() & 0x01, 0);
    }

    #[test]
    fn only_channel_one_receives_sweep_clocks() {
        let mut a = apu();
        a.write_register(reg::NR10, 0x11); // period 1, increasing, shift 1
        a.write_register(reg::NR12, 0xF0);
        a.write_register(reg::NR13, 0x00);
        a.write_register(reg::NR14, 0x82); // trigger, frequency 0x200
        let before = a.ch1.frequency;

        a.apply_sequencer(&TimingOutput {
            apu_sweep_clocks: 1,
            ..Default::default()
        });
        assert!(a.ch1.frequency > before, "the sweep moved it");
    }

    #[test]
    fn addresses_outside_the_apu_are_not_claimed() {
        let mut a = apu();
        assert_eq!(a.read_register(0xFF0F), None);
        assert_eq!(a.write_register(0xFF40, 0), None);
        assert!(GbApu::owns(reg::NR10));
        assert!(GbApu::owns(reg::WAVE_RAM_END));
        assert!(!GbApu::owns(0xFF0F));
        assert!(!GbApu::owns(0xFF40));
    }

    #[test]
    fn apu_state_round_trips_and_resumes_identically() {
        let mut a = apu();
        start_ch1(&mut a);
        a.write_register(reg::NR50, 0x57);
        a.write_register(reg::NR51, 0xF3);
        a.write_register(reg::WAVE_RAM_START, 0x9C);
        a.tick(5_000);
        a.take_samples();

        let mut w = StateWriter::new();
        a.save(&mut w);
        let blob = w.into_inner();

        let mut restored = apu();
        restored.load(&mut StateReader::new(&blob)).unwrap();

        assert_eq!(restored.ch1, a.ch1);
        assert_eq!(restored.mixer, a.mixer);
        assert_eq!(restored.read_register(reg::WAVE_RAM_START), Some(0x9C));

        // Both must then produce the same audio.
        a.tick(2_000);
        restored.tick(2_000);
        assert_eq!(restored.take_samples(), a.take_samples());
    }
}
