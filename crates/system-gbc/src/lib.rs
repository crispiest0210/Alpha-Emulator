//! Game Boy Color system assembly: the deltas a CGB adds to a DMG.
//!
//! # Composition, and where it stops working
//!
//! Prompt 11 asks for reuse of `system-gb` over a parallel implementation, and says to
//! document the choice. The choice here is split, because the CGB deltas are of two kinds.
//!
//! The deltas that are genuinely *additive* — colour palette RAM, the double-speed switch,
//! tile attributes — are new register blocks and new state that a DMG simply does not have.
//! Those live here, as standalone units that own their own registers and implement
//! [`Savable`](core_common::Savable), exactly like the units inside `system-gb`.
//!
//! The deltas that are not additive are the ones that change what an *existing* component
//! does: the PPU fetches a second map byte and looks colour up through [`CgbPalettes`] instead
//! of the DMG's two monochrome palette registers, and `STOP` becomes a speed switch when one
//! is armed. Those cannot be layered on from outside — a `GbcSystem` that merely *contained* a
//! `GbSystem` would have no way to reach inside its scanline renderer. So the plan is to
//! parameterise `system-gb`'s components by [`Model`] rather than wrapping or forking them.
//!
//! # Status
//!
//! The additive units in this crate are complete and tested. What remains is that
//! parameterisation and the `System` implementation that assembles the two:
//!
//! - `ppu-tile2d`'s compositor already takes a [`PaletteSource`](ppu_tile2d::PaletteSource),
//!   which [`CgbPalettes`] implements — but `system-gb`'s PPU hardcodes the monochrome one and
//!   reads no attribute byte.
//! - `system-gb` already banks VRAM and WRAM, so [`TileAttributes`] has somewhere to be read
//!   from; nothing reads it yet.
//! - [`SpeedSwitch`] needs `STOP` in `system-gb` to consult it, and the CPU's cycle accounting
//!   to apply [`SpeedSwitch::cpu_multiplier`].
//! - HDMA (`0xFF51`-`0xFF55`) is not written yet; its HBlank mode needs a PPU hook to exist
//!   first, so it follows the parameterisation rather than preceding it.
//! - DMG-compatibility mode needs the boot path to install a compatibility palette through
//!   [`CgbPalettes::set_colour`].

#![deny(unsafe_code)]

pub mod attributes;
pub mod palettes;
pub mod speed;

pub use attributes::{background_wins, TileAttributes};
pub use palettes::{rgb555_to_rgba8, CgbPalettes};
pub use speed::SpeedSwitch;

/// Which hardware a cartridge is running on.
///
/// Three states, not two: a CGB running a DMG cartridge is its own mode. It has the CGB's
/// double-speed switch and banked memory available, but the boot ROM has installed a
/// compatibility palette and the game addresses the machine as if it were a DMG. Collapsing
/// that into "DMG" would lose the banking; collapsing it into "CGB" would recolour games that
/// never asked to be recoloured.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Model {
    /// Original hardware.
    #[default]
    Dmg,
    /// CGB hardware running a CGB-aware cartridge.
    Cgb,
    /// CGB hardware running an unmodified DMG cartridge.
    CgbInDmgMode,
}

impl Model {
    /// Whether the CGB register blocks respond at all.
    ///
    /// True in DMG-compatibility mode as well: the hardware is present and the registers
    /// answer, which is how the boot ROM installs its compatibility palette in the first
    /// place. What differs in that mode is that the *game* never touches them.
    pub fn has_cgb_hardware(self) -> bool {
        matches!(self, Model::Cgb | Model::CgbInDmgMode)
    }

    /// Whether the picture comes from CGB palette RAM rather than the DMG's `BGP`/`OBP`.
    pub fn uses_colour_palettes(self) -> bool {
        matches!(self, Model::Cgb | Model::CgbInDmgMode)
    }

    /// Whether the background map has a second attribute byte in VRAM bank 1.
    ///
    /// False in DMG-compatibility mode: the boot ROM leaves bank 1 alone and the game writes a
    /// DMG tile map, so reading attributes there would decode uninitialised memory as palette
    /// and flip bits.
    pub fn uses_tile_attributes(self) -> bool {
        matches!(self, Model::Cgb)
    }

    /// Pick the model for a cartridge, from the CGB flag at `0x0143` of its header.
    ///
    /// `0x80` means "enhanced for CGB but still runs on a DMG" and `0xC0` means "CGB only";
    /// both run in full CGB mode on CGB hardware. Anything else is a DMG cartridge, which on
    /// CGB hardware means compatibility mode.
    pub fn for_cartridge(cgb_flag: u8, on_cgb_hardware: bool) -> Self {
        if !on_cgb_hardware {
            return Model::Dmg;
        }
        match cgb_flag {
            0x80 | 0xC0 => Model::Cgb,
            _ => Model::CgbInDmgMode,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dmg_cartridge_on_cgb_hardware_is_its_own_mode() {
        assert_eq!(Model::for_cartridge(0x00, true), Model::CgbInDmgMode);
        assert_eq!(Model::for_cartridge(0x00, false), Model::Dmg);
    }

    #[test]
    fn both_cgb_header_flags_select_full_colour_mode() {
        // 0x80 is "enhanced for CGB, still runs on a DMG" and 0xC0 is "CGB only"; on CGB
        // hardware they are the same thing.
        assert_eq!(Model::for_cartridge(0x80, true), Model::Cgb);
        assert_eq!(Model::for_cartridge(0xC0, true), Model::Cgb);
    }

    #[test]
    fn a_cgb_cartridge_in_a_dmg_still_runs_as_a_dmg() {
        // The 0x80 flag exists precisely so these cartridges boot on original hardware.
        assert_eq!(Model::for_cartridge(0x80, false), Model::Dmg);
    }

    #[test]
    fn compatibility_mode_has_the_hardware_but_not_the_tile_attributes() {
        let m = Model::CgbInDmgMode;
        assert!(m.has_cgb_hardware(), "banking and KEY1 are present");
        assert!(m.uses_colour_palettes(), "the boot ROM recoloured the game");
        assert!(
            !m.uses_tile_attributes(),
            "VRAM bank 1 holds no attribute map, so reading one would decode garbage"
        );
    }

    #[test]
    fn a_dmg_has_none_of_it() {
        let m = Model::Dmg;
        assert!(!m.has_cgb_hardware());
        assert!(!m.uses_colour_palettes());
        assert!(!m.uses_tile_attributes());
    }
}
