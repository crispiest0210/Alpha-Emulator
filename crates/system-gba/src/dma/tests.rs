use super::*;

fn src(channel: usize) -> u32 {
    BASE + channel as u32 * 12
}
fn dst(channel: usize) -> u32 {
    src(channel) + 4
}
fn count(channel: usize) -> u32 {
    src(channel) + 8
}
fn ctrl(channel: usize) -> u32 {
    src(channel) + 10
}

/// Arm a channel with a source, destination, unit count, and control bits.
fn arm(dma: &mut DmaController, channel: usize, source: u32, dest: u32, words: u16, flags: u16) {
    dma.write32(src(channel), source);
    dma.write32(dst(channel), dest);
    dma.write16(count(channel), words);
    dma.write16(ctrl(channel), control::ENABLE | flags);
}

#[test]
fn an_immediate_transfer_is_ready_as_soon_as_it_is_enabled() {
    let mut dma = DmaController::new();
    arm(&mut dma, 0, 0x0200_0000, 0x0600_0000, 8, 0);

    let transfer = dma.take_transfer().expect("it is armed");
    assert_eq!(transfer.channel, 0);
    assert_eq!(transfer.source, 0x0200_0000);
    assert_eq!(transfer.destination, 0x0600_0000);
    assert_eq!(transfer.words, 8);
    assert_eq!(transfer.unit, 2, "halfwords unless the word bit is set");
    assert_eq!(dma.take_transfer(), None, "and it does not repeat");
}

#[test]
fn the_word_size_bit_selects_four_byte_units() {
    let mut dma = DmaController::new();
    arm(&mut dma, 0, 0, 0, 4, control::WORD_SIZE);
    assert_eq!(dma.take_transfer().unwrap().unit, 4);
}

#[test]
fn a_one_shot_transfer_clears_its_own_enable_bit() {
    // How a game polls for completion without using an interrupt.
    let mut dma = DmaController::new();
    arm(&mut dma, 0, 0, 0, 1, 0);
    dma.take_transfer();
    assert_eq!(
        dma.read16(ctrl(0)).unwrap() & control::ENABLE,
        0,
        "the enable bit cleared itself"
    );
}

#[test]
fn a_word_count_of_zero_means_the_channel_maximum_not_nothing() {
    // And the maximum differs per channel, which is not a detail a game can be assumed to know
    // about: setting the same count on channel 1 and channel 3 means two different things.
    let mut dma = DmaController::new();
    arm(&mut dma, 0, 0, 0, 0, 0);
    assert_eq!(dma.take_transfer().unwrap().words, 0x4000);

    arm(&mut dma, 3, 0, 0, 0, 0);
    assert_eq!(dma.take_transfer().unwrap().words, 0x1_0000);
}

#[test]
fn a_count_wider_than_the_channel_allows_is_truncated_to_its_field() {
    let mut dma = DmaController::new();
    arm(&mut dma, 1, 0, 0, 0x4001, 0);
    assert_eq!(
        dma.take_transfer().unwrap().words,
        1,
        "14 bits on channel 1"
    );
}

#[test]
fn a_vblank_transfer_waits_for_vertical_blanking() {
    let mut dma = DmaController::new();
    arm(&mut dma, 1, 0, 0, 4, 1 << 12);
    assert_eq!(dma.take_transfer(), None, "not yet");

    dma.on_vblank();
    assert!(dma.take_transfer().is_some());
}

#[test]
fn an_hblank_transfer_waits_for_horizontal_blanking() {
    let mut dma = DmaController::new();
    arm(&mut dma, 2, 0, 0, 4, 2 << 12);
    dma.on_vblank();
    assert_eq!(dma.take_transfer(), None, "the wrong trigger");

    dma.on_hblank();
    assert!(dma.take_transfer().is_some());
}

#[test]
fn priority_is_by_channel_number_and_is_absolute() {
    // Not a fair rotation: a lower-numbered channel wins every time, which is what a game
    // relies on when it puts audio on channel 1 and a screen effect on channel 3.
    let mut dma = DmaController::new();
    arm(&mut dma, 3, 0, 0, 1, 1 << 12);
    arm(&mut dma, 1, 0, 0, 1, 1 << 12);
    dma.on_vblank();

    assert_eq!(dma.take_transfer().unwrap().channel, 1);
    assert_eq!(dma.take_transfer().unwrap().channel, 3);
    assert_eq!(dma.take_transfer(), None);
}

