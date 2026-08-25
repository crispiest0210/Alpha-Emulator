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
    render_text_background, BackgroundParams, BitDepth, PaletteSource, PixelSource, ScanlineBuffer,
    SpritePass, SpriteRule,
};

use crate::affine::transform_object_pixel;
use crate::affine::AffineBackground;
use crate::background::{Backgrounds, GbaTilemap};
use crate::bitmap;
use crate::effects::{BlendMode, Effects, Layer};
use crate::objects::{Object, ObjectAttributeMemory, ObjectMode};
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
    pub effects: &'a Effects,
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

    let kinds = layers_for_mode(mode);
    let enabled = [
        frame.video.dispcnt & (1 << 8) != 0,
        frame.video.dispcnt & (1 << 9) != 0,
        frame.video.dispcnt & (1 << 10) != 0,
        frame.video.dispcnt & (1 << 11) != 0,
    ];
    let present = std::array::from_fn(|i| enabled[i] && kinds[i].is_some());

    // Both computed before anything composites, because a window decides which layers may *enter*
    // priority resolution rather than what happens to the winner afterwards. The object window's
    // own shape comes from a scratch render that is deliberately left unmasked — it is computing
    // the mask, so it cannot be subject to it.
    let object_window = object_window_mask(frame, line);
    let visible = window_mask(frame, line, object_window.as_deref());
    if let Some(visible) = &visible {
        scanline.set_write_mask(layer_bits(visible));
    }

    for index in frame.backgrounds.draw_order(present) {
        match kinds[index] {
            Some(LayerKind::Text) => draw_text_layer(frame, index, line, &mut scanline),
            Some(LayerKind::Affine) => draw_affine_layer(frame, index, &mut scanline),
            Some(LayerKind::Bitmap) => draw_bitmap_layer(frame, mode, &mut scanline),
            None => {}
        }
    }
    draw_sprites(frame, line, SpriteSelection::Drawn, &mut scanline);

    // What is *underneath* the winning pixel, for an alpha blend. Composed as a second pass that
    // excludes, at each pixel, exactly the layer *that pixel's own winner* came from — not every
    // layer `BLDCNT` declares a first target, which is a different and narrower question.
    // Hardware picks the top two priority slots among BG0-3 and OBJ and blends the top into the
    // second one, if both are flagged as the right kind of target; which slots are flagged plays
    // no part in finding *which slot is second*. Excluding every declared first-target layer used
    // to answer that as if it did — so wherever a layer was declared both a first and a second
    // target, a common `BLDCNT` shape, it excluded itself from being the answer under its own
    // winning pixel, and the pass fell through to whatever was third, or the backdrop. Excluding
    // only the actual winner's own slot reproduces hardware's priority order for any stack, at the
    // cost of one more pass over the same layers — which only runs when a blend could actually
    // happen, a small minority of lines.
    //
    // A semi-transparent sprite forces an alpha blend whatever `BLDCNT`'s mode says, so the pass
    // is needed whenever one is on this line too — not only when an alpha blend is configured.
    // Without that, such a sprite would find nothing beneath it and render solid.
    let needs_under =
        frame.effects.blend_mode() == BlendMode::Alpha || has_semi_transparent_sprite(frame, line);
    let under = needs_under.then(|| {
        let mut under = ScanlineBuffer::new(SCREEN_WIDTH as usize);
        under.clear();
        // The same windows apply to what is underneath, and on top of them, each pixel's own
        // winner is excluded: a layer a window excludes is not there at all, and a pixel's winner
        // cannot also be what lies beneath itself.
        let base = visible
            .as_ref()
            .map(|visible| layer_bits(visible))
            .unwrap_or_else(|| vec![ppu_tile2d::ALL_LAYERS; SCREEN_WIDTH as usize]);
        let mask: Vec<u8> = (0..SCREEN_WIDTH as usize)
            .map(|x| base[x] & !scanline.get(x).layer_bit().unwrap_or(0))
            .collect();
        under.set_write_mask(mask);

        for index in frame.backgrounds.draw_order(present) {
            match kinds[index] {
                Some(LayerKind::Text) => draw_text_layer(frame, index, line, &mut under),
                Some(LayerKind::Affine) => draw_affine_layer(frame, index, &mut under),
                Some(LayerKind::Bitmap) => draw_bitmap_layer(frame, mode, &mut under),
                None => {}
            }
        }
        draw_sprites(frame, line, SpriteSelection::Drawn, &mut under);
        under
    });

    let palette = GbaPalette {
        bytes: frame.palette,
    };
    let backdrop = palette.lookup_bg(0, 0);
    let row = framebuffer.row_mut(line);
    scanline.resolve_into(&palette, backdrop, row);
    let composed = Composed {
        scanline: &scanline,
        under: under.as_ref(),
        visible: visible.as_deref(),
    };
    apply_effects(frame, composed, &palette, backdrop, row);
}

