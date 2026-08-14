use super::*;

/// Build an OAM image from a list of (index, attr0, attr1, attr2) entries.
fn oam_with(entries: &[(usize, u16, u16, u16)]) -> Vec<u8> {
    let mut oam = vec![0u8; OBJECT_COUNT * 8];
    // Everything not named is parked as hidden, which is what a game does with the entries it
    // is not using.
    for index in 0..OBJECT_COUNT {
        oam[index * 8..index * 8 + 2].copy_from_slice(&(2u16 << 8).to_le_bytes());
    }
    for &(index, a0, a1, a2) in entries {
        oam[index * 8..index * 8 + 2].copy_from_slice(&a0.to_le_bytes());
        oam[index * 8 + 2..index * 8 + 4].copy_from_slice(&a1.to_le_bytes());
        oam[index * 8 + 4..index * 8 + 6].copy_from_slice(&a2.to_le_bytes());
    }
    oam
}

#[test]
fn shape_and_size_only_name_a_dimension_together() {
    // Neither field means anything alone; the pair indexes a table of twelve.
    let cases = [
        (0, 0, (8, 8)),
        (0, 3, (64, 64)),
        (1, 0, (16, 8)),
        (1, 3, (64, 32)),
        (2, 0, (8, 16)),
        (2, 3, (32, 64)),
    ];
    for (shape, size, expected) in cases {
        let object = Object::decode((shape as u16) << 14, (size as u16) << 14, 0);
        assert_eq!(
            (object.width, object.height),
            expected,
            "shape {shape} size {size}"
        );
    }
}

#[test]
fn the_prohibited_shape_falls_back_to_a_square_rather_than_to_nothing() {
    // Hardware draws *something*; a zero-sized sprite would look like a decoding bug and no
    // game uses these combinations anyway.
    for size in 0..4u16 {
        let object = Object::decode(3 << 14, size << 14, 0);
        assert!(object.width > 0 && object.height > 0, "size {size}");
        assert_eq!(object.width, object.height, "and it is square");
    }
}

#[test]
fn coordinates_wrap_so_a_sprite_can_sit_partly_off_the_top_or_left() {
    // The fields are unsigned, and the hardware reaches negative positions by wrapping past the
    // far edge rather than by having a sign bit.
    let object = Object::decode(0x00F0, 0x01F0, 0);
    assert_eq!(object.y, -16);
    assert_eq!(object.x, -16);

    let object = Object::decode(0x0010, 0x0010, 0);
    assert_eq!(object.y, 16);
    assert_eq!(object.x, 16);
}

#[test]
fn the_four_object_modes_decode_distinctly() {
    assert_eq!(Object::decode(0, 0, 0).mode, ObjectMode::Normal);
    assert_eq!(Object::decode(1 << 8, 0, 0).mode, ObjectMode::Affine);
    assert_eq!(Object::decode(2 << 8, 0, 0).mode, ObjectMode::Hidden);
    assert_eq!(Object::decode(3 << 8, 0, 0).mode, ObjectMode::AffineDouble);
}

#[test]
fn a_hidden_sprite_is_not_the_same_as_a_zero_sized_one() {
    // Hiding is how a game parks an object it is not using without disturbing the rest of its
    // attributes, so the size and tile must survive.
    let object = Object::decode(2 << 8 | (1 << 14), 3 << 14, 0x0123);
    assert!(!object.visible());
    assert_eq!((object.width, object.height), (64, 32));
    assert_eq!(object.tile, 0x0123);
}

#[test]
fn an_object_window_sprite_draws_nothing_itself() {
    let object = Object::decode(2 << 10, 0, 0);
    assert_eq!(object.graphics_mode, GraphicsMode::ObjectWindow);
    assert!(!object.visible(), "its shape is a region, not a picture");
}

#[test]
fn a_semi_transparent_sprite_is_still_drawn() {
    let object = Object::decode(1 << 10, 0, 0);
    assert_eq!(object.graphics_mode, GraphicsMode::SemiTransparent);
    assert!(object.visible());
}

#[test]
fn the_double_size_mode_doubles_the_area_but_not_the_sprite() {
    // The larger area exists so a rotated sprite is not clipped by its own bounding box; the
    // tile data it draws from is unchanged.
    let object = Object::decode(3 << 8, 1 << 14, 0);
    assert_eq!((object.width, object.height), (16, 16));
    assert_eq!(object.screen_size(), (32, 32));
}

