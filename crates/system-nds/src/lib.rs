//! Nintendo DS system assembly.
//!
//! # Status
//!
//! **Partial, and deliberately so.** Prompt 13 scopes this as the start of DS support rather
//! than its completion; what is here is built the way the GBA was, as tested units assembled
//! last. See `README.md` for the authoritative status table.
//!
//! Implemented so far: the dual-CPU memory map ([`memory`]) and VRAM bank mapping ([`vram`]).
//!
//! Not implemented yet: the two 2D engines, the 3D core, the audio hardware,
//! DMA, timers, IPC, the cartridge, and the [`core_common::System`] implementation itself.

#![deny(unsafe_code)]

pub mod memory;
pub mod vram;

pub use memory::{Arm7Region, Arm9Region, NdsMemory, WramSplit};
pub use vram::{Vram, VramSpace};