/// Which layers each pixel of this line may draw, or `None` when no window is enabled.
///
/// Computed once per line and consumed twice: [`layer_bits`] narrows it to the write mask the
/// scanline buffers enforce, and [`apply_effects`] reads bit 5 of the same value for the
/// colour-effect enable. Those are two different questions about one register read, and asking
/// `Effects::visible_layers` twice per pixel to answer them separately would be the only cost of
/// keeping them apart.
fn window_mask(frame: &Frame<'_>, line: u32, object_window: Option<&[bool]>) -> Option<Vec<u16>> {
    let windows = [
        frame.video.dispcnt & (1 << 13) != 0,
        frame.video.dispcnt & (1 << 14) != 0,
        frame.video.dispcnt & (1 << 15) != 0,
    ];
    // With every window disabled the registers are not consulted at all, which is what a game
    // that never uses them relies on — and it keeps the buffers unmasked, so the mask check in
    // `ScanlineBuffer::set` costs a single not-taken branch.
    if !windows[0] && !windows[1] && !windows[2] {
        return None;
    }
    Some(
        (0..SCREEN_WIDTH)
            .map(|x| {
                let inside = object_window.is_some_and(|mask| mask[x as usize]);
                frame.effects.visible_layers(x, line, windows, inside)
            })
            .collect(),
    )
}

/// Narrow a per-pixel window mask to just the layer bits a [`ScanlineBuffer`] enforces.
///
/// Drops bit 5, which is the colour-effect enable rather than a sixth layer — the same bit
/// position the backdrop occupies in `BLDCNT`, meaning two different things in two register sets.
/// Handing it through as if it were a layer would let it mask a layer that does not exist.
fn layer_bits(visible: &[u16]) -> Vec<u8> {
    visible
        .iter()
        .map(|&v| (v & ppu_tile2d::ALL_LAYERS as u16) as u8)
        .collect()
}

/// One composited line, and the two extra views of it the colour effects need.
///
/// Grouped because they are three answers about the same line and always travel together: what won
/// each pixel, what was beneath the winner, and which layers each pixel permits.
struct Composed<'a> {
    scanline: &'a ScanlineBuffer,
    /// What lies under the winning pixel, composed only when an alpha blend could happen.
    under: Option<&'a ScanlineBuffer>,
    /// Per-pixel window mask, `None` when no window is enabled. See [`window_mask`].
    visible: Option<&'a [u16]>,
}

