use super::*;

fn mem() -> NdsMemory {
    NdsMemory::default()
}

#[test]
fn each_core_sees_its_own_regions() {
    assert_eq!(Arm9Region::of(0x0200_0000), Arm9Region::MainRam);
    assert_eq!(Arm9Region::of(0x0300_0000), Arm9Region::SharedWram);
    assert_eq!(Arm9Region::of(0x0400_0004), Arm9Region::Io);
    assert_eq!(Arm9Region::of(0x0500_0400), Arm9Region::Palette);
    assert_eq!(Arm9Region::of(0x0600_0000), Arm9Region::Vram);
    assert_eq!(Arm9Region::of(0x0700_0000), Arm9Region::Oam);
    assert_eq!(Arm9Region::of(0xFFFF_0000), Arm9Region::Bios);
    assert_eq!(Arm9Region::of(0xFFFF_7FFF), Arm9Region::Bios);
    // The ARM9 has no BIOS at zero and no private WRAM.
    assert_eq!(Arm9Region::of(0x0000_0000), Arm9Region::Unmapped);
    assert_eq!(Arm9Region::of(0x0380_0000), Arm9Region::SharedWram);

    assert_eq!(Arm7Region::of(0x0000_0000), Arm7Region::Bios);
    assert_eq!(Arm7Region::of(0x0200_0000), Arm7Region::MainRam);
    assert_eq!(Arm7Region::of(0x0300_0000), Arm7Region::SharedWram);
    assert_eq!(Arm7Region::of(0x037F_FFFF), Arm7Region::SharedWram);
    assert_eq!(Arm7Region::of(0x0380_0000), Arm7Region::Arm7Wram);
    assert_eq!(Arm7Region::of(0x0400_0000), Arm7Region::Io);
    assert_eq!(Arm7Region::of(0x0480_0000), Arm7Region::Wifi);
    // No palette or OAM in the ARM7's map at all — those addresses are simply nothing.
    assert_eq!(Arm7Region::of(0x0500_0000), Arm7Region::Unmapped);
    assert_eq!(Arm7Region::of(0x0700_0000), Arm7Region::Unmapped);
}

#[test]
fn main_ram_is_one_memory_seen_by_both_cores() {
    let mut m = mem();
    m.write8_arm9(0x0200_1234, 0x5A);
    assert_eq!(m.read8_arm7(0x0200_1234), Some(0x5A));

    m.write8_arm7(0x0200_1235, 0xA5);
    assert_eq!(m.read8_arm9(0x0200_1235), Some(0xA5));
}

#[test]
fn main_ram_mirrors_through_its_whole_window() {
    let mut m = mem();
    m.write8_arm9(0x0200_0010, 0x11);
    // 4 MiB of storage in a 16 MiB window: four views of the same byte.
    assert_eq!(m.read8_arm9(0x0240_0010), Some(0x11));
    assert_eq!(m.read8_arm9(0x0280_0010), Some(0x11));
    assert_eq!(m.read8_arm9(0x02FF_0010 & !0x003F_0000), Some(0x11));
}

#[test]
fn arm7_wram_is_invisible_to_the_arm9() {
    let mut m = mem();
    m.write8_arm7(0x0380_0000, 0x77);
    assert_eq!(m.read8_arm7(0x0380_0000), Some(0x77));
    // The same address on the ARM9 is the shared WRAM window, which is a different memory.
    assert_eq!(m.read8_arm9(0x0380_0000), Some(0x00));
}

#[test]
fn the_default_split_gives_the_arm9_all_of_shared_wram() {
    let mut m = mem();
    assert_eq!(m.split(), WramSplit::Arm9All);
    m.write8_arm9(0x0300_0000, 0x01);
    m.write8_arm9(0x0300_4000, 0x02);
    assert_eq!(m.read8_arm9(0x0300_0000), Some(0x01));
    assert_eq!(m.read8_arm9(0x0300_4000), Some(0x02));

    // With no share, the ARM7's window is its own WRAM instead — not the bytes just written.
    assert_eq!(m.read8_arm7(0x0300_0000), Some(0x00));
    m.write8_arm7(0x0300_0000, 0xEE);
    assert_eq!(
        m.read8_arm7(0x0380_0000),
        Some(0xEE),
        "it is the same memory"
    );
    assert_eq!(m.read8_arm9(0x0300_0000), Some(0x01), "and not the ARM9's");
}

