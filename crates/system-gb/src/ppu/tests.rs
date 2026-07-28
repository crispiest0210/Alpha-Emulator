//! PPU tests against hand-built VRAM with known expected output.
//!
//! These establish that the layer rules are right. Pixel-perfect confirmation is dmg-acid2's
//! job, and that needs the accuracy harness.

use super::*;
use ppu_tile2d::DMG_SHADES;

const VRAM_SIZE: usize = 0x2000;
const OAM_SIZE: usize = 0xA0;

/// Write an 8x8 2bpp tile filled with one colour index at `tile_index`.
fn solid_tile(vram: &mut [u8], tile_index: usize, color: u8) {
    let base = tile_index * 16;
    for row in 0..8 {
        vram[base + row * 2] = if color & 1 != 0 { 0xFF } else { 0x00 };
        vram[base + row * 2 + 1] = if color & 2 != 0 { 0xFF } else { 0x00 };
    }
}

/// Fill a tilemap with one tile number.
fn fill_map(vram: &mut [u8], map_base: usize, tile: u8) {
    vram[map_base..map_base + 32 * 32].fill(tile);
}

fn write_oam(oam: &mut [u8], slot: usize, y: u8, x: u8, tile: u8, attributes: u8) {
    oam[slot * 4] = y;
    oam[slot * 4 + 1] = x;
    oam[slot * 4 + 2] = tile;
    oam[slot * 4 + 3] = attributes;
}

/// A PPU with the background on, using unsigned tile addressing and the low tilemap.
fn setup() -> (GbPpu, Vec<u8>, Vec<u8>) {
    let mut ppu = GbPpu::new();
    ppu.lcdc = lcdc::LCD_ENABLE | lcdc::BG_ENABLE | lcdc::TILE_DATA_LOW;
    (ppu, vec![0; VRAM_SIZE], vec![0; OAM_SIZE])
}

fn shade_at(ppu: &GbPpu, x: u32, y: u32) -> Rgba8 {
    ppu.framebuffer().pixel(x, y)
}

// ---------------------------------------------------------------------------
// Background
// ---------------------------------------------------------------------------

#[test]
fn the_background_fills_the_line_from_its_tilemap() {
    let (mut ppu, mut vram, oam) = setup();
    solid_tile(&mut vram, 1, 3); // darkest
    fill_map(&mut vram, 0x1800, 1);

    ppu.render_scanline(0, &vram, &oam);
    for x in 0..SCREEN_WIDTH {
        assert_eq!(shade_at(&ppu, x, 0), DMG_SHADES[3], "x {x}");
    }
}

#[test]
fn signed_tile_addressing_bases_tile_zero_in_the_middle_of_tile_data() {
    // With LCDC.4 clear, tile 0 is at 0x9000 and tile 255 is just below it. Getting this
    // backwards renders the wrong graphics entirely rather than subtly wrong ones.
    let (mut ppu, mut vram, oam) = setup();
    ppu.lcdc &= !lcdc::TILE_DATA_LOW;

    // Tile 0 under signed addressing lives at byte offset 0x1000.
    solid_tile(&mut vram, 0x1000 / 16, 3);
    fill_map(&mut vram, 0x1800, 0);
    ppu.render_scanline(0, &vram, &oam);
    assert_eq!(shade_at(&ppu, 0, 0), DMG_SHADES[3]);

    // Tile 255 is -1, so 16 bytes below 0x1000.
    solid_tile(&mut vram, (0x1000 - 16) / 16, 1);
    fill_map(&mut vram, 0x1800, 255);
    ppu.render_scanline(0, &vram, &oam);
    assert_eq!(shade_at(&ppu, 0, 0), DMG_SHADES[1]);
}

