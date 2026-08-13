use super::*;
use core_common::Rgba8;

// ---------------------------------------------------------------------------
// Palette
// ---------------------------------------------------------------------------

#[test]
fn decoding_an_empty_palette_produces_no_swatches() {
    assert_eq!(decode_palette(&[]), Vec::new());
}

#[test]
fn known_bgr555_values_decode_to_known_colours() {
    // Same reference values `ppu_tile2d::bgr555_to_rgba` is itself tested against, so a
    // regression in either shows up here as loudly as there.
    let bytes = [
        0x00, 0x00, // black
        0xFF, 0x7F, // white
        0x1F, 0x00, // red
    ];
    let swatches = decode_palette(&bytes);
    assert_eq!(swatches.len(), 3);
    assert_eq!(swatches[0].color, Rgba8::rgb(0, 0, 0));
    assert_eq!(swatches[0].raw, 0x0000);
    assert_eq!(swatches[1].color, Rgba8::rgb(255, 255, 255));
    assert_eq!(swatches[1].raw, 0x7FFF);
    assert_eq!(swatches[2].color, Rgba8::rgb(255, 0, 0));
    assert_eq!(swatches[2].raw, 0x001F);
}

#[test]
fn a_trailing_odd_byte_still_produces_a_swatch_rather_than_panicking() {
    // Never happens against a real 1024-byte palette RAM slice, but a decode function that can
    // panic on a short input is a debugger that can crash the panel it is drawing.
    let swatches = decode_palette(&[0x34]);
    assert_eq!(swatches.len(), 1);
}

// ---------------------------------------------------------------------------
// Tiles
// ---------------------------------------------------------------------------

#[test]
fn a_solid_eight_bpp_tile_decodes_to_one_colour() {
    let mut vram = vec![0u8; 64];
    vram.fill(2); // every pixel is palette index 2
    let mut palette = vec![0u8; 512];
    palette[4..6].copy_from_slice(&0x001Fu16.to_le_bytes()); // index 2 = red

    let request = core_common::PpuDebugRequest {
        tile_char_base: 0,
        tile_count: 1,
        tile_depth: TileBitDepth::Eight,
        tile_palette_bank: 0,
    };
    let tiles = decode_tiles(&vram, &palette, &request);
    assert_eq!(tiles.len(), 1);
    for pixel in tiles[0].pixels {
        assert_eq!(pixel, Rgba8::rgb(255, 0, 0));
    }
}

#[test]
fn a_four_bpp_tile_reads_the_low_nibble_as_the_left_pixel() {
    // `ppu_tile2d::decode_tile_row` documents this as "backwards but correct" — this is the
    // check that the tile viewer inherits that convention rather than silently mirroring rows.
    let mut vram = vec![0u8; 32];
    vram[0] = 0x21; // low nibble 1, high nibble 2: pixel 0 = index 1, pixel 1 = index 2
    let mut palette = vec![0u8; 512];
    palette[2..4].copy_from_slice(&0x03E0u16.to_le_bytes()); // index 1 = green
    palette[4..6].copy_from_slice(&0x7C00u16.to_le_bytes()); // index 2 = blue

    let request = core_common::PpuDebugRequest {
        tile_char_base: 0,
        tile_count: 1,
        tile_depth: TileBitDepth::Four,
        tile_palette_bank: 0,
    };
    let tiles = decode_tiles(&vram, &palette, &request);
    assert_eq!(tiles[0].pixels[0], Rgba8::rgb(0, 255, 0), "left pixel");
    assert_eq!(tiles[0].pixels[1], Rgba8::rgb(0, 0, 255), "right pixel");
}

#[test]
fn a_four_bpp_tile_is_looked_up_in_its_requested_palette_bank() {
    let mut vram = vec![0u8; 32];
    vram[0] = 0x01; // pixel 0 = index 1
    let mut palette = vec![0u8; 512];
    // Bank 3 starts at colour 3*16 = 48, so index 1 within it is colour 49.
    palette[49 * 2..49 * 2 + 2].copy_from_slice(&0x7FFFu16.to_le_bytes());

    let request = core_common::PpuDebugRequest {
        tile_char_base: 0,
        tile_count: 1,
        tile_depth: TileBitDepth::Four,
        tile_palette_bank: 3,
    };
    let tiles = decode_tiles(&vram, &palette, &request);
    assert_eq!(tiles[0].pixels[0], Rgba8::rgb(255, 255, 255));
}

#[test]
fn successive_tiles_advance_by_the_depths_own_tile_size() {
    let mut vram = vec![0u8; 128];
    // Tile 1 (8bpp, 64 bytes on) is solid index 5.
    vram[64..128].fill(5);
    let mut palette = vec![0u8; 512];
    palette[10..12].copy_from_slice(&0x7FFFu16.to_le_bytes());

    let request = core_common::PpuDebugRequest {
        tile_char_base: 0,
        tile_count: 2,
        tile_depth: TileBitDepth::Eight,
        tile_palette_bank: 0,
    };
    let tiles = decode_tiles(&vram, &palette, &request);
    assert_eq!(tiles[0].pixels[0], Rgba8::BLACK, "tile 0 is all index 0");
    assert_eq!(tiles[1].pixels[0], Rgba8::rgb(255, 255, 255), "tile 1");
}

#[test]
fn decoding_past_the_end_of_vram_fills_with_index_zero_rather_than_panicking() {
    let vram = vec![0u8; 8]; // far short of even one tile
    let palette = vec![0u8; 512];
    let request = core_common::PpuDebugRequest {
        tile_char_base: 0,
        tile_count: 1,
        tile_depth: TileBitDepth::Eight,
        tile_palette_bank: 0,
    };
    let tiles = decode_tiles(&vram, &palette, &request);
    assert_eq!(tiles.len(), 1);
    assert_eq!(tiles[0].pixels[63], Rgba8::BLACK);
}

