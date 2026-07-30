//! Sprites.
//!
//! Each engine has 128 OAM entries of eight bytes, of which six describe a sprite and the
//! remaining two are shared out among 32 groups of affine parameters — the same overlaid layout
//! the GBA uses, so a naive "read 128 sprites, read 32 matrices" pass reads the same bytes twice
//! with two different meanings, which is correct and is worth knowing is deliberate.
//!
//! # Back to front, not front to back
//!
//! The pass walks OAM from entry 127 down to 0 and lets a nearer sprite overwrite a farther one,
//! rather than claiming pixels front-first. That is not the arrangement `ppu_tile2d::render_sprites`
//! uses, and the reason is the DS's per-sprite priority field: two sprites can be drawn in OAM
//! order and still land either side of a background layer, so "which sprite is in front" cannot be
//! resolved before the background compositing that has not happened yet. Writing the winner into
//! the sprite layer with its own priority attached defers exactly that decision.
//!
//! Within one priority level, a lower OAM index wins — which falls out of walking backwards.
//!
//! # What is not modelled
//!
//! - The **per-line sprite and cycle budget**. Hardware runs out of time and drops sprites; this
//!   draws all of them. Games do not rely on the dropout, and modelling it needs the cycle costs
//!   of every sprite shape.
//! - **Mosaic**, as everywhere else in this engine.

use super::{dispcnt, Engine2d, LinePixel};
use crate::video::SCREEN_WIDTH;
use crate::vram::Vram;
use ppu_tile2d::{decode_tile_row, BitDepth};

/// Sprite dimensions in pixels, indexed by `[shape][size]`.
const SPRITE_SIZES: [[(u32, u32); 4]; 3] = [
    [(8, 8), (16, 16), (32, 32), (64, 64)],
    [(16, 8), (32, 8), (32, 16), (64, 32)],
    [(8, 16), (8, 32), (16, 32), (32, 64)],
];

/// One decoded OAM entry.
struct Object {
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    /// The box the sprite occupies on screen, which is twice its size for a double-size affine
    /// sprite.
    box_width: u32,
    box_height: u32,
    tile: u32,
    palette: u8,
    priority: u8,
    depth: BitDepth,
    flip_x: bool,
    flip_y: bool,
    affine: Option<usize>,
    mode: u8,
}

impl Engine2d {
    pub(super) fn render_objects(&mut self, line: u32, vram: &Vram, palette: &[u8], oam: &[u8]) {
        let base = self.engine.block_offset();
        for index in (0..128usize).rev() {
            let Some(object) = decode_object(oam, base, index) else {
                continue;
            };
            self.render_object(&object, line, vram, palette, oam);
        }
    }

    fn render_object(
        &mut self,
        object: &Object,
        line: u32,
        vram: &Vram,
        palette: &[u8],
        oam: &[u8],
    ) {
        // A sprite's Y wraps at 256, so one placed near the bottom reappears at the top. That is
        // how software parks sprites off-screen and how it makes them enter from above.
        let row = (line as i32 - object.y).rem_euclid(256);
        if row >= object.box_height as i32 {
            return;
        }

        let matrix = object
            .affine
            .map(|group| read_matrix(oam, self.engine.block_offset(), group));

        for dx in 0..object.box_width as i32 {
            let screen_x = object.x + dx;
            if !(0..SCREEN_WIDTH as i32).contains(&screen_x) {
                continue;
            }

            let (sx, sy) = match matrix {
                Some([pa, pb, pc, pd]) => {
                    // Transform about the centre of the on-screen box into the sprite's own
                    // space. The halves are the box's, the offsets the sprite's, which is what
                    // makes a double-size sprite rotate without clipping its own corners.
                    let cx = object.box_width as i32 / 2;
                    let cy = object.box_height as i32 / 2;
                    let ox = dx - cx;
                    let oy = row - cy;
                    let tx = (pa as i32 * ox + pb as i32 * oy) >> 8;
                    let ty = (pc as i32 * ox + pd as i32 * oy) >> 8;
                    (tx + object.width as i32 / 2, ty + object.height as i32 / 2)
                }
                None => {
                    let mut sx = dx;
                    let mut sy = row;
                    if object.flip_x {
                        sx = object.width as i32 - 1 - sx;
                    }
                    if object.flip_y {
                        sy = object.height as i32 - 1 - sy;
                    }
                    (sx, sy)
                }
            };
            if sx < 0 || sy < 0 || sx as u32 >= object.width || sy as u32 >= object.height {
                continue;
            }

            let Some((color, opaque)) =
                self.object_pixel(object, vram, palette, sx as u32, sy as u32)
            else {
                continue;
            };
            if !opaque {
                continue;
            }

            let x = screen_x as usize;
            if object.mode == 2 {
                // An object-window sprite contributes no colour at all: its opaque pixels are
                // the window shape, and its own graphics are never drawn.
                self.obj_window[x] = true;
                continue;
            }
            self.layers[super::LAYER_OBJ][x] = LinePixel {
                color,
                opaque: true,
                priority: object.priority,
                semi_transparent: object.mode == 1,
            };
        }
    }