#[test]
fn a_split_hands_each_core_a_different_half_of_the_same_memory() {
    let mut m = mem();
    // Write both halves while the ARM9 can see all of it.
    m.write8_arm9(0x0300_0000, 0xAA); // first 16 KiB
    m.write8_arm9(0x0300_4000, 0xBB); // second 16 KiB

    m.set_split(WramSplit::Arm9Second);
    assert_eq!(
        m.read8_arm9(0x0300_0000),
        Some(0xBB),
        "ARM9 sees the second"
    );
    assert_eq!(m.read8_arm7(0x0300_0000), Some(0xAA), "ARM7 sees the first");

    m.set_split(WramSplit::Arm9First);
    assert_eq!(m.read8_arm9(0x0300_0000), Some(0xAA));
    assert_eq!(m.read8_arm7(0x0300_0000), Some(0xBB));
}

#[test]
fn a_split_half_mirrors_within_the_cores_window() {
    let mut m = mem();
    m.set_split(WramSplit::Arm9First);
    m.write8_arm9(0x0300_0000, 0x42);
    // 16 KiB assigned, so every 16 KiB is the same view.
    assert_eq!(m.read8_arm9(0x0300_4000), Some(0x42));
    assert_eq!(m.read8_arm9(0x0300_8000), Some(0x42));
}

#[test]
fn giving_the_arm7_everything_leaves_the_arm9_window_answering_nothing() {
    let mut m = mem();
    m.write8_arm9(0x0300_0000, 0xCD);
    m.set_split(WramSplit::Arm7All);

    m.set_open_bus9(0xDEAD_BEEF);
    assert_eq!(
        m.read8_arm9(0x0300_0000),
        Some(0xEF),
        "open bus, not the byte"
    );
    assert_eq!(m.read8_arm7(0x0300_0000), Some(0xCD));

    // And a write through the dead window must not reach the memory the ARM7 now owns.
    m.write8_arm9(0x0300_0000, 0x00);
    assert_eq!(m.read8_arm7(0x0300_0000), Some(0xCD));
}

#[test]
fn the_two_cores_have_independent_open_bus_values() {
    let mut m = mem();
    m.set_open_bus9(0x1111_1111);
    m.set_open_bus7(0x2222_2222);
    // `0x0B` is unmapped on both.
    assert_eq!(m.read8_arm9(0x0B00_0000), Some(0x11));
    assert_eq!(m.read8_arm7(0x0B00_0000), Some(0x22));
}

#[test]
fn open_bus_is_word_granular() {
    let mut m = mem();
    m.set_open_bus9(0x0403_0201);
    assert_eq!(m.read8_arm9(0x0B00_0000), Some(0x01));
    assert_eq!(m.read8_arm9(0x0B00_0001), Some(0x02));
    assert_eq!(m.read8_arm9(0x0B00_0002), Some(0x03));
    assert_eq!(m.read8_arm9(0x0B00_0003), Some(0x04));
}

#[test]
fn io_vram_and_the_slot2_cartridge_are_not_this_modules_business() {
    let mut m = mem();
    assert_eq!(m.read8_arm9(0x0400_0000), None);
    assert_eq!(m.read8_arm9(0x0600_0000), None);
    assert_eq!(m.read8_arm9(0x0800_0000), None);
    assert_eq!(m.read8_arm9(0x0A00_0000), None);
    assert!(!m.write8_arm9(0x0400_0000, 0));
    assert!(!m.write8_arm9(0x0600_0000, 0));

    assert_eq!(m.read8_arm7(0x0400_0000), None);
    assert_eq!(m.read8_arm7(0x0600_0000), None);
    assert!(!m.write8_arm7(0x0400_0000, 0));
}

#[test]
fn palette_and_oam_reject_byte_writes_but_take_halfwords() {
    let mut m = mem();
    m.write8_arm9(0x0500_0000, 0xFF);
    m.write8_arm9(0x0700_0000, 0xFF);
    assert_eq!(m.read8_arm9(0x0500_0000), Some(0x00), "byte write dropped");
    assert_eq!(m.read8_arm9(0x0700_0000), Some(0x00), "byte write dropped");

    m.write16_arm9(0x0500_0000, 0x7C1F);
    assert_eq!(m.read8_arm9(0x0500_0000), Some(0x1F));
    assert_eq!(m.read8_arm9(0x0500_0001), Some(0x7C));

    m.write16_arm9(0x0700_0002, 0x1234);
    assert_eq!(m.read8_arm9(0x0700_0002), Some(0x34));
    assert_eq!(m.read8_arm9(0x0700_0003), Some(0x12));
}

