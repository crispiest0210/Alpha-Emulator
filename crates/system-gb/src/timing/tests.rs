//! Tests for the scheduler wiring.
//!
//! The timer quirks here are the ones Mooneye's suite covers directly. They are edge cases
//! only in the sense that they are rarely hit deliberately — games hit them constantly by
//! accident, and getting them wrong shows up as music running at the wrong tempo.

use super::*;

/// Run the timing forward in one jump, as if the CPU had executed for that long.
fn advance(timing: &mut GbTiming, cycles: u64) -> TimingOutput {
    let target = timing.now() + Cycles(cycles);
    timing.advance_to(target)
}

/// Run forward in event-bounded slices, the way the real frame loop does, accumulating
/// everything that fires.
fn run_sliced(timing: &mut GbTiming, cycles: u64) -> TimingOutput {
    let deadline = timing.now() + Cycles(cycles);
    let mut total = TimingOutput::default();
    while timing.now() < deadline {
        let slice = timing
            .cycles_until_next_event()
            .min(deadline - timing.now());
        let target = timing.now() + slice;
        let out = timing.advance_to(target);
        total.interrupts |= out.interrupts;
        total.frame_ready |= out.frame_ready;
        total.frame_started |= out.frame_started;
        // First one wins: these name a line, and a caller running past several of them wants
        // the one it stepped up to, not the last of the batch.
        total.line_started = total.line_started.or(out.line_started);
        total.drawing_started = total.drawing_started.or(out.drawing_started);
        total.scanline_ready = total.scanline_ready.or(out.scanline_ready);
        total.apu_length_clocks += out.apu_length_clocks;
        total.apu_envelope_clocks += out.apu_envelope_clocks;
        total.apu_sweep_clocks += out.apu_sweep_clocks;
    }
    total
}

fn timing() -> GbTiming {
    GbTiming::new()
}

/// Enable the timer at the given `TAC` frequency selector.
fn start_timer(timing: &mut GbTiming, selector: u8) {
    timing.write_register(reg::TAC, 0b100 | selector);
}

// ---------------------------------------------------------------------------
// DIV
// ---------------------------------------------------------------------------

#[test]
fn div_is_the_top_byte_of_a_counter_running_at_the_clock_rate() {
    let mut t = timing();
    assert_eq!(t.read_register(reg::DIV), Some(0));

    // The counter increments every t-cycle, so DIV increments every 256.
    advance(&mut t, 255);
    assert_eq!(t.read_register(reg::DIV), Some(0));
    advance(&mut t, 1);
    assert_eq!(t.read_register(reg::DIV), Some(1));

    advance(&mut t, 256 * 10);
    assert_eq!(t.read_register(reg::DIV), Some(11));
}

#[test]
fn writing_div_zeroes_it_whatever_the_value_written() {
    let mut t = timing();
    advance(&mut t, 5000);
    assert_ne!(t.read_register(reg::DIV), Some(0));

    t.write_register(reg::DIV, 0xAB);
    assert_eq!(t.read_register(reg::DIV), Some(0));
}

// ---------------------------------------------------------------------------
// TIMA
// ---------------------------------------------------------------------------

#[test]
fn tima_increments_at_each_tac_frequency() {
    // (selector, cycles per increment)
    for (selector, period) in [(0b00u8, 1024u64), (0b01, 16), (0b10, 64), (0b11, 256)] {
        let mut t = timing();
        start_timer(&mut t, selector);

        run_sliced(&mut t, period - 1);
        assert_eq!(
            t.read_register(reg::TIMA),
            Some(0),
            "selector {selector:02b}"
        );
        run_sliced(&mut t, 1);
        assert_eq!(
            t.read_register(reg::TIMA),
            Some(1),
            "selector {selector:02b}"
        );

        run_sliced(&mut t, period * 4);
        assert_eq!(
            t.read_register(reg::TIMA),
            Some(5),
            "selector {selector:02b}"
        );
    }
}

#[test]
fn a_disabled_timer_does_not_count() {
    let mut t = timing();
    t.write_register(reg::TAC, 0b000); // frequency selected, enable clear
    run_sliced(&mut t, 10_000);
    assert_eq!(t.read_register(reg::TIMA), Some(0));
}

