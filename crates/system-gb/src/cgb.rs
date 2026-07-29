//! The hardware a Game Boy Color adds to a Game Boy.
//!
//! # Why this is in `system-gb` and not `system-gbc`
//!
//! Everything here is reached by the CPU through the bus, mid-frame, at an address in the I/O
//! block — which means [`GbSystemBus`](crate::GbSystemBus) has to be able to name these types.
//! `system-gbc` depends on this crate, so anything it defined would be invisible from here.
//!
//! The alternative was a `CgbHardware` trait in this crate for `system-gbc` to implement. That
//! was rejected: it would have exactly one implementor, its shape would be dictated entirely
//! by that implementor, and it would put dynamic dispatch in the I/O write path to buy nothing
//! but a file location. An abstraction whose only purpose is to satisfy a crate boundary is
//! not an abstraction.
//!
//! So this follows the rule the rest of the crate already follows: a thing lives with the code
//! that consumes it. [`GbModel`](crate::GbModel) is here because the memory map branches on it,
//! [`TileAttributes`] is here because the PPU decodes it, and these are here because the bus
//! serves them.
//!
//! What is left in `system-gbc` is what genuinely belongs to the *machine* rather than to any
//! one component: the assembled `GbcSystem`, and the boot path that recolours a DMG cartridge.

pub mod attributes;
pub mod hdma;
pub mod palettes;
pub mod speed;

pub use attributes::TileAttributes;
pub use hdma::{Block, Hdma};
pub use palettes::{rgb555_to_rgba8, CgbPalettes};
pub use speed::SpeedSwitch;

use core_common::{Savable, StateError, StateReader, StateWriter};

/// The shades a CGB shows for a DMG cartridge it has no palette assignment for.
///
/// A real boot ROM hashes the cartridge title against a table of around thirty hand-assigned
/// palettes, so individual games came up blue or green rather than grey. That table lives in a
/// copyrighted boot ROM, which this project does not vendor, so the fallback is the DMG's own
/// four shades — faithful to what the game was drawn for, and honest about not guessing at a
/// colour scheme the hardware would have chosen.
pub struct GbcCompatibilityShades;

impl GbcCompatibilityShades {
    /// Lightest to darkest, as RGB555.
    ///
    /// The same greys as [`ppu_tile2d::DMG_SHADES`], expressed in the palette RAM's own format
    /// so they survive a read-modify-write by a game that fades them.
    pub const GREYSCALE: [u16; 4] = [0x7FFF, 0x56B5, 0x294A, 0x0000];
}

/// Every CGB-only register block, held together so the bus can offer them as one unit.
///
/// Present on a DMG too, and inert there: `owns` is gated on the model at the call site rather
/// than by making this an `Option`, because an `Option` would put a branch on the common path
/// *and* leave save states with two shapes to handle.
#[derive(Debug, Clone, Default)]
pub struct CgbState {
    pub palettes: CgbPalettes,
    pub speed: SpeedSwitch,
    pub hdma: Hdma,
}

impl CgbState {
    pub fn new() -> Self {
        Self {
            palettes: CgbPalettes::new(),
            speed: SpeedSwitch::new(),
            hdma: Hdma::new(),
        }
    }

    /// Whether this address is one of the CGB-only registers.
    ///
    /// `VBK` and `SVBK` are deliberately absent: bank selection belongs to the memory map,
    /// which already owns those two and already knows how many banks the model has.
    pub fn owns(addr: u16) -> bool {
        CgbPalettes::owns(addr) || Hdma::owns(addr) || addr == speed::KEY1
    }

    pub fn read_register(&self, addr: u16) -> Option<u8> {
        if addr == speed::KEY1 {
            return Some(self.speed.read());
        }
        self.palettes
            .read_register(addr)
            .or_else(|| self.hdma.read_register(addr))
    }

    /// Returns true when the write started a general-purpose DMA the caller must run now.
    pub fn write_register(&mut self, addr: u16, value: u8) -> bool {
        if addr == speed::KEY1 {
            self.speed.write(value);
            return false;
        }
        if self.palettes.write_register(addr, value).is_some() {
            return false;
        }
        self.hdma.write_register(addr, value).unwrap_or(false)
    }
}

impl Savable for CgbState {
    fn save(&self, w: &mut StateWriter) {
        self.palettes.save(w);
        self.speed.save(w);
        self.hdma.save(w);
    }

    fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        self.palettes.load(r)?;
        self.speed.load(r)?;
        self.hdma.load(r)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_block_routes_each_address_to_its_own_unit() {
        let mut state = CgbState::new();
        assert!(CgbState::owns(palettes::reg::BCPS));
        assert!(CgbState::owns(hdma::reg::HDMA5));
        assert!(CgbState::owns(speed::KEY1));
        assert!(!CgbState::owns(0xFF4F), "VBK belongs to the memory map");
        assert!(!CgbState::owns(0xFF70), "and so does SVBK");

        state.write_register(speed::KEY1, 0x01);
        assert!(state.speed.is_armed());
        assert_eq!(state.read_register(speed::KEY1), Some(0x7F));

        state.write_register(palettes::reg::BCPS, 0x05);
        assert_eq!(state.read_register(palettes::reg::BCPS), Some(0x45));
    }

    #[test]
    fn only_a_general_purpose_transfer_reports_back_as_immediate() {
        let mut state = CgbState::new();
        assert!(
            !state.write_register(palettes::reg::BCPD, 0xFF),
            "a palette write is not a DMA"
        );
        assert!(state.write_register(hdma::reg::HDMA5, 0x00));
        assert!(
            !state.write_register(hdma::reg::HDMA5, 0x80),
            "HBlank waits"
        );
    }

    #[test]
    fn the_whole_block_round_trips() {
        use savestate::{decode_state, encode_state};
        let mut state = CgbState::new();
        state.write_register(speed::KEY1, 0x01);
        state.speed.switch();
        state.write_register(palettes::reg::BCPS, 0x80);
        state.write_register(palettes::reg::BCPD, 0x3C);

        let bytes = encode_state("cgb", 1, &state);
        let mut restored = CgbState::new();
        decode_state("cgb", 1, &bytes, &mut restored).unwrap();
        assert_eq!(restored.speed, state.speed);
        assert_eq!(restored.palettes, state.palettes);
        assert_eq!(restored.hdma, state.hdma);
    }
}
