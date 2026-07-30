use super::*;
use crate::memory::{OAM_SIZE, PALETTE_SIZE};
use crate::vram::VramSpace;

/// A machine's worth of graphics memory, without the machine.
struct Gfx {
    vram: Vram,
    palette: Vec<u8>,
    oam: Vec<u8>,
    out: Vec<u8>,
}

impl Gfx {
    fn new() -> Self {
        Self {
            vram: Vram::new(),
            palette: vec![0; PALETTE_SIZE],
            oam: vec![0; OAM_SIZE],
            out: vec![0; SCREEN_WIDTH as usize * 4],
        }
    }

    /// Map a bank into a space with the given MST and OFS.
    fn map(&mut self, bank: usize, mst: u8, ofs: u8) {
        self.vram.set_control(bank, 0x80 | (ofs << 3) | mst);
    }

    fn set_palette(&mut self, engine: Engine, index: usize, color: u16) {
        let base = engine.block_offset() + index * 2;
        self.palette[base] = color as u8;
        self.palette[base + 1] = (color >> 8) as u8;
    }

    fn set_sprite_palette(&mut self, engine: Engine, index: usize, color: u16) {
        self.set_palette(engine, 0x100 + index, color);
    }

    fn write_oam(&mut self, engine: Engine, entry: usize, attrs: [u16; 3]) {
        let base = engine.block_offset() + entry * 8;
        for (i, attr) in attrs.iter().enumerate() {
            self.oam[base + i * 2] = *attr as u8;
            self.oam[base + i * 2 + 1] = (*attr >> 8) as u8;
        }
    }

    fn pixel(&self, x: usize) -> [u8; 4] {
        self.out[x * 4..x * 4 + 4].try_into().unwrap()
    }
}

/// Register accessors relative to the engine's own base, so a test reads like the register map
/// rather than like an address calculation.
impl Engine2d {
    fn w16(&mut self, offset: u32, value: u16) {
        let base = self.engine().base();
        assert!(self.write16(base + offset, value));
    }

    fn w32(&mut self, offset: u32, value: u32) {
        let base = self.engine().base();
        assert!(self.write32(base + offset, value));
    }

    fn r16(&self, offset: u32) -> Option<u16> {
        self.read16(self.engine().base() + offset)
    }

    fn r32(&self, offset: u32) -> Option<u32> {
        self.read32(self.engine().base() + offset)
    }
}

fn rgba(color: u16) -> [u8; 4] {
    let c = ppu_tile2d::bgr555_to_rgba(color);
    [c.r, c.g, c.b, c.a]
}

const WHITE15: u16 = 0x7FFF;
const RED15: u16 = 0x001F;
const GREEN15: u16 = 0x03E0;
const BLUE15: u16 = 0x7C00;

/// The minimum a background needs: graphics display mode, mode 0, BG0 on.
fn text_engine(engine: Engine) -> Engine2d {
    let mut e = Engine2d::new(engine);
    e.write32(engine.base() + reg::DISPCNT, (1 << 16) | (1 << 8));
    e
}

/// Fill one 8x8 4bpp tile with colour index `index`.
fn fill_tile_4bpp(gfx: &mut Gfx, space: VramSpace, offset: u32, index: u8) {
    let byte = index | (index << 4);
    for i in 0..32 {
        gfx.vram.write8(space, offset + i, byte);
    }
}

#[test]
fn the_two_engines_differ_only_where_the_hardware_does() {
    assert_eq!(Engine::A.base(), 0x0400_0000);
    assert_eq!(Engine::B.base(), 0x0400_1000);
    assert_eq!(Engine::A.block_offset(), 0);
    assert_eq!(Engine::B.block_offset(), 0x400);
    assert_eq!(Engine::A.bg_space(), VramSpace::BgA);
    assert_eq!(Engine::B.bg_space(), VramSpace::BgB);
    assert_eq!(Engine::A.obj_ext_pal_space(), VramSpace::ObjExtPalA);
    assert_eq!(Engine::B.obj_ext_pal_space(), VramSpace::ObjExtPalB);

    // Engine B has no character or screen base offset and no 3D layer, so those bits do not
    // stick when written.
    let mut b = Engine2d::new(Engine::B);
    b.write32(Engine::B.base() + reg::DISPCNT, 0xFFFF_FFFF);
    assert_eq!(b.dispcnt() & dispcnt::CHAR_BASE, 0);
    assert_eq!(b.dispcnt() & dispcnt::SCREEN_BASE, 0);
    assert_eq!(b.dispcnt() & dispcnt::BG0_IS_3D, 0);

    let mut a = Engine2d::new(Engine::A);
    a.write32(Engine::A.base() + reg::DISPCNT, 0xFFFF_FFFF);
    assert_ne!(a.dispcnt() & dispcnt::CHAR_BASE, 0);
    assert_ne!(a.dispcnt() & dispcnt::BG0_IS_3D, 0);
}

