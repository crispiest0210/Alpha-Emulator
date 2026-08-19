//! End-to-end tests for the assembled machine.
//!
//! These run real SM83 code through the whole stack — CPU, bus, timing, PPU, APU — rather
//! than exercising any one part. The accuracy ROM suites are what finally decide correctness,
//! but these catch a broken assembly in a second instead of after a harness run.

use super::*;
use core_common::{Buttons, Rgba8};
use ppu_tile2d::DMG_SHADES;

/// Build a cartridge whose program sits at `0x0150`, entered from the header's jump.
fn rom_with_program(program: &[u8]) -> Vec<u8> {
    let mut rom = vec![0u8; 0x8000];
    // The entry point is four bytes at 0x0100; jump past the header to the real code.
    rom[0x0100] = 0xC3; // jp 0x0150
    rom[0x0101] = 0x50;
    rom[0x0102] = 0x01;
    rom[0x0150..0x0150 + program.len()].copy_from_slice(program);

    rom[0x0134..0x0139].copy_from_slice(b"TEST\0");
    rom[0x0147] = 0x03; // MBC1 + RAM + battery
    rom[0x0148] = 0x00;
    rom[0x0149] = 0x02; // 8 KiB of cartridge RAM
    rom[0x014D] = GbHeader::header_checksum(&rom);
    rom
}

/// Increment a byte in work RAM forever, so progress is observable.
///
/// ```text
/// 3E 00        ld  a, 0
/// 21 00 C0     ld  hl, 0xC000
/// 3C           inc a
/// 77           ld  (hl), a
/// 18 FC        jr  -4
/// ```
const COUNTER_PROGRAM: &[u8] = &[0x3E, 0x00, 0x21, 0x00, 0xC0, 0x3C, 0x77, 0x18, 0xFC];

/// Spin in place: `jr -2`.
const SPIN_PROGRAM: &[u8] = &[0x18, 0xFE];

fn system(program: &[u8]) -> GbSystem {
    GbSystem::new(rom_with_program(program), None).expect("the test ROM is valid")
}

fn read(system: &mut GbSystem, addr: u16) -> u8 {
    system.bus_mut().read8(addr as u32)
}

fn write(system: &mut GbSystem, addr: u16, value: u8) {
    system.bus_mut().write8(addr as u32, value);
}

/// Fill VRAM with a solid tile and point the whole tilemap at it, so the screen is not blank.
fn draw_solid_screen(system: &mut GbSystem, color: u8) {
    for row in 0..8u16 {
        let low = if color & 1 != 0 { 0xFF } else { 0x00 };
        let high = if color & 2 != 0 { 0xFF } else { 0x00 };
        write(system, 0x8010 + row * 2, low);
        write(system, 0x8010 + row * 2 + 1, high);
    }
    for cell in 0..32u16 * 32 {
        write(system, 0x9800 + cell, 1);
    }
}

// ---------------------------------------------------------------------------
// Running
// ---------------------------------------------------------------------------

#[test]
fn a_cartridge_boots_and_its_code_runs() {
    let mut gb = system(COUNTER_PROGRAM);
    assert_eq!(read(&mut gb, 0xC000), 0);

    gb.step_frame(InputState::default());
    assert_ne!(
        read(&mut gb, 0xC000),
        0,
        "the program executed and wrote to work RAM"
    );
}

#[test]
fn a_frame_takes_about_a_frame_worth_of_cycles() {
    let mut gb = system(COUNTER_PROGRAM);
    // The first call is short: a fresh machine starts at line 0, and a frame ends when the
    // PPU *enters* VBlank, so that call covers only the visible lines. Every call after it
    // runs VBlank-to-VBlank and is a full frame.
    gb.step_frame(InputState::default());
    let out = gb.step_frame(InputState::default());
    let expected = crate::timing::FRAME_CYCLES;
    let elapsed = out.cycles_elapsed.get();
    assert!(
        elapsed >= expected && elapsed < expected + 200,
        "expected about {expected} cycles, got {elapsed}"
    );
}

#[test]
fn successive_frames_advance_the_clock_monotonically() {
    let mut gb = system(COUNTER_PROGRAM);
    gb.step_frame(InputState::default()); // the short first frame
    for _ in 0..5 {
        let elapsed = gb.step_frame(InputState::default()).cycles_elapsed.get();
        assert!(
            elapsed >= crate::timing::FRAME_CYCLES,
            "a steady-state frame is a full frame: {elapsed}"
        );
    }
}

