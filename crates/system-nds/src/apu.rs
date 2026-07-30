//! The DS's sixteen-channel sound hardware.
//!
//! # Not `apu-shared`, and prompt 13 says so
//!
//! Everything else in this project descends from the Game Boy's programmable sound generator:
//! four fixed voices with envelopes and sweeps, driven by a frame sequencer. The DS is a different
//! machine. It has sixteen channels that play *sample data out of memory* at a per-channel rate,
//! in three formats, with per-channel volume and panning — and only six of the sixteen can be a
//! square wave at all, with two more as noise. Prompt 13 asks for it to be implemented here rather
//! than forced through `apu-shared`, and the shapes really do not meet: there is no envelope, no
//! sweep, no length counter, and no sequencer.
//!
//! What *would* factor out is PCM mixing, and it is four lines. Extracting four lines into a
//! shared crate to satisfy a symmetry is the abstraction-for-its-own-sake this project keeps
//! declining.
//!
//! # Channels read memory, so the APU is handed memory
//!
//! A channel is a pointer, a length, and a rate. [`NdsApu::step`] takes `&NdsMemory` and fetches
//! from it, which is why the system assembly splits its borrow of the bus rather than passing the
//! whole thing. Modelling the fetch as a DMA the CPU services was rejected: on hardware the sound
//! hardware reads memory itself, and pretending otherwise would make a channel's rate depend on
//! when the emulator happened to look at it.
//!
//! # What is approximated, and what is not
//!
//! - **Sample fetching is per output sample, not per channel clock.** A channel advances its
//!   position by a fractional step each output sample and reads the sample it lands on. That is
//!   nearest-neighbour resampling, and it is audibly a little harsher than hardware's, which
//!   fetches at the channel rate and low-pass filters on output. It is not *wrong* in the way an
//!   unmodelled behaviour would be — every channel plays the right data at the right pitch.
//! - **`SOUNDBIAS` and the output filter are not modelled**, and neither is the capture hardware.
//! - **Channels 1 and 3 cannot be routed away from the mixer.** `SOUNDCNT` bits 12 and 13 hold
//!   their written value and do nothing, because the capture unit they route to does not exist.

use crate::memory::NdsMemory;
use core_common::{AudioSample, Savable, StateError, StateReader, StateWriter, AUDIO_SAMPLE_RATE};

pub const CHANNELS: usize = 16;
/// Base of the per-channel registers, in the ARM7's I/O space.
pub const BASE: u32 = 0x0400_0400;

pub mod reg {
    pub const SOUNDCNT: u32 = 0x0400_0500;
    pub const SOUNDBIAS: u32 = 0x0400_0504;
}

/// The master clock, which is also what the channel timers divide.
const CLOCK: u32 = 33_513_982;

/// How a channel interprets the bytes it fetches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// Signed 8-bit samples, four per word.
    Pcm8,
    /// Signed 16-bit samples, two per word.
    Pcm16,
    /// IMA-ADPCM: a four-byte header then two 4-bit nibbles per byte.
    Adpcm,
    /// A square wave on channels 8-13, noise on 14-15, and silence anywhere else.
    Psg,
}

impl Format {
    fn from_bits(bits: u32) -> Self {
        match bits & 3 {
            0 => Format::Pcm8,
            1 => Format::Pcm16,
            2 => Format::Adpcm,
            _ => Format::Psg,
        }
    }
}

/// What a channel does when it reaches the end of its data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Repeat {
    /// Keep playing whatever comes next. Hardware calls this "manual"; nothing restarts it.
    Manual,
    /// Jump back to the loop point.
    Loop,
    /// Stop, clearing the busy bit so software can see it finished.
    OneShot,
}

impl Repeat {
    fn from_bits(bits: u32) -> Self {
        match bits & 3 {
            1 => Repeat::Loop,
            2 => Repeat::OneShot,
            // 0 is "manual" and 3 is prohibited; both behave as manual.
            _ => Repeat::Manual,
        }
    }
}

