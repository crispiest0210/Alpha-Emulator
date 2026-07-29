use super::*;
use crate::background::SCREEN_BLOCK;
use crate::video::{dispcnt, reg};

const RED: u16 = 0x001F;
const GREEN: u16 = 0x03E0;
const BLUE: u16 = 0x7C00;

struct Scene {
    video: VideoTiming,
    backgrounds: Backgrounds,
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
            vram: vec![0u8; 0x1_8000],
            palette: vec![0u8; 0x400],
            oam: vec![0u8; 0x400],
        }
    }

    fn colour(&mut self, index: usize, value: u16) {
        self.palette[index * 2..index * 2 + 2].copy_from_slice(&value.to_le_bytes());
    }

    /// Fill tile 1 with colour index 1 and point layer `index`'s map cell (0,0) at it.
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

    fn frame(&self) -> Frame<'_> {
        Frame {
            video: &self.video,
            backgrounds: &self.backgrounds,
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
    scene.vram[0..2].copy_from_slice(&GREEN.to_le_bytes());
    assert_eq!(scene.render(0).pixel(0, 0).g, 0xFF);
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
fn an_affine_layer_leaves_the_backdrop_rather_than_drawing_with_no_transform() {
    // Mode 2's layers are affine, and the matrix accumulation is driven from the system
    // assembly, which does not exist yet. Drawing them as text layers would put a picture on
    // screen that is wrong in a way that looks deliberate.
    let mut scene = Scene::new(2);
    scene.colour(0, BLUE);
    scene.colour(1, RED);
    scene.simple_layer(2, 0, 8);
    assert_eq!(scene.render(0).pixel(0, 0).b, 0xFF);
}

#[test]
fn the_drawn_predicate_distinguishes_the_backdrop_from_a_layer() {
    assert!(!was_drawn(PixelSource::Backdrop));
    assert!(was_drawn(PixelSource::Background));
    assert!(was_drawn(PixelSource::Sprite));
}
