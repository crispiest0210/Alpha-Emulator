use super::*;
use crate::irq::source;

/// Advance by exactly `cycles`, looping over `tick`'s now-clamped-to-one-line steps and merging
/// their events the way a single call used to: the last of each kind wins. Only valid for a test
/// that does not care which of several crossed lines a merged field describes — true of most of
/// them here, since a single call spanning at most one line has only one edge to merge in the
/// first place. A test about the collapsing behaviour itself drives the loop directly instead.
fn tick_all(video: &mut VideoTiming, mut cycles: u32) -> VideoEvents {
    let mut events = VideoEvents::default();
    while cycles > 0 {
        let (step_events, consumed) = video.tick(cycles);
        cycles -= consumed;
        if step_events.scanline_ready.is_some() {
            events.scanline_ready = step_events.scanline_ready;
        }
        events.entered_hblank |= step_events.entered_hblank;
        events.entered_vblank |= step_events.entered_vblank;
        events.frame_started |= step_events.frame_started;
        events.vcount_matched |= step_events.vcount_matched;
    }
    events
}

#[test]
fn a_frame_is_two_hundred_and_twenty_eight_lines_not_one_hundred_and_sixty() {
    // The 68 trailing lines are not a gap: they are when almost everything a game does to
    // video memory happens.
    let mut video = VideoTiming::new();
    let mut lines = 0;
    for _ in 0..LINES_PER_FRAME {
        tick_all(&mut video, CYCLES_PER_LINE);
        lines += 1;
    }
    assert_eq!(lines, 228);
    assert_eq!(video.vcount(), 0, "and it wrapped exactly");
}

#[test]
fn the_line_and_frame_lengths_multiply_out_to_the_documented_frame_cycle_count() {
    assert_eq!(CYCLES_PER_LINE, 1232);
    assert_eq!(CYCLES_PER_LINE * LINES_PER_FRAME, 280_896);
}

#[test]
fn horizontal_blanking_begins_after_the_visible_dots() {
    let mut video = VideoTiming::new();
    assert!(!video.in_hblank());
    tick_all(&mut video, HBLANK_START_CYCLE - 1);
    assert!(!video.in_hblank(), "still drawing the last visible dot");
    tick_all(&mut video, 1);
    assert!(video.in_hblank());
}

#[test]
fn entering_horizontal_blanking_is_reported_once_per_line() {
    let mut video = VideoTiming::new();
    let events = tick_all(&mut video, HBLANK_START_CYCLE);
    assert!(events.entered_hblank);
    assert_eq!(events.scanline_ready, Some(0));

    let events = tick_all(&mut video, 1);
    assert!(!events.entered_hblank, "the edge, not the level");
}

#[test]
fn a_scanline_is_reported_ready_only_while_the_beam_is_visible() {
    let mut video = VideoTiming::new();
    // Run to the last visible line.
    for _ in 0..SCREEN_HEIGHT {
        tick_all(&mut video, CYCLES_PER_LINE);
    }
    assert_eq!(video.vcount() as u32, SCREEN_HEIGHT);

    let events = tick_all(&mut video, HBLANK_START_CYCLE);
    assert!(events.entered_hblank, "HBlank still happens in VBlank");
    assert_eq!(events.scanline_ready, None, "but there is nothing to draw");
}

#[test]
fn vertical_blanking_begins_at_line_one_hundred_and_sixty() {
    let mut video = VideoTiming::new();
    for line in 0..SCREEN_HEIGHT {
        let events = tick_all(&mut video, CYCLES_PER_LINE);
        assert_eq!(
            events.entered_vblank,
            line + 1 == SCREEN_HEIGHT,
            "at line {line}"
        );
    }
    assert!(video.in_vblank());
}

#[test]
fn a_long_step_reports_every_scanline_it_crossed_rather_than_only_the_last() {
    // A DMA burst or a long instruction can cover more than a line, and collapsing that to one
    // edge silently drops scanlines. `tick` reports at most one edge per call and expects its
    // caller to loop — see `system::GbaSystemBus::advance` — so this drives that loop itself and
    // checks every one of the three lines it crosses comes back, not only the last.
    let mut video = VideoTiming::new();
    let mut remaining = CYCLES_PER_LINE * 3;
    let mut scanlines = Vec::new();
    let mut hblanks = 0;
    while remaining > 0 {
        let (events, consumed) = video.tick(remaining);
        remaining -= consumed;
        if events.entered_hblank {
            hblanks += 1;
        }
        if let Some(line) = events.scanline_ready {
            scanlines.push(line);
        }
    }
    assert_eq!(video.vcount(), 3);
    assert_eq!(hblanks, 3, "one per line crossed");
    assert_eq!(
        scanlines,
        vec![0, 1, 2],
        "every scanline, not only the last"
    );
}

#[test]
fn vcount_matched_fires_once_per_matching_line_crossed_in_a_multi_line_step() {
    // The same collapsing risk as `scanline_ready`, for the comparison interrupt, and a sharper
    // test of it: a step spanning two whole frames must report the match *twice*, once for each
    // time the matching line is crossed — a single merged flag could only ever say it happened at
    // least once, undercounting exactly the way a missed scanline would.
    let mut video = VideoTiming::new();
    video.write16(reg::DISPSTAT, 1 << 8); // match line 1

    let mut remaining = CYCLES_PER_LINE * (LINES_PER_FRAME + 3);
    let mut matches = 0;
    while remaining > 0 {
        let (events, consumed) = video.tick(remaining);
        remaining -= consumed;
        if events.vcount_matched {
            matches += 1;
        }
    }
    assert_eq!(matches, 2, "line 1 was crossed once in each of two frames");
}

