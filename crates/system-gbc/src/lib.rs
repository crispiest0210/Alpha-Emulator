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
//! - Colour resolution is wired: `system-gb`'s PPU takes a
//!   [`PaletteSource`](ppu_tile2d::PaletteSource) and a [`Model`], so handing it
//!   [`CgbPalettes`] produces a colour picture, with per-tile palettes, flips, and tile data
//!   from either VRAM bank read out of the attribute map in bank 1.
//! - Sprite-versus-background priority still uses the DMG rule. The CGB rule is written and
//!   tested as [`background_wins`], but the sprite compositor does not consult it yet, so a
//!   background tile that asks to be drawn over a sprite is currently ignored.
//! - [`SpeedSwitch`] needs `STOP` in `system-gb` to consult it, and the CPU's cycle accounting
//!   to apply [`SpeedSwitch::cpu_multiplier`].
//! - [`Hdma`] decides what to copy and when, but nothing calls it: its general-purpose mode
//!   needs the bus to perform the copy, and its HBlank mode additionally needs a PPU hook at
//!   the start of each horizontal blank.
//! - DMG-compatibility mode needs the boot path to install a compatibility palette through
//!   [`CgbPalettes::set_colour`].

#![deny(unsafe_code)]

pub mod hdma;
pub mod palettes;
pub mod speed;

pub use hdma::Hdma;

pub use palettes::{rgb555_to_rgba8, CgbPalettes};
pub use speed::SpeedSwitch;
/// Background tile attributes, and the sprite-priority rule they feed.
///
/// Re-exported for the same reason as [`Model`]: the only thing that reads an attribute byte
/// is the PPU, and the PPU lives in `system-gb`. Defining the decode here would mean the
/// renderer could not reach it without depending on this crate, which is backwards.
pub use system_gb::{background_wins, TileAttributes};

/// Which machine is being emulated.
///
/// Re-exported rather than redefined. `system-gb` already had this enum for its memory map —
/// the CGB's extra VRAM and WRAM banks are the same map with more banks — and the palette and
/// attribute questions the CGB adds are the same question about the same machine. A parallel
/// enum here would be two types that must agree forever, which is the duplication prompt 11
/// asks this crate to avoid.
pub use system_gb::GbModel as Model;

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

#[cfg(test)]
mod integration_tests {
    use super::*;
    use core_common::Rgba8;
    use system_gb::GbPpu;

    /// A tile map cell pointing at tile 1, with tile 1 filled with colour index 1.
    fn ppu_showing_colour_one() -> (GbPpu, Vec<u8>, Vec<u8>) {
        let mut vram = vec![0u8; 0x4000];
        // Tile 1's pixel data: every pixel colour index 1 (low bitplane set, high clear).
        for row in 0..8 {
            vram[0x10 + row * 2] = 0xFF;
        }
        // Tile map entry (0,0) -> tile 1.
        vram[0x1800] = 1;

        let mut ppu = GbPpu::new();
        // LCD on, background on, tile data at 0x8000.
        ppu.lcdc = 0x91;
        (ppu, vram, vec![0u8; 0xA0])
    }

    #[test]
    fn the_same_pixels_resolve_to_grey_on_a_dmg_and_to_colour_through_cgb_palette_ram() {
        // This is the whole point of keeping the scanline buffer indexed until the line is
        // done: one renderer, two lookups. If the PPU had resolved to RGBA during the fetch,
        // colour would have meant a second renderer.
        let (mut ppu, vram, oam) = ppu_showing_colour_one();

        ppu.render_scanline(0, &vram, &oam);
        let dmg = ppu.framebuffer().pixel(0, 0);
        assert_eq!(dmg.r, dmg.g, "a DMG pixel is grey");
        assert_eq!(dmg.g, dmg.b);

        let mut palettes = CgbPalettes::new();
        palettes.set_colour(false, 0, 1, 0x001F); // background palette 0, colour 1: red
        ppu.render_scanline_with(Model::Cgb, 0, &vram, &oam, &palettes);
        assert_eq!(
            ppu.framebuffer().pixel(0, 0),
            Rgba8 {
                r: 0xFF,
                g: 0,
                b: 0,
                a: 0xFF
            },
            "the same indexed pixel came out red through CGB palette RAM"
        );
    }