#[test]
fn without_a_boot_rom_the_machine_starts_in_its_post_boot_state() {
    let mut gb = system(SPIN_PROGRAM);
    // Execution begins at the cartridge entry point, not at a boot ROM.
    assert_eq!(gb.cpu().pc, 0x0100);
    assert_eq!(read(&mut gb, 0xFF40), 0x91, "the LCD is already on");
    assert_eq!(read(&mut gb, 0xFF47), 0xFC, "and BGP is configured");
}

#[test]
fn a_supplied_boot_rom_runs_first() {
    // A boot ROM that immediately unmaps itself by writing to 0xFF50, then spins.
    let boot = vec![0x3E, 0x01, 0xE0, 0x50, 0x18, 0xFE];
    let mut gb = GbSystem::new(rom_with_program(SPIN_PROGRAM), Some(boot)).unwrap();

    assert_eq!(gb.cpu().pc, 0x0000, "execution starts in the boot ROM");
    assert!(gb.bus().memory.boot_rom_enabled());

    gb.step_frame(InputState::default());
    assert!(
        !gb.bus().memory.boot_rom_enabled(),
        "the boot ROM unmapped itself"
    );
}

#[test]
fn a_malformed_cartridge_is_rejected_rather_than_panicking() {
    assert!(GbSystem::new(vec![0u8; 100], None).is_err());
    let mut gb = system(SPIN_PROGRAM);
    assert!(gb.load_cartridge(&[0u8; 100]).is_err());
}

#[test]
fn a_machine_with_no_cartridge_still_produces_frames() {
    // A frontend that has not been given a ROM must get a blank frame, not a hang.
    let mut gb = GbSystem::empty();
    let out = gb.step_frame(InputState::default());
    assert!(out.cycles_elapsed.get() > 0);
    assert_eq!(gb.framebuffer().width(), ppu::SCREEN_WIDTH);
}

// ---------------------------------------------------------------------------
// Video
// ---------------------------------------------------------------------------

#[test]
fn the_ppu_composites_a_frame_through_the_scheduler() {
    let mut gb = system(SPIN_PROGRAM);
    draw_solid_screen(&mut gb, 3);
    gb.step_frame(InputState::default());

    // BGP is 0xFC out of boot, which maps colour index 3 to the darkest shade.
    assert_eq!(gb.framebuffer().pixel(0, 0), DMG_SHADES[3]);
    assert_eq!(gb.framebuffer().pixel(159, 143), DMG_SHADES[3]);
}

#[test]
fn every_visible_line_gets_composited() {
    let mut gb = system(SPIN_PROGRAM);
    draw_solid_screen(&mut gb, 3);
    gb.step_frame(InputState::default());

    for y in 0..ppu::SCREEN_HEIGHT {
        assert_eq!(
            gb.framebuffer().pixel(80, y),
            DMG_SHADES[3],
            "line {y} was never drawn"
        );
    }
}

#[test]
fn switching_the_lcd_off_blanks_the_screen() {
    let mut gb = system(SPIN_PROGRAM);
    draw_solid_screen(&mut gb, 3);
    gb.step_frame(InputState::default());
    assert_eq!(gb.framebuffer().pixel(0, 0), DMG_SHADES[3]);

    write(&mut gb, 0xFF40, 0x11); // LCD off, background still enabled
    gb.step_frame(InputState::default());
    assert_eq!(gb.framebuffer().pixel(0, 0), DMG_SHADES[0]);
}

#[test]
fn the_vblank_interrupt_reaches_the_interrupt_flag_register() {
    let mut gb = system(SPIN_PROGRAM);
    gb.step_frame(InputState::default());
    assert_ne!(
        read(&mut gb, 0xFF0F) & 0x01,
        0,
        "VBlank was requested during the frame"
    );
}

// ---------------------------------------------------------------------------
// Input
// ---------------------------------------------------------------------------

#[test]
fn input_reaches_the_joypad_register() {
    let mut gb = system(SPIN_PROGRAM);
    gb.step_frame(InputState {
        buttons: Buttons::A | Buttons::RIGHT,
        touch: None,
    });

    // Select the action group; a pressed button reads as zero.
    write(&mut gb, 0xFF00, 0b1101_1111);
    assert_eq!(read(&mut gb, 0xFF00) & 0x01, 0, "A is pressed");

    write(&mut gb, 0xFF00, 0b1110_1111);
    assert_eq!(read(&mut gb, 0xFF00) & 0x01, 0, "Right is pressed");
    assert_ne!(read(&mut gb, 0xFF00) & 0x02, 0, "Left is not");
}