#[test]
fn the_flip_bits_mean_nothing_when_a_matrix_is_in_use() {
    // They share their position with the affine index, so reading them anyway both flips a
    // sprite that should not be flipped and corrupts which matrix it uses.
    let attr1 = (1 << 12) | (1 << 13);
    let normal = Object::decode(0, attr1, 0);
    assert!(normal.flip_x && normal.flip_y);

    let affine = Object::decode(1 << 8, attr1, 0);
    assert!(!affine.flip_x && !affine.flip_y);
    assert_eq!(affine.matrix, 0b11000, "the same bits are the matrix index");
}

#[test]
fn the_tile_number_ignores_its_low_bit_in_two_hundred_and_fifty_six_colour_mode() {
    // The number is counted in 32-byte units whatever the depth, so in 256-colour mode — where
    // a tile is 64 bytes — the low bit does not select anything.
    let attr0_8bpp = 1 << 13;
    assert_eq!(Object::decode(attr0_8bpp, 0, 0x0007).tile, 6);
    assert_eq!(Object::decode(0, 0, 0x0007).tile, 7, "but not at 4bpp");
}

#[test]
fn the_palette_field_is_unused_in_two_hundred_and_fifty_six_colour_mode() {
    assert_eq!(Object::decode(1 << 13, 0, 9 << 12).palette, 0);
    assert_eq!(Object::decode(0, 0, 9 << 12).palette, 9);
}

#[test]
fn row_stride_depends_on_a_bit_that_applies_to_every_sprite_at_once() {
    // One-dimensional mapping makes a sprite's tiles contiguous; two-dimensional makes the
    // object area one 32-tile sheet that the sprite is a window onto. Getting it backwards
    // gives sprites built from the right tiles in the wrong arrangement.
    let object = Object::decode(1 << 14, 1 << 14, 0); // 32x8, 4bpp
    assert_eq!(object.row_stride(true), 4 * 32, "its own width, in tiles");
    assert_eq!(object.row_stride(false), 32 * 32, "a full sheet row");
}

#[test]
fn only_the_one_dimensional_row_stride_scales_with_colour_depth() {
    // The half of this that is easy to get wrong, and did: in *one-dimensional* mapping a
    // sprite's tiles are contiguous, so its rows really are further apart at 256 colours. In
    // *two-dimensional* mapping the sheet is 32 slots wide and a slot is 32 bytes whatever the
    // depth — the same unit a tile number counts in — so one row down is 1024 bytes for every
    // sprite there has ever been.
    //
    // Scaling that by `tile_size()` gave 2048, which is two rows on. The top row of a 256-colour
    // sprite decoded correctly and every row below it came from the wrong place, which reads as
    // scrambled artwork rather than as a mapping bug.
    let object = Object::decode((1 << 14) | (1 << 13), 1 << 14, 0); // 32x8, 8bpp
    assert_eq!(
        object.row_stride(true),
        4 * 64,
        "contiguous tiles, so 256 colours doubles the distance"
    );
    assert_eq!(
        object.row_stride(false),
        1024,
        "a sheet row is 32 slots of 32 bytes, whatever the depth"
    );

    // And the 16-colour sprite agrees, which is what makes 1024 a property of the sheet rather
    // than a coincidence of this sprite's depth.
    let sixteen = Object::decode(1 << 14, 1 << 14, 0); // 32x8, 4bpp
    assert_eq!(sixteen.row_stride(false), object.row_stride(false));
}

#[test]
fn sprite_tiles_are_addressed_from_the_object_half_of_vram() {
    let object = Object::decode(0, 0, 4);
    assert_eq!(object.tile_offset(true), OBJ_TILE_BASE + 4 * 32);
}

#[test]
fn a_sprite_covers_only_the_lines_it_reaches() {
    let object = Object::decode(20, 1 << 14, 0); // y=20, 16x16
    assert!(!object.covers_line(19));
    assert!(object.covers_line(20));
    assert!(object.covers_line(35));
    assert!(!object.covers_line(36));
}

#[test]
fn a_double_size_sprite_covers_twice_as_many_lines() {
    let object = Object::decode((3 << 8) | 20, 1 << 14, 0);
    assert!(object.covers_line(51), "the doubled area reaches this far");
    assert!(!object.covers_line(52));
}