#[test]
fn the_high_tilemap_is_selected_by_lcdc() {
    let (mut ppu, mut vram, oam) = setup();
    solid_tile(&mut vram, 1, 1);
    solid_tile(&mut vram, 2, 3);
    fill_map(&mut vram, 0x1800, 1);
    fill_map(&mut vram, 0x1C00, 2);

    ppu.render_scanline(0, &vram, &oam);
    assert_eq!(shade_at(&ppu, 0, 0), DMG_SHADES[1]);

    ppu.lcdc |= lcdc::BG_MAP_HIGH;
    ppu.render_scanline(0, &vram, &oam);
    assert_eq!(shade_at(&ppu, 0, 0), DMG_SHADES[3]);
}

#[test]
fn disabling_the_background_shows_white_regardless_of_the_palette() {
    // The layer is off, so BGP does not apply to it — a palette that maps index 0 to black
    // must not darken the blanked screen.
    let (mut ppu, mut vram, oam) = setup();
    solid_tile(&mut vram, 1, 3);
    fill_map(&mut vram, 0x1800, 1);
    ppu.palette.bgp = 0b11_11_11_11;

    ppu.lcdc &= !lcdc::BG_ENABLE;
    ppu.render_scanline(0, &vram, &oam);
    assert_eq!(shade_at(&ppu, 0, 0), DMG_SHADES[0]);
}

#[test]
fn rewriting_bgp_recolours_the_next_line_without_touching_tiles() {
    let (mut ppu, mut vram, oam) = setup();
    solid_tile(&mut vram, 1, 1);
    fill_map(&mut vram, 0x1800, 1);

    ppu.render_scanline(0, &vram, &oam);
    assert_eq!(shade_at(&ppu, 0, 0), DMG_SHADES[1]);

    ppu.palette.bgp = 0b00_00_00_11; // index 1 now maps to the lightest shade
    ppu.render_scanline(1, &vram, &oam);
    assert_eq!(shade_at(&ppu, 0, 1), DMG_SHADES[0]);
    assert_eq!(shade_at(&ppu, 0, 0), DMG_SHADES[1], "line 0 is unchanged");
}

#[test]
fn a_mid_frame_scroll_change_splits_the_raster() {
    // The reason rendering is per-scanline. A status bar that holds still while the world
    // scrolls under it is done exactly this way, and a frame-at-a-time renderer loses it.
    let (mut ppu, mut vram, oam) = setup();
    // Two alternating tiles so a scroll of 8 is visible.
    solid_tile(&mut vram, 1, 1);
    solid_tile(&mut vram, 2, 3);
    for cell in 0..32 * 32 {
        vram[0x1800 + cell] = if cell % 2 == 0 { 1 } else { 2 };
    }

    ppu.scx = 0;
    ppu.render_scanline(0, &vram, &oam);

    // Halfway down the frame the game moves the scroll register.
    ppu.scx = 8;
    ppu.render_scanline(1, &vram, &oam);

    assert_eq!(
        shade_at(&ppu, 0, 0),
        DMG_SHADES[1],
        "line 0 starts on tile 1"
    );
    assert_eq!(
        shade_at(&ppu, 0, 1),
        DMG_SHADES[3],
        "line 1 starts one tile over"
    );
}

// ---------------------------------------------------------------------------
// Window
// ---------------------------------------------------------------------------

#[test]
fn the_window_covers_the_line_from_wx_minus_seven() {
    let (mut ppu, mut vram, oam) = setup();
    solid_tile(&mut vram, 1, 1); // background
    solid_tile(&mut vram, 2, 3); // window
    fill_map(&mut vram, 0x1800, 1);
    fill_map(&mut vram, 0x1C00, 2);

    ppu.lcdc |= lcdc::WINDOW_ENABLE | lcdc::WINDOW_MAP_HIGH;
    ppu.wy = 0;
    ppu.wx = 7 + 80; // window starts at screen x = 80

    ppu.render_scanline(0, &vram, &oam);
    assert_eq!(
        shade_at(&ppu, 79, 0),
        DMG_SHADES[1],
        "background to its left"
    );
    assert_eq!(shade_at(&ppu, 80, 0), DMG_SHADES[3], "window from x = 80");
    assert_eq!(shade_at(&ppu, 159, 0), DMG_SHADES[3]);
}

