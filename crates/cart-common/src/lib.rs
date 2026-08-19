//! Cartridge headers, memory-bank controllers, battery-backed saves, and cartridge RTCs.
//!
//! Shared infrastructure: imported by the `system-*` crates, never the reverse. Nothing here
//! knows what a PPU or a CPU is.
//!
//! # The separation this crate exists to enforce
//!
//! **How a mapper banks ROM and RAM** and **how save data reaches a file on disk** are two
//! different concerns, and they are two different traits here: [`Mapper`] and
//! [`BatteryBackedSave`].
//!
//! The predecessor project conflated them — save-chip contents were extracted by the same
//! reach-into-private-fields code that handled everything else, which is why its saves were a
//! recurring corruption source. A [`Mapper`] here hands out a `&dyn BatteryBackedSave`; nobody
//! ever reaches past it. The save chip owns its own bytes and its own serialization, and can
//! be tested with no mapper in sight.
//!
//! # Save files are raw chip contents
//!
//! [`BatteryBackedSave::as_bytes`] is the exact bytes of the physical chip, with no header,
//! no container, and no project-specific framing. A `.sav` written by this emulator is
//! byte-identical to one written by any other, so saves move between tools freely. The
//! project's richer state format wraps *everything* including these bytes, but that is a
//! separate layer and does not leak down here.
//!
//! # Address spaces
//!
//! [`Mapper`] is deliberately Game Boy shaped: it takes 16-bit CPU addresses, because ROM
//! banking is a Game Boy-family concern. The GBA and DS have no ROM bank controller — their
//! cartridges are flat, and the only thing that behaves like a mapper is the save chip. So
//! those systems use [`BatteryBackedSave`] implementations (and [`GbaGpioRtc`]) directly,
//! without a [`Mapper`] in between. Forcing one shared address-space trait over both would
//! have meant a wider interface that neither side used fully.
//!
//! # DMA is a scheduled event, not an instant
//!
//! DMA controllers belong to the systems that have them (prompts 12 and 13), but the *pattern*
//! is established here because it is easy to get wrong in a way that is hard to undo.
//!
//! A DMA transfer must not be performed as one synchronous burst inside the register write
//! that starts it. On real hardware a transfer takes time, the CPU is stalled for part of it,
//! and other timed events land in the middle. Games depend on that: HBlank-triggered DMA has
//! to complete within the blanking interval, and sound-FIFO DMA has to interleave with the
//! APU's consumption of the FIFO.
//!
//! Model it as scheduler events, using the same [`Scheduler`](core_common::Scheduler) the PPU
//! and timers use:
//!
//! ```ignore
//! // Writing the enable bit does not transfer anything. It arms the channel and schedules
//! // the completion, so the transfer occupies real time on the same timeline as everything
//! // else.
//! fn write_dma_control(&mut self, channel: usize, value: u16) {
//!     self.dma[channel].control = value;
//!     if value & DMA_ENABLE == 0 {
//!         self.scheduler.cancel_matching(|e| *e == Event::DmaComplete(channel));
//!         return;
//!     }
//!     match self.dma[channel].timing() {
//!         // Immediate transfers still cost cycles; they are scheduled, not instantaneous.
//!         Timing::Immediate => {
//!             let cost = self.dma[channel].transfer_cycles();
//!             self.scheduler.schedule(self.now + cost, Event::DmaComplete(channel));
//!         }
//!         // Everything else waits for the PPU or APU event that triggers it, which is
//!         // already on the same scheduler.
//!         Timing::HBlank | Timing::VBlank | Timing::SoundFifo => self.dma[channel].armed = true,
//!     }
//! }
//! ```
//!
//! The payoff is that DMA composes with PPU and timer events for free, instead of being a
//! special case that has to be manually ordered against them.

#![deny(unsafe_code)]

mod header;
mod mbc;
mod rtc;
mod save;

pub use header::{CgbSupport, GbHeader, GbaHeader, MapperKind};
pub use mbc::{create_mapper, Mbc1, Mbc2, Mbc3, Mbc5, NoMbc};
pub use rtc::{GbaGpioRtc, Mbc3Rtc, RtcTime, RTC_TRAILER_LEN};
pub use save::{create_save, Eeprom, Flash, SaveKind, Sram};

use core_common::{CartridgeError, Savable};

