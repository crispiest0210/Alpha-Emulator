//! GBA sprites: decoding OAM into something the shared compositor can draw.
//!
//! # One table, two things interleaved
//!
//! OAM is 128 eight-byte entries, but only the first six bytes of each are the sprite. The
//! remaining two hold *one sixteenth* of an affine matrix, so the 32 matrices are scattered
//! across the table at every fourth entry's seventh and eighth bytes. Reading OAM as a flat
//! array of sprites and a separate array of matrices — as this module does — is the only way to
//! keep either legible.
//!
//! # Shape and size are two fields that only mean something together
//!
//! Neither the two-bit shape nor the two-bit size names a dimension on its own; the pair
//! indexes a table of twelve. Four of the sixteen combinations are undefined, and hardware
//! draws *something* for them rather than nothing — but no game uses them, so they are mapped
//! to the square of that size and noted rather than treated as an error.
//!
//! # Tile numbering is not a simple index
//!
//! In 256-colour mode a sprite's tile number is still counted in 32-byte units even though its
//! tiles are 64 bytes, so the low bit is ignored. And whether the next row of a multi-tile
//! sprite is the next tile or 32 tiles further on depends on a bit in `DISPCNT` that applies to
//! every sprite at once. Both are easy to miss and both produce sprites made of the right tiles
//! in the wrong arrangement.

use core_common::{Savable, StateError, StateReader, StateWriter};
use ppu_tile2d::{BitDepth, Sprite};

/// Entries in OAM.
pub const OBJECT_COUNT: usize = 128;
/// Affine matrices, one per four OAM entries.
pub const MATRIX_COUNT: usize = 32;
/// Where object tile data begins within VRAM.
pub const OBJ_TILE_BASE: usize = 0x0001_0000;

/// How a sprite is drawn, from the two mode bits of attribute 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectMode {
    Normal,
    Affine,
    /// Not drawn at all. Distinct from a zero-size sprite: this is how a game parks an object
    /// it is not using without disturbing its other attributes.
    Hidden,
    /// Affine, with the drawing area doubled so a rotated sprite is not clipped by its own
    /// bounding box.
    AffineDouble,
}

/// What a sprite contributes to the picture, from the two graphics-mode bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GraphicsMode {
    Normal,
    /// Blended with what is underneath rather than replacing it.
    SemiTransparent,
    /// Draws nothing; its shape defines a window region for other layers.
    ObjectWindow,
}

/// One decoded OAM entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Object {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub mode: ObjectMode,
    pub graphics_mode: GraphicsMode,
    pub depth: BitDepth,
    pub tile: u16,
    pub palette: u8,
    pub priority: u8,
    pub flip_x: bool,
    pub flip_y: bool,
    pub mosaic: bool,
    /// Which of the 32 matrices to use, when [`ObjectMode`] is affine.
    pub matrix: usize,
}

/// Sprite dimensions for each shape and size pair.
///
/// The four `None` entries are the undefined combinations. Hardware draws something for them;
/// no game relies on it, so they fall back to the square of that size rather than silently
/// producing a zero-sized sprite that would look like a decoding bug.
const DIMENSIONS: [[Option<(u32, u32)>; 4]; 4] = [
    // Square
    [Some((8, 8)), Some((16, 16)), Some((32, 32)), Some((64, 64))],
    // Horizontal
    [Some((16, 8)), Some((32, 8)), Some((32, 16)), Some((64, 32))],
    // Vertical
    [Some((8, 16)), Some((8, 32)), Some((16, 32)), Some((32, 64))],
    // Prohibited
    [None, None, None, None],
];

