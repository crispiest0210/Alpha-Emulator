use super::*;

use crate::fifo::DirectSound;

/// A PSG with the master enable already set, which is what a game does first.
fn psg() -> Psg {
    let mut psg = Psg::new();
    psg.set_power(true);
    psg
}

/// Full master volume on both sides with every channel routed to both.
///
/// Low byte is `NR50` — bits 0-2 right volume, bits 4-6 left, and bits 3 and 7 the `Vin` bits
/// this machine does not have. High byte is `NR51`, one bit per channel per side.
const FULL_VOLUME_BOTH_SIDES: u16 = 0xFF77;

/// `SOUND1CNT_H` / `SOUND2CNT_L`: envelope at full volume with no movement, duty 2.
const ENVELOPE_FULL_DUTY_HALF: u16 = 0xF080;

/// Every address the block spans, gaps and channel 3 included.
const BLOCK: std::ops::Range<u32> = 0x0400_0060..0x0400_00B0;

/// Collect the left channel of the PSG's output across one full waveform cycle.
fn sweep_output(psg: &mut Psg, cycles_per_step: u32, steps: usize) -> Vec<f32> {
    let mut out = vec![psg.output().0];
    for _ in 0..steps {
        psg.tick(cycles_per_step);
        out.push(psg.output().0);
    }
    out
}

// -- Decode ------------------------------------------------------------------

#[test]
fn every_register_this_module_claims_is_claimed_at_both_of_its_bytes() {
    for addr in [
        reg::SOUND1CNT_L,
        reg::SOUND1CNT_H,
        reg::SOUND1CNT_X,
        reg::SOUND2CNT_L,
        reg::SOUND2CNT_H,
        reg::SOUND4CNT_L,
        reg::SOUND4CNT_H,
        reg::SOUNDCNT_L,
    ] {
        assert!(Psg::owns(addr), "{addr:#X}");
        assert!(Psg::owns(addr + 1), "{addr:#X} odd byte");
    }
}

#[test]
fn the_gaps_between_the_registers_decode_to_nothing() {
    // A wrong address here does not crash: it produces a register that reads back zero and does
    // nothing, which is exactly the failure this block is laid out to invite. The gaps are real
    // hardware gaps, not registers this module happens not to implement.
    let mut p = psg();
    for addr in [
        0x0400_0066,
        0x0400_006A,
        0x0400_006E,
        0x0400_007A,
        0x0400_007E,
    ] {
        assert!(!Psg::owns(addr), "{addr:#X} is a gap");
        assert_eq!(p.read16(addr), None, "{addr:#X}");
        assert_eq!(p.write16(addr, 0xFFFF), None, "{addr:#X}");
    }
}

#[test]
fn channel_threes_registers_and_wave_ram_window_are_claimed() {
    let mut p = psg();
    for addr in [
        reg::SOUND3CNT_L,
        reg::SOUND3CNT_H,
        reg::SOUND3CNT_X,
        reg::WAVE_RAM,
        reg::WAVE_RAM + WAVE_RAM_WINDOW_BYTES - 2, // the window's last halfword
    ] {
        assert!(Psg::owns(addr), "{addr:#X}");
        assert!(p.read16(addr).is_some(), "{addr:#X}");
        assert!(p.write16(addr, 0xFFFF).is_some(), "{addr:#X}");
    }
}

#[test]
fn the_psg_and_direct_sound_blocks_never_claim_the_same_address() {
    // The two blocks interleave: `SOUNDCNT_L` at 0x80 is the PSG's, `SOUNDCNT_H` at 0x82 and
    // `SOUNDCNT_X` at 0x84 belong to direct sound, and the FIFOs are further along. Both `owns`
    // functions are written as explicit ranges for this reason — a mask over either one would
    // swallow the other's registers, and the only symptom would be silence.
    for addr in BLOCK {
        assert!(
            !(Psg::owns(addr) && DirectSound::owns(addr)),
            "{addr:#X} is claimed twice"
        );
    }
    assert!(DirectSound::owns(crate::fifo::reg::SOUNDCNT_H));
    assert!(DirectSound::owns(crate::fifo::reg::SOUNDCNT_X));
    assert!(!Psg::owns(crate::fifo::reg::SOUNDCNT_H));
    assert!(!Psg::owns(crate::fifo::reg::SOUNDCNT_X));
    assert!(DirectSound::owns(0x0400_0088), "SOUNDBIAS");
    assert!(!Psg::owns(0x0400_0088));
    assert!(!Psg::owns(0x0400_005E), "below the block");
}

