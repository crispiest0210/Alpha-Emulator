//! Nintendo DS system assembly.
//!
//! # Status
//!
//! **Partial, and deliberately so.** Prompt 13 scopes this as the start of DS support rather
//! than its completion; what is here is built the way the GBA was, as tested units assembled
//! last. See `README.md` for the authoritative status table.
//!
//! Implemented: the dual-CPU memory map ([`memory`]), the BIOS calls both cores make ([`bios`]),
//! the ARM9's divider and square-root unit ([`math`]),
//! VRAM bank mapping ([`vram`]), both 2D
//! engines ([`engine2d`]), the 3D core ([`gpu3d`]), the sixteen-channel sound hardware ([`apu`]),
//! the inter-processor
//! communication hardware ([`ipc`]), the two interrupt controllers ([`irq`]), the two timer blocks
//! ([`timers`]), the two DMA controllers ([`dma`]), the video timing ([`video`]), the keypad and
//! touchscreen ([`input`]), the Slot-1 cartridge ([`cartridge`]), and the machine that assembles
//! them ([`system`]). The in-app debugger works against it through [`debug`], on the ARM9.
//!
//! Not implemented: wifi, KEY1 cartridge encryption, four BIOS calls whose answers would have to be
//! guessed at (see [`bios`]), and the 3D core's rarer effects — fog, edge
//! marking, anti-aliasing, shadow polygons, and the toon table. Prompt 13 explicitly ranks those
//! below geometry and texturing correctness, which is the order they were built in. Each is
//! documented where it is skipped rather than approximated into a picture that looks deliberate.
//!
//! # What a real ROM does with all this
//!
//! It runs. A libnds application loads both binaries, runs both cores, has every BIOS call it
//! makes answered, and draws its text on the sub screen to a frame that is byte-identical from
//! frame 60 to frame 240. A second one reads its own cartridge: 72 `0xB7` commands streaming its
//! NitroFS filesystem out of the ROM by card DMA, finished inside ten frames, and its whole
//! interface on screen. The accuracy suite runs both; see `testing/harness`'s `NDS_ROMS`.
//!
//! The second one also draws: a textured, normal-mapped 3D scene, checked against the reference
//! screenshot its author committed — mean absolute difference 6.6 of 255. What made it draw was
//! not a change to the 3D core at all but [`math`], the ARM9's divider, without which every matrix
//! libnds computes is a matrix of zeros.
//!
//! Worth knowing when reading the rest of this crate: getting there took five fixes, and **four of
//! them were not in this crate**. Two were missing ARMv5 behaviours in `cpu-arm946e` — THUMB
//! `BLX (register)` decoding as `BX`, and no interworking on a load into `R15` — and two were in
//! direct boot, which handed the ARM7 a memory map its binary could not live in and gave the ARM9
//! the ARM7's stack pointers. Every one of them was invisible to 376 passing unit tests, and every
//! one presented identically: a machine running at full speed with nothing on screen.
//!
//! That is the argument for keeping a real program in the corpus. One homebrew binary is worth
//! more than another hundred synthetic tests, because it exercises the joins rather than the
//! parts.
//!
//! # Wifi is out of scope
//!
//! The wifi hardware is not implemented and is not planned. Its register block reads as open bus,
//! which is what a DS with no card present looks like to software; games that offer local
//! multiplayer find no peer rather than hanging in a driver that never initialises.

#![deny(unsafe_code)]

pub mod apu;
pub mod bios;
pub mod cartridge;
pub mod debug;
pub mod diagnostics;
pub mod dma;
pub mod engine2d;
pub mod gpu3d;
pub mod input;
pub mod ipc;
pub mod irq;
pub mod math;
pub mod memory;
pub mod save;
pub mod system;
pub mod timers;
pub mod video;
pub mod vram;

pub use apu::NdsApu;
pub use bios::{BiosCall, BiosEffect};
pub use cartridge::NdsCartridge;
pub use dma::DmaController;
pub use engine2d::{Engine, Engine2d};
pub use gpu3d::Gpu3d;
pub use input::Input;
pub use ipc::Ipc;
pub use irq::InterruptController;
pub use math::MathUnits;
pub use memory::{Arm7Region, Arm9Region, NdsMemory, WramSplit};
pub use save::SaveChip;
pub use system::{NdsBus, NdsSystem};
pub use timers::TimerBlock;
pub use video::VideoTiming;
pub use vram::{Vram, VramSpace};

/// Which of the DS's two CPUs an operation concerns.
///
/// Lives here rather than in any one module because nearly every piece of DS hardware has two of
/// something indexed by it: two interrupt controllers, two sets of timers, two DMA controllers,
/// two views of the memory map, two ends of the IPC FIFOs. The discriminants are fixed at 0 and 1
/// so this can index a two-element array, which is what all of those turn out to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Core {
    /// The ARM946E-S: the "main" CPU, which runs game logic and drives the 3D engine.
    Arm9 = 0,
    /// The ARM7TDMI: audio, touchscreen, and the hardware the ARM9 cannot reach.
    Arm7 = 1,
}

impl Core {
    /// The other one. Most IPC operations are phrased as "this core does something to the other".
    #[inline]
    pub fn other(self) -> Core {
        match self {
            Core::Arm9 => Core::Arm7,
            Core::Arm7 => Core::Arm9,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Core::Arm9 => "ARM9",
            Core::Arm7 => "ARM7",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_core_indexes_a_two_element_array() {
        let pair = ["nine", "seven"];
        assert_eq!(pair[Core::Arm9 as usize], "nine");
        assert_eq!(pair[Core::Arm7 as usize], "seven");
        assert_eq!(Core::Arm9.other(), Core::Arm7);
        assert_eq!(Core::Arm7.other().other(), Core::Arm7);
    }
}