impl Object {
    /// Decode one entry from its three attribute halfwords.
    pub fn decode(attr0: u16, attr1: u16, attr2: u16) -> Self {
        let mode = match (attr0 >> 8) & 3 {
            0 => ObjectMode::Normal,
            1 => ObjectMode::Affine,
            2 => ObjectMode::Hidden,
            _ => ObjectMode::AffineDouble,
        };
        let graphics_mode = match (attr0 >> 10) & 3 {
            1 => GraphicsMode::SemiTransparent,
            2 => GraphicsMode::ObjectWindow,
            _ => GraphicsMode::Normal,
        };

        let shape = ((attr0 >> 14) & 3) as usize;
        let size = ((attr1 >> 14) & 3) as usize;
        let (width, height) = DIMENSIONS[shape][size]
            .unwrap_or_else(|| DIMENSIONS[0][size].expect("the square row is complete"));

        let affine = matches!(mode, ObjectMode::Affine | ObjectMode::AffineDouble);
        let depth = if attr0 & (1 << 13) != 0 {
            BitDepth::Eight
        } else {
            BitDepth::Four
        };

        Self {
            // Both coordinates wrap: the fields are unsigned but a sprite can sit partly off
            // the top or left, and the hardware reaches that by wrapping past the far edge.
            x: sign_extend_9(attr1 & 0x01FF),
            y: sign_extend_8(attr0 & 0x00FF),
            width,
            height,
            mode,
            graphics_mode,
            depth,
            // In 256-colour mode the number is still counted in 32-byte units, so the low bit
            // does not select a tile and is ignored.
            tile: match depth {
                BitDepth::Eight => attr2 & 0x03FE,
                _ => attr2 & 0x03FF,
            },
            palette: match depth {
                BitDepth::Four => ((attr2 >> 12) & 0x0F) as u8,
                _ => 0,
            },
            priority: ((attr2 >> 10) & 3) as u8,
            // The flip bits share their position with the affine index and mean nothing when a
            // matrix is in use.
            flip_x: !affine && attr1 & (1 << 12) != 0,
            flip_y: !affine && attr1 & (1 << 13) != 0,
            mosaic: attr0 & (1 << 12) != 0,
            matrix: ((attr1 >> 9) & 0x1F) as usize,
        }
    }

    /// Whether this sprite is drawn at all.
    pub fn visible(&self) -> bool {
        self.mode != ObjectMode::Hidden && self.graphics_mode != GraphicsMode::ObjectWindow
    }

    /// The area the sprite occupies on screen, which is doubled in `AffineDouble`.
    pub fn screen_size(&self) -> (u32, u32) {
        match self.mode {
            ObjectMode::AffineDouble => (self.width * 2, self.height * 2),
            _ => (self.width, self.height),
        }
    }

    /// Whether this sprite covers any part of the given scanline.
    pub fn covers_line(&self, line: i32) -> bool {
        let (_, height) = self.screen_size();
        line >= self.y && line < self.y + height as i32
    }

    /// Byte offset of the sprite's first tile within VRAM.
    ///
    /// `one_dimensional` is `DISPCNT` bit 6 and applies to every sprite at once. It decides
    /// whether the row below a tile is the next tile or 32 tiles further on, which is the
    /// difference between a sprite's rows being adjacent and being scattered.
    pub fn tile_offset(&self, _one_dimensional: bool) -> usize {
        OBJ_TILE_BASE + self.tile as usize * 32
    }

    /// How far apart two rows of this sprite's tiles are, in bytes.
    pub fn row_stride(&self, one_dimensional: bool) -> usize {
        if one_dimensional {
            // The sprite's tiles are contiguous, so the next row starts after this one's width.
            (self.width / 8) as usize * self.depth.tile_size()
        } else {
            // The whole object area is one 32-tile-wide sheet and the sprite is a window onto
            // it, so the next row is always 32 tiles on — in this sprite's own tile size.
            32 * self.depth.tile_size()
        }
    }