/// The IMA-ADPCM step table, and the index adjustment for each nibble magnitude.
///
/// Both are part of the format rather than of the DS, and getting either subtly wrong produces
/// audio that is recognisably the right sound with a rising hiss — much harder to spot than
/// silence.
const ADPCM_TABLE: [i32; 89] = [
    7, 8, 9, 10, 11, 12, 13, 14, 16, 17, 19, 21, 23, 25, 28, 31, 34, 37, 41, 45, 50, 55, 60, 66,
    73, 80, 88, 97, 107, 118, 130, 143, 157, 173, 190, 209, 230, 253, 279, 307, 337, 371, 408, 449,
    494, 544, 598, 658, 724, 796, 876, 963, 1060, 1166, 1282, 1411, 1552, 1707, 1878, 2066, 2272,
    2499, 2749, 3024, 3327, 3660, 4026, 4428, 4871, 5358, 5894, 6484, 7132, 7845, 8630, 9493,
    10442, 11487, 12635, 13899, 15289, 16818, 18500, 20350, 22385, 24623, 27086, 29794, 32767,
];
const ADPCM_INDEX_STEP: [i32; 8] = [-1, -1, -1, -1, 2, 4, 6, 8];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Channel {
    control: u32,
    source: u32,
    timer: u16,
    loop_start: u16,
    length: u32,

    /// Byte offset from `source` of the next sample to fetch.
    position: u32,
    /// Sub-sample position, as a 16.16 fixed-point count of channel samples.
    fraction: u32,
    /// The last decoded sample, held between output samples.
    current: i16,

    /// ADPCM decoder state, and the copy of it taken at the loop point.
    adpcm_value: i32,
    adpcm_index: i32,
    adpcm_loop_value: i32,
    adpcm_loop_index: i32,
    /// Whether the loop snapshot has been taken this pass.
    adpcm_loop_saved: bool,
    /// Which nibble of the current byte comes next.
    adpcm_high_nibble: bool,

    /// PSG duty position, 0-7, and the noise shift register.
    psg_phase: u8,
    noise: u16,
}

impl Default for Channel {
    fn default() -> Self {
        Self {
            control: 0,
            source: 0,
            timer: 0,
            loop_start: 0,
            length: 0,
            position: 0,
            fraction: 0,
            current: 0,
            adpcm_value: 0,
            adpcm_index: 0,
            adpcm_loop_value: 0,
            adpcm_loop_index: 0,
            adpcm_loop_saved: false,
            adpcm_high_nibble: false,
            // All ones, which is where the Game Boy's noise register starts too and is what
            // makes the first output sample noise rather than a click.
            noise: 0x7FFF,
            psg_phase: 0,
        }
    }
}

impl Channel {
    fn busy(&self) -> bool {
        self.control & (1 << 31) != 0
    }

    fn volume(&self) -> u32 {
        self.control & 0x7F
    }

    /// The volume divider, as a shift. Setting 3 is a divide by 16, not by 8 — the field is not a
    /// plain power of two and reading it as one makes every quiet channel four times too loud.
    fn volume_shift(&self) -> u32 {
        match (self.control >> 8) & 3 {
            0 => 0,
            1 => 1,
            2 => 2,
            _ => 4,
        }
    }

    fn panning(&self) -> u32 {
        (self.control >> 16) & 0x7F
    }

    fn duty(&self) -> u32 {
        (self.control >> 24) & 7
    }

    fn repeat(&self) -> Repeat {
        Repeat::from_bits(self.control >> 27)
    }

    fn format(&self) -> Format {
        Format::from_bits(self.control >> 29)
    }

    /// Channel samples per second.
    fn rate(&self) -> u32 {
        let divisor = 2 * (0x1_0000 - self.timer as u32);
        CLOCK / divisor.max(1)
    }

    /// Byte offset of the end of the data, and of the loop point.
    fn end_words(&self) -> u32 {
        self.loop_start as u32 + self.length
    }
}

