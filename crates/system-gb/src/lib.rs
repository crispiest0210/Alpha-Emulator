//! Game Boy (DMG) system assembly.
//!
//! Currently this crate provides the memory map (see [`memory`]), which prompt 06 establishes
//! as the reference pattern the GBC, GBA, and DS maps follow. The PPU, APU, timer, joypad,
//! and the `System` implementation that ties them together arrive with prompt 11.

#![deny(unsafe_code)]

pub mod memory;

pub use memory::{GbBus, GbModel};
