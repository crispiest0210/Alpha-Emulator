use super::*;

/// A ROM with a valid header and the two binaries where it says they are.
fn rom_with(arm9: &[u8], arm7: &[u8]) -> Vec<u8> {
    let mut rom = vec![0u8; 0x8000];
    rom[..12].copy_from_slice(b"ALPHA TEST\0\0");
    rom[0x0C..0x10].copy_from_slice(b"ATST");
    rom[0x10..0x12].copy_from_slice(b"01");

    let put = |rom: &mut Vec<u8>, at: usize, v: u32| {
        rom[at..at + 4].copy_from_slice(&v.to_le_bytes());
    };
    put(&mut rom, 0x20, 0x4000); // ARM9 ROM offset
    put(&mut rom, 0x24, 0x0200_0800); // ARM9 entry
    put(&mut rom, 0x28, 0x0200_0000); // ARM9 RAM address
    put(&mut rom, 0x2C, arm9.len() as u32);
    put(&mut rom, 0x30, 0x6000); // ARM7 ROM offset
    put(&mut rom, 0x34, 0x0380_0100); // ARM7 entry
    put(&mut rom, 0x38, 0x0380_0000); // ARM7 RAM address
    put(&mut rom, 0x3C, arm7.len() as u32);

    rom[0x4000..0x4000 + arm9.len()].copy_from_slice(arm9);
    rom[0x6000..0x6000 + arm7.len()].copy_from_slice(arm7);
    rom
}

fn cart() -> NdsCartridge {
    NdsCartridge::new(rom_with(&[0xAA; 0x100], &[0x77; 0x80])).unwrap()
}

/// Kick a transfer of `words` words with the given command.
fn transfer(c: &mut NdsCartridge, command: [u8; 8], block: u32) {
    // The command is a byte string and `command[0]` is its first byte, which lives at the register's
    // own address — so a 32-bit write carries it in the *low* byte, as any little-endian store does.
    c.write32(
        reg::CARD_COMMAND,
        u32::from_le_bytes(command[0..4].try_into().unwrap()),
    );
    c.write32(
        0x0400_01AC,
        u32::from_le_bytes(command[4..8].try_into().unwrap()),
    );
    c.write32(reg::ROMCTRL, romctrl::START | (block << 24));
}

#[test]
fn a_header_says_where_the_two_binaries_are() {
    let c = cart();
    let h = c.header();
    assert_eq!(h.title, "ALPHA TEST");
    assert_eq!(h.game_code, "ATST");
    assert_eq!(h.maker_code, "01");
    assert_eq!(h.arm9_entry, 0x0200_0800);
    assert_eq!(h.arm7_ram_address, 0x0380_0000);
    assert_eq!(h.arm9_size, 0x100);
}

#[test]
fn a_rom_too_small_to_hold_a_header_is_rejected() {
    assert!(matches!(
        NdsCartridge::new(vec![0; 16]),
        Err(CartridgeError::TooSmall { .. })
    ));
}

#[test]
fn a_header_pointing_past_the_end_of_the_file_is_rejected() {
    // Otherwise direct boot copies zeroes into RAM and jumps into them, which presents as a
    // black screen rather than as a bad file.
    let mut rom = rom_with(&[0; 4], &[0; 4]);
    rom[0x2C..0x30].copy_from_slice(&0x10_0000u32.to_le_bytes());
    let err = NdsCartridge::new(rom).unwrap_err();
    assert!(
        matches!(&err, CartridgeError::BadHeader(m) if m.contains("ARM9")),
        "{err}"
    );
}

#[test]
fn direct_boot_hands_back_both_binaries_and_both_entry_points() {
    let c = cart();
    let (nine, nine_bytes, seven, seven_bytes) = c.direct_boot();
    assert_eq!(nine.entry, 0x0200_0800);
    assert_eq!(nine.ram_address, 0x0200_0000);
    assert_eq!(nine_bytes.len(), 0x100);
    assert!(nine_bytes.iter().all(|b| *b == 0xAA));

    assert_eq!(seven.entry, 0x0380_0100);
    assert_eq!(seven_bytes.len(), 0x80);
    assert!(seven_bytes.iter().all(|b| *b == 0x77));
}

#[test]
fn the_rom_is_not_readable_without_a_card_command() {
    // The whole point of this module: a DS game cannot see its own ROM through the address bus.
    let mut c = cart();
    assert_eq!(c.read32(0x0800_0000), None);
    assert_eq!(c.read32(0x0200_0000), None);
    assert!(NdsCartridge::owns(reg::ROMCTRL));
    assert!(NdsCartridge::owns(reg::CARD_DATA));
    assert!(!NdsCartridge::owns(0x0400_0000));
}

