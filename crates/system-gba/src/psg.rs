//! The GBA's programmable sound generators: the Game Boy's channels, re-addressed.
//!
//! The GBA makes sound two ways. [`crate::fifo`] is one of them — two byte queues a game fills
//! by DMA. This is the other, and it is the older one: the same four channels the Game Boy has,
//! carried forward unchanged, which is why the *channels* here are `apu-shared`'s and only the
//! address decode is new.
//!
//! # What is new is the decode, not the hardware
//!
//! The channels behave exactly as they do on a CGB, so nothing is gated on a model the way
//! `system-gb::apu` gates three behaviours. What differs is the register layer:
//!
//! - The registers are **halfwords from `0x0400_0060` with gaps between them**, not the Game
//!   Boy's contiguous `NR10`-`NR52` byte block. Each halfword packs the two `NRxx` bytes it
//!   replaces, low byte first, and the addresses in between decode to nothing at all.
//! - Write-only fields **read back as zero**, not as ones. That is the GBA I/O convention and
//!   the opposite of the Game Boy's, where an undriven bit floats high.
//! - The PSG passes through a **second volume control**, `SOUNDCNT_H` bits 0-1, on top of the
//!   `SOUNDCNT_L` master volume it already had. See [`Psg::output`].
//!
//! # Two clocks, neither of them the CPU's
//!
//! The channels count Game Boy t-cycles, and the GBA's CPU runs at four times that clock. Feeding
//! them CPU cycles directly would play every note two octaves high, so [`Psg::tick`] divides —
//! carrying the remainder, since an instruction rarely costs a multiple of four.
//!
//! The 512 Hz frame sequencer that clocks lengths, envelopes, and sweeps lives here rather than
//! in the scheduler, unlike the Game Boy's: this machine has no scheduler to put it in (see
//! [`crate::system`]), and nothing outside audio wants it.
//!
//! # Channel 3 has two banks, and the CPU only ever sees one of them
//!
//! Wave RAM on this machine is two sixteen-byte banks rather than the Game Boy's one, plus a
//! 64-sample mode that plays both back to back — a real difference `apu_shared::WaveChannel`
//! did not model until it grew additive fields for exactly this (`sample_count`,
//! `wave_ram_bank1`, `active_bank`, `force_75_percent`; all default to the Game Boy's single-bank
//! behaviour, which is why extending the type was safe rather than a second implementation).
//!
//! `SOUND3CNT_L` bit 6 selects which bank *plays* in 32-sample mode; the wave RAM window at
//! `0x0400_0090` always exposes the *other* one, so a game can load a fresh waveform into the
//! bank that is not currently sounding and flip the bit to swap them.
//! That swap-while-playing idiom is the reason the window exists at all rather than a single
//! sixteen-byte block; a game not using it just leaves the bit alone and always writes the same
//! bank. **Not independently verified**: which bank the window exposes in 64-sample mode, where
//! both are already in use for playback and the inactive-bank idiom does not apply the same way.
//! This follows the same bit regardless, rather than guessing at a special case with nothing in
//! the corpus to check it against.

use apu_shared::{Mixer, NoiseChannel, SquareChannel, WaveChannel, WAVE_RAM_BYTES};
use core_common::{Savable, StateError, StateReader, StateWriter};

/// Register addresses. Each holds the two `NRxx` bytes named beside it, low byte first.
pub mod reg {
    /// Channel 1 sweep — `NR10`.
    pub const SOUND1CNT_L: u32 = 0x0400_0060;
    /// Channel 1 duty, length, and envelope — `NR11`, `NR12`.
    pub const SOUND1CNT_H: u32 = 0x0400_0062;
    /// Channel 1 frequency and trigger — `NR13`, `NR14`.
    pub const SOUND1CNT_X: u32 = 0x0400_0064;
    /// Channel 2 duty, length, and envelope — `NR21`, `NR22`.
    pub const SOUND2CNT_L: u32 = 0x0400_0068;
    /// Channel 2 frequency and trigger — `NR23`, `NR24`.
    pub const SOUND2CNT_H: u32 = 0x0400_006C;
    /// Channel 3 dimension, bank select, and DAC power — `NR30`, repositioned and extended.
    pub const SOUND3CNT_L: u32 = 0x0400_0070;
    /// Channel 3 length, volume, and the force-75%-volume bit `NR32` has no room for.
    pub const SOUND3CNT_H: u32 = 0x0400_0072;
    /// Channel 3 frequency and trigger — `NR33`, `NR34`.
    pub const SOUND3CNT_X: u32 = 0x0400_0074;
    /// Channel 4 length and envelope — `NR41`, `NR42`.
    pub const SOUND4CNT_L: u32 = 0x0400_0078;
    /// Channel 4 noise parameters and trigger — `NR43`, `NR44`.
    pub const SOUND4CNT_H: u32 = 0x0400_007C;
    /// Master volume and per-channel panning — `NR50`, `NR51`.
    pub const SOUNDCNT_L: u32 = 0x0400_0080;
    /// Sixteen bytes exposing whichever wave-RAM bank the CPU currently owns. Not part of the
    /// control-register block below — this is sample data, not a register with write-only bits.
    pub const WAVE_RAM: u32 = 0x0400_0090;

