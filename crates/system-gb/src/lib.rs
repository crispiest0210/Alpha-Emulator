//! Game Boy system assembly.
//!
//! [`GbSystem`] is the complete machine: SM83 core, memory map, scanline-accurate PPU, four
//! -channel APU, timer, joypad, and cartridge, driven by the event scheduler rather than by
//! fixed-step polling. The memory map (see [`memory`]) is the reference pattern prompt 06
//! establishes for the GBC, GBA, and DS maps to follow.
//!
//! # Not DMG-only
//!
//! The crate name says `gb` and the parts inside answer to [`GbModel`], which covers the DMG,
//! the CGB, and a CGB running a DMG cartridge. The Game Boy Color is not a different machine
//! so much as the same one with more banks, a second palette path, and a faster clock — so
//! `system-gbc` supplies the register blocks that are genuinely new hardware and parameterises
//! these components rather than forking them. Concretely, [`ppu::GbPpu::render_scanline_with`]
//! takes the palette source and the model, so one renderer produces both pictures.
//!
//! What is *not* here is anything a DMG has no concept of: colour palette RAM, the `KEY1`
//! speed switch, and VRAM DMA all live in `system-gbc`.

#![deny(unsafe_code)]

pub mod apu;
pub mod attributes;
pub mod joypad;
pub mod memory;
pub mod ppu;
pub mod system;
pub mod timing;

pub use apu::GbApu;
pub use attributes::TileAttributes;
pub use joypad::Joypad;
pub use memory::{GbBus, GbModel};
pub use ppu::GbPpu;
pub use system::{GbSystem, GbSystemBus};
pub use timing::{GbEvent, GbTiming, PpuMode, TimingOutput};