#[test]
fn a_wx_of_seven_puts_the_window_at_the_left_edge() {
    let (mut ppu, mut vram, oam) = setup();
    solid_tile(&mut vram, 1, 1);
    solid_tile(&mut vram, 2, 3);
    fill_map(&mut vram, 0x1800, 1);
    fill_map(&mut vram, 0x1C00, 2);
    ppu.lcdc |= lcdc::WINDOW_ENABLE | lcdc::WINDOW_MAP_HIGH;
    ppu.wx = 7;

    ppu.render_scanline(0, &vram, &oam);
    assert_eq!(shade_at(&ppu, 0, 0), DMG_SHADES[3]);
}

#[test]
fn the_window_does_not_appear_above_wy() {
    let (mut ppu, mut vram, oam) = setup();
    solid_tile(&mut vram, 1, 1);
    solid_tile(&mut vram, 2, 3);
    fill_map(&mut vram, 0x1800, 1);
    fill_map(&mut vram, 0x1C00, 2);
    ppu.lcdc |= lcdc::WINDOW_ENABLE | lcdc::WINDOW_MAP_HIGH;
    ppu.wy = 10;
    ppu.wx = 7;

    ppu.render_scanline(9, &vram, &oam);
    assert_eq!(shade_at(&ppu, 0, 9), DMG_SHADES[1], "background only");
    ppu.render_scanline(10, &vram, &oam);
    assert_eq!(shade_at(&ppu, 0, 10), DMG_SHADES[3], "window from WY down");
}

#[test]
fn the_window_line_counter_advances_only_on_lines_it_appears() {
    // Driving the window from LY - WY instead of its own counter breaks games that toggle it
    // mid-frame: the window would jump instead of resuming.
    let (mut ppu, mut vram, oam) = setup();
    // A window tilemap whose rows differ, so the counter's value is observable.
    solid_tile(&mut vram, 1, 1);
    solid_tile(&mut vram, 2, 2);
    solid_tile(&mut vram, 3, 3);
    fill_map(&mut vram, 0x1800, 1);
    for cell in 0..32 * 32 {
        // Row 0 of the map uses tile 2, every later row uses tile 3.
        vram[0x1C00 + cell] = if cell < 32 { 2 } else { 3 };
    }

    ppu.lcdc |= lcdc::WINDOW_ENABLE | lcdc::WINDOW_MAP_HIGH;
    ppu.wy = 0;
    ppu.wx = 7;

    ppu.begin_frame();
    // Eight lines of window: the counter walks through the first map row.
    for line in 0..8 {
        ppu.render_scanline(line, &vram, &oam);
        assert_eq!(shade_at(&ppu, 0, line as u32), DMG_SHADES[2], "line {line}");
    }

    // Switch the window off for four lines. Its counter must not advance.
    ppu.lcdc &= !lcdc::WINDOW_ENABLE;
    for line in 8..12 {
        ppu.render_scanline(line, &vram, &oam);
    }

    // Back on: it resumes at map row 1, not at row 12/8.
    ppu.lcdc |= lcdc::WINDOW_ENABLE;
    ppu.render_scanline(12, &vram, &oam);
    assert_eq!(
        shade_at(&ppu, 0, 12),
        DMG_SHADES[3],
        "the window resumed rather than jumping"
    );
}