    #[test]
    fn clearing_lcdc_bit_zero_blanks_a_dmg_but_not_a_cgb() {
        // The bit keeps its position across the two machines and changes its job. Treating the
        // CGB case as a blank would black out the screen instead of merely reordering layers.
        let (mut ppu, vram, oam) = ppu_showing_colour_one();
        ppu.lcdc = 0x90; // background bit cleared

        ppu.render_scanline(0, &vram, &oam);
        assert_eq!(
            ppu.framebuffer().pixel(0, 0),
            ppu_tile2d::DMG_SHADES[0],
            "a DMG blanks to white"
        );

        let mut palettes = CgbPalettes::new();
        palettes.set_colour(false, 0, 1, 0x001F);
        ppu.render_scanline_with(Model::Cgb, 0, &vram, &oam, &palettes);
        assert_eq!(
            ppu.framebuffer().pixel(0, 0).r,
            0xFF,
            "a CGB still draws the background"
        );
    }

    #[test]
    fn compatibility_mode_draws_through_palette_ram_but_reads_no_attributes() {
        // The combination that makes the third variant necessary: a recoloured picture from a
        // tile map that never had an attribute byte written beside it.
        let m = Model::CgbInDmgMode;
        assert!(m.uses_colour_palettes());
        assert!(!m.uses_tile_attributes());
        assert!(!m.bg_enable_blanks_background(), "it is CGB hardware");
    }

    #[test]
    fn the_attribute_byte_picks_a_palette_per_tile() {
        // Without this the whole background resolves through palette 0, which looks like
        // colour is working right up until a game uses more than one palette on a line.
        let (mut ppu, mut vram, oam) = ppu_showing_colour_one();
        // Tile (1,0) also points at tile 1, so both cells draw identical pixels.
        vram[0x1801] = 1;
        // Bank 1, same offsets: cell 0 keeps palette 0, cell 1 takes palette 3.
        vram[0x2000 + 0x1800] = 0;
        vram[0x2000 + 0x1801] = 3;

        let mut palettes = CgbPalettes::new();
        palettes.set_colour(false, 0, 1, 0x001F); // palette 0 colour 1: red
        palettes.set_colour(false, 3, 1, 0x7C00); // palette 3 colour 1: blue

        ppu.render_scanline_with(Model::Cgb, 0, &vram, &oam, &palettes);
        assert_eq!(ppu.framebuffer().pixel(0, 0).r, 0xFF, "first tile is red");
        assert_eq!(ppu.framebuffer().pixel(8, 0).b, 0xFF, "second tile is blue");
        assert_eq!(ppu.framebuffer().pixel(8, 0).r, 0x00);
    }

    #[test]
    fn compatibility_mode_ignores_whatever_is_in_the_second_bank() {
        // The reason CgbInDmgMode exists. A DMG cartridge never writes bank 1, so anything
        // read from there is uninitialised memory — and decoding it as palette and flip bits
        // would corrupt a picture that is otherwise correct.
        let (mut ppu, mut vram, oam) = ppu_showing_colour_one();
        vram[0x2000 + 0x1800] = 0xFF; // palette 7, bank 1, both flips, priority

        let mut palettes = CgbPalettes::new();
        palettes.set_colour(false, 0, 1, 0x001F);
        palettes.set_colour(false, 7, 1, 0x7C00);

        ppu.render_scanline_with(Model::CgbInDmgMode, 0, &vram, &oam, &palettes);
        assert_eq!(
            ppu.framebuffer().pixel(0, 0).r,
            0xFF,
            "still palette 0, not the 7 the stale byte names"
        );
    }

    #[test]
    fn a_tile_can_take_its_pixels_from_the_second_bank() {
        let (mut ppu, mut vram, oam) = ppu_showing_colour_one();
        // Tile 2 in bank 1, colour index 1 everywhere.
        for row in 0..8 {
            vram[0x2000 + 0x20 + row * 2] = 0xFF;
        }
        vram[0x1800] = 2; // the map names tile 2
        vram[0x2000 + 0x1800] = 0x08; // attribute: bank 1

        let mut palettes = CgbPalettes::new();
        palettes.set_colour(false, 0, 1, 0x001F);
        ppu.render_scanline_with(Model::Cgb, 0, &vram, &oam, &palettes);
        assert_eq!(
            ppu.framebuffer().pixel(0, 0).r,
            0xFF,
            "tile data came from bank 1"
        );
    }
}
