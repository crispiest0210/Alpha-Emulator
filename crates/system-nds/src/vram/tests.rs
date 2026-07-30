use super::*;

/// Bank indices, so the tests read like the register names do.
const A: usize = 0;
const B: usize = 1;
const C: usize = 2;
const D: usize = 3;
const E: usize = 4;
const F: usize = 5;
const G: usize = 6;
const H: usize = 7;
const I: usize = 8;

/// A `VRAMCNT` value: enabled, with the given MST and OFS.
fn cnt(mst: u8, ofs: u8) -> u8 {
    0x80 | (ofs << 3) | mst
}

/// Seed a bank so a read can tell which bank answered.
fn fill(v: &mut Vram, bank: usize, byte: u8) {
    v.bank_mut(bank).fill(byte);
}

#[test]
fn the_page_table_covers_every_space_exactly_once() {
    let mut expected = 0;
    for (i, space) in SPACES.iter().enumerate() {
        assert_eq!(
            SPACE_FIRST_PAGE[i], expected,
            "{space:?} starts at the wrong page"
        );
        assert_eq!(*space as usize, i, "SPACES must be in discriminant order");
        expected += space.pages();
    }
    assert_eq!(expected, TOTAL_PAGES);
    // Every space is a whole number of pages.
    for space in SPACES {
        assert_eq!(space.size() % PAGE_SIZE, 0, "{space:?}");
    }
    assert_eq!(BANK_SIZES.iter().sum::<usize>(), TOTAL_VRAM);
}

#[test]
fn a_bank_is_mapped_nowhere_until_it_is_enabled() {
    let mut v = Vram::new();
    fill(&mut v, A, 0xAA);
    // MST=1 selects engine A backgrounds, but the enable bit is clear.
    v.set_control(A, 0x01);
    assert_eq!(v.read8(VramSpace::BgA, 0), 0);
    assert_eq!(v.read8(VramSpace::Lcdc, 0), 0, "not even through LCDC");
    assert!(v.banks_at(VramSpace::BgA, 0).is_empty());

    v.set_control(A, cnt(1, 0));
    assert_eq!(v.read8(VramSpace::BgA, 0), 0xAA);
}

#[test]
fn ofs_moves_a_bank_within_its_space() {
    let mut v = Vram::new();
    fill(&mut v, A, 0x11);
    for ofs in 0..4u8 {
        v.set_control(A, cnt(1, ofs));
        let base = 0x2_0000 * ofs as u32;
        assert_eq!(v.read8(VramSpace::BgA, base), 0x11, "OFS={ofs}");
        assert_eq!(v.read8(VramSpace::BgA, base + 0x1_FFFF), 0x11);
        // And nothing outside the 128 KiB it occupies.
        if ofs > 0 {
            assert_eq!(v.read8(VramSpace::BgA, base - 1), 0);
        }
    }
}

#[test]
fn the_object_space_only_takes_the_low_ofs_bit() {
    let mut v = Vram::new();
    fill(&mut v, A, 0x22);
    // Engine A's sprite space is 256 KiB, so OFS 2 and 3 alias 0 and 1.
    v.set_control(A, cnt(2, 2));
    assert_eq!(v.read8(VramSpace::ObjA, 0), 0x22);
    v.set_control(A, cnt(2, 3));
    assert_eq!(v.read8(VramSpace::ObjA, 0x2_0000), 0x22);
}

#[test]
fn two_banks_at_one_address_are_ored_on_read_and_both_take_a_write() {
    let mut v = Vram::new();
    v.set_control(A, cnt(1, 0));
    v.set_control(B, cnt(1, 0));
    assert_eq!(v.banks_at(VramSpace::BgA, 0x100), vec![A, B]);

    v.bank_mut(A)[0x100] = 0x0F;
    v.bank_mut(B)[0x100] = 0xF0;
    assert_eq!(v.read8(VramSpace::BgA, 0x100), 0xFF, "ORed, not first-wins");

    // A write during the overlap must reach both, or unmapping one loses it.
    v.write8(VramSpace::BgA, 0x100, 0x5A);
    assert_eq!(v.bank(A)[0x100], 0x5A);
    assert_eq!(v.bank(B)[0x100], 0x5A);
}

