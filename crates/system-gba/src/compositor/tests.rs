use super::*;
use crate::background::SCREEN_BLOCK;
use crate::video::{dispcnt, reg};

const RED: u16 = 0x001F;
const GREEN: u16 = 0x03E0;
const BLUE: u16 = 0x7C00;

struct Scene {
    video: VideoTiming,
    backgrounds: Backgrounds,
    affine: [crate::affine::AffineBackground; 2],
    effects: crate::effects::Effects,
    vram: Vec<u8>,
    palette: Vec<u8>,
    oam: Vec<u8>,
}

impl Scene {
    fn new(mode: u16) -> Self {
        let mut video = VideoTiming::new();
        video.write16(reg::DISPCNT, mode);
        Self {
            video,
            backgrounds: Backgrounds::new(),
            affine: [crate::affine::AffineBackground::new(); 2],
            effects: crate::effects::Effects::new(),
            vram: vec![0u8; 0x1_8000],
            palette: vec![0u8; 0x400],
            // Parked as hidden rather than zeroed: an all-zero OAM is 128 visible 8x8 sprites
            // at the origin, which is what hardware shows and not what these scenes mean.
            oam: {
                let mut oam = vec![0u8; 0x400];
                for entry in 0..crate::objects::OBJECT_COUNT {
                    oam[entry * 8..entry * 8 + 2].copy_from_slice(&(2u16 << 8).to_le_bytes());
                }
                oam
            },
        }
    }

    fn colour(&mut self, index: usize, value: u16) {
        self.palette[index * 2..index * 2 + 2].copy_from_slice(&value.to_le_bytes());
    }

    /// Fill tile 1 with colour index 1 and point layer `index`'s map cell (0,0) at it.
    ///
    /// **One drawing layer, so this cannot test ordering.** With a single layer, "show the layer
    /// beneath" and "show the backdrop" are the same pixel — which is exactly how a window test
    /// passed for months while the compositor masked layers *after* resolving the line instead of
    /// excluding them from resolution. Any test about priority, windows, or blending wants
    /// [`Scene::two_layers`]; this one is for scenes where only one layer is the point.
    fn simple_layer(&mut self, index: usize, priority: u16, screen_block: usize) {
        for row in 0..8 {
            self.vram[0x20 + row * 4] = 0x11;
            self.vram[0x20 + row * 4 + 1] = 0x11;
            self.vram[0x20 + row * 4 + 2] = 0x11;
            self.vram[0x20 + row * 4 + 3] = 0x11;
        }
        let cell = screen_block * SCREEN_BLOCK;
        self.vram[cell..cell + 2].copy_from_slice(&1u16.to_le_bytes());

        self.backgrounds.write16(
            crate::background::CONTROL_BASE + index as u32 * 2,
            priority | ((screen_block as u16) << 8),
        );
        self.video
            .write16(reg::DISPCNT, self.video.dispcnt | (1 << (8 + index)));
    }

    /// Two text layers covering the same eight pixels, in different palettes.
    ///
    /// `front` is given priority 0 and `behind` priority 1, so `front` wins wherever both draw.
    /// They share one solid tile but their map cells name different palettes, so the rendered
    /// colour says *which layer won* rather than merely that something drew — the distinction a
    /// one-layer scene cannot make, and the reason the compositor's ordering bug went unseen.
    ///
    /// `front` renders as palette entry 1 and `behind` as entry 17; a caller sets those two
    /// colours to tell them apart, and palette entry 0 to see the backdrop.
    fn two_layers(&mut self, front: usize, behind: usize) {
        // One solid tile of colour index 1, shared by both layers.
        for row in 0..8 {
            self.vram[0x20 + row * 4..0x20 + row * 4 + 4].copy_from_slice(&[0x11; 4]);
        }
        for (priority, (index, palette)) in [(front, 0u16), (behind, 1u16)].into_iter().enumerate()
        {
            // Distinct screen blocks so the two map cells cannot collide.
            let block = 8 + priority;
            let cell = block * SCREEN_BLOCK;
            // Tile 1, with the palette number in bits 12-15 of the map entry.
            self.vram[cell..cell + 2].copy_from_slice(&(1u16 | (palette << 12)).to_le_bytes());
            self.backgrounds.write16(
                crate::background::CONTROL_BASE + index as u32 * 2,
                priority as u16 | ((block as u16) << 8),
            );
            self.video
                .write16(reg::DISPCNT, self.video.dispcnt | (1 << (8 + index)));
        }
    }

    /// Enable background 2 — the bitmap's slot in modes 3-5 — and give it the identity transform,
    /// which is what every game that does not mean to rotate its bitmap sets up before relying on
    /// the picture landing where it was drawn.
    ///
    /// Mode and the background 2 enable bit are genuinely separate registers on hardware, so this
    /// only sets the enable bit; a caller picks the mode through [`Scene::new`] as usual, and a
    /// test that wants to check the enable bit's own effect writes `DISPCNT` directly instead of
    /// calling this.
    fn enable_bitmap(&mut self) {
        self.video
            .write16(reg::DISPCNT, self.video.dispcnt | (1 << 10));
        self.affine[0].matrix = crate::affine::IDENTITY;
    }

    fn frame(&self) -> Frame<'_> {
        Frame {
            video: &self.video,
            backgrounds: &self.backgrounds,
            affine: &self.affine,
            effects: &self.effects,
            vram: &self.vram,
            palette: &self.palette,
            oam: &self.oam,
        }
    }

    fn render(&self, line: u32) -> Framebuffer {
        let mut framebuffer = Framebuffer::new(SCREEN_WIDTH, SCREEN_HEIGHT);
        render_scanline(&self.frame(), line, &mut framebuffer);
        framebuffer
    }
}

#[test]
fn each_mode_has_its_own_set_of_layers() {
    // Not a subtly wrong picture if this is wrong: a layer drawn from memory holding something
    // else entirely.
    use LayerKind::*;
    assert_eq!(
        layers_for_mode(0),
        [Some(Text), Some(Text), Some(Text), Some(Text)]
    );
    assert_eq!(
        layers_for_mode(1),
        [Some(Text), Some(Text), Some(Affine), None]
    );
    assert_eq!(layers_for_mode(2), [None, None, Some(Affine), Some(Affine)]);
    for mode in 3..=5 {
        assert_eq!(
            layers_for_mode(mode),
            [None, None, Some(Bitmap), None],
            "mode {mode}: the bitmap occupies background 2's slot"
        );
    }
    assert_eq!(layers_for_mode(6), [None; 4], "and 6 and 7 are prohibited");
}

#[test]
fn palette_ram_serves_both_background_and_sprite_lookups() {
    // One memory, unlike the Game Boy Color where they are separate.
    let mut bytes = vec![0u8; 0x400];
    bytes[2..4].copy_from_slice(&RED.to_le_bytes());
    bytes[514..516].copy_from_slice(&BLUE.to_le_bytes());
    let palette = GbaPalette { bytes: &bytes };

    assert_eq!(palette.lookup_bg(0, 1).r, 0xFF);
    assert_eq!(
        palette.lookup_sprite(0, 1).b,
        0xFF,
        "sprite palettes start halfway through"
    );
}

#[test]
fn a_sixteen_colour_palette_is_indexed_in_blocks_of_sixteen() {
    let mut bytes = vec![0u8; 0x400];
    // Palette 3, colour 2 is entry 50.
    bytes[100..102].copy_from_slice(&GREEN.to_le_bytes());
    let palette = GbaPalette { bytes: &bytes };
    assert_eq!(palette.lookup_bg(3, 2).g, 0xFF);
}

#[test]
fn nothing_drawn_shows_palette_entry_zero() {
    let mut scene = Scene::new(0);
    scene.colour(0, BLUE);
    assert_eq!(scene.render(0).pixel(0, 0).b, 0xFF);
}

#[test]
fn a_text_layer_draws_through_the_palette() {
    let mut scene = Scene::new(0);
    scene.colour(0, BLUE);
    scene.colour(1, RED);
    scene.simple_layer(0, 0, 8);
    assert_eq!(scene.render(0).pixel(0, 0).r, 0xFF);
}

#[test]
fn a_disabled_layer_is_not_drawn_even_when_it_is_configured() {
    let mut scene = Scene::new(0);
    scene.colour(0, BLUE);
    scene.colour(1, RED);
    scene.simple_layer(0, 0, 8);
    // Clear the enable bit the helper set.
    let dispcnt = scene.video.dispcnt & !(1 << 8);
    scene.video.write16(reg::DISPCNT, dispcnt);
    assert_eq!(scene.render(0).pixel(0, 0).b, 0xFF, "the backdrop");
}

