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
//! The CGB-only register blocks — colour palette RAM, the `KEY1` speed switch, VRAM DMA — are
//! in [`cgb`], which explains why they are here rather than in `system-gbc`. What is left to
//! that crate is the assembled machine and the boot path that recolours a DMG cartridge.
//!
//! # What "scanline-accurate" means here, and where it stops
//!
//! Each line is *timed* to the cycle: mode 3's length is computed from the fine scroll, the
//! window, and the line's objects ([`ppu::GbPpu::mode3_cycles`]), and mode 0 is whatever is left
//! of the 456 — so a game that writes registers from an `HBlank` interrupt lands where hardware
//! puts it. Mooneye's PPU acceptance tests are what hold that down.
//!
//! Each line is *drawn* once, when mode 3 ends, from the registers as they stand at that moment.
//! So a register rewritten **partway along a line** applies to all of it or none of it, where
//! hardware would split the line. Getting that too means a per-dot fetcher and a pixel FIFO,
//! which is a different renderer, not a refinement of this one. `mealybug-tearoom-tests` is the
//! suite that measures exactly that gap, and adding it to the corpus is open work.
//!
//! # The CPU cannot reach all of memory all of the time
//!
//! Two mechanisms take memory away from the CPU, and both are modelled. The PPU has priority
//! over the memory it is reading, so VRAM is unreachable during mode 3 and OAM during modes 2
//! and 3, as is CGB palette RAM's data port during mode 3; a read that is locked out returns
//! `0xFF` and a write is dropped. And [`oam_dma`] is a transfer that takes 160 machine cycles
//! rather than an instant copy, holding one of the two memory buses for the duration — which
//! is what the HRAM wait loop in every commercial game is waiting *for*.
//!
//! Both rules live on [`GbSystemBus`] rather than in [`memory`], because the memory map does
//! not know what the PPU is doing and a copy of the mode kept next to it would be a second
//! source of truth for a value that changes every few hundred cycles.
//!
//! Mooneye's four `oam_dma` acceptance ROMs pass, and its `ppu` set is what the mode-3 length
//! above is measured against. The corpus records, per ROM, which of them still do not pass and
//! why — the remaining ones need the LCD-on frame's first line modelled and `LY`/`LYC`
//! compared while the LCD is off, neither of which the mode-3 length reaches.

#![deny(unsafe_code)]

pub mod apu;
pub mod cgb;
pub mod debug;
pub mod joypad;
pub mod memory;
pub mod oam_dma;
pub mod ppu;
pub mod system;
pub mod timing;

pub use apu::GbApu;
pub use cgb::{CgbPalettes, CgbState, GbcCompatibilityShades, Hdma, SpeedSwitch, TileAttributes};
pub use joypad::Joypad;
pub use memory::{GbBus, GbModel};
pub use oam_dma::OamDma;
pub use ppu::GbPpu;
pub use system::{GbSystem, GbSystemBus};
pub use timing::{GbEvent, GbTiming, PpuMode, TimingOutput};
