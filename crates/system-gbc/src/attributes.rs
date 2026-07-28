//! CGB background tile attributes, and the priority rule they feed.
//!
//! On a DMG the background map is one byte per tile: a tile index. A CGB keeps a second byte
//! for each tile in VRAM bank 1 at the same address, carrying palette, bank, flips, and a
//! priority bit. The bytes live in the existing VRAM banking that `system-gb` already
//! implements; what does not exist yet is the meaning of that second byte, which is what this
//! module is.
//!
//! # The priority rule is not "the sprite bit wins"
//!
//! Three inputs decide whether a sprite or the background is visible at a pixel: `LCDC` bit 0
//! (which on a CGB stops meaning "background off" and starts meaning "background priority
//! off"), the background tile's priority bit, and the sprite's own priority bit. Getting this
//! wrong is not subtle — it is the difference between a character walking behind scenery and
//! through it — so the rule lives in one function, [`background_wins`], with the truth table
//! spelled out in its tests rather than scattered across the pixel loop.

use core_common::{Savable, StateError, StateReader, StateWriter};

/// One byte of CGB background tile attributes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TileAttributes {
    /// Background palette 0-7.
    pub palette: u8,
    /// Which VRAM bank holds the tile's pixel data.
    pub bank: u8,
    pub flip_x: bool,
    pub flip_y: bool,
    /// The tile asks to be drawn over sprites. Subject to `LCDC` bit 0 — see
    /// [`background_wins`].
    pub priority: bool,
}

impl TileAttributes {
    #[inline]
    pub fn from_byte(value: u8) -> Self {
        Self {
            palette: value & 0x07,
            bank: (value >> 3) & 0x01,
            flip_x: value & 0x20 != 0,
            flip_y: value & 0x40 != 0,
            priority: value & 0x80 != 0,
        }
    }

    #[inline]
    pub fn to_byte(self) -> u8 {
        (self.palette & 0x07)
            | ((self.bank & 0x01) << 3)
            | ((self.flip_x as u8) << 5)
            | ((self.flip_y as u8) << 6)
            | ((self.priority as u8) << 7)
    }
}

/// Whether the background pixel covers the sprite pixel.
///
/// `master_priority` is `LCDC` bit 0, which changes meaning between models: on a DMG it blanks
/// the background entirely, while on a CGB the background always draws and the bit instead
/// decides whether background and tile priority are honoured at all. Clearing it is how a CGB
/// game forces sprites to the front for a cutscene without touching its tile maps.
///
/// A background colour index of zero is always behind the sprite regardless of any priority
/// bit — index zero is the transparent one, and no priority setting makes a hole opaque.
#[inline]
pub fn background_wins(
    master_priority: bool,
    tile_priority: bool,
    sprite_behind: bool,
    background_colour: u8,
) -> bool {
    if background_colour == 0 {
        return false;
    }
    if !master_priority {
        return false;
    }
    tile_priority || sprite_behind
}

impl Savable for TileAttributes {
    fn save(&self, w: &mut StateWriter) {
        w.write_u8(self.to_byte());
    }

    fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        *self = Self::from_byte(r.read_u8()?);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_attribute_byte_round_trips_through_the_decoded_form() {
        // The unused bit 4 is the reason this is a sweep rather than a handful of cases: it
        // must not survive the round trip, and a spot check would miss that.
        for byte in 0..=u8::MAX {
            let decoded = TileAttributes::from_byte(byte);
            assert_eq!(
                decoded.to_byte(),
                byte & 0xEF,
                "attribute byte {byte:#04X} did not round trip"
            );
        }
    }

    #[test]
    fn the_fields_land_in_the_right_bits() {
        let a = TileAttributes::from_byte(0b1110_1101);
        assert_eq!(a.palette, 5);
        assert_eq!(a.bank, 1);
        assert!(a.flip_x);
        assert!(a.flip_y);
        assert!(a.priority);
    }

    #[test]
    fn a_transparent_background_pixel_never_covers_a_sprite() {
        // Colour zero is the transparent index; no priority bit makes a hole opaque.
        for tile_priority in [false, true] {
            for sprite_behind in [false, true] {
                assert!(
                    !background_wins(true, tile_priority, sprite_behind, 0),
                    "colour 0 covered a sprite with tile={tile_priority} sprite={sprite_behind}"
                );
            }
        }
    }

    #[test]
    fn clearing_lcdc_bit_zero_forces_every_sprite_to_the_front() {
        // On a CGB this is the "master priority" bit, not a background switch — it is how a
        // game puts sprites over everything for a cutscene without editing its tile maps.
        for tile_priority in [false, true] {
            for sprite_behind in [false, true] {
                for colour in 1..4 {
                    assert!(
                        !background_wins(false, tile_priority, sprite_behind, colour),
                        "background won with master priority off"
                    );
                }
            }
        }
    }

    #[test]
    fn either_priority_bit_puts_the_background_in_front() {
        assert!(background_wins(true, true, false, 1), "the tile asked");
        assert!(background_wins(true, false, true, 1), "the sprite yielded");
        assert!(background_wins(true, true, true, 1), "both");
        assert!(
            !background_wins(true, false, false, 1),
            "neither, so the sprite is in front"
        );
    }

    #[test]
    fn attributes_round_trip_through_a_save_state() {
        use savestate::{decode_state, encode_state};
        let a = TileAttributes::from_byte(0b1010_1011);
        let bytes = encode_state("gbc-attrs", 1, &a);
        let mut restored = TileAttributes::default();
        decode_state("gbc-attrs", 1, &bytes, &mut restored).unwrap();
        assert_eq!(a, restored);
    }
}
