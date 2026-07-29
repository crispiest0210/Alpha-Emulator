//! Game Boy Advance system assembly.
//!
//! Follows prompt 11's proven pattern, adapted for a much larger machine, and built up in the
//! same order: the memory map first, then the register blocks that sit on it, then the
//! assembly that drives them.
//!
//! # Status
//!
//! Three subsystems are complete and tested; nothing is assembled yet, so this crate does not
//! run a ROM.
//!
//! | Done | Not started |
//! |---|---|
//! | [`memory`] — regions, mirroring, open bus, the 8-bit write quirk | Tile modes 0-2, including affine backgrounds |
//! | [`irq`] — `IE`/`IF`/`IME`, acknowledge-by-writing-ones | Sprites, windows, and colour blending |
//! | [`timers`] — four channels, prescalers, cascade | APU: four PSG channels plus two DMA-fed FIFOs |
//! | [`dma`] — four channels, all trigger modes, priority | Wait-state timing (`WAITCNT`) |
//! | [`video`] — scanline machine, `DISPCNT`/`DISPSTAT`/`VCOUNT` | Cartridge wiring and the `System` impl |
//! | [`bitmap`] — modes 3, 4, and 5 | |
//!
//! The GBA is the system the *predecessor* project targeted, so prompt 12 sets the bar at "at
//! least as correct and complete as the vendored core it replaces, with the test coverage that
//! core never had". Nothing here has been run against `gba-suite` or `arm7wrestler` yet — the
//! ARM core they exercise has never been run against them either, and it will be worth doing
//! both together once there is a machine to run them on.

#![deny(unsafe_code)]

pub mod bitmap;
pub mod dma;
pub mod irq;
pub mod memory;
pub mod timers;
pub mod video;

pub use bitmap::bgr555_to_rgba8;
pub use dma::DmaController;
pub use irq::InterruptController;
pub use memory::{GbaBus, Region};
pub use timers::Timers;
pub use video::VideoTiming;