#[test]
fn write_only_fields_read_back_as_zero_rather_than_as_ones() {
    // The opposite of the Game Boy, where an undriven bit floats high — see
    // `system-gb::apu::read_mask`, which is built the other way up.
    let mut p = psg();
    let expected = [
        (reg::SOUND1CNT_L, 0x007F),
        (reg::SOUND1CNT_H, 0xFFC0),
        (reg::SOUND1CNT_X, 0x4000),
        (reg::SOUND2CNT_L, 0xFFC0),
        (reg::SOUND2CNT_H, 0x4000),
        (reg::SOUND4CNT_L, 0xFF00),
        (reg::SOUND4CNT_H, 0x40FF),
        (reg::SOUNDCNT_L, 0xFF77),
    ];
    for (addr, mask) in expected {
        p.write16(addr, 0xFFFF);
        assert_eq!(p.read16(addr), Some(mask), "{addr:#X}");
    }
}

#[test]
fn the_trigger_bit_never_reads_back_because_it_is_a_strobe() {
    let mut p = psg();
    p.write16(reg::SOUND1CNT_H, ENVELOPE_FULL_DUTY_HALF);
    p.write16(reg::SOUND1CNT_X, TRIGGER | LENGTH_ENABLE | 0x0500);
    assert!(p.ch1.enabled, "it did trigger");
    assert_eq!(
        p.read16(reg::SOUND1CNT_X),
        Some(LENGTH_ENABLE),
        "only the length-enable flag survives a read"
    );
}

#[test]
fn each_register_reaches_the_field_it_names() {
    let mut p = psg();

    // Channel 1: sweep, duty, envelope, and an eleven-bit frequency across the halfword.
    p.write16(reg::SOUND1CNT_L, 0x0035); // shift 5, increasing, period 3
    let sweep = p.ch1.sweep.expect("channel 1 has a sweep unit");
    assert_eq!(sweep.period, 3);
    assert!(!sweep.decreasing);
    assert_eq!(sweep.shift, 5);

    p.write16(reg::SOUND1CNT_H, 0xABC0); // duty 3, envelope volume 10 increasing period 3
    assert_eq!(p.ch1.duty, 3);
    assert_eq!(p.ch1.envelope.initial_volume, 10);
    assert!(p.ch1.envelope.increasing);
    assert_eq!(p.ch1.envelope.period, 3);

    p.write16(reg::SOUND1CNT_X, 0x07FF);
    assert_eq!(p.ch1.frequency, 0x7FF, "all eleven bits, in one write");

    // Channel 2 is the same shape one register earlier, and has no sweep at all.
    p.write16(reg::SOUND2CNT_L, 0xF040); // duty 1, envelope at full volume
    p.write16(reg::SOUND2CNT_H, 0x0123);
    assert_eq!(p.ch2.duty, 1);
    assert_eq!(p.ch2.frequency, 0x123);
    assert!(p.ch2.sweep.is_none());

    // Channel 4's noise parameters share the low byte the way `NR43` does.
    p.write16(reg::SOUND4CNT_H, 0x005B); // shift 5, short mode, divisor 3
    assert_eq!(p.ch4.clock_shift, 5);
    assert!(p.ch4.short_mode);
    assert_eq!(p.ch4.divisor_code, 3);
    p.write16(reg::SOUND4CNT_L, 0xB200); // envelope volume 11 decreasing period 2
    assert_eq!(p.ch4.envelope.initial_volume, 11);
    assert!(!p.ch4.envelope.increasing);
    assert_eq!(p.ch4.envelope.period, 2);

    // `SOUNDCNT_L` is `NR50` and `NR51` stacked, low byte first.
    p.write16(reg::SOUNDCNT_L, 0xA953);
    assert_eq!(p.mixer.read_nr50(), 0x53);
    assert_eq!(p.mixer.read_nr51(), 0xA9);
    assert_eq!(p.mixer.left_volume, 5);
    assert_eq!(p.mixer.right_volume, 3);
}

#[test]
fn a_sweep_write_that_leaves_negate_mode_kills_the_channel() {
    // The one place `SOUND1CNT_L` does more than store bits. Same rule as `NR10`.
    let mut p = psg();
    p.write16(reg::SOUND1CNT_H, ENVELOPE_FULL_DUTY_HALF);
    p.write16(reg::SOUND1CNT_L, 0x001B); // period 1, decreasing, shift 3
    p.write16(reg::SOUND1CNT_X, TRIGGER | 500);
    assert!(p.ch1.enabled);

    p.tick(2 * 32768); // a sweep step, which calculates in negate mode
    p.write16(reg::SOUND1CNT_L, 0x0013); // same, but no longer decreasing
    assert!(!p.ch1.enabled, "leaving negate mode strands the borrow");
}