#[test]
fn a_repeating_transfer_stays_enabled_and_rearms_on_each_trigger() {
    let mut dma = DmaController::new();
    arm(
        &mut dma,
        1,
        0x0200_0000,
        0x0600_0000,
        4,
        control::REPEAT | (1 << 12),
    );
    dma.on_vblank();
    assert!(dma.take_transfer().is_some());
    assert_ne!(dma.read16(ctrl(1)).unwrap() & control::ENABLE, 0);

    dma.on_vblank();
    assert!(dma.take_transfer().is_some(), "and it runs again");
}

#[test]
fn a_repeating_immediate_transfer_is_not_an_infinite_loop() {
    // There is no trigger to repeat *on*, so hardware treats the repeat bit as meaningless
    // here. Honouring it literally hangs the machine.
    let mut dma = DmaController::new();
    arm(&mut dma, 0, 0, 0, 1, control::REPEAT);
    assert!(dma.take_transfer().is_some());
    assert_eq!(dma.take_transfer(), None);
}

#[test]
fn the_running_addresses_advance_past_what_each_transfer_covered() {
    let mut dma = DmaController::new();
    arm(
        &mut dma,
        1,
        0x0200_0000,
        0x0600_0000,
        4,
        control::REPEAT | control::WORD_SIZE | (1 << 12),
    );
    dma.on_vblank();
    let first = dma.take_transfer().unwrap();
    dma.on_vblank();
    let second = dma.take_transfer().unwrap();

    assert_eq!(first.source, 0x0200_0000);
    assert_eq!(second.source, 0x0200_0010, "four words on from the first");
    assert_eq!(second.destination, 0x0600_0010);
}

#[test]
fn a_decrementing_source_walks_backwards() {
    let mut dma = DmaController::new();
    arm(
        &mut dma,
        1,
        0x0200_0100,
        0,
        4,
        control::REPEAT | (1 << 7) | (1 << 12),
    );
    dma.on_vblank();
    dma.take_transfer();
    dma.on_vblank();
    assert_eq!(dma.take_transfer().unwrap().source, 0x0200_00F8);
}

#[test]
fn a_fixed_address_does_not_move_between_repeats() {
    // How a sound FIFO transfer works: the destination is one register, forever.
    let mut dma = DmaController::new();
    arm(
        &mut dma,
        1,
        0x0200_0000,
        0x0400_00A0,
        4,
        control::REPEAT | (2 << 5) | (1 << 12),
    );
    dma.on_vblank();
    dma.take_transfer();
    dma.on_vblank();
    assert_eq!(dma.take_transfer().unwrap().destination, 0x0400_00A0);
}

#[test]
fn a_reloading_destination_snaps_back_so_the_next_repeat_refills_the_same_buffer() {
    let mut dma = DmaController::new();
    arm(
        &mut dma,
        1,
        0x0200_0000,
        0x0600_0000,
        4,
        control::REPEAT | (3 << 5) | (1 << 12),
    );
    dma.on_vblank();
    dma.take_transfer();
    dma.on_vblank();
    let second = dma.take_transfer().unwrap();
    assert_eq!(second.destination, 0x0600_0000, "back to the start");
    assert_eq!(second.source, 0x0200_0008, "but the source kept going");
}

#[test]
fn only_channels_one_and_two_serve_a_sound_fifo() {
    const FIFO_A: u32 = 0x0400_00A0;
    let mut dma = DmaController::new();
    // Channel 3 pointed at the FIFO with special timing: not a sound channel.
    arm(&mut dma, 3, 0x0200_0000, FIFO_A, 4, 3 << 12);
    // Channel 1 likewise, and this one is.
    arm(&mut dma, 1, 0x0200_0000, FIFO_A, 4, 3 << 12);

    dma.on_fifo_empty(FIFO_A);
    assert_eq!(dma.take_transfer().unwrap().channel, 1);
    assert_eq!(dma.take_transfer(), None, "channel 3 was not armed");
}

