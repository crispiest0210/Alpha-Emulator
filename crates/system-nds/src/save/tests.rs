use super::*;

/// Run one whole SPI transaction and return what the chip drove back.
fn transact(chip: &mut SaveChip, bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    for (i, byte) in bytes.iter().enumerate() {
        let last = i + 1 == bytes.len();
        out.push(chip.transfer(*byte, !last));
    }
    out
}

fn write_enable(chip: &mut SaveChip) {
    transact(chip, &[command::WRITE_ENABLE]);
}

/// A write of `data` at `addr`, with `address_bytes` of address — the width a game with that chip
/// would actually send.
fn write(chip: &mut SaveChip, address_bytes: usize, addr: usize, data: &[u8]) {
    write_enable(chip);
    let mut bytes = vec![command::WRITE];
    for i in (0..address_bytes).rev() {
        bytes.push((addr >> (i * 8)) as u8);
    }
    bytes.extend_from_slice(data);
    transact(chip, &bytes);
}

/// A read of `len` bytes at `addr`.
fn read(chip: &mut SaveChip, address_bytes: usize, addr: usize, len: usize) -> Vec<u8> {
    let mut bytes = vec![command::READ];
    for i in (0..address_bytes).rev() {
        bytes.push((addr >> (i * 8)) as u8);
    }
    bytes.extend(std::iter::repeat_n(0u8, len));
    let out = transact(chip, &bytes);
    out[1 + address_bytes..].to_vec()
}

#[test]
fn nothing_reaches_the_disk_until_the_chip_is_known() {
    // The whole safeguard: a file of the wrong shape is worse than no file, because the game
    // fails to read it back and the player loses a save without being told why.
    let mut chip = SaveChip::new();
    assert!(chip.kind().is_none());
    assert!(chip.save_ram().is_none());

    // Reads before classification are blank, which is what every one of these chips reads as
    // when erased — so a fresh cartridge behaves correctly during exactly that window.
    assert_eq!(read(&mut chip, 3, 0, 4), vec![0xFF; 4]);
    assert!(chip.save_ram().is_none());

    // A full-page write is decisive, and only then is there a file to write out.
    write(&mut chip, 3, 0, &[0xAA; 256]);
    assert_eq!(chip.kind(), Some(Flash256K));
    assert!(chip.save_ram().is_some(), "now there is a file to write");
}

#[test]
fn a_full_page_write_identifies_the_chip_that_wrote_it() {
    // The four page sizes at their three address widths give four distinct total lengths that no
    // other chip can produce, and page-aligned blocks are how save libraries actually write.
    for expected in [Eeprom512, Eeprom8K, Eeprom64K, Flash256K] {
        let mut chip = SaveChip::new();
        write(
            &mut chip,
            expected.address_bytes(),
            0,
            &vec![0xAA; expected.page_size()],
        );
        assert_eq!(chip.kind(), Some(expected), "{expected:?}");
    }
}

#[test]
fn an_ambiguous_write_is_held_rather_than_guessed_at() {
    // A five-byte transaction is a one-byte write on all three address widths. Guessing parses
    // part of the address as data or part of the data as address, and writes to the wrong place.
    let mut chip = SaveChip::new();
    write(&mut chip, 3, 0x1234, &[0x01]);
    assert_eq!(chip.kind(), None, "still undetermined");
    assert_eq!(chip.held_writes(), 1);
    assert!(chip.save_ram().is_none(), "and still nothing to write out");

    // A read of the same address is blank, because the write has not been placed — which is
    // consistent: a write nobody can place is one nobody can read back either.
    assert_eq!(read(&mut chip, 3, 0x1234, 1), vec![0xFF]);
}

#[test]
fn held_writes_are_replayed_once_something_decisive_arrives() {
    let mut chip = SaveChip::new();
    write(&mut chip, 3, 0x1234, &[0x01]);
    write(&mut chip, 3, 0x5678, &[0x02]);
    assert_eq!(chip.held_writes(), 2);

    // A full-page write settles it, and the two held ones land at the addresses they always
    // meant rather than being lost.
    write(&mut chip, 3, 0, &[0xAA; 256]);
    assert_eq!(chip.kind(), Some(Flash256K));
    assert_eq!(chip.held_writes(), 0);
    assert_eq!(read(&mut chip, 3, 0x1234, 1), vec![0x01]);
    assert_eq!(read(&mut chip, 3, 0x5678, 1), vec![0x02]);
}

