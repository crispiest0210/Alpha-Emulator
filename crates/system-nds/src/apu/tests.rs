use super::*;
use crate::memory::NdsMemory;

/// Channel register addresses.
fn cnt(ch: u32) -> u32 {
    BASE + ch * 16
}
fn sad(ch: u32) -> u32 {
    BASE + ch * 16 + 4
}
fn tmr(ch: u32) -> u32 {
    BASE + ch * 16 + 8
}
fn len(ch: u32) -> u32 {
    BASE + ch * 16 + 12
}

const BUSY: u32 = 1 << 31;
const CENTRE: u32 = 64 << 16;
const FULL: u32 = 0x7F;

/// A timer value giving roughly `rate` samples a second.
fn timer_for(rate: u32) -> u16 {
    (0x1_0000 - (CLOCK / (2 * rate))) as u16
}

/// An APU with the master switch on at full volume.
fn apu() -> NdsApu {
    let mut apu = NdsApu::new();
    apu.write32_reg(reg::SOUNDCNT, (1 << 15) | 0x7F);
    apu
}

/// Memory with `bytes` written at `0x0200_0000`.
fn memory_with(bytes: &[u8]) -> NdsMemory {
    let mut memory = NdsMemory::default();
    for (i, byte) in bytes.iter().enumerate() {
        memory.write8_arm7(0x0200_0000 + i as u32, *byte);
    }
    memory
}

/// Run long enough to produce `count` output samples.
fn run(apu: &mut NdsApu, memory: &NdsMemory, count: usize) -> Vec<AudioSample> {
    let per_sample = CLOCK / AUDIO_SAMPLE_RATE;
    apu.step(per_sample * count as u32, memory);
    apu.take_samples().to_vec()
}

#[test]
fn the_block_owns_its_registers_and_nothing_else() {
    assert!(NdsApu::owns(BASE));
    assert!(NdsApu::owns(BASE + 16 * 16 - 1));
    // The channel block runs right up to SOUNDCNT, so the first address past everything is the
    // one after SOUNDBIAS.
    assert_eq!(BASE + 16 * 16, reg::SOUNDCNT);
    assert!(!NdsApu::owns(BASE - 4));
    assert!(NdsApu::owns(reg::SOUNDCNT));
    assert!(NdsApu::owns(reg::SOUNDBIAS));
    assert!(!NdsApu::owns(0x0400_0508));
}

#[test]
fn nothing_is_produced_until_the_master_switch_is_on() {
    let memory = memory_with(&[0x40; 64]);
    let mut apu = NdsApu::new();
    apu.write32_reg(sad(0), 0x0200_0000);
    apu.write32_reg(tmr(0), timer_for(16_000) as u32);
    apu.write32_reg(len(0), 16);
    apu.write32_reg(cnt(0), BUSY | CENTRE | FULL);

    let samples = run(&mut apu, &memory, 16);
    assert!(samples.iter().all(|s| *s == AudioSample::SILENCE));

    apu.write32_reg(reg::SOUNDCNT, (1 << 15) | 0x7F);
    let samples = run(&mut apu, &memory, 16);
    assert!(samples.iter().any(|s| s.left != 0.0));
}

#[test]
fn a_pcm8_channel_plays_the_bytes_it_was_pointed_at() {
    // A square wave in 8-bit samples: four loud, four quiet.
    let data: Vec<u8> = (0..32)
        .map(|i| if i % 8 < 4 { 0x60 } else { 0x00 })
        .collect();
    let memory = memory_with(&data);
    let mut apu = apu();
    apu.write32_reg(sad(0), 0x0200_0000);
    apu.write32_reg(tmr(0), timer_for(AUDIO_SAMPLE_RATE) as u32);
    apu.write32_reg(len(0), 8);
    apu.write32_reg(cnt(0), BUSY | CENTRE | FULL);

    let samples = run(&mut apu, &memory, 8);
    assert!(samples[0].left > 0.0, "the loud half: {:?}", samples[0]);
    assert!(samples.iter().any(|s| s.left.abs() < 0.01), "and the quiet");
    // Centred panning puts the same signal in both ears.
    for sample in &samples {
        assert!((sample.left - sample.right).abs() < 1e-6);
    }
}

