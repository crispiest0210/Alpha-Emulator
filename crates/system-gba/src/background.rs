//! GBA tile backgrounds: the four `BGxCNT` layers of modes 0, 1, and 2.
//!
//! # What this adds to `ppu-tile2d`, and what it does not
//!
//! Prompt 08 puts the pixel pipeline in the shared crate and leaves each system to convert its
//! own tilemap format into the normalised [`TileRef`]. That is exactly the split here: this
//! module decodes a GBA map entry — a 16-bit word, against the Game Boy's one byte — and hands
//! the result over. The tile fetch, the palette lookup, and the scanline composition are not
//! reimplemented.
//!
//! # A screen larger than one map
//!
//! A GBA background can be 512 pixels wide, tall, or both, and the extra area is not one large
//! map. It is two or four 256×256 maps stored one after another, so a tile at x=300 is in the
//! *second* map's cell 5, not the first map's cell 37. Treating the map as a flat 64-cell grid
//! is the classic way to get a background that looks correct until it scrolls past 256 pixels.

use core_common::{Savable, StateError, StateReader, StateWriter};
use ppu_tile2d::{BitDepth, TileRef, TilemapSource};

/// Base address of `BG0CNT`; each layer's control register is two bytes on.
pub const CONTROL_BASE: u32 = 0x0400_0008;
/// Base address of `BG0HOFS`; each layer has a horizontal then a vertical offset.
pub const SCROLL_BASE: u32 = 0x0400_0010;

pub const LAYERS: usize = 4;

/// Character (tile data) blocks are 16 KiB apart.
pub const CHAR_BLOCK: usize = 0x4000;
/// Screen (tilemap) blocks are 2 KiB apart.
pub const SCREEN_BLOCK: usize = 0x800;

mod control {
    pub const PRIORITY: u16 = 0x0003;
    pub const CHAR_BASE: u16 = 0x000C;
    pub const MOSAIC: u16 = 1 << 6;
    /// Set selects 256 colours in one palette; clear selects 16 colours in sixteen palettes.
    pub const FULL_PALETTE: u16 = 1 << 7;
    pub const SCREEN_BASE: u16 = 0x1F00;
    /// For affine layers: wrap around rather than showing the backdrop past the edge.
    pub const AFFINE_WRAP: u16 = 1 << 13;
    pub const SIZE: u16 = 0xC000;
}

/// One background layer's registers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Background {
    pub control: u16,
    pub scroll_x: u16,
    pub scroll_y: u16,
}

impl Background {
    /// Lower is drawn in front. Ties are broken by layer number, lower first.
    pub fn priority(&self) -> u8 {
        (self.control & control::PRIORITY) as u8
    }

    /// Byte offset of this layer's tile data within VRAM.
    pub fn char_base(&self) -> usize {
        ((self.control & control::CHAR_BASE) >> 2) as usize * CHAR_BLOCK
    }

    /// Byte offset of this layer's tilemap within VRAM.
    pub fn screen_base(&self) -> usize {
        ((self.control & control::SCREEN_BASE) >> 8) as usize * SCREEN_BLOCK
    }

    pub fn bit_depth(&self) -> BitDepth {
        if self.control & control::FULL_PALETTE != 0 {
            BitDepth::Eight
        } else {
            BitDepth::Four
        }
    }

    pub fn mosaic(&self) -> bool {
        self.control & control::MOSAIC != 0
    }

    pub fn affine_wraps(&self) -> bool {
        self.control & control::AFFINE_WRAP != 0
    }

    /// Size in tiles, as (width, height).
    ///
    /// The four settings mean different things for a text layer and an affine one, which is why
    /// this takes the layer kind rather than being a plain lookup: 512×512 for a text layer is
    /// setting 3, and for an affine layer setting 3 means 1024×1024.
    pub fn size_in_tiles(&self, affine: bool) -> (u32, u32) {
        let setting = (self.control & control::SIZE) >> 14;
        if affine {
            let side = 16 << setting;
            (side, side)
        } else {
            match setting {
                0 => (32, 32),
                1 => (64, 32),
                2 => (32, 64),
                _ => (64, 64),
            }
        }
    }
}

/// All four layers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Backgrounds {
    pub layers: [Background; LAYERS],
}

impl Backgrounds {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn owns(addr: u32) -> bool {
        (CONTROL_BASE..CONTROL_BASE + 8).contains(&addr)
            || (SCROLL_BASE..SCROLL_BASE + 16).contains(&addr)
    }

