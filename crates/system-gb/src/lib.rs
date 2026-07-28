//! Game Boy (DMG) system assembly.
//!
//! Currently this crate provides the memory map (see [`memory`]), which prompt 06 establishes
//! as the reference pattern the GBC, GBA, and DS maps follow. The PPU, APU, timer, joypad,
//! and the `System` implementation that ties them together arrive with prompt 11.

#![deny(unsafe_code)]

pub mod apu;
pub mod attributes;
pub mod joypad;
pub mod memory;
pub mod ppu;
pub mod system;
pub mod timing;

pub use apu::GbApu;
pub use attributes::{background_wins, TileAttributes};
pub use joypad::Joypad;
pub use memory::{GbBus, GbModel};
pub use ppu::GbPpu;
pub use system::{GbSystem, GbSystemBus};
pub use timing::{GbEvent, GbTiming, PpuMode, TimingOutput};