#[test]
fn a_layer_absent_from_the_mode_is_not_drawn_even_if_enabled() {
    // Mode 2 has no background 0. Enabling it must not draw one from whatever happens to be at
    // its configured addresses.
    let mut scene = Scene::new(2);
    scene.colour(0, BLUE);
    scene.colour(1, RED);
    scene.simple_layer(0, 0, 8);
    assert_eq!(scene.render(0).pixel(0, 0).b, 0xFF, "the backdrop");
}

#[test]
fn a_lower_priority_number_is_drawn_in_front() {
    let mut scene = Scene::new(0);
    scene.colour(0, BLUE);
    scene.colour(1, RED); // layer 0 uses palette 0
    scene.colour(17, GREEN); // layer 1 will use palette 1

    scene.simple_layer(0, 3, 8); // behind
    scene.simple_layer(1, 0, 9); // in front
                                 // Give layer 1's map entry palette 1 so the two are distinguishable.
    let cell = 9 * SCREEN_BLOCK;
    scene.vram[cell..cell + 2].copy_from_slice(&(1u16 | (1 << 12)).to_le_bytes());

    assert_eq!(
        scene.render(0).pixel(0, 0).g,
        0xFF,
        "the priority 0 layer won"
    );
}

#[test]
fn forced_blank_shows_white_and_ignores_every_layer() {
    // It is how a game hides a mid-frame rewrite of VRAM, so it must not depend on the contents
    // that rewrite is in the middle of changing.
    let mut scene = Scene::new(0);
    scene.colour(0, BLUE);
    scene.colour(1, RED);
    scene.simple_layer(0, 0, 8);
    scene
        .video
        .write16(reg::DISPCNT, scene.video.dispcnt | dispcnt::FORCED_BLANK);

    let framebuffer = scene.render(0);
    assert_eq!(framebuffer.pixel(0, 0), Rgba8::WHITE);
    assert_eq!(framebuffer.pixel(SCREEN_WIDTH - 1, 0), Rgba8::WHITE);
}

#[test]
fn a_bitmap_mode_draws_the_bitmap_rather_than_a_tile_layer() {
    let mut scene = Scene::new(3);
    scene.enable_bitmap();
    scene.vram[0..2].copy_from_slice(&GREEN.to_le_bytes());
    assert_eq!(scene.render(0).pixel(0, 0).g, 0xFF);
}

#[test]
fn a_rotated_mode_three_bitmap_samples_a_different_texture_pixel_per_column() {
    // A bitmap mode is background 2 wearing a direct-colour format; it is sampled through the
    // very same affine transform an affine tile background is, so a matrix other than the
    // identity must visibly rotate the picture. Writing the bitmap straight to the framebuffer —
    // what this used to do — could never produce that: the matrix would simply never be
    // consulted, whatever the registers held.
    let mut scene = Scene::new(3);
    scene.enable_bitmap();
    // Swap the axes: screen column x samples texture row x of column 0, instead of column x of
    // row 0 as the identity transform would.
    scene.affine[0].matrix.pa = 0;
    scene.affine[0].matrix.pc = 1 << crate::affine::FRACTIONAL_BITS;
    // A marker at texture column 0, row 5 — screen column 5 finds it only through the rotation;
    // under the identity transform screen column 5 would instead read texture column 5, row 0,
    // which is untouched.
    let offset = 5 * (SCREEN_WIDTH as usize * 2);
    scene.vram[offset..offset + 2].copy_from_slice(&GREEN.to_le_bytes());

    let frame = scene.render(0);
    assert_eq!(
        frame.pixel(5, 0).g,
        0xFF,
        "screen column 5 sampled texture row 5 through the rotated transform"
    );
    assert_eq!(
        frame.pixel(0, 0).g,
        0,
        "screen column 0 sampled texture row 0, where nothing was drawn"
    );
}

#[test]
fn a_priority_three_sprite_is_hidden_behind_a_mode_three_bitmap() {
    // On hardware a bitmap mode is background 2 with an ordinary priority, so a sprite compares
    // against it exactly as it would against any other background — including losing. The bitmap
    // path used to write every sprite pixel over the bitmap unconditionally, with no comparison
    // at all, so a farther sprite always won.
    let mut scene = Scene::new(3);
    scene.enable_bitmap();
    // Background 2's own priority, lower numbers nearer: give it the frontmost priority so a
    // priority-3 (furthest) sprite loses to it everywhere the bitmap actually drew.
    scene
        .backgrounds
        .write16(crate::background::CONTROL_BASE + 2 * 2, 0);
    for x in 0..SCREEN_WIDTH as usize {
        scene.vram[x * 2..x * 2 + 2].copy_from_slice(&RED.to_le_bytes());
    }
    scene.colour(257, GREEN);
    scene.sprite_tiles();
    // Priority 3, attribute 2 bits 10-11.
    scene.set_sprite(0, 0, 0, 3 << 10);

    let frame = scene.render(0);
    assert_eq!(
        frame.pixel(0, 0).r,
        0xFF,
        "the bitmap, at background 2's priority 0, hid the priority-3 sprite"
    );
}

#[test]
fn a_window_excluding_background_two_reveals_the_backdrop_inside_it_in_mode_four() {
    // A bitmap mode's picture is background 2, so a window that excludes it does exactly what it
    // would to any other background: the pixel is kept out of priority resolution entirely and
    // the backdrop shows, rather than the bitmap being drawn regardless and the window doing
    // nothing at all — which is what writing it straight to the framebuffer, bypassing the
    // compositor, used to mean.
    use crate::effects::{reg as ereg, Layer};
    let mut scene = Scene::new(4);
    scene.enable_bitmap();
    scene.colour(0, BLUE); // backdrop
    scene.colour(1, RED); // mode 4 palette entry 1
    for x in 0..SCREEN_WIDTH as usize {
        scene.vram[x] = 1;
    }

    scene.effects.write16(ereg::WIN0H, 4); // x in 0..4 inside, 4.. outside
    scene.effects.write16(ereg::WIN0V, 160);
    scene.effects.write16(ereg::WININ, 0); // background 2 excluded inside the window
    scene.effects.write16(ereg::WINOUT, Layer::Bg2.bit());
    scene
        .video
        .write16(reg::DISPCNT, scene.video.dispcnt | (1 << 13)); // window 0 enabled

    let frame = scene.render(0);
    assert_eq!(
        frame.pixel(2, 0).b,
        0xFF,
        "the backdrop shows inside the window"
    );
    assert_eq!(
        frame.pixel(6, 0).r,
        0xFF,
        "and the bitmap still shows outside it"
    );
}

#[test]
fn the_frame_select_bit_still_picks_the_hidden_buffer_through_the_compositor_in_mode_four() {
    // Double buffering is the whole point of mode 4's page-flip bit, so routing the bitmap
    // through the ordinary compositor path must not lose it.
    use crate::video::dispcnt;
    let mut scene = Scene::new(4);
    scene.enable_bitmap();
    scene.colour(1, RED); // frame 0's pixel
    scene.colour(2, GREEN); // frame 1's pixel
    scene.vram[0] = 1; // frame 0, at (0, 0)
    scene.vram[0xA000] = 2; // frame 1, at (0, 0)

    assert_eq!(scene.render(0).pixel(0, 0).r, 0xFF, "frame 0 shows first");

    scene
        .video
        .write16(reg::DISPCNT, scene.video.dispcnt | dispcnt::FRAME_SELECT);
    assert_eq!(
        scene.render(0).pixel(0, 0).g,
        0xFF,
        "the frame-select bit flips to the hidden buffer"
    );
}

#[test]
fn the_frame_select_bit_still_picks_the_hidden_buffer_through_the_compositor_in_mode_five() {
    // Mode 5 buys its double buffering by shrinking the picture instead of dropping colour depth,
    // but the page-flip bit is the same register and must still work once routed through the
    // ordinary compositor path.
    use crate::video::dispcnt;
    let mut scene = Scene::new(5);
    scene.enable_bitmap();
    scene.vram[0..2].copy_from_slice(&RED.to_le_bytes()); // frame 0, at (0, 0)
    scene.vram[0xA000..0xA002].copy_from_slice(&GREEN.to_le_bytes()); // frame 1, at (0, 0)

    assert_eq!(scene.render(0).pixel(0, 0).r, 0xFF, "frame 0 shows first");

    scene
        .video
        .write16(reg::DISPCNT, scene.video.dispcnt | dispcnt::FRAME_SELECT);
    assert_eq!(
        scene.render(0).pixel(0, 0).g,
        0xFF,
        "the frame-select bit flips to the hidden buffer"
    );
}