#[test]
fn beginning_a_frame_resets_the_window_counter() {
    let (mut ppu, mut vram, oam) = setup();
    solid_tile(&mut vram, 2, 2);
    solid_tile(&mut vram, 3, 3);
    for cell in 0..32 * 32 {
        vram[0x1C00 + cell] = if cell < 32 { 2 } else { 3 };
    }
    ppu.lcdc |= lcdc::WINDOW_ENABLE | lcdc::WINDOW_MAP_HIGH;
    ppu.wx = 7;

    for line in 0..16 {
        ppu.render_scanline(line, &vram, &oam);
    }
    assert_eq!(
        shade_at(&ppu, 0, 15),
        DMG_SHADES[3],
        "past the first map row"
    );

    ppu.begin_frame();
    ppu.render_scanline(0, &vram, &oam);
    assert_eq!(shade_at(&ppu, 0, 0), DMG_SHADES[2], "back to map row 0");
}

// ---------------------------------------------------------------------------
// Sprites
// ---------------------------------------------------------------------------

#[test]
fn a_sprite_draws_at_its_biased_oam_position() {
    // OAM stores Y+16 and X+8 so a sprite can sit partly off the top or left edge.
    let (mut ppu, mut vram, mut oam) = setup();
    solid_tile(&mut vram, 1, 3);
    ppu.lcdc |= lcdc::OBJ_ENABLE;
    write_oam(&mut oam, 0, 16, 8, 1, 0); // screen (0, 0)

    ppu.render_scanline(0, &vram, &oam);
    assert_eq!(shade_at(&ppu, 0, 0), DMG_SHADES[3]);
    assert_eq!(shade_at(&ppu, 7, 0), DMG_SHADES[3]);
    assert_eq!(shade_at(&ppu, 8, 0), DMG_SHADES[0], "eight pixels wide");
}

#[test]
fn a_sprite_can_hang_off_the_top_of_the_screen() {
    let (mut ppu, mut vram, mut oam) = setup();
    solid_tile(&mut vram, 1, 3);
    ppu.lcdc |= lcdc::OBJ_ENABLE;
    write_oam(&mut oam, 0, 12, 8, 1, 0); // y = -4

    ppu.render_scanline(0, &vram, &oam);
    assert_eq!(shade_at(&ppu, 0, 0), DMG_SHADES[3], "its lower half shows");
    ppu.render_scanline(4, &vram, &oam);
    assert_eq!(shade_at(&ppu, 0, 4), DMG_SHADES[0], "and it ends at y = 4");
}

#[test]
fn the_sprite_palette_bit_selects_between_obp0_and_obp1() {
    let (mut ppu, mut vram, mut oam) = setup();
    solid_tile(&mut vram, 1, 1);
    ppu.lcdc |= lcdc::OBJ_ENABLE;
    ppu.palette.obp[0] = 0b11_11_11_11; // index 1 -> darkest
    ppu.palette.obp[1] = 0b00_00_00_00; // index 1 -> lightest

    write_oam(&mut oam, 0, 16, 8, 1, 0x00);
    write_oam(&mut oam, 1, 16, 24, 1, 0x10);
    ppu.render_scanline(0, &vram, &oam);

    assert_eq!(shade_at(&ppu, 0, 0), DMG_SHADES[3]);
    assert_eq!(shade_at(&ppu, 16, 0), DMG_SHADES[0]);
}

#[test]
fn a_behind_background_sprite_shows_only_over_background_index_zero() {
    let (mut ppu, mut vram, mut oam) = setup();
    // Background tile 1 is index 0 (transparent to priority), tile 2 is index 3.
    solid_tile(&mut vram, 1, 0);
    solid_tile(&mut vram, 2, 3);
    solid_tile(&mut vram, 3, 1); // the sprite
    for cell in 0..32 * 32 {
        vram[0x1800 + cell] = if cell % 2 == 0 { 1 } else { 2 };
    }
    ppu.lcdc |= lcdc::OBJ_ENABLE;
    ppu.palette.obp[0] = 0b11_11_11_11;

    write_oam(&mut oam, 0, 16, 8, 3, 0x80); // behind background, at x = 0
    ppu.render_scanline(0, &vram, &oam);

    // First background tile is index 0, so the sprite shows through.
    assert_eq!(shade_at(&ppu, 0, 0), DMG_SHADES[3]);
    // Second tile is opaque, so it hides the sprite.
    assert_eq!(shade_at(&ppu, 8, 0), ppu.palette.lookup_bg(0, 3));
}