#[test]
fn a_pcm16_channel_reads_two_bytes_per_sample() {
    let mut data = Vec::new();
    for i in 0..16i16 {
        data.extend_from_slice(&(i * 2000).to_le_bytes());
    }
    let memory = memory_with(&data);
    let mut apu = apu();
    apu.write32_reg(sad(0), 0x0200_0000);
    apu.write32_reg(tmr(0), timer_for(AUDIO_SAMPLE_RATE) as u32);
    apu.write32_reg(len(0), 8);
    // Format 1, hard left.
    apu.write32_reg(cnt(0), BUSY | (1 << 29) | FULL);

    let samples = run(&mut apu, &memory, 8);
    // Rising ramp: each sample is louder than the last.
    for pair in samples.windows(2) {
        assert!(pair[1].left >= pair[0].left, "{pair:?}");
    }
    assert!(samples.iter().all(|s| s.right.abs() < 1e-6), "hard left");
}

#[test]
fn panning_crossfades_rather_than_setting_two_volumes() {
    let memory = memory_with(&[0x7F; 64]);
    let mut apu = apu();
    apu.write32_reg(sad(0), 0x0200_0000);
    apu.write32_reg(tmr(0), timer_for(AUDIO_SAMPLE_RATE) as u32);
    apu.write32_reg(len(0), 16);

    apu.write32_reg(cnt(0), BUSY | FULL); // panning 0: hard left
    let left_only = run(&mut apu, &memory, 4);
    assert!(left_only[0].left > 0.0 && left_only[0].right.abs() < 1e-6);

    apu.write32_reg(cnt(0), 0);
    apu.write32_reg(cnt(0), BUSY | (127 << 16) | FULL);
    let right_only = run(&mut apu, &memory, 4);
    // 127 of 128 is not *quite* silent on the left, which is what hardware's denominator gives.
    assert!(right_only[0].right > 0.0);
    assert!(right_only[0].left < right_only[0].right / 100.0);
}

#[test]
fn the_volume_divider_of_three_is_a_sixteenth_not_an_eighth() {
    // The field is not a plain power of two. Reading it as one makes every quiet channel four
    // times too loud, which is audible and easy to attribute to the wrong thing.
    let memory = memory_with(&[0x7F; 64]);
    let mut apu = apu();
    apu.write32_reg(sad(0), 0x0200_0000);
    apu.write32_reg(tmr(0), timer_for(AUDIO_SAMPLE_RATE) as u32);
    apu.write32_reg(len(0), 16);

    let mut loudness = Vec::new();
    for divider in 0..4u32 {
        apu.write32_reg(cnt(0), 0);
        apu.write32_reg(cnt(0), BUSY | CENTRE | (divider << 8) | FULL);
        loudness.push(run(&mut apu, &memory, 4)[0].left);
    }
    assert!((loudness[0] / loudness[1] - 2.0).abs() < 0.01);
    assert!((loudness[0] / loudness[2] - 4.0).abs() < 0.01);
    assert!((loudness[0] / loudness[3] - 16.0).abs() < 0.01, "not 8");
}

#[test]
fn a_one_shot_channel_clears_its_own_busy_bit() {
    // That is how software sees a sound finish, so it has to be observable.
    let memory = memory_with(&[0x40; 64]);
    let mut apu = apu();
    apu.write32_reg(sad(0), 0x0200_0000);
    apu.write32_reg(tmr(0), timer_for(AUDIO_SAMPLE_RATE) as u32);
    apu.write32_reg(len(0), 2); // eight PCM8 samples
    apu.write32_reg(cnt(0), BUSY | CENTRE | (2 << 27) | FULL);
    assert!(apu.channel_is_busy(0));

    run(&mut apu, &memory, 16);
    assert!(!apu.channel_is_busy(0));
    assert_eq!(apu.read32_reg(cnt(0)).unwrap() & BUSY, 0);
}

