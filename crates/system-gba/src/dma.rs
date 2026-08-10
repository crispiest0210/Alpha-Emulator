//! The GBA's four DMA channels.
//!
//! Prompt 12 singles this out as one of the more commonly under-implemented areas, and the
//! reason is that "copy N units from A to B" is the easy part. What makes DMA correct is
//! everything around it: *when* a transfer starts, what happens to the addresses afterwards,
//! and which channel wins when two are ready at once.
//!
//! # Priority is by channel number, and it is absolute
//!
//! Channel 0 beats channel 1 beats 2 beats 3, always. A lower-numbered channel that becomes
//! ready mid-transfer does not preempt on real hardware, but between transfers the order is
//! fixed — which is why [`DmaController::take_transfer`] scans from 0 every time rather than
//! round-robining.
//!
//! # The channels are not interchangeable
//!
//! Channel 0 cannot reach the cartridge at all, channel 3 is the only one that can write to it,
//! and only channels 1 and 2 can feed a sound FIFO. Their word counters are different widths
//! too. Treating all four alike produces a machine where a game's audio DMA silently works on
//! the wrong channel — see `Channel::max_words`.
//!
//! # This decides, it does not copy
//!
//! Same split as the Game Boy's OAM and VRAM DMA in prompt 11: the copy crosses every region of
//! the memory map, so the controller yields a [`Transfer`] and the bus performs it.

use core_common::{Savable, StateError, StateReader, StateWriter};

pub const CHANNELS: usize = 4;

/// Base address of channel 0's registers. Each channel is twelve bytes further on.
pub const BASE: u32 = 0x0400_00B0;

/// What starts a transfer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartTiming {
    Immediate,
    VBlank,
    HBlank,
    /// Channel-dependent: a sound FIFO on channels 1 and 2, video capture on channel 3, and
    /// nothing at all on channel 0.
    Special,
}

impl StartTiming {
    fn from_bits(bits: u16) -> Self {
        match bits & 3 {
            0 => StartTiming::Immediate,
            1 => StartTiming::VBlank,
            2 => StartTiming::HBlank,
            _ => StartTiming::Special,
        }
    }
}

/// How an address moves after each unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddressStep {
    Increment,
    Decrement,
    Fixed,
    /// Increment during the transfer, then snap back to the start for the next repeat. Only
    /// destinations may do this; it is what lets a repeating transfer refill the same buffer.
    IncrementReload,
}

impl AddressStep {
    fn from_bits(bits: u16) -> Self {
        match bits & 3 {
            0 => AddressStep::Increment,
            1 => AddressStep::Decrement,
            2 => AddressStep::Fixed,
            _ => AddressStep::IncrementReload,
        }
    }

    fn delta(self, unit: u32) -> i64 {
        match self {
            AddressStep::Increment | AddressStep::IncrementReload => unit as i64,
            AddressStep::Decrement => -(unit as i64),
            AddressStep::Fixed => 0,
        }
    }
}

/// One block of work for the bus to perform.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Transfer {
    pub channel: usize,
    pub source: u32,
    pub destination: u32,
    /// Units to move, already resolved from the "zero means maximum" encoding.
    pub words: u32,
    /// 2 or 4 bytes.
    pub unit: u32,
    pub source_step: AddressStep,
    pub destination_step: AddressStep,
    /// Whether finishing this transfer should raise the channel's interrupt.
    pub raise_irq: bool,
}

mod control {
    pub const DEST_STEP: u16 = 0x0060;
    pub const SRC_STEP: u16 = 0x0180;
    pub const REPEAT: u16 = 1 << 9;
    pub const WORD_SIZE: u16 = 1 << 10;
    pub const TIMING: u16 = 0x3000;
    pub const IRQ: u16 = 1 << 14;
    pub const ENABLE: u16 = 1 << 15;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct Channel {
    /// As written by the game.
    source: u32,
    destination: u32,
    words: u16,
    control: u16,

    /// The live addresses, latched when the transfer was armed.
    ///
    /// Separate from the written values because a repeating transfer restarts from the latched
    /// *destination* but keeps its running *source* — reusing one pair of fields makes a
    /// sound-FIFO refill read the same bytes forever.
    current_source: u32,
    current_destination: u32,
    /// Whether this channel is armed and waiting for its trigger.
    armed: bool,
}

impl Channel {
    fn enabled(&self) -> bool {
        self.control & control::ENABLE != 0
    }