    /// The first address of the block, which the read-back table is indexed from.
    pub const BLOCK_START: u32 = SOUND1CNT_L;
    /// One past the last address of the block.
    pub const BLOCK_END: u32 = SOUNDCNT_L + 2;
}

/// Bytes in the wave-RAM window: one bank's worth.
const WAVE_RAM_WINDOW_BYTES: u32 = WAVE_RAM_BYTES as u32;

/// Halfword slots between [`reg::BLOCK_START`] and [`reg::BLOCK_END`], gaps included.
const SLOTS: usize = ((reg::BLOCK_END - reg::BLOCK_START) / 2) as usize;

/// CPU cycles per Game Boy t-cycle. The GBA runs its PSG from the same divider the Game Boy
/// does, off a clock four times as fast.
const CYCLES_PER_TICK: u32 = 4;

/// T-cycles between frame-sequencer steps: 4194304 / 512.
const SEQUENCER_PERIOD: u32 = 8192;

/// Which bits of a register a read actually returns.
///
/// Zero in every write-only position, because this machine reads an undriven I/O bit as 0 — the
/// Game Boy returns ones there and `system-gb::apu::read_mask` is built the other way up for
/// exactly that reason. A game that reads `SOUND1CNT_X` back gets its length-enable bit and
/// nothing else: not the frequency it wrote, and never the trigger, which is a strobe.
fn read_mask(addr: u32) -> u16 {
    match addr {
        // Sweep shift, direction, and time. Bits 7-15 do not exist.
        reg::SOUND1CNT_L => 0x007F,
        // Duty and the whole envelope byte; the length is write-only.
        reg::SOUND1CNT_H | reg::SOUND2CNT_L => 0xFFC0,
        // Only the length-enable flag. The frequency and the trigger are write-only.
        reg::SOUND1CNT_X | reg::SOUND2CNT_H | reg::SOUND3CNT_X => 0x4000,
        // Dimension and bank select; the DAC power bit and the four unused low bits are not
        // readable. GBATEK marks bit 7 write-only here, unlike every other channel's on/off
        // equivalent — there is no register-level way to ask a GBA whether channel 3 is powered.
        reg::SOUND3CNT_L => 0x0060,
        // Volume and the force-75% bit; the length in the low byte is write-only.
        reg::SOUND3CNT_H => 0xE000,
        // The envelope byte only; channel 4's length shares the low byte and is write-only.
        reg::SOUND4CNT_L => 0xFF00,
        // The noise parameters are fully readable, plus the length-enable flag.
        reg::SOUND4CNT_H => 0x40FF,
        // Both volumes and all eight panning bits. Bits 3 and 7 are the Game Boy's `Vin` mixing
        // bits, and this machine has no cartridge audio pin to mix in.
        reg::SOUNDCNT_L => 0xFF77,
        _ => 0,
    }
}

/// The four PSG channels and their register layer.
#[derive(Debug, Clone, PartialEq)]
pub struct Psg {
    pub ch1: SquareChannel,
    pub ch2: SquareChannel,
    pub ch3: WaveChannel,
    pub ch4: NoiseChannel,
    /// `SOUNDCNT_L`: panning and the PSG's own master volume.
    pub mixer: Mixer,

    /// `SOUNDCNT_X` bit 7, which gates direct sound and the PSG together on hardware. Owned by
    /// [`crate::fifo::DirectSoundControl`] and mirrored here through [`Psg::set_power`].
    powered: bool,

    /// The raw halfword last written to each slot, for the masked read-back.
    written: [u16; SLOTS],

    /// CPU cycles not yet worth a whole t-cycle.
    divider: u32,
    /// T-cycles since the last frame-sequencer step.
    sequencer_timer: u32,
    /// Which of the eight sequencer steps ran last.
    sequencer_step: u8,
}

impl Default for Psg {
    fn default() -> Self {
        Self::new()
    }
}

