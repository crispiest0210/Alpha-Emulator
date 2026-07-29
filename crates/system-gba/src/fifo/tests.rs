use super::*;

/// Both channels on both sides at full volume, both paced by timer 0.
///
/// Spelled out bit by bit rather than as a block, because the enables are not contiguous: bits
/// 10 and 14 are the timer selects and bits 11 and 15 are the reset strobes, so a mask that
/// looks like "all the channel A bits" silently reroutes and resets it.
const BOTH_CHANNELS_FULL: u16 = (1 << 2) // A at full volume
    | (1 << 3) // B at full volume
    | (1 << 8) | (1 << 9) // A right and left
    | (1 << 12) | (1 << 13); // B right and left

fn enabled_sound() -> DirectSound {
    let mut sound = DirectSound::new();
    sound.write16(reg::SOUNDCNT_X, 1 << 7);
    sound.write16(reg::SOUNDCNT_H, BOTH_CHANNELS_FULL);
    sound
}

#[test]
fn a_fresh_queue_is_empty_and_already_asking_for_data() {
    let fifo = SoundFifo::new();
    assert!(fifo.is_empty());
    assert!(fifo.needs_refill(), "so the first DMA burst is requested");
}

#[test]
fn a_word_write_pushes_four_samples_low_byte_first() {
    let mut fifo = SoundFifo::new();
    fifo.push_word(0x0403_0201);
    assert_eq!(fifo.len(), 4);
    assert_eq!(fifo.pop_sample(), 1, "the low byte is the earliest sample");
    assert_eq!(fifo.pop_sample(), 2);
    assert_eq!(fifo.pop_sample(), 3);
    assert_eq!(fifo.pop_sample(), 4);
}

#[test]
fn samples_are_signed() {
    let mut fifo = SoundFifo::new();
    fifo.push_word(0x0000_0080);
    assert_eq!(fifo.pop_sample(), -128, "not 128");
}

#[test]
fn the_queue_holds_thirty_two_bytes_and_drops_what_will_not_fit() {
    // Dropping the *newest* is right here, unlike the input channel in frontend-core: these
    // bytes are a sequence, so discarding an old one skips forward in the audio rather than
    // merely delaying it.
    let mut fifo = SoundFifo::new();
    for word in 0..10u32 {
        fifo.push_word(word * 0x0101_0101 + 0x0101_0101);
    }
    assert_eq!(fifo.len(), CAPACITY);
    assert_eq!(fifo.pop_sample(), 1, "the oldest survived");
}

#[test]
fn a_refill_is_requested_once_the_queue_falls_to_half() {
    let mut fifo = SoundFifo::new();
    for _ in 0..8 {
        fifo.push_word(0);
    }
    assert_eq!(fifo.len(), CAPACITY);
    assert!(!fifo.needs_refill());

    for _ in 0..(CAPACITY - REFILL_THRESHOLD) {
        fifo.pop_sample();
    }
    assert!(fifo.needs_refill());
}

#[test]
fn an_empty_queue_holds_its_last_sample_rather_than_going_silent() {
    // What hardware does, and why an underrun sounds like a click or a buzz rather than a gap.
    // Returning zero would make a starved channel *quieter* than a working one, which is the
    // opposite of the symptom to listen for.
    let mut fifo = SoundFifo::new();
    fifo.push(127);
    assert_eq!(fifo.pop_sample(), 127);
    assert!(fifo.is_empty());
    assert_eq!(fifo.pop_sample(), 127, "held, not dropped to zero");
    assert_eq!(fifo.current_sample(), 127);
}

#[test]
fn the_queue_wraps_rather_than_running_off_the_end() {
    let mut fifo = SoundFifo::new();
    for round in 0..20 {
        fifo.push(round as i8);
        assert_eq!(fifo.pop_sample(), round as i8, "round {round}");
    }
}

