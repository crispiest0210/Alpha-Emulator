use super::*;

/// Read one byte at `address` the way a driver does: opcode, three address bytes, then data.
fn read(firmware: &mut Firmware, address: u32, len: usize) -> Vec<u8> {
    firmware.deselect();
    firmware.transfer(op::READ);
    for shift in [16, 8, 0] {
        firmware.transfer((address >> shift) as u8);
    }
    let out = (0..len).map(|_| firmware.transfer(0)).collect();
    firmware.deselect();
    out
}

fn status(firmware: &mut Firmware) -> u8 {
    firmware.deselect();
    firmware.transfer(op::READ_STATUS);
    let value = firmware.transfer(0);
    firmware.deselect();
    value
}

#[test]
fn an_idle_chip_does_not_report_a_write_in_progress() {
    // The whole reason this module exists: `0xFF` here is "busy", and a driver that waits for
    // busy to clear before doing anything never gets to do anything.
    let mut firmware = Firmware::new();
    assert_eq!(status(&mut firmware) & status::WIP, 0);
}

#[test]
fn the_header_points_at_the_settings_block() {
    let firmware = Firmware::new();
    let pointer = u16::from_le_bytes([
        firmware.image()[SETTINGS_POINTER],
        firmware.image()[SETTINGS_POINTER + 1],
    ]);
    assert_eq!(pointer as usize * 8, SETTINGS_A);
}

#[test]
fn both_settings_blocks_pass_the_checksum_software_applies() {
    let firmware = Firmware::new();
    for at in [SETTINGS_A, SETTINGS_B] {
        let block = &firmware.image()[at..at + SETTINGS_LEN];
        let stored = u16::from_le_bytes([block[0x72], block[0x73]]);
        assert_eq!(
            crate::bios::crc16(0xFFFF, &block[0x00..0x70]),
            stored,
            "settings block at {at:#X}"
        );
    }
}

#[test]
fn one_settings_block_is_exactly_one_update_newer_than_the_other() {
    // Software takes the newer of the two by this comparison. Equal counters are a tie it has no
    // rule for, and a difference of more than one reads as the *other* block being newer.
    let firmware = Firmware::new();
    let counter = |at: usize| {
        u16::from_le_bytes([firmware.image()[at + 0x70], firmware.image()[at + 0x71]]) & 0x7F
    };
    assert_eq!((counter(SETTINGS_A) + 1) & 0x7F, counter(SETTINGS_B));
}

#[test]
fn a_read_returns_the_image_and_runs_on_across_bytes() {
    let mut firmware = Firmware::new();
    let bytes = read(&mut firmware, SETTINGS_A as u32, 4);
    assert_eq!(bytes, firmware.image()[SETTINGS_A..SETTINGS_A + 4].to_vec());
}

#[test]
fn deselecting_ends_a_command_rather_than_continuing_it() {
    // Two reads in a row must both start where they were told to. Without the framing the second
    // continues the first, which is a whole settings block read from the wrong place.
    let mut firmware = Firmware::new();
    let first = read(&mut firmware, SETTINGS_A as u32, 4);
    let second = read(&mut firmware, SETTINGS_A as u32, 4);
    assert_eq!(first, second);
}

#[test]
fn a_write_needs_the_enable_latch_and_clears_nothing_without_it() {
    let mut firmware = Firmware::new();
    let before = firmware.image()[SETTINGS_A];
    firmware.transfer(op::PAGE_WRITE);
    for shift in [16, 8, 0] {
        firmware.transfer((SETTINGS_A as u32 >> shift) as u8);
    }
    firmware.transfer(0x5A);
    firmware.deselect();
    assert_eq!(firmware.image()[SETTINGS_A], before);

    firmware.transfer(op::WRITE_ENABLE);
    firmware.deselect();
    assert_eq!(status(&mut firmware) & status::WEL, status::WEL);
    firmware.transfer(op::PAGE_WRITE);
    for shift in [16, 8, 0] {
        firmware.transfer((SETTINGS_A as u32 >> shift) as u8);
    }
    firmware.transfer(0x5A);
    firmware.deselect();
    assert_eq!(firmware.image()[SETTINGS_A], 0x5A);
}

#[test]
fn a_page_write_wraps_inside_its_page() {
    // A driver writing a whole 256-byte block relies on this: the last byte lands at the end of
    // the page rather than at the start of the next one.
    let mut firmware = Firmware::new();
    firmware.transfer(op::WRITE_ENABLE);
    firmware.deselect();
    let base = SETTINGS_A as u32 + 0xFF;
    firmware.transfer(op::PAGE_WRITE);
    for shift in [16, 8, 0] {
        firmware.transfer((base >> shift) as u8);
    }
    firmware.transfer(0x11);
    firmware.transfer(0x22);
    firmware.deselect();
    assert_eq!(firmware.image()[SETTINGS_A + 0xFF], 0x11);
    assert_eq!(firmware.image()[SETTINGS_A], 0x22);
    // And the byte after the page is untouched, which is the half that would be silently wrong.
    assert_eq!(
        firmware.image()[SETTINGS_A + 0x100],
        Firmware::new().image()[SETTINGS_A + 0x100]
    );
}

#[test]
fn a_state_round_trip_keeps_the_image_and_the_command_in_progress() {
    let mut firmware = Firmware::new();
    firmware.transfer(op::WRITE_ENABLE);
    firmware.deselect();
    firmware.transfer(op::READ);
    firmware.transfer(0x03);

    let mut writer = StateWriter::new();
    firmware.save(&mut writer);
    let bytes = writer.into_inner();
    let mut restored = Firmware::new();
    let mut reader = StateReader::new(&bytes);
    restored.load(&mut reader).unwrap();
    assert_eq!(restored, firmware);
}