impl Psg {
    pub fn new() -> Self {
        Self {
            ch1: SquareChannel::with_sweep(),
            ch2: SquareChannel::new(),
            ch3: WaveChannel::new(),
            ch4: NoiseChannel::new(),
            mixer: Mixer::default(),
            powered: false,
            written: [0; SLOTS],
            divider: 0,
            sequencer_timer: 0,
            sequencer_step: 0,
        }
    }

    /// Whether an address belongs to the PSG block.
    ///
    /// Written as explicit ranges rather than a masked comparison, for the reason
    /// [`crate::fifo::DirectSound::owns`] gives at length: the block is not contiguous. Gaps sit
    /// at `0x66`, `0x6A`, `0x6E`, `0x7A`, and `0x7E`, and `SOUNDCNT_H` and `SOUNDCNT_X` belong to
    /// the direct-sound block *between* `SOUNDCNT_L` and the FIFOs. Any single mask over that
    /// either swallows registers this module must not answer for or misses ones it must — and
    /// both failures are silent, because an unclaimed sound register reads back zero and does
    /// nothing rather than trapping.
    pub fn owns(addr: u32) -> bool {
        (reg::SOUND1CNT_L..reg::SOUND1CNT_L + 2).contains(&addr)
            || (reg::SOUND1CNT_H..reg::SOUND1CNT_H + 2).contains(&addr)
            || (reg::SOUND1CNT_X..reg::SOUND1CNT_X + 2).contains(&addr)
            || (reg::SOUND2CNT_L..reg::SOUND2CNT_L + 2).contains(&addr)
            || (reg::SOUND2CNT_H..reg::SOUND2CNT_H + 2).contains(&addr)
            || (reg::SOUND3CNT_L..reg::SOUND3CNT_L + 2).contains(&addr)
            || (reg::SOUND3CNT_H..reg::SOUND3CNT_H + 2).contains(&addr)
            || (reg::SOUND3CNT_X..reg::SOUND3CNT_X + 2).contains(&addr)
            || (reg::SOUND4CNT_L..reg::SOUND4CNT_L + 2).contains(&addr)
            || (reg::SOUND4CNT_H..reg::SOUND4CNT_H + 2).contains(&addr)
            || (reg::SOUNDCNT_L..reg::SOUNDCNT_L + 2).contains(&addr)
            || (reg::WAVE_RAM..reg::WAVE_RAM + WAVE_RAM_WINDOW_BYTES).contains(&addr)
    }

    /// Which read-back slot an owned address falls in.
    fn slot(addr: u32) -> usize {
        ((addr - reg::BLOCK_START) / 2) as usize
    }

    pub fn is_powered(&self) -> bool {
        self.powered
    }

    /// Follow `SOUNDCNT_X` bit 7.
    ///
    /// One master enable gates the whole sound unit on hardware, and the bit lives in the
    /// direct-sound block. Clearing it does to the PSG what clearing `NR52` bit 7 does to a CGB:
    /// every register reads zero, every channel stops, and writes are discarded until it comes
    /// back. Length counters go with the rest — that is the CGB rule, and this machine follows
    /// the CGB throughout. (A DMG keeps them, which is the one place `system-gb::apu` has to
    /// choose.)
    pub fn set_power(&mut self, on: bool) {
        if on == self.powered {
            return;
        }
        self.powered = on;
        if on {
            return;
        }
        // Wave RAM survives a power cycle intact — it is sample data a game loaded, not a sound
        // register — which is why both banks are saved and put back rather than left to
        // `WaveChannel::new`'s zeroed default. `system-gb::apu::set_power` does the same for the
        // Game Boy's one bank; this machine follows the CGB rule throughout, so unlike that DMG
        // case the length counter is not carried over.
        let (wave_ram, wave_ram_bank1) = (self.ch3.wave_ram, self.ch3.wave_ram_bank1);
        self.ch1 = SquareChannel::with_sweep();
        self.ch2 = SquareChannel::new();
        self.ch3 = WaveChannel::new();
        self.ch3.wave_ram = wave_ram;
        self.ch3.wave_ram_bank1 = wave_ram_bank1;
        self.ch4 = NoiseChannel::new();
        self.mixer = Mixer::default();
        self.written = [0; SLOTS];
        self.sequencer_timer = 0;
        self.sequencer_step = 0;
    }