#[test]
fn command_b7_reads_a_block_out_of_the_rom() {
    let mut c = cart();
    // Block size 1 is 0x200 bytes, which is 128 words.
    transfer(&mut c, [0xB7, 0, 0, 0x40, 0x00, 0, 0, 0], 1);
    assert!(c.data_ready());
    // 0x4000 is the ARM9 binary, all 0xAA.
    assert_eq!(c.read32(reg::CARD_DATA), Some(0xAAAA_AAAA));
    for _ in 1..128 {
        c.read32(reg::CARD_DATA);
    }
    assert!(!c.data_ready(), "the transfer finished");
}

#[test]
fn command_zero_reads_the_header() {
    let mut c = cart();
    transfer(&mut c, [0x00, 0, 0, 0, 0, 0, 0, 0], 1);
    let word = c.read32(reg::CARD_DATA).unwrap();
    assert_eq!(&word.to_le_bytes(), b"ALPH");
}

#[test]
fn the_block_size_field_of_seven_is_one_word_not_thirty_two_kilobytes() {
    // Read as a plain shift, this makes every chip-ID read a transfer that never completes.
    let mut c = cart();
    transfer(&mut c, [0xB8, 0, 0, 0, 0, 0, 0, 0], 7);
    assert!(c.data_ready());
    let id = c.read32(reg::CARD_DATA).unwrap();
    assert_eq!(id & 0xFF, 0xC2);
    assert!(!c.data_ready(), "exactly one word");
}

#[test]
fn a_block_size_of_zero_transfers_nothing() {
    let mut c = cart();
    transfer(&mut c, [0xB7, 0, 0, 0x40, 0, 0, 0, 0], 0);
    assert!(!c.data_ready());
    assert_eq!(c.read32(reg::ROMCTRL).unwrap() & romctrl::START, 0);
}

#[test]
fn an_unrecognised_command_reads_as_all_ones() {
    let mut c = cart();
    transfer(&mut c, [0x3C, 0, 0, 0, 0, 0, 0, 0], 7);
    assert_eq!(c.read32(reg::CARD_DATA), Some(0xFFFF_FFFF));
}

#[test]
fn over_reading_a_transfer_returns_ones_rather_than_spinning() {
    let mut c = cart();
    transfer(&mut c, [0xB8, 0, 0, 0, 0, 0, 0, 0], 7);
    c.read32(reg::CARD_DATA);
    assert_eq!(c.read32(reg::CARD_DATA), Some(0xFFFF_FFFF));
    assert_eq!(c.read32(reg::CARD_DATA), Some(0xFFFF_FFFF));
}

#[test]
fn a_read_past_the_end_of_the_rom_is_ones_rather_than_a_panic() {
    let mut c = cart();
    transfer(&mut c, [0xB7, 0x0F, 0xFF, 0xF0, 0x00, 0, 0, 0], 1);
    assert_eq!(c.read32(reg::CARD_DATA), Some(0xFFFF_FFFF));
}

#[test]
fn a_read_above_the_secure_area_wraps_inside_its_four_kilobyte_block() {
    // A driver relies on this for the last partial block of a file.
    let mut rom = rom_with(&[0; 4], &[0; 4]);
    rom[0x7FFC..0x8000].copy_from_slice(&0x1234_5678u32.to_le_bytes());
    rom[0x7000..0x7004].copy_from_slice(&0xCAFE_BABEu32.to_le_bytes());
    let mut c = NdsCartridge::new(rom).unwrap();

    transfer(&mut c, [0xB7, 0x00, 0x00, 0x7F, 0xFC, 0, 0, 0], 7);
    assert_eq!(c.read32(reg::CARD_DATA), Some(0x1234_5678));
    // The next word wraps to the start of the 4 KiB block rather than running into 0x8000.
    transfer(&mut c, [0xB7, 0x00, 0x00, 0x7F, 0xFC, 0, 0, 0], 0);
    transfer(&mut c, [0xB7, 0x00, 0x00, 0x70, 0x00, 0, 0, 0], 7);
    assert_eq!(c.read32(reg::CARD_DATA), Some(0xCAFE_BABE));
}