#[test]
fn tima_overflow_reloads_from_tma_and_raises_the_timer_interrupt() {
    let mut t = timing();
    t.write_register(reg::TMA, 0x42);
    t.write_register(reg::TIMA, 0xFF);
    start_timer(&mut t, 0b01); // every 16 cycles

    let out = run_sliced(&mut t, 16);
    assert_eq!(
        t.read_register(reg::TIMA),
        Some(0),
        "TIMA reads zero during the reload delay"
    );
    assert_eq!(out.interrupts & interrupt::TIMER, 0, "the interrupt waits");

    let out = run_sliced(&mut t, 4);
    assert_eq!(t.read_register(reg::TIMA), Some(0x42), "TMA is loaded");
    assert_ne!(
        out.interrupts & interrupt::TIMER,
        0,
        "and the interrupt fires"
    );
}

#[test]
fn writing_tima_during_the_reload_delay_cancels_the_reload() {
    // Software that catches the four-cycle window and writes TIMA keeps its own value, and
    // no interrupt fires at all.
    let mut t = timing();
    t.write_register(reg::TMA, 0x42);
    t.write_register(reg::TIMA, 0xFF);
    start_timer(&mut t, 0b01);

    run_sliced(&mut t, 16);
    assert_eq!(t.read_register(reg::TIMA), Some(0));

    t.write_register(reg::TIMA, 0x7F);
    let out = run_sliced(&mut t, 8);
    assert_eq!(t.read_register(reg::TIMA), Some(0x7F), "the write stands");
    assert_eq!(
        out.interrupts & interrupt::TIMER,
        0,
        "and the cancelled reload raises nothing"
    );
}

#[test]
fn writing_tma_during_the_reload_delay_supplies_the_reloaded_value() {
    let mut t = timing();
    t.write_register(reg::TMA, 0x42);
    t.write_register(reg::TIMA, 0xFF);
    start_timer(&mut t, 0b01);

    run_sliced(&mut t, 16);
    // The new modulo takes effect for this reload, not the next one.
    t.write_register(reg::TMA, 0x99);
    run_sliced(&mut t, 4);
    assert_eq!(t.read_register(reg::TIMA), Some(0x99));
}

// ---------------------------------------------------------------------------
// The falling-edge quirks
// ---------------------------------------------------------------------------

#[test]
fn resetting_div_while_the_selected_bit_is_high_increments_tima() {
    // The classic quirk: a write to a divider register bumps an unrelated timer, because
    // both hang off the same counter and zeroing it produces a falling edge.
    let mut t = timing();
    start_timer(&mut t, 0b11); // bit 7, every 256 cycles

    // Get the counter to a point where bit 7 is set.
    run_sliced(&mut t, 130);
    assert_eq!(t.read_register(reg::TIMA), Some(0), "no increment yet");

    t.write_register(reg::DIV, 0);
    assert_eq!(
        t.read_register(reg::TIMA),
        Some(1),
        "zeroing the counter dropped bit 7, which is a falling edge"
    );
}

#[test]
fn resetting_div_while_the_selected_bit_is_low_does_nothing() {
    let mut t = timing();
    start_timer(&mut t, 0b11); // bit 7

    run_sliced(&mut t, 10); // bit 7 still clear
    t.write_register(reg::DIV, 0);
    assert_eq!(t.read_register(reg::TIMA), Some(0));
}

#[test]
fn changing_tac_onto_a_low_bit_increments_tima() {
    let mut t = timing();
    start_timer(&mut t, 0b11); // bit 7
    run_sliced(&mut t, 130); // bit 7 high, bit 3 low at cycle 130 (130 & 8 == 0)

    // Moving the multiplexer from a high bit to a low one is a falling edge.
    t.write_register(reg::TAC, 0b100 | 0b01); // now bit 3
    assert_eq!(t.read_register(reg::TIMA), Some(1));
}

