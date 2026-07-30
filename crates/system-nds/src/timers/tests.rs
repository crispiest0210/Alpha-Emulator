use super::*;

/// `TMxCNT_L` for a channel.
fn count(channel: u32) -> u32 {
    BASE + channel * 4
}

/// `TMxCNT_H` for a channel.
fn control(channel: u32) -> u32 {
    BASE + channel * 4 + 2
}

const ENABLE: u16 = 0x80;
const IRQ: u16 = 0x40;
const CASCADE: u16 = 0x04;

#[test]
fn the_block_owns_its_sixteen_bytes_and_nothing_else() {
    assert!(TimerBlock::owns(BASE));
    assert!(TimerBlock::owns(BASE + 15));
    assert!(!TimerBlock::owns(BASE - 1));
    assert!(!TimerBlock::owns(BASE + 16));
}

#[test]
fn a_timer_does_not_run_until_it_is_enabled() {
    let mut t = TimerBlock::new();
    t.step(1000);
    assert_eq!(t.counter(0), 0);

    t.write16(control(0), ENABLE);
    t.step(1000);
    assert_eq!(t.counter(0), 1000, "one count per cycle at prescaler 1");
}

#[test]
fn writing_the_low_half_sets_the_reload_and_not_the_counter() {
    let mut t = TimerBlock::new();
    t.write16(count(0), 0x1234);
    assert_eq!(t.reload(0), 0x1234);
    assert_eq!(t.counter(0), 0, "the running counter is untouched");

    // Enabling is what copies it across.
    t.write16(control(0), ENABLE);
    assert_eq!(t.counter(0), 0x1234);
    assert_eq!(t.read16(count(0)), Some(0x1234), "and reads the counter");
}

#[test]
fn re_enabling_reloads_but_rewriting_a_running_control_register_does_not() {
    let mut t = TimerBlock::new();
    t.write16(count(0), 0x0100);
    t.write16(control(0), ENABLE);
    t.step(0x50);
    assert_eq!(t.counter(0), 0x0150);

    // Still enabled, so no reload.
    t.write16(control(0), ENABLE | IRQ);
    assert_eq!(t.counter(0), 0x0150);

    // Off and on again does reload.
    t.write16(control(0), 0);
    t.write16(control(0), ENABLE);
    assert_eq!(t.counter(0), 0x0100);
}

#[test]
fn each_prescaler_setting_divides_the_system_clock() {
    for (setting, shift) in PRESCALER_SHIFT.iter().enumerate() {
        let mut t = TimerBlock::new();
        t.write16(control(0), ENABLE | setting as u16);
        t.step(1 << shift);
        assert_eq!(t.counter(0), 1, "prescaler setting {setting}");
        // One cycle short of the next tick is still one count.
        t.step((1 << shift) - 1);
        assert_eq!(t.counter(0), 1);
        t.step(1);
        assert_eq!(t.counter(0), 2);
    }
}

#[test]
fn the_prescaler_remainder_carries_across_steps() {
    let mut t = TimerBlock::new();
    t.write16(control(0), ENABLE | 1); // divide by 64
    for _ in 0..64 {
        t.step(1);
    }
    assert_eq!(t.counter(0), 1, "sixty-four one-cycle steps are one tick");
}

#[test]
fn overflow_reloads_and_raises_the_interrupt_only_when_enabled() {
    let mut t = TimerBlock::new();
    t.write16(count(0), 0xFFFE);
    t.write16(control(0), ENABLE);
    assert_eq!(t.step(1), 0);
    assert_eq!(t.counter(0), 0xFFFF);
    assert_eq!(t.step(1), 0, "overflowed, but no interrupt requested");
    assert_eq!(t.counter(0), 0xFFFE, "reloaded, not wrapped to zero");

    t.write16(control(0), 0);
    t.write16(control(0), ENABLE | IRQ);
    assert_eq!(t.step(2), 0b0001);
}

#[test]
fn a_single_step_spanning_many_overflows_counts_all_of_them() {
    // A prescaler of 1 with a reload near the top overflows thousands of times per scanline,
    // and the tick count is computed rather than looped for exactly this case.
    let mut t = TimerBlock::new();
    t.write16(count(0), 0xFFFF); // a one-count lap
    t.write16(control(0), ENABLE | IRQ);
    t.write16(count(1), 0);
    t.write16(control(1), ENABLE | IRQ | CASCADE);

    t.step(1000);
    assert_eq!(t.counter(1), 1000, "one cascade tick per lap of channel 0");
}

#[test]
fn a_cascading_channel_ignores_its_own_prescaler() {
    let mut t = TimerBlock::new();
    t.write16(count(0), 0xFFFF);
    t.write16(control(0), ENABLE);
    // Prescaler 3 (divide by 1024) *and* cascade: cascade wins.
    t.write16(control(1), ENABLE | CASCADE | 3);

    t.step(10);
    assert_eq!(t.counter(1), 10, "ten laps of channel 0, ten counts");
}