#[test]
fn a_fifo_refill_only_wakes_the_channel_feeding_that_fifo() {
    const FIFO_A: u32 = 0x0400_00A0;
    const FIFO_B: u32 = 0x0400_00A4;
    let mut dma = DmaController::new();
    arm(&mut dma, 1, 0, FIFO_A, 4, 3 << 12);
    arm(&mut dma, 2, 0, FIFO_B, 4, 3 << 12);

    dma.on_fifo_empty(FIFO_B);
    assert_eq!(dma.take_transfer().unwrap().channel, 2);
    assert_eq!(dma.take_transfer(), None);
}

#[test]
fn rewriting_control_on_a_running_transfer_does_not_relatch_its_addresses() {
    // A game adjusts the interrupt bit of a repeating transfer mid-flight; snapping the
    // addresses back would make it re-send data it has already sent.
    let mut dma = DmaController::new();
    arm(
        &mut dma,
        1,
        0x0200_0000,
        0x0600_0000,
        4,
        control::REPEAT | (1 << 12),
    );
    dma.on_vblank();
    dma.take_transfer();

    dma.write16(
        ctrl(1),
        control::ENABLE | control::REPEAT | control::IRQ | (1 << 12),
    );
    dma.on_vblank();
    assert_eq!(
        dma.take_transfer().unwrap().source,
        0x0200_0008,
        "it kept going rather than restarting"
    );
}

#[test]
fn clearing_the_enable_bit_disarms_a_waiting_transfer() {
    let mut dma = DmaController::new();
    arm(&mut dma, 1, 0, 0, 4, 1 << 12);
    dma.on_vblank();
    dma.write16(ctrl(1), 0);
    assert_eq!(dma.take_transfer(), None);
}

#[test]
fn the_interrupt_bit_travels_with_the_transfer() {
    let mut dma = DmaController::new();
    arm(&mut dma, 0, 0, 0, 1, 0);
    assert!(!dma.take_transfer().unwrap().raise_irq);

    arm(&mut dma, 0, 0, 0, 1, control::IRQ);
    assert!(dma.take_transfer().unwrap().raise_irq);
}

#[test]
fn the_address_registers_are_write_only() {
    let mut dma = DmaController::new();
    arm(&mut dma, 0, 0x1234_5678, 0x8765_4321, 9, 0);
    assert_eq!(dma.read32(src(0)), Some(0));
    assert_eq!(dma.read32(dst(0)), Some(0));
    assert_eq!(dma.read16(count(0)), Some(0));
}

#[test]
fn the_block_claims_four_channels_and_no_more() {
    assert!(DmaController::owns(BASE));
    assert!(DmaController::owns(BASE + 47));
    assert!(!DmaController::owns(BASE - 1));
    assert!(!DmaController::owns(BASE + 48));
    let dma = DmaController::new();
    assert_eq!(dma.read16(BASE + 48), None);
}

#[test]
fn dma_state_round_trips_between_repeats() {
    use savestate::{decode_state, encode_state};
    let mut dma = DmaController::new();
    arm(
        &mut dma,
        2,
        0x0200_0000,
        0x0600_0000,
        16,
        control::REPEAT | control::WORD_SIZE | (2 << 12),
    );
    dma.on_hblank();
    dma.take_transfer();

    let bytes = encode_state("gba-dma", 1, &dma);
    let mut restored = DmaController::new();
    decode_state("gba-dma", 1, &bytes, &mut restored).unwrap();
    assert_eq!(restored, dma);

    // And it resumes at the address it had reached, not at the one the game wrote.
    restored.on_hblank();
    dma.on_hblank();
    assert_eq!(restored.take_transfer(), dma.take_transfer());
}