// -- Sound -------------------------------------------------------------------

#[test]
fn channel_one_makes_a_sound_when_it_is_triggered() {
    let mut p = psg();
    p.write16(reg::SOUNDCNT_L, FULL_VOLUME_BOTH_SIDES);
    assert_eq!(p.output(), (0.0, 0.0), "nothing playing yet");

    p.write16(reg::SOUND1CNT_H, ENVELOPE_FULL_DUTY_HALF);
    p.write16(reg::SOUND1CNT_X, TRIGGER | 2044); // a 16 t-cycle period

    let levels = sweep_output(&mut p, 64, 8);
    assert!(levels.iter().any(|&v| v != 0.0), "{levels:?}");
    assert!(
        levels.iter().any(|&v| v != levels[0]),
        "and it is a waveform, not a held level: {levels:?}"
    );
}

#[test]
fn channel_two_makes_a_sound_when_it_is_triggered() {
    let mut p = psg();
    p.write16(reg::SOUNDCNT_L, FULL_VOLUME_BOTH_SIDES);
    p.write16(reg::SOUND2CNT_L, ENVELOPE_FULL_DUTY_HALF);
    p.write16(reg::SOUND2CNT_H, TRIGGER | 2044);

    let levels = sweep_output(&mut p, 64, 8);
    assert!(levels.iter().any(|&v| v != 0.0), "{levels:?}");
    assert!(levels.iter().any(|&v| v != levels[0]), "{levels:?}");
}

#[test]
fn channel_four_makes_a_sound_when_it_is_triggered() {
    let mut p = psg();
    p.write16(reg::SOUNDCNT_L, FULL_VOLUME_BOTH_SIDES);
    p.write16(reg::SOUND4CNT_L, 0xF000); // envelope full volume, no movement
    p.write16(reg::SOUND4CNT_H, TRIGGER); // fastest divisor, no shift

    // The register starts full, so the first level is silence by design; the noise appears as
    // the shift register walks.
    let levels = sweep_output(&mut p, 32, 40);
    assert!(levels.iter().any(|&v| v != 0.0), "{levels:?}");
}

#[test]
fn channel_three_makes_a_sound_when_it_is_triggered() {
    let mut p = psg();
    p.write16(reg::SOUNDCNT_L, FULL_VOLUME_BOTH_SIDES);
    assert_eq!(p.output(), (0.0, 0.0), "nothing playing yet");

    // Loaded directly into the field that plays at the defaults (bank 0, `active_bank` false)
    // rather than through the wave-RAM window: that indirection, and which bank it targets, is
    // `the_wave_ram_window_exposes_the_bank_not_selected_for_playback`'s own test.
    for (i, byte) in p.ch3.wave_ram.iter_mut().enumerate() {
        *byte = ((((i * 2 + 1) % 16) << 4) | ((i * 2) % 16)) as u8;
    }
    p.write16(reg::SOUND3CNT_L, 1 << 7); // DAC on, dimension and bank left at their defaults
    p.write16(reg::SOUND3CNT_H, 1 << 13); // full volume
    p.write16(reg::SOUND3CNT_X, TRIGGER | 2044);

    let levels = sweep_output(&mut p, 32, 32);
    assert!(levels.iter().any(|&v| v != 0.0), "{levels:?}");
    assert!(
        levels.iter().any(|&v| v != levels[0]),
        "and it is a waveform, not a held level: {levels:?}"
    );
}

#[test]
fn the_wave_ram_window_exposes_the_bank_not_selected_for_playback() {
    // The double-buffering idiom the window exists for: a game loads a fresh waveform into
    // whichever bank is not currently sounding, then flips the bank-select bit to swap them
    // without a gap. `active_bank` is the bank *playing*, so the CPU must see the other one.
    let mut p = psg();
    p.write16(reg::SOUND3CNT_L, 1 << 7); // DAC on, bank 0 selected (bit 6 clear)
    p.write16(reg::WAVE_RAM, 0x1111);
    assert_eq!(
        p.ch3.wave_ram_bank1[0..2],
        [0x11, 0x11],
        "bank 0 plays, so the window wrote bank 1"
    );
    assert_eq!(
        p.ch3.wave_ram[0..2],
        [0, 0],
        "and left the playing bank alone"
    );

    p.write16(reg::SOUND3CNT_L, (1 << 7) | (1 << 6)); // swap to bank 1
    p.write16(reg::WAVE_RAM, 0x2222);
    assert_eq!(
        p.ch3.wave_ram[0..2],
        [0x22, 0x22],
        "bank 1 plays now, so the window wrote bank 0"
    );
}