    /// Convert to the shared compositor's normalised form.
    ///
    /// Only meaningful for a non-affine sprite: an affine one needs the matrix applied per
    /// pixel, which [`Sprite`] does not describe.
    pub fn to_sprite(&self, one_dimensional: bool) -> Sprite {
        Sprite {
            x: self.x,
            y: self.y,
            width: self.width,
            height: self.height,
            tile_offset: self.tile_offset(one_dimensional),
            palette: self.palette,
            flip_x: self.flip_x,
            flip_y: self.flip_y,
            // The GBA resolves sprite-versus-background priority per layer rather than with a
            // single "behind background" bit, so this is always false and the caller compares
            // `priority` against the background's instead.
            behind_background: false,
            row_stride: self.row_stride(one_dimensional),
        }
    }
}

#[inline]
fn sign_extend_8(value: u16) -> i32 {
    ((value as u8) as i8) as i32
}

#[inline]
fn sign_extend_9(value: u16) -> i32 {
    let value = value & 0x01FF;
    if value & 0x0100 != 0 {
        value as i32 - 0x0200
    } else {
        value as i32
    }
}

/// One affine transformation, as four 8.8 fixed-point values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AffineMatrix {
    pub pa: i16,
    pub pb: i16,
    pub pc: i16,
    pub pd: i16,
}

/// Read every sprite and matrix out of OAM.
///
/// Returned by value rather than borrowed because the caller needs OAM's *contents* while
/// holding a mutable borrow of the framebuffer, and 128 decoded entries is small enough that
/// copying beats fighting the borrow checker for it.
pub struct ObjectAttributeMemory {
    pub objects: [Object; OBJECT_COUNT],
    pub matrices: [AffineMatrix; MATRIX_COUNT],
}

impl ObjectAttributeMemory {
    /// Decode the whole table.
    ///
    /// The matrices are gathered in the same pass rather than in a second one, because they are
    /// physically interleaved with the sprites — every fourth entry's last halfword is one of
    /// the sixteen matrix components.
    pub fn decode(oam: &[u8]) -> Self {
        let halfword = |index: usize| -> u16 {
            match (oam.get(index * 2), oam.get(index * 2 + 1)) {
                (Some(&low), Some(&high)) => u16::from_le_bytes([low, high]),
                _ => 0,
            }
        };

        let mut objects = [Object::decode(0x0200, 0, 0); OBJECT_COUNT];
        for (index, object) in objects.iter_mut().enumerate() {
            let base = index * 4;
            *object = Object::decode(halfword(base), halfword(base + 1), halfword(base + 2));
        }

        let mut matrices = [AffineMatrix::default(); MATRIX_COUNT];
        for (index, matrix) in matrices.iter_mut().enumerate() {
            // Component n of matrix m is at halfword (m * 16) + (n * 4) + 3.
            let base = index * 16 + 3;
            *matrix = AffineMatrix {
                pa: halfword(base) as i16,
                pb: halfword(base + 4) as i16,
                pc: halfword(base + 8) as i16,
                pd: halfword(base + 12) as i16,
            };
        }

        Self { objects, matrices }
    }

    /// The sprites covering a scanline, front-most first.
    ///
    /// Order is by priority, then by OAM index — a lower index wins a tie, which is how a game
    /// controls overlap without changing priorities. Returned front-most first because that is
    /// the order the compositor consumes.
    pub fn visible_on_line(&self, line: i32) -> Vec<usize> {
        let mut found: Vec<usize> = (0..OBJECT_COUNT)
            .filter(|&i| self.objects[i].visible() && self.objects[i].covers_line(line))
            .collect();
        found.sort_by_key(|&i| (self.objects[i].priority, i));
        found
    }
}

impl Savable for AffineMatrix {
    fn save(&self, w: &mut StateWriter) {
        w.write_u16(self.pa as u16);
        w.write_u16(self.pb as u16);
        w.write_u16(self.pc as u16);
        w.write_u16(self.pd as u16);
    }

    fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        self.pa = r.read_u16()? as i16;
        self.pb = r.read_u16()? as i16;
        self.pc = r.read_u16()? as i16;
        self.pd = r.read_u16()? as i16;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
