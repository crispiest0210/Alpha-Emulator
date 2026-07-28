//! Shared 2D tile, sprite, and palette compositing primitives.
//!
//! Used by the Game Boy, Game Boy Color, and Game Boy Advance background and sprite layers,
//! and later by both of the DS's 2D engines. The DS's 3D core is *not* here — it shares
//! nothing with this — and neither are the GBA's bitmap modes, which are a direct framebuffer
//! rather than anything tile-based.
//!
//! # What is genuinely shared, and what is not
//!
//! Keeping a "shared" crate honest means being strict about where the line falls. These three
//! generations really do share their pixel pipeline: decode a tile from a bitplane or packed
//! format, look the index up in a palette, resolve priority between layers. They do *not*
//! share their tilemap entry formats or their OAM layouts — a Game Boy map cell is one byte,
//! a GBC cell adds an attribute byte in the second VRAM bank, and a GBA cell is a 16-bit word
//! with its own bit assignments.
//!
//! So the split is: this crate owns the pipeline and the primitives, and each system converts
//! its own tilemap and OAM into the normalized [`TileRef`] and [`Sprite`] this crate consumes.
//! That conversion is a few lines per system and keeps the shared code free of the
//! `if system == Gb` branches that would otherwise accumulate here until it was shared in
//! name only.
//!
//! # Scanline at a time, never a frame at a time
//!
//! Rendering is driven by the PPU mode events from the scheduler: when a line's drawing period
//! ends, that line is composited with the register values *as they are at that moment*. Games
//! rely on this. Changing the scroll registers mid-frame to split the screen — a status bar
//! that stays put while the world scrolls under it — is standard technique, and a renderer
//! that batched the whole frame at VBlank would draw it with whichever scroll value happened
//! to be last, losing the effect entirely.
//!
//! # Indexed until the last moment
//!
//! [`ScanlineBuffer`] holds *palette indices*, not colors, until the whole line is composited.
//! That is not an optimization — it is required for correctness. Game Boy sprite priority is
//! decided against the background's raw colour index: a sprite marked "behind background"
//! shows through wherever the background pixel is index 0, whatever colour index 0 currently
//! maps to. Resolving to RGB during background rendering would throw away the one value the
//! sprite pass needs.

#![deny(unsafe_code)]

mod compositor;
mod palette;

pub use compositor::{
    decode_tile_row, render_sprites, render_text_background, BackgroundParams, Sprite,
    TilemapSource,
};
pub use palette::{Bgr555Palette, MonochromePalette, PaletteSource, DMG_SHADES};

use core_common::Rgba8;

/// Bits per pixel in a tile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BitDepth {
    /// Two bitplanes per row, 16 bytes per 8x8 tile. Game Boy and Game Boy Color.
    Two,
    /// Two pixels per byte, 32 bytes per tile. The GBA's 16-colour mode.
    Four,
    /// One byte per pixel, 64 bytes per tile. The GBA's 256-colour mode.
    Eight,
}

impl BitDepth {
    /// Bytes occupied by one 8x8 tile.
    pub const fn tile_size(self) -> usize {
        match self {
            BitDepth::Two => 16,
            BitDepth::Four => 32,
            BitDepth::Eight => 64,
        }
    }

    /// Bytes occupied by one row of a tile.
    pub const fn row_size(self) -> usize {
        match self {
            BitDepth::Two => 2,
            BitDepth::Four => 4,
            BitDepth::Eight => 8,
        }
    }

    /// Number of distinct colour indices, including the transparent index 0.
    pub const fn colors(self) -> u16 {
        match self {
            BitDepth::Two => 4,
            BitDepth::Four => 16,
            BitDepth::Eight => 256,
        }
    }
}

/// Which layer produced a pixel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub enum PixelSource {
    /// Nothing has been drawn here; the backdrop colour shows.
    #[default]
    Backdrop,
    Background,
    Sprite,
}

/// One composited pixel, still as a palette index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct IndexedPixel {
    /// Index within the palette. Zero is transparent for sprites, and is what background
    /// pixels are tested against when resolving sprite priority.
    pub color: u8,
    pub palette: u8,
    /// Layer priority, lower in front. Used by systems that have more than one background.
    pub priority: u8,
    pub source: PixelSource,
}

impl IndexedPixel {
    /// Whether a sprite drawn over this background pixel would be hidden by it, given the
    /// sprite's "behind background" flag.
    ///
    /// Index 0 is the background's own transparency: a "behind background" sprite still shows
    /// through it. This is the rule that forces the buffer to stay indexed.
    #[inline]
    pub fn hides_sprite_behind_it(&self) -> bool {
        self.source == PixelSource::Background && self.color != 0
    }
}

/// One scanline of composited, not-yet-coloured pixels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanlineBuffer {
    pixels: Vec<IndexedPixel>,
}

impl ScanlineBuffer {
    pub fn new(width: usize) -> Self {
        Self {
            pixels: vec![IndexedPixel::default(); width],
        }
    }

