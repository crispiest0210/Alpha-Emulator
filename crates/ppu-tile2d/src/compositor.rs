//! Tile decode and scanline compositing.

use crate::{BitDepth, IndexedPixel, PixelSource, ScanlineBuffer, TileRef};

/// Decode one row of a tile into colour indices, leftmost pixel first.
///
/// `out` receives eight indices. `row_data` is the row's bytes, already sliced out by the
/// caller — [`BitDepth::row_size`] long.
///
/// The 2bpp format is the odd one: rather than packing pixels into bytes, it stores two
/// **bitplanes**, one byte each, and pixel `x` takes bit `7-x` from each. That is why a Game
/// Boy tile cannot be read as a straightforward array of pixel values.
#[inline]
pub fn decode_tile_row(row_data: &[u8], depth: BitDepth, out: &mut [u8; 8]) {
    match depth {
        BitDepth::Two => {
            let low = row_data.first().copied().unwrap_or(0);
            let high = row_data.get(1).copied().unwrap_or(0);
            for (x, pixel) in out.iter_mut().enumerate() {
                let shift = 7 - x;
                *pixel = ((low >> shift) & 1) | (((high >> shift) & 1) << 1);
            }
        }
        BitDepth::Four => {
            for (i, pair) in out.chunks_exact_mut(2).enumerate() {
                let byte = row_data.get(i).copied().unwrap_or(0);
                // The low nibble is the *left* pixel, which reads backwards but is correct.
                pair[0] = byte & 0x0F;
                pair[1] = byte >> 4;
            }
        }
        BitDepth::Eight => {
            for (x, pixel) in out.iter_mut().enumerate() {
                *pixel = row_data.get(x).copied().unwrap_or(0);
            }
        }
    }
}

/// Fetch the decoded row of a tile, honouring both flips.
#[inline]
fn tile_row_pixels(
    tile: &TileRef,
    tile_data: &[u8],
    depth: BitDepth,
    row_in_tile: u32,
    out: &mut [u8; 8],
) {
    let row = if tile.flip_y {
        7 - row_in_tile
    } else {
        row_in_tile
    };
    let offset = tile.data_offset + row as usize * depth.row_size();
    let end = offset + depth.row_size();

    if end <= tile_data.len() {
        decode_tile_row(&tile_data[offset..end], depth, out);
    } else {
        // A tile pointing outside the region reads as transparent rather than panicking; a
        // game with a mis-set base register should show garbage, not take the emulator down.
        *out = [0; 8];
    }

    if tile.flip_x {
        out.reverse();
    }
}

/// Supplies tilemap cells for a text-mode background.
///
/// Implemented by each system, because the cell formats have nothing in common: one byte on
/// the Game Boy, a byte plus an attribute byte in the second VRAM bank on the GBC, and a
/// 16-bit word on the GBA.
pub trait TilemapSource {
    /// The cell at map coordinates `(tile_x, tile_y)`, which the caller has already wrapped
    /// into the map's dimensions.
    fn tile_at(&self, tile_x: u32, tile_y: u32) -> TileRef;
}

/// Everything a text-mode background scanline needs beyond its tilemap.
#[derive(Debug, Clone, Copy)]
pub struct BackgroundParams {
    /// The screen line being drawn.
    pub line: u32,
    /// Scroll offsets, in pixels.
    pub scroll_x: u32,
    pub scroll_y: u32,
    /// Map dimensions in tiles. The map wraps at these bounds.
    pub map_width: u32,
    pub map_height: u32,
    pub depth: BitDepth,
    /// Which layer this is, recorded on every pixel it draws. Zero for a system with one
    /// background; see [`IndexedPixel::layer`](crate::IndexedPixel::layer).
    pub layer: u8,
    /// Leftmost screen pixel to draw. Used by the Game Boy's window, which starts partway
    /// across the line.
    pub start_x: usize,
    /// Screen-space offset subtracted before scrolling, again for the window.
    pub origin_x: u32,
    /// Whether colour index 0 means "transparent" rather than a colour.
    ///
    /// The two machines genuinely disagree, so this is a parameter rather than a rule. A Game Boy
    /// Advance background is one of four *layers*: index 0 lets whatever is behind show through,
    /// and the backdrop shows where nothing covers. A Game Boy background is the bottom of the
    /// picture with nothing behind it, index 0 is an ordinary shade, and sprite priority is
    /// decided by comparing against it — so writing it is required there.
    ///
    /// Getting this wrong on the GBA does not lose a pixel here and there. The frontmost enabled
    /// text layer becomes opaque across the whole screen and hides every layer behind it, which
    /// looks like large flat bands of one palette colour over the real picture — worst on menus
    /// and text boxes, where the front layer is mostly empty.
    pub transparent_index_zero: bool,
}

