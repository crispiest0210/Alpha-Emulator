//! Putting the layers together into one scanline.
//!
//! # What decides what you see
//!
//! Up to four backgrounds and 128 sprites can want the same pixel. The rule is priority first
//! — 0 in front, 3 behind — and then, at equal priority, sprites beat backgrounds and a lower
//! background number beats a higher one. Every layer keeps its own palette index until the very
//! end, so a pixel that loses costs nothing but a comparison.
//!
//! # Which backgrounds exist depends on the mode
//!
//! Modes 0 to 2 differ in which of the four layers are present and whether they are text or
//! affine, and modes 3 to 5 have no tile layers at all — the bitmap *is* background 2. Getting
//! this wrong does not produce a subtly wrong picture; it produces a layer drawn from memory
//! that holds something else entirely.

use core_common::{Framebuffer, Rgba8};
use ppu_tile2d::{
    render_sprites, render_text_background, BackgroundParams, BitDepth, PaletteSource, PixelSource,
    ScanlineBuffer, SpriteRule,
};

use crate::affine::AffineBackground;
use crate::background::{Backgrounds, GbaTilemap};
use crate::bitmap;
use crate::objects::{ObjectAttributeMemory, ObjectMode};
use crate::video::{dispcnt, VideoTiming, SCREEN_HEIGHT, SCREEN_WIDTH};

/// Which background layers a mode has, and whether each is affine.
///
/// Returned as a fixed array rather than a `Vec` because the answer is a property of the mode,
/// not of the frame, and every caller wants to index it by layer number.
pub fn layers_for_mode(mode: u16) -> [Option<LayerKind>; 4] {
    use LayerKind::*;
    match mode {
        0 => [Some(Text), Some(Text), Some(Text), Some(Text)],
        1 => [Some(Text), Some(Text), Some(Affine), None],
        2 => [None, None, Some(Affine), Some(Affine)],
        // The bitmap modes have no tile layers: the bitmap occupies background 2's slot, and
        // the compositor draws it directly rather than through the tile pipeline.
        3..=5 => [None, None, Some(Bitmap), None],
        _ => [None; 4],
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayerKind {
    Text,
    Affine,
    Bitmap,
}

/// GBA palette RAM, as a [`PaletteSource`].
///
/// Background colours are the first 256 entries and sprite colours the second 256, so one
/// structure serves both lookups — unlike the Game Boy Color, where they are separate memories.
pub struct GbaPalette<'a> {
    pub bytes: &'a [u8],
}

impl GbaPalette<'_> {
    fn colour(&self, index: usize) -> Rgba8 {
        let offset = index * 2;
        match (self.bytes.get(offset), self.bytes.get(offset + 1)) {
            (Some(&low), Some(&high)) => bitmap::bgr555_to_rgba8(u16::from_le_bytes([low, high])),
            _ => Rgba8::BLACK,
        }
    }
}

impl PaletteSource for GbaPalette<'_> {
    fn lookup_bg(&self, palette: u8, color: u8) -> Rgba8 {
        self.colour((palette as usize & 0x0F) * 16 + color as usize)
    }

    fn lookup_sprite(&self, palette: u8, color: u8) -> Rgba8 {
        // Sprite palettes start halfway through the memory.
        self.colour(256 + (palette as usize & 0x0F) * 16 + color as usize)
    }
}

/// Everything a scanline render needs that is not the framebuffer.
pub struct Frame<'a> {
    pub video: &'a VideoTiming,
    pub backgrounds: &'a Backgrounds,
    /// The two affine layers' matrices and accumulated positions, for layers 2 and 3.
    pub affine: &'a [AffineBackground; 2],
    pub vram: &'a [u8],
    pub palette: &'a [u8],
    pub oam: &'a [u8],
}

