//! The GBA's two direct-sound channels.
//!
//! # Not a synthesiser
//!
//! The four channels in `apu-shared` generate a waveform from a description. These two do not:
//! they are byte queues that a game fills with recorded audio through DMA and drains one sample
//! at a time on a timer overflow. That is the whole mechanism, and it is why they live here
//! rather than in the shared crate — nothing about them is shared with the Game Boy.
//!
//! # The refill request is the interesting part
//!
//! A queue holds 32 bytes. When it drops to half, the channel asks its DMA channel for more,
//! and the DMA channel writes sixteen bytes back in one burst. Miss that request and the queue
//! runs dry; on hardware that is an audible click rather than silence, because the channel holds
//! its last sample rather than dropping to zero. Both behaviours are modelled: see
//! [`SoundFifo::pop_sample`].
//!
//! # A timer paces it, and which timer is the game's choice
//!
//! The sample rate is not a property of the channel. It is however often the selected timer
//! overflows, which is how a game plays 16 kHz audio on one channel and 32 kHz on the other.

use core_common::{Savable, StateError, StateReader, StateWriter};

use crate::timers::Overflows;

/// Bytes a queue holds.
pub const CAPACITY: usize = 32;
/// Falling to this many bytes is what triggers a refill request.
pub const REFILL_THRESHOLD: usize = CAPACITY / 2;

/// Register addresses.
pub mod reg {
    /// PSG and direct-sound mixing.
    pub const SOUNDCNT_H: u32 = 0x0400_0082;
    /// Master enable.
    pub const SOUNDCNT_X: u32 = 0x0400_0084;
    pub const FIFO_A: u32 = 0x0400_00A0;
    pub const FIFO_B: u32 = 0x0400_00A4;
}

/// One direct-sound channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SoundFifo {
    queue: [i8; CAPACITY],
    read: usize,
    write: usize,
    len: usize,
    /// The sample currently being output, held between timer overflows.
    current: i8,
}

impl Default for SoundFifo {
    fn default() -> Self {
        Self::new()
    }
}

impl SoundFifo {
    pub fn new() -> Self {
        Self {
            queue: [0; CAPACITY],
            read: 0,
            write: 0,
            len: 0,
            current: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Whether the queue has fallen far enough to need refilling.
    pub fn needs_refill(&self) -> bool {
        self.len <= REFILL_THRESHOLD
    }

    /// Empty the queue, as writing the reset bit in `SOUNDCNT_H` does.
    ///
    /// The held sample is cleared too: a reset is a game changing what it is playing, and
    /// carrying the last byte of the previous sound across is an audible pop.
    pub fn reset(&mut self) {
        *self = Self::new();
    }

    /// Push one byte. Bytes beyond capacity are dropped rather than overwriting unplayed ones.
    ///
    /// Dropping the newest is the right way round here, unlike the input channel in
    /// `frontend-core`: these bytes are a *sequence*, and discarding an old one would skip
    /// forward in the audio rather than merely delaying it.
    pub fn push(&mut self, byte: i8) {
        if self.len == CAPACITY {
            return;
        }
        self.queue[self.write] = byte;
        self.write = (self.write + 1) % CAPACITY;
        self.len += 1;
    }

    /// Push a 32-bit word, which is how a game and a DMA channel both write to a FIFO.
    ///
    /// Little-endian: the low byte is the earliest sample.
    pub fn push_word(&mut self, value: u32) {
        for shift in [0, 8, 16, 24] {
            self.push((value >> shift) as u8 as i8);
        }
    }

    /// Advance to the next sample, as a timer overflow does.
    ///
    /// An empty queue holds the previous sample rather than returning silence. That is what
    /// hardware does, and it is why an underrun sounds like a click or a buzz rather than a
    /// gap — modelling it as zero would make a starved channel *quieter* than a working one,
    /// which is the opposite of the symptom a game developer would be listening for.
    pub fn pop_sample(&mut self) -> i8 {
        if self.len > 0 {
            self.current = self.queue[self.read];
            self.read = (self.read + 1) % CAPACITY;
            self.len -= 1;
        }
        self.current
    }

    /// The sample currently being output, without advancing.
    pub fn current_sample(&self) -> i8 {
        self.current
    }
}

/// Which timer paces a channel, and how loudly it plays.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DirectSoundControl {
    /// Raw `SOUNDCNT_H`.
    pub control: u16,
    /// Raw `SOUNDCNT_X`; only bit 7 is writable.
    pub master: u16,
}

impl DirectSoundControl {
    /// Volume the four PSG channels are mixed at, as a numerator over four.
    ///
    /// The fourth setting is prohibited and reads back as it was written; it is treated as
    /// silent rather than as full volume, so a game that lands there by accident does not get
    /// a burst of noise.
    pub fn psg_volume(&self) -> u32 {
        match self.control & 3 {
            0 => 1,
            1 => 2,
            2 => 4,
            _ => 0,
        }
    }