#[test]
fn a_sixty_four_sample_channel_three_plays_both_banks_through_the_register_layer() {
    let mut p = psg();
    p.write16(reg::SOUNDCNT_L, FULL_VOLUME_BOTH_SIDES);
    p.ch3.wave_ram = [0x11; WAVE_RAM_BYTES]; // every bank-0 sample is 1
    p.ch3.wave_ram_bank1 = [0x22; WAVE_RAM_BYTES]; // every bank-1 sample is 2
    p.write16(reg::SOUND3CNT_L, (1 << 7) | (1 << 5)); // DAC on, 64-sample dimension
    p.write16(reg::SOUND3CNT_H, 1 << 13); // full volume
    p.write16(reg::SOUND3CNT_X, TRIGGER | 2044);

    // `Psg::tick` takes CPU cycles, four per t-cycle, unlike `WaveChannel::tick` which the other
    // apu-shared-level tests drive directly in t-cycles.
    let period_in_cpu_cycles = (2048 - 2044) * 2 * 4;
    assert_eq!(p.ch3.output(), 1, "sample 0 is bank 0");
    p.tick(period_in_cpu_cycles * 32);
    assert_eq!(p.ch3.output(), 2, "sample 32 crossed into bank 1");
}

#[test]
fn force_75_percent_reaches_the_wave_channel_through_soundcnt_h() {
    // `volume_shift` 0 mutes by forcing every sample to the same fixed digital level, not by
    // driving the DAC toward analog zero — 0 maps to this DAC's *loudest* extreme (see
    // `apu_shared::dac_output`), so a magnitude comparison could not tell "muted" from "loud"
    // apart. What actually distinguishes them is variation: a muted channel holds one constant
    // level regardless of what the waveform says, and an unmuted one moves with it.
    let mut p = psg();
    p.write16(reg::SOUNDCNT_L, FULL_VOLUME_BOTH_SIDES);
    for (i, byte) in p.ch3.wave_ram.iter_mut().enumerate() {
        *byte = ((((i * 2 + 1) % 16) << 4) | ((i * 2) % 16)) as u8;
    }
    p.write16(reg::SOUND3CNT_L, 1 << 7);
    p.write16(reg::SOUND3CNT_H, 0); // volume_shift 0: mute, if the override did not apply
    p.write16(reg::SOUND3CNT_X, TRIGGER | 2044);
    let muted = sweep_output(&mut p, 32, 8);
    assert!(
        muted.iter().all(|&v| v == muted[0]),
        "volume_shift 0 should hold one level regardless of the waveform: {muted:?}"
    );

    p.write16(reg::SOUND3CNT_H, 1 << 15); // force 75%, volume_shift still 0
    p.write16(reg::SOUND3CNT_X, TRIGGER | 2044);
    let forced = sweep_output(&mut p, 32, 8);
    assert!(
        forced.iter().any(|&v| v != forced[0]),
        "the force-75% override should let the waveform move again: {forced:?}"
    );
}

#[test]
fn panning_routes_a_channel_to_the_side_that_selected_it() {
    let mut p = psg();
    // Full volume both sides; channel 1 left only, channel 4 right only.
    p.write16(reg::SOUNDCNT_L, 0x1077);
    p.write16(reg::SOUND1CNT_H, ENVELOPE_FULL_DUTY_HALF);
    p.write16(reg::SOUND1CNT_X, TRIGGER | 2044);
    p.write16(reg::SOUND4CNT_L, 0xF000);
    p.write16(reg::SOUND4CNT_H, TRIGGER);

    let (left, right) = p.output();
    assert!(left != 0.0, "channel 1 is enabled on the left");
    assert_eq!(right, 0.0, "and on no other side");

    // Channel 4's own bits, one side at a time: bit 3 of `NR51` is right, bit 7 is left.
    p.write16(reg::SOUNDCNT_L, 0x0877);
    p.tick(4 * 200); // walk the shift register off its all-ones start
    let (left, right) = p.output();
    assert_eq!(left, 0.0);
    assert!(right != 0.0, "channel 4 reached the right side");
}