/// Composite one scanline.
///
/// Draws back to front into a shared [`ScanlineBuffer`], so a pixel that is covered later costs
/// only the comparison that covered it — the indexed form from prompt 08 is what makes that
/// cheap enough to do naively.
pub fn render_scanline(frame: &Frame<'_>, line: u32, framebuffer: &mut Framebuffer) {
    if line >= SCREEN_HEIGHT {
        return;
    }

    // Forced blank is not "draw nothing": the screen goes white and video memory is untouched,
    // which is what makes it usable for hiding a mid-frame rewrite of VRAM.
    if frame.video.forced_blank() {
        let row = framebuffer.row_mut(line);
        for pixel in row.chunks_exact_mut(4) {
            pixel.copy_from_slice(&[0xFF, 0xFF, 0xFF, 0xFF]);
        }
        return;
    }

    let mode = frame.video.mode();
    let mut scanline = ScanlineBuffer::new(SCREEN_WIDTH as usize);
    scanline.clear();

    // Sprites draw over a bitmap just as they draw over a tile layer, so both paths go through
    // the same buffer — the bitmap is written straight to the row first and the sprites are
    // composited on top of it afterwards.
    if (3..=5).contains(&mode) {
        let row = framebuffer.row_mut(line);
        bitmap::render_scanline(
            mode,
            line,
            frame.vram,
            frame.palette,
            frame.video.bitmap_frame_offset(),
            row,
        );
        draw_sprites(frame, line, &mut scanline);
        let palette = GbaPalette {
            bytes: frame.palette,
        };
        overlay_sprites(&scanline, &palette, framebuffer.row_mut(line));
        return;
    }

    let kinds = layers_for_mode(mode);
    let enabled = [
        frame.video.dispcnt & (1 << 8) != 0,
        frame.video.dispcnt & (1 << 9) != 0,
        frame.video.dispcnt & (1 << 10) != 0,
        frame.video.dispcnt & (1 << 11) != 0,
    ];
    let present = std::array::from_fn(|i| enabled[i] && kinds[i].is_some());

    for index in frame.backgrounds.draw_order(present) {
        match kinds[index] {
            Some(LayerKind::Text) => draw_text_layer(frame, index, line, &mut scanline),
            Some(LayerKind::Affine) => draw_affine_layer(frame, index, &mut scanline),
            _ => {}
        }
    }
    draw_sprites(frame, line, &mut scanline);

    let palette = GbaPalette {
        bytes: frame.palette,
    };
    let backdrop = palette.lookup_bg(0, 0);
    let row = framebuffer.row_mut(line);
    scanline.resolve_into(&palette, backdrop, row);
}

fn draw_text_layer(frame: &Frame<'_>, index: usize, line: u32, scanline: &mut ScanlineBuffer) {
    let layer = frame.backgrounds.layers[index];
    let (width, height) = layer.size_in_tiles(false);
    let map = GbaTilemap {
        vram: frame.vram,
        screen_base: layer.screen_base(),
        char_base: layer.char_base(),
        depth: layer.bit_depth(),
        width,
        height,
    };
    let params = BackgroundParams::full_line(
        line,
        layer.scroll_x as u32,
        layer.scroll_y as u32,
        layer.bit_depth(),
    );
    render_text_background(&map, frame.vram, &params, scanline);
}

