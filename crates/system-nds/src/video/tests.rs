use super::*;

const ARM9: Core = Core::Arm9;
const ARM7: Core = Core::Arm7;

/// Run one whole line, returning the events in order.
fn step_line(v: &mut VideoTiming) -> Vec<VideoEvent> {
    let mut events = Vec::new();
    loop {
        let budget = v.cycles_until_next_event();
        let event = v.advance(budget).unwrap();
        events.push(event);
        if event == VideoEvent::LineEnd {
            return events;
        }
    }
}

fn step_lines(v: &mut VideoTiming, count: usize) {
    for _ in 0..count {
        step_line(v);
    }
}

#[test]
fn the_frame_matches_the_rate_the_frontend_already_carries() {
    // 33.513982 MHz / 560190 cycles is 59.8261 Hz, which is what `frontend_core::platform`
    // reports for the DS. These two numbers must not drift apart.
    assert_eq!(CYCLES_PER_LINE, 2130);
    assert_eq!(CYCLES_PER_FRAME, 560_190);
    let rate = 33_513_982.0 / CYCLES_PER_FRAME as f64;
    assert!((rate - 59.8261).abs() < 0.001, "{rate} Hz");
}

#[test]
fn a_line_is_hblank_then_line_end() {
    let mut v = VideoTiming::new();
    assert_eq!(v.cycles_until_next_event(), HBLANK_CYCLE);
    assert_eq!(
        step_line(&mut v),
        [VideoEvent::HBlankStart, VideoEvent::LineEnd]
    );
    assert_eq!(v.line(), 1);
    assert_eq!(v.cycle_in_line(), 0);
}

#[test]
fn a_whole_frame_is_two_hundred_and_sixty_three_lines() {
    let mut v = VideoTiming::new();
    let mut cycles = 0u32;
    for _ in 0..LINES_PER_FRAME {
        loop {
            let budget = v.cycles_until_next_event();
            cycles += budget;
            if v.advance(budget) == Some(VideoEvent::LineEnd) {
                break;
            }
        }
    }
    assert_eq!(v.line(), 0, "wrapped back to the top");
    assert_eq!(cycles, CYCLES_PER_FRAME);
}

#[test]
fn the_hblank_flag_is_up_only_after_the_visible_dots() {
    let mut v = VideoTiming::new();
    assert!(!v.in_hblank());
    v.advance(v.cycles_until_next_event());
    assert!(v.in_hblank(), "at dot 256");
    v.advance(v.cycles_until_next_event());
    assert!(!v.in_hblank(), "and down again on the next line");
}

#[test]
fn the_vblank_flag_covers_lines_192_to_261() {
    let mut v = VideoTiming::new();
    assert!(!v.in_vblank());
    step_lines(&mut v, 192);
    assert_eq!(v.line(), 192);
    assert!(v.in_vblank());
    assert!(!v.is_visible_line());

    step_lines(&mut v, 69);
    assert_eq!(v.line(), 261);
    assert!(v.in_vblank());

    // Cleared on the last line, not at the wrap. That one-line difference is visible to software.
    step_lines(&mut v, 1);
    assert_eq!(v.line(), 262);
    assert!(!v.in_vblank());
}

#[test]
fn hblank_fires_on_every_line_including_during_vblank() {
    let mut v = VideoTiming::new();
    v.write16(ARM9, reg::DISPSTAT, 1 << 4);
    step_lines(&mut v, 200);
    let mut count = 0;
    for _ in 0..10 {
        step_line(&mut v);
        if v.take_pending(ARM9).hblank {
            count += 1;
        }
    }
    assert_eq!(count, 10, "the hblank interrupt does not stop in vblank");
}

#[test]
fn vblank_interrupts_once_per_frame_and_only_when_enabled() {
    let mut v = VideoTiming::new();
    step_lines(&mut v, 192);
    assert!(!v.take_pending(ARM9).vblank, "not enabled");

    v.write16(ARM9, reg::DISPSTAT, 1 << 3);
    let mut frames = 0;
    for _ in 0..LINES_PER_FRAME {
        step_line(&mut v);
        if v.take_pending(ARM9).vblank {
            frames += 1;
        }
    }
    assert_eq!(frames, 1);
}

#[test]
fn the_two_cores_have_independent_interrupt_enables() {
    let mut v = VideoTiming::new();
    v.write16(ARM9, reg::DISPSTAT, 1 << 3); // ARM9 wants vblank
    v.write16(ARM7, reg::DISPSTAT, 1 << 4); // ARM7 wants hblank
    step_lines(&mut v, 192);

    let nine = v.take_pending(ARM9);
    let seven = v.take_pending(ARM7);
    assert!(nine.vblank && !nine.hblank);
    assert!(seven.hblank && !seven.vblank);
}

#[test]
fn the_two_cores_have_independent_vcount_targets() {
    let mut v = VideoTiming::new();
    // ARM9 waits for line 100, ARM7 for line 50, both with the interrupt on.
    v.write16(ARM9, reg::DISPSTAT, (100 << 8) | (1 << 5));
    v.write16(ARM7, reg::DISPSTAT, (50 << 8) | (1 << 5));

    let mut nine_at = None;
    let mut seven_at = None;
    for _ in 0..LINES_PER_FRAME {
        step_line(&mut v);
        if v.take_pending(ARM9).vcount {
            nine_at = Some(v.line());
        }
        if v.take_pending(ARM7).vcount {
            seven_at = Some(v.line());
        }
    }
    assert_eq!(nine_at, Some(100));
    assert_eq!(seven_at, Some(50));
}