    /// Advance the waveforms and the frame sequencer by `cycles` of the *CPU's* clock.
    pub fn tick(&mut self, cycles: u32) {
        if !self.powered {
            return;
        }
        self.divider += cycles;
        let ticks = self.divider / CYCLES_PER_TICK;
        // The remainder is kept rather than dropped: instruction costs are not multiples of
        // four, and discarding up to three cycles each time would run the channels slow by a
        // margin that grows with how finely the machine is stepped.
        self.divider %= CYCLES_PER_TICK;
        if ticks == 0 {
            return;
        }

        self.ch1.tick(ticks);
        self.ch2.tick(ticks);
        self.ch3.tick(ticks);
        self.ch4.tick(ticks);

        self.sequencer_timer += ticks;
        while self.sequencer_timer >= SEQUENCER_PERIOD {
            self.sequencer_timer -= SEQUENCER_PERIOD;
            self.clock_sequencer();
        }
    }

    /// One 512 Hz step: lengths on the even steps, sweeps on 2 and 6, envelopes on 7.
    fn clock_sequencer(&mut self) {
        self.sequencer_step = (self.sequencer_step + 1) % 8;
        if self.sequencer_step.is_multiple_of(2) {
            self.ch1.clock_length();
            self.ch2.clock_length();
            self.ch3.clock_length();
            self.ch4.clock_length();
        }
        if self.sequencer_step == 2 || self.sequencer_step == 6 {
            self.ch1.clock_sweep();
        }
        if self.sequencer_step == 7 {
            self.ch1.clock_envelope();
            self.ch2.clock_envelope();
            self.ch4.clock_envelope();
        }
    }

    /// The PSG's stereo output, panned and scaled by its own master volume.
    ///
    /// This is *not* the whole volume chain. `SOUNDCNT_H` bits 0-1 attenuate the result again by
    /// a quarter, a half, or not at all, and the two controls cascade rather than one overriding
    /// the other — so that half is applied by the caller, where the direct-sound mix it is summed
    /// with also lives. See `GbaSystemBus::generate_samples`.
    pub fn output(&self) -> (f32, f32) {
        if !self.powered {
            return (0.0, 0.0);
        }
        let sample = self.mixer.mix([
            self.ch1.signal(),
            self.ch2.signal(),
            self.ch3.signal(),
            self.ch4.signal(),
        ]);
        (sample.left, sample.right)
    }

    /// Which bank the wave-RAM window currently exposes: always the one *not* selected for
    /// playback. See the module docs for the idiom this serves and the one case it is not
    /// verified for.
    fn wave_ram_mut(&mut self) -> &mut [u8; WAVE_RAM_BYTES] {
        if self.ch3.active_bank {
            &mut self.ch3.wave_ram
        } else {
            &mut self.ch3.wave_ram_bank1
        }
    }

    pub fn read16(&self, addr: u32) -> Option<u16> {
        if (reg::WAVE_RAM..reg::WAVE_RAM + WAVE_RAM_WINDOW_BYTES).contains(&addr) {
            // Wave RAM is sample data, not a control register: it has no read mask and is not
            // gated on `powered`, the same way `system-gb::apu` never blocks a read of `ch3.wave_ram`.
            let bank = if self.ch3.active_bank {
                &self.ch3.wave_ram
            } else {
                &self.ch3.wave_ram_bank1
            };
            let offset = (addr - reg::WAVE_RAM) as usize;
            return Some(u16::from_le_bytes([bank[offset], bank[offset + 1]]));
        }
        if !Self::owns(addr) {
            return None;
        }
        Some(self.written[Self::slot(addr)] & read_mask(addr))
    }