/// Where the object window covers this line.
///
/// A sprite whose graphics mode is `ObjectWindow` draws nothing. Its *shape* — every pixel of it
/// that is not colour 0 — is a window region instead, and `WINOUT`'s high byte says which layers
/// and whether colour effects apply inside it. Games use it for shapes a rectangle cannot make.
///
/// Reported as never covering until now, which is not a neutral default: a game that reveals its
/// content *through* an object window gets a blank region instead. Pokémon Emerald's battle screen
/// puts the action menu and message box there, so the bottom fifty scanlines came out as pure
/// backdrop.
///
/// The mask is built by rendering those sprites into a scratch buffer rather than by re-deriving
/// tile addressing, flips, depth and affine transforms a second time — every one of which is a
/// place for the two paths to disagree.
fn object_window_mask(frame: &Frame<'_>, line: u32) -> Option<Vec<bool>> {
    if frame.video.dispcnt & (1 << 15) == 0 {
        return None;
    }
    let oam = ObjectAttributeMemory::decode(frame.oam);
    let mut scratch = ScanlineBuffer::new(SCREEN_WIDTH as usize);
    scratch.clear();
    // Deliberately unmasked: this scratch buffer is *computing* the window mask, so it cannot be
    // subject to one. It also goes through the same merged pass as everything else, so an affine
    // object-window sprite defines its region by exactly the rules an affine drawn sprite obeys.
    compose_sprites(
        frame,
        &oam,
        line,
        SpriteSelection::ObjectWindow,
        &mut scratch,
    );

    Some(
        (0..SCREEN_WIDTH as usize)
            .map(|x| scratch.get(x).source == PixelSource::Sprite)
            .collect(),
    )
}

/// Which layer a buffered pixel came from.
fn layer_of(indexed: ppu_tile2d::IndexedPixel) -> Layer {
    match indexed.source {
        PixelSource::Sprite => Layer::Object,
        // A bitmap mode's picture is background 2 wearing a different pixel format; it is
        // selected as a blend or window target exactly as an indexed background 2 would be.
        PixelSource::Background | PixelSource::DirectColor => {
            Layer::background(indexed.layer as usize)
        }
        PixelSource::Backdrop => Layer::Backdrop,
    }
}

/// Blend the resolved line, where a colour effect is configured and the window allows it.
///
/// Runs after the line is resolved because both its questions are about the *winning* pixel: which
/// layer produced it, and what is behind it.
///
/// It no longer decides which layers are visible. That is a question about who may enter priority
/// resolution, not about the winner, and answering it here — by overpainting the winner with the
/// backdrop — produced hard-edged rectangles of flat backdrop wherever a window was used to reveal
/// a *lower* layer rather than to hide everything. Hardware excludes the masked layer from the
/// contest so the next one down wins; that now happens during compositing, in
/// `ScanlineBuffer::set`. See the `ppu-tile2d` crate docs.
fn apply_effects(
    frame: &Frame<'_>,
    composed: Composed<'_>,
    palette: &GbaPalette<'_>,
    backdrop: Rgba8,
    row: &mut [u8],
) {
    let Composed {
        scanline,
        under,
        visible,
    } = composed;
    let mode = frame.effects.blend_mode();
    // Windows have already been applied to the buffer, so with no colour effect there is normally
    // nothing left for this pass to do. A semi-transparent sprite is the exception: it blends
    // whatever `BLDCNT`'s mode says, including when that mode is "none", so its presence is what
    // decides whether the pass can be skipped rather than the register alone. `under` is built
    // exactly when a blend could happen, which makes it the cheap way to ask.
    if mode == BlendMode::None && under.is_none() {
        return;
    }

    for (x, pixel) in row.chunks_exact_mut(4).enumerate() {
        let indexed = scanline.get(x);
        let layer = layer_of(indexed);

        // A semi-transparent sprite is a first target whatever `BLDCNT` selects, and forces an
        // alpha blend even where `BLDCNT` asks for a brightness effect or for none at all. That is
        // why it rides on the pixel: it varies per sprite, so no register consulted here could
        // answer it. Games use it for shadows, water, reflections, and battle-move flashes, all of
        // which rendered as solid blocks while the mode was decoded and never read.
        let forced = indexed.forces_blend;
        if !forced && !frame.effects.is_first_target(layer) {
            continue;
        }
        // Bit 5 is the one thing a window still says about the *winner*: whether colour effects
        // apply inside that region at all. It is not a sixth layer, which is why it survived the
        // move of layer masking into compositing rather than going with it. A game darkening the
        // world behind a menu switches the effect off inside the menu's window; honouring only the
        // layer bits darkened the menu too, and its panels came out grey rather than white.
        if visible.is_some_and(|mask| mask[x] & Layer::COLOUR_EFFECT == 0) {
            continue;
        }
        let top = Rgba8 {
            r: pixel[0],
            g: pixel[1],
            b: pixel[2],
            a: pixel[3],
        };
        // A forced blend is an *alpha* blend whatever the register says, which is what stops a
        // semi-transparent sprite being brightened or darkened along with everything else.
        let effective = if forced { BlendMode::Alpha } else { mode };

        // An alpha blend needs what is underneath, and hardware only blends when that lower pixel's
        // layer is a *second* target — otherwise the top pixel is written through unchanged. The
        // brightness effects have no lower layer at all, so they pass the top pixel twice.
        let lower = match effective {
            BlendMode::Alpha => {
                let beneath =
                    under.map_or_else(ppu_tile2d::IndexedPixel::default, |buffer| buffer.get(x));
                if !frame.effects.is_second_target(layer_of(beneath)) {
                    continue;
                }
                match beneath.source {
                    PixelSource::Backdrop => backdrop,
                    PixelSource::Background => palette.lookup_bg(beneath.palette, beneath.color),
                    PixelSource::Sprite => palette.lookup_sprite(beneath.palette, beneath.color),
                    PixelSource::DirectColor => {
                        ppu_tile2d::bgr555_to_rgba(beneath.as_direct_color())
                    }
                }
            }
            _ => top,
        };
        write_pixel(pixel, frame.effects.blend(effective, top, lower));
    }
}