#[test]
fn disabling_the_timer_while_its_bit_is_high_increments_tima() {
    // The enable gates the multiplexer output, so clearing it also produces a falling edge.
    let mut t = timing();
    start_timer(&mut t, 0b11); // bit 7
    run_sliced(&mut t, 130);

    t.write_register(reg::TAC, 0b000);
    assert_eq!(t.read_register(reg::TIMA), Some(1));

    // And with the timer off it stops counting.
    run_sliced(&mut t, 10_000);
    assert_eq!(t.read_register(reg::TIMA), Some(1));
}

#[test]
fn tac_reads_back_with_its_unused_bits_set() {
    let mut t = timing();
    t.write_register(reg::TAC, 0x05);
    assert_eq!(t.read_register(reg::TAC), Some(0xFD));
}

// ---------------------------------------------------------------------------
// PPU mode timing
// ---------------------------------------------------------------------------

#[test]
fn a_scanline_runs_through_the_three_visible_modes_in_order() {
    let mut t = timing();
    assert_eq!(t.ppu.mode, PpuMode::OamScan);
    assert_eq!(t.ppu.ly, 0);

    run_sliced(&mut t, OAM_SCAN_CYCLES);
    assert_eq!(t.ppu.mode, PpuMode::Drawing);

    run_sliced(&mut t, MIN_DRAWING_CYCLES);
    assert_eq!(t.ppu.mode, PpuMode::HBlank);

    run_sliced(&mut t, MAX_HBLANK_CYCLES);
    assert_eq!(t.ppu.mode, PpuMode::OamScan, "and on to the next line");
    assert_eq!(t.ppu.ly, 1);
}

/// Step to the start of mode 3 on the current line, answering `drawing_started` with `length`.
fn enter_drawing(timing: &mut GbTiming, length: u64) {
    let out = run_sliced(timing, OAM_SCAN_CYCLES);
    assert_eq!(out.drawing_started, Some(timing.ppu.ly));
    assert_eq!(timing.ppu.mode, PpuMode::Drawing);
    timing.set_mode3_length(length);
}

#[test]
fn mode_three_is_scheduled_at_its_minimum_and_the_system_corrects_it() {
    // The scheduler cannot see SCX, the window, or OAM, so it books the shortest mode 3 there
    // is and the system replaces it in the same cycle.
    let mut t = timing();
    let out = run_sliced(&mut t, OAM_SCAN_CYCLES);
    assert_eq!(out.drawing_started, Some(0), "the mode reports itself");
    assert_eq!(t.mode3_length(), MIN_DRAWING_CYCLES);

    t.set_mode3_length(MIN_DRAWING_CYCLES + 40);
    assert_eq!(t.mode3_length(), MIN_DRAWING_CYCLES + 40);

    // The correction is measured from where mode 3 started, so mode 0 begins exactly 40 cycles
    // later than it would have — not 40 cycles after wherever the clock had reached.
    run_sliced(&mut t, MIN_DRAWING_CYCLES + 39);
    assert_eq!(t.ppu.mode, PpuMode::Drawing, "still drawing");
    run_sliced(&mut t, 1);
    assert_eq!(t.ppu.mode, PpuMode::HBlank);
}

#[test]
fn a_longer_mode_three_comes_out_of_mode_zero_not_the_line() {
    // The line is 456 cycles whatever the fetcher does. Every cycle mode 3 gains, mode 0 loses.
    for length in [
        MIN_DRAWING_CYCLES,
        MIN_DRAWING_CYCLES + 1,
        MIN_DRAWING_CYCLES + 63,
        crate::ppu::MODE3_MAX_CYCLES,
    ] {
        let mut t = timing();
        let start = t.now();
        enter_drawing(&mut t, length);

        // To the end of mode 3.
        run_sliced(&mut t, length);
        assert_eq!(t.ppu.mode, PpuMode::HBlank, "length {length}");
        assert_eq!(
            (t.now() - start).get(),
            OAM_SCAN_CYCLES + length,
            "mode 0 begins {length} cycles into the line"
        );

        // And on to the next line, which still starts on the 456-cycle grid.
        run_sliced(&mut t, LINE_CYCLES - OAM_SCAN_CYCLES - length);
        assert_eq!(t.ppu.ly, 1, "length {length}");
        assert_eq!(t.ppu.mode, PpuMode::OamScan);
        assert_eq!((t.now() - start).get(), LINE_CYCLES);
    }
}