    pub fn write16(&mut self, addr: u32, value: u16) -> Option<()> {
        if (reg::WAVE_RAM..reg::WAVE_RAM + WAVE_RAM_WINDOW_BYTES).contains(&addr) {
            let offset = (addr - reg::WAVE_RAM) as usize;
            let [low, high] = value.to_le_bytes();
            let bank = self.wave_ram_mut();
            bank[offset] = low;
            bank[offset + 1] = high;
            return Some(());
        }
        if !Self::owns(addr) {
            return None;
        }
        // Every write is dropped while the sound unit is off, including the length fields a DMG
        // would still accept. Same divergence as [`Self::set_power`], and the same answer.
        if !self.powered {
            return Some(());
        }
        self.written[Self::slot(addr)] = value;

        match addr {
            reg::SOUND1CNT_L => {
                if let Some(sweep) = &mut self.ch1.sweep {
                    if !sweep.write_register((value & 0x7F) as u8) {
                        self.ch1.enabled = false;
                    }
                }
            }
            reg::SOUND1CNT_H => write_duty_length_envelope(&mut self.ch1, value),
            reg::SOUND1CNT_X => write_frequency_trigger(&mut self.ch1, value),
            reg::SOUND2CNT_L => write_duty_length_envelope(&mut self.ch2, value),
            reg::SOUND2CNT_H => write_frequency_trigger(&mut self.ch2, value),
            reg::SOUND3CNT_L => {
                self.ch3.sample_count = if value & (1 << 5) != 0 { 64 } else { 32 };
                self.ch3.active_bank = value & (1 << 6) != 0;
                self.ch3.dac_enabled = value & (1 << 7) != 0;
                // The wave channel has no envelope, so its DAC is its own bit rather than
                // implied by one — same as `system-gb::apu`'s `NR30` handling, and the same
                // reason: switching it off has to silence the channel immediately, not just
                // starve it of a future trigger.
                if !self.ch3.dac_enabled {
                    self.ch3.enabled = false;
                }
            }
            reg::SOUND3CNT_H => {
                self.ch3.length.write_length(value & 0xFF);
                self.ch3.volume_shift = ((value >> 13) & 0x03) as u8;
                self.ch3.force_75_percent = value & (1 << 15) != 0;
            }
            reg::SOUND3CNT_X => {
                self.ch3.frequency = value & 0x07FF;
                self.ch3.length.enabled = value & LENGTH_ENABLE != 0;
                if value & TRIGGER != 0 {
                    self.ch3.trigger();
                }
            }
            reg::SOUND4CNT_L => {
                self.ch4.length.write_length(value & 0x3F);
                self.ch4.write_envelope((value >> 8) as u8);
            }
            reg::SOUND4CNT_H => {
                self.ch4.divisor_code = (value & 0x07) as u8;
                self.ch4.short_mode = value & (1 << 3) != 0;
                self.ch4.clock_shift = ((value >> 4) & 0x0F) as u8;
                self.ch4.length.enabled = value & LENGTH_ENABLE != 0;
                if value & TRIGGER != 0 {
                    self.ch4.trigger();
                }
            }
            reg::SOUNDCNT_L => {
                self.mixer.write_nr50(value as u8);
                self.mixer.write_nr51((value >> 8) as u8);
            }
            _ => {}
        }
        Some(())
    }
}

/// `NRx4` bit 6, in its halfword position: stop the channel when the length expires.
const LENGTH_ENABLE: u16 = 1 << 14;
/// `NRx4` bit 7, in its halfword position: start the note.
const TRIGGER: u16 = 1 << 15;

/// The `SOUND1CNT_H` / `SOUND2CNT_L` shape: `NRx1` in the low byte, `NRx2` in the high byte.
fn write_duty_length_envelope(channel: &mut SquareChannel, value: u16) {
    channel.duty = ((value >> 6) & 0x03) as u8;
    channel.length.write_length(value & 0x3F);
    channel.write_envelope((value >> 8) as u8);
}

/// The `SOUND1CNT_X` / `SOUND2CNT_H` shape: eleven bits of frequency, then the two `NRx4` flags.
///
/// The Game Boy's `NRx4` quirks — a length-enable edge raised in the first half of a length
/// period clocking the counter once on its own, and a trigger reloading through that same window
/// losing a step — are **not** modelled here. They need the sequencer step at the instant of the
/// write, which this module has, but nothing in the corpus tests them on this machine and the
/// audible cost is one 256 Hz step on a note that ends on a length counter. `system-gb::apu` has
/// both, driven by Blargg's `dmg_sound` suite; if a GBA equivalent ever lands, that is where to
/// copy them from.
fn write_frequency_trigger(channel: &mut SquareChannel, value: u16) {
    channel.frequency = value & 0x07FF;
    channel.length.enabled = value & LENGTH_ENABLE != 0;
    if value & TRIGGER != 0 {
        channel.trigger();
    }
}

impl Savable for Psg {
    fn save(&self, w: &mut StateWriter) {
        self.ch1.save(w);
        self.ch2.save(w);
        self.ch3.save(w);
        self.ch4.save(w);
        self.mixer.save(w);
        w.write_bool(self.powered);
        for slot in &self.written {
            w.write_u16(*slot);
        }
        w.write_u32(self.divider);
        w.write_u32(self.sequencer_timer);
        w.write_u8(self.sequencer_step);
    }

    fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        self.ch1.load(r)?;
        self.ch2.load(r)?;
        self.ch3.load(r)?;
        self.ch4.load(r)?;
        self.mixer.load(r)?;
        self.powered = r.read_bool()?;
        for slot in &mut self.written {
            *slot = r.read_u16()?;
        }
        self.divider = r.read_u32()?;
        self.sequencer_timer = r.read_u32()?;
        // Masked rather than trusted: a save state is not a trusted input, and an out-of-range
        // step would silently stop the sequencer from ever reaching its envelope step.
        self.sequencer_step = r.read_u8()? % 8;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