#[test]
fn the_vcount_target_is_nine_bits_split_across_the_register() {
    let mut v = VideoTiming::new();
    // Line 261 needs bit 8, which lives at bit 7 of DISPSTAT rather than next to the rest.
    v.write16(
        ARM9,
        reg::DISPSTAT,
        ((261 & 0xFF) << 8) | (1 << 7) | (1 << 5),
    );
    let mut matched = None;
    for _ in 0..LINES_PER_FRAME {
        step_line(&mut v);
        if v.take_pending(ARM9).vcount {
            matched = Some(v.line());
        }
    }
    assert_eq!(matched, Some(261));
}

#[test]
fn the_vcount_flag_tracks_the_line_and_reads_back() {
    let mut v = VideoTiming::new();
    v.write16(ARM9, reg::DISPSTAT, 5 << 8);
    assert_eq!(v.read16(ARM9, reg::DISPSTAT).unwrap() & (1 << 2), 0);
    step_lines(&mut v, 5);
    assert_eq!(v.line(), 5);
    assert_ne!(v.read16(ARM9, reg::DISPSTAT).unwrap() & (1 << 2), 0);
    step_lines(&mut v, 1);
    assert_eq!(v.read16(ARM9, reg::DISPSTAT).unwrap() & (1 << 2), 0);
}

#[test]
fn setting_the_target_to_the_current_line_flags_it_without_interrupting() {
    // Hardware compares on the line transition. Software that arms the match for a line already
    // showing must not get an interrupt for a transition that never happened.
    let mut v = VideoTiming::new();
    step_lines(&mut v, 10);
    v.take_pending(ARM9);
    v.write16(ARM9, reg::DISPSTAT, (10 << 8) | (1 << 5));
    assert_ne!(
        v.read16(ARM9, reg::DISPSTAT).unwrap() & (1 << 2),
        0,
        "flagged"
    );
    assert!(!v.take_pending(ARM9).vcount, "but not interrupted");
}

#[test]
fn the_status_flags_are_read_only() {
    let mut v = VideoTiming::new();
    v.write16(ARM9, reg::DISPSTAT, 0x0007);
    let value = v.read16(ARM9, reg::DISPSTAT).unwrap();
    assert_eq!(value & 0b011, 0, "vblank and hblank come from the counters");

    step_lines(&mut v, 192);
    assert_ne!(
        v.read16(ARM9, reg::DISPSTAT).unwrap() & 1,
        0,
        "and they appear without being written"
    );
}

#[test]
fn vcount_reads_the_line_counter() {
    let mut v = VideoTiming::new();
    assert_eq!(v.read16(ARM9, reg::VCOUNT), Some(0));
    step_lines(&mut v, 137);
    assert_eq!(v.read16(ARM9, reg::VCOUNT), Some(137));
    assert_eq!(v.read8(ARM9, reg::VCOUNT), Some(137));
    assert_eq!(v.read16(ARM9, 0x0400_0008), None);
}

#[test]
fn byte_writes_reach_the_target_and_the_enables() {
    let mut v = VideoTiming::new();
    v.write8(ARM9, reg::DISPSTAT + 1, 77);
    v.write8(ARM9, reg::DISPSTAT, 1 << 5);
    step_lines(&mut v, 77);
    assert!(v.take_pending(ARM9).vcount);
}

#[test]
fn the_frame_loop_never_gets_a_zero_budget() {
    let mut v = VideoTiming::new();
    for _ in 0..2000 {
        let budget = v.cycles_until_next_event();
        assert!(budget > 0);
        v.advance(budget);
    }
}

#[test]
fn timing_round_trips_through_a_save_state_mid_line() {
    use savestate::{decode_state, encode_state};

    let mut v = VideoTiming::new();
    v.write16(ARM9, reg::DISPSTAT, (60 << 8) | (1 << 5) | (1 << 3));
    step_lines(&mut v, 59);
    v.advance(v.cycles_until_next_event()); // into hblank of line 59

    let blob = encode_state("nds", 1, &v);
    let mut restored = VideoTiming::new();
    decode_state("nds", 1, &blob, &mut restored).unwrap();

    assert_eq!(restored.line(), 59);
    assert!(restored.in_hblank());
    assert_eq!(
        restored.cycles_until_next_event(),
        v.cycles_until_next_event()
    );
    step_line(&mut restored);
    assert!(restored.take_pending(ARM9).vcount, "the target survived");
}

#[test]
fn a_state_from_outside_the_frame_is_rejected() {
    use savestate::{decode_state, encode_state, StateWriter};

    let mut w = StateWriter::new();
    w.write_u16(LINES_PER_FRAME); // one past the last line
    w.write_u32(0);
    for _ in 0..2 {
        w.write_u16(0);
    }
    for _ in 0..6 {
        w.write_bool(false);
    }
    let blob = encode_state("nds", 1, &RawBlob(w.into_inner()));

    let mut restored = VideoTiming::new();
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
