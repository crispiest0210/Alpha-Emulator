use super::*;
use savestate::{decode_state, encode_state};

const EWRAM: u32 = 0x0200_0000;
const IWRAM: u32 = 0x0300_0000;
const PALETTE: u32 = 0x0500_0000;
const VRAM: u32 = 0x0600_0000;
const OAM: u32 = 0x0700_0000;

#[test]
fn the_address_nibble_selects_the_region() {
    assert_eq!(Region::of(0x0000_0100), Region::Bios);
    assert_eq!(Region::of(EWRAM), Region::EWram);
    assert_eq!(Region::of(IWRAM), Region::IWram);
    assert_eq!(Region::of(0x0400_0000), Region::Io);
    assert_eq!(Region::of(PALETTE), Region::Palette);
    assert_eq!(Region::of(VRAM), Region::Vram);
    assert_eq!(Region::of(OAM), Region::Oam);
    assert_eq!(Region::of(0x0E00_0000), Region::Sram);
    // The three ROM windows are the same ROM at different wait states, which is how a game
    // changes timing without moving its data.
    assert_eq!(Region::of(0x0800_0000), Region::Rom { wait_state: 0 });
    assert_eq!(Region::of(0x0900_0000), Region::Rom { wait_state: 0 });
    assert_eq!(Region::of(0x0A00_0000), Region::Rom { wait_state: 1 });
    assert_eq!(Region::of(0x0C00_0000), Region::Rom { wait_state: 2 });
    // The gap between BIOS and EWRAM is real and belongs to nothing.
    assert_eq!(Region::of(0x0100_0000), Region::Unmapped);
}

#[test]
fn work_ram_mirrors_across_its_whole_region() {
    let mut bus = GbaBus::new(None);
    bus.write8(EWRAM, 0xAB);
    assert_eq!(bus.read8(EWRAM + EWRAM_SIZE as u32), Some(0xAB));
    // 0x00FC_0000 is 63 whole mirrors up; a mirror boundary, unlike a round-looking address.
    assert_eq!(bus.read8(EWRAM + 0x00FC_0000), Some(0xAB));

    bus.write8(IWRAM, 0xCD);
    assert_eq!(bus.read8(IWRAM + IWRAM_SIZE as u32), Some(0xCD));
    assert_eq!(bus.read8(IWRAM + 0x00FF_8000), Some(0xCD));
}

#[test]
fn vram_mirrors_its_last_thirty_two_kilobytes_rather_than_wrapping_to_the_start() {
    // The 96 KiB region sits in a 128 KiB window, and the missing 32 KiB is a second view of
    // the 32 KiB before it. Games write object tiles through both views; treating the gap as a
    // plain wrap corrupts sprite graphics in a way that looks like a tile-decoding bug.
    assert_eq!(vram_offset(VRAM), 0);
    assert_eq!(vram_offset(VRAM + 0x0001_7FFF), 0x0001_7FFF);
    assert_eq!(vram_offset(VRAM + 0x0001_8000), 0x0001_0000, "not 0");
    assert_eq!(vram_offset(VRAM + 0x0001_FFFF), 0x0001_7FFF);
    // And the whole 128 KiB window repeats.
    assert_eq!(vram_offset(VRAM + 0x0002_0000), 0);

    let mut bus = GbaBus::new(None);
    bus.write16(VRAM + 0x0001_0000, 0xBEEF);
    assert_eq!(bus.read16(VRAM + 0x0001_8000), Some(0xBEEF));
}

#[test]
fn a_byte_write_to_palette_ram_lands_in_both_halves_of_the_halfword() {
    // Palette RAM is on a 16-bit bus and cannot take a byte. Storing one plainly produces
    // single-pixel colour corruption that is very hard to trace back to the store.
    let mut bus = GbaBus::new(None);
    bus.write8(PALETTE + 1, 0x5A);
    assert_eq!(bus.read16(PALETTE), Some(0x5A5A));
}

#[test]
fn a_byte_write_to_the_background_half_of_vram_is_doubled_too() {
    let mut bus = GbaBus::new(None);
    bus.write8(VRAM + 3, 0x77);
    assert_eq!(bus.read16(VRAM + 2), Some(0x7777));
}

#[test]
fn a_byte_write_to_oam_is_dropped_entirely() {
    // Unlike palette RAM and VRAM, OAM does not double the byte — it ignores the write.
    let mut bus = GbaBus::new(None);
    bus.write16(OAM, 0x1234);
    bus.write8(OAM, 0xFF);
    assert_eq!(bus.read16(OAM), Some(0x1234), "the write was ignored");
}