#[test]
fn a_write_reports_whether_anything_took_it() {
    let mut v = Vram::new();
    assert!(!v.write8(VramSpace::BgA, 0, 0x11), "nothing mapped");
    v.set_control(A, cnt(1, 0));
    assert!(v.write8(VramSpace::BgA, 0, 0x11));
    // Past the bank but inside the space is still a hole.
    assert!(!v.write8(VramSpace::BgA, 0x2_0000, 0x11));
    // And past the space entirely.
    assert!(!v.write8(VramSpace::BgA, 0x8_0000, 0x11));
}

#[test]
fn every_bank_appears_at_its_own_fixed_place_in_the_lcdc_window() {
    // The one arrangement where all nine are visible at once, which is how software uploads to
    // banks it has assigned to the 3D core.
    let expected = [
        (A, 0x0_0000u32),
        (B, 0x2_0000),
        (C, 0x4_0000),
        (D, 0x6_0000),
        (E, 0x8_0000),
        (F, 0x9_0000),
        (G, 0x9_4000),
        (H, 0x9_8000),
        (I, 0xA_0000),
    ];
    let mut v = Vram::new();
    for (bank, _) in expected {
        v.set_control(bank, cnt(0, 0));
        fill(&mut v, bank, bank as u8 + 1);
    }
    for (bank, base) in expected {
        assert_eq!(
            v.read8(VramSpace::Lcdc, base),
            bank as u8 + 1,
            "bank {} at {base:#X}",
            BANK_NAMES[bank]
        );
        let last = base + BANK_SIZES[bank] as u32 - 1;
        assert_eq!(v.read8(VramSpace::Lcdc, last), bank as u8 + 1);
    }
    // The window is exactly full: 656 KiB, no gaps and nothing past the end.
    assert_eq!(v.read8(VramSpace::Lcdc, TOTAL_VRAM as u32), 0);
}

#[test]
fn banks_c_and_d_are_how_the_arm7_gets_vram() {
    let mut v = Vram::new();
    fill(&mut v, C, 0xCC);
    fill(&mut v, D, 0xDD);
    v.set_control(C, cnt(2, 0));
    v.set_control(D, cnt(2, 1));

    assert_eq!(v.read8(VramSpace::Arm7, 0), 0xCC);
    assert_eq!(v.read8(VramSpace::Arm7, 0x2_0000), 0xDD);
    // And they are then not in engine A's background space at all.
    assert_eq!(v.read8(VramSpace::BgA, 0), 0);
}

#[test]
fn engine_b_gets_its_own_banks_and_bank_i_sits_above_bank_h() {
    let mut v = Vram::new();
    fill(&mut v, H, 0x77);
    fill(&mut v, I, 0x99);
    v.set_control(H, cnt(1, 0));
    v.set_control(I, cnt(1, 0));

    // H is 32 KiB at the start of engine B's background space, I is 16 KiB directly above it.
    assert_eq!(v.read8(VramSpace::BgB, 0), 0x77);
    assert_eq!(v.read8(VramSpace::BgB, 0x7FFF), 0x77);
    assert_eq!(v.read8(VramSpace::BgB, 0x8000), 0x99);
    assert_eq!(v.read8(VramSpace::BgB, 0xBFFF), 0x99);
    assert_eq!(v.read8(VramSpace::BgB, 0xC000), 0);

    // C and D are engine B's other option, and they take the whole space.
    v.set_control(C, cnt(4, 0));
    fill(&mut v, C, 0x33);
    assert_eq!(v.read8(VramSpace::BgB, 0x1_0000), 0x33);
    v.set_control(D, cnt(4, 0));
    fill(&mut v, D, 0x44);
    assert_eq!(v.read8(VramSpace::ObjB, 0), 0x44);
}

