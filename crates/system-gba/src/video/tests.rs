use super::*;
use crate::irq::source;

#[test]
fn a_frame_is_two_hundred_and_twenty_eight_lines_not_one_hundred_and_sixty() {
    // The 68 trailing lines are not a gap: they are when almost everything a game does to
    // video memory happens.
    let mut video = VideoTiming::new();
    let mut lines = 0;
    for _ in 0..LINES_PER_FRAME {
        video.tick(CYCLES_PER_LINE);
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
    video.tick(HBLANK_START_CYCLE - 1);
    assert!(!video.in_hblank(), "still drawing the last visible dot");
    video.tick(1);
    assert!(video.in_hblank());
}

#[test]
fn entering_horizontal_blanking_is_reported_once_per_line() {
    let mut video = VideoTiming::new();
    let events = video.tick(HBLANK_START_CYCLE);
    assert!(events.entered_hblank);
    assert_eq!(events.scanline_ready, Some(0));

    let events = video.tick(1);
    assert!(!events.entered_hblank, "the edge, not the level");
}

#[test]
fn a_scanline_is_reported_ready_only_while_the_beam_is_visible() {
    let mut video = VideoTiming::new();
    // Run to the last visible line.
    for _ in 0..SCREEN_HEIGHT {
        video.tick(CYCLES_PER_LINE);
    }
    assert_eq!(video.vcount() as u32, SCREEN_HEIGHT);

    let events = video.tick(HBLANK_START_CYCLE);
    assert!(events.entered_hblank, "HBlank still happens in VBlank");
    assert_eq!(events.scanline_ready, None, "but there is nothing to draw");
}

#[test]
fn vertical_blanking_begins_at_line_one_hundred_and_sixty() {
    let mut video = VideoTiming::new();
    for line in 0..SCREEN_HEIGHT {
        let events = video.tick(CYCLES_PER_LINE);
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
    // edge silently drops scanlines.
    let mut video = VideoTiming::new();
    let events = video.tick(CYCLES_PER_LINE * 3 + HBLANK_START_CYCLE);
    assert_eq!(video.vcount(), 3);
    assert!(events.entered_hblank);
    assert_eq!(events.scanline_ready, Some(3), "the most recent one");
}

#[test]
fn the_frame_start_edge_fires_when_vcount_wraps() {
    let mut video = VideoTiming::new();
    for _ in 0..LINES_PER_FRAME - 1 {
        assert!(!video.tick(CYCLES_PER_LINE).frame_started);
    }
    assert!(video.tick(CYCLES_PER_LINE).frame_started);
}

#[test]
fn the_vcount_comparison_fires_on_the_line_the_game_asked_for() {
    let mut video = VideoTiming::new();
    video.write16(reg::DISPSTAT, 5 << 8);

    for line in 1..8 {
        let events = video.tick(CYCLES_PER_LINE);
        assert_eq!(events.vcount_matched, line == 5, "at line {line}");
    }
}

#[test]
fn a_write_to_dispstat_cannot_clear_the_hardware_flags() {
    // A plain store would let a game clear the VBlank flag, which hardware does not allow.
    let mut video = VideoTiming::new();
    for _ in 0..SCREEN_HEIGHT {
        video.tick(CYCLES_PER_LINE);
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
    video.tick(HBLANK_START_CYCLE);
    assert_ne!(video.read16(reg::DISPSTAT).unwrap() & dispstat::HBLANK, 0);
    video.tick(CYCLES_PER_LINE - HBLANK_START_CYCLE);
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
        let events = video.tick(CYCLES_PER_LINE);
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
    let events = video.tick(HBLANK_START_CYCLE);
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
    video.tick(CYCLES_PER_LINE * 40 + 700);

    let bytes = encode_state("gba-video", 1, &video);
    let mut restored = VideoTiming::new();
    decode_state("gba-video", 1, &bytes, &mut restored).unwrap();
    assert_eq!(restored, video);

    // And it resumes mid-line rather than snapping to a boundary.
    assert_eq!(
        restored.tick(CYCLES_PER_LINE).scanline_ready,
        video.tick(CYCLES_PER_LINE).scanline_ready
    );
}