#[test]
fn a_line_past_the_bottom_of_the_screen_is_not_drawn() {
    let mut scene = Scene::new(3);
    scene.vram[0..2].copy_from_slice(&GREEN.to_le_bytes());
    let mut framebuffer = Framebuffer::new(SCREEN_WIDTH, SCREEN_HEIGHT);
    render_scanline(&scene.frame(), SCREEN_HEIGHT, &mut framebuffer);
    // A fresh framebuffer is all zeroes including alpha, so an untouched pixel is
    // distinguishable from a black one that was actually drawn.
    assert_eq!(framebuffer.pixel(0, 0).a, 0, "untouched");
}

#[test]
fn an_affine_layer_draws_through_its_transform() {
    // Affine layers walk the *screen* and ask the transform where each pixel came from, rather
    // than walking the map — a rotated map does not visit screen pixels in order.
    let mut scene = Scene::new(2);
    scene.colour(0, BLUE);
    scene.colour(1, RED);
    // 256-colour tile 0: one byte per pixel, colour index 1 everywhere.
    for byte in 0..64 {
        scene.vram[byte] = 1;
    }
    // The map is one byte per tile with no attributes; cell 0 names tile 0.
    scene.vram[8 * SCREEN_BLOCK] = 0;
    scene
        .backgrounds
        .write16(crate::background::CONTROL_BASE + 4, 8 << 8);
    scene.video.write16(reg::DISPCNT, 2 | (1 << 10));
    scene.affine[0].matrix = crate::affine::IDENTITY;

    assert_eq!(
        scene.render(0).pixel(0, 0).r,
        0xFF,
        "the identity transform"
    );
    // Every unset map cell names tile 0, which is the one that was filled, so the whole
    // 128-pixel map is opaque. Past its edge is where the backdrop shows.
    assert_eq!(
        scene.render(0).pixel(127, 0).r,
        0xFF,
        "the last mapped pixel"
    );
    assert_eq!(scene.render(0).pixel(128, 0).b, 0xFF, "and past its edge");
}

#[test]
fn an_affine_layer_outside_its_map_shows_the_backdrop_unless_it_wraps() {
    // Wrapping is not the default: a game rotating a small map wants the edges to fall away
    // rather than tile, and reading the bit backwards makes a spinning floor look like
    // wallpaper.
    let mut scene = Scene::new(2);
    scene.colour(0, BLUE);
    scene.colour(1, RED);
    for byte in 0..64 {
        scene.vram[byte] = 1;
    }
    scene.vram[8 * SCREEN_BLOCK] = 0;
    scene
        .backgrounds
        .write16(crate::background::CONTROL_BASE + 4, 8 << 8);
    scene.video.write16(reg::DISPCNT, 2 | (1 << 10));
    scene.affine[0].matrix = crate::affine::IDENTITY;
    // Start one whole 128x128-pixel map to the right, so every pixel is off the map.
    scene.affine[0].write32(0x8, 128 << 8);
    scene.affine[0].begin_frame();

    assert_eq!(scene.render(0).pixel(0, 0).b, 0xFF, "off the map");

    // With the wrap bit set the same position folds back onto the tile.
    scene
        .backgrounds
        .write16(crate::background::CONTROL_BASE + 4, (8 << 8) | (1 << 13));
    assert_eq!(scene.render(0).pixel(0, 0).r, 0xFF, "wrapped");
}

#[test]
fn the_drawn_predicate_distinguishes_the_backdrop_from_a_layer() {
    assert!(!was_drawn(PixelSource::Backdrop));
    assert!(was_drawn(PixelSource::Background));
    assert!(was_drawn(PixelSource::Sprite));
}

// -- Sprites ---------------------------------------------------------------

impl Scene {
    /// Put a sprite at OAM index 0 covering the top-left corner, drawn from tile 0 of the
    /// object half of VRAM with colour index 1 everywhere.
    fn simple_sprite(&mut self, attr0_extra: u16, attr2: u16) {
        let base = crate::objects::OBJ_TILE_BASE;
        for row in 0..8 {
            for byte in 0..4 {
                self.vram[base + row * 4 + byte] = 0x11;
            }
        }
        self.oam[0..2].copy_from_slice(&attr0_extra.to_le_bytes());
        self.oam[2..4].copy_from_slice(&0u16.to_le_bytes());
        self.oam[4..6].copy_from_slice(&attr2.to_le_bytes());
        self.video.write16(
            reg::DISPCNT,
            self.video.dispcnt | dispcnt::OBJ | dispcnt::OBJ_1D_MAPPING,
        );
    }

    /// Fill the first object tile slot with colour index 1 and enable objects.
    ///
    /// Unlike [`Scene::simple_sprite`] this writes no OAM entry, so a test can place several
    /// sprites itself with [`Scene::set_sprite`].
    fn sprite_tiles(&mut self) {
        let base = crate::objects::OBJ_TILE_BASE;
        for row in 0..8 {
            for byte in 0..4 {
                self.vram[base + row * 4 + byte] = 0x11;
            }
        }
        self.video.write16(
            reg::DISPCNT,
            self.video.dispcnt | dispcnt::OBJ | dispcnt::OBJ_1D_MAPPING,
        );
    }

    /// Write one OAM entry's three attribute halfwords.
    ///
    /// Leaves the fourth halfword alone: it is not part of the sprite but one sixteenth of an
    /// affine matrix, which [`Scene::identity_matrix`] writes separately.
    fn set_sprite(&mut self, index: usize, attr0: u16, attr1: u16, attr2: u16) {
        let base = index * 8;
        self.oam[base..base + 2].copy_from_slice(&attr0.to_le_bytes());
        self.oam[base + 2..base + 4].copy_from_slice(&attr1.to_le_bytes());
        self.oam[base + 4..base + 6].copy_from_slice(&attr2.to_le_bytes());
    }

    /// Make matrix 0 the identity, so an affine sprite lands exactly where a plain one would.
    ///
    /// That is what lets a test compare the two paths at the same pixel: any difference is the
    /// compositing rule, not the transform.
    fn identity_matrix(&mut self) {
        for (n, value) in [0x0100u16, 0x0000, 0x0000, 0x0100].iter().enumerate() {
            self.oam[n * 8 + 6..n * 8 + 8].copy_from_slice(&value.to_le_bytes());
        }
    }
}

/// Attribute-0 bits these tests set by name rather than by number.
const AFFINE: u16 = 1 << 8;
const SEMI_TRANSPARENT: u16 = 1 << 10;
/// Attribute-2 bit 12: the low bit of the 16-colour palette number.
const PALETTE_1: u16 = 1 << 12;

#[test]
fn a_sprite_draws_over_a_text_layer() {
    let mut scene = Scene::new(0);
    scene.colour(0, BLUE);
    scene.colour(1, RED);
    scene.colour(257, GREEN); // sprite palette 0, colour 1
    scene.simple_layer(0, 0, 8);
    scene.simple_sprite(0, 0);

    assert_eq!(
        scene.render(0).pixel(0, 0).g,
        0xFF,
        "the sprite is in front"
    );
}

#[test]
fn a_sprite_draws_over_a_bitmap_mode_too() {
    // Bitmap modes are not a separate world: sprites compose over them the same way.
    let mut scene = Scene::new(3);
    scene.enable_bitmap();
    // The whole first line, so there is bitmap either side of the eight-pixel sprite.
    for x in 0..SCREEN_WIDTH as usize {
        scene.vram[x * 2..x * 2 + 2].copy_from_slice(&RED.to_le_bytes());
    }
    scene.colour(257, GREEN);
    scene.simple_sprite(0, 0);

    let framebuffer = scene.render(0);
    assert_eq!(framebuffer.pixel(0, 0).g, 0xFF, "the sprite");
    assert_eq!(
        framebuffer.pixel(100, 0).r,
        0xFF,
        "and the bitmap beside it"
    );
}

#[test]
fn clearing_the_object_enable_bit_removes_every_sprite() {
    let mut scene = Scene::new(0);
    scene.colour(0, BLUE);
    scene.colour(257, GREEN);
    scene.simple_sprite(0, 0);
    let dispcnt = scene.video.dispcnt & !dispcnt::OBJ;
    scene.video.write16(reg::DISPCNT, dispcnt);
    assert_eq!(scene.render(0).pixel(0, 0).b, 0xFF, "the backdrop");
}