#[test]
fn channel_threes_untriggered_slot_does_not_displace_channel_four() {
    // `Mixer::mix` reads panning by position, so an unplayed channel still occupies its own
    // slot rather than shifting the ones after it — if channel 3's silence here came from
    // being skipped rather than merely untriggered, channel 4 would be routed through channel
    // 3's enable bits, and a game panning them differently would hear the wrong one.
    let mut p = psg();
    p.write16(reg::SOUND4CNT_L, 0xF000);
    p.write16(reg::SOUND4CNT_H, TRIGGER);
    p.tick(4 * 200);

    // Only channel 3's enables set, on both sides.
    p.write16(reg::SOUNDCNT_L, 0x4477);
    assert_eq!(p.output(), (0.0, 0.0), "channel 3 is not playing anything");

    // Only channel 4's.
    p.write16(reg::SOUNDCNT_L, 0x8877);
    let (left, right) = p.output();
    assert!(left != 0.0 && right != 0.0, "channel 4 has its own bits");
}

#[test]
fn the_master_volume_scales_what_comes_out() {
    let mut p = psg();
    p.write16(reg::SOUNDCNT_L, FULL_VOLUME_BOTH_SIDES);
    p.write16(reg::SOUND1CNT_H, ENVELOPE_FULL_DUTY_HALF);
    p.write16(reg::SOUND1CNT_X, TRIGGER | 2044);
    let loud = p.output().0.abs();

    p.write16(reg::SOUNDCNT_L, 0xFF00); // volume 0 both sides
    let quiet = p.output().0.abs();
    assert!(quiet < loud, "{quiet} is not quieter than {loud}");
    assert!(
        quiet > 0.0,
        "but volume 0 is the quietest step, not silence"
    );
}

// -- Clocks ------------------------------------------------------------------

#[test]
fn the_channels_run_at_a_quarter_of_the_cpu_clock() {
    // The GBA clocks its PSG through the same divider the Game Boy does, off a clock four times
    // as fast. Feeding the shared channels CPU cycles directly would play every note two
    // octaves high — audible, and invisible to any test that only checks for non-silence.
    let mut p = psg();
    p.write16(reg::SOUND1CNT_H, ENVELOPE_FULL_DUTY_HALF);
    p.write16(reg::SOUND1CNT_X, TRIGGER | 2044);

    let mut reference = SquareChannel::new();
    reference.write_envelope(0xF0);
    reference.duty = 2;
    reference.frequency = 2044;
    reference.trigger();

    for step in 0..24 {
        p.tick(4 * 7); // twenty-eight CPU cycles is seven t-cycles
        reference.tick(7);
        assert_eq!(p.ch1.output(), reference.output(), "step {step}");
    }
}

#[test]
fn the_cycles_left_over_by_the_divider_are_carried_rather_than_dropped() {
    // Instruction costs are not multiples of four. Dropping the remainder would run every
    // channel slow in proportion to how finely the machine happens to be stepped, which is a
    // pitch error that changes with the emulator's own scheduling.
    let armed = || {
        let mut p = psg();
        p.write16(reg::SOUND1CNT_H, ENVELOPE_FULL_DUTY_HALF);
        p.write16(reg::SOUND1CNT_X, TRIGGER | 2044);
        p
    };
    let mut coarse = armed();
    let mut fine = armed();

    for _ in 0..64 {
        coarse.tick(4);
        for _ in 0..4 {
            fine.tick(1);
        }
    }
    assert_eq!(fine.ch1, coarse.ch1);
}

#[test]
fn the_frame_sequencer_clocks_the_envelope_on_its_eighth_step() {
    // 512 Hz off a 16.78 MHz clock is 32768 CPU cycles a step, and the envelope moves on step 7.
    let mut p = psg();
    p.write16(reg::SOUND1CNT_H, 0xF180); // volume 15, decreasing, period 1
    p.write16(reg::SOUND1CNT_X, TRIGGER | 1000);
    assert_eq!(p.ch1.envelope.volume, 15);

    p.tick(6 * 32768);
    assert_eq!(p.ch1.envelope.volume, 15, "not yet");
    p.tick(32768);
    assert_eq!(p.ch1.envelope.volume, 14, "step 7 is the envelope's");
}