#[test]
fn the_background_mode_table_matches_the_hardwares() {
    use BackgroundKind::*;
    let kind = |mode, layer, bgcnt| BackgroundKind::of(Engine::A, mode, layer, 1 << 16, bgcnt);

    assert_eq!(
        [0, 1, 2, 3].map(|l| kind(0, l, 0)),
        [Text, Text, Text, Text]
    );
    assert_eq!(
        [0, 1, 2, 3].map(|l| kind(1, l, 0)),
        [Text, Text, Text, Affine]
    );
    assert_eq!(
        [0, 1, 2, 3].map(|l| kind(2, l, 0)),
        [Text, Text, Affine, Affine]
    );
    assert_eq!(
        [0, 1, 2, 3].map(|l| kind(3, l, 0)),
        [Text, Text, Text, ExtendedRotscale]
    );
    assert_eq!(
        [0, 1, 2, 3].map(|l| kind(4, l, 0)),
        [Text, Text, Affine, ExtendedRotscale]
    );
    assert_eq!(
        [0, 1, 2, 3].map(|l| kind(5, l, 0)),
        [Text, Text, ExtendedRotscale, ExtendedRotscale]
    );

    // The three extended sub-types come from two BGxCNT bits that mean something else in every
    // other mode.
    assert_eq!(kind(5, 2, 0x80), ExtendedBitmap);
    assert_eq!(kind(5, 2, 0x84), ExtendedDirectBitmap);

    // Mode 6 is engine A only, and is two layers wide.
    assert_eq!(
        [0, 1, 2, 3].map(|l| kind(6, l, 0)),
        [ThreeD, None, LargeBitmap, None]
    );
    assert_eq!(
        BackgroundKind::of(Engine::B, 6, 2, 1 << 16, 0),
        None,
        "engine B has no mode 6"
    );

    // BG0 becomes the 3D layer in any mode, on engine A only.
    let three_d = (1 << 16) | dispcnt::BG0_IS_3D;
    assert_eq!(BackgroundKind::of(Engine::A, 0, 0, three_d, 0), ThreeD);
    assert_eq!(BackgroundKind::of(Engine::B, 0, 0, three_d, 0), Text);
}

#[test]
fn display_mode_zero_is_white_rather_than_black() {
    // A game leaves the display off during boot, and rendering that as black makes the console
    // look broken rather than idle.
    let mut gfx = Gfx::new();
    let mut e = Engine2d::new(Engine::A);
    e.render_line(0, &gfx.vram, &gfx.palette, &gfx.oam, &mut gfx.out);
    assert_eq!(gfx.pixel(0), [255, 255, 255, 255]);

    // And so is forced blank.
    let mut e2 = text_engine(Engine::A);
    e2.w32(reg::DISPCNT, (1 << 16) | dispcnt::FORCED_BLANK);
    e2.render_line(0, &gfx.vram, &gfx.palette, &gfx.oam, &mut gfx.out);
    assert_eq!(gfx.pixel(0), [255, 255, 255, 255]);
    let _ = &mut e;
}

#[test]
fn nothing_drawn_shows_the_backdrop() {
    let mut gfx = Gfx::new();
    gfx.set_palette(Engine::A, 0, BLUE15);
    let mut e = text_engine(Engine::A);
    e.render_line(0, &gfx.vram, &gfx.palette, &gfx.oam, &mut gfx.out);
    assert_eq!(gfx.pixel(0), rgba(BLUE15));
    assert_eq!(gfx.pixel(255), rgba(BLUE15));
}

#[test]
fn a_text_background_draws_from_the_bank_currently_mapped_to_it() {
    let mut gfx = Gfx::new();
    // Bank A into engine A's background space.
    gfx.map(0, 1, 0);
    gfx.set_palette(Engine::A, 0, BLUE15);
    gfx.set_palette(Engine::A, 1, RED15);

    let mut e = text_engine(Engine::A);
    // Tile data at char base 0, tilemap at screen base block 1 (0x800).
    e.w16(reg::BGCNT, 1 << 8);
    // Map cell (0,0) points at tile 1. Every other cell is zero and so points at tile 0, which
    // is left blank — an all-zero tilemap is 32 copies of tile 0, not an empty row.
    gfx.vram.write16(VramSpace::BgA, 0x800, 1);
    fill_tile_4bpp(&mut gfx, VramSpace::BgA, 0x20, 1);

    e.render_line(0, &gfx.vram, &gfx.palette, &gfx.oam, &mut gfx.out);
    assert_eq!(gfx.pixel(0), rgba(RED15), "the tile");
    assert_eq!(gfx.pixel(8), rgba(BLUE15), "and the backdrop beside it");

    // Unmapping the bank leaves the layer reading zeroes, so the backdrop returns. This is what
    // makes the VRAM mapping observable through the renderer rather than only through `Vram`.
    gfx.vram.set_control(0, 0);
    e.render_line(0, &gfx.vram, &gfx.palette, &gfx.oam, &mut gfx.out);
    assert_eq!(gfx.pixel(0), rgba(BLUE15));
}