#[test]
fn a_longer_mode_three_delays_the_hblank_stat_interrupt() {
    // This is the whole point of the exercise. A game rewriting SCX from an HBlank interrupt
    // is depending on that interrupt landing where hardware puts it; a mode 0 that starts up
    // to 117 cycles early hands the write to the wrong line.
    let mut t = timing();
    t.write_register(reg::LYC, 0xFF); // keep coincidence out of it
    t.write_register(reg::STAT, 0x08); // HBlank source only
    enter_drawing(&mut t, MIN_DRAWING_CYCLES + 20);

    let out = run_sliced(&mut t, MIN_DRAWING_CYCLES);
    assert_eq!(
        out.interrupts & interrupt::LCD_STAT,
        0,
        "the unpenalised mode 3 would have ended here"
    );
    let out = run_sliced(&mut t, 20);
    assert_ne!(out.interrupts & interrupt::LCD_STAT, 0, "20 cycles later");
}

#[test]
fn mode_three_lengths_outside_what_hardware_can_do_are_clamped() {
    // A mode 3 longer than the line would push mode 0 past the end of it and desynchronise LY
    // from the frame, so the length is capped rather than trusted.
    let mut t = timing();
    enter_drawing(&mut t, 10_000);
    assert_eq!(t.mode3_length(), crate::ppu::MODE3_MAX_CYCLES);

    let mut t = timing();
    enter_drawing(&mut t, 0);
    assert_eq!(t.mode3_length(), MIN_DRAWING_CYCLES);
}

#[test]
fn setting_a_mode_three_length_outside_mode_three_does_nothing() {
    // The length belongs to the line being drawn. Arriving late — during mode 0, say — it
    // would move an event that is no longer mode 3's.
    let mut t = timing();
    run_sliced(&mut t, OAM_SCAN_CYCLES + MIN_DRAWING_CYCLES);
    assert_eq!(t.ppu.mode, PpuMode::HBlank);
    let before = t.cycles_until_next_event();
    t.set_mode3_length(MIN_DRAWING_CYCLES + 50);
    assert_eq!(t.cycles_until_next_event(), before);
}

#[test]
fn every_line_reports_its_own_start_and_drawing_period() {
    // The window's WY latch is sampled at the start of a line and nowhere else, so a line that
    // does not report itself would silently lose its window.
    let mut t = timing();
    let mut starts = Vec::new();
    let mut draws = Vec::new();
    let deadline = t.now() + Cycles(FRAME_CYCLES);
    while t.now() < deadline {
        let slice = t.cycles_until_next_event().min(deadline - t.now());
        let target = t.now() + slice;
        let out = t.advance_to(target);
        starts.extend(out.line_started);
        draws.extend(out.drawing_started);
    }
    // Line 0 of the first frame is the one line no event begins: the PPU is already in mode 2
    // when the machine powers on, so the system samples it directly. Every other visible line
    // is here, and the frame wrap re-reports line 0 for the next frame.
    assert_eq!(starts, (1..VISIBLE_LINES).chain([0]).collect::<Vec<_>>());
    assert_eq!(draws, (0..VISIBLE_LINES).collect::<Vec<_>>());
}

#[test]
fn stats_mode_field_trails_the_mode_change_by_one_machine_cycle() {
    // The interrupt and the flag are not the same event. Mooneye's `intr_2_mode0_timing` reads
    // the flag and `intr_2_0_timing` counts from the interrupt, and the two disagree by exactly
    // this — which is the only reason to believe the lag is real rather than an off-by-four in
    // a mode length.
    let mut t = timing();
    let mode_of = |t: &GbTiming| t.read_register(reg::STAT).unwrap() & 0x03;

    assert_eq!(mode_of(&t), PpuMode::OamScan as u8, "power-on has no lag");

    // Mode 2 -> 3.
    advance(&mut t, OAM_SCAN_CYCLES);
    assert_eq!(t.ppu.mode, PpuMode::Drawing);
    assert_eq!(mode_of(&t), PpuMode::OamScan as u8, "STAT still says 2");
    advance(&mut t, STAT_MODE_LAG_CYCLES);
    assert_eq!(mode_of(&t), PpuMode::Drawing as u8);

    // Mode 3 -> 0, and its interrupt, which is *not* delayed.
    let out = advance(&mut t, MIN_DRAWING_CYCLES - STAT_MODE_LAG_CYCLES);
    assert_eq!(t.ppu.mode, PpuMode::HBlank);
    assert_eq!(mode_of(&t), PpuMode::Drawing as u8, "STAT still says 3");
    assert_eq!(
        out.scanline_ready,
        Some(0),
        "while the line is already done"
    );
    advance(&mut t, STAT_MODE_LAG_CYCLES);
    assert_eq!(mode_of(&t), PpuMode::HBlank as u8);
}