#[test]
fn pressing_a_selected_button_requests_the_joypad_interrupt() {
    let mut gb = system(SPIN_PROGRAM);
    write(&mut gb, 0xFF00, 0b1101_1111); // select the action buttons
    write(&mut gb, 0xFF0F, 0x00);

    gb.step_frame(InputState {
        buttons: Buttons::START,
        touch: None,
    });
    assert_ne!(read(&mut gb, 0xFF0F) & 0x10, 0);
}

// ---------------------------------------------------------------------------
// Audio
// ---------------------------------------------------------------------------

#[test]
fn a_frame_produces_roughly_a_frames_worth_of_audio() {
    let mut gb = system(SPIN_PROGRAM);
    // Skip the short first frame, then measure a steady-state one.
    gb.step_frame(InputState::default());
    gb.take_audio_samples();
    gb.step_frame(InputState::default());
    let produced = gb.take_audio_samples().len();

    let expected = core_common::AUDIO_SAMPLE_RATE as f64 * crate::timing::FRAME_CYCLES as f64
        / crate::timing::CLOCK_HZ as f64;
    assert!(
        (produced as f64 - expected).abs() < 5.0,
        "expected about {expected:.0} samples, got {produced}"
    );
}

#[test]
fn audio_is_drained_exactly_once_per_frame() {
    let mut gb = system(SPIN_PROGRAM);
    gb.step_frame(InputState::default());
    assert!(!gb.take_audio_samples().is_empty());
    assert!(gb.take_audio_samples().is_empty());
}

// ---------------------------------------------------------------------------
// Cartridge saves
// ---------------------------------------------------------------------------

#[test]
fn a_battery_backed_cartridge_exposes_its_save_ram() {
    let mut gb = system(SPIN_PROGRAM);
    assert!(gb.save_ram().is_some());

    write(&mut gb, 0x0000, 0x0A); // enable cartridge RAM
    write(&mut gb, 0xA000, 0x5A);
    assert_eq!(gb.save_ram().unwrap()[0], 0x5A);

    // And it round-trips through a .sav file.
    let dumped = gb.save_ram().unwrap().to_vec();
    let mut fresh = system(SPIN_PROGRAM);
    fresh.load_save_ram(&dumped).unwrap();
    write(&mut fresh, 0x0000, 0x0A);
    assert_eq!(read(&mut fresh, 0xA000), 0x5A);
}

#[test]
fn a_write_to_save_ram_is_reported_so_the_frontend_can_flush() {
    let mut gb = system(SPIN_PROGRAM);
    let out = gb.step_frame(InputState::default());
    assert!(!out.save_ram_dirty, "nothing wrote to it");

    write(&mut gb, 0x0000, 0x0A);
    write(&mut gb, 0xA000, 0x01);
    let out = gb.step_frame(InputState::default());
    assert!(out.save_ram_dirty);

    let out = gb.step_frame(InputState::default());
    assert!(!out.save_ram_dirty, "and the flag clears once reported");
}

// ---------------------------------------------------------------------------
// Reset
// ---------------------------------------------------------------------------

#[test]
fn reset_restarts_the_machine_but_keeps_the_cartridge_and_its_save() {
    // Resetting a console does not eject the game, and certainly does not wipe the save.
    let mut gb = system(COUNTER_PROGRAM);
    write(&mut gb, 0x0000, 0x0A);
    write(&mut gb, 0xA000, 0x77);
    for _ in 0..3 {
        gb.step_frame(InputState::default());
    }
    assert_ne!(read(&mut gb, 0xC000), 0);

    gb.reset();
    assert_eq!(read(&mut gb, 0xC000), 0, "work RAM was cleared");
    assert_eq!(gb.cpu().pc, 0x0100, "and execution restarted");
    assert_eq!(gb.save_ram().unwrap()[0], 0x77, "but the save survived");

    gb.step_frame(InputState::default());
    assert_ne!(read(&mut gb, 0xC000), 0, "and it runs again");
}

// ---------------------------------------------------------------------------
// Save states
// ---------------------------------------------------------------------------

