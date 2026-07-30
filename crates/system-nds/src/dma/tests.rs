use super::*;

fn src(channel: u32) -> u32 {
    BASE + channel * 12
}
fn dst(channel: u32) -> u32 {
    BASE + channel * 12 + 4
}
fn cnt(channel: u32) -> u32 {
    BASE + channel * 12 + 8
}
fn ctl(channel: u32) -> u32 {
    BASE + channel * 12 + 10
}

const ENABLE: u16 = 1 << 15;
const IRQ: u16 = 1 << 14;
const WORD: u16 = 1 << 10;
const REPEAT: u16 = 1 << 9;

fn arm9() -> DmaController {
    DmaController::new(Core::Arm9)
}
fn arm7() -> DmaController {
    DmaController::new(Core::Arm7)
}

/// Arm channel `ch` as an immediate transfer of `words` units.
fn immediate(d: &mut DmaController, ch: u32, from: u32, to: u32, words: u32, extra: u16) {
    d.write32(src(ch), from);
    d.write32(dst(ch), to);
    d.write16(cnt(ch), words as u16);
    d.write16(ctl(ch), ENABLE | extra);
}

#[test]
fn the_controller_owns_its_channel_registers() {
    let d = arm9();
    assert!(d.owns(BASE));
    assert!(d.owns(BASE + 47));
    // The fill words start immediately after the four channels, at BASE + 48.
    assert_eq!(FILL_BASE, BASE + 48);
    assert!(d.owns(FILL_BASE), "and the ARM9 has them");
    assert!(!d.owns(FILL_BASE + 16));
    assert!(!arm7().owns(FILL_BASE), "which the ARM7 does not");
    assert!(!arm7().owns(BASE + 48));
}

#[test]
fn an_immediate_transfer_arms_when_the_enable_bit_goes_up() {
    let mut d = arm9();
    assert!(d.take_transfer().is_none());
    immediate(&mut d, 0, 0x0200_0000, 0x0201_0000, 16, WORD | IRQ);

    let t = d.take_transfer().expect("armed");
    assert_eq!(t.channel, 0);
    assert_eq!(t.source, 0x0200_0000);
    assert_eq!(t.destination, 0x0201_0000);
    assert_eq!(t.words, 16);
    assert_eq!(t.unit, 4);
    assert!(t.raise_irq);
    assert!(d.take_transfer().is_none(), "and only once");
}

#[test]
fn a_one_shot_transfer_clears_its_own_enable_bit() {
    let mut d = arm9();
    immediate(&mut d, 0, 0, 0x100, 4, 0);
    d.take_transfer().unwrap();
    assert_eq!(d.read16(ctl(0)).unwrap() & ENABLE, 0, "a game polls this");
}

#[test]
fn priority_is_by_channel_number_and_absolute() {
    let mut d = arm9();
    immediate(&mut d, 3, 0, 0x300, 1, 0);
    immediate(&mut d, 1, 0, 0x100, 1, 0);
    immediate(&mut d, 2, 0, 0x200, 1, 0);
    assert_eq!(d.take_transfer().unwrap().channel, 1);
    assert_eq!(d.take_transfer().unwrap().channel, 2);
    assert_eq!(d.take_transfer().unwrap().channel, 3);
}

#[test]
fn the_arm9_counts_twenty_one_bits_of_words() {
    let mut d = arm9();
    d.write32(src(0), 0);
    d.write32(dst(0), 0x0200_0000);
    // 0x1F000 units: more than a halfword holds is not the point — the point is that the top
    // five bits live in the control register's low bits and must survive the write.
    d.write32(cnt(0), 0x0010_0000 | (ENABLE as u32) << 16);
    let t = d.take_transfer().unwrap();
    assert_eq!(t.words, 0x10_0000);

    // And zero means the full 21-bit maximum, which is two million units.
    d.write32(cnt(0), (ENABLE as u32) << 16);
    assert_eq!(d.take_transfer().unwrap().words, 0x20_0000);
}

#[test]
fn writing_the_control_halfword_carries_the_arm9s_high_count_bits() {
    let mut d = arm9();
    d.write16(cnt(0), 0x8000);
    // Bits 0-4 of the control halfword are bits 16-20 of the word count.
    d.write16(ctl(0), ENABLE | 0x0003);
    let t = d.take_transfer().unwrap();
    assert_eq!(
        t.words, 0x0003_8000,
        "halves treated as independent loses this"
    );
}

#[test]
fn the_arm7s_maximum_count_still_differs_per_channel() {
    let mut d = arm7();
    // Zero means maximum: 14 bits on channels 0-2, 16 on channel 3.
    for ch in 0..3u32 {
        immediate(&mut d, ch, 0, 0x100, 0, 0);
        assert_eq!(d.take_transfer().unwrap().words, 0x4000, "channel {ch}");
    }
    immediate(&mut d, 3, 0, 0x100, 0, 0);
    assert_eq!(d.take_transfer().unwrap().words, 0x1_0000);

    // And the ARM7 has no 21-bit count at all: control bits 0-4 are not count bits there.
    d.write16(cnt(3), 0x0010);
    d.write16(ctl(3), ENABLE | 0x001F);
    assert_eq!(d.take_transfer().unwrap().words, 0x10);
}

