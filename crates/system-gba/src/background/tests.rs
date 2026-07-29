use super::*;

fn control_addr(layer: usize) -> u32 {
    CONTROL_BASE + layer as u32 * 2
}

fn hofs_addr(layer: usize) -> u32 {
    SCROLL_BASE + layer as u32 * 4
}

/// A map with `entry` written into cell (x, y) of the given block layout.
fn map_with(width: u32, entry: u16, tile_x: u32, tile_y: u32) -> Vec<u8> {
    let mut vram = vec![0u8; 0x1_8000];
    let block = (tile_x / 32) + (tile_y / 32) * (width / 32);
    let cell = (tile_y % 32) * 32 + (tile_x % 32);
    let offset = block as usize * SCREEN_BLOCK + cell as usize * 2;
    vram[offset..offset + 2].copy_from_slice(&entry.to_le_bytes());
    vram
}

#[test]
fn the_control_register_decodes_into_its_fields() {
    let mut bgs = Backgrounds::new();
    // Priority 2, char base 1, 256 colours, screen base 3, size 3.
    bgs.write16(
        control_addr(0),
        2 | (1 << 2) | control::FULL_PALETTE | (3 << 8) | (3 << 14),
    );
    let layer = bgs.layers[0];

    assert_eq!(layer.priority(), 2);
    assert_eq!(layer.char_base(), CHAR_BLOCK);
    assert_eq!(layer.screen_base(), 3 * SCREEN_BLOCK);
    assert_eq!(layer.bit_depth(), BitDepth::Eight);
    assert_eq!(layer.size_in_tiles(false), (64, 64));
}

#[test]
fn the_four_size_settings_mean_different_things_for_text_and_affine_layers() {
    // Setting 3 is 512x512 for a text layer and 1024x1024 for an affine one. Sharing one lookup
    // gives an affine background a quarter of the map it should have.
    let mut bgs = Backgrounds::new();
    for setting in 0..4u16 {
        bgs.write16(control_addr(2), setting << 14);
        let layer = bgs.layers[2];
        let text = layer.size_in_tiles(false);
        let affine = layer.size_in_tiles(true);
        assert_eq!(affine, (16 << setting, 16 << setting), "affine {setting}");
        assert_ne!(text, affine, "setting {setting} means two different sizes");
    }
    bgs.write16(control_addr(2), 3 << 14);
    assert_eq!(bgs.layers[2].size_in_tiles(false), (64, 64));
    assert_eq!(bgs.layers[2].size_in_tiles(true), (128, 128));
}

#[test]
fn scroll_registers_are_write_only_and_nine_bits_wide() {
    let mut bgs = Backgrounds::new();
    bgs.write16(hofs_addr(1), 0xFFFF);
    assert_eq!(bgs.layers[1].scroll_x, 0x01FF, "truncated to what is wired");
    assert_eq!(
        bgs.read16(hofs_addr(1)),
        Some(0),
        "and it does not read back"
    );
}

#[test]
fn each_layer_has_its_own_horizontal_and_vertical_scroll() {
    let mut bgs = Backgrounds::new();
    for layer in 0..LAYERS {
        bgs.write16(hofs_addr(layer), 10 + layer as u16);
        bgs.write16(hofs_addr(layer) + 2, 20 + layer as u16);
    }
    for layer in 0..LAYERS {
        assert_eq!(bgs.layers[layer].scroll_x, 10 + layer as u16);
        assert_eq!(bgs.layers[layer].scroll_y, 20 + layer as u16);
    }
}

#[test]
fn a_map_entry_decodes_into_a_tile_reference() {
    let vram = map_with(32, 0x0000, 0, 0);
    let map = GbaTilemap {
        vram: &vram,
        screen_base: 0,
        char_base: 0,
        depth: BitDepth::Four,
        width: 32,
        height: 32,
    };
    assert_eq!(map.tile_at(0, 0), TileRef::default());

    // Tile 5, both flips, palette 9.
    let vram = map_with(32, 5 | 0x0400 | 0x0800 | (9 << 12), 0, 0);
    let map = GbaTilemap { vram: &vram, ..map };
    let tile = map.tile_at(0, 0);
    assert_eq!(tile.data_offset, 5 * 32, "16-colour tiles are 32 bytes");
    assert!(tile.flip_x);
    assert!(tile.flip_y);
    assert_eq!(tile.palette, 9);
}

#[test]
fn the_palette_field_is_unused_in_two_hundred_and_fifty_six_colour_mode() {
    // There is one palette in that mode, so the bits mean nothing — reading them anyway would
    // index a palette that does not exist.
    let vram = map_with(32, 7 | (9 << 12), 0, 0);
    let map = GbaTilemap {
        vram: &vram,
        screen_base: 0,
        char_base: 0,
        depth: BitDepth::Eight,
        width: 32,
        height: 32,
    };
    let tile = map.tile_at(0, 0);
    assert_eq!(tile.palette, 0);
    assert_eq!(tile.data_offset, 7 * 64, "256-colour tiles are 64 bytes");
}