#[test]
fn a_save_state_round_trip_is_frame_exact() {
    // The headline regression test. The predecessor implemented save states by reaching into
    // a third-party core's private fields, needed a "warm reboot" after every load, and still
    // corrupted tiles. Here every component serializes itself, and the check is that a
    // reloaded machine produces bit-identical frames to one that never stopped.
    let mut gb = system(COUNTER_PROGRAM);
    draw_solid_screen(&mut gb, 2);
    for _ in 0..3 {
        gb.step_frame(InputState::default());
    }

    // Drain first, so both paths start from an empty audio buffer. Staged output samples are
    // not machine state and deliberately are not serialized.
    gb.take_audio_samples();
    let state = gb.save_state();

    // Reference: keep running.
    for _ in 0..4 {
        gb.step_frame(InputState::default());
    }
    let reference_frame = gb.framebuffer().clone();
    let reference_counter = read(&mut gb, 0xC000);
    let reference_audio = gb.take_audio_samples().to_vec();

    // Diverge hard, then load and replay the same four frames.
    for _ in 0..10 {
        gb.step_frame(InputState {
            buttons: Buttons::all(),
            touch: None,
        });
    }
    gb.load_state(&state).expect("the state loads");
    gb.take_audio_samples();
    for _ in 0..4 {
        gb.step_frame(InputState::default());
    }

    assert_eq!(
        read(&mut gb, 0xC000),
        reference_counter,
        "CPU and RAM match"
    );
    assert_eq!(
        gb.framebuffer(),
        &reference_frame,
        "and the picture is bit-identical"
    );
    assert_eq!(
        gb.take_audio_samples(),
        reference_audio.as_slice(),
        "and so is the audio"
    );
}

#[test]
fn a_save_state_captures_every_subsystem() {
    // A component missing from serialization shows up as a divergence a few frames after the
    // load, which is far harder to trace than a failure at load time — so check the pieces
    // individually as well as through the frame-exactness test above.
    let mut gb = system(COUNTER_PROGRAM);
    write(&mut gb, 0xFF07, 0x05); // a running timer
    write(&mut gb, 0xFF12, 0xF0); // an armed audio channel
    write(&mut gb, 0xFF14, 0x87);
    write(&mut gb, 0xFF43, 0x22); // a scrolled background
    write(&mut gb, 0x0000, 0x0A);
    write(&mut gb, 0xA000, 0x99); // cartridge RAM
    for _ in 0..2 {
        gb.step_frame(InputState::default());
    }

    let state = gb.save_state();
    let mut restored = system(COUNTER_PROGRAM);
    restored.load_state(&state).unwrap();

    assert_eq!(restored.cpu(), gb.cpu(), "the CPU");
    assert_eq!(read(&mut restored, 0xFF43), 0x22, "PPU registers");
    assert_eq!(
        read(&mut restored, 0xFF05),
        read(&mut gb, 0xFF05),
        "the timer"
    );
    assert_eq!(read(&mut restored, 0xA000), 0x99, "cartridge RAM");
    assert_eq!(
        restored.bus().apu.ch1.enabled,
        gb.bus().apu.ch1.enabled,
        "the APU"
    );
    assert_eq!(restored.framebuffer(), gb.framebuffer(), "the framebuffer");
}

#[test]
fn a_state_from_another_system_is_refused() {
    let mut gb = system(SPIN_PROGRAM);
    let mut state = gb.save_state();
    let index = state
        .windows(2)
        .position(|w| w == b"gb")
        .expect("the system id is in the header");
    state[index] = b'X';

    assert!(matches!(
        gb.load_state(&state),
        Err(core_common::StateError::WrongSystem { .. })
    ));
}

#[test]
fn a_truncated_state_is_refused() {
    let mut gb = system(SPIN_PROGRAM);
    let state = gb.save_state();
    assert!(gb.load_state(&state[..state.len() / 2]).is_err());
}

// ---------------------------------------------------------------------------
// Determinism
// ---------------------------------------------------------------------------

#[test]
fn two_identical_runs_produce_identical_output() {
    // Determinism is what save states, rewind, and the accuracy harness all rest on.
    fn run() -> (Vec<u8>, u64, Rgba8) {
        let mut gb = system(COUNTER_PROGRAM);
        draw_solid_screen(&mut gb, 1);
        let mut cycles = 0;
        for frame in 0..8 {
            let buttons = if frame % 3 == 0 {
                Buttons::A
            } else {
                Buttons::empty()
            };
            cycles += gb
                .step_frame(InputState {
                    buttons,
                    touch: None,
                })
                .cycles_elapsed
                .get();
        }
        (
            gb.framebuffer().as_bytes().to_vec(),
            cycles,
            gb.framebuffer().pixel(0, 0),
        )
    }

    assert_eq!(run(), run());
}

