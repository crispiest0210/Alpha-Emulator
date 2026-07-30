//! Background layers: text, affine, and the three extended types.
//!
//! # Why `ppu_tile2d::render_text_background` is not used
//!
//! That function wants the tile data as one contiguous slice, which is right for every system
//! before this one — a Game Boy's tile data is 6 KiB at a fixed address, a GBA's is a window into
//! one 96 KiB block. A DS background's tile data is spread across up to seven VRAM banks behind
//! the page table in [`crate::vram`], and materialising a contiguous copy of a 512 KiB space every
//! line to satisfy the signature would cost far more than the fetch it replaces.
//!
//! What *is* shared is the part that is genuinely common: [`decode_tile_row`] turns a row of
//! bytes into eight colour indices in whichever of the three bit depths, and it is the piece with
//! the format traps in it. `ppu-tile2d`'s own documentation draws the line in this place — the
//! crate owns the pipeline and each system converts its own tilemap — so this is that split
//! applied, not an exception to it.
//!
//! # The five kinds are one dispatch, not five renderers
//!
//! Which kind each of the four layers is depends on the background mode *and* on two bits of
//! `BGxCNT` whose meaning changes with it. Getting that table wrong produces a layer drawn by the
//! wrong renderer, which looks like corrupt tile data rather than like a mode mix-up, so
//! [`BackgroundKind::of`] is a single function with the whole table in it and its own test.

use super::{dispcnt, Engine, Engine2d, LinePixel};
use crate::video::SCREEN_WIDTH;
use crate::vram::Vram;
use ppu_tile2d::{decode_tile_row, BitDepth};

/// What a background layer currently is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackgroundKind {
    /// Scrolling tilemap, 4 or 8 bits per pixel.
    Text,
    /// Rotated and scaled tilemap with 8-bit map entries and 8bpp tiles.
    Affine,
    /// Rotated and scaled tilemap with 16-bit map entries, so it keeps flips and palettes.
    ExtendedRotscale,
    /// A rotated and scaled 8-bit paletted bitmap.
    ExtendedBitmap,
    /// A rotated and scaled 15-bit direct-colour bitmap.
    ExtendedDirectBitmap,
    /// Engine A's mode 6 BG2. Not implemented; see the module docs of the parent.
    LargeBitmap,
    /// Engine A's BG0 when `DISPCNT` bit 3 is set. The 3D core does not exist.
    ThreeD,
    /// This layer does not exist in this mode.
    None,
}

impl BackgroundKind {
    /// The whole mode table.
    ///
    /// `mode` is `DISPCNT`'s low three bits, `bgcnt` the layer's own control register.
    pub fn of(engine: Engine, mode: u32, layer: usize, dispcnt_value: u32, bgcnt: u16) -> Self {
        // BG0 becomes the 3D layer whatever the mode says, and only on engine A.
        if layer == 0 && engine == Engine::A && dispcnt_value & dispcnt::BG0_IS_3D != 0 {
            return BackgroundKind::ThreeD;
        }
        if mode == 6 {
            return match (engine, layer) {
                (Engine::A, 0) => BackgroundKind::ThreeD,
                (Engine::A, 2) => BackgroundKind::LargeBitmap,
                _ => BackgroundKind::None,
            };
        }
        match layer {
            0 | 1 => BackgroundKind::Text,
            2 => match mode {
                0 | 1 | 3 => BackgroundKind::Text,
                2 | 4 => BackgroundKind::Affine,
                _ => Self::extended(bgcnt),
            },
            _ => match mode {
                0 => BackgroundKind::Text,
                1 | 2 => BackgroundKind::Affine,
                _ => Self::extended(bgcnt),
            },
        }
    }

    /// Which of the three extended types a layer is, from two bits of `BGxCNT` that mean
    /// something else entirely in every other mode.
    fn extended(bgcnt: u16) -> Self {
        match (bgcnt & 0x80 != 0, bgcnt & 0x04 != 0) {
            (false, _) => BackgroundKind::ExtendedRotscale,
            (true, false) => BackgroundKind::ExtendedBitmap,
            (true, true) => BackgroundKind::ExtendedDirectBitmap,
        }
    }
}