impl BackgroundParams {
    /// A whole-line background with no window offset.
    pub fn full_line(line: u32, scroll_x: u32, scroll_y: u32, depth: BitDepth) -> Self {
        Self {
            line,
            scroll_x,
            scroll_y,
            map_width: 32,
            map_height: 32,
            depth,
            // The Game Boy's rule, because it is the older machine and the one whose background
            // is the bottom of the picture. A layered system opts in.
            transparent_index_zero: false,
            layer: 0,
            start_x: 0,
            origin_x: 0,
        }
    }
}

/// Composite one scanline of a text-mode (non-affine) background.
///
/// Writes every pixel it covers, including index 0 — a background has no transparency of its
/// own, and index 0 must survive into the buffer because sprite priority is decided against
/// it.
pub fn render_text_background<M: TilemapSource>(
    map: &M,
    tile_data: &[u8],
    params: &BackgroundParams,
    out: &mut ScanlineBuffer,
) {
    let map_pixel_height = params.map_height * 8;
    let map_pixel_width = params.map_width * 8;
    if map_pixel_width == 0 || map_pixel_height == 0 {
        return;
    }

    let source_y = (params.line.wrapping_add(params.scroll_y)) % map_pixel_height;
    let tile_y = source_y / 8;
    let row_in_tile = source_y % 8;

    let mut pixels = [0u8; 8];
    // Track which tile is loaded so a run of pixels inside one tile decodes its row once
    // rather than eight times.
    let mut loaded_tile_x = u32::MAX;
    let mut tile = TileRef::default();

    for x in params.start_x..out.width() {
        let screen_x = x as u32 - params.origin_x.min(x as u32);
        let source_x = (screen_x.wrapping_add(params.scroll_x)) % map_pixel_width;
        let tile_x = source_x / 8;

        if tile_x != loaded_tile_x {
            tile = map.tile_at(tile_x, tile_y);
            tile_row_pixels(&tile, tile_data, params.depth, row_in_tile, &mut pixels);
            loaded_tile_x = tile_x;
        }

        let color = pixels[(source_x % 8) as usize];
        if color == 0 && params.transparent_index_zero {
            continue;
        }
        out.set(
            x,
            IndexedPixel {
                color,
                palette: tile.palette,
                priority: tile.priority,
                layer: params.layer,
                source: PixelSource::Background,
                // Only a sprite can force a blend; a background is selected by `BLDCNT` or not.
                forces_blend: false,
            },
        );
    }
}

/// One sprite, normalized out of whatever the system's OAM looks like.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sprite {
    /// Screen position of the top-left corner. Signed so a sprite can hang off either edge.
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    /// Byte offset of the sprite's first tile within the tile-data slice.
    pub tile_offset: usize,
    pub palette: u8,
    pub flip_x: bool,
    pub flip_y: bool,
    /// Hidden wherever the background pixel is not index 0.
    pub behind_background: bool,
    /// Bytes between one row of the sprite's tiles and the next.
    ///
    /// Zero means "the rows are contiguous", which is the Game Boy's only arrangement and the
    /// GBA's one-dimensional mapping. The GBA can instead treat its whole object area as a
    /// 32-tile-wide sheet that each sprite is a window onto, and then the next row is a fixed
    /// distance away regardless of the sprite's width — hence a field rather than a derivation.
    pub row_stride: usize,
    /// This sprite's priority, 0 nearest. Compared against the background pixel's under
    /// [`SpriteRule::ByPriority`]; ignored by the Game Boy, which has one sprite plane.
    pub priority: u8,
    /// How many bits each of this sprite's pixels takes.
    ///
    /// Per sprite rather than per call, because on the Game Boy Advance it genuinely varies: bit 13
    /// of an OAM entry's first attribute selects 16 or 256 colours, and one scanline can hold both.
    /// It was a parameter of [`render_sprites`], every GBA sprite was rendered as 16-colour, and a
    /// 256-colour one came out as a stretched checkerboard — one byte read as two indices — which
    /// looks like a corrupt tile rather than like an unimplemented feature.
    pub depth: BitDepth,
    /// Whether this sprite's pixels blend with what is under them whatever the blend registers
    /// say. See [`IndexedPixel::forces_blend`](crate::IndexedPixel::forces_blend), which it is
    /// copied onto.
    pub forces_blend: bool,
}

/// The claimed-pixel state one scanline's sprite compositing carries from sprite to sprite.
///
/// Sprites are offered front-to-back and the first to claim a pixel keeps it — whether or not it
/// ends up *visible*, because a sprite that loses to the background has still claimed the pixel
/// and a farther sprite must not show through the hole. That rule, and the priority comparison
/// against the background, are the whole of sprite compositing beyond decoding tiles.
///
/// # Why the state is out here rather than inside a renderer
///
/// A system can have more than one sprite renderer. The Game Boy Advance has two: ordinary sprites
/// go through [`render_sprite`], and rotated or scaled ones it draws itself, because this crate has
/// no notion of an affine matrix. With the claim state private to [`render_sprites`], those two
/// could not see each other — so the affine path wrote pixels unconditionally, ignoring background
/// priority entirely, and every ordinary sprite then overwrote every affine one regardless of which
/// was in front. A rotating object punched through the text box before it, and a farther plain
/// sprite erased a nearer rotated one.
///
/// Holding the state here lets both renderers share one ordered pass, so they compete under one
/// rule. It is the same reasoning that puts the window mask on [`ScanlineBuffer`] rather than in
/// each background renderer; see the crate docs.
pub struct SpritePass {
    /// Which pixels a nearer sprite has already claimed.
    claimed: Vec<bool>,
    rule: SpriteRule,
}