#[test]
fn a_wide_map_is_stored_as_two_blocks_not_one_wide_grid() {
    // The classic way to get a background that looks right until it scrolls past 256 pixels: a
    // tile at x=37 is in the *second* block's column 5, not the first block's column 37.
    let vram = map_with(64, 0x0123, 37, 0);
    let map = GbaTilemap {
        vram: &vram,
        screen_base: 0,
        char_base: 0,
        depth: BitDepth::Four,
        width: 64,
        height: 32,
    };
    assert_eq!(map.tile_at(37, 0).data_offset, 0x0123 * 32);

    // And the naive flat-grid reading finds nothing there.
    let flat_offset = 37 * 2;
    assert_eq!(&vram[flat_offset..flat_offset + 2], &[0, 0]);
}

#[test]
fn a_tall_map_places_its_second_block_below_the_first() {
    let vram = map_with(32, 0x00AA, 0, 40);
    let map = GbaTilemap {
        vram: &vram,
        screen_base: 0,
        char_base: 0,
        depth: BitDepth::Four,
        width: 32,
        height: 64,
    };
    assert_eq!(map.tile_at(0, 40).data_offset, 0xAA * 32);
}

#[test]
fn a_full_size_map_uses_all_four_blocks() {
    for (x, y) in [(0, 0), (40, 0), (0, 40), (40, 40)] {
        let vram = map_with(64, 0x0055, x, y);
        let map = GbaTilemap {
            vram: &vram,
            screen_base: 0,
            char_base: 0,
            depth: BitDepth::Four,
            width: 64,
            height: 64,
        };
        assert_eq!(map.tile_at(x, y).data_offset, 0x55 * 32, "cell ({x}, {y})");
    }
}

#[test]
fn the_map_wraps_rather_than_reading_past_its_edge() {
    let vram = map_with(32, 0x0011, 0, 0);
    let map = GbaTilemap {
        vram: &vram,
        screen_base: 0,
        char_base: 0,
        depth: BitDepth::Four,
        width: 32,
        height: 32,
    };
    assert_eq!(map.tile_at(32, 32).data_offset, 0x11 * 32);
}

#[test]
fn the_char_and_screen_bases_offset_the_fetch() {
    let mut vram = vec![0u8; 0x1_8000];
    let entry: u16 = 3;
    let offset = 5 * SCREEN_BLOCK;
    vram[offset..offset + 2].copy_from_slice(&entry.to_le_bytes());

    let map = GbaTilemap {
        vram: &vram,
        screen_base: 5 * SCREEN_BLOCK,
        char_base: 2 * CHAR_BLOCK,
        depth: BitDepth::Four,
        width: 32,
        height: 32,
    };
    assert_eq!(map.tile_at(0, 0).data_offset, 2 * CHAR_BLOCK + 3 * 32);
}

#[test]
fn layers_draw_back_to_front_with_lower_numbers_in_front_at_equal_priority() {
    // Games rely on the tie-break to put a HUD over a background of the same priority.
    let mut bgs = Backgrounds::new();
    bgs.write16(control_addr(0), 1);
    bgs.write16(control_addr(1), 0);
    bgs.write16(control_addr(2), 1);
    bgs.write16(control_addr(3), 3);

    // Back to front: priority 3 first, then the two at priority 1 with the higher layer number
    // first, then priority 0 last so it ends up in front.
    assert_eq!(bgs.draw_order([true; LAYERS]), vec![3, 2, 0, 1]);
}

#[test]
fn a_disabled_layer_is_not_drawn_at_all() {
    let mut bgs = Backgrounds::new();
    bgs.write16(control_addr(1), 0);
    assert_eq!(bgs.draw_order([false, true, false, false]), vec![1]);
    assert!(bgs.draw_order([false; LAYERS]).is_empty());
}

#[test]
fn the_block_claims_its_registers_and_no_others() {
    assert!(Backgrounds::owns(CONTROL_BASE));
    assert!(Backgrounds::owns(CONTROL_BASE + 6));
    assert!(Backgrounds::owns(SCROLL_BASE));
    assert!(Backgrounds::owns(SCROLL_BASE + 14));
    // The control and scroll blocks are adjacent, so there is no gap between them to probe.
    // What lies outside is the affine parameter block, starting right after the scrolls.
    assert!(!Backgrounds::owns(SCROLL_BASE + 16), "BG2PA and beyond");
    assert!(!Backgrounds::owns(0x0400_0000), "DISPCNT belongs to video");
}

#[test]
fn background_state_round_trips() {
    use savestate::{decode_state, encode_state};
    let mut bgs = Backgrounds::new();
    for layer in 0..LAYERS {
        bgs.write16(control_addr(layer), (layer as u16) | (1 << 8));
        bgs.write16(hofs_addr(layer), 100 + layer as u16);
        bgs.write16(hofs_addr(layer) + 2, 200 + layer as u16);
    }

    let bytes = encode_state("gba-bg", 1, &bgs);
    let mut restored = Backgrounds::new();
    decode_state("gba-bg", 1, &bytes, &mut restored).unwrap();
    assert_eq!(restored, bgs);
}