/// Draw one affine background layer.
///
/// Unlike a text layer, this walks the *screen* left to right and asks the transform where each
/// pixel came from, rather than walking the map. That is the only way round it can work: a
/// rotated map does not visit screen pixels in order.
///
/// Affine layers are always 256-colour and have no per-tile attributes — the map is one byte per
/// tile with no palette, flip, or priority bits, because the transform is doing the work those
/// would have done.
fn draw_affine_layer(frame: &Frame<'_>, index: usize, scanline: &mut ScanlineBuffer) {
    let layer = frame.backgrounds.layers[index];
    let affine = &frame.affine[index - 2];
    let (width, height) = layer.size_in_tiles(true);
    let map_base = layer.screen_base();
    let char_base = layer.char_base();
    let pixels = (width * 8, height * 8);

    for x in 0..SCREEN_WIDTH {
        let (tx, ty) = affine.texture_at(x);

        // Outside the map, a layer either wraps or shows nothing, depending on a control bit.
        // Wrapping is not the default: a game rotating a small map wants the edges to fall away
        // rather than tile, and reading the bit backwards makes a spinning floor look like
        // wallpaper.
        let (tx, ty) = if layer.affine_wraps() {
            (
                tx.rem_euclid(pixels.0 as i32),
                ty.rem_euclid(pixels.1 as i32),
            )
        } else {
            if tx < 0 || ty < 0 || tx >= pixels.0 as i32 || ty >= pixels.1 as i32 {
                continue;
            }
            (tx, ty)
        };

        let cell = map_base + (ty as usize / 8) * width as usize + (tx as usize / 8);
        let tile = frame.vram.get(cell).copied().unwrap_or(0) as usize;
        // 256 colours, so one byte per pixel and 64 bytes per tile.
        let offset = char_base + tile * 64 + (ty as usize % 8) * 8 + (tx as usize % 8);
        let colour = frame.vram.get(offset).copied().unwrap_or(0);
        if colour == 0 {
            continue;
        }
        scanline.set(
            x as usize,
            ppu_tile2d::IndexedPixel {
                color: colour,
                palette: 0,
                priority: layer.priority(),
                source: PixelSource::Background,
            },
        );
    }
}

/// Draw the sprites covering this line, front-most first.
///
/// Only non-affine sprites for now: an affine one needs its matrix applied per pixel, which the
/// shared compositor does not describe. They are skipped rather than drawn untransformed,
/// because an untransformed rotated sprite looks like a deliberate picture that is simply wrong.
fn draw_sprites(frame: &Frame<'_>, line: u32, scanline: &mut ScanlineBuffer) {
    if frame.video.dispcnt & dispcnt::OBJ == 0 {
        return;
    }
    let oam = ObjectAttributeMemory::decode(frame.oam);
    let one_dimensional = frame.video.dispcnt & dispcnt::OBJ_1D_MAPPING != 0;

    let sprites: Vec<_> = oam
        .visible_on_line(line as i32)
        .into_iter()
        .map(|index| oam.objects[index])
        .filter(|object| object.mode == ObjectMode::Normal)
        .map(|object| object.to_sprite(one_dimensional))
        .collect();

    // Sprite tile data is addressed from the object half of VRAM, and a 256-colour sprite
    // decodes differently from a 16-colour one — but a scanline can hold both, so the depth is
    // taken from each sprite rather than from the batch. Passed as four-bit here because that
    // is what the shared entry point takes; eight-bit sprites are a follow-up.
    render_sprites(
        &sprites,
        frame.vram,
        BitDepth::Four,
        line,
        SpriteRule::SpriteDecides,
        scanline,
    );
}

/// Write the sprite pixels of a resolved buffer over an already-drawn row.
///
/// Used by the bitmap path, where the background is already RGBA in the row and only the sprite
/// pixels need resolving.
fn overlay_sprites(scanline: &ScanlineBuffer, palette: &GbaPalette<'_>, row: &mut [u8]) {
    for (x, pixel) in row.chunks_exact_mut(4).enumerate() {
        let indexed = scanline.get(x);
        if indexed.source != PixelSource::Sprite {
            continue;
        }
        let colour = palette.lookup_sprite(indexed.palette, indexed.color);
        pixel.copy_from_slice(&[colour.r, colour.g, colour.b, colour.a]);
    }
}

/// Whether a pixel came from a background rather than the backdrop.
///
/// Small enough to inline at the two call sites that want it, but named because "did anything
/// draw here" is the question the priority rule keeps asking.
#[inline]
pub fn was_drawn(source: PixelSource) -> bool {
    source != PixelSource::Backdrop
}

#[cfg(test)]
mod tests;