/// All sixteen channels and the master mixer.
#[derive(Debug, Clone, PartialEq)]
pub struct NdsApu {
    channels: [Channel; CHANNELS],
    soundcnt: u16,
    soundbias: u16,
    /// Fractional output-sample accumulator, in master cycles.
    sample_accumulator: u32,
    output: Vec<AudioSample>,
    /// The buffer handed out by `take_samples`, swapped rather than reallocated.
    drained: Vec<AudioSample>,
}

impl Default for NdsApu {
    fn default() -> Self {
        Self::new()
    }
}

impl NdsApu {
    pub fn new() -> Self {
        Self {
            channels: [Channel::default(); CHANNELS],
            soundcnt: 0,
            soundbias: 0x200,
            sample_accumulator: 0,
            output: Vec::new(),
            drained: Vec::new(),
        }
    }

    pub fn owns(addr: u32) -> bool {
        (BASE..BASE + (CHANNELS as u32) * 16).contains(&addr)
            || matches!(addr & !3, reg::SOUNDCNT | reg::SOUNDBIAS)
    }

    /// Whether the master enable is on. The mixer is silent without it, however loud a channel is.
    fn enabled(&self) -> bool {
        self.soundcnt & (1 << 15) != 0
    }

    fn master_volume(&self) -> u32 {
        (self.soundcnt & 0x7F) as u32
    }

    /// Advance by `cycles` master cycles, producing output samples as they fall due.
    pub fn step(&mut self, cycles: u32, memory: &NdsMemory) {
        self.sample_accumulator += cycles;
        let per_sample = CLOCK / AUDIO_SAMPLE_RATE;
        while self.sample_accumulator >= per_sample {
            self.sample_accumulator -= per_sample;
            let sample = self.mix(memory);
            self.output.push(sample);
        }
    }

    /// Produce one output sample, advancing every running channel.
    fn mix(&mut self, memory: &NdsMemory) -> AudioSample {
        if !self.enabled() {
            return AudioSample::SILENCE;
        }
        let mut left = 0.0f32;
        let mut right = 0.0f32;
        for index in 0..CHANNELS {
            let Some((sample, pan)) = self.advance_channel(index, memory) else {
                continue;
            };
            // Panning runs 0 (hard left) to 127 (nearly hard right) over a denominator of
            // *128*, not 127, which is what makes the documented centre value of 64 land exactly
            // in the middle. Dividing by 127 puts every centred channel a hair to the right,
            // which is inaudible alone and becomes a real image shift across sixteen of them.
            let position = pan as f32 / 128.0;
            left += sample * (1.0 - position);
            right += sample * position;
        }
        let master = self.master_volume() as f32 / 127.0;
        // Sixteen channels at full volume would clip; the divisor keeps the mix inside the range
        // `AudioSample` documents without a limiter the frontend cannot undo.
        let scale = master / 8.0;
        AudioSample {
            left: (left * scale).clamp(-1.0, 1.0),
            right: (right * scale).clamp(-1.0, 1.0),
        }
    }

    /// Advance one channel by one output sample and return what it contributes.
    fn advance_channel(&mut self, index: usize, memory: &NdsMemory) -> Option<(f32, u32)> {
        if !self.channels[index].busy() {
            return None;
        }
        let rate = self.channels[index].rate();
        // 16.16 fixed point: how many channel samples one output sample is worth.
        let step = ((rate as u64) << 16) / AUDIO_SAMPLE_RATE as u64;
        let total = self.channels[index].fraction as u64 + step;
        let whole = (total >> 16) as u32;
        self.channels[index].fraction = (total & 0xFFFF) as u32;

        for _ in 0..whole {
            self.next_sample(index, memory);
            if !self.channels[index].busy() {
                break;
            }
        }

        let channel = &self.channels[index];
        let volume = channel.volume() as f32 / 127.0;
        let value = channel.current as f32 / 32768.0;
        Some((
            value * volume / (1u32 << channel.volume_shift()) as f32,
            channel.panning(),
        ))
    }