#[test]
fn a_hidden_sprite_is_not_drawn() {
    let mut scene = Scene::new(0);
    scene.colour(0, BLUE);
    scene.colour(257, GREEN);
    scene.simple_sprite(2 << 8, 0); // hidden
    assert_eq!(scene.render(0).pixel(0, 0).b, 0xFF);
}

#[test]
fn an_affine_sprite_with_no_matrix_written_draws_nothing() {
    // Matrix 0 is all zeroes until a game writes one, which collapses every screen pixel onto
    // the same texture pixel. Degenerate, and it must not index out of bounds.
    let mut scene = Scene::new(0);
    scene.colour(0, BLUE);
    scene.colour(257, GREEN);
    scene.simple_sprite(1 << 8, 0); // affine
    assert_eq!(
        scene.render(0).pixel(100, 0).b,
        0xFF,
        "nothing outside its box"
    );
}

#[test]
fn a_sprite_off_this_line_is_not_drawn() {
    let mut scene = Scene::new(0);
    scene.colour(0, BLUE);
    scene.colour(257, GREEN);
    scene.simple_sprite(100, 0); // y = 100
    assert_eq!(scene.render(0).pixel(0, 0).b, 0xFF, "line 0 is above it");
    assert_eq!(
        scene.render(100).pixel(0, 100).g,
        0xFF,
        "but line 100 has it"
    );
}

// -- Windows and blending --------------------------------------------------

#[test]
fn a_window_excludes_a_layer_from_resolution_rather_than_painting_over_the_winner() {
    // The contract, and the bug this file's window tests could not previously see. Hardware keeps
    // an excluded layer out of the priority contest, so the next enabled layer down wins the
    // pixel. Masking after the fact — letting the front layer win and then overwriting it with the
    // backdrop — is a different picture: hard-edged rectangles of flat backdrop wherever a window
    // was used to *filter* rather than to hide, which is text-box interiors, battle HUDs, and a
    // cave's light radius.
    //
    // The window edge runs through the middle of the drawn tile, so one assertion covers each side
    // and both are pixels the layers actually cover.
    use crate::effects::{reg as ereg, Layer};
    let mut scene = Scene::new(0);
    scene.colour(0, BLUE); // backdrop
    scene.colour(1, RED); // BG0, in front
    scene.colour(17, GREEN); // BG1, behind
    scene.two_layers(0, 1);

    scene.effects.write16(ereg::WIN0H, 4); // x in 0..4 inside, 4..8 outside
    scene.effects.write16(ereg::WIN0V, 160);
    // Inside, only the layer behind may draw; outside, both may.
    scene.effects.write16(ereg::WININ, Layer::Bg1.bit());
    scene
        .effects
        .write16(ereg::WINOUT, Layer::Bg0.bit() | Layer::Bg1.bit());
    scene
        .video
        .write16(reg::DISPCNT, scene.video.dispcnt | (1 << 13));

    let framebuffer = scene.render(0);
    let inside = framebuffer.pixel(2, 0);
    assert_eq!(
        inside.g, 0xFF,
        "inside the window the excluded front layer steps aside and BG1 wins"
    );
    assert_eq!(
        inside.b, 0x00,
        "and it is BG1's colour, not the backdrop showing through a hole"
    );
    assert_eq!(
        framebuffer.pixel(6, 0).r,
        0xFF,
        "outside it BG0 is permitted and still wins"
    );
}

#[test]
fn a_window_that_permits_no_layer_at_all_still_shows_the_backdrop() {
    // The other half of the contract: excluding every layer is what *does* leave the backdrop, and
    // the backdrop itself is never maskable — bit 5 is the colour-effect enable, not a
    // backdrop-enable.
    use crate::effects::{reg as ereg, Layer};
    let mut scene = Scene::new(0);
    scene.colour(0, BLUE);
    scene.colour(1, RED);
    scene.colour(17, GREEN);
    scene.two_layers(0, 1);

    scene.effects.write16(ereg::WIN0H, 0x00F0);
    scene.effects.write16(ereg::WIN0V, 0x00A0);
    scene.effects.write16(ereg::WININ, 0); // nothing may draw
    scene
        .effects
        .write16(ereg::WINOUT, Layer::Bg0.bit() | Layer::Bg1.bit());
    scene
        .video
        .write16(reg::DISPCNT, scene.video.dispcnt | (1 << 13));

    assert_eq!(
        scene.render(0).pixel(0, 0).b,
        0xFF,
        "with every layer excluded there is nothing left but the backdrop"
    );
}

#[test]
fn winout_excludes_a_layer_outside_the_window_and_reveals_the_one_beneath() {
    // `WINOUT` is a separate register from `WININ` and takes the same path, so it needs its own
    // check that it excludes rather than overpaints.
    use crate::effects::{reg as ereg, Layer};
    let mut scene = Scene::new(0);
    scene.colour(0, BLUE);
    scene.colour(1, RED);
    scene.colour(17, GREEN);
    scene.two_layers(0, 1);

    scene.effects.write16(ereg::WIN0H, 4); // x in 0..4 inside, 4..8 outside
    scene.effects.write16(ereg::WIN0V, 160);
    scene
        .effects
        .write16(ereg::WININ, Layer::Bg0.bit() | Layer::Bg1.bit());
    scene.effects.write16(ereg::WINOUT, Layer::Bg1.bit());
    scene
        .video
        .write16(reg::DISPCNT, scene.video.dispcnt | (1 << 13));

    let framebuffer = scene.render(0);
    assert_eq!(framebuffer.pixel(2, 0).r, 0xFF, "inside, BG0 still wins");
    let outside = framebuffer.pixel(6, 0);
    assert_eq!(outside.g, 0xFF, "outside, BG1 wins in BG0's place");
    assert_eq!(outside.b, 0x00, "rather than the backdrop");
}

#[test]
fn an_object_window_excluding_a_layer_reveals_the_one_beneath() {
    // The object window reaches the same masking path by a different route — its region comes from
    // a sprite's shape rather than a rectangle — so it gets the same two-layer check. This is the
    // case Pokémon Emerald's battle screen actually hits.
    use crate::effects::{reg as ereg, Layer};
    const OBJECT_WINDOW: u16 = 2 << 10;

    let mut scene = Scene::new(0);
    scene.colour(0, BLUE);
    scene.colour(1, RED);
    scene.colour(17, GREEN);
    scene.two_layers(0, 1);
    scene.simple_sprite(OBJECT_WINDOW, 0); // covers x 0..8 on line 0
    scene
        .video
        .write16(reg::DISPCNT, scene.video.dispcnt | (1 << 15));

    // Inside the sprite's shape only the layer behind may draw; outside it, both may.
    scene.effects.write16(
        ereg::WINOUT,
        (Layer::Bg1.bit() << 8) | Layer::Bg0.bit() | Layer::Bg1.bit(),
    );

    let framebuffer = scene.render(0);
    let inside = framebuffer.pixel(0, 0);
    assert_eq!(inside.g, 0xFF, "inside the object window BG1 wins");
    assert_eq!(inside.b, 0x00, "not the backdrop");
    assert_eq!(
        framebuffer.pixel(6, 0).g,
        0xFF,
        "the sprite is 8 wide, so this is still inside it"
    );
}

#[test]
fn a_window_can_exclude_the_sprite_layer_and_let_a_background_win() {
    // Sprites are masked by their own bit, not by a background's, and excluding them must let the
    // background underneath win rather than punching a backdrop hole through it.
    use crate::effects::{reg as ereg, Layer};
    let mut scene = Scene::new(0);
    scene.colour(0, BLUE);
    scene.colour(1, RED); // BG0
    scene.colour(257, GREEN); // sprite palette 0, colour 1
    scene.simple_layer(0, 0, 8);
    scene.simple_sprite(0, 0);

    assert_eq!(
        scene.render(0).pixel(0, 0).g,
        0xFF,
        "with no window the sprite is in front"
    );

    scene.effects.write16(ereg::WIN0H, 0x00F0);
    scene.effects.write16(ereg::WIN0V, 0x00A0);
    scene.effects.write16(ereg::WININ, Layer::Bg0.bit()); // sprites excluded
    scene
        .video
        .write16(reg::DISPCNT, scene.video.dispcnt | (1 << 13));

    let masked = scene.render(0).pixel(0, 0);
    assert_eq!(masked.r, 0xFF, "excluding the sprite lets BG0 win");
    assert_eq!(masked.b, 0x00, "rather than showing the backdrop");
}