#[test]
fn a_probe_settles_it_and_replays_what_was_waiting_too() {
    let mut chip = SaveChip::new();
    write(&mut chip, 3, 0x20, &[0x99]);
    assert_eq!(chip.held_writes(), 1);
    transact(&mut chip, &[command::READ_ID, 0, 0, 0]);
    assert!(chip.kind().is_some_and(|k| k.is_flash()));
    assert_eq!(read(&mut chip, 3, 0x20, 1), vec![0x99]);
}

#[test]
fn holding_is_bounded_rather_than_growing_forever() {
    // A cartridge that only ever writes partial pages and never probes cannot be identified, and
    // an unbounded queue would turn that into a memory leak instead of a logged failure.
    let mut chip = SaveChip::new();
    for i in 0..200u32 {
        write(&mut chip, 3, i as usize * 4, &[i as u8]);
    }
    assert_eq!(chip.kind(), None);
    assert!(chip.held_writes() <= 64, "{}", chip.held_writes());
    assert!(chip.save_ram().is_none(), "and still no file is written");
}

#[test]
fn a_large_eeprom_is_told_from_a_small_one_by_its_page() {
    let mut chip = SaveChip::new();
    write(&mut chip, 2, 0, &[0x11; 128]);
    assert_eq!(chip.kind(), Some(Eeprom64K));
    assert_eq!(chip.save_ram().unwrap().len(), 64 * 1024);

    // And a 32-byte page is the smaller one, at the same address width.
    let mut chip = SaveChip::new();
    write(&mut chip, 2, 0, &[0x11; 32]);
    assert_eq!(chip.kind(), Some(Eeprom8K));
}

#[test]
fn asking_for_a_jedec_id_settles_it_as_flash_on_its_own() {
    // `RDID` exists only on FLASH, so being asked is itself the answer.
    let mut chip = SaveChip::new();
    let out = transact(&mut chip, &[command::READ_ID, 0, 0, 0]);
    assert!(chip.kind().is_some_and(|k| k.is_flash()));
    assert_eq!(&out[1..], &[0x20, 0x40, 0x12]);
}

#[test]
fn the_high_half_read_settles_it_as_the_smallest_eeprom() {
    // That command exists only on the 512-byte chip.
    let mut chip = SaveChip::new();
    transact(&mut chip, &[command::READ_HIGH, 0, 0]);
    assert_eq!(chip.kind(), Some(Eeprom512));
}

#[test]
fn a_save_file_settles_the_type_outright_with_no_inference() {
    // The reason the heuristic only ever runs on a cartridge's first save.
    for (size, kind) in SIZES {
        let mut chip = SaveChip::new();
        chip.load_file(&vec![0x5A; size]).expect("a standard size");
        assert_eq!(chip.kind(), Some(kind), "{size} bytes");
        assert_eq!(chip.save_ram().unwrap().len(), size);
        assert_eq!(chip.save_ram().unwrap()[0], 0x5A);
    }
}

#[test]
fn a_save_file_of_no_standard_size_is_refused_rather_than_padded() {
    let mut chip = SaveChip::new();
    assert!(matches!(
        chip.load_file(&[0; 1234]),
        Err(CartridgeError::SaveSizeMismatch { .. })
    ));
    assert!(chip.kind().is_none(), "and nothing was adopted");
}

#[test]
fn a_write_then_a_read_returns_what_was_written() {
    let mut chip = SaveChip::new();
    chip.load_file(&vec![0xFF; 64 * 1024]).unwrap();
    write(&mut chip, 2, 0x100, &[1, 2, 3, 4]);
    assert_eq!(read(&mut chip, 2, 0x100, 4), vec![1, 2, 3, 4]);
    assert_eq!(
        read(&mut chip, 2, 0x104, 1),
        vec![0xFF],
        "and nothing past it"
    );
}