#[inline]
fn write_pixel(out: &mut [u8], colour: Rgba8) {
    out[0] = colour.r;
    out[1] = colour.g;
    out[2] = colour.b;
    out[3] = colour.a;
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
        priority: layer.priority(),
    };

    if !layer.mosaic() {
        let params = BackgroundParams {
            layer: index as u8,
            // The layer's real size, not `full_line`'s 32x32 default. A background may be 64
            // tiles wide, 64 tall, or both, and `render_text_background` wraps on *these*
            // numbers — so leaving them at 32 made a larger map wrap at half its size and never
            // reach its second screen block. Pokémon Emerald's battle menu lives in exactly that
            // block, on a 32x64 background scrolled to 320: the whole bottom of the screen came
            // out as backdrop.
            map_width: width,
            map_height: height,
            // Index 0 is transparent on this machine: a background is one of four layers, and the
            // one behind — or the backdrop — shows through. Writing it made the frontmost enabled
            // text layer opaque across the whole screen, which covered the real picture with flat
            // bands of one palette colour. The affine and sprite paths here have always skipped
            // it.
            transparent_index_zero: true,
            ..BackgroundParams::full_line(
                line,
                layer.scroll_x as u32,
                layer.scroll_y as u32,
                layer.bit_depth(),
            )
        };
        render_text_background(&map, frame.vram, &params, scanline);
        return;
    }

    // Mosaic is a sample-and-hold: the screen is divided into `(h, v)`-pixel blocks and every
    // pixel in a block shows the colour of the block's top-left one. Vertical is a held source
    // *line*: quantizing `line` to the block boundary before rendering makes every screen line
    // inside a block sample from the same source row, at no extra cost, because nothing about
    // this renderer holds state across calls to begin with. Horizontal cannot be expressed the
    // same way — a rendered row already commits to one colour per column by the time it reaches
    // the shared buffer — so the row renders once at full resolution into a scratch buffer, and
    // every real column re-samples its own block's leftmost column from it. Only text layers do
    // this: an affine background's per-line state is accumulated externally across the whole
    // frame rather than recomputed from `line`, so holding it across several output lines would
    // need snapshotting that state at block boundaries, which nothing here does — affine
    // background mosaic, and the bitmap layer's (modes 3-5 sample through the same accumulated
    // affine state), are not implemented.
    let (h_size, v_size) = frame.effects.bg_mosaic_size();
    let effective_line = (line / v_size) * v_size;
    let params = BackgroundParams {
        layer: index as u8,
        map_width: width,
        map_height: height,
        transparent_index_zero: true,
        ..BackgroundParams::full_line(
            effective_line,
            layer.scroll_x as u32,
            layer.scroll_y as u32,
            layer.bit_depth(),
        )
    };
    let mut scratch = ScanlineBuffer::new(scanline.width());
    scratch.clear();
    render_text_background(&map, frame.vram, &params, &mut scratch);
    for x in 0..scanline.width() {
        let effective_x = (x as u32 / h_size) * h_size;
        let pixel = scratch.get(effective_x as usize);
        // A transparent source pixel must not be written at all: `set` treats a pixel with no
        // layer bit as always committing, so writing the backdrop explicitly would paint over
        // whatever a farther layer already drew here, rather than leaving it showing through as
        // an untouched column would.
        if pixel.source == PixelSource::Backdrop {
            continue;
        }
        scanline.set(x, pixel);
    }
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
///
/// Mosaic is not applied here even when `BGxCNT`'s bit is set. [`draw_text_layer`]'s vertical
/// mosaic works by asking the renderer for an earlier line's state, which is free there because
/// nothing survives between calls; this layer's `current_x`/`current_y` are instead accumulated
/// once per real line by the system driver and never kept for any line but the latest, so holding
/// several output lines to one source line would need snapshotting that state at every mosaic
/// block boundary, which nothing here does.
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
                layer: index as u8,
                source: PixelSource::Background,
                forces_blend: false,
            },
        );
    }
}