#[test]
fn with_no_window_enabled_the_registers_are_not_consulted() {
    use crate::effects::{reg as ereg, Layer};
    let mut scene = Scene::new(0);
    scene.colour(0, BLUE);
    scene.colour(1, RED);
    scene.simple_layer(0, 0, 8);
    // A configuration that would mask everything, with the enable bits left clear.
    scene.effects.write16(ereg::WININ, 0);
    scene.effects.write16(ereg::WINOUT, Layer::Bg1.bit());

    assert_eq!(scene.render(0).pixel(0, 0).r, 0xFF, "still drawn");
}

#[test]
fn darkening_takes_the_selected_layer_toward_black() {
    use crate::effects::{reg as ereg, Layer};
    let mut scene = Scene::new(0);
    scene.colour(0, BLUE);
    scene.colour(1, RED);
    scene.simple_layer(0, 0, 8);

    let undarkened = scene.render(0).pixel(0, 0).r;
    scene
        .effects
        .write16(ereg::BLDCNT, Layer::Bg0.bit() | (3 << 6));
    scene.effects.write16(ereg::BLDY, 8); // half
    let darkened = scene.render(0).pixel(0, 0).r;

    assert_eq!(undarkened, 0xFF);
    assert!(
        darkened < undarkened,
        "{darkened} should be below {undarkened}"
    );
}

#[test]
fn a_layer_that_is_not_a_blend_target_is_left_alone() {
    use crate::effects::{reg as ereg, Layer};
    let mut scene = Scene::new(0);
    scene.colour(0, BLUE);
    scene.colour(1, RED);
    scene.simple_layer(0, 0, 8);
    // Blending is on, but background 0 is not selected.
    scene
        .effects
        .write16(ereg::BLDCNT, Layer::Bg1.bit() | (3 << 6));
    scene.effects.write16(ereg::BLDY, 16);

    assert_eq!(scene.render(0).pixel(0, 0).r, 0xFF, "untouched");
}

#[test]
fn an_affine_sprite_draws_through_its_matrix() {
    // The identity matrix should put it exactly where an ordinary sprite would be, which is the
    // check that the centre-relative transform is not off by half a sprite.
    let mut scene = Scene::new(0);
    scene.colour(0, BLUE);
    scene.colour(257, GREEN);
    scene.simple_sprite(1 << 8, 0); // affine
                                    // Matrix 0 is the identity, in the last halfword of OAM entries 0 through 3.
    for (n, value) in [0x0100u16, 0x0000, 0x0000, 0x0100].iter().enumerate() {
        scene.oam[n * 8 + 6..n * 8 + 8].copy_from_slice(&value.to_le_bytes());
    }

    let framebuffer = scene.render(0);
    assert_eq!(framebuffer.pixel(0, 0).g, 0xFF, "drawn");
    assert_eq!(framebuffer.pixel(7, 0).g, 0xFF, "the whole 8 pixels");
    assert_eq!(framebuffer.pixel(8, 0).b, 0xFF, "and no further");
}

#[test]
fn a_double_size_affine_sprite_covers_twice_the_area() {
    // The extra area exists so a rotation is not clipped by the sprite's own corners, and is
    // deliberately empty until the rotation moves something into it.
    let mut scene = Scene::new(0);
    scene.colour(0, BLUE);
    scene.colour(257, GREEN);
    scene.simple_sprite(3 << 8, 0); // affine, double size
    for (n, value) in [0x0100u16, 0x0000, 0x0000, 0x0100].iter().enumerate() {
        scene.oam[n * 8 + 6..n * 8 + 8].copy_from_slice(&value.to_le_bytes());
    }

    // The artwork sits in the middle of the doubled box, so line 0 of the box maps above the
    // top of the artwork and shows the margin; line 4 is where it starts.
    assert_eq!(
        scene.render(0).pixel(0, 0).b,
        0xFF,
        "the margin, vertically"
    );
    let framebuffer = scene.render(4);
    assert_eq!(framebuffer.pixel(0, 4).b, 0xFF, "the margin, horizontally");
    assert_eq!(framebuffer.pixel(6, 4).g, 0xFF, "and the artwork inside it");
}

#[test]
fn an_alpha_blend_mixes_with_the_layer_underneath_rather_than_the_backdrop() {
    // The general case, which used to be approximated by the backdrop because the scanline buffer
    // keeps only the winning pixel. On Pokémon Emerald's title screen that approximation blended
    // the whole sky and the Rayquaza artwork against black, so the screen came out brown with no
    // artwork visible at all — a complete, plausible, wrong picture rather than a missing one.
    use crate::effects::{reg as ereg, Layer};
    let mut scene = Scene::new(0);
    scene.colour(0, BLUE); // backdrop
    scene.colour(1, RED); // the layers' colour index
    scene.simple_layer(1, 0, 8); // BG1 in front
    scene.simple_layer(0, 1, 9); // BG0 behind it

    // Half of BG1 over half of whatever is below, and BG0 is the only declared lower target.
    scene.effects.write16(
        ereg::BLDCNT,
        Layer::Bg1.bit() | (1 << 6) | (Layer::Bg0.bit() << 8),
    );
    scene.effects.write16(ereg::BLDALPHA, 8 | (8 << 8));

    let blended = scene.render(0).pixel(0, 0);
    assert_eq!(
        (blended.r, blended.g, blended.b),
        (0xFF, 0, 0),
        "red over red is red; against the blue backdrop it would have come out purple"
    );
}

#[test]
fn an_alpha_blend_is_skipped_when_the_layer_underneath_is_not_a_second_target() {
    // Hardware writes the top pixel through unchanged rather than blending it with something it
    // was not told to blend with. Blending anyway is how a layer ends up mixed with the backdrop.
    use crate::effects::{reg as ereg, Layer};
    let mut scene = Scene::new(0);
    scene.colour(0, BLUE);
    scene.colour(1, RED);
    scene.simple_layer(1, 0, 8);
    scene.simple_layer(0, 1, 9);

    // BG1 blends, but only against BG2 — which is not in this scene, so nothing blends.
    scene.effects.write16(
        ereg::BLDCNT,
        Layer::Bg1.bit() | (1 << 6) | (Layer::Bg2.bit() << 8),
    );
    scene.effects.write16(ereg::BLDALPHA, 0);

    assert_eq!(
        scene.render(0).pixel(0, 0).r,
        0xFF,
        "with both weights at zero, a blend that happened would be black"
    );
}

#[test]
fn an_alpha_blend_uses_the_layer_beneath_even_when_it_is_itself_a_first_target() {
    // BLDCNT 1st{OBJ, BG0} 2nd{BG0}: the very common shape where a layer is declared both a blend
    // source and a blend destination. A second pass that composed "what is underneath" by
    // skipping every declared first-target layer treated that as "BG0 does not count" and found
    // nothing beneath the sprite but the backdrop, so a translucent sprite over artwork mixed
    // with black rather than with the artwork it was actually sitting on.
    use crate::effects::{reg as ereg, Layer};
    let mut scene = Scene::new(0);
    scene.colour(0, BLUE); // backdrop; must not appear in the blend
    scene.colour(1, GREEN); // BG0, directly beneath the sprite
    scene.colour(257, RED); // the sprite: palette 0, colour index 1
    scene.simple_layer(0, 1, 8); // BG0 at priority 1, behind the sprite
    scene.sprite_tiles();
    scene.set_sprite(0, 0, 0, 0); // an ordinary sprite at (0, 0), priority 0

    scene.effects.write16(
        ereg::BLDCNT,
        Layer::Object.bit() | Layer::Bg0.bit() | (1 << 6) | (Layer::Bg0.bit() << 8),
    );
    scene.effects.write16(ereg::BLDALPHA, 8 | (8 << 8)); // half and half

    let px = scene.render(0).pixel(0, 0);
    assert!(px.g > 0, "the sprite should have blended with BG0: {px:?}");
    assert_eq!(px.b, 0, "the backdrop took no part in the mix: {px:?}");
}