impl SpritePass {
    pub fn new(width: usize, rule: SpriteRule) -> Self {
        Self {
            claimed: vec![false; width],
            rule,
        }
    }

    /// Offer one sprite pixel, which must already be opaque.
    ///
    /// Returns whether this sprite claimed the pixel. A `false` return means a nearer sprite got
    /// there first; a `true` return does *not* mean the pixel is visible, because the background
    /// may still cover it — the claim is what stops a farther sprite trying.
    ///
    /// `behind_background` is the sprite's own "behind background" flag, which only the Game Boy
    /// rules consult; the GBA compares the two priorities instead and leaves it false.
    pub fn place(
        &mut self,
        out: &mut ScanlineBuffer,
        x: usize,
        pixel: IndexedPixel,
        behind_background: bool,
    ) -> bool {
        if x >= self.claimed.len() || self.claimed[x] {
            return false;
        }
        self.claimed[x] = true;

        let background = out.get(x);
        let covered = background.source == PixelSource::Background
            && match self.rule {
                // A transparent background pixel never covers anything, whatever the priorities
                // say.
                SpriteRule::ByPriority => {
                    background.color != 0 && pixel.priority > background.priority
                }
                rule => background_wins(
                    rule,
                    // `TileRef::priority` counts lower as nearer, so zero is the tile asking to be
                    // drawn in front.
                    background.priority == 0,
                    behind_background,
                    background.color,
                ),
            };
        if !covered {
            out.set(x, pixel);
        }
        true
    }
}

/// Composite one scanline of sprites.
///
/// `sprites` must already be in priority order, front-most first, and already filtered to
/// those on this line. Both are the system's job: the per-line sprite limit and the
/// tie-breaking rule differ between them — a DMG breaks ties by X coordinate while a GBC uses
/// OAM index — and neither belongs in shared code.
///
/// Colour index 0 is transparent and never written, which is what lets sprites overlap
/// without punching holes in each other.
/// How a sprite pixel competes with the background pixel underneath it.
///
/// The two machines disagree about who is allowed to enter the contest, not about how to
/// resolve it, which is why this is a parameter rather than a second renderer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpriteRule {
    /// Compare the two priorities: the sprite is in front where its own is less than or equal to
    /// the background pixel's. The Game Boy Advance's rule, and the reason a sprite carries a
    /// priority at all.
    ///
    /// Without this every sprite won against every background, so a character walked *over* the
    /// text box in front of them rather than behind it.
    ByPriority,
    /// The DMG's rule: only the sprite gets an opinion. It loses where its own "behind
    /// background" bit is set and the background pixel is opaque.
    SpriteDecides,
    /// The CGB's rule: the background *tile* can also demand to be in front, and `LCDC` bit 0
    /// can wave the whole contest away. `master_priority` is that bit — clearing it is how a
    /// game forces every sprite to the front for a cutscene without editing its tile maps.
    SpriteOrTileDecides { master_priority: bool },
}

/// Whether the background pixel covers the sprite pixel.
///
/// Split out from the compositing loop because it is the rule most easily got wrong and the
/// most expensive to get wrong — it is the difference between a character walking behind
/// scenery and through it — so it is worth testing on its own rather than only through a
/// rendered scanline.
///
/// A background colour index of zero is always behind the sprite whatever any priority bit
/// says: index zero is the transparent one, and no priority setting makes a hole opaque.
#[inline]
pub fn background_wins(
    rule: SpriteRule,
    tile_asks_for_priority: bool,
    sprite_yields: bool,
    background_colour: u8,
) -> bool {
    if background_colour == 0 {
        return false;
    }
    match rule {
        // Answered by the caller, which has both priorities; this entry point only sees one bit
        // of each and cannot compare them.
        SpriteRule::ByPriority => false,
        SpriteRule::SpriteDecides => sprite_yields,
        SpriteRule::SpriteOrTileDecides { master_priority } => {
            master_priority && (tile_asks_for_priority || sprite_yields)
        }
    }
}

pub fn render_sprites(
    sprites: &[Sprite],
    tile_data: &[u8],
    line: u32,
    rule: SpriteRule,
    out: &mut ScanlineBuffer,
) {
    let mut pass = SpritePass::new(out.width(), rule);
    for sprite in sprites {
        render_sprite(sprite, tile_data, line, &mut pass, out);
    }
}