    pub fn width(&self) -> usize {
        self.pixels.len()
    }

    /// Reset every pixel to the backdrop, ready for the next line.
    pub fn clear(&mut self) {
        self.pixels.fill(IndexedPixel::default());
    }

    #[inline]
    pub fn get(&self, x: usize) -> IndexedPixel {
        self.pixels[x]
    }

    #[inline]
    pub fn set(&mut self, x: usize, pixel: IndexedPixel) {
        self.pixels[x] = pixel;
    }

    pub fn as_slice(&self) -> &[IndexedPixel] {
        &self.pixels
    }

    /// Resolve the line to colours and write it into a framebuffer row.
    ///
    /// `backdrop` fills anything no layer covered. The row is written directly as RGBA bytes,
    /// which is the framebuffer's storage format, so nothing is repacked on the way out.
    pub fn resolve_into(&self, palettes: &dyn PaletteSource, backdrop: Rgba8, row: &mut [u8]) {
        for (pixel, out) in self.pixels.iter().zip(row.chunks_exact_mut(4)) {
            let color = match pixel.source {
                PixelSource::Backdrop => backdrop,
                PixelSource::Background => palettes.lookup_bg(pixel.palette, pixel.color),
                PixelSource::Sprite => palettes.lookup_sprite(pixel.palette, pixel.color),
            };
            out[0] = color.r;
            out[1] = color.g;
            out[2] = color.b;
            out[3] = color.a;
        }
    }
}

/// One tilemap cell, normalized out of whatever format the system stores it in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TileRef {
    /// Byte offset of the tile's data within the tile-data slice given to the renderer.
    ///
    /// An offset rather than a tile number because systems address tile data differently —
    /// the Game Boy's `8800` addressing mode indexes signed from the middle of the region —
    /// and resolving that is the system's business, not this crate's.
    pub data_offset: usize,
    pub palette: u8,
    pub flip_x: bool,
    pub flip_y: bool,
    /// Lower is in front.
    pub priority: u8,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bit_depths_report_their_layout() {
        assert_eq!(BitDepth::Two.tile_size(), 16);
        assert_eq!(BitDepth::Two.row_size(), 2);
        assert_eq!(BitDepth::Two.colors(), 4);

        assert_eq!(BitDepth::Four.tile_size(), 32);
        assert_eq!(BitDepth::Four.row_size(), 4);
        assert_eq!(BitDepth::Four.colors(), 16);

        assert_eq!(BitDepth::Eight.tile_size(), 64);
        assert_eq!(BitDepth::Eight.row_size(), 8);
        assert_eq!(BitDepth::Eight.colors(), 256);

        // Eight rows of a tile fill it exactly.
        for depth in [BitDepth::Two, BitDepth::Four, BitDepth::Eight] {
            assert_eq!(depth.row_size() * 8, depth.tile_size());
        }
    }

    #[test]
    fn a_fresh_scanline_is_all_backdrop() {
        let line = ScanlineBuffer::new(8);
        assert_eq!(line.width(), 8);
        for x in 0..8 {
            assert_eq!(line.get(x).source, PixelSource::Backdrop);
        }
    }

    #[test]
    fn only_a_nonzero_background_pixel_hides_a_sprite_behind_it() {
        let opaque = IndexedPixel {
            color: 2,
            source: PixelSource::Background,
            ..Default::default()
        };
        assert!(opaque.hides_sprite_behind_it());

        // Index 0 is the background's transparency, so a sprite shows through.
        let transparent = IndexedPixel {
            color: 0,
            source: PixelSource::Background,
            ..Default::default()
        };
        assert!(!transparent.hides_sprite_behind_it());

        // And the backdrop never hides anything.
        assert!(!IndexedPixel::default().hides_sprite_behind_it());
    }

    #[test]
    fn resolving_writes_rgba_bytes_and_uses_the_backdrop_for_untouched_pixels() {
        let palettes = MonochromePalette::new();
        let mut line = ScanlineBuffer::new(3);
        line.set(
            1,
            IndexedPixel {
                color: 3,
                palette: 0,
                priority: 0,
                source: PixelSource::Background,
            },
        );

        let backdrop = Rgba8::rgb(1, 2, 3);
        let mut row = vec![0u8; 3 * 4];
        line.resolve_into(&palettes, backdrop, &mut row);

        assert_eq!(
            &row[0..4],
            &[1, 2, 3, 255],
            "untouched pixels take the backdrop"
        );
        // Shade 3 of the default palette is the darkest.
        assert_eq!(&row[4..8], &[0, 0, 0, 255]);
        assert_eq!(&row[8..12], &[1, 2, 3, 255]);
    }

    #[test]
    fn clearing_returns_the_line_to_the_backdrop() {
        let mut line = ScanlineBuffer::new(4);
        line.set(
            2,
            IndexedPixel {
                color: 1,
                source: PixelSource::Sprite,
                ..Default::default()
            },
        );
        line.clear();
        assert_eq!(line.get(2), IndexedPixel::default());
    }
}