#[test]
fn the_frame_sequencer_clocks_the_length_counters_on_its_even_steps() {
    let mut p = psg();
    p.write16(reg::SOUND1CNT_H, 0xF0BF); // one length step left, duty 2
    p.write16(reg::SOUND1CNT_X, TRIGGER | LENGTH_ENABLE | 1000);
    assert!(p.ch1.enabled);

    p.tick(32768);
    assert!(p.ch1.enabled, "step 1 is odd, so nothing clocked");
    p.tick(32768);
    assert!(!p.ch1.enabled, "step 2 ran the length counter out");
}

#[test]
fn the_frame_sequencer_clocks_the_sweep_on_steps_two_and_six() {
    let mut p = psg();
    p.write16(reg::SOUND1CNT_L, 0x0014); // period 1, increasing, shift 4
    p.write16(reg::SOUND1CNT_H, ENVELOPE_FULL_DUTY_HALF);
    p.write16(reg::SOUND1CNT_X, TRIGGER | 500);

    p.tick(32768);
    assert_eq!(p.ch1.frequency, 500, "step 1 is not a sweep step");
    p.tick(32768);
    assert_eq!(p.ch1.frequency, 531, "500 + 500>>4, on step 2");
}

// -- Power -------------------------------------------------------------------

#[test]
fn a_powered_down_psg_is_silent_and_ignores_every_write() {
    // One master enable gates direct sound and the PSG together, and clearing it does to this
    // side what clearing `NR52` bit 7 does to a CGB.
    let mut p = psg();
    p.write16(reg::SOUNDCNT_L, FULL_VOLUME_BOTH_SIDES);
    p.write16(reg::SOUND1CNT_H, ENVELOPE_FULL_DUTY_HALF);
    p.write16(reg::SOUND1CNT_X, TRIGGER | 2044);
    assert!(p.output().0 != 0.0);

    p.set_power(false);
    assert_eq!(p.output(), (0.0, 0.0));
    assert!(!p.ch1.enabled);
    assert_eq!(p.read16(reg::SOUNDCNT_L), Some(0), "registers cleared");

    // Writes are accepted by the bus and discarded, including the length fields a DMG would
    // still take.
    p.write16(reg::SOUNDCNT_L, FULL_VOLUME_BOTH_SIDES);
    p.write16(reg::SOUND1CNT_H, ENVELOPE_FULL_DUTY_HALF);
    p.write16(reg::SOUND1CNT_X, TRIGGER | 2044);
    assert!(!p.ch1.enabled);
    assert_eq!(p.read16(reg::SOUND1CNT_H), Some(0));
    assert_eq!(p.output(), (0.0, 0.0));

    // And the channels stay stopped rather than resuming where they left off.
    p.set_power(true);
    p.tick(4 * 1000);
    assert_eq!(p.output(), (0.0, 0.0));
}

#[test]
fn a_powered_down_psg_does_not_advance_its_clocks() {
    let mut p = Psg::new();
    assert!(!p.is_powered(), "a machine starts with its sound unit off");
    p.tick(4 * 100_000);
    assert_eq!(p, Psg::new(), "nothing moved");
}

// -- State -------------------------------------------------------------------

#[test]
fn psg_state_round_trips_and_resumes_the_same_waveform() {
    let mut p = psg();
    p.write16(reg::SOUNDCNT_L, 0x5A37);
    p.write16(reg::SOUND1CNT_L, 0x0035);
    p.write16(reg::SOUND1CNT_H, ENVELOPE_FULL_DUTY_HALF);
    p.write16(reg::SOUND1CNT_X, TRIGGER | 1500);
    p.write16(reg::SOUND2CNT_L, 0xC340);
    p.write16(reg::SOUND2CNT_H, TRIGGER | 700);
    p.write16(reg::SOUND4CNT_L, 0xF200);
    p.write16(reg::SOUND4CNT_H, TRIGGER | 0x0042);
    // An odd number of cycles, so the divider's remainder is non-zero as well.
    p.tick(9_999);

    let mut w = StateWriter::new();
    p.save(&mut w);
    let blob = w.into_inner();

    let mut restored = Psg::new();
    restored.load(&mut StateReader::new(&blob)).unwrap();
    assert_eq!(restored, p);
    assert!(restored.is_powered());
    assert_eq!(restored.read16(reg::SOUNDCNT_L), Some(0x5A37 & 0xFF77));

    // And both must go on producing the same audio, which is the part a field left out of
    // `save` would break rather than the comparison above.
    for _ in 0..50 {
        p.tick(137);
        restored.tick(137);
        assert_eq!(restored.output(), p.output());
    }
}