// ---------------------------------------------------------------------------
// STOP
// ---------------------------------------------------------------------------

/// `10 00` is STOP, `3C` is INC A, `18 FE` spins.
const STOP_PROGRAM: &[u8] = &[0x10, 0x00, 0x3C, 0x18, 0xFE];

#[test]
fn a_button_press_releases_stop() {
    let mut gb = system(STOP_PROGRAM);
    write(&mut gb, 0xFF00, 0b1101_1111); // select the action buttons
    write(&mut gb, 0xFFFF, 0x00); // every interrupt disabled

    gb.step_frame(InputState::default());
    assert!(gb.cpu.is_stopped(), "the CPU entered low-power mode");

    gb.step_frame(InputState {
        buttons: Buttons::A,
        ..InputState::default()
    });
    assert!(
        !gb.cpu.is_stopped(),
        "a joypad line going low releases STOP even with the interrupt disabled"
    );
}

#[test]
fn stop_persists_while_nothing_is_pressed() {
    let mut gb = system(STOP_PROGRAM);
    write(&mut gb, 0xFFFF, 0x00);
    for _ in 0..4 {
        gb.step_frame(InputState::default());
    }
    assert!(gb.cpu.is_stopped());
}

// ---------------------------------------------------------------------------
// The APU, seen through the assembled system
// ---------------------------------------------------------------------------

#[test]
fn the_frame_sequencer_reaches_the_apu_over_real_time() {
    // Blargg's sound tests all spin waiting on channel state, so the first thing worth
    // establishing is that the 512 Hz sequencer actually reaches the APU once the system is
    // assembled. The units are tested in isolation; the wiring between them was not.
    let mut gb = system(SPIN_PROGRAM);

    write(&mut gb, 0xFF26, 0x80); // power the APU on
    write(&mut gb, 0xFF12, 0xF0); // full volume, no envelope decay
    write(&mut gb, 0xFF11, 0x3D); // a length of three 256 Hz steps
    write(&mut gb, 0xFF14, 0xC0); // trigger with the length counter enabled
    assert_eq!(read(&mut gb, 0xFF26) & 0x01, 0x01, "the channel started");

    // Length clocks at 256 Hz, so three steps is about 12 ms — inside two frames even if the
    // first-half quirk shaves one step off.
    gb.step_frame(InputState::default());
    gb.step_frame(InputState::default());
    assert_eq!(
        read(&mut gb, 0xFF26) & 0x01,
        0,
        "the length counter expired and NR52 reports the channel as off"
    );
}

#[test]
fn a_channel_with_its_length_counter_disabled_plays_indefinitely() {
    let mut gb = system(SPIN_PROGRAM);
    write(&mut gb, 0xFF26, 0x80);
    assert_eq!(read(&mut gb, 0xFF26) & 0x0F, 0, "nothing playing yet");

    write(&mut gb, 0xFF12, 0xF0);
    write(&mut gb, 0xFF11, 0x3F);
    write(&mut gb, 0xFF14, 0x80); // trigger, length counter *disabled*
    for _ in 0..3 {
        gb.step_frame(InputState::default());
    }
    assert_eq!(
        read(&mut gb, 0xFF26) & 0x01,
        0x01,
        "nothing clocks the length counter, so the channel never expires"
    );
}

#[test]
fn rewinding_a_running_machine_returns_it_to_where_it_was() {
    // The acceptance test the rewind buffer exists for: it holds real save states of a real
    // machine, and loading one puts that machine back. Unit-testing the ring on its own proves
    // the bookkeeping; this proves the thing it is bookkeeping for.
    use savestate::RewindBuffer;

    let mut gb = system(COUNTER_PROGRAM);
    let mut buffer = RewindBuffer::new(8, 2);

    for frame in 0..12u64 {
        if buffer.wants_snapshot() {
            buffer.push(frame, gb.save_state());
        } else {
            buffer.frame_elapsed();
        }
        gb.step_frame(InputState::default());
    }

    let counter_now = read(&mut gb, 0xC000);
    let snapshot = buffer.rewind().expect("there is history").clone();
    gb.load_state(&snapshot.state).expect("the state is valid");
    let counter_then = read(&mut gb, 0xC000);

    assert_ne!(
        counter_then, counter_now,
        "the machine went back to an earlier moment"
    );

    // And running forward from there reaches the same place again, which is what makes rewind
    // usable rather than merely a jump backwards.
    let frames_to_replay = 12 - snapshot.frame;
    for _ in 0..frames_to_replay {
        gb.step_frame(InputState::default());
    }
    assert_eq!(read(&mut gb, 0xC000), counter_now);
}

