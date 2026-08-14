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
//! # A masked layer never enters priority resolution
//!
//! Where a system has per-pixel regions that exclude a layer — the Game Boy Advance's windows —
//! the excluded layer must be kept out of the *contest*, not painted over after it wins. Those
//! are different pictures: hardware lets the next enabled layer down win, so a window used to
//! filter reveals what is beneath; masking after the fact reveals the backdrop instead, as
//! hard-edged rectangles of flat colour where the lower layer should be.
//!
//! That contract lives in [`ScanlineBuffer::set`], which every write path funnels through, rather
//! than in each renderer. It is deliberate: the GBA composites four backgrounds and its sprites
//! through *four* separate paths, two of them shared ([`render_text_background`],
//! [`render_sprites`]) and two of them system-specific because this crate has no notion of an
//! affine matrix. A rule enforced per-renderer is a rule the system-specific paths cannot see,
//! which is exactly how the after-the-fact masking got written in the first place. Gating the
//! single point where a pixel is committed makes it impossible for a new path to miss.
//!
//! See [`ScanlineBuffer::set_write_mask`]. Buffers are unmasked by default, so a system without
//! windows pays one predictable not-taken branch per pixel written and nothing else.
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
    background_wins, decode_tile_row, render_sprite, render_sprites, render_text_background,
    BackgroundParams, Sprite, SpritePass, SpriteRule, TilemapSource,
};
pub use palette::{bgr555_to_rgba, Bgr555Palette, MonochromePalette, PaletteSource, DMG_SHADES};

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
    /// Which layer drew this pixel, for systems whose effects are selected per layer.
    ///
    /// Distinct from `priority`, which two layers can share. The GBA's windows and colour
    /// blending both ask "which layer is this?" of the *winning* pixel, and no combination of
    /// the other fields answers it — a background at priority 2 and another at priority 2 are
    /// indistinguishable without it. Zero on systems that have only one background.
    pub layer: u8,
    pub source: PixelSource,
    /// Whether this pixel blends with what is under it whatever the blend registers say.
    ///
    /// The Game Boy Advance's semi-transparent object mode is the only thing that sets it: such a
    /// sprite is *always* a blend first target, no matter what `BLDCNT` selects, and it forces an
    /// alpha blend even where `BLDCNT` asks for a brightness effect or for none at all. That is a
    /// property of the individual pixel rather than of its layer — one sprite can be
    /// semi-transparent while another on the same line is not — so no register the effects pass
    /// could consult afterwards can answer it, and it has to travel on the pixel.
    ///
    /// False everywhere else. The Game Boy and Game Boy Color have no blending at all.
    pub forces_blend: bool,
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

    /// Which bit of a write mask this pixel's layer occupies.
    ///
    /// `None` for the backdrop, which is not a maskable layer: it is *what shows* once every
    /// layer has been masked away, so a mask can never exclude it.
    #[inline]
    pub const fn layer_bit(&self) -> Option<u8> {
        match self.source {
            PixelSource::Backdrop => None,
            PixelSource::Sprite => Some(SPRITE_LAYER_BIT),
            // Four backgrounds, so the layer number is two bits wide. Masking rather than
            // indexing keeps a malformed layer number from shifting out of range.
            PixelSource::Background => Some(1 << (self.layer & 3)),
        }
    }
}

/// The sprite layer's bit in a write mask.
///
/// Backgrounds take bits 0-3 by layer number and sprites bit 4, which is the numbering the GBA's
/// `WININ`/`WINOUT` already use — so a system whose window registers follow it can hand its
/// register value over with no translation beyond dropping the bits above.
pub const SPRITE_LAYER_BIT: u8 = 1 << 4;

/// Every layer permitted: four backgrounds and the sprites.
pub const ALL_LAYERS: u8 = 0x1F;

/// One scanline of composited, not-yet-coloured pixels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanlineBuffer {
    pixels: Vec<IndexedPixel>,
    /// Per-pixel bitmask of the layers allowed to write here, or empty when unmasked.
    ///
    /// Empty rather than `Option<Vec<_>>` so the hot path tests a length instead of unwrapping,
    /// and so a system that never uses windows carries no per-pixel state at all.
    write_mask: Vec<u8>,
}