/// Map sizes in tiles for a text background, by `BGxCNT`'s size field.
const TEXT_SIZES: [(u32, u32); 4] = [(32, 32), (64, 32), (32, 64), (64, 64)];
/// Map sizes in tiles for an affine or extended-rotscale background.
const AFFINE_SIZES: [u32; 4] = [16, 32, 64, 128];
/// Bitmap sizes in pixels for an extended bitmap background.
const BITMAP_SIZES: [(u32, u32); 4] = [(128, 128), (256, 256), (512, 256), (512, 512)];

impl Engine2d {
    pub(super) fn render_background(
        &mut self,
        layer: usize,
        line: u32,
        vram: &Vram,
        palette: &[u8],
    ) {
        let mode = self.dispcnt & dispcnt::MODE;
        let bgcnt = self.bgcnt[layer];
        let kind = BackgroundKind::of(self.engine, mode, layer, self.dispcnt, bgcnt);
        match kind {
            BackgroundKind::Text => self.render_text(layer, line, vram, palette),
            BackgroundKind::Affine => self.render_affine(layer, vram, palette, false),
            BackgroundKind::ExtendedRotscale => self.render_affine(layer, vram, palette, true),
            BackgroundKind::ExtendedBitmap => self.render_bitmap(layer, vram, palette, false),
            BackgroundKind::ExtendedDirectBitmap => self.render_bitmap(layer, vram, palette, true),
            // Both leave the layer transparent, which is what "not implemented" has to look like
            // for the backdrop to show through rather than a plausible wrong picture.
            BackgroundKind::LargeBitmap | BackgroundKind::ThreeD | BackgroundKind::None => {}
        }
    }

    /// Base of this layer's tile data, including engine A's `DISPCNT` offset.
    fn char_base(&self, layer: usize) -> u32 {
        let block = ((self.bgcnt[layer] >> 2) & 0x0F) as u32 * 0x4000;
        block + self.dispcnt_char_offset()
    }

    /// Base of this layer's tilemap, including engine A's `DISPCNT` offset.
    fn screen_base(&self, layer: usize) -> u32 {
        let block = ((self.bgcnt[layer] >> 8) & 0x1F) as u32 * 0x800;
        block + self.dispcnt_screen_offset()
    }

    fn dispcnt_char_offset(&self) -> u32 {
        match self.engine {
            Engine::A => ((self.dispcnt & dispcnt::CHAR_BASE) >> 24) * 0x1_0000,
            Engine::B => 0,
        }
    }

    fn dispcnt_screen_offset(&self) -> u32 {
        match self.engine {
            Engine::A => ((self.dispcnt & dispcnt::SCREEN_BASE) >> 27) * 0x1_0000,
            Engine::B => 0,
        }
    }

    /// The extended-palette slot this layer reads, or `None` when extended palettes are off.
    ///
    /// BG0 and BG1 can be redirected to slots 2 and 3 by `BGxCNT` bit 13, which is the same bit
    /// that means "wrap the map" on an affine layer. Two layers pointed at one slot is legal and
    /// is how a game shares a palette between them.
    fn ext_palette_slot(&self, layer: usize) -> Option<u32> {
        if self.dispcnt & dispcnt::BG_EXT_PALETTE == 0 {
            return None;
        }
        Some(match layer {
            0 | 1 if self.bgcnt[layer] & (1 << 13) != 0 => layer as u32 + 2,
            _ => layer as u32,
        })
    }

    /// Look up a colour for an 8bpp background pixel, from either the extended palette or
    /// palette RAM.
    fn bg_color_8bpp(
        &self,
        vram: &Vram,
        palette: &[u8],
        layer: usize,
        entry_palette: u8,
        index: u8,
    ) -> u16 {
        match self.ext_palette_slot(layer) {
            Some(slot) => {
                let offset = slot * 0x2000 + (entry_palette as u32 * 256 + index as u32) * 2;
                vram.read16(self.engine.bg_ext_pal_space(), offset)
            }
            // Without extended palettes an 8bpp background ignores the entry's palette field
            // and indexes the single 256-colour palette.
            None => super::read_palette(palette, self.engine.block_offset(), index as usize),
        }
    }