#[test]
fn a_looping_channel_keeps_playing_and_stays_busy() {
    let data: Vec<u8> = (0..8).map(|i| if i < 4 { 0x60 } else { 0x00 }).collect();
    let memory = memory_with(&data);
    let mut apu = apu();
    apu.write32_reg(sad(0), 0x0200_0000);
    apu.write32_reg(tmr(0), timer_for(AUDIO_SAMPLE_RATE) as u32);
    apu.write32_reg(len(0), 2);
    apu.write32_reg(cnt(0), BUSY | CENTRE | (1 << 27) | FULL);

    let samples = run(&mut apu, &memory, 64);
    assert!(apu.channel_is_busy(0), "still going after eight passes");
    // The loud half comes round again rather than the channel going quiet for good.
    assert!(samples[40..].iter().any(|s| s.left > 0.0));
}

#[test]
fn a_channel_restarts_only_on_the_rising_edge_of_the_busy_bit() {
    let memory = memory_with(&[0x60; 64]);
    let mut apu = apu();
    apu.write32_reg(sad(0), 0x0200_0000);
    apu.write32_reg(tmr(0), timer_for(AUDIO_SAMPLE_RATE) as u32);
    apu.write32_reg(len(0), 16);
    apu.write32_reg(cnt(0), BUSY | CENTRE | FULL);
    run(&mut apu, &memory, 8);

    // Adjusting the volume of a running channel must not send it back to the start.
    let before = apu.channels[0].position;
    apu.write32_reg(cnt(0), BUSY | CENTRE | 0x40);
    assert_eq!(apu.channels[0].position, before);

    apu.write32_reg(cnt(0), 0);
    apu.write32_reg(cnt(0), BUSY | CENTRE | FULL);
    assert_eq!(apu.channels[0].position, 0, "off and on does restart it");
}

#[test]
fn the_channel_rate_comes_from_the_timer_and_a_faster_one_consumes_more_data() {
    let mut apu = NdsApu::new();
    apu.write32_reg(tmr(0), timer_for(32_768) as u32);
    let slow = apu.channels[0].rate();
    apu.write32_reg(tmr(0), timer_for(65_536) as u32);
    let fast = apu.channels[0].rate();
    assert!(fast > slow * 3 / 2, "{slow} then {fast}");

    // And it is the divisor form, not a plain count: a timer of zero is the slowest rate.
    apu.write32_reg(tmr(0), 0);
    assert_eq!(apu.channels[0].rate(), CLOCK / (2 * 0x1_0000));
}

#[test]
fn adpcm_takes_its_header_before_its_first_nibble() {
    // Header: initial value 0x1000, index 0. Then nibbles that all step upward.
    let mut data = Vec::new();
    data.extend_from_slice(&0x0000_1000u32.to_le_bytes());
    data.extend_from_slice(&[0x44; 32]); // nibble 4 each time: a clear positive step
    let memory = memory_with(&data);

    let mut apu = apu();
    apu.write32_reg(sad(0), 0x0200_0000);
    apu.write32_reg(tmr(0), timer_for(AUDIO_SAMPLE_RATE) as u32);
    apu.write32_reg(len(0), 8);
    apu.write32_reg(cnt(0), BUSY | CENTRE | (2 << 29) | FULL);

    run(&mut apu, &memory, 4);
    assert!(
        apu.channels[0].adpcm_value > 0x1000,
        "started from the header's value, not from zero: {:#X}",
        apu.channels[0].adpcm_value
    );
    assert!(
        apu.channels[0].adpcm_index > 0,
        "and the index moved with it"
    );
    // Four nibbles is two bytes past the four-byte header.
    assert_eq!(apu.channels[0].position, 6);
}

#[test]
fn adpcm_reconstructs_a_known_sequence() {
    // Hand-worked against the IMA reference: from value 0 index 0 (step 7), nibble 4 adds
    // 7/8 + 7 = 7 (integer division) and moves the index to 4.
    let mut data = Vec::new();
    data.extend_from_slice(&0u32.to_le_bytes());
    data.push(0x04);
    let memory = memory_with(&data);

    let mut apu = apu();
    apu.write32_reg(sad(0), 0x0200_0000);
    apu.write32_reg(tmr(0), timer_for(AUDIO_SAMPLE_RATE) as u32);
    apu.write32_reg(len(0), 8);
    apu.write32_reg(cnt(0), BUSY | CENTRE | (2 << 29) | FULL);

    run(&mut apu, &memory, 1);
    // step/8 rounds to zero, plus the whole step for nibble bit 2.
    assert_eq!(apu.channels[0].adpcm_value, 7);
    assert_eq!(
        apu.channels[0].adpcm_index, 2,
        "the index table, not the nibble"
    );
}