#[test]
fn ly_reads_153_for_one_machine_cycle_and_then_reads_zero() {
    // The last line of the frame is the only one whose LY does not last the whole line. Games
    // put raster splits on `LYC = 0` and would get them a line early without this.
    let mut t = timing();
    let ly = |t: &GbTiming| t.read_register(reg::LY).unwrap();

    // To the start of line 153.
    run_sliced(&mut t, LINE_CYCLES * 153);
    assert_eq!(ly(&t), 153);
    assert_eq!(t.ppu.mode, PpuMode::VBlank);

    run_sliced(&mut t, LY_153_CYCLES);
    assert_eq!(ly(&t), 0, "LY rolls over while the line runs on");
    assert_eq!(
        t.ppu.mode,
        PpuMode::VBlank,
        "still line 153, still in VBlank"
    );

    // The rest of line 153 still belongs to line 153: mode 2 does not start early.
    run_sliced(&mut t, LINE_CYCLES - LY_153_CYCLES - 1);
    assert_eq!(t.ppu.mode, PpuMode::VBlank);
    let out = run_sliced(&mut t, 1);
    assert_eq!(t.ppu.mode, PpuMode::OamScan, "and the frame begins on time");
    assert!(out.frame_started);
    assert_eq!(ly(&t), 0);
}

#[test]
fn a_lyc_zero_interrupt_arrives_on_line_153_not_on_line_0() {
    // The consequence games actually use, and the reason the rollover is worth modelling: with
    // `LYC = 0` the coincidence interrupt arrives most of a line before the frame does.
    let mut t = timing();
    t.write_register(reg::LYC, 0);
    t.write_register(reg::STAT, 0x40); // coincidence source only

    // Up to the start of line 153, by which point line 0's coincidence is long over.
    run_sliced(&mut t, LINE_CYCLES * 153);
    assert_eq!(t.read_register(reg::LY), Some(153));

    let out = run_sliced(&mut t, LY_153_CYCLES);
    assert_ne!(
        out.interrupts & interrupt::LCD_STAT,
        0,
        "LY rolled to 0 while line 153 still had 452 cycles to run"
    );

    // And not again when the frame actually starts: the condition never dropped in between, so
    // there is no second rising edge. One interrupt, a line early — which is the whole point.
    let out = run_sliced(&mut t, LINE_CYCLES - LY_153_CYCLES);
    assert_eq!(out.interrupts & interrupt::LCD_STAT, 0);
    assert!(t.ppu.ly == 0 && t.ppu.mode == PpuMode::OamScan);
}

#[test]
fn the_mode_lengths_sum_to_a_scanline() {
    assert_eq!(
        OAM_SCAN_CYCLES + MIN_DRAWING_CYCLES + MAX_HBLANK_CYCLES,
        LINE_CYCLES
    );
    assert_eq!(LINE_CYCLES * TOTAL_LINES as u64, FRAME_CYCLES);
}

#[test]
fn ly_advances_one_line_per_456_cycles_across_the_whole_frame() {
    let mut t = timing();
    for line in 0..TOTAL_LINES {
        assert_eq!(t.ppu.ly, line, "at line {line}");
        run_sliced(&mut t, LINE_CYCLES);
    }
    assert_eq!(t.ppu.ly, 0, "and wraps back to the top");
    assert_eq!(t.now(), Cycles(FRAME_CYCLES));
}