#[test]
fn scroll_registers_move_the_map_under_the_screen() {
    let mut gfx = Gfx::new();
    gfx.map(0, 1, 0);
    gfx.set_palette(Engine::A, 0, BLUE15);
    gfx.set_palette(Engine::A, 1, RED15);
    let mut e = text_engine(Engine::A);
    e.w16(reg::BGCNT, 1 << 8);
    gfx.vram.write16(VramSpace::BgA, 0x800, 1);
    fill_tile_4bpp(&mut gfx, VramSpace::BgA, 0x20, 1);

    // Scroll four pixels left: the tile now occupies x 0..4 and the empty cell beside it takes
    // over from x 4.
    e.w16(reg::BGOFS, 4);
    e.render_line(0, &gfx.vram, &gfx.palette, &gfx.oam, &mut gfx.out);
    assert_eq!(gfx.pixel(3), rgba(RED15));
    assert_eq!(gfx.pixel(4), rgba(BLUE15));

    // And scrolling vertically past the tile's eight rows loses it entirely.
    e.w16(reg::BGOFS, 0);
    e.w16(reg::BGOFS + 2, 8);
    e.render_line(0, &gfx.vram, &gfx.palette, &gfx.oam, &mut gfx.out);
    assert_eq!(gfx.pixel(0), rgba(BLUE15));
}

#[test]
fn a_wide_tilemap_is_blocks_side_by_side_not_one_flat_grid() {
    // A 64-tile-wide map is two 2 KiB blocks, so the cell at tile x=32 is at the start of the
    // second block rather than 32 cells into the first. Treating it as flat puts the right half
    // of the screen 32 tiles too far along.
    let mut gfx = Gfx::new();
    gfx.map(0, 1, 0);
    gfx.set_palette(Engine::A, 0, BLUE15);
    gfx.set_palette(Engine::A, 1, RED15);
    fill_tile_4bpp(&mut gfx, VramSpace::BgA, 0x40, 1); // tile 2

    let mut e = text_engine(Engine::A);
    e.w16(reg::BGCNT, (1 << 8) | (1 << 14)); // screen base 1, size 1 (512x256)
                                             // Second block, cell (0,0): tile 2.
    gfx.vram.write16(VramSpace::BgA, 0x800 + 0x800, 2);
    // Scroll so screen x 0 is map tile x 32.
    e.w16(reg::BGOFS, 32 * 8);

    e.render_line(0, &gfx.vram, &gfx.palette, &gfx.oam, &mut gfx.out);
    assert_eq!(gfx.pixel(0), rgba(RED15));
}

#[test]
fn an_eight_bit_background_uses_the_extended_palette_when_it_is_switched_on() {
    let mut gfx = Gfx::new();
    gfx.map(0, 1, 0); // A -> BgA
    gfx.map(4, 4, 0); // E -> BgExtPalA
    gfx.set_palette(Engine::A, 0, BLUE15);
    gfx.set_palette(Engine::A, 5, RED15);
    // Slot 0, palette 3, index 5.
    gfx.vram
        .write16(VramSpace::BgExtPalA, (3 * 256 + 5) * 2, GREEN15);

    let mut e = text_engine(Engine::A);
    e.w16(reg::BGCNT, (1 << 8) | 0x80); // 8bpp
    gfx.vram.write16(VramSpace::BgA, 0x800, 3 << 12); // tile 0, palette 3
    for i in 0..64 {
        gfx.vram.write8(VramSpace::BgA, i, 5);
    }

    // Extended palettes off: the palette field is ignored and palette RAM index 5 is used.
    e.render_line(0, &gfx.vram, &gfx.palette, &gfx.oam, &mut gfx.out);
    assert_eq!(gfx.pixel(0), rgba(RED15));

    e.w32(reg::DISPCNT, e.dispcnt() | dispcnt::BG_EXT_PALETTE);
    e.render_line(0, &gfx.vram, &gfx.palette, &gfx.oam, &mut gfx.out);
    assert_eq!(
        gfx.pixel(0),
        rgba(GREEN15),
        "now the entry's palette counts"
    );
}