#[test]
fn a_rewind_buffer_can_walk_back_across_its_whole_depth_of_real_states() {
    use savestate::RewindBuffer;

    let mut gb = system(COUNTER_PROGRAM);
    let mut buffer = RewindBuffer::new(4, 1);
    for frame in 0..4u64 {
        buffer.push(frame, gb.save_state());
        gb.step_frame(InputState::default());
    }

    let mut seen = Vec::new();
    while let Some(snapshot) = buffer.rewind() {
        let state = snapshot.state.clone();
        gb.load_state(&state).expect("every stored state loads");
        seen.push(read(&mut gb, 0xC000));
    }
    assert_eq!(seen.len(), 3, "three steps back from the newest of four");
}

// ---------------------------------------------------------------------------
// The debugger, driven from above
// ---------------------------------------------------------------------------

#[test]
fn an_execution_breakpoint_stops_a_running_machine() {
    // The integration prompt 15 asks for, arranged so the system never learns that breakpoints
    // exist: the caller steps instructions and consults the registry between them. That is what
    // keeps `debugger` above the systems rather than inside them.
    use debugger::Breakpoints;

    // The header's entry point jumps to 0x0150, where the test program is placed: `3E 00`
    // LD A,0, then `3C` INC A at 0x0152, then a spin.
    let mut gb = system(&[0x3E, 0x00, 0x3C, 0x18, 0xFE]);
    let mut points = Breakpoints::new();
    points.add_execution(0x0152);

    let mut stopped_at = None;
    for _ in 0..16 {
        let pc = gb.cpu.pc;
        if let Some(trigger) = points.check_execution(pc as u32) {
            stopped_at = Some(trigger);
            break;
        }
        gb.step_instruction();
    }

    assert_eq!(
        stopped_at,
        Some(debugger::Trigger::Execution { addr: 0x0152 }),
        "it stopped before running the instruction at the breakpoint"
    );
}

#[test]
fn a_write_watchpoint_catches_the_access_that_set_a_register() {
    // The other half of prompt 15's acceptance test: a watchpoint on an MMIO register fires on
    // the expected write and not on unrelated traffic.
    use debugger::{AccessKind, Breakpoints, Watchpoint};

    let mut points = Breakpoints::new();
    points.add_watchpoint(Watchpoint::at(0xFF47, AccessKind::Write)); // BGP

    let mut gb = system(SPIN_PROGRAM);
    assert_eq!(
        points.check_access(0xFF40, AccessKind::Write, 0x91),
        None,
        "an unrelated register does not trigger it"
    );

    write(&mut gb, 0xFF47, 0xE4);
    assert!(
        points
            .check_access(0xFF47, AccessKind::Write, 0xE4)
            .is_some(),
        "and the watched one does"
    );
}

#[test]
fn stepping_one_instruction_advances_the_clock_by_that_instruction() {
    let mut gb = system(COUNTER_PROGRAM);
    let cycles = gb.step_instruction();
    assert!(cycles.get() > 0, "an instruction costs something");
    assert_eq!(gb.bus().timing.now(), cycles);
}

// ---------------------------------------------------------------------------
// What the CPU cannot reach, and when
// ---------------------------------------------------------------------------

/// Park the PPU in one mode with the LCD on, so a lockout can be tested at a known instant.
///
/// Reaching a mode by running the machine to it would be at the mercy of where an instruction
/// happened to land; these tests are about the rule, not about the schedule that produces it.
fn park_in(system: &mut GbSystem, mode: PpuMode) {
    system.bus_mut().timing.ppu.lcd_enabled = true;
    system.bus_mut().timing.ppu.mode = mode;
}

#[test]
fn vram_is_unreachable_while_the_ppu_draws() {
    let mut gb = system(SPIN_PROGRAM);
    park_in(&mut gb, PpuMode::HBlank);
    write(&mut gb, 0x8000, 0x5A);
    assert_eq!(read(&mut gb, 0x8000), 0x5A);

    park_in(&mut gb, PpuMode::Drawing);
    assert_eq!(read(&mut gb, 0x8000), 0xFF, "the PPU has the memory");
    write(&mut gb, 0x8000, 0xA5);

    park_in(&mut gb, PpuMode::HBlank);
    assert_eq!(
        read(&mut gb, 0x8000),
        0x5A,
        "the write was dropped, not held"
    );
}