#[test]
fn a_write_without_the_enable_latch_is_ignored() {
    let mut chip = SaveChip::new();
    chip.load_file(&vec![0xFF; 8 * 1024]).unwrap();
    // No `WREN` first.
    transact(&mut chip, &[command::WRITE, 0, 0, 0xAA]);
    assert_eq!(read(&mut chip, 2, 0, 1), vec![0xFF]);
    assert!(!chip.is_dirty());

    write(&mut chip, 2, 0, &[0xAA]);
    assert_eq!(read(&mut chip, 2, 0, 1), vec![0xAA]);
}

#[test]
fn the_enable_latch_clears_itself_after_one_write() {
    // Otherwise a game that enables once writes forever, and a stray store corrupts a save.
    let mut chip = SaveChip::new();
    chip.load_file(&vec![0xFF; 8 * 1024]).unwrap();
    write(&mut chip, 2, 0, &[0x11]);
    transact(&mut chip, &[command::WRITE, 0, 1, 0x22]);
    assert_eq!(
        read(&mut chip, 2, 1, 1),
        vec![0xFF],
        "the second was refused"
    );
}

#[test]
fn the_status_register_reports_the_enable_latch_and_never_a_busy_chip() {
    let mut chip = SaveChip::new();
    chip.load_file(&vec![0xFF; 8 * 1024]).unwrap();
    assert_eq!(transact(&mut chip, &[command::READ_STATUS, 0])[1] & 0b11, 0);
    write_enable(&mut chip);
    let status = transact(&mut chip, &[command::READ_STATUS, 0])[1];
    assert_eq!(status & STATUS_WRITE_ENABLED, STATUS_WRITE_ENABLED);
    // Write timing is not modelled, so a game polling for "not busy" is satisfied at once.
    assert_eq!(status & STATUS_WRITE_IN_PROGRESS, 0);
}

#[test]
fn a_page_write_wraps_inside_its_page_rather_than_running_into_the_next() {
    // What these chips do, and what a game relies on when it writes a partial page.
    let mut chip = SaveChip::new();
    chip.load_file(&vec![0xFF; 8 * 1024]).unwrap();
    let page = Eeprom8K.page_size();
    // Start two bytes before the end of a page and write four.
    write(&mut chip, 2, page - 2, &[1, 2, 3, 4]);
    assert_eq!(read(&mut chip, 2, page - 2, 2), vec![1, 2]);
    assert_eq!(
        read(&mut chip, 2, 0, 2),
        vec![3, 4],
        "wrapped to the page top"
    );
    assert_eq!(
        read(&mut chip, 2, page, 2),
        vec![0xFF; 2],
        "not the next page"
    );
}

#[test]
fn the_small_eeproms_upper_half_is_a_separate_command() {
    let mut chip = SaveChip::new();
    chip.load_file(&[0xFF; 512]).unwrap();
    write_enable(&mut chip);
    transact(&mut chip, &[command::WRITE_HIGH, 0x00, 0x77]);

    // The high write landed at 0x100, not at 0.
    assert_eq!(read(&mut chip, 1, 0, 1), vec![0xFF]);
    let out = transact(&mut chip, &[command::READ_HIGH, 0x00, 0x00]);
    assert_eq!(out[2], 0x77);
}

#[test]
fn a_flash_chip_grows_to_cover_an_address_a_game_actually_touches() {
    // Committing to 512 KiB because it is commonest silently drops the upper half of a 1 MiB
    // cartridge's save, which is the failure this module exists to avoid.
    let mut chip = SaveChip::new();
    write(&mut chip, 3, 0, &[0xAA; 256]);
    assert_eq!(chip.kind(), Some(Flash256K));

    write(&mut chip, 3, 0x60_000, &[0x11]);
    // Once the chip is known, a short write is no longer ambiguous and applies at once.
    assert_eq!(chip.kind(), Some(Flash512K));
    write(&mut chip, 3, 0xC0_000, &[0x22]);
    assert_eq!(chip.kind(), Some(Flash1M));

    // And nothing written earlier was lost on the way up.
    assert_eq!(read(&mut chip, 3, 0, 1), vec![0xAA]);
    assert_eq!(read(&mut chip, 3, 0x60_000, 1), vec![0x11]);
    assert_eq!(read(&mut chip, 3, 0xC0_000, 1), vec![0x22]);
}