/// A Game Boy cartridge's memory bank controller.
///
/// # Address space
///
/// Addresses are raw Game Boy CPU addresses, and a mapper only ever sees the two windows the
/// cartridge is wired to:
///
/// - `0x0000`–`0x7FFF`: ROM. Reads return cartridge ROM through whatever banking is active;
///   **writes are bank-switching register writes**, not memory writes. That is the whole
///   trick of a Game Boy mapper, and it is why [`Mapper::write`] exists at all for a
///   read-only medium.
/// - `0xA000`–`0xBFFF`: cartridge RAM, RTC registers, or nothing.
///
/// The system's bus is responsible for routing only those windows here.
pub trait Mapper: Savable {
    fn read(&mut self, addr: u16) -> u8;
    fn write(&mut self, addr: u16, value: u8);

    /// Read without side effects, or `None` where that is not possible.
    ///
    /// `read` takes `&mut self` because a cartridge is a device, not memory: an MBC3 RAM read can
    /// be answering the RTC rather than returning stored bytes. But the *ROM* windows of every
    /// mapper here are pure address decoding — bank number in, byte out — and a debugger that
    /// cannot disassemble ROM is not a debugger at all, which is what refusing every cartridge read
    /// left us with.
    ///
    /// So this is the narrow path: implement it for what is provably pure and answer `None` for the
    /// rest. The default refuses everything, which is the safe answer for a mapper that has not
    /// thought about it — a memory viewer showing `--` is correct, and one that advanced a state
    /// machine to avoid showing `--` would change the bug being investigated.
    fn peek(&self, _addr: u16) -> Option<u8> {
        None
    }

    /// The battery-backed save chip, if this cartridge has one.
    ///
    /// Returning a trait object rather than exposing the mapper's storage directly is the
    /// point: the frontend's save-to-disk path goes through this and cannot reach anything
    /// else the mapper owns.
    fn battery_save(&self) -> Option<&dyn BatteryBackedSave> {
        None
    }

    fn battery_save_mut(&mut self) -> Option<&mut dyn BatteryBackedSave> {
        None
    }

    /// The cartridge RTC, if present. Only MBC3 has one.
    fn rtc(&self) -> Option<&Mbc3Rtc> {
        None
    }

    fn rtc_mut(&mut self) -> Option<&mut Mbc3Rtc> {
        None
    }

    /// Advance any time-dependent cartridge hardware.
    ///
    /// Driven by emulated cycles rather than wall-clock time so that a save state replays
    /// identically and the accuracy harness stays deterministic.
    fn tick(&mut self, _cycles: u64, _cycles_per_second: u64) {}

    /// Whether the rumble motor is currently energized (MBC5 rumble cartridges).
    fn rumble(&self) -> bool {
        false
    }

    /// A short description for the UI and logs, e.g. `"MBC5 + RAM + Battery + Rumble"`.
    fn describe(&self) -> String;
}

/// A battery-backed save chip: SRAM, Flash, or EEPROM.
///
/// # Why reads take `&mut self`
///
/// Only SRAM is plain memory. Flash and EEPROM are command-driven devices with internal state
/// machines — a Flash read can be answering a "read chip ID" command rather than returning
/// stored data, and every EEPROM access advances a bit-serial protocol. Modelling the read as
/// side-effect-free would make those two impossible to implement correctly.
pub trait BatteryBackedSave: Savable {
    fn kind(&self) -> SaveKind;

    /// Size of the physical chip in bytes, which is also the length of [`Self::as_bytes`].
    fn size(&self) -> usize {
        self.as_bytes().len()
    }

    fn read_byte(&mut self, addr: u32) -> u8;
    fn write_byte(&mut self, addr: u32, value: u8);

    /// The raw chip contents, exactly as they should be written to a `.sav` file.
    fn as_bytes(&self) -> &[u8];

    /// Restore from a `.sav` file.
    ///
    /// A size mismatch is an error rather than a partial load: silently accepting a save from
    /// a differently-sized chip produces a game that looks like it loaded and then behaves
    /// strangely, which is far worse to diagnose than a refusal.
    fn load_from_bytes(&mut self, data: &[u8]) -> Result<(), CartridgeError>;

    /// Whether the game has written since the last [`Self::clear_dirty`].
    ///
    /// The frontend uses this to debounce flushing to disk, so a save becomes durable shortly
    /// after the game writes it instead of only at a clean shutdown.
    fn is_dirty(&self) -> bool;

    fn clear_dirty(&mut self);
}