#[test]
fn a_256_colour_sprite_is_one_byte_a_pixel() {
    // Bit 13 of attribute 0. Every GBA sprite used to be decoded as 16-colour whatever this said,
    // and a 256-colour one then came out as a stretched checkerboard — each byte read as two
    // 4-bit indices — which reads as a corrupt tile rather than as a missing feature. Pokémon
    // Emerald's "EMERALD VERSION" wordmark is drawn this way.
    const EIGHT_BIT: u16 = 1 << 13;
    let mut scene = Scene::new(0);
    scene.colour(0, BLUE);
    scene.colour(256 + 0x11, GREEN); // sprite palette, colour index 0x11

    // One byte per pixel: 0x11 in all eight columns of every row, which a 4bpp decode would read
    // as colour 1 in sixteen half-width columns instead.
    let base = crate::objects::OBJ_TILE_BASE;
    for byte in 0..64 {
        scene.vram[base + byte] = 0x11;
    }
    scene.oam[0..2].copy_from_slice(&EIGHT_BIT.to_le_bytes());
    scene.oam[2..4].copy_from_slice(&0u16.to_le_bytes());
    scene.oam[4..6].copy_from_slice(&0u16.to_le_bytes());
    scene.video.write16(
        reg::DISPCNT,
        scene.video.dispcnt | dispcnt::OBJ | dispcnt::OBJ_1D_MAPPING,
    );

    let frame = scene.render(0);
    for x in 0..8 {
        assert_eq!(
            frame.pixel(x, 0).g,
            0xFF,
            "column {x} should be the 8-bit index 0x11"
        );
    }
    assert_eq!(
        frame.pixel(8, 0).b,
        0xFF,
        "and the sprite is 8 pixels wide, not 16"
    );
}

#[test]
fn colour_zero_in_a_text_layer_lets_the_layer_behind_show_through() {
    // On this machine a background is one of four *layers*, and index 0 is transparent. Writing
    // it made the frontmost enabled text layer opaque across the whole screen: every layer behind
    // it and the backdrop disappeared under flat bands of one palette colour. Menus and text
    // boxes were the worst affected, because their front layer is mostly empty.
    //
    // The Game Boy's rule is the opposite and both are right — see
    // `BackgroundParams::transparent_index_zero`.
    let mut scene = Scene::new(0);
    scene.colour(0, BLUE); // backdrop
    scene.colour(1, RED); // the back layer's colour
    scene.simple_layer(1, 1, 8); // BG1 behind, drawing red

    // BG0 in front, enabled, with a map cell whose tile is entirely colour 0.
    scene
        .backgrounds
        .write16(crate::background::CONTROL_BASE, 0);
    scene
        .video
        .write16(reg::DISPCNT, scene.video.dispcnt | (1 << 8));

    assert_eq!(
        scene.render(0).pixel(0, 0).r,
        0xFF,
        "the red layer behind should show through a transparent front layer"
    );
}

#[test]
fn a_sprite_behind_a_background_by_priority_does_not_cover_it() {
    // The rule this machine actually uses: compare the sprite's priority with the background's.
    // Every GBA sprite used to win against every background, because the Game Boy's rule was
    // applied — a single "behind background" bit that the GBA decoder always leaves false. What
    // that looked like was a character walking *over* the text box in front of them.
    let mut scene = Scene::new(0);
    scene.colour(0, BLUE);
    scene.colour(1, RED); // the background's colour
    scene.colour(257, GREEN); // the sprite's colour
    scene.simple_layer(0, 0, 8); // BG0 at priority 0, the nearest
    scene.simple_sprite(0, 0); // sprite at priority 0 by default

    assert_eq!(
        scene.render(0).pixel(0, 0).g,
        0xFF,
        "at equal priority the sprite is in front"
    );

    // Now put the sprite behind: attribute 2 bits 10-11 carry its priority.
    scene.simple_sprite(0, 2 << 10);
    assert_eq!(
        scene.render(0).pixel(0, 0).r,
        0xFF,
        "a higher priority number is further back, so the background covers it"
    );
}

#[test]
fn a_window_can_switch_the_colour_effect_off_inside_it() {
    // Bit 5 of WININ/WINOUT is not a sixth layer — it is whether colour effects apply in that
    // region at all. Ignoring it darkened everything a game meant to leave alone, which is why
    // menu panels came out grey instead of white.
    use crate::effects::{reg as ereg, Layer};
    let mut scene = Scene::new(0);
    scene.colour(0, BLUE);
    scene.colour(1, RED);
    scene.simple_layer(0, 0, 8);
    scene
        .effects
        .write16(ereg::BLDCNT, Layer::Bg0.bit() | (3 << 6)); // darken BG0
    scene.effects.write16(ereg::BLDY, 16); // fully

    let unwindowed = scene.render(0).pixel(0, 0).r;
    assert!(unwindowed < 0x40, "with no window the darkening applies");

    // Window 0 over the whole screen, with BG0 visible but the effect bit clear.
    scene
        .video
        .write16(reg::DISPCNT, scene.video.dispcnt | (1 << 13));
    scene.effects.write16(ereg::WIN0H, 0x00F0);
    scene.effects.write16(ereg::WIN0V, 0x00A0);
    scene.effects.write16(ereg::WININ, Layer::Bg0.bit());
    assert_eq!(
        scene.render(0).pixel(0, 0).r,
        0xFF,
        "inside the window the effect is switched off, so the layer keeps its colour"
    );
}

#[test]
fn an_object_window_sprite_masks_by_its_shape_rather_than_drawing() {
    // A sprite whose graphics mode is `ObjectWindow` draws nothing; its shape is a region, and
    // `WINOUT`'s high byte says what is visible inside it. Reporting it as never covering is not a
    // neutral default — a game that reveals content *through* one gets a blank region instead.
    // Pokémon Emerald's battle screen puts the action menu there, so the bottom fifty scanlines
    // came out as pure backdrop.
    use crate::effects::{reg as ereg, Layer};
    const OBJECT_WINDOW: u16 = 2 << 10; // attribute 0, bits 10-11

    let mut scene = Scene::new(0);
    scene.colour(0, BLUE); // backdrop
    scene.colour(1, RED); // BG0's colour
    scene.simple_layer(0, 0, 8);
    scene.simple_sprite(OBJECT_WINDOW, 0);
    scene.video.write16(
        reg::DISPCNT,
        scene.video.dispcnt | (1 << 15), // object window enable
    );

    // Nothing at all outside the object window; BG0 inside it.
    scene.effects.write16(ereg::WINOUT, Layer::Bg0.bit() << 8);

    let frame = scene.render(0);
    assert_eq!(
        frame.pixel(0, 0).r,
        0xFF,
        "inside the sprite's shape BG0 shows"
    );
    assert_eq!(
        frame.pixel(100, 0).b,
        0xFF,
        "outside it nothing is visible, so the backdrop shows"
    );
    assert_eq!(
        frame.pixel(0, 0).g,
        0x00,
        "and the sprite itself never draws"
    );
}

#[test]
fn a_background_larger_than_one_screen_block_reaches_its_other_blocks() {
    // `BackgroundParams::full_line` describes a 32x32 map, and the GBA text renderer wraps on the
    // size *it* is given — so a layer left at that default wrapped at half its real size and never
    // reached its second screen block. Pokémon Emerald's battle menu lives in exactly that block,
    // on a 32x64 background scrolled to 320, and the whole bottom of the screen came out as
    // backdrop because line 140 read block 0 instead of block 1.
    use crate::background::SCREEN_BLOCK;
    let mut scene = Scene::new(0);
    scene.colour(0, BLUE);
    scene.colour(1, RED);

    // A solid tile 1, and a map whose *second* screen block names it while the first does not.
    for row in 0..8 {
        scene.vram[0x20 + row * 4..0x20 + row * 4 + 4].copy_from_slice(&[0x11; 4]);
    }
    let block1 = SCREEN_BLOCK; // screen base 0, so the second block starts one block along
    scene.vram[block1..block1 + 2].copy_from_slice(&1u16.to_le_bytes());

    // BG0: screen base 0, size 2 (32x64 tiles), enabled.
    scene
        .backgrounds
        .write16(crate::background::CONTROL_BASE, 2 << 14);
    scene
        .video
        .write16(reg::DISPCNT, scene.video.dispcnt | (1 << 8));
    // Scroll down by 256 pixels: the top of the screen is now the top of the second block.
    scene.backgrounds.write16(0x0400_0012, 256); // BG0VOFS

    assert_eq!(
        scene.render(0).pixel(0, 0).r,
        0xFF,
        "scrolled into the second screen block, its tile should be what draws"
    );
}

// -- Affine and plain sprites in one pass ----------------------------------