#[test]
fn only_ten_sprites_are_drawn_per_line_and_they_are_the_first_ten_in_oam() {
    // Games use the limit deliberately to hide sprites, so drawing an eleventh would be a
    // visible inaccuracy rather than a kindness.
    let (mut ppu, mut vram, mut oam) = setup();
    solid_tile(&mut vram, 1, 3);
    ppu.lcdc |= lcdc::OBJ_ENABLE;

    // Twelve sprites across the line, eight pixels apart, in OAM order.
    for slot in 0..12 {
        write_oam(&mut oam, slot, 16, 8 + slot as u8 * 8, 1, 0);
    }
    ppu.render_scanline(0, &vram, &oam);

    for slot in 0..10 {
        assert_eq!(
            shade_at(&ppu, slot as u32 * 8, 0),
            DMG_SHADES[3],
            "sprite {slot} drew"
        );
    }
    for slot in 10..12 {
        assert_eq!(
            shade_at(&ppu, slot as u32 * 8, 0),
            DMG_SHADES[0],
            "sprite {slot} was dropped by the per-line limit"
        );
    }
}

#[test]
fn a_lower_x_coordinate_wins_an_overlap_on_dmg() {
    let (mut ppu, mut vram, mut oam) = setup();
    solid_tile(&mut vram, 1, 1);
    solid_tile(&mut vram, 2, 3);
    ppu.lcdc |= lcdc::OBJ_ENABLE;
    ppu.palette.obp[0] = 0b11_10_01_00;

    // The later OAM entry has the smaller X, so it must win despite coming second.
    write_oam(&mut oam, 0, 16, 12, 2, 0); // x = 4, tile index 3
    write_oam(&mut oam, 1, 16, 8, 1, 0); // x = 0, tile index 1
    ppu.render_scanline(0, &vram, &oam);

    assert_eq!(
        shade_at(&ppu, 4, 0),
        DMG_SHADES[1],
        "the sprite at the lower X is in front"
    );
}

#[test]
fn oam_order_breaks_a_tie_between_sprites_at_the_same_x() {
    let (mut ppu, mut vram, mut oam) = setup();
    solid_tile(&mut vram, 1, 1);
    solid_tile(&mut vram, 2, 3);
    ppu.lcdc |= lcdc::OBJ_ENABLE;
    ppu.palette.obp[0] = 0b11_10_01_00;

    write_oam(&mut oam, 0, 16, 8, 1, 0);
    write_oam(&mut oam, 1, 16, 8, 2, 0);
    ppu.render_scanline(0, &vram, &oam);

    assert_eq!(
        shade_at(&ppu, 0, 0),
        DMG_SHADES[1],
        "the earlier OAM entry wins"
    );
}

#[test]
fn tall_sprites_use_a_tile_pair_with_the_low_bit_ignored() {
    let (mut ppu, mut vram, mut oam) = setup();
    solid_tile(&mut vram, 2, 1); // upper half
    solid_tile(&mut vram, 3, 3); // lower half
    ppu.lcdc |= lcdc::OBJ_ENABLE | lcdc::OBJ_TALL;
    ppu.palette.obp[0] = 0b11_10_01_00;

    // Tile 3 is requested, but the low bit is ignored so the pair starts at tile 2.
    write_oam(&mut oam, 0, 16, 8, 3, 0);

    ppu.render_scanline(0, &vram, &oam);
    assert_eq!(shade_at(&ppu, 0, 0), DMG_SHADES[1], "upper tile");
    ppu.render_scanline(8, &vram, &oam);
    assert_eq!(shade_at(&ppu, 0, 8), DMG_SHADES[3], "lower tile");
    ppu.render_scanline(15, &vram, &oam);
    assert_eq!(shade_at(&ppu, 0, 15), DMG_SHADES[3], "sixteen pixels tall");
}