#[test]
fn a_chip_never_shrinks_so_an_old_save_still_loads() {
    let mut chip = SaveChip::new();
    chip.load_file(&vec![0x11; 1024 * 1024]).unwrap();
    assert_eq!(chip.kind(), Some(Flash1M));
    write(&mut chip, 3, 0, &[0x22]);
    assert_eq!(chip.kind(), Some(Flash1M), "a low write does not shrink it");
}

#[test]
fn erasing_returns_bytes_to_the_blank_state() {
    let mut chip = SaveChip::new();
    chip.load_file(&vec![0x00; 256 * 1024]).unwrap();
    write_enable(&mut chip);
    transact(&mut chip, &[command::PAGE_ERASE, 0, 0, 0]);
    assert_eq!(read(&mut chip, 3, 0, 4), vec![0xFF; 4]);
    assert_eq!(read(&mut chip, 3, 0x100, 1), vec![0x00], "one page only");

    write_enable(&mut chip);
    transact(&mut chip, &[command::CHIP_ERASE]);
    assert_eq!(read(&mut chip, 3, 0x1000, 1), vec![0xFF]);
}

#[test]
fn the_dirty_flag_tracks_real_changes_so_a_flush_is_not_scheduled_for_nothing() {
    let mut chip = SaveChip::new();
    chip.load_file(&vec![0xFF; 8 * 1024]).unwrap();
    assert!(!chip.is_dirty());

    // Writing the value that is already there is not a change.
    write(&mut chip, 2, 0, &[0xFF]);
    assert!(!chip.is_dirty());

    write(&mut chip, 2, 0, &[0x01]);
    assert!(chip.is_dirty());
    chip.clear_dirty();
    assert!(!chip.is_dirty());
}

#[test]
fn a_fresh_chip_is_blank_rather_than_zeroed() {
    // Zeroes are data a game may mistake for a real save; all ones is what an erased chip of
    // either technology actually reads as.
    let mut chip = SaveChip::new();
    write(&mut chip, 3, 0, &[0x01; 256]);
    assert_eq!(read(&mut chip, 3, 0x1000, 4), vec![0xFF; 4]);
}

#[test]
fn the_chip_round_trips_through_a_save_state() {
    use savestate::{decode_state, encode_state};

    let mut chip = SaveChip::new();
    write(&mut chip, 2, 0, &[0; 32]);
    write(&mut chip, 2, 0x40, &[1, 2, 3, 4]);
    write_enable(&mut chip);

    let blob = encode_state("nds", 1, &chip);
    let mut restored = SaveChip::new();
    decode_state("nds", 1, &blob, &mut restored).unwrap();

    assert_eq!(restored.kind(), chip.kind());
    assert_eq!(restored.save_ram(), chip.save_ram());
    assert_eq!(read(&mut restored, 2, 0x40, 4), vec![1, 2, 3, 4]);
    // The enable latch survived, so a write already authorised still lands.
    write_enable(&mut restored);
    transact(&mut restored, &[command::WRITE, 0, 0x50, 0x99]);
    assert_eq!(read(&mut restored, 2, 0x50, 1), vec![0x99]);
}

#[test]
fn a_state_whose_chip_and_data_disagree_is_rejected() {
    use savestate::{decode_state, encode_state, StateWriter};

    let mut w = StateWriter::new();
    w.write_u8(1); // Eeprom512
    w.write_blob(&[0u8; 64]); // but only 64 bytes of it
    w.write_u8(0);
    w.write_bool(false);
    let blob = encode_state("nds", 1, &RawBlob(w.into_inner()));

    let mut restored = SaveChip::new();
    assert!(decode_state("nds", 1, &blob, &mut restored).is_err());
}

#[test]
fn a_state_naming_a_chip_this_build_does_not_have_is_rejected() {
    use savestate::{decode_state, encode_state, StateWriter};

    let mut w = StateWriter::new();
    w.write_u8(99);
    w.write_blob(&[]);
    w.write_u8(0);
    w.write_bool(false);
    let blob = encode_state("nds", 1, &RawBlob(w.into_inner()));

    let mut restored = SaveChip::new();
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