/// Draw the bitmap that occupies background 2's slot in modes 3, 4, and 5.
///
/// A bitmap mode has no tile layers: the bitmap *is* background 2, sampled through the very same
/// affine matrix and reference point as an affine tile background — [`draw_affine_layer`], which
/// this mirrors almost line for line — and so subject to the same wraparound rule. It used to be
/// written straight to the framebuffer and the whole scanline returned before anything else ran,
/// which meant a rotated picture never rotated (the matrix was simply never consulted), a window
/// over it did nothing, the blend unit never saw it, and every sprite pixel overwrote it with no
/// priority comparison at all. Drawing it into the shared buffer instead puts it through the same
/// `draw_order`, window mask, blend pass, and sprite-priority rule as any other layer.
///
/// Mode 4 is addressed exactly like an ordinary 256-colour background — one byte per pixel,
/// looked up in palette RAM, index 0 transparent — so it reuses [`PixelSource::Background`]
/// unchanged. Modes 3 and 5 have no palette indirection at all, a 15-bit colour directly in VRAM,
/// which is what [`ppu_tile2d::IndexedPixel::direct_color`] exists to carry through the indexed
/// pipeline; neither has a transparent index, so every in-bounds pixel is opaque.
///
/// Mosaic is not applied here for the same reason it is not applied to an affine tile
/// background — see [`draw_affine_layer`] — this layer samples through the very same
/// per-scanline accumulated affine state.
fn draw_bitmap_layer(frame: &Frame<'_>, mode: u16, scanline: &mut ScanlineBuffer) {
    let layer = frame.backgrounds.layers[2];
    let affine = &frame.affine[0];
    // The screen size bits of BG2CNT are not consulted here: a bitmap mode's picture size is
    // fixed by the mode number, not by the affine background's usual size field.
    let (width, height) = match mode {
        5 => (bitmap::MODE5_WIDTH, bitmap::MODE5_HEIGHT),
        _ => (SCREEN_WIDTH, SCREEN_HEIGHT),
    };

    for x in 0..SCREEN_WIDTH {
        let (tx, ty) = affine.texture_at(x);

        let (tx, ty) = if layer.affine_wraps() {
            (tx.rem_euclid(width as i32), ty.rem_euclid(height as i32))
        } else {
            if tx < 0 || ty < 0 || tx >= width as i32 || ty >= height as i32 {
                continue;
            }
            (tx, ty)
        };

        match mode {
            4 => {
                let stride = SCREEN_WIDTH as usize;
                let offset = frame.video.bitmap_frame_offset() + ty as usize * stride + tx as usize;
                let Some(&index) = frame.vram.get(offset) else {
                    continue;
                };
                // Index zero is transparent here exactly as it is in a tile mode, so it shows
                // whatever is behind background 2 rather than palette entry zero drawn over it.
                if index == 0 {
                    continue;
                }
                scanline.set(
                    x as usize,
                    ppu_tile2d::IndexedPixel {
                        color: index,
                        palette: 0,
                        priority: layer.priority(),
                        layer: 2,
                        source: PixelSource::Background,
                        forces_blend: false,
                    },
                );
            }
            _ => {
                // Modes 3 and 5: a direct 15-bit colour, two bytes per pixel, no palette and no
                // transparent index. Mode 3 has room for only one buffer, so the frame-select bit
                // is ignored there exactly as it was in the direct-to-framebuffer path; mode 5
                // buys double buffering by shrinking the picture instead of dropping colour
                // depth, so it still needs it.
                let base = if mode == 5 {
                    frame.video.bitmap_frame_offset()
                } else {
                    0
                };
                let stride = width as usize * 2;
                let offset = base + ty as usize * stride + tx as usize * 2;
                let (Some(&low), Some(&high)) =
                    (frame.vram.get(offset), frame.vram.get(offset + 1))
                else {
                    continue;
                };
                let colour = u16::from_le_bytes([low, high]);
                scanline.set(
                    x as usize,
                    ppu_tile2d::IndexedPixel::direct_color(colour, layer.priority(), 2),
                );
            }
        }
    }
}

