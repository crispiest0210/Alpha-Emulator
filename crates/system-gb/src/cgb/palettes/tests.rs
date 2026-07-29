use super::*;
use savestate::{decode_state, encode_state};

/// Write a colour through the register interface, the way a game does.
fn write_colour(p: &mut CgbPalettes, index_reg: u16, data_reg: u16, slot: u8, rgb555: u16) {
    p.write_register(index_reg, slot * 2);
    p.write_register(data_reg, rgb555 as u8);
    p.write_register(index_reg, slot * 2 + 1);
    p.write_register(data_reg, (rgb555 >> 8) as u8);
}

#[test]
fn palette_ram_powers_up_white() {
    // Not a detail: a game that draws before writing its palettes shows white on hardware and
    // would show black if this came up zeroed.
    let p = CgbPalettes::new();
    assert_eq!(p.lookup_bg(0, 0), Rgba8::WHITE);
    assert_eq!(p.lookup_sprite(7, 3), Rgba8::WHITE);
}

#[test]
fn a_colour_written_through_the_registers_comes_back_out_of_the_lookup() {
    let mut p = CgbPalettes::new();
    // Palette 2, colour 1, pure red.
    write_colour(&mut p, reg::BCPS, reg::BCPD, 2 * 4 + 1, 0x001F);
    assert_eq!(
        p.lookup_bg(2, 1),
        Rgba8 {
            r: 0xFF,
            g: 0,
            b: 0,
            a: 0xFF
        }
    );
    // Nothing else moved.
    assert_eq!(p.lookup_bg(2, 0), Rgba8::WHITE);
    assert_eq!(p.lookup_sprite(2, 1), Rgba8::WHITE);
}

#[test]
fn background_and_sprite_palettes_are_separate_memories() {
    let mut p = CgbPalettes::new();
    write_colour(&mut p, reg::BCPS, reg::BCPD, 0, 0x001F);
    write_colour(&mut p, reg::OCPS, reg::OCPD, 0, 0x7C00);
    assert_eq!(p.lookup_bg(0, 0).r, 0xFF);
    assert_eq!(p.lookup_bg(0, 0).b, 0x00);
    assert_eq!(p.lookup_sprite(0, 0).b, 0xFF);
    assert_eq!(p.lookup_sprite(0, 0).r, 0x00);
}

#[test]
fn auto_increment_walks_the_whole_bank_from_one_index_write() {
    // This is how a game uploads a palette set: point at zero once, then stream 64 bytes.
    let mut p = CgbPalettes::new();
    p.write_register(reg::BCPS, 0x80); // index 0, auto-increment on
    for byte in 0..PALETTE_BYTES {
        p.write_register(reg::BCPD, byte as u8);
    }
    p.write_register(reg::BCPS, 0x00); // back to the start, increment off
    for byte in 0..PALETTE_BYTES {
        assert_eq!(
            p.read_register(reg::BCPD),
            Some(byte as u8),
            "byte {byte} of the stream"
        );
        p.write_register(reg::BCPS, (byte as u8 + 1) & 0x3F);
    }
}

#[test]
fn auto_increment_wraps_within_the_bank() {
    let mut p = CgbPalettes::new();
    p.write_register(reg::BCPS, 0x80 | 0x3F); // last slot, auto-increment on
    p.write_register(reg::BCPD, 0xAB);
    assert_eq!(p.read_register(reg::BCPS), Some(0x80 | 0x40));
}

#[test]
fn reads_do_not_advance_the_index() {
    // A fade routine reads a colour, darkens it, and writes it back to the same slot. If a
    // read moved the index, every fade would smear across the palette.
    let mut p = CgbPalettes::new();
    p.write_register(reg::BCPS, 0x80);
    p.write_register(reg::BCPD, 0x11); // index advances to 1
    p.write_register(reg::BCPS, 0x80); // back to 0
    assert_eq!(p.read_register(reg::BCPD), Some(0x11));
    assert_eq!(p.read_register(reg::BCPD), Some(0x11), "still slot 0");
    assert_eq!(p.read_register(reg::BCPS), Some(0x80 | 0x40));
}

#[test]
fn the_index_register_reads_back_with_its_unused_bit_set() {
    let mut p = CgbPalettes::new();
    p.write_register(reg::OCPS, 0x05);
    assert_eq!(p.read_register(reg::OCPS), Some(0x45));
    p.write_register(reg::OCPS, 0x85);
    assert_eq!(p.read_register(reg::OCPS), Some(0xC5));
}

#[test]
fn five_bit_channels_expand_to_the_full_eight_bit_range() {
    // The naive `c << 3` leaves white at 248, which is a visible grey cast on every bright
    // colour. Both ends have to be exact.
    assert_eq!(rgb555_to_rgba8(0x0000), Rgba8::BLACK);
    assert_eq!(rgb555_to_rgba8(0x7FFF), Rgba8::WHITE);
    // And it must be monotonic in between.
    let mut previous = 0;
    for level in 0..32u16 {
        let value = rgb555_to_rgba8(level).r;
        assert!(value >= previous, "channel value went backwards at {level}");
        previous = value;
    }
}

#[test]
fn set_colour_matches_what_the_registers_would_have_written() {
    let mut direct = CgbPalettes::new();
    direct.set_colour(false, 3, 2, 0x1234);
    let mut through_registers = CgbPalettes::new();
    write_colour(
        &mut through_registers,
        reg::BCPS,
        reg::BCPD,
        3 * 4 + 2,
        0x1234,
    );
    // The register path leaves the index pointing at the last byte it wrote; `set_colour`
    // never touches it. Park both at zero so the comparison is about the colour data.
    through_registers.write_register(reg::BCPS, 0x00);
    assert_eq!(direct, through_registers);
}

#[test]
fn addresses_outside_the_block_are_not_claimed() {
    let mut p = CgbPalettes::new();
    assert!(!CgbPalettes::owns(0xFF67));
    assert!(!CgbPalettes::owns(0xFF6C));
    assert_eq!(p.read_register(0xFF67), None);
    assert_eq!(p.write_register(0xFF6C, 0), None);
}

#[test]
fn palette_state_round_trips() {
    let mut p = CgbPalettes::new();
    p.write_register(reg::BCPS, 0x80);
    for byte in 0..PALETTE_BYTES {
        p.write_register(reg::BCPD, (byte * 3) as u8);
    }
    p.write_register(reg::OCPS, 0x0A);
    p.write_register(reg::OCPD, 0x77);

    let bytes = encode_state("gbc-palettes", 1, &p);
    let mut restored = CgbPalettes::new();
    decode_state("gbc-palettes", 1, &bytes, &mut restored).unwrap();
    assert_eq!(p, restored);
}