#[test]
fn the_frame_start_edge_fires_when_vcount_wraps() {
    let mut video = VideoTiming::new();
    for _ in 0..LINES_PER_FRAME - 1 {
        assert!(!tick_all(&mut video, CYCLES_PER_LINE).frame_started);
    }
    assert!(tick_all(&mut video, CYCLES_PER_LINE).frame_started);
}

#[test]
fn the_vcount_comparison_fires_on_the_line_the_game_asked_for() {
    let mut video = VideoTiming::new();
    video.write16(reg::DISPSTAT, 5 << 8);

    for line in 1..8 {
        let events = tick_all(&mut video, CYCLES_PER_LINE);
        assert_eq!(events.vcount_matched, line == 5, "at line {line}");
    }
}

#[test]
fn a_write_to_dispstat_cannot_clear_the_hardware_flags() {
    // A plain store would let a game clear the VBlank flag, which hardware does not allow.
    let mut video = VideoTiming::new();
    for _ in 0..SCREEN_HEIGHT {
        tick_all(&mut video, CYCLES_PER_LINE);
    }
    assert_ne!(video.read16(reg::DISPSTAT).unwrap() & dispstat::VBLANK, 0);

    video.write16(reg::DISPSTAT, 0);
    assert_ne!(
        video.read16(reg::DISPSTAT).unwrap() & dispstat::VBLANK,
        0,
        "still in vertical blanking, whatever the game wrote"
    );
}

#[test]
fn the_status_flags_track_the_beam_rather_than_being_stored() {
    let mut video = VideoTiming::new();
    assert_eq!(video.read16(reg::DISPSTAT).unwrap() & dispstat::HBLANK, 0);
    tick_all(&mut video, HBLANK_START_CYCLE);
    assert_ne!(video.read16(reg::DISPSTAT).unwrap() & dispstat::HBLANK, 0);
    tick_all(&mut video, CYCLES_PER_LINE - HBLANK_START_CYCLE);
    assert_eq!(
        video.read16(reg::DISPSTAT).unwrap() & dispstat::HBLANK,
        0,
        "and it clears again on the next line"
    );
}

#[test]
fn vcount_is_read_only() {
    let mut video = VideoTiming::new();
    video.write16(reg::VCOUNT, 100);
    assert_eq!(video.vcount(), 0, "the beam did not move");
}

#[test]
fn only_the_enabled_video_interrupts_are_requested() {
    let mut video = VideoTiming::new();
    video.write16(reg::DISPSTAT, dispstat::VBLANK_IRQ);

    let mut sources = 0;
    for _ in 0..SCREEN_HEIGHT {
        let events = tick_all(&mut video, CYCLES_PER_LINE);
        sources |= video.interrupt_sources(&events);
    }
    assert_eq!(
        sources,
        source::VBLANK,
        "HBlank happened 160 times but nobody asked to hear about it"
    );
}

#[test]
fn enabling_the_hblank_interrupt_reports_it() {
    let mut video = VideoTiming::new();
    video.write16(reg::DISPSTAT, dispstat::HBLANK_IRQ);
    let events = tick_all(&mut video, HBLANK_START_CYCLE);
    assert_eq!(video.interrupt_sources(&events), source::HBLANK);
}

#[test]
fn the_bitmap_frame_bit_selects_the_second_buffer() {
    // Double buffering is why modes 4 and 5 have two frames at all: draw into the hidden one
    // and flip a single bit.
    let mut video = VideoTiming::new();
    assert_eq!(video.bitmap_frame_offset(), 0);
    video.write16(reg::DISPCNT, dispcnt::FRAME_SELECT);
    assert_eq!(video.bitmap_frame_offset(), 0xA000);
}

#[test]
fn the_mode_comes_from_the_low_three_bits_of_dispcnt() {
    let mut video = VideoTiming::new();
    for mode in 0..6 {
        video.write16(reg::DISPCNT, mode);
        assert_eq!(video.mode(), mode);
    }
}

#[test]
fn video_state_round_trips_mid_scanline() {
    use savestate::{decode_state, encode_state};
    let mut video = VideoTiming::new();
    video.write16(reg::DISPCNT, 3 | dispcnt::BG2);
    video.write16(reg::DISPSTAT, dispstat::VBLANK_IRQ | (90 << 8));
    tick_all(&mut video, CYCLES_PER_LINE * 40 + 700);

    let bytes = encode_state("gba-video", 1, &video);
    let mut restored = VideoTiming::new();
    decode_state("gba-video", 1, &bytes, &mut restored).unwrap();
    assert_eq!(restored, video);

    // And it resumes mid-line rather than snapping to a boundary.
    assert_eq!(
        tick_all(&mut restored, CYCLES_PER_LINE).scanline_ready,
        tick_all(&mut video, CYCLES_PER_LINE).scanline_ready
    );
}