#[test]
fn the_four_texture_slots_are_filled_by_ofs() {
    let mut v = Vram::new();
    for (i, bank) in [A, B, C, D].into_iter().enumerate() {
        fill(&mut v, bank, 0xA0 + i as u8);
        v.set_control(bank, cnt(3, i as u8));
    }
    for i in 0..4u32 {
        assert_eq!(
            v.read8(VramSpace::Texture, 0x2_0000 * i),
            0xA0 + i as u8,
            "texture slot {i}"
        );
    }
    assert!(v.space_is_mapped(VramSpace::Texture));
}

#[test]
fn bank_f_splits_its_ofs_into_two_different_steps() {
    // This is the trap in the mapping table: F's two OFS bits are a 16 KiB step and a 64 KiB
    // step, not one two-bit number, so OFS=2 lands at 0x10000 rather than 0x8000.
    let expected = [0x0_0000u32, 0x0_4000, 0x1_0000, 0x1_4000];
    for (ofs, base) in expected.into_iter().enumerate() {
        let mut v = Vram::new();
        fill(&mut v, F, 0xF0);
        v.set_control(F, cnt(1, ofs as u8));
        assert_eq!(v.read8(VramSpace::BgA, base), 0xF0, "OFS={ofs}");
        assert_eq!(v.read8(VramSpace::BgA, base + 0x3FFF), 0xF0);
        assert_eq!(v.read8(VramSpace::BgA, base + 0x4000), 0, "16 KiB only");
    }
}

#[test]
fn a_bank_larger_than_its_target_space_is_truncated_rather_than_overflowing() {
    // Bank E is 64 KiB; the extended background palette space is 32 KiB. Only half of E is used
    // and the mapping must not run off the end of the space.
    let mut v = Vram::new();
    fill(&mut v, E, 0xEE);
    v.set_control(E, cnt(4, 0));
    assert_eq!(v.read8(VramSpace::BgExtPalA, 0), 0xEE);
    assert_eq!(v.read8(VramSpace::BgExtPalA, 0x7FFF), 0xEE);
    assert!(v.space_is_mapped(VramSpace::BgExtPalA));
    // Reading past the space is a hole, not a panic.
    assert_eq!(v.read8(VramSpace::BgExtPalA, 0x8000), 0);
}

#[test]
fn f_and_g_select_a_pair_of_extended_palette_slots() {
    let mut v = Vram::new();
    fill(&mut v, F, 0x0F);
    fill(&mut v, G, 0xF0);
    // Only the low OFS bit selects here: the space is 32 KiB, F covers 16 KiB of it.
    v.set_control(F, cnt(4, 0));
    v.set_control(G, cnt(4, 1));
    assert_eq!(v.read8(VramSpace::BgExtPalA, 0x0000), 0x0F, "slots 0-1");
    assert_eq!(v.read8(VramSpace::BgExtPalA, 0x4000), 0xF0, "slots 2-3");

    // The sprite extended palette is a single 8 KiB slot, so only half of F reaches it.
    v.set_control(F, cnt(5, 0));
    assert_eq!(v.read8(VramSpace::ObjExtPalA, 0), 0x0F);
    assert_eq!(v.read8(VramSpace::ObjExtPalA, 0x1FFF), 0x0F);
    assert!(v.space_is_mapped(VramSpace::ObjExtPalA));
    assert!(!v.space_is_mapped(VramSpace::ObjExtPalB));
}

#[test]
fn an_mst_a_bank_does_not_define_maps_nothing() {
    let mut v = Vram::new();
    fill(&mut v, A, 0xAA);
    // Bank A has no MST 4-7 at all.
    for mst in 4..8u8 {
        v.set_control(A, cnt(mst, 0));
        for space in SPACES {
            assert!(!v.space_is_mapped(space), "MST={mst} mapped {space:?}");
        }
    }
    // Bank H has no MST 3 either.
    v.set_control(A, 0);
    v.set_control(H, cnt(3, 0));
    assert!(!v.space_is_mapped(VramSpace::BgB));
}