    fn render_text(&mut self, layer: usize, line: u32, vram: &Vram, palette: &[u8]) {
        let bgcnt = self.bgcnt[layer];
        let priority = (bgcnt & 3) as u8;
        let depth = if bgcnt & 0x80 != 0 {
            BitDepth::Eight
        } else {
            BitDepth::Four
        };
        let (map_w, map_h) = TEXT_SIZES[((bgcnt >> 14) & 3) as usize];
        let char_base = self.char_base(layer);
        let screen_base = self.screen_base(layer);
        let space = self.engine.bg_space();

        let source_y = (line + self.bgvofs[layer] as u32) % (map_h * 8);
        let tile_y = source_y / 8;
        let row_in_tile = source_y % 8;

        let mut pixels = [0u8; 8];
        let mut loaded_tile_x = u32::MAX;
        let mut entry_palette = 0u8;

        for x in 0..SCREEN_WIDTH {
            let source_x = (x + self.bghofs[layer] as u32) % (map_w * 8);
            let tile_x = source_x / 8;

            if tile_x != loaded_tile_x {
                let entry = vram.read16(space, screen_base + map_offset(map_w, tile_x, tile_y));
                let tile = (entry & 0x3FF) as u32;
                let flip_x = entry & 0x400 != 0;
                let flip_y = entry & 0x800 != 0;
                entry_palette = (entry >> 12) as u8;

                let row = if flip_y { 7 - row_in_tile } else { row_in_tile };
                let offset =
                    char_base + tile * depth.tile_size() as u32 + row * depth.row_size() as u32;
                let mut row_bytes = [0u8; 8];
                for (i, byte) in row_bytes[..depth.row_size()].iter_mut().enumerate() {
                    *byte = vram.read8(space, offset + i as u32);
                }
                decode_tile_row(&row_bytes[..depth.row_size()], depth, &mut pixels);
                if flip_x {
                    pixels.reverse();
                }
                loaded_tile_x = tile_x;
            }

            let index = pixels[(source_x % 8) as usize];
            if index == 0 {
                continue;
            }
            let color = match depth {
                BitDepth::Eight => self.bg_color_8bpp(vram, palette, layer, entry_palette, index),
                _ => super::read_palette(
                    palette,
                    self.engine.block_offset(),
                    entry_palette as usize * 16 + index as usize,
                ),
            };
            self.layers[layer][x as usize] = LinePixel {
                color,
                opaque: true,
                priority,
                semi_transparent: false,
            };
        }
    }

    /// Affine and extended-rotscale layers, which differ only in their map entry format.
    fn render_affine(&mut self, layer: usize, vram: &Vram, palette: &[u8], wide_entries: bool) {
        let bgcnt = self.bgcnt[layer];
        let priority = (bgcnt & 3) as u8;
        let block = layer - 2;
        let tiles = AFFINE_SIZES[((bgcnt >> 14) & 3) as usize];
        let size = tiles * 8;
        // Bit 13 wraps the map instead of leaving the outside transparent. On a text layer the
        // same bit selects an extended-palette slot.
        let wrap = bgcnt & (1 << 13) != 0;
        let char_base = self.char_base(layer);
        let screen_base = self.screen_base(layer);
        let space = self.engine.bg_space();
        let [pa, _, pc, _] = self.bgp[block];

        for x in 0..SCREEN_WIDTH {
            let px = self.bgx_internal[block].wrapping_add(pa as i32 * x as i32) >> 8;
            let py = self.bgy_internal[block].wrapping_add(pc as i32 * x as i32) >> 8;
            let Some((sx, sy)) = fold(px, py, size, wrap) else {
                continue;
            };

            let tile_index = (sy / 8) * tiles + (sx / 8);
            let (tile, flip_x, flip_y, entry_palette) = if wide_entries {
                let entry = vram.read16(space, screen_base + tile_index * 2);
                (
                    (entry & 0x3FF) as u32,
                    entry & 0x400 != 0,
                    entry & 0x800 != 0,
                    (entry >> 12) as u8,
                )
            } else {
                (
                    vram.read8(space, screen_base + tile_index) as u32,
                    false,
                    false,
                    0,
                )
            };

            let mut row = sy % 8;
            let mut col = sx % 8;
            if flip_y {
                row = 7 - row;
            }
            if flip_x {
                col = 7 - col;
            }
            let index = vram.read8(space, char_base + tile * 64 + row * 8 + col);
            if index == 0 {
                continue;
            }
            let color = self.bg_color_8bpp(vram, palette, layer, entry_palette, index);
            self.layers[layer][x as usize] = LinePixel {
                color,
                opaque: true,
                priority,
                semi_transparent: false,
            };
        }
    }