#[test]
fn an_affine_background_transforms_the_map() {
    let mut gfx = Gfx::new();
    gfx.map(0, 1, 0);
    gfx.set_palette(Engine::A, 0, BLUE15);
    gfx.set_palette(Engine::A, 7, RED15);

    let mut e = Engine2d::new(Engine::A);
    // Mode 2 makes BG2 affine; enable BG2 only.
    e.w32(reg::DISPCNT, (1 << 16) | (1 << 10) | 2);
    e.w16(reg::BGCNT + 4, 1 << 8); // BG2: screen base 1
                                   // 16x16 tiles of map (size 0); cell 0 is tile 1, and tile 0 is left blank so the rest of
                                   // the all-zero map draws nothing.
    gfx.vram.write8(VramSpace::BgA, 0x800, 1);
    for i in 64..128 {
        gfx.vram.write8(VramSpace::BgA, i, 7);
    }
    // Identity matrix, origin at 0.
    e.w16(reg::BG2PA, 0x100);
    e.w16(reg::BG2PA + 6, 0x100);
    e.on_frame_start();

    e.render_line(0, &gfx.vram, &gfx.palette, &gfx.oam, &mut gfx.out);
    assert_eq!(gfx.pixel(0), rgba(RED15));
    assert_eq!(gfx.pixel(7), rgba(RED15));
    assert_eq!(gfx.pixel(8), rgba(BLUE15), "past the one tile");

    // Doubling PA halves the sampling rate, so the tile covers sixteen screen pixels.
    e.w16(reg::BG2PA, 0x80);
    e.render_line(0, &gfx.vram, &gfx.palette, &gfx.oam, &mut gfx.out);
    assert_eq!(gfx.pixel(15), rgba(RED15));
    assert_eq!(gfx.pixel(16), rgba(BLUE15));
}

#[test]
fn an_affine_background_outside_its_map_is_transparent_unless_it_wraps() {
    let mut gfx = Gfx::new();
    gfx.map(0, 1, 0);
    gfx.set_palette(Engine::A, 0, BLUE15);
    gfx.set_palette(Engine::A, 7, RED15);

    let mut e = Engine2d::new(Engine::A);
    e.w32(reg::DISPCNT, (1 << 16) | (1 << 10) | 2);
    e.w16(reg::BGCNT + 4, 1 << 8);
    gfx.vram.write8(VramSpace::BgA, 0x800, 1);
    for i in 64..128 {
        gfx.vram.write8(VramSpace::BgA, i, 7);
    }
    e.w16(reg::BG2PA, 0x100);
    e.w16(reg::BG2PA + 6, 0x100);
    // Start one map (128 pixels) to the right: outside a 16x16-tile map.
    e.w32(reg::BG2PA + 8, 128 << 8);
    e.on_frame_start();

    e.render_line(0, &gfx.vram, &gfx.palette, &gfx.oam, &mut gfx.out);
    assert_eq!(gfx.pixel(0), rgba(BLUE15), "off the map, transparent");

    e.w16(reg::BGCNT + 4, (1 << 8) | (1 << 13)); // wrap
    e.render_line(0, &gfx.vram, &gfx.palette, &gfx.oam, &mut gfx.out);
    assert_eq!(gfx.pixel(0), rgba(RED15), "wrapped back onto tile 0");
}

#[test]
fn a_direct_colour_bitmap_background_uses_bit_fifteen_as_alpha() {
    let mut gfx = Gfx::new();
    gfx.map(0, 1, 0);
    gfx.set_palette(Engine::A, 0, BLUE15);

    let mut e = Engine2d::new(Engine::A);
    e.w32(reg::DISPCNT, (1 << 16) | (1 << 10) | 5); // mode 5, BG2 on
    e.w16(reg::BGCNT + 4, 0x84); // extended, direct colour, base block 0
    e.w16(reg::BG2PA, 0x100);
    e.w16(reg::BG2PA + 6, 0x100);
    e.on_frame_start();

    // Pixel 0 opaque green, pixel 1 the same colour with the alpha bit clear.
    gfx.vram.write16(VramSpace::BgA, 0, 0x8000 | GREEN15);
    gfx.vram.write16(VramSpace::BgA, 2, GREEN15);

    e.render_line(0, &gfx.vram, &gfx.palette, &gfx.oam, &mut gfx.out);
    assert_eq!(gfx.pixel(0), rgba(GREEN15));
    assert_eq!(gfx.pixel(1), rgba(BLUE15), "not black — transparent");
}

#[test]
fn the_three_d_layer_leaves_a_gap_rather_than_a_plausible_wrong_picture() {
    // The 3D core does not exist. BG0 as the 3D layer must show the backdrop, not a flat colour
    // that looks like a deliberate render.
    let mut gfx = Gfx::new();
    gfx.map(0, 1, 0);
    gfx.set_palette(Engine::A, 0, BLUE15);
    gfx.set_palette(Engine::A, 1, RED15);
    fill_tile_4bpp(&mut gfx, VramSpace::BgA, 0, 1);
    gfx.vram.write16(VramSpace::BgA, 0x800, 0);

    let mut e = text_engine(Engine::A);
    e.w16(reg::BGCNT, 1 << 8);
    e.render_line(0, &gfx.vram, &gfx.palette, &gfx.oam, &mut gfx.out);
    assert_eq!(gfx.pixel(0), rgba(RED15), "as a text layer it draws");

    e.w32(reg::DISPCNT, e.dispcnt() | dispcnt::BG0_IS_3D);
    e.render_line(0, &gfx.vram, &gfx.palette, &gfx.oam, &mut gfx.out);
    assert_eq!(
        gfx.pixel(0),
        rgba(BLUE15),
        "as the 3D layer it draws nothing"
    );
}

