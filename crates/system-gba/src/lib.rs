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
//! - **No accuracy coverage.** `gba-suite` and `arm7wrestler` are not in the corpus, so
//!   everything here rests on hardware documentation and unit tests. They also exercise the ARM
//!   core, which has never been run against anything — worth doing both together.
//! - Affine backgrounds and affine sprites are decoded and transformed but not composited; they
//!   show the backdrop rather than an untransformed approximation.
//! - Wait states are computed by [`waitstates`] but not yet charged to the CPU, so every access
//!   currently costs what the ARM core says it does.
//! - The four `apu-shared` PSG channels are not mixed in alongside the two FIFO channels.
//! - Windows, colour blending, and mosaic are not implemented.
//! - EEPROM saves are reported as absent rather than emulated; SRAM and Flash work.
//! - Keypad input is not wired to `KEYINPUT`.
//!
//! The GBA is the system the *predecessor* project targeted, so prompt 12 sets the bar at "at
//! least as correct and complete as the vendored core it replaces, with the test coverage that
//! core never had". The second half of that is met; the first is unmeasured until the accuracy
//! ROMs land.

#![deny(unsafe_code)]

pub mod affine;
pub mod background;
pub mod bitmap;
pub mod cartridge;
pub mod compositor;
pub mod dma;
pub mod fifo;
pub mod irq;
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
pub use fifo::{DirectSound, SoundFifo};
pub use irq::InterruptController;
pub use memory::{GbaBus, Region};
pub use objects::{Object, ObjectAttributeMemory};
pub use system::{GbaSystem, GbaSystemBus};
pub use timers::Timers;
pub use video::VideoTiming;
pub use waitstates::{Access, WaitControl};