#[test]
fn adpcm_consumes_two_nibbles_per_byte() {
    let mut data = Vec::new();
    data.extend_from_slice(&0u32.to_le_bytes());
    data.extend_from_slice(&[0x88; 16]);
    let memory = memory_with(&data);

    let mut apu = apu();
    apu.write32_reg(sad(0), 0x0200_0000);
    apu.write32_reg(tmr(0), timer_for(AUDIO_SAMPLE_RATE) as u32);
    apu.write32_reg(len(0), 8);
    apu.write32_reg(cnt(0), BUSY | CENTRE | (2 << 29) | FULL);

    run(&mut apu, &memory, 4);
    // Four samples out of two bytes, so the position advanced by two from the header's word.
    assert_eq!(apu.channels[0].position, 4 + 2);
}

#[test]
fn a_psg_channel_produces_a_square_wave_on_the_channels_that_have_one() {
    let memory = NdsMemory::default();
    let mut apu = apu();
    // Channel 8 with a 50% duty cycle, format 3.
    apu.write32_reg(tmr(8), timer_for(AUDIO_SAMPLE_RATE) as u32);
    apu.write32_reg(cnt(8), BUSY | CENTRE | (3 << 29) | (3 << 24) | FULL);

    let samples = run(&mut apu, &memory, 16);
    assert!(samples.iter().any(|s| s.left > 0.0));
    assert!(samples.iter().any(|s| s.left < 0.0));
}

#[test]
fn a_psg_channel_below_eight_is_silent_rather_than_a_square_wave() {
    // Only channels 8-13 have a square wave and only 14-15 have noise. A game that sets format 3
    // on channel 0 gets nothing, which is what hardware does.
    let memory = NdsMemory::default();
    let mut apu = apu();
    apu.write32_reg(tmr(0), timer_for(AUDIO_SAMPLE_RATE) as u32);
    apu.write32_reg(cnt(0), BUSY | CENTRE | (3 << 29) | (3 << 24) | FULL);
    let samples = run(&mut apu, &memory, 16);
    assert!(samples.iter().all(|s| s.left.abs() < 1e-6));
}

#[test]
fn the_noise_channels_do_not_repeat_within_a_short_run() {
    let memory = NdsMemory::default();
    let mut apu = apu();
    apu.write32_reg(tmr(14), timer_for(AUDIO_SAMPLE_RATE) as u32);
    apu.write32_reg(cnt(14), BUSY | CENTRE | (3 << 29) | FULL);

    let samples = run(&mut apu, &memory, 64);
    let highs = samples.iter().filter(|s| s.left > 0.0).count();
    assert!(highs > 8 && highs < 56, "not noise: {highs} of 64 high");
}

#[test]
fn sixteen_channels_at_once_do_not_clip_the_mix() {
    let memory = memory_with(&[0x7F; 256]);
    let mut apu = apu();
    for ch in 0..16u32 {
        apu.write32_reg(sad(ch), 0x0200_0000);
        apu.write32_reg(tmr(ch), timer_for(AUDIO_SAMPLE_RATE) as u32);
        apu.write32_reg(len(ch), 32);
        apu.write32_reg(cnt(ch), BUSY | CENTRE | FULL);
    }
    let samples = run(&mut apu, &memory, 32);
    for sample in &samples {
        assert!(
            sample.left.abs() <= 1.0 && sample.right.abs() <= 1.0,
            "{sample:?}"
        );
    }
    assert!(samples.iter().any(|s| s.left > 0.5), "and it is loud");
}

#[test]
fn the_master_volume_scales_everything() {
    let memory = memory_with(&[0x7F; 64]);
    let mut apu = apu();
    apu.write32_reg(sad(0), 0x0200_0000);
    apu.write32_reg(tmr(0), timer_for(AUDIO_SAMPLE_RATE) as u32);
    apu.write32_reg(len(0), 16);
    apu.write32_reg(cnt(0), BUSY | CENTRE | FULL);
    let loud = run(&mut apu, &memory, 4)[0].left;

    apu.write32_reg(reg::SOUNDCNT, (1 << 15) | 0x3F);
    let quiet = run(&mut apu, &memory, 4)[0].left;
    assert!(quiet < loud * 0.6, "{quiet} against {loud}");
}