#[test]
fn disabling_sprites_removes_them_without_affecting_the_background() {
    let (mut ppu, mut vram, mut oam) = setup();
    solid_tile(&mut vram, 1, 1);
    solid_tile(&mut vram, 2, 3);
    fill_map(&mut vram, 0x1800, 1);
    write_oam(&mut oam, 0, 16, 8, 2, 0);

    ppu.lcdc |= lcdc::OBJ_ENABLE;
    ppu.render_scanline(0, &vram, &oam);
    assert_eq!(shade_at(&ppu, 0, 0), DMG_SHADES[3]);

    ppu.lcdc &= !lcdc::OBJ_ENABLE;
    ppu.render_scanline(0, &vram, &oam);
    assert_eq!(shade_at(&ppu, 0, 0), DMG_SHADES[1], "background survives");
}

// ---------------------------------------------------------------------------
// Registers and state
// ---------------------------------------------------------------------------

#[test]
fn registers_read_back_what_was_written() {
    let mut ppu = GbPpu::new();
    for (addr, value) in [
        (reg::SCY, 0x12u8),
        (reg::SCX, 0x34),
        (reg::BGP, 0x1B),
        (reg::OBP0, 0x2D),
        (reg::OBP1, 0x3C),
        (reg::WY, 0x56),
        (reg::WX, 0x78),
    ] {
        ppu.write_register(addr, value);
        assert_eq!(ppu.read_register(addr), Some(value), "{addr:#06X}");
    }
    assert_eq!(ppu.read_register(0xFF00), None, "not our register");
}

#[test]
fn switching_the_lcd_off_blanks_the_panel() {
    let (mut ppu, mut vram, oam) = setup();
    solid_tile(&mut vram, 1, 3);
    fill_map(&mut vram, 0x1800, 1);
    ppu.render_scanline(0, &vram, &oam);
    assert_eq!(shade_at(&ppu, 0, 0), DMG_SHADES[3]);

    ppu.write_register(reg::LCDC, ppu.lcdc & !lcdc::LCD_ENABLE);
    assert_eq!(shade_at(&ppu, 0, 0), DMG_SHADES[0], "the panel went white");
}

#[test]
fn rendering_past_the_bottom_of_the_screen_is_ignored() {
    let (mut ppu, vram, oam) = setup();
    ppu.render_scanline(200, &vram, &oam); // must not panic
}

#[test]
fn ppu_state_round_trips_including_the_window_counter() {
    let (mut ppu, mut vram, oam) = setup();
    solid_tile(&mut vram, 1, 2);
    fill_map(&mut vram, 0x1800, 1);
    ppu.lcdc |= lcdc::WINDOW_ENABLE;
    ppu.wx = 7;
    ppu.scx = 0x20;
    ppu.palette.bgp = 0x1B;
    for line in 0..5 {
        ppu.render_scanline(line, &vram, &oam);
    }

    let mut w = StateWriter::new();
    ppu.save(&mut w);
    let blob = w.into_inner();

    let mut restored = GbPpu::new();
    restored.load(&mut StateReader::new(&blob)).unwrap();

    assert_eq!(restored.lcdc, ppu.lcdc);
    assert_eq!(restored.scx, ppu.scx);
    assert_eq!(restored.palette, ppu.palette);
    assert_eq!(restored.window_line, ppu.window_line);
    assert_eq!(
        restored.framebuffer(),
        ppu.framebuffer(),
        "the picture survives so a load does not show a stale frame"
    );

    // And both continue identically.
    ppu.render_scanline(5, &vram, &oam);
    restored.render_scanline(5, &vram, &oam);
    assert_eq!(restored.framebuffer(), ppu.framebuffer());
}