#[test]
fn an_affine_sprite_obeys_background_priority_in_both_directions() {
    // The affine path wrote pixels straight into the buffer with no comparison against the
    // background at all, so a rotated object punched through whatever was in front of it. Both
    // directions are asserted because only writing unconditionally passes one of them.
    let mut scene = Scene::new(0);
    scene.colour(0, BLUE);
    scene.colour(1, RED); // BG0
    scene.colour(257, GREEN); // the sprite
    scene.sprite_tiles();
    scene.identity_matrix();

    // BG0 at priority 0, affine sprite at priority 3: the background is in front.
    scene.simple_layer(0, 0, 8);
    scene.set_sprite(0, AFFINE, 0, 3 << 10);
    assert_eq!(
        scene.render(0).pixel(0, 0).r,
        0xFF,
        "a priority-3 affine sprite is behind a priority-0 background"
    );

    // The other way round: BG0 at priority 3, sprite at priority 0.
    let mut scene = Scene::new(0);
    scene.colour(0, BLUE);
    scene.colour(1, RED);
    scene.colour(257, GREEN);
    scene.sprite_tiles();
    scene.identity_matrix();
    scene.simple_layer(0, 3, 8);
    scene.set_sprite(0, AFFINE, 0, 0);
    assert_eq!(
        scene.render(0).pixel(0, 0).g,
        0xFF,
        "and a priority-0 affine sprite is in front of a priority-3 background"
    );
}

#[test]
fn a_plain_sprite_does_not_overwrite_an_affine_one_that_wins_on_oam_order() {
    // The two paths could not see each other: the shared renderer treated only a *background*
    // pixel as something it could lose to, so an affine sprite's pixel was overwritten by any
    // plain sprite regardless of which was in front. Equal priority, so OAM index decides.
    let mut scene = Scene::new(0);
    scene.colour(0, BLUE);
    scene.colour(257, GREEN); // palette 0 — the affine sprite
    scene.colour(273, RED); // palette 1 — the plain sprite
    scene.sprite_tiles();
    scene.identity_matrix();

    scene.set_sprite(0, AFFINE, 0, 0); // affine, OAM 0
    scene.set_sprite(4, 0, 0, PALETTE_1); // plain, OAM 4
    let px = scene.render(0).pixel(0, 0);
    assert_eq!(px.g, 0xFF, "the lower OAM index wins, affine or not");
    assert_eq!(px.r, 0x00, "the farther plain sprite did not overwrite it");
}

#[test]
fn an_affine_sprite_does_not_overwrite_a_plain_one_that_wins_on_oam_order() {
    // The same rule the other way, which is the half that a back-to-front affine pass got wrong:
    // it drew affine sprites *over* everything already placed.
    let mut scene = Scene::new(0);
    scene.colour(0, BLUE);
    scene.colour(257, GREEN); // palette 0 — the affine sprite
    scene.colour(273, RED); // palette 1 — the plain sprite
    scene.sprite_tiles();
    scene.identity_matrix();

    scene.set_sprite(0, 0, 0, PALETTE_1); // plain, OAM 0
    scene.set_sprite(4, AFFINE, 0, 0); // affine, OAM 4
    let px = scene.render(0).pixel(0, 0);
    assert_eq!(
        px.r, 0xFF,
        "the plain sprite at the lower index keeps the pixel"
    );
    assert_eq!(px.g, 0x00, "the farther affine sprite did not overwrite it");
}

#[test]
fn a_semi_transparent_sprite_blends_even_when_objects_are_not_a_declared_blend_source() {
    // The force path. A semi-transparent OBJ is a first target whatever `BLDCNT` selects and
    // blends whatever mode it asks for — here the mode is "none" and the OBJ first-target bit is
    // clear, so nothing but the graphics mode can be producing a blend. The mode was decoded and
    // never read, and these sprites — shadows, water, reflections, battle-move flashes — came out
    // as solid blocks.
    use crate::effects::{reg as ereg, Layer};
    let mut scene = Scene::new(0);
    scene.colour(0, BLUE);
    scene.colour(1, RED); // BG0, underneath
    scene.colour(257, GREEN); // the sprite
    scene.simple_layer(0, 1, 8); // BG0 at priority 1, behind the sprite
    scene.sprite_tiles();
    scene.set_sprite(0, SEMI_TRANSPARENT, 0, 0);

    // BG0 is a second target; OBJ is *not* a first target and the mode is none.
    scene.effects.write16(ereg::BLDCNT, Layer::Bg0.bit() << 8);
    scene.effects.write16(ereg::BLDALPHA, 8 | (8 << 8)); // half and half

    let px = scene.render(0).pixel(0, 0);
    assert!(
        px.g > 0 && px.r > 0,
        "the sprite blended with BG0 rather than replacing it: {px:?}"
    );
    assert!(px.g < 0xFF, "and it is a mix, not the sprite's own colour");
}

#[test]
fn a_semi_transparent_sprite_is_not_brightened_by_an_active_brightness_effect() {
    // Semi-transparency forces an *alpha* blend, which is what stops a brightness effect applying
    // to it. With no second target declared there is nothing to blend with, so the sprite keeps
    // its own colour — while an otherwise identical normal sprite is taken to white.
    use crate::effects::{reg as ereg, Layer};
    let build = |semi: bool| {
        let mut scene = Scene::new(0);
        scene.colour(0, BLUE);
        scene.colour(257, GREEN);
        scene.sprite_tiles();
        scene.set_sprite(0, if semi { SEMI_TRANSPARENT } else { 0 }, 0, 0);
        // Brighten, with OBJ as the first target and no second target at all.
        scene
            .effects
            .write16(ereg::BLDCNT, Layer::Object.bit() | (2 << 6));
        scene.effects.write16(ereg::BLDY, 16); // fully toward white
        scene
    };

    let normal = build(false).render(0).pixel(0, 0);
    assert_eq!(
        (normal.r, normal.g, normal.b),
        (0xFF, 0xFF, 0xFF),
        "a normal sprite is taken to white"
    );

    let semi = build(true).render(0).pixel(0, 0);
    assert_eq!(
        (semi.r, semi.g, semi.b),
        (0x00, 0xFF, 0x00),
        "a semi-transparent one keeps its colour instead of being brightened"
    );
}

#[test]
fn an_eight_bit_sprite_in_two_dimensional_mapping_finds_its_second_row() {
    // In 2D mapping the object sheet is 32 *slots* wide and a slot is 32 bytes whatever the
    // depth, so one row down is 1024 bytes for every sprite. Scaling by the sprite's own tile size
    // gave 2048 for a 256-colour sprite: its top row decoded correctly and every row below came
    // from the wrong place, which reads as scrambled artwork rather than a mapping bug.
    let mut scene = Scene::new(0);
    scene.colour(0, BLUE);
    scene.colour(257, GREEN); // 8bpp forces palette 0, so colour 1 is entry 257
    scene.colour(258, RED); // and colour 2 is entry 258

    let base = crate::objects::OBJ_TILE_BASE;
    for byte in 0..8 {
        scene.vram[base + byte] = 1; // the sprite's first row
        scene.vram[base + 1024 + byte] = 2; // one sheet row on, not two
    }

    // 8x16 (shape 2, size 0), 256 colours, at the origin.
    scene.set_sprite(0, (2 << 14) | (1 << 13), 0, 0);
    // Objects on, and *two*-dimensional mapping: no OBJ_1D_MAPPING bit.
    scene
        .video
        .write16(reg::DISPCNT, scene.video.dispcnt | dispcnt::OBJ);

    assert_eq!(scene.render(0).pixel(0, 0).g, 0xFF, "the first tile row");
    assert_eq!(
        scene.render(8).pixel(0, 8).r,
        0xFF,
        "and the second comes from 1024 bytes on"
    );
}

// -- Mosaic ---------------------------------------------------------------

/// A 4bpp tile whose eight columns read 1, 2, 3, 1, 2, 3, 1, 2 in every row.
///
/// Backgrounds and sprites are both 4bpp or 8bpp on this machine — there is no 2bpp mode, unlike
/// the Game Boy — so one striped tile shape serves both. Index 0 is deliberately absent: it is
/// this machine's transparency, and a mosaic block that happened to land on it would be
/// indistinguishable from a hole rather than a held colour.
fn striped_tile_4bpp() -> [u8; 32] {
    let mut tile = [0u8; 32];
    for row in 0..8 {
        tile[row * 4] = 0x21; // pixel0=1 (low nibble), pixel1=2 (high nibble)
        tile[row * 4 + 1] = 0x13; // pixel2=3, pixel3=1
        tile[row * 4 + 2] = 0x32; // pixel4=2, pixel5=3
        tile[row * 4 + 3] = 0x21; // pixel6=1, pixel7=2
    }
    tile
}

const WHITE: u16 = 0x7FFF;