#[test]
fn the_command_is_eight_bytes_at_eight_addresses() {
    // The property that matters is not the width of the access but *which address holds which byte
    // of the command*: the first byte — the opcode — is at the register's own address, and the four
    // that name the ROM offset follow it upwards. A 32-bit write therefore carries the opcode in
    // its low byte, because that is the byte an ARM store puts at the lowest address.
    //
    // Modelled the other way round, as one big-endian word, this test still passes when written as
    // a round trip and every *byte* write silently lands at the mirror of its own position. libnds
    // writes the command a byte at a time, so what it actually got was the fourth byte of its
    // command read back as the opcode.
    let mut c = cart();
    // `B7 00 00 40 00 ...`: the opcode, then 0x4000 as four bytes, most significant first.
    c.write32(
        reg::CARD_COMMAND,
        u32::from_le_bytes([0xB7, 0x00, 0x00, 0x40]),
    );
    c.write32(0x0400_01AC, u32::from_le_bytes([0x00, 0x00, 0x00, 0x00]));
    c.write32(reg::ROMCTRL, romctrl::START | (1 << 24));
    assert_eq!(
        c.read32(reg::CARD_DATA),
        Some(0xAAAA_AAAA),
        "0xB7 reading from 0x4000"
    );

    // The same command assembled one byte at a time, the way libnds assembles it, has to mean the
    // same thing.
    let mut c = cart();
    for (offset, byte) in [0xB7u8, 0x00, 0x00, 0x40, 0x00, 0x00, 0x00, 0x00]
        .into_iter()
        .enumerate()
    {
        c.write8(reg::CARD_COMMAND + offset as u32, byte);
    }
    c.write32(reg::ROMCTRL, romctrl::START | (1 << 24));
    assert_eq!(c.read32(reg::CARD_DATA), Some(0xAAAA_AAAA));
}

#[test]
fn an_empty_slot_answers_every_command_with_ones() {
    let mut c = NdsCartridge::empty();
    assert!(!c.is_present());
    transfer(&mut c, [0xB7, 0, 0, 0, 0, 0, 0, 0], 7);
    assert_eq!(c.read32(reg::CARD_DATA), Some(0xFFFF_FFFF));
    transfer(&mut c, [0xB8, 0, 0, 0, 0, 0, 0, 0], 7);
    assert_eq!(c.read32(reg::CARD_DATA), Some(0xFFFF_FFFF));
}

#[test]
fn there_is_no_save_chip_and_it_says_so() {
    let mut c = cart();
    assert!(c.save_ram().is_none());
    // The auxiliary SPI data port reads as zero rather than as a chip that is not there.
    assert_eq!(c.read16(reg::AUXSPIDATA), Some(0));
    assert!(c.write16(reg::AUXSPIDATA, 0x9F));
}

#[test]
fn narrow_accesses_reach_the_registers() {
    let mut c = cart();
    c.write16(reg::AUXSPICNT, 0x8040);
    assert_eq!(c.read16(reg::AUXSPICNT), Some(0x8040));
    // Byte zero of the command is at byte zero of the register, which is where a driver writing
    // the command one byte at a time puts the opcode.
    c.write8(reg::CARD_COMMAND, 0xB7);
    assert_eq!(c.read8(reg::CARD_COMMAND), Some(0xB7));
    c.write8(reg::CARD_COMMAND + 7, 0x5A);
    assert_eq!(c.read8(reg::CARD_COMMAND + 7), Some(0x5A));
}

#[test]
fn the_cartridge_round_trips_through_a_save_state_mid_transfer() {
    use savestate::{decode_state, encode_state};

    let mut c = cart();
    transfer(&mut c, [0xB7, 0, 0, 0x40, 0x00, 0, 0, 0], 1);
    c.read32(reg::CARD_DATA);
    c.read32(reg::CARD_DATA);

    let blob = encode_state("nds", 1, &c);
    let mut restored = cart();
    decode_state("nds", 1, &blob, &mut restored).unwrap();

    assert!(restored.data_ready());
    // Two of 128 words already taken. The ARM9 binary is only 0x100 bytes, so 62 more are
    // 0xAA and the rest of the 0x200-byte block is the zeroed tail of the file.
    for _ in 0..62 {
        assert_eq!(restored.read32(reg::CARD_DATA), Some(0xAAAA_AAAA));
    }
    for _ in 0..64 {
        assert_eq!(restored.read32(reg::CARD_DATA), Some(0));
    }
    assert!(!restored.data_ready());
}

#[test]
fn a_state_claiming_an_impossible_transfer_length_is_rejected() {
    use savestate::{decode_state, encode_state, StateWriter};

    let mut w = StateWriter::new();
    w.write_u16(0);
    w.write_u32(0);
    w.write_bytes(&[0u8; 8]);
    w.write_u64(0x10_0000); // far larger than any block
    let blob = encode_state("nds", 1, &RawBlob(w.into_inner()));

    let mut restored = cart();
    assert!(decode_state("nds", 1, &blob, &mut restored).is_err());
}

struct RawBlob(Vec<u8>);

impl Savable for RawBlob {
    fn save(&self, w: &mut StateWriter) {
        w.write_bytes(&self.0);
    }
    fn load(&mut self, _r: &mut StateReader) -> Result<(), StateError> {
        Ok(())
    }
}