/// Composite one sprite into a pass already in progress.
///
/// Split out of [`render_sprites`] so a system with a second sprite renderer of its own can
/// interleave the two in one front-to-back pass and have them compete under one rule — see
/// [`SpritePass`]. Callers must offer sprites front-most first; the pass enforces the claim, not
/// the ordering.
///
/// Colour index 0 is transparent and never written, which is what lets sprites overlap without
/// punching holes in each other.
pub fn render_sprite(
    sprite: &Sprite,
    tile_data: &[u8],
    line: u32,
    pass: &mut SpritePass,
    out: &mut ScanlineBuffer,
) {
    let row = line as i32 - sprite.y;
    if row < 0 || row >= sprite.height as i32 {
        return;
    }
    let row = if sprite.flip_y {
        sprite.height - 1 - row as u32
    } else {
        row as u32
    };

    // Tall sprites are stacked 8x8 tiles, so the row selects which tile as well as which
    // row within it.
    let tile_index_in_sprite = row / 8;
    let row_in_tile = row % 8;

    let mut pixels = [0u8; 8];
    let depth = sprite.depth;
    for tile_column in 0..(sprite.width / 8) {
        // A horizontal flip reverses which tile column appears where, not just the
        // pixels inside each one.
        let source_column = if sprite.flip_x {
            sprite.width / 8 - 1 - tile_column
        } else {
            tile_column
        };
        let row_stride = match sprite.row_stride {
            0 => (sprite.width / 8) as usize * depth.tile_size(),
            stride => stride,
        };
        let tile = TileRef {
            data_offset: sprite.tile_offset
                + tile_index_in_sprite as usize * row_stride
                + source_column as usize * depth.tile_size(),
            palette: sprite.palette,
            flip_x: sprite.flip_x,
            // The row was already flipped above, so the tile fetch must not flip again.
            flip_y: false,
            priority: 0,
        };
        tile_row_pixels(&tile, tile_data, depth, row_in_tile, &mut pixels);

        for (pixel_x, &color) in pixels.iter().enumerate() {
            if color == 0 {
                continue; // transparent
            }
            let screen_x = sprite.x + (tile_column * 8) as i32 + pixel_x as i32;
            if screen_x < 0 || screen_x as usize >= out.width() {
                continue;
            }
            pass.place(
                out,
                screen_x as usize,
                IndexedPixel {
                    color,
                    palette: sprite.palette,
                    // The sprite's own priority, which the pass compares against the background's.
                    // It was written as a flat zero here, which threw away the one value a second
                    // sprite renderer would need to compete on equal terms.
                    priority: sprite.priority,
                    layer: 0,
                    source: PixelSource::Sprite,
                    forces_blend: sprite.forces_blend,
                },
                sprite.behind_background,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 2bpp tile whose eight rows read 0,1,2,3,0,1,2,3 across every column.
    fn striped_2bpp_tile() -> Vec<u8> {
        let mut tile = vec![0u8; 16];
        for row in 0..8 {
            // Columns 0-1 index 0, 2-3 index 1, 4-5 index 2, 6-7 index 3.
            tile[row * 2] = 0b0011_0011; // low bitplane
            tile[row * 2 + 1] = 0b0000_1111; // high bitplane
        }
        tile
    }

    #[test]
    fn two_bpp_decodes_from_bitplanes_not_packed_pixels() {
        let tile = striped_2bpp_tile();
        let mut out = [0u8; 8];
        decode_tile_row(&tile[0..2], BitDepth::Two, &mut out);
        assert_eq!(out, [0, 0, 1, 1, 2, 2, 3, 3]);
    }

    #[test]
    fn two_bpp_takes_the_leftmost_pixel_from_the_high_bit() {
        // A single set bit in the low plane at bit 7 is the *left* pixel.
        let mut out = [0u8; 8];
        decode_tile_row(&[0b1000_0000, 0x00], BitDepth::Two, &mut out);
        assert_eq!(out, [1, 0, 0, 0, 0, 0, 0, 0]);

        decode_tile_row(&[0x00, 0b0000_0001], BitDepth::Two, &mut out);
        assert_eq!(out, [0, 0, 0, 0, 0, 0, 0, 2]);
    }

    #[test]
    fn four_bpp_puts_the_low_nibble_on_the_left() {
        let mut out = [0u8; 8];
        decode_tile_row(&[0x21, 0x43, 0x65, 0x87], BitDepth::Four, &mut out);
        assert_eq!(out, [1, 2, 3, 4, 5, 6, 7, 8]);
    }

    #[test]
    fn eight_bpp_is_one_byte_per_pixel() {
        let mut out = [0u8; 8];
        decode_tile_row(&[1, 2, 3, 4, 5, 6, 7, 8], BitDepth::Eight, &mut out);
        assert_eq!(out, [1, 2, 3, 4, 5, 6, 7, 8]);
    }

    #[test]
    fn a_short_row_decodes_as_transparent_rather_than_panicking() {
        let mut out = [0u8; 8];
        decode_tile_row(&[], BitDepth::Two, &mut out);
        assert_eq!(out, [0; 8]);
    }

    // -- Backgrounds ---------------------------------------------------------

    /// A tilemap where every cell points at tile 0, with configurable flips.
    struct UniformMap {
        tile: TileRef,
    }

    impl TilemapSource for UniformMap {
        fn tile_at(&self, _tile_x: u32, _tile_y: u32) -> TileRef {
            self.tile
        }
    }

    /// A tilemap that returns a different tile per column, for testing scroll.
    struct ColumnMap;

    impl TilemapSource for ColumnMap {
        fn tile_at(&self, tile_x: u32, _tile_y: u32) -> TileRef {
            TileRef {
                data_offset: (tile_x as usize % 2) * 16,
                ..Default::default()
            }
        }
    }

    fn colors_of(line: &ScanlineBuffer) -> Vec<u8> {
        (0..line.width()).map(|x| line.get(x).color).collect()
    }

    #[test]
    fn a_background_scanline_repeats_its_tile_across_the_line() {
        let tile = striped_2bpp_tile();
        let map = UniformMap {
            tile: TileRef::default(),
        };
        let mut line = ScanlineBuffer::new(16);
        render_text_background(
            &map,
            &tile,
            &BackgroundParams::full_line(0, 0, 0, BitDepth::Two),
            &mut line,
        );
        assert_eq!(
            colors_of(&line),
            vec![0, 0, 1, 1, 2, 2, 3, 3, 0, 0, 1, 1, 2, 2, 3, 3]
        );
    }

    #[test]
    fn horizontal_scroll_shifts_the_line_within_the_tile() {
        let tile = striped_2bpp_tile();
        let map = UniformMap {
            tile: TileRef::default(),
        };
        let mut line = ScanlineBuffer::new(8);
        render_text_background(
            &map,
            &tile,
            &BackgroundParams::full_line(0, 2, 0, BitDepth::Two),
            &mut line,
        );
        // Scrolling right by two starts the line two pixels into the pattern.
        assert_eq!(colors_of(&line), vec![1, 1, 2, 2, 3, 3, 0, 0]);
    }

    #[test]
    fn the_background_wraps_at_the_map_edge() {
        // Scrolling past the end of a 32-tile map comes back to the start rather than
        // reading off the end.
        let mut tiles = vec![0u8; 32];
        tiles[0..16].copy_from_slice(&striped_2bpp_tile());
        // Tile 1 is solid index 3.
        for row in 0..8 {
            tiles[16 + row * 2] = 0xFF;
            tiles[16 + row * 2 + 1] = 0xFF;
        }

        let mut line = ScanlineBuffer::new(8);
        let mut params = BackgroundParams::full_line(0, 0, 0, BitDepth::Two);
        // Start one pixel before the map wraps: tile 31 then tile 0.
        params.scroll_x = 32 * 8 - 4;
        render_text_background(&ColumnMap, &tiles, &params, &mut line);

        // Tile 31 is odd so it uses tile data 1 (solid), tile 0 is even (striped).
        assert_eq!(colors_of(&line), vec![3, 3, 3, 3, 0, 0, 1, 1]);
    }

    #[test]
    fn vertical_scroll_selects_the_row_within_the_tile() {
        // A tile whose rows each hold their own row number.
        let mut tile = vec![0u8; 16];
        for row in 0..8u8 {
            tile[row as usize * 2] = if row & 1 != 0 { 0xFF } else { 0 };
            tile[row as usize * 2 + 1] = if row & 2 != 0 { 0xFF } else { 0 };
        }
        let map = UniformMap {
            tile: TileRef::default(),
        };

        for (scroll_y, expected) in [(0u32, 0u8), (1, 1), (2, 2), (3, 3), (4, 0)] {
            let mut line = ScanlineBuffer::new(8);
            render_text_background(
                &map,
                &tile,
                &BackgroundParams::full_line(0, 0, scroll_y, BitDepth::Two),
                &mut line,
            );
            assert_eq!(line.get(0).color, expected, "scroll_y {scroll_y}");
        }
    }

    #[test]
    fn tile_flips_mirror_the_fetched_row() {
        let tile = striped_2bpp_tile();

        let mut line = ScanlineBuffer::new(8);
        render_text_background(
            &UniformMap {
                tile: TileRef {
                    flip_x: true,
                    ..Default::default()
                },
            },
            &tile,
            &BackgroundParams::full_line(0, 0, 0, BitDepth::Two),
            &mut line,
        );
        assert_eq!(colors_of(&line), vec![3, 3, 2, 2, 1, 1, 0, 0]);
    }

    #[test]
    fn a_background_writes_index_zero_so_sprite_priority_can_see_it() {
        let tile = striped_2bpp_tile();
        let mut line = ScanlineBuffer::new(8);
        render_text_background(
            &UniformMap {
                tile: TileRef::default(),
            },
            &tile,
            &BackgroundParams::full_line(0, 0, 0, BitDepth::Two),
            &mut line,
        );
        // Pixel 0 is colour index 0, but it is a Background pixel, not the backdrop.
        assert_eq!(line.get(0).color, 0);
        assert_eq!(line.get(0).source, PixelSource::Background);
    }

    #[test]
    fn a_window_style_background_starts_partway_across_the_line() {
        let tile = striped_2bpp_tile();
        let mut line = ScanlineBuffer::new(16);
        let mut params = BackgroundParams::full_line(0, 0, 0, BitDepth::Two);
        params.start_x = 8;
        params.origin_x = 8;
        render_text_background(
            &UniformMap {
                tile: TileRef::default(),
            },
            &tile,
            &params,
            &mut line,
        );

        // Nothing before the start.
        for x in 0..8 {
            assert_eq!(line.get(x).source, PixelSource::Backdrop, "x {x}");
        }
        // And the pattern begins at its own origin rather than mid-tile.
        assert_eq!(
            colors_of(&line)[8..],
            [0, 0, 1, 1, 2, 2, 3, 3],
            "the window starts at the left of its own tile"
        );
    }

    // -- Sprites -------------------------------------------------------------

    /// An 8x8 tile that is solid colour index 1.
    fn solid_tile(color: u8) -> Vec<u8> {
        let mut tile = vec![0u8; 16];
        for row in 0..8 {
            tile[row * 2] = if color & 1 != 0 { 0xFF } else { 0 };
            tile[row * 2 + 1] = if color & 2 != 0 { 0xFF } else { 0 };
        }
        tile
    }

    fn sprite_at(x: i32, y: i32) -> Sprite {
        Sprite {
            depth: BitDepth::Two,
            priority: 0,
            x,
            y,
            width: 8,
            height: 8,
            tile_offset: 0,
            palette: 0,
            flip_x: false,
            flip_y: false,
            behind_background: false,
            row_stride: 0,
            forces_blend: false,
        }
    }

    #[test]
    fn a_sprite_draws_only_where_it_covers_the_line() {
        let tile = solid_tile(1);
        let mut line = ScanlineBuffer::new(16);
        render_sprites(
            &[sprite_at(4, 0)],
            &tile,
            0,
            SpriteRule::SpriteDecides,
            &mut line,
        );

        for x in 0..4 {
            assert_eq!(line.get(x).source, PixelSource::Backdrop);
        }
        for x in 4..12 {
            assert_eq!(line.get(x).source, PixelSource::Sprite, "x {x}");
            assert_eq!(line.get(x).color, 1);
        }
        for x in 12..16 {
            assert_eq!(line.get(x).source, PixelSource::Backdrop);
        }
    }

    #[test]
    fn a_sprite_off_this_line_draws_nothing() {
        let tile = solid_tile(1);
        let mut line = ScanlineBuffer::new(16);
        render_sprites(
            &[sprite_at(0, 8)],
            &tile,
            0,
            SpriteRule::SpriteDecides,
            &mut line,
        );
        assert!((0..16).all(|x| line.get(x).source == PixelSource::Backdrop));
    }

    #[test]
    fn a_sprite_hanging_off_the_edge_is_clipped_not_wrapped() {
        let tile = solid_tile(1);
        let mut line = ScanlineBuffer::new(16);
        render_sprites(
            &[sprite_at(-4, 0)],
            &tile,
            0,
            SpriteRule::SpriteDecides,
            &mut line,
        );
        for x in 0..4 {
            assert_eq!(line.get(x).source, PixelSource::Sprite, "x {x}");
        }
        for x in 4..16 {
            assert_eq!(line.get(x).source, PixelSource::Backdrop, "x {x}");
        }
    }

    #[test]
    fn transparent_sprite_pixels_leave_what_is_underneath() {
        // Colour index 0 within a sprite is transparent, which is what lets sprites be any
        // shape at all.
        let tile = striped_2bpp_tile(); // columns 0-1 are index 0
        let mut line = ScanlineBuffer::new(8);
        render_sprites(
            &[sprite_at(0, 0)],
            &tile,
            0,
            SpriteRule::SpriteDecides,
            &mut line,
        );
        assert_eq!(line.get(0).source, PixelSource::Backdrop);
        assert_eq!(line.get(1).source, PixelSource::Backdrop);
        assert_eq!(line.get(2).source, PixelSource::Sprite);
    }

    #[test]
    fn the_first_sprite_in_the_list_wins_an_overlap() {
        // Callers pass sprites front-most first, so a later one cannot paint over an earlier.
        let mut tiles = solid_tile(1);
        tiles.extend(solid_tile(3));

        let front = Sprite {
            palette: 0,
            ..sprite_at(0, 0)
        };
        let behind = Sprite {
            tile_offset: 16,
            palette: 1,
            ..sprite_at(4, 0)
        };

        let mut line = ScanlineBuffer::new(16);
        render_sprites(
            &[front, behind],
            &tiles,
            0,
            SpriteRule::SpriteDecides,
            &mut line,
        );

        // Where they overlap, the front sprite's colour and palette survive.
        for x in 0..8 {
            assert_eq!(line.get(x).color, 1, "x {x}");
            assert_eq!(line.get(x).palette, 0, "x {x}");
        }
        // And the one behind still draws where it is not covered.
        for x in 8..12 {
            assert_eq!(line.get(x).color, 3, "x {x}");
            assert_eq!(line.get(x).palette, 1, "x {x}");
        }
    }

    #[test]
    fn a_behind_background_sprite_shows_through_index_zero_only() {
        let bg_tile = striped_2bpp_tile(); // columns 0-1 index 0, the rest non-zero
        let sprite_tile = solid_tile(2);

        let mut line = ScanlineBuffer::new(8);
        render_text_background(
            &UniformMap {
                tile: TileRef::default(),
            },
            &bg_tile,
            &BackgroundParams::full_line(0, 0, 0, BitDepth::Two),
            &mut line,
        );
        render_sprites(
            &[Sprite {
                behind_background: true,
                ..sprite_at(0, 0)
            }],
            &sprite_tile,
            0,
            SpriteRule::SpriteDecides,
            &mut line,
        );

        // Shows through where the background is index 0.
        assert_eq!(line.get(0).source, PixelSource::Sprite);
        assert_eq!(line.get(1).source, PixelSource::Sprite);
        // Hidden everywhere the background is opaque.
        for x in 2..8 {
            assert_eq!(line.get(x).source, PixelSource::Background, "x {x}");
        }
    }

    #[test]
    fn a_hidden_behind_background_sprite_still_blocks_the_one_behind_it() {
        // The nearer sprite loses to the background but keeps the pixel: a farther sprite
        // does not get to appear in the gap.
        let bg_tile = solid_tile(3);
        let mut tiles = solid_tile(1);
        tiles.extend(solid_tile(2));

        let mut line = ScanlineBuffer::new(8);
        render_text_background(
            &UniformMap {
                tile: TileRef::default(),
            },
            &bg_tile,
            &BackgroundParams::full_line(0, 0, 0, BitDepth::Two),
            &mut line,
        );
        render_sprites(
            &[
                Sprite {
                    behind_background: true,
                    ..sprite_at(0, 0)
                },
                Sprite {
                    tile_offset: 16,
                    ..sprite_at(0, 0)
                },
            ],
            &tiles,
            0,
            SpriteRule::SpriteDecides,
            &mut line,
        );

        for x in 0..8 {
            assert_eq!(
                line.get(x).source,
                PixelSource::Background,
                "the background still shows at x {x}"
            );
        }
    }

    #[test]
    fn a_tall_sprite_selects_its_second_tile_on_the_lower_half() {
        let mut tiles = solid_tile(1);
        tiles.extend(solid_tile(3));

        let tall = Sprite {
            height: 16,
            ..sprite_at(0, 0)
        };

        let mut top = ScanlineBuffer::new(8);
        render_sprites(&[tall], &tiles, 3, SpriteRule::SpriteDecides, &mut top);
        assert_eq!(top.get(0).color, 1, "the upper tile");

        let mut bottom = ScanlineBuffer::new(8);
        render_sprites(&[tall], &tiles, 11, SpriteRule::SpriteDecides, &mut bottom);
        assert_eq!(bottom.get(0).color, 3, "the lower tile");
    }

    #[test]
    fn flipping_a_tall_sprite_swaps_its_tiles_as_well_as_its_rows() {
        let mut tiles = solid_tile(1);
        tiles.extend(solid_tile(3));

        let flipped = Sprite {
            height: 16,
            flip_y: true,
            ..sprite_at(0, 0)
        };

        let mut top = ScanlineBuffer::new(8);
        render_sprites(&[flipped], &tiles, 3, SpriteRule::SpriteDecides, &mut top);
        assert_eq!(top.get(0).color, 3, "the lower tile is now on top");
    }

    #[test]
    fn a_horizontally_flipped_wide_sprite_reverses_its_tile_columns() {
        // Two 8x8 tiles side by side, distinguishable from each other.
        let mut tiles = solid_tile(1);
        tiles.extend(solid_tile(3));

        let wide = Sprite {
            width: 16,
            flip_x: true,
            ..sprite_at(0, 0)
        };

        let mut line = ScanlineBuffer::new(16);
        render_sprites(&[wide], &tiles, 0, SpriteRule::SpriteDecides, &mut line);
        // Unflipped this would be tile 0 then tile 1; flipped it is the other way round.
        assert_eq!(line.get(0).color, 3);
        assert_eq!(line.get(8).color, 1);
    }

    // -- The sprite/background priority rule -------------------------------

    #[test]
    fn a_transparent_background_pixel_never_covers_a_sprite() {
        // Colour zero is the transparent index. No priority bit makes a hole opaque, so this
        // holds under both machines' rules and for every combination of the other inputs.
        for rule in [
            SpriteRule::SpriteDecides,
            SpriteRule::SpriteOrTileDecides {
                master_priority: true,
            },
        ] {
            for tile in [false, true] {
                for sprite in [false, true] {
                    assert!(
                        !background_wins(rule, tile, sprite, 0),
                        "colour 0 covered a sprite: {rule:?} tile={tile} sprite={sprite}"
                    );
                }
            }
        }
    }

    #[test]
    fn under_the_dmg_rule_only_the_sprite_gets_an_opinion() {
        let rule = SpriteRule::SpriteDecides;
        assert!(background_wins(rule, false, true, 1), "the sprite yielded");
        assert!(!background_wins(rule, false, false, 1));
        // A tile asking for priority is ignored: a DMG tile map has nowhere to ask.
        assert!(!background_wins(rule, true, false, 1));
    }

    #[test]
    fn under_the_cgb_rule_either_side_can_put_the_background_in_front() {
        let rule = SpriteRule::SpriteOrTileDecides {
            master_priority: true,
        };
        assert!(background_wins(rule, true, false, 1), "the tile asked");
        assert!(background_wins(rule, false, true, 1), "the sprite yielded");
        assert!(background_wins(rule, true, true, 1), "both");
        assert!(
            !background_wins(rule, false, false, 1),
            "neither, so the sprite is in front"
        );
    }

    #[test]
    fn clearing_master_priority_forces_every_sprite_to_the_front() {
        // On a CGB, `LCDC` bit 0 keeps its position and changes its job. This is how a game
        // puts sprites over everything for a cutscene without editing its tile maps.
        let rule = SpriteRule::SpriteOrTileDecides {
            master_priority: false,
        };
        for tile in [false, true] {
            for sprite in [false, true] {
                for colour in 1..4 {
                    assert!(
                        !background_wins(rule, tile, sprite, colour),
                        "background won with master priority off"
                    );
                }
            }
        }
    }

    #[test]
    fn a_zero_row_stride_means_the_rows_are_contiguous() {
        // The Game Boy's only arrangement, and the GBA's one-dimensional mapping. Spelling it as
        // zero rather than making every caller compute it keeps the common case honest.
        let mut tile = vec![0u8; 0x400];
        // A 16-wide sprite is two tiles per row, so its second row starts at tile 2.
        tile[2 * 32] = 0xFF;

        let mut line = ScanlineBuffer::new(32);
        let sprite = Sprite {
            width: 16,
            height: 16,
            depth: BitDepth::Four,
            ..sprite_at(0, 0)
        };
        render_sprites(&[sprite], &tile, 8, SpriteRule::SpriteDecides, &mut line);
        assert_ne!(line.get(0).color, 0, "row 8 came from tile 2");
    }

    #[test]
    fn a_row_stride_puts_the_next_row_a_fixed_distance_away() {
        // The GBA's two-dimensional mapping: the object area is one 32-tile-wide sheet and the
        // sprite is a window onto it, so the next row is 32 tiles on whatever the sprite's width.
        let mut tile = vec![0u8; 0x4000];
        tile[32 * 32] = 0xFF;

        let mut line = ScanlineBuffer::new(32);
        let sprite = Sprite {
            width: 16,
            height: 16,
            row_stride: 32 * 32,
            depth: BitDepth::Four,
            ..sprite_at(0, 0)
        };
        render_sprites(&[sprite], &tile, 8, SpriteRule::SpriteDecides, &mut line);
        assert_ne!(line.get(0).color, 0, "row 8 came from 32 tiles on");
    }
}

#[cfg(test)]
mod transparency_tests {
    use super::*;

    fn params(transparent: bool) -> BackgroundParams {
        BackgroundParams {
            transparent_index_zero: transparent,
            ..BackgroundParams::full_line(0, 0, 0, BitDepth::Two)
        }
    }

    struct OneTile;
    impl TilemapSource for OneTile {
        fn tile_at(&self, _x: u32, _y: u32) -> TileRef {
            TileRef::default()
        }
    }

    #[test]
    fn the_two_machines_disagree_about_colour_zero_and_both_are_right() {
        // A Game Boy Advance background is a layer over something else, so index 0 lets it show
        // through. A Game Boy background is the bottom of the picture, index 0 is an ordinary
        // shade, and sprite priority is decided by comparing against it — so it must be written.
        let tile = vec![0u8; 16]; // every pixel index 0

        let mut layered = ScanlineBuffer::new(8);
        layered.clear();
        render_text_background(&OneTile, &tile, &params(true), &mut layered);
        assert_eq!(
            layered.get(0).source,
            PixelSource::Backdrop,
            "nothing written, so whatever is behind stays"
        );

        let mut flat = ScanlineBuffer::new(8);
        flat.clear();
        render_text_background(&OneTile, &tile, &params(false), &mut flat);
        assert_eq!(
            flat.get(0).source,
            PixelSource::Background,
            "the Game Boy writes it as a colour"
        );
    }
}