#[test]
fn vblank_begins_at_line_144_and_raises_its_interrupt() {
    let mut t = timing();
    let out = run_sliced(&mut t, LINE_CYCLES * VISIBLE_LINES as u64);

    assert_eq!(t.ppu.ly, VISIBLE_LINES);
    assert_eq!(t.ppu.mode, PpuMode::VBlank);
    assert_ne!(out.interrupts & interrupt::VBLANK, 0);
    assert!(out.frame_ready, "the frame is complete at VBlank");
}

#[test]
fn the_ppu_stays_in_vblank_for_ten_lines() {
    let mut t = timing();
    run_sliced(&mut t, LINE_CYCLES * VISIBLE_LINES as u64);
    for line in VISIBLE_LINES..TOTAL_LINES {
        assert_eq!(t.ppu.ly, line);
        assert_eq!(t.ppu.mode, PpuMode::VBlank, "line {line}");
        run_sliced(&mut t, LINE_CYCLES);
    }
    assert_eq!(t.ppu.mode, PpuMode::OamScan);
}

#[test]
fn stat_reports_the_mode_and_coincidence_with_bit_seven_set() {
    let mut t = timing();
    // At reset LY and LYC are both zero, so coincidence is genuinely set.
    assert_eq!(
        t.read_register(reg::STAT),
        Some(0x80 | 0x04 | 2),
        "bit 7 set, coincidence, mode 2"
    );

    t.write_register(reg::LYC, 0);
    assert_eq!(
        t.read_register(reg::STAT).unwrap() & 0x04,
        0x04,
        "LY == LYC sets the coincidence bit"
    );

    t.write_register(reg::LYC, 5);
    assert_eq!(t.read_register(reg::STAT).unwrap() & 0x04, 0);
}

#[test]
fn writing_stat_touches_only_the_source_selects() {
    let mut t = timing();
    t.write_register(reg::STAT, 0xFF);
    let stat = t.read_register(reg::STAT).unwrap();
    assert_eq!(stat & 0x78, 0x78, "the selects took the write");
    assert_eq!(stat & 0x03, 2, "but the mode is still read-only");
}

#[test]
fn the_stat_interrupt_fires_on_a_rising_edge_not_once_per_source() {
    // STAT blocking: with both HBlank and OAM selected, consecutive modes assert the same
    // line continuously, so only the first transition raises an interrupt.
    let mut t = timing();
    t.write_register(reg::LYC, 0xFF); // keep coincidence out of it
    t.write_register(reg::STAT, 0x08); // HBlank source only

    // Reach the first HBlank.
    let out = run_sliced(&mut t, OAM_SCAN_CYCLES + MIN_DRAWING_CYCLES);
    assert_eq!(t.ppu.mode, PpuMode::HBlank);
    assert_ne!(out.interrupts & interrupt::LCD_STAT, 0, "rising edge");

    // The next HBlank is a fresh edge because OAM scan and drawing dropped the line between.
    let out = run_sliced(&mut t, LINE_CYCLES);
    assert_ne!(out.interrupts & interrupt::LCD_STAT, 0);
}

#[test]
fn a_coincidence_interrupt_fires_when_the_line_matches() {
    let mut t = timing();
    t.write_register(reg::STAT, 0x40); // coincidence source only
    t.write_register(reg::LYC, 3);

    let out = run_sliced(&mut t, LINE_CYCLES * 2);
    assert_eq!(out.interrupts & interrupt::LCD_STAT, 0, "not yet");

    let out = run_sliced(&mut t, LINE_CYCLES);
    assert_eq!(t.ppu.ly, 3);
    assert_ne!(out.interrupts & interrupt::LCD_STAT, 0);
}

#[test]
fn turning_the_lcd_off_parks_the_ppu_and_turning_it_on_restarts_the_frame() {
    let mut t = timing();
    run_sliced(&mut t, LINE_CYCLES * 10 + 100);
    assert_eq!(t.ppu.ly, 10);

    t.write_register(reg::LCDC, 0x00);
    assert_eq!(t.read_register(reg::LY), Some(0), "LY reads zero when off");
    assert_eq!(t.read_register(reg::STAT).unwrap() & 3, 0, "and mode zero");

    // Nothing advances while it is off.
    run_sliced(&mut t, LINE_CYCLES * 5);
    assert_eq!(t.read_register(reg::LY), Some(0));

    t.write_register(reg::LCDC, 0x80);
    assert_eq!(
        t.ppu.mode,
        PpuMode::OamScan,
        "restarts at the top of a frame"
    );
    run_sliced(&mut t, OAM_SCAN_CYCLES);
    assert_eq!(t.ppu.mode, PpuMode::Drawing, "and is running again");
}