#[test]
fn vram_is_reachable_in_every_mode_but_drawing() {
    let mut gb = system(SPIN_PROGRAM);
    for mode in [PpuMode::HBlank, PpuMode::VBlank, PpuMode::OamScan] {
        park_in(&mut gb, mode);
        write(&mut gb, 0x8100, 0x3C);
        assert_eq!(read(&mut gb, 0x8100), 0x3C, "VRAM is open in {mode:?}");
        write(&mut gb, 0x8100, 0x00);
    }
}

#[test]
fn oam_is_unreachable_during_the_sprite_scan_as_well_as_the_draw() {
    let mut gb = system(SPIN_PROGRAM);
    park_in(&mut gb, PpuMode::HBlank);
    write(&mut gb, 0xFE00, 0x42);
    assert_eq!(read(&mut gb, 0xFE00), 0x42);

    // Mode 2 is the extra one: the PPU is reading OAM to decide which sprites are on this
    // line, which is a period VRAM is left alone for.
    for mode in [PpuMode::OamScan, PpuMode::Drawing] {
        park_in(&mut gb, mode);
        assert_eq!(read(&mut gb, 0xFE00), 0xFF, "OAM is closed in {mode:?}");
        write(&mut gb, 0xFE00, 0x99);
    }

    park_in(&mut gb, PpuMode::VBlank);
    assert_eq!(read(&mut gb, 0xFE00), 0x42, "and neither write landed");
}

#[test]
fn colour_palette_data_is_unreachable_while_the_ppu_draws() {
    let mut gb = GbSystem::with_model(rom_with_program(SPIN_PROGRAM), None, GbModel::Cgb)
        .expect("the test ROM is valid");

    // Auto-increment off, so every write here addresses the same palette byte.
    park_in(&mut gb, PpuMode::HBlank);
    write(&mut gb, 0xFF68, 0x00);
    write(&mut gb, 0xFF69, 0x1F);
    assert_eq!(read(&mut gb, 0xFF69), 0x1F);

    park_in(&mut gb, PpuMode::Drawing);
    assert_eq!(read(&mut gb, 0xFF69), 0xFF, "BCPD is closed while drawing");
    write(&mut gb, 0xFF69, 0x7E);
    // Index 0 with auto-increment off, and bit 6 reading high as it always does.
    assert_eq!(
        read(&mut gb, 0xFF68),
        0x40,
        "but the index register stays reachable"
    );

    park_in(&mut gb, PpuMode::HBlank);
    assert_eq!(read(&mut gb, 0xFF69), 0x1F, "the palette write was dropped");
}

#[test]
fn switching_the_lcd_off_opens_everything_again() {
    // Which is the only reason a game can fill VRAM at startup at all: it does that before
    // switching the display on, in one uninterrupted run.
    let mut gb = system(SPIN_PROGRAM);
    gb.bus_mut().timing.ppu.lcd_enabled = false;
    gb.bus_mut().timing.ppu.mode = PpuMode::Drawing;

    write(&mut gb, 0x8000, 0x11);
    write(&mut gb, 0xFE00, 0x22);
    assert_eq!(read(&mut gb, 0x8000), 0x11);
    assert_eq!(read(&mut gb, 0xFE00), 0x22);
}

// ---------------------------------------------------------------------------
// OAM DMA
// ---------------------------------------------------------------------------

/// Report `cycles` t-cycles of CPU time to the bus, the way the CPU itself would.
fn tick(system: &mut GbSystem, cycles: u64) {
    system.bus_mut().tick(Cycles(cycles));
}

/// A machine with the LCD off and `0xC000` onward filled, ready to be copied out of.
fn machine_ready_for_dma() -> GbSystem {
    let mut gb = system(SPIN_PROGRAM);
    // The LCD is off so that the PPU's own lockouts cannot be mistaken for the DMA's.
    gb.bus_mut().timing.ppu.lcd_enabled = false;
    for offset in 0..0xA0u16 {
        write(&mut gb, 0xC000 + offset, offset as u8);
    }
    gb
}