impl ScanlineBuffer {
    pub fn new(width: usize) -> Self {
        Self {
            pixels: vec![IndexedPixel::default(); width],
            write_mask: Vec::new(),
        }
    }

    pub fn width(&self) -> usize {
        self.pixels.len()
    }

    /// Reset every pixel to the backdrop, ready for the next line.
    ///
    /// Leaves the write mask alone: it describes *this* line's windows, and a caller that sets
    /// one then clears the buffer means to draw the same line again, not to drop the mask.
    /// [`Self::clear_write_mask`] is the way to drop it.
    pub fn clear(&mut self) {
        self.pixels.fill(IndexedPixel::default());
    }

    /// Restrict which layers may write, per pixel.
    ///
    /// `mask[x]` is a bitmask of layers — backgrounds by number in bits 0-3, sprites in
    /// [`SPRITE_LAYER_BIT`] — permitted to draw at column `x`. A layer that is not permitted is
    /// excluded from priority resolution entirely rather than overpainted afterwards, so the next
    /// permitted layer down wins the pixel; see the crate docs for why that distinction is the
    /// whole point.
    ///
    /// Panics if `mask` is not exactly as wide as the buffer, because the alternative is a
    /// silently mis-windowed line, which looks like a rendering bug arbitrarily far from here.
    pub fn set_write_mask(&mut self, mask: Vec<u8>) {
        assert_eq!(
            mask.len(),
            self.pixels.len(),
            "a write mask must be exactly as wide as the scanline it masks"
        );
        self.write_mask = mask;
    }

    /// Drop any write mask, so every layer may write again.
    pub fn clear_write_mask(&mut self) {
        self.write_mask.clear();
    }

    #[inline]
    pub fn get(&self, x: usize) -> IndexedPixel {
        self.pixels[x]
    }

    /// Commit a pixel, unless a write mask excludes its layer here.
    ///
    /// This is the single point every renderer commits through — shared and system-specific
    /// alike — and therefore the only place the "a masked layer never enters priority
    /// resolution" contract needs to be enforced. See the crate docs.
    #[inline]
    pub fn set(&mut self, x: usize, pixel: IndexedPixel) {
        if !self.write_mask.is_empty() {
            // The backdrop is not maskable, so a pixel with no layer bit always commits.
            if let Some(bit) = pixel.layer_bit() {
                if self.write_mask[x] & bit == 0 {
                    return;
                }
            }
        }
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
            layer: 0,
            source: PixelSource::Background,
            ..Default::default()
        };
        assert!(opaque.hides_sprite_behind_it());