    /// One pixel of a sprite, as a colour and whether it is opaque.
    fn object_pixel(
        &self,
        object: &Object,
        vram: &Vram,
        palette: &[u8],
        sx: u32,
        sy: u32,
    ) -> Option<(u16, bool)> {
        let space = self.engine.obj_space();
        if object.mode == 3 {
            let offset = self.bitmap_object_offset(object, sx, sy)?;
            let value = vram.read16(space, offset);
            return Some((value & 0x7FFF, value & 0x8000 != 0));
        }

        let one_d = self.dispcnt & dispcnt::OBJ_1D_MAPPING != 0;
        let tile_size = object.depth.tile_size() as u32;
        let tile_x = sx / 8;
        let tile_y = sy / 8;
        let offset = if one_d {
            // One-dimensional mapping: the sprite's tiles are consecutive, and the tile number
            // is scaled by a boundary from `DISPCNT` rather than by the tile size.
            let boundary = 32u32 << ((self.dispcnt & dispcnt::OBJ_1D_BOUNDARY) >> 20);
            object.tile * boundary + (tile_y * (object.width / 8) + tile_x) * tile_size
        } else {
            // Two-dimensional: the whole object region is a 32-tile-wide sheet the sprite is a
            // window onto, so moving down one row of the sprite is 32 tiles, not `width / 8`.
            // An 8bpp sprite covers two of those tiles per tile, which is why the row stride is
            // 32 * 32 bytes in both depths and the column step is the tile size.
            object.tile * 32 + tile_y * 32 * 32 + tile_x * tile_size
        };

        let row_size = object.depth.row_size() as u32;
        let row_offset = offset + (sy % 8) * row_size;
        let mut bytes = [0u8; 8];
        for (i, byte) in bytes[..row_size as usize].iter_mut().enumerate() {
            *byte = vram.read8(space, row_offset + i as u32);
        }
        let mut pixels = [0u8; 8];
        decode_tile_row(&bytes[..row_size as usize], object.depth, &mut pixels);
        let index = pixels[(sx % 8) as usize];
        if index == 0 {
            return Some((0, false));
        }

        let color = match object.depth {
            BitDepth::Eight => match self.dispcnt & dispcnt::OBJ_EXT_PALETTE {
                0 => {
                    super::read_palette(palette, self.engine.block_offset() + 0x200, index as usize)
                }
                _ => vram.read16(
                    self.engine.obj_ext_pal_space(),
                    (object.palette as u32 * 256 + index as u32) * 2,
                ),
            },
            _ => super::read_palette(
                palette,
                self.engine.block_offset() + 0x200,
                object.palette as usize * 16 + index as usize,
            ),
        };
        Some((color, true))
    }

    /// Byte offset of a bitmap sprite's pixel, which has three different address formulas.
    fn bitmap_object_offset(&self, object: &Object, sx: u32, sy: u32) -> Option<u32> {
        let tile = object.tile;
        if self.dispcnt & dispcnt::OBJ_BITMAP_1D != 0 {
            let boundary = if self.dispcnt & dispcnt::OBJ_BITMAP_1D_BOUNDARY != 0 {
                256
            } else {
                128
            };
            Some(tile * boundary + (sy * object.width + sx) * 2)
        } else {
            // Two-dimensional: the object region is a sheet either 128 or 256 pixels wide.
            let (sheet_width, row_shift) = if self.dispcnt & dispcnt::OBJ_BITMAP_WIDE != 0 {
                (256u32, 0x100u32)
            } else {
                (128, 0x80)
            };
            let base = (tile & 0x1F) * 0x10 + (tile & 0x3E0) * row_shift;
            Some(base + (sy * sheet_width + sx) * 2)
        }
    }
}

/// Decode one OAM entry, or `None` when it draws nothing.
fn decode_object(oam: &[u8], base: usize, index: usize) -> Option<Object> {
    let entry = base + index * 8;
    let attr0 = read16(oam, entry);
    let attr1 = read16(oam, entry + 2);
    let attr2 = read16(oam, entry + 4);

    let affine_flag = attr0 & 0x100 != 0;
    let double_or_disable = attr0 & 0x200 != 0;
    // Bit 9 means two different things: "double the on-screen box" for an affine sprite and
    // "this entry is disabled" for a plain one. Reading it as one or the other unconditionally
    // either hides every rotated sprite or shows 128 sprites nobody asked for.
    if !affine_flag && double_or_disable {
        return None;
    }

    let shape = ((attr0 >> 14) & 3) as usize;
    if shape == 3 {
        return None;
    }
    let size = ((attr1 >> 14) & 3) as usize;
    let (width, height) = SPRITE_SIZES[shape][size];
    let double = affine_flag && double_or_disable;

    let mode = ((attr0 >> 10) & 3) as u8;
    let depth = if attr0 & 0x2000 != 0 {
        BitDepth::Eight
    } else {
        BitDepth::Four
    };

    Some(Object {
        // X is nine bits signed: a sprite can start off the left edge.
        x: ((attr1 & 0x1FF) as i32) << 23 >> 23,
        y: (attr0 & 0xFF) as i32,
        width,
        height,
        box_width: if double { width * 2 } else { width },
        box_height: if double { height * 2 } else { height },
        tile: (attr2 & 0x3FF) as u32,
        palette: (attr2 >> 12) as u8,
        priority: ((attr2 >> 10) & 3) as u8,
        depth,
        flip_x: !affine_flag && attr1 & 0x1000 != 0,
        flip_y: !affine_flag && attr1 & 0x2000 != 0,
        affine: affine_flag.then_some(((attr1 >> 9) & 0x1F) as usize),
        mode,
    })
}

/// The four affine parameters of a group, which live in the unused halfword of four OAM entries.
///
/// `base` is the engine's OAM block, so group `g` starts at entry `4g` of *that* engine's 128.
fn read_matrix(oam: &[u8], base: usize, group: usize) -> [i16; 4] {
    let start = base + group * 32;
    std::array::from_fn(|i| read16(oam, start + i * 8 + 6) as i16)
}

fn read16(oam: &[u8], offset: usize) -> u16 {
    let low = oam.get(offset).copied().unwrap_or(0) as u16;
    let high = oam.get(offset + 1).copied().unwrap_or(0) as u16;
    low | (high << 8)
}