#[test]
fn the_two_cores_decode_the_start_timing_field_differently() {
    // The ARM9 reads three bits from 11.
    assert_eq!(
        StartTiming::from_bits(Core::Arm9, 2 << 11),
        StartTiming::HBlank
    );
    assert_eq!(
        StartTiming::from_bits(Core::Arm9, 7 << 11),
        StartTiming::GeometryFifo
    );
    // The ARM7 reads two from 12, so the same bits mean something else entirely: the ARM9's
    // hblank encoding is the ARM7's vblank, and the ARM9's vblank is the ARM7's immediate.
    assert_eq!(
        StartTiming::from_bits(Core::Arm7, 2 << 11),
        StartTiming::VBlank
    );
    assert_eq!(
        StartTiming::from_bits(Core::Arm7, 1 << 11),
        StartTiming::Immediate
    );
    assert_eq!(
        StartTiming::from_bits(Core::Arm7, 2 << 12),
        StartTiming::CardSlot
    );
}

#[test]
fn a_vblank_transfer_waits_for_vblank() {
    let mut d = arm9();
    d.write32(src(0), 0);
    d.write32(dst(0), 0x0200_0000);
    d.write16(cnt(0), 8);
    d.write16(ctl(0), ENABLE | (1 << 11));
    assert!(d.take_transfer().is_none(), "not immediate");

    d.on_vblank();
    assert_eq!(d.take_transfer().unwrap().words, 8);
}

#[test]
fn only_the_arm9_has_an_hblank_timing() {
    let mut nine = arm9();
    nine.write16(ctl(0), ENABLE | (2 << 11));
    nine.on_hblank();
    assert!(nine.take_transfer().is_some());

    // The same bits on the ARM7 are a *vblank* transfer, so hblank arms nothing at all there.
    let mut seven = arm7();
    seven.write16(ctl(0), ENABLE | (2 << 11));
    seven.on_hblank();
    assert!(seven.take_transfer().is_none());
    seven.on_vblank();
    assert!(seven.take_transfer().is_some());
}

#[test]
fn the_arm7s_third_timing_means_two_things_depending_on_the_channel() {
    // Channels 0 and 1 read timing 3 as the Slot-2 cartridge; 2 and 3 read it as wifi, which
    // this emulator never arms.
    let mut d = arm7();
    for ch in 0..4u32 {
        d.write32(dst(ch), 0x0200_0000);
        d.write16(cnt(ch), 4);
        d.write16(ctl(ch), ENABLE | (3 << 12));
    }
    assert!(d.take_transfer().is_none());
    d.arm_for(StartTiming::GbaSlot);
    assert_eq!(d.take_transfer().unwrap().channel, 0);
    assert_eq!(d.take_transfer().unwrap().channel, 1);
    assert!(d.take_transfer().is_none(), "2 and 3 are wifi channels");
}

#[test]
fn a_repeating_transfer_stays_enabled_and_refires_on_each_trigger() {
    let mut d = arm9();
    d.write32(src(0), 0x0200_0000);
    d.write32(dst(0), 0x0400_04A0);
    d.write16(cnt(0), 4);
    d.write16(ctl(0), ENABLE | REPEAT | WORD | (1 << 11));

    d.on_vblank();
    let first = d.take_transfer().unwrap();
    assert_ne!(d.read16(ctl(0)).unwrap() & ENABLE, 0, "still enabled");

    d.on_vblank();
    let second = d.take_transfer().unwrap();
    // The running source advanced; the destination did not, because it is a fixed register.
    assert_eq!(second.source, first.source + 16);
}

#[test]
fn a_repeating_immediate_transfer_stops_after_one_go() {
    // There is no trigger to repeat on, so hardware treats the repeat bit as meaningless.
    let mut d = arm9();
    immediate(&mut d, 0, 0, 0x100, 4, REPEAT);
    assert!(d.take_transfer().is_some());
    assert!(d.take_transfer().is_none());
    assert_eq!(d.read16(ctl(0)).unwrap() & ENABLE, 0);
}

#[test]
fn the_reload_destination_step_snaps_back_for_the_next_repeat() {
    let mut d = arm9();
    d.write32(src(0), 0x0200_0000);
    d.write32(dst(0), 0x0202_0000);
    d.write16(cnt(0), 4);
    // Destination step 3 = increment/reload, in bits 5-6.
    d.write16(ctl(0), ENABLE | REPEAT | WORD | (3 << 5) | (1 << 11));

    d.on_vblank();
    assert_eq!(d.take_transfer().unwrap().destination, 0x0202_0000);
    d.on_vblank();
    assert_eq!(
        d.take_transfer().unwrap().destination,
        0x0202_0000,
        "reloaded rather than advanced"
    );
}