#[test]
fn an_oam_dma_takes_640_cycles_and_lands_every_byte() {
    let mut gb = machine_ready_for_dma();
    write(&mut gb, 0xFF46, 0xC0);

    // Two machine cycles of nothing, then 160 of copying.
    tick(&mut gb, 8);
    assert!(gb.bus().oam_dma.is_running(), "the transfer has begun");

    tick(&mut gb, 636);
    assert!(
        gb.bus().oam_dma.is_running(),
        "and is not finished a machine cycle early"
    );
    tick(&mut gb, 4);
    assert!(gb.bus().oam_dma.is_idle(), "640 cycles, to the cycle");

    for offset in 0..0xA0u16 {
        assert_eq!(
            read(&mut gb, 0xFE00 + offset),
            offset as u8,
            "OAM byte {offset:#04X}"
        );
    }
}

#[test]
fn a_transfer_in_progress_locks_the_cpu_out_of_the_bus_it_is_using() {
    let mut gb = machine_ready_for_dma();
    write(&mut gb, 0xFF80, 0x77);
    write(&mut gb, 0x8000, 0x88);

    write(&mut gb, 0xFF46, 0xC0);
    assert_eq!(
        read(&mut gb, 0xC000),
        0x00,
        "nothing is locked out during the startup delay"
    );

    tick(&mut gb, 8);
    assert_eq!(
        read(&mut gb, 0xC000),
        0xFF,
        "work RAM shares the source bus"
    );
    assert_eq!(read(&mut gb, 0x0100), 0xFF, "and so does the cartridge");
    assert_eq!(read(&mut gb, 0xFE00), 0xFF, "OAM is being written");
    assert_eq!(
        read(&mut gb, 0xFF80),
        0x77,
        "HRAM is why the wait loop works"
    );
    assert_eq!(read(&mut gb, 0xFF46), 0xC0, "and the I/O page stays open");
    assert_eq!(
        read(&mut gb, 0x8000),
        0x88,
        "VRAM is on the other bus, so this transfer never touches it"
    );

    tick(&mut gb, 640);
    assert_eq!(read(&mut gb, 0xC000), 0x00, "and it all reopens after");
}

#[test]
fn a_transfer_out_of_vram_locks_the_other_bus_instead() {
    // The asymmetry is what lets mooneye's `oam_dma_start` run its interrupt vector out of the
    // cartridge while a VRAM-sourced transfer is still going.
    let mut gb = machine_ready_for_dma();
    write(&mut gb, 0x8000, 0x88);

    write(&mut gb, 0xFF46, 0x80);
    tick(&mut gb, 8);

    assert_eq!(read(&mut gb, 0x8000), 0xFF, "VRAM is the source now");
    assert_eq!(read(&mut gb, 0xFE00), 0xFF, "OAM is locked out either way");
    assert_eq!(read(&mut gb, 0xC000), 0x00, "but work RAM is free");
    assert_eq!(read(&mut gb, 0x0100), 0xC3, "and so is the cartridge");
}

#[test]
fn a_write_to_the_dma_register_does_not_stop_the_transfer_already_running() {
    let mut gb = machine_ready_for_dma();
    write(&mut gb, 0x8000, 0x88);
    write(&mut gb, 0xFF46, 0xC0);
    tick(&mut gb, 8 + 4 * 16);

    // Restarting with a VRAM source. For the next two machine cycles the *old* transfer still
    // owns the external bus; only then does the new one take over the video bus.
    write(&mut gb, 0xFF46, 0x80);
    tick(&mut gb, 4);
    assert_eq!(
        read(&mut gb, 0xC000),
        0xFF,
        "the old transfer is still going"
    );

    tick(&mut gb, 4);
    assert_eq!(
        read(&mut gb, 0xC000),
        0x00,
        "now the old one has been replaced"
    );
    assert_eq!(read(&mut gb, 0x8000), 0xFF, "and the new one holds VRAM");
}

#[test]
fn a_transfer_survives_a_save_state_mid_flight() {
    let mut gb = machine_ready_for_dma();
    write(&mut gb, 0xFF46, 0xC0);
    tick(&mut gb, 8 + 4 * 40);

    let bytes = savestate::encode_state("gb", STATE_VERSION, &gb);
    let mut restored = machine_ready_for_dma();
    savestate::decode_state("gb", STATE_VERSION, &bytes, &mut restored).unwrap();

    tick(&mut gb, 600);
    tick(&mut restored, 600);
    assert!(gb.bus().oam_dma.is_idle() && restored.bus().oam_dma.is_idle());
    for offset in 0..0xA0u16 {
        assert_eq!(
            read(&mut restored, 0xFE00 + offset),
            read(&mut gb, 0xFE00 + offset),
            "OAM byte {offset:#04X} after a restore mid-transfer"
        );
    }
}