// ---------------------------------------------------------------------------
// OAM
// ---------------------------------------------------------------------------

fn object(attr0: u16, attr1: u16, attr2: u16) -> Object {
    Object::decode(attr0, attr1, attr2)
}

#[test]
fn a_normal_sprites_row_names_its_position_size_and_tile() {
    // Shape 0 (square), size 1 -> 16x16; tile 5; priority 2; palette 3 (4bpp).
    let obj = object(0x0010, 0x4000, (3 << 12) | (2 << 10) | 5);
    let row = decode_oam_row(&obj, 7, None);
    assert_eq!(row.index, 7);
    assert_eq!(row.x, 0);
    assert_eq!(row.y, 16);
    assert_eq!(row.width, 16);
    assert_eq!(row.height, 16);
    assert_eq!(row.priority, 2);
    assert_eq!(row.palette, 3);
    assert_eq!(row.tile, 5);
    assert_eq!(row.mode, "Normal");
    assert_eq!(row.graphics_mode, "Normal");
    assert_eq!(row.affine_index, None, "a non-affine sprite has no matrix");
}

#[test]
fn an_affine_sprites_row_names_its_matrix() {
    let obj = object(1 << 8, 5 << 9, 0); // affine mode, matrix 5
    let row = decode_oam_row(&obj, 0, None);
    assert_eq!(row.mode, "Affine");
    assert_eq!(row.affine_index, Some(5));
}

#[test]
fn a_hidden_sprites_row_says_so() {
    let obj = object(2 << 8, 0, 0);
    let row = decode_oam_row(&obj, 0, None);
    assert_eq!(row.mode, "Hidden");
}

#[test]
fn an_object_window_sprites_row_names_its_graphics_mode() {
    let obj = object(2 << 10, 0, 0);
    let row = decode_oam_row(&obj, 0, None);
    assert_eq!(row.graphics_mode, "ObjectWindow");
}

#[test]
fn on_current_scanline_is_true_only_while_the_sprite_covers_it() {
    // y=16, height 16: covers lines 16..32.
    let obj = object(16, 0x4000, 0); // shape 0 size 1 = 16x16, y = 16
    assert!(decode_oam_row(&obj, 0, Some(16)).on_current_scanline);
    assert!(decode_oam_row(&obj, 0, Some(31)).on_current_scanline);
    assert!(!decode_oam_row(&obj, 0, Some(32)).on_current_scanline);
    assert!(!decode_oam_row(&obj, 0, Some(15)).on_current_scanline);
    assert!(
        !decode_oam_row(&obj, 0, None).on_current_scanline,
        "no current line means nothing is reported as current"
    );
}

#[test]
fn a_hidden_sprite_never_reports_as_on_the_current_scanline() {
    // Parked at (0,0) with mode Hidden: it would geometrically cover line 0, but it is not
    // actually one of the sprites in play.
    let obj = object(2 << 8, 0, 0);
    assert!(!decode_oam_row(&obj, 0, Some(0)).on_current_scanline);
}

#[test]
fn decoding_the_whole_table_produces_128_rows_in_order() {
    let oam = vec![0u8; 0x400];
    let rows = decode_oam(&oam, None);
    assert_eq!(rows.len(), 128);
    for (i, row) in rows.iter().enumerate() {
        assert_eq!(row.index, i);
    }
}

// ---------------------------------------------------------------------------
// Registers
// ---------------------------------------------------------------------------

#[test]
fn decoded_registers_read_scroll_from_the_stored_layer_not_the_bus() {
    // The exact trap named in this module's docs: a bus read of BGxHOFS/BGxVOFS always answers
    // zero, so if this regressed to reading through `Backgrounds::read16` every scroll value
    // here would silently go back to (0, 0).
    let video = VideoTiming::new();
    let mut backgrounds = Backgrounds::new();
    backgrounds.layers[0].scroll_x = 320;
    backgrounds.layers[0].scroll_y = 64;
    let effects = Effects::new();

    let registers = decode_registers(&video, &backgrounds, &effects);
    assert_eq!(registers.backgrounds[0].scroll_x, 320);
    assert_eq!(registers.backgrounds[0].scroll_y, 64);
}

#[test]
fn decoded_registers_read_bldy_from_the_stored_value_not_the_bus() {
    let video = VideoTiming::new();
    let backgrounds = Backgrounds::new();
    let mut effects = Effects::new();
    effects.write16(effects_reg::BLDY, 12);

    let registers = decode_registers(&video, &backgrounds, &effects);
    assert_eq!(registers.bldy, 12);
}

#[test]
fn decoded_registers_report_which_backgrounds_are_enabled() {
    let mut video = VideoTiming::new();
    video.write16(crate::video::reg::DISPCNT, 1 << 8); // BG0 only
    let backgrounds = Backgrounds::new();
    let effects = Effects::new();

    let registers = decode_registers(&video, &backgrounds, &effects);
    assert!(registers.backgrounds[0].enabled);
    assert!(!registers.backgrounds[1].enabled);
}

#[test]
fn decoded_window_bounds_split_into_the_four_edges() {
    let video = VideoTiming::new();
    let backgrounds = Backgrounds::new();
    let mut effects = Effects::new();
    effects.write16(effects_reg::WIN0H, (10 << 8) | 200); // left 10, right 200
    effects.write16(effects_reg::WIN0V, (5 << 8) | 150); // top 5, bottom 150

    let registers = decode_registers(&video, &backgrounds, &effects);
    let win0 = registers.windows[0];
    assert_eq!(
        (win0.left, win0.right, win0.top, win0.bottom),
        (10, 200, 5, 150)
    );
}