#[test]
fn horizontal_bg_mosaic_holds_each_source_column_across_its_block() {
    // Hardware's sample-and-hold: with a horizontal block size of two, screen columns 0-1 must
    // both show source column 0's colour and 2-3 must both show column 2's.
    use crate::effects::reg as ereg;
    let mut scene = Scene::new(0);
    scene.colour(0, BLUE);
    scene.colour(1, RED);
    scene.colour(2, GREEN);
    scene.colour(3, WHITE);
    scene.vram[0x20..0x40].copy_from_slice(&striped_tile_4bpp());
    let cell = 8 * SCREEN_BLOCK;
    scene.vram[cell..cell + 2].copy_from_slice(&1u16.to_le_bytes());
    scene.backgrounds.write16(
        crate::background::CONTROL_BASE,
        (8 << 8) | 1 << 6, /* BGxCNT mosaic bit */
    );
    scene
        .video
        .write16(reg::DISPCNT, scene.video.dispcnt | (1 << 8));
    scene.effects.write16(ereg::MOSAIC, 1); // BG H-size field 1 -> block size 2

    let frame = scene.render(0);
    assert_eq!(
        frame.pixel(0, 0).r,
        0xFF,
        "column 0, unmosaiced source colour"
    );
    assert_eq!(frame.pixel(1, 0).r, 0xFF, "held to column 0's colour");
    assert_eq!(frame.pixel(2, 0), Rgba8::WHITE, "column 2, its own block");
    assert_eq!(frame.pixel(3, 0), Rgba8::WHITE, "held to column 2's colour");
}

#[test]
fn vertical_bg_mosaic_holds_each_source_row_across_its_block() {
    // The same effect down the screen: with a vertical block size of two, line 1 must show line
    // 0's row rather than its own, because a rendered line has no notion of "next" to hold from
    // — the previous rendered line simply keeps being redrawn until the block ends.
    use crate::effects::reg as ereg;
    let mut scene = Scene::new(0);
    scene.colour(0, BLUE);
    scene.colour(1, RED);
    scene.colour(2, GREEN);
    // Row 0 solid colour 1, row 1 solid colour 2 (4bpp: one byte holds two same-index pixels).
    scene.vram[0x20..0x24].copy_from_slice(&[0x11; 4]);
    scene.vram[0x24..0x28].copy_from_slice(&[0x22; 4]);
    let cell = 8 * SCREEN_BLOCK;
    scene.vram[cell..cell + 2].copy_from_slice(&1u16.to_le_bytes());
    scene.backgrounds.write16(
        crate::background::CONTROL_BASE,
        (8 << 8) | 1 << 6, /* BGxCNT mosaic bit */
    );
    scene
        .video
        .write16(reg::DISPCNT, scene.video.dispcnt | (1 << 8));
    scene.effects.write16(ereg::MOSAIC, 1 << 4); // BG V-size field 1 -> block size 2

    assert_eq!(scene.render(0).pixel(0, 0).r, 0xFF, "line 0 samples row 0");
    assert_eq!(
        scene.render(1).pixel(0, 1).r,
        0xFF,
        "line 1 is held to row 0's colour, not row 1's"
    );
}

#[test]
fn a_bg_mosaic_size_of_zero_is_exactly_the_unmosaiced_picture() {
    // The regression that matters: the mosaic bit being *on* with every size field at zero must
    // produce the identical picture to mosaic being off, since a one-pixel block changes nothing.
    use crate::effects::reg as ereg;
    let build = |mosaic_on: bool| {
        let mut scene = Scene::new(0);
        scene.colour(0, BLUE);
        scene.colour(1, RED);
        scene.colour(2, GREEN);
        scene.colour(3, WHITE);
        scene.vram[0x20..0x40].copy_from_slice(&striped_tile_4bpp());
        let cell = 8 * SCREEN_BLOCK;
        scene.vram[cell..cell + 2].copy_from_slice(&1u16.to_le_bytes());
        let control = (8u16 << 8)
            | if mosaic_on {
                1 << 6 /* BGxCNT mosaic bit */
            } else {
                0
            };
        scene
            .backgrounds
            .write16(crate::background::CONTROL_BASE, control);
        scene
            .video
            .write16(reg::DISPCNT, scene.video.dispcnt | (1 << 8));
        scene.effects.write16(ereg::MOSAIC, 0);
        scene
    };

    let plain = build(false).render(0);
    let mosaiced = build(true).render(0);
    for x in 0..8 {
        assert_eq!(
            plain.pixel(x, 0),
            mosaiced.pixel(x, 0),
            "column {x}: a zero-size block must not change anything"
        );
    }
}

#[test]
fn horizontal_obj_mosaic_holds_each_local_column_across_its_block() {
    // The sprite's own eight-column stripe, exactly as the background test uses, but anchored to
    // the sprite's local origin rather than the screen's — see `draw_mosaic_sprite`.
    use crate::effects::reg as ereg;
    let mut scene = Scene::new(0);
    scene.colour(0, BLUE);
    scene.colour(257, RED); // sprite palette 0, index 1
    scene.colour(258, GREEN); // index 2
    scene.colour(259, WHITE); // index 3
    let base = crate::objects::OBJ_TILE_BASE;
    scene.vram[base..base + 32].copy_from_slice(&striped_tile_4bpp());
    scene.video.write16(
        reg::DISPCNT,
        scene.video.dispcnt | dispcnt::OBJ | dispcnt::OBJ_1D_MAPPING,
    );
    scene.set_sprite(0, 1 << 12, 0, 0); // mosaic bit, plain sprite, at the origin
    scene.effects.write16(ereg::MOSAIC, 1 << 8); // OBJ H-size field 1 -> block size 2

    let frame = scene.render(0);
    assert_eq!(
        frame.pixel(0, 0).r,
        0xFF,
        "column 0, unmosaiced source colour"
    );
    assert_eq!(frame.pixel(1, 0).r, 0xFF, "held to column 0's colour");
    assert_eq!(frame.pixel(2, 0), Rgba8::WHITE, "column 2, its own block");
    assert_eq!(frame.pixel(3, 0), Rgba8::WHITE, "held to column 2's colour");
}

#[test]
fn vertical_obj_mosaic_holds_each_local_row_across_its_block() {
    use crate::effects::reg as ereg;
    let mut scene = Scene::new(0);
    scene.colour(0, BLUE);
    scene.colour(257, RED);
    scene.colour(258, GREEN);
    let base = crate::objects::OBJ_TILE_BASE;
    // Row 0 solid colour 1, row 1 solid colour 2.
    scene.vram[base..base + 4].copy_from_slice(&[0x11; 4]);
    scene.vram[base + 4..base + 8].copy_from_slice(&[0x22; 4]);
    scene.video.write16(
        reg::DISPCNT,
        scene.video.dispcnt | dispcnt::OBJ | dispcnt::OBJ_1D_MAPPING,
    );
    scene.set_sprite(0, 1 << 12, 0, 0);
    scene.effects.write16(ereg::MOSAIC, 1 << 12); // OBJ V-size field 1 -> block size 2

    assert_eq!(scene.render(0).pixel(0, 0).r, 0xFF, "line 0 samples row 0");
    assert_eq!(
        scene.render(1).pixel(0, 1).r,
        0xFF,
        "line 1 is held to row 0's colour, not row 1's"
    );
}

#[test]
fn an_obj_mosaic_size_of_zero_is_exactly_the_unmosaiced_picture() {
    use crate::effects::reg as ereg;
    let build = |mosaic_on: bool| {
        let mut scene = Scene::new(0);
        scene.colour(0, BLUE);
        scene.colour(257, RED);
        scene.colour(258, GREEN);
        scene.colour(259, WHITE);
        let base = crate::objects::OBJ_TILE_BASE;
        scene.vram[base..base + 32].copy_from_slice(&striped_tile_4bpp());
        scene.video.write16(
            reg::DISPCNT,
            scene.video.dispcnt | dispcnt::OBJ | dispcnt::OBJ_1D_MAPPING,
        );
        let attr0 = if mosaic_on { 1 << 12 } else { 0 };
        scene.set_sprite(0, attr0, 0, 0);
        scene.effects.write16(ereg::MOSAIC, 0);
        scene
    };

    let plain = build(false).render(0);
    let mosaiced = build(true).render(0);
    for x in 0..8 {
        assert_eq!(
            plain.pixel(x, 0),
            mosaiced.pixel(x, 0),
            "column {x}: a zero-size OBJ block must not change anything"
        );
    }
}