#[test]
fn the_matrices_are_gathered_from_between_the_sprites() {
    // Matrix components are interleaved with the sprite entries: component n of matrix m is the
    // last halfword of OAM entry (m * 4 + n). Reading OAM as a flat sprite array and then
    // looking for matrices elsewhere finds nothing.
    let mut oam = oam_with(&[]);
    // Matrix 1's four components: entries 4, 5, 6, 7, seventh and eighth bytes of each.
    for (n, value) in [0x0100u16, 0x0000, 0x0000, 0x0100].iter().enumerate() {
        let entry = 4 + n;
        oam[entry * 8 + 6..entry * 8 + 8].copy_from_slice(&value.to_le_bytes());
    }

    let decoded = ObjectAttributeMemory::decode(&oam);
    assert_eq!(
        decoded.matrices[1],
        AffineMatrix {
            pa: 0x0100,
            pb: 0,
            pc: 0,
            pd: 0x0100,
        },
        "the identity matrix, in 8.8 fixed point"
    );
}

#[test]
fn every_entry_is_decoded_not_just_the_first_few() {
    let oam = oam_with(&[(127, 0x0040, 0, 0x0055)]);
    let decoded = ObjectAttributeMemory::decode(&oam);
    assert_eq!(decoded.objects[127].y, 64);
    assert_eq!(decoded.objects[127].tile, 0x55);
}

#[test]
fn sprites_on_a_line_come_back_front_most_first() {
    // Priority first, then OAM index — a lower index wins a tie, which is how a game controls
    // overlap without changing priorities.
    let oam = oam_with(&[
        (0, 10, 0, 2 << 10), // priority 2
        (1, 10, 0, 0),       // priority 0
        (2, 10, 0, 0),       // priority 0, later in OAM
        (3, 10, 0, 1 << 10), // priority 1
    ]);
    let decoded = ObjectAttributeMemory::decode(&oam);
    assert_eq!(decoded.visible_on_line(12), vec![1, 2, 3, 0]);
}

#[test]
fn hidden_and_window_sprites_are_left_out_of_the_line_list() {
    let oam = oam_with(&[
        (0, 10, 0, 0),
        (1, 10 | (2 << 8), 0, 0),  // hidden
        (2, 10 | (2 << 10), 0, 0), // object window
    ]);
    let decoded = ObjectAttributeMemory::decode(&oam);
    assert_eq!(decoded.visible_on_line(12), vec![0]);
}

#[test]
fn a_sprite_that_does_not_reach_the_line_is_left_out() {
    let oam = oam_with(&[(0, 100, 0, 0)]);
    let decoded = ObjectAttributeMemory::decode(&oam);
    assert!(decoded.visible_on_line(12).is_empty());
    assert_eq!(decoded.visible_on_line(100), vec![0]);
}

#[test]
fn a_short_oam_image_decodes_without_panicking() {
    // Defensive because OAM arrives as a slice from the memory map, and a truncated one during
    // a save-state load must not take the renderer down mid-frame. It reads as zeros, which is
    // 128 eight-by-eight sprites at the origin — exactly what hardware shows for a
    // freshly-powered OAM, so there is nothing to special-case beyond not indexing off the end.
    let decoded = ObjectAttributeMemory::decode(&[]);
    assert_eq!(decoded.visible_on_line(0).len(), OBJECT_COUNT);
    assert!(decoded.visible_on_line(8).is_empty(), "8x8 at the origin");
    assert_eq!(decoded.matrices[31], AffineMatrix::default());
}

#[test]
fn conversion_to_the_shared_sprite_type_carries_what_the_compositor_needs() {
    // Square shape (no shape bits), size 1, so 16x16.
    let object = Object::decode(20, 30 | (1 << 12) | (1 << 14), 7 | (3 << 12));
    let sprite = object.to_sprite(true);
    assert_eq!(sprite.x, 30);
    assert_eq!(sprite.y, 20);
    assert_eq!((sprite.width, sprite.height), (16, 16));
    assert_eq!(sprite.palette, 3);
    assert!(sprite.flip_x);
    assert_eq!(sprite.tile_offset, OBJ_TILE_BASE + 7 * 32);
    assert!(
        !sprite.behind_background,
        "the GBA resolves this per layer, not with one bit"
    );
}

#[test]
fn an_affine_matrix_round_trips() {
    use savestate::{decode_state, encode_state};
    let matrix = AffineMatrix {
        pa: 0x0100,
        pb: -256,
        pc: 128,
        pd: -1,
    };
    let bytes = encode_state("gba-matrix", 1, &matrix);
    let mut restored = AffineMatrix::default();
    decode_state("gba-matrix", 1, &bytes, &mut restored).unwrap();
    assert_eq!(restored, matrix);
}
