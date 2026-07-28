//! CGB colour palette RAM, behind `BCPS`/`BCPD` and `OCPS`/`OCPD`.
//!
//! # Why this is not a `[Rgba8; 32]`
//!
//! The palettes are 64 bytes of RAM the CPU reads and writes a byte at a time through an
//! index register, and games *do* read them back — a fade routine typically reads a colour,
//! darkens it, and writes it back. Storing decoded [`Rgba8`] would mean re-encoding on every
//! read, and RGB555 does not round-trip through RGBA8888 unless the conversion is exactly
//! invertible. Keeping the hardware's own bytes and converting on lookup keeps reads exact
//! and puts the lossy step where it belongs: at the point the picture is produced.

use core_common::{Rgba8, Savable, StateError, StateReader, StateWriter};
use ppu_tile2d::PaletteSource;

/// Eight palettes of four colours, two bytes each.
pub const PALETTE_BYTES: usize = 64;

/// Register addresses.
pub mod reg {
    /// Background palette index and auto-increment flag.
    pub const BCPS: u16 = 0xFF68;
    /// Background palette data at the current index.
    pub const BCPD: u16 = 0xFF69;
    /// Sprite palette index and auto-increment flag.
    pub const OCPS: u16 = 0xFF6A;
    /// Sprite palette data at the current index.
    pub const OCPD: u16 = 0xFF6B;
}

/// One index register: six address bits and an auto-increment flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct Index {
    address: u8,
    auto_increment: bool,
}

impl Index {
    fn write(&mut self, value: u8) {
        self.address = value & 0x3F;
        self.auto_increment = value & 0x80 != 0;
    }

    fn read(&self) -> u8 {
        // Bit 6 is unused and reads high.
        self.address | ((self.auto_increment as u8) << 7) | 0x40
    }

    /// Advance after a *write*, and only after a write — reads never move the index, which is
    /// what lets a fade routine read a colour and write it back to the same slot.
    fn advance(&mut self) {
        if self.auto_increment {
            self.address = (self.address + 1) & 0x3F;
        }
    }
}

/// Background and sprite colour palette RAM.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CgbPalettes {
    background: [u8; PALETTE_BYTES],
    sprite: [u8; PALETTE_BYTES],
    bg_index: Index,
    obj_index: Index,
}

impl Default for CgbPalettes {
    fn default() -> Self {
        Self::new()
    }
}

impl CgbPalettes {
    /// Power-on state.
    ///
    /// All ones, not zeroes: palette RAM comes up uninitialised, and the CGB boot ROM leaves
    /// it that way for a DMG cartridge it has not recoloured. A game that displays before
    /// writing its palettes sees white on hardware, and would see black here if this were
    /// zeroed — a difference that shows up on the very first frame.
    pub fn new() -> Self {
        Self {
            background: [0xFF; PALETTE_BYTES],
            sprite: [0xFF; PALETTE_BYTES],
            bg_index: Index::default(),
            obj_index: Index::default(),
        }
    }

    pub fn owns(addr: u16) -> bool {
        (reg::BCPS..=reg::OCPD).contains(&addr)
    }

    pub fn read_register(&self, addr: u16) -> Option<u8> {
        Some(match addr {
            reg::BCPS => self.bg_index.read(),
            reg::BCPD => self.background[self.bg_index.address as usize],
            reg::OCPS => self.obj_index.read(),
            reg::OCPD => self.sprite[self.obj_index.address as usize],
            _ => return None,
        })
    }

    pub fn write_register(&mut self, addr: u16, value: u8) -> Option<()> {
        match addr {
            reg::BCPS => self.bg_index.write(value),
            reg::BCPD => {
                self.background[self.bg_index.address as usize] = value;
                self.bg_index.advance();
            }
            reg::OCPS => self.obj_index.write(value),
            reg::OCPD => {
                self.sprite[self.obj_index.address as usize] = value;
                self.obj_index.advance();
            }
            _ => return None,
        }
        Some(())
    }

    /// Overwrite a whole colour, for the boot path that installs a compatibility palette.
    pub fn set_colour(&mut self, sprite: bool, palette: u8, colour: u8, rgb555: u16) {
        let offset = slot(palette, colour);
        let target = if sprite {
            &mut self.sprite
        } else {
            &mut self.background
        };
        target[offset] = rgb555 as u8;
        target[offset + 1] = (rgb555 >> 8) as u8;
    }

    fn colour(bytes: &[u8; PALETTE_BYTES], palette: u8, index: u8) -> Rgba8 {
        let offset = slot(palette, index);
        rgb555_to_rgba8(u16::from_le_bytes([bytes[offset], bytes[offset + 1]]))
    }
}

/// Byte offset of a colour within a palette bank.
///
/// Masked rather than asserted: the callers are a pixel pipeline reading two- and three-bit
/// fields, so out-of-range is not reachable, and a panic in the middle of a scanline would be
/// a worse failure than a wrapped read.
#[inline]
fn slot(palette: u8, colour: u8) -> usize {
    ((palette as usize & 0x07) * 8) + ((colour as usize & 0x03) * 2)
}

/// Expand 5-bit channels to 8 bits.
///
/// `c << 3 | c >> 2` rather than `c * 255 / 31`: it is exact at both ends (0 stays 0, 31
/// becomes 255), monotonic, and needs no division. The alternative `c << 3` leaves white at
/// 248 — a visible grey cast across every bright colour on screen.
#[inline]
pub fn rgb555_to_rgba8(value: u16) -> Rgba8 {
    let expand = |c: u16| {
        let c = (c & 0x1F) as u8;
        (c << 3) | (c >> 2)
    };
    Rgba8 {
        r: expand(value),
        g: expand(value >> 5),
        b: expand(value >> 10),
        a: 0xFF,
    }
}

impl PaletteSource for CgbPalettes {
    #[inline]
    fn lookup_bg(&self, palette: u8, color: u8) -> Rgba8 {
        Self::colour(&self.background, palette, color)
    }

    #[inline]
    fn lookup_sprite(&self, palette: u8, color: u8) -> Rgba8 {
        Self::colour(&self.sprite, palette, color)
    }
}

impl Savable for CgbPalettes {
    fn save(&self, w: &mut StateWriter) {
        w.write_bytes(&self.background);
        w.write_bytes(&self.sprite);
        w.write_u8(self.bg_index.read());
        w.write_u8(self.obj_index.read());
    }

    fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        r.read_bytes(&mut self.background)?;
        r.read_bytes(&mut self.sprite)?;
        let bg = r.read_u8()?;
        let obj = r.read_u8()?;
        self.bg_index.write(bg);
        self.obj_index.write(obj);
        Ok(())
    }
}

#[cfg(test)]
mod tests;