    fn repeats(&self) -> bool {
        self.control & control::REPEAT != 0
    }

    fn unit(&self) -> u32 {
        if self.control & control::WORD_SIZE != 0 {
            4
        } else {
            2
        }
    }

    fn timing(&self) -> StartTiming {
        StartTiming::from_bits((self.control & control::TIMING) >> 12)
    }

    fn source_step(&self) -> AddressStep {
        AddressStep::from_bits((self.control & control::SRC_STEP) >> 7)
    }

    fn destination_step(&self) -> AddressStep {
        AddressStep::from_bits((self.control & control::DEST_STEP) >> 5)
    }

    /// Units this channel moves, with zero meaning its maximum.
    ///
    /// The maximum differs: channels 0-2 count 14 bits, channel 3 counts 16. A game that sets a
    /// large count on channel 3 and the same count on channel 1 is not making a mistake — the
    /// channels genuinely differ.
    fn max_words(index: usize) -> u32 {
        if index == 3 {
            0x1_0000
        } else {
            0x4000
        }
    }

    fn resolved_words(&self, index: usize) -> u32 {
        let max = Self::max_words(index);
        let requested = self.words as u32 & (max - 1);
        if requested == 0 {
            max
        } else {
            requested
        }
    }
}

/// All four channels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DmaController {
    channels: [Channel; CHANNELS],
}

impl DmaController {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn owns(addr: u32) -> bool {
        (BASE..BASE + (CHANNELS as u32 * 12)).contains(&addr)
    }

    /// Note that vertical blanking began, arming any channel waiting for it.
    pub fn on_vblank(&mut self) {
        self.arm_for(StartTiming::VBlank);
    }

    pub fn on_hblank(&mut self) {
        self.arm_for(StartTiming::HBlank);
    }

    /// A sound FIFO wants more data.
    ///
    /// Only channels 1 and 2 can serve one, and only the channel whose destination is the FIFO
    /// that ran dry — which is why this takes the address rather than a channel number.
    pub fn on_fifo_empty(&mut self, fifo_address: u32) {
        for index in 1..=2 {
            let channel = &mut self.channels[index];
            if channel.enabled()
                && channel.timing() == StartTiming::Special
                && channel.destination == fifo_address
            {
                channel.armed = true;
            }
        }
    }

    fn arm_for(&mut self, timing: StartTiming) {
        for channel in &mut self.channels {
            if channel.enabled() && channel.timing() == timing {
                channel.armed = true;
            }
        }
    }

    /// Whether any channel is waiting to run.
    ///
    /// The whole point is to answer "no" without a call. The bus asks after *every instruction* and
    /// the answer is no almost always — a game arms a channel a handful of times a frame — so
    /// inlining four bool loads at the call site saves entering [`Self::take_transfer`] and
    /// building an `Option<Transfer>` millions of times a second.
    ///
    /// Derived rather than cached: `armed` is written from six places and a stale flag would drop
    /// a transfer, which is far worse than the four loads it would save.
    #[inline]
    pub fn any_armed(&self) -> bool {
        self.channels.iter().any(|channel| channel.armed)
    }

