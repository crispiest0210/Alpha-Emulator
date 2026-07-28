//! CGB background tile attributes, and the priority rule they feed.
//!
//! On a DMG the background map is one byte per tile: a tile index. A CGB keeps a second byte
//! for each tile in VRAM bank 1 at the same address, carrying palette, bank, flips, and a
//! priority bit. The bytes live in the existing VRAM banking that `system-gb` already
//! implements; what does not exist yet is the meaning of that second byte, which is what this
//! module is.
//!
//! # Only the bit layout is here
//!
//! What the priority bit *means* — the three-way contest between `LCDC` bit 0, the tile, and
//! the sprite — is [`ppu_tile2d::background_wins`], next to the compositor that applies it.
//! This module decodes the byte; that one resolves it.

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
    fn attributes_round_trip_through_a_save_state() {
        use savestate::{decode_state, encode_state};
        let a = TileAttributes::from_byte(0b1010_1011);
        let bytes = encode_state("gbc-attrs", 1, &a);
        let mut restored = TileAttributes::default();
        decode_state("gbc-attrs", 1, &bytes, &mut restored).unwrap();
        assert_eq!(a, restored);
    }
}
