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
        draw_sprites(frame, line, SpriteSelection::Drawn, &mut scanline);
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
            _ => {}
        }
    }
    draw_sprites(frame, line, SpriteSelection::Drawn, &mut scanline);

    // What is *underneath* the winning pixel, for an alpha blend. Composed as a second pass with
    // the first-target layers left out, rather than by widening `ScanlineBuffer` to keep a
    // runner-up: that buffer is shared with the Game Boy, which has no colour effects at all and
    // would carry the second slot on every line to no purpose. The pass only runs when a blend
    // could actually happen, which is a small minority of lines.
    //
    // Exact whenever the layer directly beneath the top pixel is not itself a first target. Where
    // two first-target layers stack, hardware blends the top with the second and this skips to the
    // third. Nothing in the corpus does that, and the alternative is keeping every layer's pixel.
    //
    // A semi-transparent sprite forces an alpha blend whatever `BLDCNT`'s mode says, so the buffer
    // is needed whenever one is on this line too — not only when an alpha blend is configured.
    // Without that, such a sprite would find nothing beneath it and render solid.
    let needs_under =
        frame.effects.blend_mode() == BlendMode::Alpha || has_semi_transparent_sprite(frame, line);
    let under = needs_under.then(|| {
        let mut under = ScanlineBuffer::new(SCREEN_WIDTH as usize);
        under.clear();
        // The same windows apply to what is underneath: a layer a window excludes is not merely
        // hidden, it is not there, so it cannot be the lower half of a blend either.
        if let Some(visible) = &visible {
            under.set_write_mask(layer_bits(visible));
        }
        for index in frame.backgrounds.draw_order(present) {
            if frame.effects.is_first_target(Layer::background(index)) {
                continue;
            }
            match kinds[index] {
                Some(LayerKind::Text) => draw_text_layer(frame, index, line, &mut under),
                Some(LayerKind::Affine) => draw_affine_layer(frame, index, &mut under),
                _ => {}
            }
        }
        // Sprites can be the lower half of a blend, but a sprite that is itself the *source* of one
        // cannot blend with itself. Two things make a sprite a source: the object layer being a
        // declared first target, which excludes them all, and an individual sprite being
        // semi-transparent, which `NotBlendSource` filters out per sprite.
        if !frame.effects.is_first_target(Layer::Object) {
            draw_sprites(frame, line, SpriteSelection::NotBlendSource, &mut under);
        }
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
    /// What lies under the winning pixel, composed only when an alpha blend is configured.
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
        PixelSource::Background => Layer::background(indexed.layer as usize),
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
    let params = BackgroundParams {
        layer: index as u8,
        // The layer's real size, not `full_line`'s 32x32 default. A background may be 64 tiles
        // wide, 64 tall, or both, and `render_text_background` wraps on *these* numbers — so
        // leaving them at 32 made a larger map wrap at half its size and never reach its second
        // screen block. Pokémon Emerald's battle menu lives in exactly that block, on a 32x64
        // background scrolled to 320: the whole bottom of the screen came out as backdrop.
        map_width: width,
        map_height: height,
        // Index 0 is transparent on this machine: a background is one of four layers, and the one
        // behind — or the backdrop — shows through. Writing it made the frontmost enabled text
        // layer opaque across the whole screen, which covered the real picture with flat bands of
        // one palette colour. The affine and sprite paths here have always skipped it.
        transparent_index_zero: true,
        ..BackgroundParams::full_line(
            line,
            layer.scroll_x as u32,
            layer.scroll_y as u32,
            layer.bit_depth(),
        )
    };
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
                layer: index as u8,
                source: PixelSource::Background,
                forces_blend: false,
            },
        );
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
    /// Only sprites that are *not* blend first targets, for the under-buffer.
    ///
    /// A first-target sprite is the thing being blended, so it cannot also be what it blends
    /// *with*. Semi-transparent sprites are first targets whatever `BLDCNT` says, so they are
    /// excluded here even when the object layer is not a declared target.
    NotBlendSource,
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
        SpriteSelection::NotBlendSource => oam
            .visible_on_line(line as i32)
            .into_iter()
            .filter(|&i| oam.objects[i].graphics_mode != GraphicsMode::SemiTransparent)
            .collect(),
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