#[test]
fn a_sound_fifo_transfer_ignores_the_channels_own_count_and_destination_step() {
    // Hardware fixes the shape of a FIFO transfer: four 32-bit words into a destination that does
    // not move, whatever the channel's own registers say. That is not pedantry — a game does not
    // bother writing settings the hardware overrides. Pokémon Emerald leaves DMA 1 with an
    // incrementing destination and a stale word count, and honouring them marched a refill out of
    // `FIFO_A` and straight up through the DMA control registers above it, thousands of times a
    // second, writing audio samples over the machine's own configuration.
    let mut dma = DmaController::new();
    dma.write16(BASE + 12 + 4, 0x00A0); // destination low: FIFO_A
    dma.write16(BASE + 12 + 6, 0x0400); // destination high
    dma.write16(BASE + 12, 0x0000);
    dma.write16(BASE + 12 + 2, 0x0200); // source in EWRAM
    dma.write16(BASE + 12 + 8, 0x2000); // a word count that must be ignored
                                        // Enable, repeat, 32-bit, Special timing, destination step = increment.
    dma.write16(BASE + 12 + 10, 0x8000 | (1 << 9) | (1 << 10) | (3 << 12));

    dma.on_fifo_empty(0x0400_00A0);
    let transfer = dma.take_transfer().expect("armed");
    assert_eq!(transfer.words, 4, "always four words");
    assert_eq!(transfer.unit, 4, "always 32-bit");
    assert_eq!(transfer.destination, 0x0400_00A0);
    assert_eq!(transfer.destination_step, AddressStep::Fixed);

    // And the running destination must not have moved either, or the next refill lands on the
    // DMA registers rather than the FIFO.
    dma.on_fifo_empty(0x0400_00A0);
    let second = dma.take_transfer().expect("armed again");
    assert_eq!(second.destination, 0x0400_00A0, "still the FIFO");
    assert_eq!(
        second.source,
        transfer.source + 16,
        "but the source walks forward through the sample buffer"
    );
}

#[test]
fn an_ordinary_transfer_still_honours_its_own_settings() {
    // The override above is keyed to channels 1 and 2 with Special timing. Everything else must
    // keep reading its registers, including channel 3's Special, which is video capture.
    let mut dma = DmaController::new();
    dma.write16(BASE + 12 + 4, 0x0000);
    dma.write16(BASE + 12 + 6, 0x0300);
    dma.write16(BASE + 12 + 8, 0x0010);
    dma.write16(BASE + 12 + 10, 0x8000 | (1 << 10)); // enable, 32-bit, immediate

    let transfer = dma
        .take_transfer()
        .expect("immediate transfers arm on enable");
    assert_eq!(transfer.words, 0x10, "its own count");
    assert_eq!(transfer.destination_step, AddressStep::Increment);
}

#[test]
fn channel_zero_masks_its_addresses_to_twenty_seven_bits() {
    // Channel 0 cannot reach the cartridge at all, and 27 bits is exactly the window that
    // excludes it — bit 27 (0x0800_0000) is the first bit above that window.
    let mut dma = DmaController::new();
    arm(
        &mut dma,
        0,
        0x0800_0000 | 0x0200_0000,
        0x0800_0000 | 0x0600_0000,
        1,
        0,
    );
    let transfer = dma.take_transfer().expect("it is armed");
    assert_eq!(
        transfer.source, 0x0200_0000,
        "the 28th bit is not on channel 0's bus"
    );
    assert_eq!(transfer.destination, 0x0600_0000);
}

#[test]
fn channels_one_through_three_mask_their_addresses_to_twenty_eight_bits() {
    // Bit 28 (0x1000_0000) is the first bit above every other channel's 28-bit window.
    let mut dma = DmaController::new();
    arm(
        &mut dma,
        1,
        0x1000_0000 | 0x0200_0000,
        0x1000_0000 | 0x0600_0000,
        1,
        0,
    );
    let transfer = dma.take_transfer().expect("it is armed");
    assert_eq!(transfer.source, 0x0200_0000);
    assert_eq!(transfer.destination, 0x0600_0000);
}

#[test]
fn a_repeating_transfer_wraps_its_running_address_within_the_channel_window_rather_than_past_it() {
    // The mask is not only applied once at latch time: each step re-applies it, or an address
    // that increments up to the top of the window would carry into the bit the mask exists to
    // exclude instead of wrapping back to the start of it.
    let mut dma = DmaController::new();
    // Source one word below channel 0's 27-bit ceiling, incrementing by four bytes (the word
    // bit), repeating on HBlank (bits 12-13 = 2) so a second transfer needs only another
    // `on_hblank`, not a fresh register write that would re-latch the address from scratch.
    let flags = control::WORD_SIZE | control::REPEAT | (2 << 12);
    arm(&mut dma, 0, 0x07FF_FFFC, 0, 1, flags);

    dma.on_hblank();
    let first = dma.take_transfer().unwrap();
    assert_eq!(first.source, 0x07FF_FFFC);

    dma.on_hblank();
    let second = dma.take_transfer().unwrap();
    assert_eq!(
        second.source, 0,
        "stepping past the 27-bit ceiling wraps to the start of the window"
    );
}
