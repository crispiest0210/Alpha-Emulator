use super::*;

fn arm9() -> InterruptController {
    InterruptController::new(Core::Arm9)
}

fn arm7() -> InterruptController {
    InterruptController::new(Core::Arm7)
}

/// Enable everything this core has, master switch on.
fn armed(mut c: InterruptController) -> InterruptController {
    c.write32(reg::IE, u32::MAX);
    c.write32(reg::IME, 1);
    c
}

#[test]
fn the_three_registers_are_owned_and_nothing_next_to_them_is() {
    assert!(InterruptController::owns(reg::IME));
    assert!(InterruptController::owns(reg::IE));
    assert!(InterruptController::owns(reg::IF));
    assert!(InterruptController::owns(reg::IF + 2), "halfword within IF");

    // The registers either side. A mask wide enough to group IE and IF would take these too.
    assert!(!InterruptController::owns(0x0400_0218));
    assert!(!InterruptController::owns(0x0400_020C));
    assert!(!InterruptController::owns(0x0400_0200), "the GBA's IE");
}

#[test]
fn nothing_fires_until_both_switches_allow_it() {
    let mut c = arm9();
    c.raise(sources::VBLANK);
    assert!(!c.pending(), "IE and IME both clear");

    c.write32(reg::IE, sources::VBLANK);
    assert!(!c.pending(), "IME still clear");

    c.write32(reg::IME, 1);
    assert!(c.pending());

    c.write32(reg::IME, 0);
    assert!(!c.pending(), "the master switch alone stops it");
}

#[test]
fn a_disabled_source_still_records_its_flag() {
    let mut c = arm9();
    c.write32(reg::IME, 1);
    c.raise(sources::HBLANK);
    assert!(!c.pending());
    assert_eq!(
        c.flags(),
        sources::HBLANK,
        "IE gates dispatch, not recording"
    );
    assert_eq!(c.active(), 0);
}

#[test]
fn writing_ones_to_if_acknowledges_and_writing_zeroes_does_nothing() {
    let mut c = armed(arm9());
    c.raise(sources::VBLANK | sources::HBLANK | sources::TIMER0);

    c.write32(reg::IF, 0);
    assert_eq!(
        c.flags(),
        sources::VBLANK | sources::HBLANK | sources::TIMER0
    );

    c.write32(reg::IF, sources::HBLANK);
    assert_eq!(c.flags(), sources::VBLANK | sources::TIMER0);
    assert!(c.pending(), "the other two are still there");

    c.write32(reg::IF, u32::MAX);
    assert_eq!(c.flags(), 0);
    assert!(!c.pending());
}

#[test]
fn a_halfword_write_to_if_acknowledges_only_its_own_half() {
    let mut c = armed(arm9());
    c.raise(sources::VBLANK | sources::IPC_SYNC);
    // The naive implementation — read the word, splice in the halfword, store it — writes ones
    // back into the untouched half and acknowledges it too.
    c.write16(reg::IF, sources::VBLANK as u16);
    assert_eq!(c.flags(), sources::IPC_SYNC, "the high half survived");

    c.raise(sources::VBLANK);
    c.write16(reg::IF + 2, (sources::IPC_SYNC >> 16) as u16);
    assert_eq!(c.flags(), sources::VBLANK);
}

#[test]
fn a_byte_write_to_if_acknowledges_only_its_own_byte() {
    let mut c = armed(arm9());
    c.raise(sources::VBLANK | sources::DMA0 | sources::IPC_SYNC);
    c.write8(reg::IF, sources::VBLANK as u8);
    assert_eq!(c.flags(), sources::DMA0 | sources::IPC_SYNC);
    c.write8(reg::IF + 1, (sources::DMA0 >> 8) as u8);
    assert_eq!(c.flags(), sources::IPC_SYNC);
}

#[test]
fn a_halfword_write_to_ie_splices_rather_than_replacing() {
    let mut c = arm9();
    c.write32(reg::IE, sources::GEOMETRY_FIFO);
    c.write16(reg::IE, sources::VBLANK as u16);
    assert_eq!(
        c.read32(reg::IE),
        Some(sources::VBLANK | sources::GEOMETRY_FIFO)
    );
}

#[test]
fn the_two_cores_do_not_have_the_same_sources() {
    // Only the ARM9 has the geometry FIFO.
    let mut nine = armed(arm9());
    nine.raise(sources::GEOMETRY_FIFO);
    assert!(nine.pending());

    let mut seven = armed(arm7());
    seven.raise(sources::GEOMETRY_FIFO);
    assert_eq!(seven.flags(), 0, "dropped rather than left unserviceable");

    // And only the ARM7 has SPI, wifi, the lid, and the serial port.
    for source in [sources::SPI, sources::WIFI, sources::LID, sources::SERIAL] {
        let mut seven = armed(arm7());
        seven.raise(source);
        assert_eq!(seven.flags(), source);

        let mut nine = armed(arm9());
        nine.raise(source);
        assert_eq!(nine.flags(), 0, "{source:#X} is not an ARM9 source");
    }
}