/// Whether any semi-transparent sprite covers this line.
///
/// Asked so the under-buffer can be built for it. Cheap enough to answer by decoding OAM again:
/// it only runs on a line whose blend mode is not already alpha, and it stops at the first hit.
fn has_semi_transparent_sprite(frame: &Frame<'_>, line: u32) -> bool {
    if frame.video.dispcnt & dispcnt::OBJ == 0 {
        return false;
    }
    let oam = ObjectAttributeMemory::decode(frame.oam);
    oam.visible_on_line(line as i32)
        .into_iter()
        .any(|i| oam.objects[i].graphics_mode == crate::objects::GraphicsMode::SemiTransparent)
}

/// Which sprites a pass should draw.
///
/// The three passes differ only in this, which is why they share one routine: drawing them by
/// three different code paths is how the affine and ordinary sprites came to disagree about
/// priority in the first place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SpriteSelection {
    /// Every sprite that draws: the main compositing pass.
    Drawn,
    /// Only sprites whose graphics mode makes them a window shape. These draw nothing; the
    /// pixels they claim define a region.
    ObjectWindow,
}

/// Draw the sprites covering this line into `scanline`, front-most first.
///
/// # One pass, both kinds of sprite
///
/// Ordinary and affine sprites are composited together in a single ordered pass over OAM, sharing
/// one [`SpritePass`] and therefore one claimed-pixel mask and one priority rule. They were two
/// passes, and the two could not see each other: the affine path wrote pixels unconditionally with
/// no comparison against the background at all, and the ordinary path then overwrote every affine
/// pixel because it treated only a *background* pixel as something it could lose to. So a rotating
/// object punched through the text box in front of it, and a farther plain sprite erased a nearer
/// rotated one — two symptoms of the same missing shared state.
///
/// Affine sprites are still drawn here rather than by the shared crate, which has no notion of a
/// matrix; what moved into the shared crate is the *rule* they must obey. See [`SpritePass`].
fn compose_sprites(
    frame: &Frame<'_>,
    oam: &ObjectAttributeMemory,
    line: u32,
    selection: SpriteSelection,
    scanline: &mut ScanlineBuffer,
) {
    let one_dimensional = frame.video.dispcnt & dispcnt::OBJ_1D_MAPPING != 0;
    // Compare the sprite's priority against the background's, which is this machine's rule.
    // `SpriteDecides` is the Game Boy's, and under it every GBA sprite won against every
    // background — so a character walked over the text box in front of them.
    let mut pass = SpritePass::new(SCREEN_WIDTH as usize, SpriteRule::ByPriority);

    for index in sprite_order(oam, line, selection) {
        let object = oam.objects[index];
        if object.mode == ObjectMode::Normal {
            if object.mosaic {
                draw_mosaic_sprite(frame, &object, line, one_dimensional, &mut pass, scanline);
                continue;
            }
            // Sprite tile data is addressed from the object half of VRAM, and a 256-colour sprite
            // decodes differently from a 16-colour one — but a scanline can hold both, so the
            // depth rides on each sprite rather than on this call.
            ppu_tile2d::render_sprite(
                &object.to_sprite(one_dimensional),
                frame.vram,
                line,
                &mut pass,
                scanline,
            );
        } else {
            draw_affine_sprite(
                frame,
                oam,
                &object,
                line as i32,
                one_dimensional,
                &mut pass,
                scanline,
            );
        }
    }
}