#[test]
fn samples_come_out_at_the_frontends_rate() {
    let memory = memory_with(&[0x40; 256]);
    let mut apu = apu();
    apu.write32_reg(sad(0), 0x0200_0000);
    apu.write32_reg(tmr(0), timer_for(32_768) as u32);
    apu.write32_reg(len(0), 64);
    apu.write32_reg(cnt(0), BUSY | CENTRE | (1 << 27) | FULL);

    // One second of master cycles should be one second of samples, within rounding.
    apu.step(CLOCK, &memory);
    let count = apu.take_samples().len();
    let expected = AUDIO_SAMPLE_RATE as usize;
    assert!(
        count.abs_diff(expected) < expected / 100,
        "{count} samples for a second at {expected} Hz"
    );
}

#[test]
fn taking_samples_drains_them() {
    let memory = memory_with(&[0x40; 64]);
    let mut apu = apu();
    apu.write32_reg(sad(0), 0x0200_0000);
    apu.write32_reg(tmr(0), timer_for(AUDIO_SAMPLE_RATE) as u32);
    apu.write32_reg(len(0), 16);
    apu.write32_reg(cnt(0), BUSY | CENTRE | FULL);

    assert!(!run(&mut apu, &memory, 8).is_empty());
    assert!(apu.take_samples().is_empty(), "each sample exactly once");
}

#[test]
fn write_only_channel_registers_read_as_zero_but_splice_correctly() {
    let mut apu = NdsApu::new();
    apu.write32_reg(sad(0), 0x0203_0000);
    assert_eq!(apu.read32_reg(sad(0)), Some(0));
    // A halfword write must splice into what was *written*, not into the zero that reads back.
    apu.write16_reg(sad(0), 0x1234);
    assert_eq!(apu.channels[0].source, 0x0203_1234);

    apu.write32_reg(tmr(0), 0xABCD_1234);
    apu.write16_reg(tmr(0) + 2, 0x5555);
    assert_eq!(apu.channels[0].timer, 0x1234);
    assert_eq!(apu.channels[0].loop_start, 0x5555);
}

#[test]
fn the_control_register_reads_back_and_narrow_writes_reach_it() {
    let mut apu = NdsApu::new();
    apu.write32_reg(cnt(3), 0x1234_5678);
    assert_eq!(apu.read32_reg(cnt(3)), Some(0x1234_5678));
    assert_eq!(apu.read16_reg(cnt(3)), Some(0x5678));
    assert_eq!(apu.read8_reg(cnt(3) + 3), Some(0x12));

    apu.write8_reg(cnt(3) + 3, 0x80);
    assert!(
        apu.channel_is_busy(3),
        "the busy bit is reachable a byte at a time"
    );
    assert_eq!(apu.read32_reg(0x0400_0508), None);
}

#[test]
fn the_apu_round_trips_through_a_save_state_mid_sample() {
    use savestate::{decode_state, encode_state};

    let mut data = Vec::new();
    data.extend_from_slice(&0x0000_2000u32.to_le_bytes());
    data.extend_from_slice(&[0x37; 64]);
    let memory = memory_with(&data);

    let mut apu = apu();
    apu.write32_reg(sad(0), 0x0200_0000);
    apu.write32_reg(tmr(0), timer_for(22_050) as u32);
    apu.write32_reg(len(0), 16);
    apu.write32_reg(cnt(0), BUSY | CENTRE | (2 << 29) | (1 << 27) | FULL);
    run(&mut apu, &memory, 37);

    let blob = encode_state("nds", 1, &apu);
    let mut restored = NdsApu::new();
    decode_state("nds", 1, &blob, &mut restored).unwrap();

    assert_eq!(restored.channels[0], apu.channels[0]);
    assert!(
        restored.take_samples().is_empty(),
        "the queue is not restored"
    );

    // And both carry on identically from here.
    let a = run(&mut apu, &memory, 16);
    let b = run(&mut restored, &memory, 16);
    assert_eq!(a, b);
}
