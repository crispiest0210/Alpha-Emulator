//! Game Boy Advance system assembly.
//!
//! Follows prompt 11's proven pattern, adapted for a much larger machine, and built up in the
//! same order: the memory map first, then the register blocks that sit on it, then the
//! assembly that drives them.
//!
//! # Status
//!
//! **The machine runs.** A cartridge boots, the display renders through the compositor, DMA and
//! timers drive the sound FIFOs, and interrupts reach the game's handler with or without a BIOS.
//! A save state round-trips frame-exactly and two runs of the same ROM are identical.
//!
//! What is not done, in rough order of how much it matters:
//!
//! - **All three `gba-suite` ROMs pass**: the instruction set in both states, and memory.
//! - Affine *sprites* are decoded and transformed but not composited; they show nothing rather
//!   than an untransformed approximation. Affine backgrounds are drawn.
//! - Wait states are computed by [`waitstates`] but not yet charged to the CPU, so every access
//!   currently costs what the ARM core says it does.
//! - The four `apu-shared` PSG channels are not mixed alongside the two FIFO channels, and
//!   doing it needs a decision first. Their *register* layer — `NR10`-`NR52` decode, read masks,
//!   power-down semantics — is shared by the Game Boy, the Game Boy Color, and the GBA, but it
//!   lives in `system-gb::apu` and this crate cannot reach it: `system-*` crates may not depend
//!   on each other. Duplicating it is exactly the copy-paste this project avoids, so it wants
//!   moving into `apu-shared` — and the obstacle is that three of its behaviours are gated on
//!   `GbModel`, which would have to move too or be replaced by something narrower.
//! - Colour blending applies, but an alpha blend uses the backdrop as what lies underneath: the
//!   scanline buffer keeps only the winning pixel, so the layer below it is not available.
//!   That covers the common case of a layer blended over the background colour and nothing
//!   more. The object window is likewise reported as never covering, since sprites drawn into
//!   it are not yet distinguished. Mosaic is not implemented at all.
//! - EEPROM saves are reported as absent rather than emulated; SRAM and Flash work.
//! - The HLE BIOS in [`bios`] answers the calls games actually make; the rest change nothing
//!   rather than guessing, which shows up in a trace instead of surfacing far from its cause.
//!
//! The GBA is the system the *predecessor* project targeted, so prompt 12 sets the bar at "at
//! least as correct and complete as the vendored core it replaces, with the test coverage that
//! core never had". The second half of that is met; the first is unmeasured until the accuracy
//! ROMs land.

#![deny(unsafe_code)]

pub mod affine;
pub mod background;
pub mod bios;
pub mod bitmap;
pub mod cartridge;
pub mod compositor;
pub mod dma;
pub mod effects;
pub mod fifo;
pub mod irq;
pub mod keypad;
pub mod memory;
pub mod objects;
pub mod system;
pub mod timers;
pub mod video;
pub mod waitstates;

pub use affine::AffineBackground;
pub use background::{Backgrounds, GbaTilemap};
pub use bitmap::bgr555_to_rgba8;
pub use cartridge::Cartridge;
pub use compositor::{Frame, GbaPalette};
pub use dma::DmaController;
pub use effects::{BlendMode, Effects, Layer};
pub use fifo::{DirectSound, SoundFifo};
pub use irq::InterruptController;
pub use keypad::Keypad;
pub use memory::{GbaBus, Region};
pub use objects::{Object, ObjectAttributeMemory};
pub use system::{GbaSystem, GbaSystemBus};
pub use timers::Timers;
pub use video::VideoTiming;
pub use waitstates::{Access, WaitControl};