#[test]
fn a_sprite_draws_over_the_backdrop() {
    let mut gfx = Gfx::new();
    gfx.map(0, 2, 0); // bank A -> ObjA
    gfx.set_palette(Engine::A, 0, BLUE15);
    gfx.set_sprite_palette(Engine::A, 1, RED15);
    fill_tile_4bpp(&mut gfx, VramSpace::ObjA, 0, 1);

    let mut e = Engine2d::new(Engine::A);
    e.w32(reg::DISPCNT, (1 << 16) | dispcnt::OBJ_ENABLE);
    // 8x8 at (16, 0), tile 0, palette 0.
    gfx.write_oam(Engine::A, 0, [0, 16, 0]);

    e.render_line(0, &gfx.vram, &gfx.palette, &gfx.oam, &mut gfx.out);
    assert_eq!(gfx.pixel(16), rgba(RED15));
    assert_eq!(gfx.pixel(23), rgba(RED15));
    assert_eq!(gfx.pixel(24), rgba(BLUE15));
    assert_eq!(gfx.pixel(15), rgba(BLUE15));
}

#[test]
fn bit_nine_disables_a_plain_sprite_and_doubles_an_affine_one() {
    // The same bit means two different things, and reading it as one or the other
    // unconditionally either hides every rotated sprite or shows 128 nobody asked for.
    let mut gfx = Gfx::new();
    gfx.map(0, 2, 0);
    gfx.set_palette(Engine::A, 0, BLUE15);
    gfx.set_sprite_palette(Engine::A, 1, RED15);
    fill_tile_4bpp(&mut gfx, VramSpace::ObjA, 0, 1);

    let mut e = Engine2d::new(Engine::A);
    e.w32(reg::DISPCNT, (1 << 16) | dispcnt::OBJ_ENABLE);

    gfx.write_oam(Engine::A, 0, [0x200, 16, 0]); // plain, bit 9 set: disabled
    e.render_line(0, &gfx.vram, &gfx.palette, &gfx.oam, &mut gfx.out);
    assert_eq!(gfx.pixel(16), rgba(BLUE15));

    // Affine with an identity matrix and bit 9 set: a 16x16 box around an 8x8 sprite.
    for (i, value) in [0x100i16, 0, 0, 0x100].iter().enumerate() {
        let entry = Engine::A.block_offset() + i * 8 + 6;
        gfx.oam[entry] = *value as u8;
        gfx.oam[entry + 1] = (*value >> 8) as u8;
    }
    gfx.write_oam(Engine::A, 0, [0x300, 16, 0]);
    e.render_line(4, &gfx.vram, &gfx.palette, &gfx.oam, &mut gfx.out);
    // The sprite is centred in the 16-wide box, so it covers x 20..28.
    assert_eq!(gfx.pixel(16), rgba(BLUE15), "the padding");
    assert_eq!(gfx.pixel(20), rgba(RED15));
}

#[test]
fn a_lower_oam_index_wins_at_the_same_priority() {
    let mut gfx = Gfx::new();
    gfx.map(0, 2, 0);
    gfx.set_palette(Engine::A, 0, BLUE15);
    gfx.set_sprite_palette(Engine::A, 1, RED15);
    gfx.set_sprite_palette(Engine::A, 17, GREEN15);
    fill_tile_4bpp(&mut gfx, VramSpace::ObjA, 0, 1);

    let mut e = Engine2d::new(Engine::A);
    e.w32(reg::DISPCNT, (1 << 16) | dispcnt::OBJ_ENABLE);
    gfx.write_oam(Engine::A, 0, [0, 0, 0]); // palette 0 -> red
    gfx.write_oam(Engine::A, 1, [0, 0, 1 << 12]); // palette 1 -> green, same place

    e.render_line(0, &gfx.vram, &gfx.palette, &gfx.oam, &mut gfx.out);
    assert_eq!(gfx.pixel(0), rgba(RED15));
}

