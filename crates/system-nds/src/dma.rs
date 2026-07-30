//! Four DMA channels per core, eight in the machine.
//!
//! Built on prompt 12's GBA controller — armed by a trigger, handed to the bus as a
//! [`Transfer`], priority strictly by channel number — with the DS's differences, which are more
//! than they first look:
//!
//! - **The word count is 21 bits on the ARM9**, not 14 or 16, and those extra five bits live in
//!   what the GBA calls `DMAxCNT_H`'s unused low bits. `DMAxCNT` is really one 32-bit register
//!   and treating the halves as independent loses transfers longer than 65535 units — which is
//!   most of a main-RAM clear.
//! - **The two cores have different start timings**, from different bit fields. The ARM9 reads
//!   three bits and has eight timings including the geometry FIFO; the ARM7 reads two and has
//!   four. Decoding the ARM7's field as three bits makes every one of its cartridge transfers
//!   look like a display-sync transfer that never fires.
//! - **The ARM7's maximum count still differs per channel** — channels 0-2 count 14 bits and
//!   channel 3 counts 16 — while the ARM9's is 21 on all four.
//!
//! # A transfer is described, not performed
//!
//! This module never touches memory. [`DmaController::take_transfer`] returns the highest-priority
//! ready transfer and advances the channel's bookkeeping as if it had happened; the system
//! assembly performs the copy through whichever core's view of the bus owns the channel. That is
//! what keeps this a unit with its own tests, and it is also the only arrangement that works —
//! the two cores' DMA controllers move data through *different* address spaces.
//!
//! # What is not modelled
//!
//! Transfers are performed in one go rather than interleaved with CPU execution, so a game that
//! watches a DMA progress by polling its destination sees it complete instantly. The GBA does the
//! same and nothing in the corpus has cared. Main-memory display DMA and wifi DMA are decoded and
//! then never armed, because neither the capture unit nor the wifi hardware exists — they are
//! visibly absent rather than approximated.

use crate::Core;
use core_common::{Savable, StateError, StateReader, StateWriter};

pub const CHANNELS: usize = 4;
/// Base of the channel registers, identical in both cores' I/O space.
pub const BASE: u32 = 0x0400_00B0;
/// The four `DMA_FILL` words, which exist only on the ARM9 and only as DMA source data.
pub const FILL_BASE: u32 = 0x0400_00E0;

/// What starts a transfer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartTiming {
    Immediate,
    VBlank,
    /// ARM9 only.
    HBlank,
    /// The start of a display frame. ARM9 only, and not armed: see the module docs.
    DisplayStart,
    /// Main-memory display mode. ARM9 only, and not armed.
    MainMemoryDisplay,
    /// A Slot-1 cartridge word is ready.
    CardSlot,
    /// The Slot-2 (Game Boy Advance) cartridge.
    GbaSlot,
    /// The 3D geometry command FIFO fell below half full. ARM9 only.
    GeometryFifo,
    /// ARM7 only, and never armed.
    Wifi,
}

impl StartTiming {
    /// Decode the start-timing field, which is three bits wide on the ARM9 and two on the ARM7.
    pub fn from_bits(core: Core, control: u16) -> Self {
        match core {
            Core::Arm9 => match (control >> 11) & 7 {
                0 => StartTiming::Immediate,
                1 => StartTiming::VBlank,
                2 => StartTiming::HBlank,
                3 => StartTiming::DisplayStart,
                4 => StartTiming::MainMemoryDisplay,
                5 => StartTiming::CardSlot,
                6 => StartTiming::GbaSlot,
                _ => StartTiming::GeometryFifo,
            },
            Core::Arm7 => match (control >> 12) & 3 {
                0 => StartTiming::Immediate,
                1 => StartTiming::VBlank,
                2 => StartTiming::CardSlot,
                // Channels 0 and 1 read this as the Slot-2 cartridge, 2 and 3 as wifi. The
                // channel is not known here, so the caller's `arm_for` distinguishes them.
                _ => StartTiming::GbaSlot,
            },
        }
    }
}