#[test]
fn both_engines_palettes_and_oams_are_one_block_each() {
    let mut m = mem();
    // Engine B's palette starts 1 KiB into the region, and its OAM likewise.
    m.write16_arm9(0x0500_0400, 0xABCD);
    assert_eq!(m.palette()[0x400], 0xCD);
    assert_eq!(m.palette()[0], 0x00, "engine A is untouched");

    m.write16_arm9(0x0700_0400, 0xABCD);
    assert_eq!(m.oam()[0x400], 0xCD);
    assert_eq!(m.oam()[0], 0x00);

    // Both mirror through their 16 MiB windows, in 2 KiB steps — the size of the two engines'
    // blocks together, not of one engine's.
    assert_eq!(m.read8_arm9(0x0500_0C00), Some(0xCD));
    assert_eq!(m.read8_arm9(0x0500_0800), Some(0x00), "that is engine A");
}

#[test]
fn a_bios_is_optional_and_reads_as_open_bus_when_absent() {
    let mut m = mem();
    assert!(!m.has_arm9_bios());
    assert!(!m.has_arm7_bios());
    m.set_open_bus9(0x9999_9999);
    m.set_open_bus7(0x7777_7777);
    assert_eq!(m.read8_arm9(0xFFFF_0000), Some(0x99));
    assert_eq!(m.read8_arm7(0x0000_0000), Some(0x77));
}

#[test]
fn a_supplied_bios_answers_at_its_own_base_on_each_core() {
    let mut arm9 = vec![0u8; ARM9_BIOS_SIZE];
    arm9[0] = 0x12;
    arm9[ARM9_BIOS_SIZE - 1] = 0x34;
    let mut arm7 = vec![0u8; ARM7_BIOS_SIZE];
    arm7[0] = 0x56;

    let m = NdsMemory::new(Some(arm9), Some(arm7));
    assert!(m.has_arm9_bios() && m.has_arm7_bios());
    assert_eq!(m.read8_arm9(0xFFFF_0000), Some(0x12));
    assert_eq!(m.read8_arm9(0xFFFF_7FFF), Some(0x34));
    assert_eq!(m.read8_arm7(0x0000_0000), Some(0x56));
    // Neither BIOS is visible to the other core.
    assert_eq!(m.read8_arm7(0xFFFF_0000), Some(0x00));
    assert_eq!(m.read8_arm9(0x0000_0000), Some(0x00));
}

#[test]
fn neither_bios_is_writable() {
    let m9 = vec![0u8; ARM9_BIOS_SIZE];
    let m7 = vec![0u8; ARM7_BIOS_SIZE];
    let mut m = NdsMemory::new(Some(m9), Some(m7));
    m.write8_arm9(0xFFFF_0000, 0xFF);
    m.write8_arm7(0x0000_0000, 0xFF);
    assert_eq!(m.read8_arm9(0xFFFF_0000), Some(0x00));
    assert_eq!(m.read8_arm7(0x0000_0000), Some(0x00));
}

#[test]
fn split_bits_round_trip() {
    for bits in 0u8..4 {
        assert_eq!(WramSplit::from_bits(bits).bits(), bits);
    }
    // Only the low two bits select; the rest of the register is other fields.
    assert_eq!(WramSplit::from_bits(0xFE), WramSplit::from_bits(2));
}

#[test]
fn memory_round_trips_through_a_save_state() {
    use savestate::{decode_state, encode_state};

    let mut m = mem();
    m.write8_arm9(0x0200_0100, 0x11);
    m.write8_arm9(0x0300_0000, 0x22);
    m.write8_arm7(0x0380_0000, 0x33);
    m.write16_arm9(0x0500_0000, 0x4455);
    m.write16_arm9(0x0700_0000, 0x6677);
    m.set_split(WramSplit::Arm9First);
    m.set_open_bus9(0xAAAA_AAAA);

    let blob = encode_state("nds", 1, &m);
    let mut restored = mem();
    decode_state("nds", 1, &blob, &mut restored).unwrap();

    assert_eq!(restored.read8_arm9(0x0200_0100), Some(0x11));
    assert_eq!(restored.read8_arm9(0x0300_0000), Some(0x22));
    assert_eq!(restored.read8_arm7(0x0380_0000), Some(0x33));
    assert_eq!(restored.read8_arm9(0x0500_0001), Some(0x44));
    assert_eq!(restored.read8_arm9(0x0700_0001), Some(0x66));
    assert_eq!(restored.split(), WramSplit::Arm9First);
    assert_eq!(restored.read8_arm9(0x0B00_0000), Some(0xAA));
}