    /// Decode the next channel sample, handling the end of the data.
    fn next_sample(&mut self, index: usize, memory: &NdsMemory) {
        let format = self.channels[index].format();
        if format == Format::Psg {
            self.next_psg_sample(index);
            return;
        }

        // ADPCM takes its four-byte header before anything else, and does it once.
        if format == Format::Adpcm && self.channels[index].position == 0 {
            let header = self.read32(memory, index, 0);
            let channel = &mut self.channels[index];
            channel.adpcm_value = (header & 0xFFFF) as i16 as i32;
            channel.adpcm_index = ((header >> 16) & 0x7F).min(88) as i32;
            // Four *bytes*. `position` is a byte offset throughout — mixing byte and word
            // offsets here reads the first nibble out of the middle of the header.
            channel.position = 4;
            channel.adpcm_high_nibble = false;
        }

        let sample = match format {
            Format::Pcm8 => {
                let byte = self.read8(memory, index, self.channels[index].position);
                self.channels[index].position += 1;
                ((byte as i8 as i32) << 8) as i16
            }
            Format::Pcm16 => {
                let word = self.read16(memory, index, self.channels[index].position);
                self.channels[index].position += 2;
                word as i16
            }
            _ => self.next_adpcm_sample(index, memory),
        };
        self.channels[index].current = sample;
        self.check_end(index, format);
    }

    /// One IMA-ADPCM nibble.
    fn next_adpcm_sample(&mut self, index: usize, memory: &NdsMemory) -> i16 {
        let byte = self.read8(memory, index, self.channels[index].position);
        let channel = &mut self.channels[index];
        let nibble = if channel.adpcm_high_nibble {
            channel.position += 1;
            byte >> 4
        } else {
            byte & 0x0F
        };
        channel.adpcm_high_nibble = !channel.adpcm_high_nibble;

        let step = ADPCM_TABLE[channel.adpcm_index.clamp(0, 88) as usize];
        // The reconstruction every IMA decoder does, written out rather than as a loop so the
        // halving sequence is visible: step/8 plus a half, a quarter, and an eighth of it.
        let mut difference = step / 8;
        if nibble & 1 != 0 {
            difference += step / 4;
        }
        if nibble & 2 != 0 {
            difference += step / 2;
        }
        if nibble & 4 != 0 {
            difference += step;
        }
        if nibble & 8 != 0 {
            channel.adpcm_value -= difference;
        } else {
            channel.adpcm_value += difference;
        }
        channel.adpcm_value = channel.adpcm_value.clamp(-0x8000, 0x7FFF);
        channel.adpcm_index =
            (channel.adpcm_index + ADPCM_INDEX_STEP[(nibble & 7) as usize]).clamp(0, 88);
        channel.adpcm_value as i16
    }

    /// A square wave on channels 8-13, noise on 14-15, silence elsewhere.
    fn next_psg_sample(&mut self, index: usize) {
        let channel = &mut self.channels[index];
        match index {
            8..=13 => {
                channel.psg_phase = (channel.psg_phase + 1) & 7;
                // The duty field counts *low* eighths: 0 is a 12.5% duty cycle that is high for
                // one eighth, and 7 is silence rather than a full-on wave.
                let high = channel.psg_phase as u32 > channel.duty();
                channel.current = if high { 0x7FFF } else { -0x8000 };
            }
            14 | 15 => {
                let feedback = (channel.noise ^ (channel.noise >> 1)) & 1;
                channel.noise = (channel.noise >> 1) | (feedback << 14);
                channel.current = if channel.noise & 1 != 0 {
                    0x7FFF
                } else {
                    -0x8000
                };
            }
            _ => channel.current = 0,
        }
    }