#[test]
fn a_cascading_channel_does_not_advance_while_its_source_is_stopped() {
    let mut t = TimerBlock::new();
    t.write16(control(1), ENABLE | CASCADE);
    t.step(100_000);
    assert_eq!(t.counter(1), 0, "channel 0 is not running");
}

#[test]
fn channel_zero_has_nothing_to_cascade_from_so_the_bit_does_nothing() {
    let mut t = TimerBlock::new();
    t.write16(control(0), ENABLE | CASCADE);
    t.step(100);
    assert_eq!(t.counter(0), 100, "it runs off the prescaler regardless");
    assert_eq!(
        t.read16(control(0)),
        Some(ENABLE | CASCADE),
        "and the bit still reads back"
    );
}

#[test]
fn a_three_deep_cascade_advances_in_one_step() {
    // Channels are stepped in order, so each cascade sees the overflows produced by this call
    // rather than the previous one.
    let mut t = TimerBlock::new();
    t.write16(count(0), 0xFFFF);
    t.write16(control(0), ENABLE);
    t.write16(count(1), 0xFFFF);
    t.write16(control(1), ENABLE | CASCADE);
    t.write16(count(2), 0xFFFF);
    t.write16(control(2), ENABLE | CASCADE);
    t.write16(count(3), 0xFFF8);
    t.write16(control(3), ENABLE | CASCADE | IRQ);

    // Eight system cycles become eight laps of channel 0, which become eight counts on each
    // channel above it, which is exactly enough to overflow channel 3.
    assert_eq!(t.step(8), 0b1000, "eight cycles reach channel 3");
    assert_eq!(t.counter(3), 0xFFF8, "and it reloaded");
    assert_eq!(t.step(1), 0, "one more cycle is one more count, not a lap");
    assert_eq!(t.counter(3), 0xFFF9);
}

#[test]
fn several_channels_can_interrupt_from_one_step() {
    let mut t = TimerBlock::new();
    for channel in 0..4u32 {
        t.write16(count(channel), 0xFFFF);
        t.write16(control(channel), ENABLE | IRQ);
    }
    assert_eq!(t.step(1), 0b1111);
}

#[test]
fn byte_writes_splice_into_the_reload_rather_than_the_counter() {
    let mut t = TimerBlock::new();
    t.write16(count(0), 0xAABB);
    t.write16(control(0), ENABLE);
    t.step(0x100);
    assert_ne!(t.counter(0), t.reload(0), "the two have diverged");

    // Splicing a byte into what `read16` returns would set the reload to the live counter.
    t.write8(count(0), 0xCC);
    assert_eq!(t.reload(0), 0xAACC);
    t.write8(count(0) + 1, 0xDD);
    assert_eq!(t.reload(0), 0xDDCC);
}

#[test]
fn byte_reads_see_the_halves_of_the_live_counter() {
    let mut t = TimerBlock::new();
    t.write16(count(0), 0x1234);
    t.write16(control(0), ENABLE);
    assert_eq!(t.read8(count(0)), Some(0x34));
    assert_eq!(t.read8(count(0) + 1), Some(0x12));
    assert_eq!(t.read8(control(0)), Some(ENABLE as u8));
    assert_eq!(t.read8(BASE - 1), None);
}

#[test]
fn unused_control_bits_do_not_read_back() {
    let mut t = TimerBlock::new();
    t.write16(control(0), 0xFFFF);
    assert_eq!(t.read16(control(0)), Some(0x00C7));
}

#[test]
fn a_second_block_is_completely_independent() {
    // Each core has its own four. They share a base address and nothing else.
    let mut nine = TimerBlock::new();
    let mut seven = TimerBlock::new();
    nine.write16(control(0), ENABLE);
    nine.step(50);
    seven.step(50);
    assert_eq!(nine.counter(0), 50);
    assert_eq!(seven.counter(0), 0);
}

#[test]
fn timers_round_trip_through_a_save_state_mid_prescale() {
    use savestate::{decode_state, encode_state};

    let mut t = TimerBlock::new();
    t.write16(count(2), 0x0F00);
    t.write16(control(2), ENABLE | IRQ | 1); // divide by 64
    t.step(100); // one tick and 36 cycles of residual

    let blob = encode_state("nds", 1, &t);
    let mut restored = TimerBlock::new();
    decode_state("nds", 1, &blob, &mut restored).unwrap();

    assert_eq!(restored.counter(2), t.counter(2));
    assert_eq!(restored.reload(2), 0x0F00);
    // The residual has to survive too, or the timer runs slow for one tick after every load.
    restored.step(28);
    t.step(28);
    assert_eq!(restored.counter(2), t.counter(2));
    assert_eq!(restored.counter(2), 0x0F02, "two ticks in, not one");
}