#[test]
fn sprite_priority_puts_a_background_between_two_sprites() {
    // This is why the sprite pass cannot resolve "which sprite is in front" on its own: the
    // answer depends on a background layer it has not seen.
    let mut gfx = Gfx::new();
    gfx.map(0, 1, 0); // A -> BgA
    gfx.map(1, 2, 0); // B -> ObjA
    gfx.set_palette(Engine::A, 0, WHITE15);
    gfx.set_palette(Engine::A, 1, BLUE15);
    gfx.set_sprite_palette(Engine::A, 1, RED15);
    fill_tile_4bpp(&mut gfx, VramSpace::BgA, 0, 1);
    fill_tile_4bpp(&mut gfx, VramSpace::ObjA, 0, 1);
    gfx.vram.write16(VramSpace::BgA, 0x800, 0);

    let mut e = Engine2d::new(Engine::A);
    e.w32(reg::DISPCNT, (1 << 16) | (1 << 8) | dispcnt::OBJ_ENABLE);
    e.w16(reg::BGCNT, (1 << 8) | 1); // BG0 at priority 1

    // A priority-0 sprite is in front of it; a priority-2 sprite is behind it.
    gfx.write_oam(Engine::A, 0, [0, 0, 0]);
    e.render_line(0, &gfx.vram, &gfx.palette, &gfx.oam, &mut gfx.out);
    assert_eq!(gfx.pixel(0), rgba(RED15), "sprite in front");

    gfx.write_oam(Engine::A, 0, [0, 0, 2 << 10]);
    e.render_line(0, &gfx.vram, &gfx.palette, &gfx.oam, &mut gfx.out);
    assert_eq!(gfx.pixel(0), rgba(BLUE15), "background in front");
}

#[test]
fn a_sprite_beats_a_background_of_equal_priority() {
    let mut gfx = Gfx::new();
    gfx.map(0, 1, 0);
    gfx.map(1, 2, 0);
    gfx.set_palette(Engine::A, 1, BLUE15);
    gfx.set_sprite_palette(Engine::A, 1, RED15);
    fill_tile_4bpp(&mut gfx, VramSpace::BgA, 0, 1);
    fill_tile_4bpp(&mut gfx, VramSpace::ObjA, 0, 1);
    gfx.vram.write16(VramSpace::BgA, 0x800, 0);

    let mut e = Engine2d::new(Engine::A);
    e.w32(reg::DISPCNT, (1 << 16) | (1 << 8) | dispcnt::OBJ_ENABLE);
    e.w16(reg::BGCNT, (1 << 8) | 2);
    gfx.write_oam(Engine::A, 0, [0, 0, 2 << 10]);

    e.render_line(0, &gfx.vram, &gfx.palette, &gfx.oam, &mut gfx.out);
    assert_eq!(gfx.pixel(0), rgba(RED15));
}

#[test]
fn a_lower_numbered_background_wins_at_equal_priority() {
    let mut gfx = Gfx::new();
    gfx.map(0, 1, 0);
    gfx.set_palette(Engine::A, 1, RED15);
    gfx.set_palette(Engine::A, 2, GREEN15);
    fill_tile_4bpp(&mut gfx, VramSpace::BgA, 0, 1);
    fill_tile_4bpp(&mut gfx, VramSpace::BgA, 0x20, 2);
    gfx.vram.write16(VramSpace::BgA, 0x800, 0); // BG0 map -> tile 0
    gfx.vram.write16(VramSpace::BgA, 0x1000, 1); // BG1 map -> tile 1

    let mut e = Engine2d::new(Engine::A);
    e.w32(reg::DISPCNT, (1 << 16) | (1 << 8) | (1 << 9));
    e.w16(reg::BGCNT, 1 << 8);
    e.w16(reg::BGCNT + 2, 2 << 8);

    e.render_line(0, &gfx.vram, &gfx.palette, &gfx.oam, &mut gfx.out);
    assert_eq!(gfx.pixel(0), rgba(RED15));
}

#[test]
fn master_brightness_fades_the_finished_picture() {
    let mut gfx = Gfx::new();
    gfx.set_palette(Engine::A, 0, WHITE15);
    let mut e = text_engine(Engine::A);

    // Fade all the way down: white becomes black whatever produced it.
    e.w16(reg::MASTER_BRIGHT, (2 << 14) | 16);
    e.render_line(0, &gfx.vram, &gfx.palette, &gfx.oam, &mut gfx.out);
    assert_eq!(gfx.pixel(0), [0, 0, 0, 255]);

    // And all the way up turns anything white.
    gfx.set_palette(Engine::A, 0, 0);
    e.w16(reg::MASTER_BRIGHT, (1 << 14) | 16);
    e.render_line(0, &gfx.vram, &gfx.palette, &gfx.oam, &mut gfx.out);
    assert_eq!(gfx.pixel(0), [255, 255, 255, 255]);

    // Mode 0 leaves it alone even with a factor set.
    e.w16(reg::MASTER_BRIGHT, 16);
    e.render_line(0, &gfx.vram, &gfx.palette, &gfx.oam, &mut gfx.out);
    assert_eq!(gfx.pixel(0), [0, 0, 0, 255]);
}