#[test]
fn the_common_sources_are_common() {
    for source in [
        sources::VBLANK,
        sources::HBLANK,
        sources::VCOUNT,
        sources::timer(0),
        sources::timer(3),
        sources::dma(0),
        sources::dma(3),
        sources::KEYPAD,
        sources::GBA_SLOT,
        sources::IPC_SYNC,
        sources::IPC_SEND_EMPTY,
        sources::IPC_RECV_NOT_EMPTY,
        sources::CARD_TRANSFER,
        sources::CARD_IREQ,
    ] {
        for mut c in [armed(arm9()), armed(arm7())] {
            c.raise(source);
            assert_eq!(c.flags(), source, "{source:#X} on {:?}", c.core());
        }
    }
}

#[test]
fn ie_cannot_enable_a_source_the_core_does_not_have() {
    let mut c = arm7();
    c.write32(reg::IE, u32::MAX);
    assert_eq!(c.read32(reg::IE), Some(sources::ARM7));
    assert_eq!(c.read32(reg::IE).unwrap() & sources::GEOMETRY_FIFO, 0);

    let mut c = arm9();
    c.write32(reg::IE, u32::MAX);
    assert_eq!(c.read32(reg::IE), Some(sources::ARM9));
    assert_eq!(c.read32(reg::IE).unwrap() & sources::WIFI, 0);
}

#[test]
fn ime_is_one_bit_however_wide_the_write_was() {
    let mut c = arm9();
    c.write32(reg::IME, 0xFFFF_FFFF);
    assert_eq!(c.read32(reg::IME), Some(1));
    c.write32(reg::IME, 0xFFFF_FFFE);
    assert_eq!(c.read32(reg::IME), Some(0), "only bit 0 is the switch");
}

#[test]
fn timer_and_dma_source_bits_are_contiguous_from_their_channel() {
    assert_eq!(sources::timer(0), sources::TIMER0);
    assert_eq!(sources::timer(3), sources::TIMER3);
    assert_eq!(sources::dma(0), sources::DMA0);
    assert_eq!(sources::dma(3), sources::DMA3);
}

#[test]
fn narrow_reads_see_the_right_slice_of_a_word() {
    let mut c = arm9();
    c.raise(sources::VBLANK | sources::IPC_SEND_EMPTY);
    assert_eq!(c.read32(reg::IF), Some(0x0002_0001));
    assert_eq!(c.read16(reg::IF), Some(0x0001));
    assert_eq!(c.read16(reg::IF + 2), Some(0x0002));
    assert_eq!(c.read8(reg::IF), Some(0x01));
    assert_eq!(c.read8(reg::IF + 2), Some(0x02));
    assert_eq!(c.read8(reg::IF + 3), Some(0x00));
    assert_eq!(c.read32(0x0400_0218), None);
}

#[test]
fn the_two_vectors_are_the_same_offset_at_different_bases() {
    // The ARM9 runs with high vectors, which is a CP15 setting `cpu-arm946e` already implements.
    assert_eq!(ARM9_IRQ_VECTOR & 0xFFFF, 0x18);
    assert_eq!(ARM7_IRQ_VECTOR, 0x18);
    assert_eq!(ARM9_IRQ_VECTOR, 0xFFFF_0000 + 0x18);
}

#[test]
fn reset_clears_everything_but_keeps_the_core_identity() {
    let mut c = armed(arm7());
    c.raise(sources::SPI);
    c.reset();
    assert_eq!(c.flags(), 0);
    assert_eq!(c.read32(reg::IE), Some(0));
    assert_eq!(c.read32(reg::IME), Some(0));
    assert_eq!(c.core(), Core::Arm7);
}

#[test]
fn the_controller_round_trips_through_a_save_state() {
    use savestate::{decode_state, encode_state};

    let mut c = armed(arm9());
    c.raise(sources::VCOUNT | sources::GEOMETRY_FIFO);
    let blob = encode_state("nds", 1, &c);

    let mut restored = arm9();
    decode_state("nds", 1, &blob, &mut restored).unwrap();
    assert_eq!(restored.flags(), sources::VCOUNT | sources::GEOMETRY_FIFO);
    assert_eq!(restored.read32(reg::IE), Some(sources::ARM9));
    assert!(restored.pending());
}