#[test]
fn writing_ly_resets_the_line_counter() {
    let mut t = timing();
    run_sliced(&mut t, LINE_CYCLES * 7);
    assert_eq!(t.ppu.ly, 7);
    t.write_register(reg::LY, 100);
    assert_eq!(t.ppu.ly, 0, "a write resets rather than sets");
}

// ---------------------------------------------------------------------------
// APU frame sequencer
// ---------------------------------------------------------------------------

#[test]
fn the_sequencer_steps_every_8192_cycles() {
    let mut t = timing();
    run_sliced(&mut t, 8191);
    assert_eq!(t.apu.step, 0);
    run_sliced(&mut t, 1);
    assert_eq!(t.apu.step, 1);

    run_sliced(&mut t, 8192 * 7);
    assert_eq!(t.apu.step, 0, "eight steps make one cycle of the sequence");
}

#[test]
fn the_sequence_clocks_length_envelope_and_sweep_on_the_right_steps() {
    let expected = [
        // (step, length, sweep, envelope)
        (0u8, true, false, false),
        (1, false, false, false),
        (2, true, true, false),
        (3, false, false, false),
        (4, true, false, false),
        (5, false, false, false),
        (6, true, true, false),
        (7, false, false, true),
    ];
    for (step, length, sweep, envelope) in expected {
        let clocks = ApuSequencer::clocks_for(step);
        assert_eq!(clocks.length, length, "step {step} length");
        assert_eq!(clocks.sweep, sweep, "step {step} sweep");
        assert_eq!(clocks.envelope, envelope, "step {step} envelope");
    }
}

#[test]
fn one_full_sequence_reports_the_expected_clock_counts() {
    let mut t = timing();
    let out = run_sliced(&mut t, 8192 * 8);
    assert_eq!(out.apu_length_clocks, 4, "length clocks at 256 Hz");
    assert_eq!(out.apu_sweep_clocks, 2, "sweep at 128 Hz");
    assert_eq!(out.apu_envelope_clocks, 1, "envelope at 64 Hz");
}

#[test]
fn resetting_div_can_also_step_the_sequencer() {
    // The sequencer hangs off the same counter as the timer, so a game resetting DIV in a
    // loop can shorten notes. Deriving it from the divider is what reproduces that.
    let mut t = timing();
    run_sliced(&mut t, 5000); // bit 12 is high past 4096
    assert_eq!(t.apu.step, 0);

    t.write_register(reg::DIV, 0);
    assert_eq!(t.apu.step, 1, "zeroing the counter dropped bit 12");
}

// ---------------------------------------------------------------------------
// Overshoot and rescheduling
// ---------------------------------------------------------------------------

#[test]
fn an_event_scheduled_in_the_past_still_fires() {
    // Instructions overshoot slice boundaries constantly; a late event must still be
    // serviced rather than skipped.
    let mut t = timing();
    start_timer(&mut t, 0b01); // every 16 cycles

    // Jump well past several increments in one go, as a long instruction would.
    advance(&mut t, 100);
    assert_eq!(
        t.read_register(reg::TIMA),
        Some(6),
        "every missed increment fired"
    );
}

#[test]
fn recurring_events_do_not_drift_when_instructions_overshoot() {
    // Rescheduling from the event's own timestamp rather than from `now` is what keeps this
    // exact. Rescheduling from `now` would stretch every line by the overshoot.
    let mut ragged = timing();
    let mut exact = timing();

    // Advance one in awkward 7-cycle steps, the other in clean single cycles.
    let total = LINE_CYCLES * 20;
    let mut elapsed = 0;
    while elapsed < total {
        let step = 7.min(total - elapsed);
        advance(&mut ragged, step);
        elapsed += step;
    }
    run_sliced(&mut exact, total);

    assert_eq!(ragged.now(), exact.now());
    assert_eq!(
        ragged.ppu.ly, exact.ppu.ly,
        "the line counter did not drift with the ragged stepping"
    );
    assert_eq!(ragged.ppu.mode, exact.ppu.mode);
    assert_eq!(ragged.apu.step, exact.apu.step);
}