/// How an address moves between units of a transfer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddressStep {
    Increment,
    Decrement,
    Fixed,
    /// Increment during the transfer, then snap back to the written value for the next repeat.
    /// Destination only; the encoding is prohibited on a source and behaves as increment.
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

    /// Bytes to move per unit transferred.
    pub fn delta(self, unit: u32) -> i64 {
        match self {
            AddressStep::Increment | AddressStep::IncrementReload => unit as i64,
            AddressStep::Decrement => -(unit as i64),
            AddressStep::Fixed => 0,
        }
    }
}

fn advance(addr: u32, step: AddressStep, span: i64) -> u32 {
    match step {
        AddressStep::Increment | AddressStep::IncrementReload => addr.wrapping_add(span as u32),
        AddressStep::Decrement => addr.wrapping_sub(span as u32),
        AddressStep::Fixed => addr,
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
    pub raise_irq: bool,
}

mod control {
    pub const DEST_STEP: u16 = 0x0060;
    pub const SRC_STEP: u16 = 0x0180;
    pub const REPEAT: u16 = 1 << 9;
    pub const WORD_SIZE: u16 = 1 << 10;
    pub const IRQ: u16 = 1 << 14;
    pub const ENABLE: u16 = 1 << 15;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct Channel {
    source: u32,
    destination: u32,
    /// Up to 21 bits on the ARM9. Stored as a word because the field genuinely is one.
    words: u32,
    control: u16,

    /// The live addresses, latched on the off-to-on edge of the enable bit.
    current_source: u32,
    current_destination: u32,
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

    fn source_step(&self) -> AddressStep {
        // A source may not use the reload encoding, so it falls back to plain increment.
        match AddressStep::from_bits((self.control & control::SRC_STEP) >> 7) {
            AddressStep::IncrementReload => AddressStep::Increment,
            step => step,
        }
    }

    fn destination_step(&self) -> AddressStep {
        AddressStep::from_bits((self.control & control::DEST_STEP) >> 5)
    }

    /// Units this channel can move, with zero meaning its maximum.
    fn max_words(core: Core, index: usize) -> u32 {
        match core {
            Core::Arm9 => 0x20_0000,
            Core::Arm7 if index == 3 => 0x1_0000,
            Core::Arm7 => 0x4000,
        }
    }

    fn resolved_words(&self, core: Core, index: usize) -> u32 {
        let max = Channel::max_words(core, index);
        let requested = self.words & (max - 1);
        if requested == 0 {
            max
        } else {
            requested
        }
    }
}

/// One core's four channels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DmaController {
    core: Core,
    channels: [Channel; CHANNELS],
    /// The `DMA_FILL` words. ARM9 only; the ARM7's block reads as nothing.
    fill: [u32; CHANNELS],
}

impl DmaController {
    pub fn new(core: Core) -> Self {
        Self {
            core,
            channels: [Channel::default(); CHANNELS],
            fill: [0; CHANNELS],
        }
    }

    pub fn core(&self) -> Core {
        self.core
    }

    pub fn owns(&self, addr: u32) -> bool {
        (BASE..BASE + (CHANNELS as u32 * 12)).contains(&addr)
            || (self.core == Core::Arm9 && (FILL_BASE..FILL_BASE + 16).contains(&addr))
    }

    pub fn on_vblank(&mut self) {
        self.arm_for(StartTiming::VBlank);
    }

    /// Horizontal blanking began. ARM9 only — the ARM7 has no hblank timing, and calling this on
    /// an ARM7 controller arms nothing because none of its channels can decode to `HBlank`.
    pub fn on_hblank(&mut self) {
        self.arm_for(StartTiming::HBlank);
    }

    /// A Slot-1 cartridge transfer has a word ready.
    pub fn on_card_ready(&mut self) {
        self.arm_for(StartTiming::CardSlot);
    }

    /// The geometry command FIFO fell below half full. ARM9 only.
    pub fn on_geometry_fifo_half_empty(&mut self) {
        self.arm_for(StartTiming::GeometryFifo);
    }

    fn arm_for(&mut self, timing: StartTiming) {
        for index in 0..CHANNELS {
            // The ARM7's timing 3 means the Slot-2 cartridge on channels 0 and 1 and wifi on 2
            // and 3, from the same two bits. Nothing else in either core's decode depends on the
            // channel number, so this is the one place it has to.
            let decoded = self.timing(index);
            let decoded = match (self.core, decoded, index) {
                (Core::Arm7, StartTiming::GbaSlot, 2 | 3) => StartTiming::Wifi,
                _ => decoded,
            };
            let channel = &mut self.channels[index];
            if channel.enabled() && decoded == timing {
                channel.armed = true;
            }
        }
    }

    fn timing(&self, index: usize) -> StartTiming {
        StartTiming::from_bits(self.core, self.channels[index].control)
    }

    /// The highest-priority ready transfer, if any.
    ///
    /// Scanned from channel 0 every call rather than round-robined: DMA priority is by channel
    /// number and it is absolute.
    pub fn take_transfer(&mut self) -> Option<Transfer> {
        let index = (0..CHANNELS).find(|&i| self.channels[i].armed)?;
        let core = self.core;
        let words = self.channels[index].resolved_words(core, index);
        let unit = self.channels[index].unit();
        let timing = self.timing(index);
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

        let span = words as i64 * unit as i64;
        channel.current_source = advance(channel.current_source, channel.source_step(), span);
        channel.current_destination = match channel.destination_step() {
            AddressStep::IncrementReload => channel.destination,
            step => advance(channel.current_destination, step, span),
        };

        // A one-shot transfer clears its own enable bit, which is how a game polls for completion
        // without an interrupt. A repeating *immediate* transfer would never stop, so hardware
        // treats the repeat bit as meaningless without a trigger to repeat on.
        if !channel.repeats() || timing == StartTiming::Immediate {
            channel.control &= !control::ENABLE;
        }
        Some(transfer)
    }

    pub fn read32(&self, addr: u32) -> Option<u32> {
        if self.core == Core::Arm9 && (FILL_BASE..FILL_BASE + 16).contains(&addr) {
            return Some(self.fill[((addr - FILL_BASE) / 4) as usize]);
        }
        if !self.owns(addr) {
            return None;
        }
        let index = ((addr - BASE) / 12) as usize;
        Some(match (addr - BASE) % 12 {
            // Source and destination are write-only and read as zero.
            0 | 4 => 0,
            // The count and the control register are one word on the ARM9 and this is the honest
            // way to read it; on the ARM7 the count half reads back as written.
            _ => self.channels[index].words | ((self.channels[index].control as u32) << 16),
        })
    }

    pub fn write32(&mut self, addr: u32, value: u32) -> bool {
        if self.core == Core::Arm9 && (FILL_BASE..FILL_BASE + 16).contains(&addr) {
            self.fill[((addr - FILL_BASE) / 4) as usize] = value;
            return true;
        }
        if !self.owns(addr) {
            return false;
        }
        let index = ((addr - BASE) / 12) as usize;
        match (addr - BASE) % 12 {
            0 => self.channels[index].source = value,
            4 => self.channels[index].destination = value,
            _ => {
                self.set_count(index, value & 0xFFFF);
                self.set_control(index, (value >> 16) as u16, value);
            }
        }
        true
    }

    pub fn read16(&self, addr: u32) -> Option<u16> {
        let word = self.read32(addr & !3)?;
        Some(if addr & 2 == 0 {
            word as u16
        } else {
            (word >> 16) as u16
        })
    }

    pub fn write16(&mut self, addr: u32, value: u16) -> bool {
        if self.core == Core::Arm9 && (FILL_BASE..FILL_BASE + 16).contains(&addr) {
            let word = self.fill[((addr - FILL_BASE) / 4) as usize];
            let spliced = if addr & 2 == 0 {
                (word & 0xFFFF_0000) | value as u32
            } else {
                (word & 0xFFFF) | ((value as u32) << 16)
            };
            return self.write32(addr & !3, spliced);
        }
        if !self.owns(addr) {
            return false;
        }
        let index = ((addr - BASE) / 12) as usize;
        let channel = &mut self.channels[index];
        match (addr - BASE) % 12 {
            0 => channel.source = (channel.source & 0xFFFF_0000) | value as u32,
            2 => channel.source = (channel.source & 0xFFFF) | ((value as u32) << 16),
            4 => channel.destination = (channel.destination & 0xFFFF_0000) | value as u32,
            6 => channel.destination = (channel.destination & 0xFFFF) | ((value as u32) << 16),
            8 => self.set_count(index, value as u32),
            _ => {
                // On the ARM9 the control register's low five bits are the top of the word
                // count, so writing it must carry them across. Reconstruct the full word rather
                // than treating the halves as independent.
                let words = self.channels[index].words;
                let full = (words & 0xFFFF) | ((value as u32) << 16);
                self.set_control(index, value, full);
            }
        }
        true
    }

    /// Set the low half of the word count, preserving whatever the control half contributed.
    fn set_count(&mut self, index: usize, low: u32) {
        let channel = &mut self.channels[index];
        channel.words = (channel.words & !0xFFFF) | (low & 0xFFFF);
    }

    /// Apply a control-register write. `full` is the whole 32-bit `DMAxCNT` as it now stands, so
    /// the ARM9's high count bits can be taken from it.
    fn set_control(&mut self, index: usize, value: u16, full: u32) {
        let core = self.core;
        let channel = &mut self.channels[index];
        let was_enabled = channel.enabled();
        channel.control = value;
        channel.words = match core {
            Core::Arm9 => full & 0x1F_FFFF,
            Core::Arm7 => full & 0xFFFF,
        };

        // Latching happens on the off-to-on edge, not on every control write: a game that
        // adjusts the repeat or interrupt bit of a running transfer must not have its addresses
        // snap back to the start.
        if !was_enabled && channel.enabled() {
            channel.current_source = channel.source;
            channel.current_destination = channel.destination;
            if StartTiming::from_bits(core, value) == StartTiming::Immediate {
                channel.armed = true;
            }
        }
        if !channel.enabled() {
            channel.armed = false;
        }
    }

    pub fn read8(&self, addr: u32) -> Option<u8> {
        let word = self.read32(addr & !3)?;
        Some((word >> ((addr & 3) * 8)) as u8)
    }

    pub fn write8(&mut self, addr: u32, value: u8) -> bool {
        let Some(half) = self.read16(addr & !1) else {
            return false;
        };
        let spliced = if addr & 1 == 0 {
            (half & 0xFF00) | value as u16
        } else {
            (half & 0x00FF) | ((value as u16) << 8)
        };
        self.write16(addr & !1, spliced)
    }

    /// A `DMA_FILL` word, which is what a fill transfer reads as its source.
    pub fn fill(&self, index: usize) -> u32 {
        self.fill[index]
    }

    pub fn reset(&mut self) {
        self.channels = [Channel::default(); CHANNELS];
        self.fill = [0; CHANNELS];
    }
}

impl Savable for DmaController {
    fn save(&self, w: &mut StateWriter) {
        for channel in &self.channels {
            w.write_u32(channel.source);
            w.write_u32(channel.destination);
            w.write_u32(channel.words);
            w.write_u16(channel.control);
            w.write_u32(channel.current_source);
            w.write_u32(channel.current_destination);
            w.write_bool(channel.armed);
        }
        for word in self.fill {
            w.write_u32(word);
        }
    }

    fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        for channel in &mut self.channels {
            channel.source = r.read_u32()?;
            channel.destination = r.read_u32()?;
            channel.words = r.read_u32()?;
            channel.control = r.read_u16()?;
            channel.current_source = r.read_u32()?;
            channel.current_destination = r.read_u32()?;
            channel.armed = r.read_bool()?;
        }
        for word in &mut self.fill {
            *word = r.read_u32()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