#[test]
fn reassigning_a_bank_moves_it_rather_than_copying_it() {
    let mut v = Vram::new();
    v.set_control(A, cnt(1, 0));
    v.write8(VramSpace::BgA, 0x10, 0x5A);
    v.set_control(A, cnt(2, 0));
    assert_eq!(v.read8(VramSpace::BgA, 0x10), 0, "gone from where it was");
    assert_eq!(v.read8(VramSpace::ObjA, 0x10), 0x5A, "and is where it went");
}

#[test]
fn the_arm9_windows_mirror_within_themselves() {
    let mut v = Vram::new();
    // Each engine window is 2 MiB of address space over at most 512 KiB of mapping.
    assert_eq!(arm9_space(0x0600_0000), Some((VramSpace::BgA, 0)));
    assert_eq!(arm9_space(0x0608_0000), Some((VramSpace::BgA, 0)));
    assert_eq!(arm9_space(0x0620_0000), Some((VramSpace::BgB, 0)));
    assert_eq!(arm9_space(0x0622_0000), Some((VramSpace::BgB, 0)));
    assert_eq!(arm9_space(0x0640_0000), Some((VramSpace::ObjA, 0)));
    assert_eq!(arm9_space(0x0644_0000), Some((VramSpace::ObjA, 0)));
    assert_eq!(arm9_space(0x0660_0000), Some((VramSpace::ObjB, 0)));
    assert_eq!(arm9_space(0x0680_0000), Some((VramSpace::Lcdc, 0)));
    // The LCDC window does not mirror; past 656 KiB nothing answers.
    assert_eq!(arm9_space(0x0680_0000 + TOTAL_VRAM as u32), None);

    // The ARM7 sees one 256 KiB window, mirrored.
    assert_eq!(arm7_space(0x0600_0000), (VramSpace::Arm7, 0));
    assert_eq!(arm7_space(0x0604_0000), (VramSpace::Arm7, 0));

    v.set_control(A, cnt(1, 0));
    let (space, offset) = arm9_space(0x0608_0010).unwrap();
    v.write8(space, offset, 0x42);
    assert_eq!(v.read8(VramSpace::BgA, 0x10), 0x42);
}

#[test]
fn wide_accesses_compose_little_endian() {
    let mut v = Vram::new();
    v.set_control(A, cnt(1, 0));
    v.write16(VramSpace::BgA, 0x20, 0x1234);
    assert_eq!(v.read8(VramSpace::BgA, 0x20), 0x34);
    assert_eq!(v.read16(VramSpace::BgA, 0x20), 0x1234);
    v.write16(VramSpace::BgA, 0x22, 0xABCD);
    assert_eq!(v.read32(VramSpace::BgA, 0x20), 0xABCD_1234);
}

#[test]
fn a_write_spanning_a_page_boundary_reaches_both_pages() {
    let mut v = Vram::new();
    // F and G are adjacent 16 KiB banks in engine A's background space.
    v.set_control(F, cnt(1, 0));
    v.set_control(G, cnt(1, 1));
    v.write16(VramSpace::BgA, 0x3FFF, 0xBEEF);
    assert_eq!(v.bank(F)[0x3FFF], 0xEF);
    assert_eq!(v.bank(G)[0], 0xBE);
}

#[test]
fn vram_round_trips_through_a_save_state_and_rebuilds_its_mapping() {
    use savestate::{decode_state, encode_state};

    let mut v = Vram::new();
    v.set_control(A, cnt(1, 1));
    v.set_control(H, cnt(1, 0));
    v.write8(VramSpace::BgA, 0x2_0000, 0x5A);
    v.write8(VramSpace::BgB, 0x10, 0xA5);

    let blob = encode_state("nds", 1, &v);
    let mut restored = Vram::new();
    decode_state("nds", 1, &blob, &mut restored).unwrap();

    assert_eq!(restored.control(A), cnt(1, 1));
    // The page table is derived, so this only reads correctly if `load` rebuilt it.
    assert_eq!(restored.read8(VramSpace::BgA, 0x2_0000), 0x5A);
    assert_eq!(restored.read8(VramSpace::BgB, 0x10), 0xA5);
    assert_eq!(restored.banks_at(VramSpace::BgA, 0x2_0000), vec![A]);
}