    /// Whether channel A (or B) plays at full volume rather than half.
    pub fn direct_full_volume(&self, channel_b: bool) -> bool {
        let bit = if channel_b { 1 << 3 } else { 1 << 2 };
        self.control & bit != 0
    }

    pub fn enabled_right(&self, channel_b: bool) -> bool {
        let bit = if channel_b { 1 << 12 } else { 1 << 8 };
        self.control & bit != 0
    }

    pub fn enabled_left(&self, channel_b: bool) -> bool {
        let bit = if channel_b { 1 << 13 } else { 1 << 9 };
        self.control & bit != 0
    }

    /// Which timer overflow advances this channel: 0 or 1.
    pub fn timer(&self, channel_b: bool) -> usize {
        let bit = if channel_b { 1 << 14 } else { 1 << 10 };
        usize::from(self.control & bit != 0)
    }

    /// Whether this write asked to reset the channel's queue.
    ///
    /// The bit is a strobe, not a state: it triggers on the write and never reads back set, so
    /// it is answered from the value being written rather than from stored state.
    pub fn reset_requested(value: u16, channel_b: bool) -> bool {
        let bit = if channel_b { 1 << 15 } else { 1 << 11 };
        value & bit != 0
    }

    /// The master enable. With it clear the whole sound unit is off and its registers are
    /// read-only zero on hardware.
    pub fn sound_enabled(&self) -> bool {
        self.master & (1 << 7) != 0
    }
}

/// Both channels and their shared control registers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DirectSound {
    pub a: SoundFifo,
    pub b: SoundFifo,
    pub control: DirectSoundControl,
}

impl DirectSound {
    pub fn new() -> Self {
        Self {
            a: SoundFifo::new(),
            b: SoundFifo::new(),
            control: DirectSoundControl::default(),
        }
    }

    /// Written as explicit ranges rather than a masked comparison.
    ///
    /// These registers are not aligned to a common boundary — `SOUNDCNT_H` is a halfword at an
    /// address ending in 2 — so any single mask groups some of them with registers belonging to
    /// the PSG block next door and misses others entirely.
    pub fn owns(addr: u32) -> bool {
        (reg::SOUNDCNT_H..reg::SOUNDCNT_H + 2).contains(&addr)
            || (reg::SOUNDCNT_X..reg::SOUNDCNT_X + 4).contains(&addr)
            || (reg::FIFO_A..reg::FIFO_B + 4).contains(&addr)
    }

    pub fn write16(&mut self, addr: u32, value: u16) -> Option<()> {
        match addr {
            reg::SOUNDCNT_H => {
                // The reset bits are strobes: act on them, then keep them clear.
                if DirectSoundControl::reset_requested(value, false) {
                    self.a.reset();
                }
                if DirectSoundControl::reset_requested(value, true) {
                    self.b.reset();
                }
                self.control.control = value & !(1 << 11) & !(1 << 15);
            }
            reg::SOUNDCNT_X => self.control.master = value & (1 << 7),
            // The two FIFOs themselves. `owns` has always claimed these addresses and this match
            // never handled them, so every byte a DMA channel delivered was accepted by the bus
            // and then dropped: the queues stayed empty, the held sample stayed zero, and the
            // machine produced digital silence for every game that uses direct sound — which is
            // every commercial game. Nothing failed, because nothing tested that a sample written
            // to a FIFO can be read back out of it.
            //
            // A halfword rather than a word because the bus splits every 32-bit access in two, and
            // the FIFO is a byte queue underneath: low byte first, which is the order a little-
            // endian word delivers its samples in.
            _ if (reg::FIFO_A..reg::FIFO_A + 4).contains(&addr) => {
                self.a.push(value as u8 as i8);
                self.a.push((value >> 8) as u8 as i8);
            }
            _ if (reg::FIFO_B..reg::FIFO_B + 4).contains(&addr) => {
                self.b.push(value as u8 as i8);
                self.b.push((value >> 8) as u8 as i8);
            }
            _ => return None,
        }
        Some(())
    }