#[test]
fn a_byte_write_to_the_object_half_of_vram_is_dropped_like_oam() {
    let mut bus = GbaBus::new(None);
    bus.write16(VRAM + 0x0001_0000, 0x1234);
    bus.write8(VRAM + 0x0001_0000, 0xFF);
    assert_eq!(bus.read16(VRAM + 0x0001_0000), Some(0x1234));
}

#[test]
fn a_halfword_write_is_not_subject_to_the_byte_quirk() {
    let mut bus = GbaBus::new(None);
    bus.write16(PALETTE, 0x1234);
    assert_eq!(bus.read16(PALETTE), Some(0x1234));
    bus.write32(VRAM, 0xDEAD_BEEF);
    assert_eq!(bus.read32(VRAM), Some(0xDEAD_BEEF));
}

#[test]
fn the_bios_answers_only_code_running_inside_it() {
    // A cartridge that reads the BIOS from its own code is checking for an emulator that maps
    // it unconditionally. Returning the real bytes there is a detectable difference.
    let mut bios = vec![0u8; BIOS_SIZE];
    bios[0x10] = 0x42;
    let mut bus = GbaBus::new(Some(bios));

    bus.set_open_bus(0);
    assert_eq!(bus.read8(0x10), Some(0), "the game sees open bus");

    bus.set_in_bios(true);
    assert_eq!(bus.read8(0x10), Some(0x42), "BIOS code sees the BIOS");
}

#[test]
fn a_machine_with_no_bios_reads_the_region_as_bios_open_bus() {
    // Not the general open-bus field: BIOS reads from outside the BIOS are the BIOS's *own*
    // sticky last-fetched value, a separate mechanism — see `bios_open_bus`.
    let mut bus = GbaBus::new(None);
    bus.set_in_bios(true);
    bus.set_bios_open_bus(0xDEAD_BEEF);
    assert_eq!(bus.read8(0x00), Some(0xEF));
}

#[test]
fn an_unmapped_read_returns_the_matching_byte_of_the_last_bus_word() {
    // Open bus is word-granular: a byte read from nowhere returns the byte of the last *word*
    // at that alignment, not simply the last byte transferred.
    let mut bus = GbaBus::new(None);
    bus.set_open_bus(0x1122_3344);
    assert_eq!(bus.read8(0x0100_0000), Some(0x44));
    assert_eq!(bus.read8(0x0100_0001), Some(0x33));
    assert_eq!(bus.read8(0x0100_0002), Some(0x22));
    assert_eq!(bus.read8(0x0100_0003), Some(0x11));
}

#[test]
fn the_cartridge_and_io_regions_are_left_to_their_owners() {
    // The same split prompt 06 uses for the Game Boy: this module owns the board, the mapper
    // owns the cartridge, and the system assembly owns the registers.
    let mut bus = GbaBus::new(None);
    assert_eq!(bus.read8(0x0400_0000), None);
    assert_eq!(bus.read8(0x0800_0000), None);
    assert_eq!(bus.read8(0x0E00_0000), None);
    assert!(!bus.write8(0x0400_0000, 0));
    assert!(!bus.write8(0x0800_0000, 0));
}

#[test]
fn reads_and_writes_are_aligned_before_they_are_performed() {
    // The ARM core hands over unaligned addresses and expects the bus to have forced alignment;
    // prompt 04 puts the rotation on the CPU side, so a misaligned access here must not split
    // across two words.
    let mut bus = GbaBus::new(None);
    bus.write32(IWRAM, 0xAABB_CCDD);
    assert_eq!(bus.read32(IWRAM + 2), Some(0xAABB_CCDD));
    assert_eq!(bus.read16(IWRAM + 1), Some(0xCCDD));
}

#[test]
fn memory_round_trips_without_carrying_the_bios() {
    // The BIOS is user-supplied, identical across runs, and 16 KiB that would otherwise sit in
    // every rewind frame.
    let mut bus = GbaBus::new(Some(vec![0xFF; BIOS_SIZE]));
    bus.write32(EWRAM, 0x1234_5678);
    bus.write16(VRAM + 0x100, 0xABCD);
    bus.write16(OAM + 8, 0x0F0F);
    bus.set_open_bus(0x9999_9999);

    let bytes = encode_state("gba-memory", 1, &bus);
    let mut restored = GbaBus::new(None);
    decode_state("gba-memory", 1, &bytes, &mut restored).unwrap();

    assert_eq!(restored.read32(EWRAM), Some(0x1234_5678));
    assert_eq!(restored.read16(VRAM + 0x100), Some(0xABCD));
    assert_eq!(restored.read16(OAM + 8), Some(0x0F0F));
    assert_eq!(restored.open_bus32(), 0x9999_9999);
    assert!(!restored.has_bios(), "the state did not smuggle one in");
}