    /// The two extended bitmap types. The bitmap lives at the *screen* base, in 16 KiB units
    /// rather than the 2 KiB a tilemap uses.
    fn render_bitmap(&mut self, layer: usize, vram: &Vram, palette: &[u8], direct: bool) {
        let bgcnt = self.bgcnt[layer];
        let priority = (bgcnt & 3) as u8;
        let block = layer - 2;
        let (width, height) = BITMAP_SIZES[((bgcnt >> 14) & 3) as usize];
        let wrap = bgcnt & (1 << 13) != 0;
        let base = ((bgcnt >> 8) & 0x1F) as u32 * 0x4000;
        let space = self.engine.bg_space();
        let [pa, _, pc, _] = self.bgp[block];

        for x in 0..SCREEN_WIDTH {
            let px = self.bgx_internal[block].wrapping_add(pa as i32 * x as i32) >> 8;
            let py = self.bgy_internal[block].wrapping_add(pc as i32 * x as i32) >> 8;
            let Some((sx, sy)) = fold_rect(px, py, width, height, wrap) else {
                continue;
            };
            let offset = sy * width + sx;

            let (color, opaque) = if direct {
                let value = vram.read16(space, base + offset * 2);
                // Bit 15 is the alpha bit: a direct-colour bitmap pixel with it clear is
                // transparent, not black.
                (value & 0x7FFF, value & 0x8000 != 0)
            } else {
                let index = vram.read8(space, base + offset);
                (
                    super::read_palette(palette, self.engine.block_offset(), index as usize),
                    index != 0,
                )
            };
            if !opaque {
                continue;
            }
            self.layers[layer][x as usize] = LinePixel {
                color,
                opaque: true,
                priority,
                semi_transparent: false,
            };
        }
    }
}

/// Byte offset of a tilemap cell in a text background.
///
/// A map wider or taller than 32 tiles is not one big grid: it is two or four 2 KiB blocks laid
/// out left-to-right then top-to-bottom, each internally 32x32. Treating it as a flat
/// `ty * width + tx` array puts the right-hand half of the screen 32 tiles too far along and
/// looks like a scroll bug.
fn map_offset(map_w: u32, tile_x: u32, tile_y: u32) -> u32 {
    let block = (tile_x / 32) + (tile_y / 32) * (map_w / 32);
    block * 0x800 + (tile_y % 32) * 64 + (tile_x % 32) * 2
}

/// Fold a transformed coordinate into a square map, or reject it when the map does not wrap.
fn fold(px: i32, py: i32, size: u32, wrap: bool) -> Option<(u32, u32)> {
    fold_rect(px, py, size, size, wrap)
}

fn fold_rect(px: i32, py: i32, width: u32, height: u32, wrap: bool) -> Option<(u32, u32)> {
    if wrap {
        Some((
            px.rem_euclid(width as i32) as u32,
            py.rem_euclid(height as i32) as u32,
        ))
    } else if px < 0 || py < 0 || px as u32 >= width || py as u32 >= height {
        None
    } else {
        Some((px as u32, py as u32))
    }
}