/// Which OAM entries a pass draws, front-most first.
///
/// Front-most first is what the shared claim rule expects: the first sprite to claim a pixel keeps
/// it. [`ObjectAttributeMemory::visible_on_line`] already sorts by priority then OAM index, which
/// is the tie-break hardware uses.
fn sprite_order(oam: &ObjectAttributeMemory, line: u32, selection: SpriteSelection) -> Vec<usize> {
    use crate::objects::GraphicsMode;
    match selection {
        SpriteSelection::Drawn => oam.visible_on_line(line as i32),
        // `visible_on_line` deliberately excludes object-window sprites, since they do not draw,
        // so this selection does its own scan. The same priority and index order is kept, so an
        // object window built from overlapping shapes resolves the way a drawn sprite would.
        SpriteSelection::ObjectWindow => {
            let mut found: Vec<usize> = (0..crate::objects::OBJECT_COUNT)
                .filter(|&i| {
                    let object = oam.objects[i];
                    object.graphics_mode == GraphicsMode::ObjectWindow
                        && object.mode != ObjectMode::Hidden
                        && object.covers_line(line as i32)
                })
                .collect();
            found.sort_by_key(|&i| (oam.objects[i].priority, i));
            found
        }
    }
}

/// Draw the sprites covering this line, if the object layer is enabled at all.
fn draw_sprites(
    frame: &Frame<'_>,
    line: u32,
    selection: SpriteSelection,
    scanline: &mut ScanlineBuffer,
) {
    if frame.video.dispcnt & dispcnt::OBJ == 0 {
        return;
    }
    let oam = ObjectAttributeMemory::decode(frame.oam);
    compose_sprites(frame, &oam, line, selection, scanline);
}

/// Draw one rotated or scaled sprite.
///
/// Runs the opposite way from a background: for each pixel of the sprite's *screen* box, the
/// matrix says which texture pixel it came from. A double-size sprite's box is twice its own
/// size, which is what stops a rotation clipping against its own corners — the extra area is
/// deliberately empty until the rotation moves something into it.
///
/// Mosaic is not applied even when the sprite's attribute bit is set. [`draw_mosaic_sprite`]
/// quantizes the sprite's own local pixel coordinates directly, which only works because an
/// ordinary sprite's local coordinate is a simple offset from its screen position; an affine
/// sprite's local coordinate comes from the matrix instead, and mosaic there is not implemented.
fn draw_affine_sprite(
    frame: &Frame<'_>,
    oam: &ObjectAttributeMemory,
    object: &Object,
    line: i32,
    one_dimensional: bool,
    pass: &mut SpritePass,
    scanline: &mut ScanlineBuffer,
) {
    let matrix = &oam.matrices[object.matrix];
    let (box_width, box_height) = object.screen_size();
    let (half_w, half_h) = (object.width as i32 / 2, object.height as i32 / 2);
    let (box_half_w, box_half_h) = (box_width as i32 / 2, box_height as i32 / 2);
    let row_stride = object.row_stride(one_dimensional);
    let tile_size = object.depth.tile_size();

    for screen_x in 0..box_width as i32 {
        let x = object.x + screen_x;
        if x < 0 || x >= SCREEN_WIDTH as i32 {
            continue;
        }
        // Offsets are measured from the centre of the box, which is where the matrix pivots.
        let (tx, ty) = transform_object_pixel(
            matrix,
            screen_x - box_half_w,
            (line - object.y) - box_half_h,
            half_w,
            half_h,
        );
        // Outside the sprite's own bounds the rotation has moved this pixel off the artwork.
        if tx < 0 || ty < 0 || tx >= object.width as i32 || ty >= object.height as i32 {
            continue;
        }

        let (tile_x, tile_y) = (tx as usize / 8, ty as usize / 8);
        let base = object.tile_offset(one_dimensional) + tile_y * row_stride + tile_x * tile_size;
        let (in_x, in_y) = (tx as usize % 8, ty as usize % 8);

        let colour = match object.depth {
            BitDepth::Eight => frame.vram.get(base + in_y * 8 + in_x).copied().unwrap_or(0),
            _ => {
                // Four bits per pixel, two pixels to a byte, low nibble first.
                let byte = frame
                    .vram
                    .get(base + in_y * 4 + in_x / 2)
                    .copied()
                    .unwrap_or(0);
                if in_x % 2 == 0 {
                    byte & 0x0F
                } else {
                    byte >> 4
                }
            }
        };
        if colour == 0 {
            continue;
        }

        // Through the shared pass, not straight into the buffer. Writing directly is what let an
        // affine sprite ignore the background's priority entirely and then be overwritten by any
        // ordinary sprite regardless of which was in front.
        pass.place(
            scanline,
            x as usize,
            ppu_tile2d::IndexedPixel {
                color: colour,
                palette: object.palette,
                priority: object.priority,
                layer: 0,
                source: PixelSource::Sprite,
                forces_blend: object.graphics_mode == crate::objects::GraphicsMode::SemiTransparent,
            },
            // The GBA compares priorities rather than consulting a "behind background" bit.
            false,
        );
    }
}