        // Index 0 is the background's transparency, so a sprite shows through.
        let transparent = IndexedPixel {
            color: 0,
            layer: 0,
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
                layer: 0,
                source: PixelSource::Background,
                forces_blend: false,
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

    /// A background pixel on `layer`, distinguishable by its colour index.
    fn bg(layer: u8, color: u8) -> IndexedPixel {
        IndexedPixel {
            color,
            palette: 0,
            priority: 0,
            layer,
            source: PixelSource::Background,
            forces_blend: false,
        }
    }

    #[test]
    fn an_unmasked_buffer_accepts_every_layer() {
        // The default, and the only path the Game Boy and Game Boy Color ever take.
        let mut line = ScanlineBuffer::new(2);
        line.set(0, bg(3, 7));
        assert_eq!(line.get(0), bg(3, 7));
    }

    #[test]
    fn a_masked_layer_never_reaches_the_buffer_so_the_next_one_down_wins() {
        // The whole contract in one assertion: layer 1 is excluded, so when layer 0 draws
        // afterwards it takes the pixel — rather than layer 1 winning and being overpainted with
        // the backdrop, which is what the GBA compositor used to do and is a different picture.
        let mut line = ScanlineBuffer::new(1);
        line.set_write_mask(vec![1 << 0]); // only background 0 may draw
        line.set(0, bg(1, 9));
        assert_eq!(
            line.get(0).source,
            PixelSource::Backdrop,
            "the masked layer did not write"
        );
        line.set(0, bg(0, 4));
        assert_eq!(line.get(0), bg(0, 4), "and the permitted layer still can");
    }

    #[test]
    fn a_mask_of_zero_leaves_the_backdrop_showing() {
        let mut line = ScanlineBuffer::new(1);
        line.set_write_mask(vec![0]);
        line.set(0, bg(0, 5));
        line.set(
            0,
            IndexedPixel {
                color: 5,
                layer: 0,
                source: PixelSource::Sprite,
                ..Default::default()
            },
        );
        assert_eq!(line.get(0).source, PixelSource::Backdrop);
    }

    #[test]
    fn sprites_are_masked_by_their_own_bit_not_a_background_one() {
        let mut line = ScanlineBuffer::new(1);
        // Every background permitted, sprites not.
        line.set_write_mask(vec![0x0F]);
        let sprite = IndexedPixel {
            color: 2,
            layer: 0,
            source: PixelSource::Sprite,
            ..Default::default()
        };
        line.set(0, sprite);
        assert_eq!(line.get(0).source, PixelSource::Backdrop, "sprite masked");

        line.set_write_mask(vec![SPRITE_LAYER_BIT]);
        line.set(0, sprite);
        assert_eq!(line.get(0), sprite, "and permitted by its own bit");
        // Background 0 shares bit 0 with nothing sprite-related, so it is still excluded.
        line.set(0, bg(0, 1));
        assert_eq!(line.get(0), sprite, "background 0 is not permitted here");
    }

    #[test]
    fn the_mask_is_per_pixel() {
        let mut line = ScanlineBuffer::new(3);
        line.set_write_mask(vec![1 << 0, 0, 1 << 0]);
        for x in 0..3 {
            line.set(x, bg(0, 6));
        }
        assert_eq!(line.get(0).color, 6);
        assert_eq!(line.get(1).source, PixelSource::Backdrop, "masked column");
        assert_eq!(line.get(2).color, 6);
    }

    #[test]
    fn layer_bits_follow_the_window_register_numbering() {
        // Backgrounds 0-3 in bits 0-3 and sprites in bit 4, so a system whose window registers
        // use that order hands its value over untranslated.
        for layer in 0..4u8 {
            assert_eq!(bg(layer, 1).layer_bit(), Some(1 << layer));
        }
        assert_eq!(
            IndexedPixel {
                source: PixelSource::Sprite,
                ..Default::default()
            }
            .layer_bit(),
            Some(SPRITE_LAYER_BIT)
        );
        assert_eq!(
            IndexedPixel::default().layer_bit(),
            None,
            "the backdrop is not a maskable layer"
        );
        assert_eq!(ALL_LAYERS, 0x1F, "four backgrounds and the sprites");
    }

    #[test]
    fn clearing_the_mask_lets_every_layer_write_again() {
        let mut line = ScanlineBuffer::new(1);
        line.set_write_mask(vec![0]);
        line.set(0, bg(0, 3));
        assert_eq!(line.get(0).source, PixelSource::Backdrop);
        line.clear_write_mask();
        line.set(0, bg(0, 3));
        assert_eq!(line.get(0).color, 3);
    }

    #[test]
    #[should_panic(expected = "exactly as wide")]
    fn a_mask_of_the_wrong_width_is_rejected_rather_than_silently_mis_windowing() {
        let mut line = ScanlineBuffer::new(4);
        line.set_write_mask(vec![0xFF; 3]);
    }

    #[test]
    fn clearing_the_line_keeps_the_mask() {
        // `clear` readies the buffer for drawing; the mask describes the line's windows, which
        // have not changed. Dropping it here would silently unmask the alpha-blend pass, which
        // clears a second buffer for the same line.
        let mut line = ScanlineBuffer::new(1);
        line.set_write_mask(vec![0]);
        line.clear();
        line.set(0, bg(0, 3));
        assert_eq!(line.get(0).source, PixelSource::Backdrop);
    }

    #[test]
    fn clearing_returns_the_line_to_the_backdrop() {
        let mut line = ScanlineBuffer::new(4);
        line.set(
            2,
            IndexedPixel {
                color: 1,
                layer: 0,
                source: PixelSource::Sprite,
                ..Default::default()
            },
        );
        line.clear();
        assert_eq!(line.get(2), IndexedPixel::default());
    }
}