    /// The highest-priority transfer that is ready, if any.
    ///
    /// Scans from channel 0 every call rather than round-robining: priority is by channel
    /// number and it is absolute, so a fair rotation would be the wrong behaviour.
    pub fn take_transfer(&mut self) -> Option<Transfer> {
        let index = (0..CHANNELS).find(|&i| self.channels[i].armed)?;
        let words = self.channels[index].resolved_words(index);
        let unit = self.channels[index].unit();
        let channel = &mut self.channels[index];

        let transfer = Transfer {
            channel: index,
            source: channel.current_source,
            destination: channel.current_destination,
            words,
            unit,
            source_step: channel.source_step(),
            destination_step: channel.destination_step(),
            raise_irq: channel.control & control::IRQ != 0,
        };

        channel.armed = false;

        // Walk the running addresses past what this transfer covered.
        let span = words as i64 * unit as i64;
        channel.current_source = advance(channel.current_source, channel.source_step(), span);
        channel.current_destination = match channel.destination_step() {
            // The reload variant snaps back so the next repeat refills the same buffer.
            AddressStep::IncrementReload => channel.destination,
            step => advance(channel.current_destination, step, span),
        };

        if !channel.repeats() {
            // A one-shot transfer clears its own enable bit, which is how a game polls for
            // completion without an interrupt.
            channel.control &= !control::ENABLE;
        } else if channel.timing() == StartTiming::Immediate {
            // A repeating immediate transfer would never stop. Hardware treats the repeat bit
            // as meaningless without a trigger to repeat *on*.
            channel.control &= !control::ENABLE;
        }
        Some(transfer)
    }

    pub fn read16(&self, addr: u32) -> Option<u16> {
        if !Self::owns(addr) {
            return None;
        }
        let index = ((addr - BASE) / 12) as usize;
        let offset = (addr - BASE) % 12;
        Some(match offset {
            // Source and destination are write-only; reading them returns zero.
            0..=7 => 0,
            8 => 0,
            _ => self.channels[index].control,
        })
    }

    pub fn write16(&mut self, addr: u32, value: u16) -> Option<()> {
        if !Self::owns(addr) {
            return None;
        }
        let index = ((addr - BASE) / 12) as usize;
        let offset = (addr - BASE) % 12;
        let channel = &mut self.channels[index];

        match offset {
            0 => channel.source = (channel.source & 0xFFFF_0000) | value as u32,
            2 => channel.source = (channel.source & 0xFFFF) | ((value as u32) << 16),
            4 => channel.destination = (channel.destination & 0xFFFF_0000) | value as u32,
            6 => channel.destination = (channel.destination & 0xFFFF) | ((value as u32) << 16),
            8 => channel.words = value,
            _ => {
                let was_enabled = channel.enabled();
                channel.control = value;

                // Latching happens on the off-to-on edge, not on every control write. A game
                // adjusts the repeat or interrupt bit of a running transfer and must not have
                // its addresses snap back to the start.
                if !was_enabled && channel.enabled() {
                    channel.current_source = channel.source;
                    channel.current_destination = channel.destination;
                    if channel.timing() == StartTiming::Immediate {
                        channel.armed = true;
                    }
                }
                if !channel.enabled() {
                    channel.armed = false;
                }
            }
        }
        Some(())
    }

    pub fn write32(&mut self, addr: u32, value: u32) -> Option<()> {
        self.write16(addr, value as u16)?;
        self.write16(addr + 2, (value >> 16) as u16)
    }

    pub fn read32(&self, addr: u32) -> Option<u32> {
        Some((self.read16(addr)? as u32) | ((self.read16(addr + 2)? as u32) << 16))
    }
}

/// Move an address by a signed span, wrapping like the hardware counter does.
#[inline]
fn advance(addr: u32, step: AddressStep, span: i64) -> u32 {
    let delta = step.delta(1) * span;
    (addr as i64).wrapping_add(delta) as u32
}

impl Savable for DmaController {
    fn save(&self, w: &mut StateWriter) {
        for channel in &self.channels {
            w.write_u32(channel.source);
            w.write_u32(channel.destination);
            w.write_u16(channel.words);
            w.write_u16(channel.control);
            w.write_u32(channel.current_source);
            w.write_u32(channel.current_destination);
            w.write_bool(channel.armed);
        }
    }

    fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        for channel in &mut self.channels {
            channel.source = r.read_u32()?;
            channel.destination = r.read_u32()?;
            channel.words = r.read_u16()?;
            channel.control = r.read_u16()?;
            channel.current_source = r.read_u32()?;
            channel.current_destination = r.read_u32()?;
            channel.armed = r.read_bool()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
