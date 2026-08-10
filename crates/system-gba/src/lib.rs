//! Game Boy Advance system assembly.
//!
//! Follows prompt 11's proven pattern, adapted for a much larger machine, and built up in the
//! same order: the memory map first, then the register blocks that sit on it, then the
//! assembly that drives them.
//!
//! # Status
//!
//! **Commercial games run.** A cartridge boots, the display renders through the compositor, DMA
//! and timers drive the sound FIFOs, and interrupts reach the game's handler with or without a
//! BIOS. A save state round-trips frame-exactly and two runs of the same ROM are identical. All
//! three `gba-suite` ROMs pass — the instruction set in both states, and memory — and a real game
//! plays in the window at a measured 100% speed with no dropped frames or audio samples.
//!
//! What is not done, in rough order of how much it matters:
//!
//! - **The four `apu-shared` PSG channels are not mixed** alongside the two FIFO channels, so a
//!   game whose music comes through them is silent. What blocks it is smaller than it looks and
//!   was recorded here backwards for a while: the *channels* already live in `apu-shared` and are
//!   directly usable. What lives in `system-gb::apu`, unreachable because `system-*` crates may
//!   not depend on each other, is the **address decode** — and the GBA's is genuinely different
//!   anyway. Its registers are halfwords at `0x0400_0060`, laid out with gaps rather than as the
//!   Game Boy's contiguous `NR10`-`NR52`; its wave RAM is two banks of sixteen bytes with the CPU
//!   seeing whichever is not playing; and it has a 75% volume step the Game Boy lacks. So this is
//!   a new register layer over shared channels, not a copy of an existing one, and the `GbModel`
//!   gating that was called the obstacle does not apply — the GBA follows the CGB rule throughout.
//! - **EEPROM saves are reported as absent** rather than emulated; SRAM and Flash work, and a real
//!   cartridge's chip and size are detected correctly. No game has yet been played far enough to
//!   write a save file, so the path from a game's write to a file on disk is unverified.
//! - **Mosaic is not implemented**, and the object window is reported as never covering, since
//!   sprites drawn into it are not yet distinguished from ordinary ones.
//! - **No cartridge GPIO**, so a game with a real-time clock finds none. That is a supported
//!   hardware state rather than a failure — a cartridge with a dead battery behaves the same way
//!   and games handle it — but the clock never advances.
//! - The HLE BIOS in [`bios`] answers the calls games actually make; the rest change nothing
//!   rather than guessing, which shows up in a trace instead of surfacing far from its cause.
//!
//! # The bug worth knowing about before touching timing
//!
//! Every memory access was charged three to six times over, so an ARM instruction in internal
//! WRAM cost 13 cycles against hardware's 1. **No test failed and the emulator reported 100%
//! speed throughout**, because a frame is a fixed number of cycles however few instructions fit
//! inside it. What a commercial game lost was nine tenths of its processor, and what that looked
//! like was a frozen picture with the CPU visibly running. See `system::GbaSystemBus::charge`:
//! charge once, at the width the CPU asked for, and charge only the waiting.
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
pub mod debug;
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