    /// Loop, stop, or carry on, having just consumed a sample.
    fn check_end(&mut self, index: usize, format: Format) {
        let channel = &mut self.channels[index];
        let words = channel.position.div_ceil(4);
        if words < channel.end_words() {
            // Snapshot the ADPCM decoder as the loop point goes past, since a loop has to resume
            // from the decoder state at that point rather than from the start of the data.
            if format == Format::Adpcm
                && !channel.adpcm_loop_saved
                && words >= channel.loop_start as u32
            {
                channel.adpcm_loop_value = channel.adpcm_value;
                channel.adpcm_loop_index = channel.adpcm_index;
                channel.adpcm_loop_saved = true;
            }
            return;
        }
        match channel.repeat() {
            Repeat::Loop => {
                channel.position = channel.loop_start as u32 * 4;
                channel.adpcm_high_nibble = false;
                if format == Format::Adpcm {
                    channel.adpcm_value = channel.adpcm_loop_value;
                    channel.adpcm_index = channel.adpcm_loop_index;
                }
            }
            Repeat::OneShot => {
                // Clearing the busy bit is how software sees a sound finish.
                channel.control &= !(1 << 31);
                channel.current = 0;
            }
            Repeat::Manual => {}
        }
    }

    fn read8(&self, memory: &NdsMemory, index: usize, offset: u32) -> u8 {
        let addr = self.channels[index].source.wrapping_add(offset);
        memory.read8_arm7(addr).unwrap_or(0)
    }

    fn read16(&self, memory: &NdsMemory, index: usize, offset: u32) -> u16 {
        let addr = self.channels[index].source.wrapping_add(offset) & !1;
        memory.read_wide_arm7(addr, 2).unwrap_or(0) as u16
    }

    fn read32(&self, memory: &NdsMemory, index: usize, word: u32) -> u32 {
        let addr = self.channels[index].source.wrapping_add(word * 4) & !3;
        memory.read_wide_arm7(addr, 4).unwrap_or(0)
    }

    /// Take the samples produced since the last call.
    pub fn take_samples(&mut self) -> &[AudioSample] {
        std::mem::swap(&mut self.output, &mut self.drained);
        self.output.clear();
        &self.drained
    }

    pub fn read32_reg(&self, addr: u32) -> Option<u32> {
        if let Some((index, offset)) = Self::decode(addr) {
            return Some(match offset {
                0 => self.channels[index].control,
                // Source, timer, loop point, and length are write-only and read as zero.
                _ => 0,
            });
        }
        match addr & !3 {
            reg::SOUNDCNT => Some(self.soundcnt as u32),
            reg::SOUNDBIAS => Some(self.soundbias as u32),
            _ => None,
        }
    }

    pub fn write32_reg(&mut self, addr: u32, value: u32) -> bool {
        if let Some((index, offset)) = Self::decode(addr) {
            match offset {
                0 => self.set_control(index, value),
                4 => self.channels[index].source = value & 0x07FF_FFFF,
                8 => {
                    self.channels[index].timer = value as u16;
                    self.channels[index].loop_start = (value >> 16) as u16;
                }
                _ => self.channels[index].length = value & 0x003F_FFFF,
            }
            return true;
        }
        match addr & !3 {
            reg::SOUNDCNT => self.soundcnt = value as u16,
            reg::SOUNDBIAS => self.soundbias = value as u16 & 0x03FF,
            _ => return false,
        }
        true
    }

    /// Write a channel's control register, restarting it on the rising edge of the busy bit.
    fn set_control(&mut self, index: usize, value: u32) {
        let was_busy = self.channels[index].busy();
        let channel = &mut self.channels[index];
        channel.control = value;
        if !was_busy && channel.busy() {
            channel.position = 0;
            channel.fraction = 0;
            channel.current = 0;
            channel.adpcm_high_nibble = false;
            channel.adpcm_loop_saved = false;
            channel.psg_phase = 0;
            channel.noise = 0x7FFF;
        }
    }

    pub fn read16_reg(&self, addr: u32) -> Option<u16> {
        let word = self.read32_reg(addr & !3)?;
        Some(if addr & 2 == 0 {
            word as u16
        } else {
            (word >> 16) as u16
        })
    }