    pub fn read16(&self, addr: u32) -> Option<u16> {
        Some(match addr {
            reg::SOUNDCNT_H => self.control.control,
            reg::SOUNDCNT_X => self.control.master,
            // The queues are write-only. Reading one returns nothing rather than a sample: a
            // game has no way to inspect how much audio is left.
            reg::FIFO_A | reg::FIFO_B => 0,
            _ => return None,
        })
    }

    pub fn write32(&mut self, addr: u32, value: u32) -> Option<()> {
        match addr {
            reg::FIFO_A => self.a.push_word(value),
            reg::FIFO_B => self.b.push_word(value),
            _ => {
                self.write16(addr, value as u16)?;
                self.write16(addr + 2, (value >> 16) as u16)?;
            }
        }
        Some(())
    }

    /// Advance whichever channels the given timers pace, once per overflow.
    ///
    /// `overflowed` is what [`crate::Timers::tick`] returns. Both channels can be paced by the
    /// same timer, which is how a game plays stereo from one clock.
    ///
    /// # Once per overflow, not once per call
    ///
    /// The count is the whole point of [`Overflows`] carrying one. A `tick` covering a long DMA
    /// burst can overflow a sound timer dozens of times, and a channel that pops a single sample
    /// for all of them plays at a fraction of its rate — and, worse, never drains far enough to ask
    /// for a refill, so the queue stops being a queue.
    pub fn on_timer_overflow(&mut self, overflowed: &Overflows) {
        let a = overflowed.count(self.control.timer(false));
        let b = overflowed.count(self.control.timer(true));
        for _ in 0..a {
            self.a.pop_sample();
        }
        for _ in 0..b {
            self.b.pop_sample();
        }
    }

    /// Which FIFO addresses need a DMA refill right now.
    ///
    /// Returned as addresses rather than channel numbers because that is what
    /// [`crate::DmaController::on_fifo_empty`] matches on — a DMA channel is bound to a FIFO by
    /// its destination address, not by an index.
    pub fn refill_requests(&self) -> impl Iterator<Item = u32> + use<> {
        let a = self.a.needs_refill().then_some(reg::FIFO_A);
        let b = self.b.needs_refill().then_some(reg::FIFO_B);
        a.into_iter().chain(b)
    }

    /// The two channels' current output, scaled by their volume settings.
    ///
    /// Returned as `(left, right)` in the range -1.0 to 1.0. A channel not enabled on a side
    /// contributes nothing there, which is how a game pans one.
    pub fn output(&self) -> (f32, f32) {
        if !self.control.sound_enabled() {
            return (0.0, 0.0);
        }
        let mut left = 0.0;
        let mut right = 0.0;
        for (fifo, is_b) in [(&self.a, false), (&self.b, true)] {
            let scale = if self.control.direct_full_volume(is_b) {
                1.0
            } else {
                0.5
            };
            let sample = fifo.current_sample() as f32 / 128.0 * scale;
            if self.control.enabled_left(is_b) {
                left += sample;
            }
            if self.control.enabled_right(is_b) {
                right += sample;
            }
        }
        (left.clamp(-1.0, 1.0), right.clamp(-1.0, 1.0))
    }
}

impl Savable for SoundFifo {
    fn save(&self, w: &mut StateWriter) {
        for byte in &self.queue {
            w.write_u8(*byte as u8);
        }
        w.write_u32(self.read as u32);
        w.write_u32(self.write as u32);
        w.write_u32(self.len as u32);
        w.write_u8(self.current as u8);
    }

    fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        for byte in &mut self.queue {
            *byte = r.read_u8()? as i8;
        }
        self.read = r.read_u32()? as usize % CAPACITY;
        self.write = r.read_u32()? as usize % CAPACITY;
        // Clamped rather than trusted: a corrupt length would index past the queue on the very
        // next pop, and a save state is not a trusted input.
        self.len = (r.read_u32()? as usize).min(CAPACITY);
        self.current = r.read_u8()? as i8;
        Ok(())
    }
}

impl Savable for DirectSound {
    fn save(&self, w: &mut StateWriter) {
        self.a.save(w);
        self.b.save(w);
        w.write_u16(self.control.control);
        w.write_u16(self.control.master);
    }

    fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        self.a.load(r)?;
        self.b.load(r)?;
        self.control.control = r.read_u16()?;
        self.control.master = r.read_u16()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