#[test]
fn alpha_blending_mixes_the_top_two_layers() {
    let mut gfx = Gfx::new();
    gfx.map(0, 1, 0);
    gfx.set_palette(Engine::A, 0, 0); // backdrop black
    gfx.set_palette(Engine::A, 1, WHITE15);
    fill_tile_4bpp(&mut gfx, VramSpace::BgA, 0, 1);
    gfx.vram.write16(VramSpace::BgA, 0x800, 0);

    let mut e = text_engine(Engine::A);
    e.w16(reg::BGCNT, 1 << 8);
    // BG0 is the first target, the backdrop the second, half and half.
    e.w16(reg::BLDCNT, (1 << 0) | (1 << 6) | (1 << 13));
    e.w16(reg::BLDALPHA, 8 | (8 << 8));

    e.render_line(0, &gfx.vram, &gfx.palette, &gfx.oam, &mut gfx.out);
    // 31 * 8/16 + 0 * 8/16 = 15 in each channel.
    assert_eq!(gfx.pixel(0), rgba(15 | (15 << 5) | (15 << 10)));
}

#[test]
fn a_semi_transparent_sprite_blends_whatever_bldcnt_selects_as_first_target() {
    let mut gfx = Gfx::new();
    gfx.map(0, 2, 0);
    gfx.set_palette(Engine::A, 0, 0);
    gfx.set_sprite_palette(Engine::A, 1, WHITE15);
    fill_tile_4bpp(&mut gfx, VramSpace::ObjA, 0, 1);

    let mut e = Engine2d::new(Engine::A);
    e.w32(reg::DISPCNT, (1 << 16) | dispcnt::OBJ_ENABLE);
    // No effect selected at all, but the backdrop is a second target.
    e.w16(reg::BLDCNT, 1 << 13);
    e.w16(reg::BLDALPHA, 8 | (8 << 8));
    gfx.write_oam(Engine::A, 0, [1 << 10, 0, 0]); // mode 1: semi-transparent

    e.render_line(0, &gfx.vram, &gfx.palette, &gfx.oam, &mut gfx.out);
    assert_eq!(gfx.pixel(0), rgba(15 | (15 << 5) | (15 << 10)));
}

#[test]
fn a_window_hides_a_layer_outside_it() {
    let mut gfx = Gfx::new();
    gfx.map(0, 1, 0);
    gfx.set_palette(Engine::A, 0, BLUE15);
    gfx.set_palette(Engine::A, 1, RED15);
    // Fill the whole first tile row of the map with tile 0.
    fill_tile_4bpp(&mut gfx, VramSpace::BgA, 0, 1);
    for tx in 0..32u32 {
        gfx.vram.write16(VramSpace::BgA, 0x800 + tx * 2, 0);
    }

    let mut e = text_engine(Engine::A);
    e.w16(reg::BGCNT, 1 << 8);
    e.w32(reg::DISPCNT, e.dispcnt() | dispcnt::WIN0_ENABLE);
    // Window 0 covers x 64..128 and y 0..192, and permits BG0 inside only.
    e.w16(reg::WIN0H, (64 << 8) | 128);
    e.w16(reg::WIN0H + 4, 192);
    e.w16(reg::WININ, 0x0001);
    e.w16(reg::WININ + 2, 0x0000);

    e.render_line(0, &gfx.vram, &gfx.palette, &gfx.oam, &mut gfx.out);
    assert_eq!(gfx.pixel(63), rgba(BLUE15), "outside");
    assert_eq!(gfx.pixel(64), rgba(RED15), "inside");
    assert_eq!(gfx.pixel(127), rgba(RED15));
    assert_eq!(gfx.pixel(128), rgba(BLUE15));
}

#[test]
fn a_window_whose_edges_are_inverted_wraps_around_the_screen() {
    let mut gfx = Gfx::new();
    gfx.map(0, 1, 0);
    gfx.set_palette(Engine::A, 0, BLUE15);
    gfx.set_palette(Engine::A, 1, RED15);
    fill_tile_4bpp(&mut gfx, VramSpace::BgA, 0, 1);
    for tx in 0..32u32 {
        gfx.vram.write16(VramSpace::BgA, 0x800 + tx * 2, 0);
    }

    let mut e = text_engine(Engine::A);
    e.w16(reg::BGCNT, 1 << 8);
    e.w32(reg::DISPCNT, e.dispcnt() | dispcnt::WIN0_ENABLE);
    e.w16(reg::WIN0H, (200 << 8) | 50); // right edge before left
    e.w16(reg::WIN0H + 4, 192);
    e.w16(reg::WININ, 0x0001);
    e.w16(reg::WININ + 2, 0x0000);

    e.render_line(0, &gfx.vram, &gfx.palette, &gfx.oam, &mut gfx.out);
    assert_eq!(gfx.pixel(0), rgba(RED15), "the wrapped part");
    assert_eq!(gfx.pixel(49), rgba(RED15));
    assert_eq!(gfx.pixel(50), rgba(BLUE15));
    assert_eq!(gfx.pixel(200), rgba(RED15));
}