#[test]
fn the_slice_bound_never_reaches_zero() {
    // A zero-length slice would let the frame loop spin without the CPU making progress.
    let mut t = timing();
    for _ in 0..200 {
        let slice = t.cycles_until_next_event();
        assert!(slice > Cycles::ZERO);
        let target = t.now() + slice;
        t.advance_to(target);
    }
}

// ---------------------------------------------------------------------------
// Determinism
// ---------------------------------------------------------------------------

#[test]
fn two_runs_from_the_same_state_produce_identical_event_traces() {
    // The property save states, rewind, and the accuracy harness all rest on.
    fn run() -> Vec<(Cycles, GbEvent)> {
        let mut t = timing();
        t.trace = Some(Vec::new());
        t.write_register(reg::TAC, 0b101);
        t.write_register(reg::TMA, 0x30);
        t.write_register(reg::STAT, 0x48);
        t.write_register(reg::LYC, 42);

        // Step in an irregular but reproducible pattern, as instructions would.
        let mut step = 3u64;
        while t.now() < Cycles(FRAME_CYCLES * 2) {
            advance(&mut t, step);
            step = (step * 7 + 1) % 29 + 1;
        }
        t.trace.take().unwrap()
    }

    let first = run();
    let second = run();
    assert!(!first.is_empty(), "the trace actually recorded something");
    assert_eq!(first, second);
}

#[test]
fn the_event_trace_is_ordered_by_cycle() {
    let mut t = timing();
    t.trace = Some(Vec::new());
    start_timer(&mut t, 0b00);
    run_sliced(&mut t, FRAME_CYCLES);

    let trace = t.trace.take().unwrap();
    assert!(trace.len() > 100);
    for pair in trace.windows(2) {
        assert!(
            pair[0].0 <= pair[1].0,
            "events fired out of order: {:?} then {:?}",
            pair[0],
            pair[1]
        );
    }
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

#[test]
fn timing_state_round_trips_and_resumes_identically() {
    let mut t = timing();
    t.write_register(reg::TAC, 0b110);
    t.write_register(reg::TMA, 0x20);
    t.write_register(reg::STAT, 0x40);
    t.write_register(reg::LYC, 60);
    run_sliced(&mut t, 12_345);

    let mut w = StateWriter::new();
    t.save(&mut w);
    let blob = w.into_inner();

    let mut restored = timing();
    restored.load(&mut StateReader::new(&blob)).unwrap();

    assert_eq!(restored.now(), t.now());
    assert_eq!(restored.ppu.ly, t.ppu.ly);
    assert_eq!(restored.ppu.mode, t.ppu.mode);
    assert_eq!(restored.apu.step, t.apu.step);
    assert_eq!(
        restored.read_register(reg::TIMA),
        t.read_register(reg::TIMA)
    );
    assert_eq!(restored.read_register(reg::DIV), t.read_register(reg::DIV));

    // The real test is that both continue identically, not merely that fields match: the
    // scheduler's pending events and its sequence counter had to survive too.
    let a = run_sliced(&mut t, FRAME_CYCLES);
    let b = run_sliced(&mut restored, FRAME_CYCLES);
    assert_eq!(a, b);
    assert_eq!(restored.ppu.ly, t.ppu.ly);
    assert_eq!(
        restored.read_register(reg::TIMA),
        t.read_register(reg::TIMA)
    );
}

#[test]
fn reset_returns_to_power_on() {
    let mut t = timing();
    start_timer(&mut t, 0b01);
    run_sliced(&mut t, 50_000);

    t.reset();
    assert_eq!(t.now(), Cycles::ZERO);
    assert_eq!(t.ppu.ly, 0);
    assert_eq!(t.ppu.mode, PpuMode::OamScan);
    assert_eq!(t.apu.step, 0);
    assert_eq!(t.read_register(reg::TIMA), Some(0));
    assert!(t.pending_events() > 0, "and is scheduled to run again");
}