    pub fn read16(&self, addr: u32) -> Option<u16> {
        if (CONTROL_BASE..CONTROL_BASE + 8).contains(&addr) {
            return Some(self.layers[((addr - CONTROL_BASE) / 2) as usize].control);
        }
        // The scroll registers are write-only; reading returns zero rather than the value.
        if (SCROLL_BASE..SCROLL_BASE + 16).contains(&addr) {
            return Some(0);
        }
        None
    }

    pub fn write16(&mut self, addr: u32, value: u16) -> Option<()> {
        if (CONTROL_BASE..CONTROL_BASE + 8).contains(&addr) {
            self.layers[((addr - CONTROL_BASE) / 2) as usize].control = value;
            return Some(());
        }
        if (SCROLL_BASE..SCROLL_BASE + 16).contains(&addr) {
            let offset = addr - SCROLL_BASE;
            let layer = (offset / 4) as usize;
            // Only nine bits are wired; a game writing a larger value gets it truncated rather
            // than scrolling somewhere the hardware cannot reach.
            let value = value & 0x01FF;
            if offset.is_multiple_of(4) {
                self.layers[layer].scroll_x = value;
            } else {
                self.layers[layer].scroll_y = value;
            }
            return Some(());
        }
        None
    }

    /// Which layers are enabled in `DISPCNT`, in the order they should be drawn.
    ///
    /// Back to front: higher priority number first, and within one priority the *higher* layer
    /// number first, so that layer 0 ends up in front of layer 3 at equal priority. Games rely
    /// on that tie-break to put a HUD over a background of the same priority.
    pub fn draw_order(&self, enabled: [bool; LAYERS]) -> Vec<usize> {
        let mut order: Vec<usize> = (0..LAYERS).filter(|&i| enabled[i]).collect();
        order.sort_by_key(|&i| {
            (
                std::cmp::Reverse(self.layers[i].priority()),
                std::cmp::Reverse(i),
            )
        });
        order
    }
}

/// A GBA text-mode tilemap, ready for [`ppu_tile2d::render_text_background`].
///
/// Holds only what the fetch needs, so that the borrow of VRAM stays as short as possible and
/// the renderer never sees the register layer at all.
pub struct GbaTilemap<'a> {
    pub vram: &'a [u8],
    pub screen_base: usize,
    pub char_base: usize,
    pub depth: BitDepth,
    /// Size in tiles.
    pub width: u32,
    pub height: u32,
    /// This layer's `BGxCNT` priority, 0 nearest.
    pub priority: u8,
}

impl TilemapSource for GbaTilemap<'_> {
    fn tile_at(&self, tile_x: u32, tile_y: u32) -> TileRef {
        let tile_x = tile_x % self.width;
        let tile_y = tile_y % self.height;

        // The map is stored as two or four 256x256 blocks in sequence, not as one wide grid.
        // A tile at x=300 is in the second block's column 5, not the first block's column 37.
        let block = (tile_x / 32) + (tile_y / 32) * (self.width / 32);
        let cell = (tile_y % 32) * 32 + (tile_x % 32);
        let offset = self.screen_base + (block as usize * SCREEN_BLOCK) + cell as usize * 2;

        let entry = match (self.vram.get(offset), self.vram.get(offset + 1)) {
            (Some(&low), Some(&high)) => u16::from_le_bytes([low, high]),
            _ => 0,
        };

        TileRef {
            data_offset: self.char_base + (entry & 0x03FF) as usize * self.depth.tile_size(),
            // The palette field is only meaningful in 16-colour mode; in 256-colour mode there
            // is one palette and the field is unused rather than zero-by-convention.
            palette: match self.depth {
                BitDepth::Four => ((entry >> 12) & 0x0F) as u8,
                _ => 0,
            },
            flip_x: entry & 0x0400 != 0,
            flip_y: entry & 0x0800 != 0,
            // The layer's priority, recorded on every pixel it draws. A GBA text map cell has no
            // priority bit of its own — unlike the Game Boy Color's tile attributes — so this comes
            // from `BGxCNT`. It was hard-coded to zero, which left every background claiming to be
            // the frontmost and made sprite-versus-background priority unresolvable.
            priority: self.priority,
        }
    }
}

impl Savable for Backgrounds {
    fn save(&self, w: &mut StateWriter) {
        for layer in &self.layers {
            w.write_u16(layer.control);
            w.write_u16(layer.scroll_x);
            w.write_u16(layer.scroll_y);
        }
    }

    fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        for layer in &mut self.layers {
            layer.control = r.read_u16()?;
            layer.scroll_x = r.read_u16()?;
            layer.scroll_y = r.read_u16()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