#[test]
fn address_steps_move_the_running_addresses_the_right_way() {
    let mut d = arm9();
    d.write32(src(0), 0x0200_1000);
    d.write32(dst(0), 0x0202_1000);
    d.write16(cnt(0), 4);
    // Source decrement (1 << 7), destination fixed (2 << 5).
    d.write16(
        ctl(0),
        ENABLE | REPEAT | WORD | (1 << 7) | (2 << 5) | (1 << 11),
    );

    d.on_vblank();
    d.take_transfer().unwrap();
    d.on_vblank();
    let second = d.take_transfer().unwrap();
    assert_eq!(second.source, 0x0200_1000 - 16);
    assert_eq!(second.destination, 0x0202_1000);
}

#[test]
fn a_source_may_not_use_the_reload_encoding() {
    let mut d = arm9();
    // Source step 3 is prohibited and behaves as increment.
    d.write16(ctl(0), ENABLE | (3 << 7));
    assert_eq!(
        d.take_transfer().unwrap().source_step,
        AddressStep::Increment
    );
}

#[test]
fn adjusting_a_running_channels_control_does_not_relatch_its_addresses() {
    let mut d = arm9();
    d.write32(src(0), 0x0200_0000);
    d.write32(dst(0), 0x0300_0000);
    d.write16(cnt(0), 4);
    d.write16(ctl(0), ENABLE | REPEAT | WORD | (1 << 11));
    d.on_vblank();
    d.take_transfer().unwrap();

    // The game turns the interrupt on mid-flight. The addresses must not snap back.
    d.write16(ctl(0), ENABLE | REPEAT | WORD | IRQ | (1 << 11));
    d.on_vblank();
    let t = d.take_transfer().unwrap();
    assert_eq!(t.source, 0x0200_0010);
    assert!(t.raise_irq);
}

#[test]
fn clearing_the_enable_bit_disarms_a_waiting_channel() {
    let mut d = arm9();
    d.write16(ctl(0), ENABLE | (1 << 11));
    d.on_vblank();
    d.write16(ctl(0), 0);
    assert!(d.take_transfer().is_none());
}

#[test]
fn the_unit_size_bit_picks_halfwords_or_words() {
    let mut d = arm9();
    immediate(&mut d, 0, 0, 0x100, 1, 0);
    assert_eq!(d.take_transfer().unwrap().unit, 2);
    immediate(&mut d, 0, 0, 0x100, 1, WORD);
    assert_eq!(d.take_transfer().unwrap().unit, 4);
}

#[test]
fn source_and_destination_are_write_only() {
    let mut d = arm9();
    d.write32(src(0), 0x0200_1234);
    d.write32(dst(0), 0x0300_5678);
    assert_eq!(d.read32(src(0)), Some(0));
    assert_eq!(d.read32(dst(0)), Some(0));
    assert_eq!(d.read32(BASE - 4), None);
}

#[test]
fn the_fill_words_are_arm9_only_storage() {
    let mut d = arm9();
    d.write32(FILL_BASE + 4, 0xDEAD_BEEF);
    assert_eq!(d.fill(1), 0xDEAD_BEEF);
    assert_eq!(d.read32(FILL_BASE + 4), Some(0xDEAD_BEEF));
    d.write16(FILL_BASE + 4, 0x1234);
    assert_eq!(d.fill(1), 0xDEAD_1234);

    let mut seven = arm7();
    assert!(!seven.write32(FILL_BASE, 1));
    assert_eq!(seven.read32(FILL_BASE), None);
}

#[test]
fn byte_writes_reach_the_control_register() {
    let mut d = arm9();
    d.write32(src(0), 0x0200_0000);
    d.write32(dst(0), 0x0300_0000);
    d.write16(cnt(0), 2);
    d.write8(ctl(0) + 1, (ENABLE >> 8) as u8);
    assert!(d.take_transfer().is_some(), "the enable edge still fired");
    assert_eq!(d.read8(BASE - 1), None);
}

#[test]
fn dma_round_trips_through_a_save_state_mid_repeat() {
    use savestate::{decode_state, encode_state};

    let mut d = arm9();
    d.write32(src(1), 0x0200_0000);
    d.write32(dst(1), 0x0400_04A0);
    d.write16(cnt(1), 4);
    d.write16(ctl(1), ENABLE | REPEAT | WORD | (1 << 11));
    d.on_vblank();
    d.take_transfer().unwrap();

    let blob = encode_state("nds", 1, &d);
    let mut restored = arm9();
    decode_state("nds", 1, &blob, &mut restored).unwrap();

    restored.on_vblank();
    let t = restored.take_transfer().unwrap();
    // The *running* source, not the written one, is what has to have survived.
    assert_eq!(t.source, 0x0200_0010);
    assert_eq!(t.channel, 1);
}