#[test]
fn resetting_clears_the_held_sample_too() {
    // A reset is a game changing what it is playing; carrying the last byte of the previous
    // sound across is an audible pop.
    let mut fifo = SoundFifo::new();
    fifo.push_word(0x7F7F_7F7F);
    fifo.pop_sample();
    fifo.reset();
    assert!(fifo.is_empty());
    assert_eq!(fifo.current_sample(), 0);
}

#[test]
fn the_reset_bits_are_strobes_and_do_not_read_back() {
    let mut sound = enabled_sound();
    sound.write32(reg::FIFO_A, 0x7F7F_7F7F);
    sound.write32(reg::FIFO_B, 0x7F7F_7F7F);
    assert_eq!(sound.a.len(), 4);

    sound.write16(reg::SOUNDCNT_H, BOTH_CHANNELS_FULL | (1 << 11));
    assert!(sound.a.is_empty(), "channel A was reset");
    assert_eq!(sound.b.len(), 4, "and channel B was not");
    assert_eq!(
        sound.read16(reg::SOUNDCNT_H).unwrap() & (1 << 11),
        0,
        "the bit does not stay set"
    );
}

#[test]
fn each_channel_selects_its_own_timer() {
    // How a game plays 16 kHz on one channel and 32 kHz on the other.
    let mut sound = enabled_sound();
    // Channel A on timer 0, channel B on timer 1.
    sound.write16(reg::SOUNDCNT_H, BOTH_CHANNELS_FULL | (1 << 14));
    assert_eq!(sound.control.timer(false), 0);
    assert_eq!(sound.control.timer(true), 1);

    sound.write32(reg::FIFO_A, 0x0000_0011);
    sound.write32(reg::FIFO_B, 0x0000_0022);

    sound.on_timer_overflow(1 << 0);
    assert_eq!(sound.a.current_sample(), 0x11);
    assert_eq!(sound.b.current_sample(), 0, "timer 1 has not fired");

    sound.on_timer_overflow(1 << 1);
    assert_eq!(sound.b.current_sample(), 0x22);
}

#[test]
fn both_channels_can_share_one_timer() {
    let mut sound = enabled_sound();
    sound.write32(reg::FIFO_A, 0x0000_0011);
    sound.write32(reg::FIFO_B, 0x0000_0022);
    sound.on_timer_overflow(1 << 0);
    assert_eq!(sound.a.current_sample(), 0x11);
    assert_eq!(sound.b.current_sample(), 0x22);
}

#[test]
fn a_refill_request_names_the_fifo_address_the_dma_channel_matches_on() {
    // A DMA channel is bound to a FIFO by destination address, not by an index, so the request
    // has to be an address.
    let sound = DirectSound::new();
    let requests: Vec<u32> = sound.refill_requests().collect();
    assert_eq!(requests, vec![reg::FIFO_A, reg::FIFO_B]);

    let mut sound = DirectSound::new();
    for _ in 0..8 {
        sound.write32(reg::FIFO_A, 0);
    }
    let requests: Vec<u32> = sound.refill_requests().collect();
    assert_eq!(requests, vec![reg::FIFO_B], "A is full");
}

#[test]
fn the_master_switch_silences_everything() {
    let mut sound = enabled_sound();
    sound.write32(reg::FIFO_A, 0x7F7F_7F7F);
    sound.on_timer_overflow(1);
    assert_ne!(sound.output().0, 0.0);

    sound.write16(reg::SOUNDCNT_X, 0);
    assert_eq!(sound.output(), (0.0, 0.0));
}

#[test]
fn a_channel_not_enabled_on_a_side_contributes_nothing_there() {
    // How a game pans a channel.
    let mut sound = DirectSound::new();
    sound.write16(reg::SOUNDCNT_X, 1 << 7);
    // Channel A: full volume, left only.
    sound.write16(reg::SOUNDCNT_H, (1 << 2) | (1 << 9));
    sound.write32(reg::FIFO_A, 0x0000_007F);
    sound.on_timer_overflow(1);

    let (left, right) = sound.output();
    assert!(left > 0.9, "left got the sample");
    assert_eq!(right, 0.0, "and the right side got nothing");
}