/// Draw one ordinary (non-affine) sprite with mosaic applied.
///
/// Mosaic quantizes the sprite's own *local* pixel coordinates, not the screen ones it happens to
/// land on: a sprite's blockiness has to look the same wherever it is positioned, or moving it one
/// pixel would shift which of its own pixels share a block and the pattern would visibly swim.
/// Sprite-local coordinates start at the sprite's own top-left corner, not the screen's.
///
/// Vertical is a held source *row*: [`ppu_tile2d::render_sprite`] samples strictly from
/// `line - sprite.y`, so asking it for a quantized line produces the same row for every screen
/// line inside a block, at no extra cost. Horizontal cannot be expressed the same way — a rendered
/// row already commits to one colour per column by the time it reaches the shared buffer — so the
/// sprite renders once at full resolution into a scratch buffer, and every real column re-samples
/// its own block's leftmost column from it, the same trick [`draw_text_layer`] uses.
///
/// Affine (rotated or scaled) sprites are not covered: their local coordinate comes from the
/// matrix rather than directly from the screen position, and mosaic there is not implemented.
fn draw_mosaic_sprite(
    frame: &Frame<'_>,
    object: &Object,
    line: u32,
    one_dimensional: bool,
    pass: &mut SpritePass,
    scanline: &mut ScanlineBuffer,
) {
    let row = line as i32 - object.y;
    if row < 0 || row >= object.height as i32 {
        return;
    }
    let (h_size, v_size) = frame.effects.obj_mosaic_size();
    let effective_row = (row / v_size as i32) * v_size as i32;
    let effective_line = (object.y + effective_row) as u32;

    let mut scratch = ScanlineBuffer::new(scanline.width());
    scratch.clear();
    let mut scratch_pass = SpritePass::new(scanline.width(), SpriteRule::ByPriority);
    ppu_tile2d::render_sprite(
        &object.to_sprite(one_dimensional),
        frame.vram,
        effective_line,
        &mut scratch_pass,
        &mut scratch,
    );

    for local_x in 0..object.width as i32 {
        let x = object.x + local_x;
        if x < 0 || x as usize >= scanline.width() {
            continue;
        }
        let effective_local_x = (local_x / h_size as i32) * h_size as i32;
        let effective_x = object.x + effective_local_x;
        if effective_x < 0 || effective_x as usize >= scanline.width() {
            continue;
        }
        let pixel = scratch.get(effective_x as usize);
        if pixel.source != PixelSource::Sprite {
            continue;
        }
        pass.place(scanline, x as usize, pixel, false);
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
