//! Nintendo DS system assembly.
//!
//! # Status
//!
//! **Partial, and deliberately so.** Prompt 13 scopes this as the start of DS support rather
//! than its completion; what is here is built the way the GBA was, as tested units assembled
//! last. See `README.md` for the authoritative status table.
//!
//! Implemented so far: the dual-CPU memory map ([`memory`]), VRAM bank mapping ([`vram`]), and
//! the inter-processor communication hardware ([`ipc`]).
//!
//! Not implemented yet: the two 2D engines, the 3D core, the audio hardware, DMA, timers,
//! interrupts, the cartridge, and the [`core_common::System`] implementation itself.
//!
//! # Wifi is out of scope
//!
//! The wifi hardware is not implemented and is not planned. Its register block reads as open bus,
//! which is what a DS with no card present looks like to software; games that offer local
//! multiplayer find no peer rather than hanging in a driver that never initialises.

#![deny(unsafe_code)]

pub mod ipc;
pub mod memory;
pub mod vram;

pub use ipc::Ipc;
pub use memory::{Arm7Region, Arm9Region, NdsMemory, WramSplit};
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