#[test]
fn the_half_volume_setting_halves_the_output() {
    let mut quiet = DirectSound::new();
    quiet.write16(reg::SOUNDCNT_X, 1 << 7);
    quiet.write16(reg::SOUNDCNT_H, 1 << 9); // left, half volume
    quiet.write32(reg::FIFO_A, 0x0000_007F);
    quiet.on_timer_overflow(1);

    let mut loud = DirectSound::new();
    loud.write16(reg::SOUNDCNT_X, 1 << 7);
    loud.write16(reg::SOUNDCNT_H, (1 << 2) | (1 << 9));
    loud.write32(reg::FIFO_A, 0x0000_007F);
    loud.on_timer_overflow(1);

    assert!((loud.output().0 - quiet.output().0 * 2.0).abs() < 1e-6);
}

#[test]
fn the_prohibited_psg_volume_setting_is_silent_rather_than_loud() {
    // A game landing there by accident should not get a burst of noise.
    let mut sound = DirectSound::new();
    sound.write16(reg::SOUNDCNT_H, 3);
    assert_eq!(sound.control.psg_volume(), 0);
    sound.write16(reg::SOUNDCNT_H, 2);
    assert_eq!(sound.control.psg_volume(), 4);
}

#[test]
fn the_queues_are_write_only() {
    let mut sound = enabled_sound();
    sound.write32(reg::FIFO_A, 0x1234_5678);
    assert_eq!(
        sound.read16(reg::FIFO_A),
        Some(0),
        "a game cannot inspect how much audio is left"
    );
}

#[test]
fn the_block_claims_its_registers_and_no_others() {
    assert!(DirectSound::owns(reg::SOUNDCNT_H));
    assert!(DirectSound::owns(reg::SOUNDCNT_X));
    assert!(DirectSound::owns(reg::FIFO_A));
    assert!(DirectSound::owns(reg::FIFO_B + 3));
    assert!(!DirectSound::owns(0x0400_0060), "that is the PSG block");
    assert!(!DirectSound::owns(0x0400_00B0), "that is DMA");
}

#[test]
fn direct_sound_state_round_trips_mid_playback() {
    use savestate::{decode_state, encode_state};
    let mut sound = enabled_sound();
    sound.write32(reg::FIFO_A, 0x0403_0201);
    sound.write32(reg::FIFO_B, 0x0807_0605);
    sound.on_timer_overflow(1);

    let bytes = encode_state("gba-fifo", 1, &sound);
    let mut restored = DirectSound::new();
    decode_state("gba-fifo", 1, &bytes, &mut restored).unwrap();
    assert_eq!(restored, sound);

    // And the next sample is the one that was actually next, not the start of the queue.
    restored.on_timer_overflow(1);
    sound.on_timer_overflow(1);
    assert_eq!(restored.a.current_sample(), sound.a.current_sample());
    assert_eq!(restored.a.current_sample(), 2);
}

#[test]
fn a_corrupt_length_in_a_save_state_cannot_index_past_the_queue() {
    // A save state is not a trusted input; an unclamped length would index off the end on the
    // very next pop.
    use savestate::{decode_state, encode_state};
    let sound = DirectSound::new();
    let mut bytes = encode_state("gba-fifo", 1, &sound);
    // The length field of channel A: 32 queue bytes, then read and write, then len.
    let header = bytes.len() - (2 * (CAPACITY + 13) + 4);
    let offset = header + CAPACITY + 8;
    bytes[offset..offset + 4].copy_from_slice(&u32::MAX.to_le_bytes());

    let mut restored = DirectSound::new();
    if decode_state("gba-fifo", 1, &bytes, &mut restored).is_ok() {
        assert!(restored.a.len() <= CAPACITY);
        restored.a.pop_sample();
    }
}