#[test]
fn display_mode_two_reads_a_bank_straight_out_of_the_lcdc_window() {
    let mut gfx = Gfx::new();
    // Bank A in LCDC mode: the only way to reach a bank the display is reading directly.
    gfx.map(0, 0, 0);
    gfx.vram.write16(VramSpace::Lcdc, 0, GREEN15);
    gfx.vram.write16(VramSpace::Lcdc, 2, RED15);

    let mut e = Engine2d::new(Engine::A);
    e.w32(reg::DISPCNT, 2 << 16);
    e.render_line(0, &gfx.vram, &gfx.palette, &gfx.oam, &mut gfx.out);
    assert_eq!(gfx.pixel(0), rgba(GREEN15));
    assert_eq!(gfx.pixel(1), rgba(RED15));

    // Line 1 is 256 pixels further in.
    gfx.vram.write16(VramSpace::Lcdc, 256 * 2, BLUE15);
    e.render_line(1, &gfx.vram, &gfx.palette, &gfx.oam, &mut gfx.out);
    assert_eq!(gfx.pixel(0), rgba(BLUE15));
}

#[test]
fn the_affine_reference_points_advance_one_line_at_a_time() {
    let mut e = Engine2d::new(Engine::A);
    e.w32(reg::BG2PA + 8, 0); // BG2X
    e.w32(reg::BG2PA + 12, 0); // BG2Y
    e.w16(reg::BG2PA + 2, 0x40); // PB
    e.w16(reg::BG2PA + 6, 0x100); // PD
    e.on_frame_start();
    assert_eq!(e.bgx_internal[0], 0);

    e.on_line_end();
    assert_eq!(e.bgx_internal[0], 0x40, "one row of PB");
    assert_eq!(e.bgy_internal[0], 0x100, "one row of PD");

    // The frame start reloads from the written value, so the drift does not accumulate.
    e.on_frame_start();
    assert_eq!(e.bgx_internal[0], 0);
}

#[test]
fn writing_a_reference_point_takes_effect_without_waiting_for_the_next_frame() {
    let mut e = Engine2d::new(Engine::A);
    e.on_frame_start();
    e.on_line_end();
    e.w32(reg::BG2PA + 8, 0x1234);
    assert_eq!(e.bgx_internal[0], 0x1234);
}

#[test]
fn the_reference_points_are_twenty_eight_bit_signed() {
    let mut e = Engine2d::new(Engine::A);
    e.w32(reg::BG2PA + 8, 0x0FFF_FFFF);
    assert_eq!(e.bgx[0], -1, "the top bit of 28 is the sign");
    e.w32(reg::BG2PA + 8, 0x0800_0000);
    assert_eq!(e.bgx[0], -0x0800_0000);
}

#[test]
fn write_only_registers_read_as_zero() {
    let mut e = text_engine(Engine::A);
    e.w16(reg::BGOFS, 0x1FF);
    e.w32(reg::BG2PA + 8, 0x1234);
    assert_eq!(e.r16(reg::BGOFS), Some(0));
    assert_eq!(e.r32(reg::BG2PA + 8), Some(0));
    // But the control registers read back.
    e.w16(reg::BGCNT, 0x1234);
    assert_eq!(e.r16(reg::BGCNT), Some(0x1234));
    assert_eq!(e.read16(Engine::B.base()), None, "not this engine's");
}

#[test]
fn an_engine_round_trips_through_a_save_state() {
    use savestate::{decode_state, encode_state};

    let mut e = text_engine(Engine::A);
    e.w16(reg::BGCNT, 0x0155);
    e.w16(reg::BGOFS, 0x1AB);
    e.w32(reg::BG2PA + 8, 0x0012_3456);
    e.on_frame_start();
    e.on_line_end();
    e.w16(reg::BLDCNT, 0x1234);
    e.w16(reg::MASTER_BRIGHT, (1 << 14) | 7);

    let blob = encode_state("nds", 1, &e);
    let mut restored = Engine2d::new(Engine::A);
    decode_state("nds", 1, &blob, &mut restored).unwrap();

    assert_eq!(restored.dispcnt(), e.dispcnt());
    assert_eq!(restored.r16(reg::BGCNT), Some(0x0155));
    assert_eq!(restored.bghofs[0], 0x1AB);
    assert_eq!(restored.bgx_internal[0], e.bgx_internal[0]);
    assert_eq!(restored.r16(reg::BLDCNT), Some(0x1234));
}