    pub fn write16_reg(&mut self, addr: u32, value: u16) -> bool {
        let Some(current) = self.read32_reg(addr & !3) else {
            return false;
        };
        // The write-only channel registers read as zero, so a halfword write to one has to splice
        // into what was *written*, not what reads back.
        let current = match Self::decode(addr & !3) {
            Some((index, 4)) => self.channels[index].source,
            Some((index, 8)) => {
                self.channels[index].timer as u32 | ((self.channels[index].loop_start as u32) << 16)
            }
            Some((index, 12)) => self.channels[index].length,
            _ => current,
        };
        let spliced = if addr & 2 == 0 {
            (current & 0xFFFF_0000) | value as u32
        } else {
            (current & 0xFFFF) | ((value as u32) << 16)
        };
        self.write32_reg(addr & !3, spliced)
    }

    pub fn read8_reg(&self, addr: u32) -> Option<u8> {
        let word = self.read32_reg(addr & !3)?;
        Some((word >> ((addr & 3) * 8)) as u8)
    }

    pub fn write8_reg(&mut self, addr: u32, value: u8) -> bool {
        let Some(half) = self.read16_reg(addr & !1) else {
            return false;
        };
        let spliced = if addr & 1 == 0 {
            (half & 0xFF00) | value as u16
        } else {
            (half & 0x00FF) | ((value as u16) << 8)
        };
        self.write16_reg(addr & !1, spliced)
    }

    fn decode(addr: u32) -> Option<(usize, u32)> {
        if !(BASE..BASE + (CHANNELS as u32) * 16).contains(&addr) {
            return None;
        }
        let offset = addr - BASE;
        Some(((offset / 16) as usize, offset % 16))
    }

    /// A channel's live state, for the debugger and for tests.
    pub fn channel_is_busy(&self, index: usize) -> bool {
        self.channels[index].busy()
    }

    pub fn reset(&mut self) {
        let output = std::mem::take(&mut self.output);
        let drained = std::mem::take(&mut self.drained);
        *self = Self::new();
        self.output = output;
        self.drained = drained;
        self.output.clear();
        self.drained.clear();
    }
}

impl Savable for NdsApu {
    fn save(&self, w: &mut StateWriter) {
        for channel in &self.channels {
            w.write_u32(channel.control);
            w.write_u32(channel.source);
            w.write_u16(channel.timer);
            w.write_u16(channel.loop_start);
            w.write_u32(channel.length);
            w.write_u32(channel.position);
            w.write_u32(channel.fraction);
            w.write_i16(channel.current);
            w.write_i32(channel.adpcm_value);
            w.write_i32(channel.adpcm_index);
            w.write_i32(channel.adpcm_loop_value);
            w.write_i32(channel.adpcm_loop_index);
            w.write_bool(channel.adpcm_loop_saved);
            w.write_bool(channel.adpcm_high_nibble);
            w.write_u8(channel.psg_phase);
            w.write_u16(channel.noise);
        }
        w.write_u16(self.soundcnt);
        w.write_u16(self.soundbias);
        w.write_u32(self.sample_accumulator);
        // The output buffer is not saved: it is what has not been handed to the frontend yet, and
        // a state that restored it would play a fraction of a second twice.
    }

    fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        for channel in &mut self.channels {
            channel.control = r.read_u32()?;
            channel.source = r.read_u32()?;
            channel.timer = r.read_u16()?;
            channel.loop_start = r.read_u16()?;
            channel.length = r.read_u32()?;
            channel.position = r.read_u32()?;
            channel.fraction = r.read_u32()?;
            channel.current = r.read_i16()?;
            channel.adpcm_value = r.read_i32()?;
            channel.adpcm_index = r.read_i32()?;
            channel.adpcm_loop_value = r.read_i32()?;
            channel.adpcm_loop_index = r.read_i32()?;
            channel.adpcm_loop_saved = r.read_bool()?;
            channel.adpcm_high_nibble = r.read_bool()?;
            channel.psg_phase = r.read_u8()?;
            channel.noise = r.read_u16()?;
        }
        self.soundcnt = r.read_u16()?;
        self.soundbias = r.read_u16()?;
        self.sample_accumulator = r.read_u32()?;
        self.output.clear();
        Ok(())
    }
}

#[cfg(test)]
mod tests;
