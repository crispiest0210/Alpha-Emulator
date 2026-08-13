//! Tests for the harness itself, and the accuracy suites it drives.
//!
//! The adapters are tested against synthetic systems whose reporting is scripted, because a
//! bug in an adapter would silently invalidate every accuracy claim in the project. A
//! pass-detector that always says "pass" would make the whole suite green and meaningless.

use super::*;
use crate::corpus::{self, Convention, Hardware, TestRom, GB_ROMS};
use core_common::{AudioSample, CartridgeError, Cycles, FrameOutput, Savable, StateError};
use core_common::{StateReader, StateWriter};

// ---------------------------------------------------------------------------
// A scriptable system, for testing the adapters
// ---------------------------------------------------------------------------

/// A system that reports whatever the test tells it to, after a given number of frames.
struct ScriptedSystem {
    frames: u32,
    /// Frame at which the scripted result appears.
    reveal_at: u32,
    serial: Vec<u8>,
    serial_reveal: &'static str,
    memory: Vec<u8>,
    memory_reveal: Option<(u8, &'static str)>,
    registers: Vec<RegisterValue>,
    register_reveal: Option<Vec<(&'static str, u64)>>,
    framebuffer: Framebuffer,
}

impl ScriptedSystem {
    fn new() -> Self {
        Self {
            frames: 0,
            reveal_at: 3,
            serial: Vec::new(),
            serial_reveal: "",
            memory: vec![0; 0x1000],
            memory_reveal: None,
            registers: Vec::new(),
            register_reveal: None,
            framebuffer: Framebuffer::new(8, 8),
        }
    }

    fn reporting_serial(text: &'static str) -> Self {
        Self {
            serial_reveal: text,
            ..Self::new()
        }
    }

    fn reporting_memory(status: u8, text: &'static str) -> Self {
        Self {
            memory_reveal: Some((status, text)),
            ..Self::new()
        }
    }

    fn reporting_registers(values: Vec<(&'static str, u64)>) -> Self {
        Self {
            register_reveal: Some(values),
            ..Self::new()
        }
    }
}

impl System for ScriptedSystem {
    fn id(&self) -> &'static str {
        "scripted"
    }
    /// Recorded rather than acted on: the scripted system has no joypad to route it to, and the
    /// harness drives every ROM with no input anyway.
    fn set_input(&mut self, _input: InputState) {}

    /// The harness drives whole frames, never instructions, so this is unreachable. It panics
    /// rather than returning zero: a zero would make a caller's stepping loop spin forever, and a
    /// test double should fail loudly when used for something it does not model.
    fn step_instruction(&mut self) -> core_common::Cycles {
        unimplemented!("the scripted system models frames, not instructions")
    }
    fn display_name(&self) -> &'static str {
        "Scripted"
    }
    fn state_version(&self) -> u32 {
        1
    }
    fn reset(&mut self) {}
    fn load_cartridge(&mut self, _rom: &[u8]) -> Result<(), CartridgeError> {
        Ok(())
    }
    fn framebuffer(&self) -> &Framebuffer {
        &self.framebuffer
    }
    fn take_audio_samples(&mut self) -> &[AudioSample] {
        &[]
    }
    fn save_ram(&self) -> Option<&[u8]> {
        None
    }
    fn load_save_ram(&mut self, _data: &[u8]) -> Result<(), CartridgeError> {
        Ok(())
    }

    fn step_frame(&mut self, _input: InputState) -> FrameOutput {
        self.frames += 1;
        if self.frames == self.reveal_at {
            self.serial.extend_from_slice(self.serial_reveal.as_bytes());

            if let Some((status, text)) = self.memory_reveal {
                self.memory[0] = status;
                self.memory[1..4].copy_from_slice(&[0xDE, 0xB0, 0x61]);
                self.memory[4..4 + text.len()].copy_from_slice(text.as_bytes());
                self.memory[4 + text.len()] = 0;
            }
            if let Some(values) = &self.register_reveal {
                self.registers = values
                    .iter()
                    .map(|(name, value)| RegisterValue::new(name, *value, 8))
                    .collect();
            }
        }
        FrameOutput {
            cycles_elapsed: Cycles(100),
            ..Default::default()
        }
    }
}

impl Savable for ScriptedSystem {
    fn save(&self, w: &mut StateWriter) {
        w.write_u32(self.frames);
    }
    fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        self.frames = r.read_u32()?;
        Ok(())
    }
}

impl TestableSystem for ScriptedSystem {
    fn serial_output(&self) -> &[u8] {
        &self.serial
    }
    fn cpu_registers(&self) -> Vec<RegisterValue> {
        self.registers.clone()
    }
    fn read_byte(&mut self, addr: u32) -> u8 {
        // Cartridge RAM begins at 0xA000.
        self.memory
            .get(addr.wrapping_sub(0xA000) as usize)
            .copied()
            .unwrap_or(0)
    }
}

// ---------------------------------------------------------------------------
// The serial adapter
// ---------------------------------------------------------------------------

#[test]
fn the_serial_adapter_recognizes_a_pass() {
    let mut system = ScriptedSystem::reporting_serial("cpu_instrs\n\nPassed all tests\n");
    let outcome = run_blargg_serial(&mut system, 20);
    assert!(outcome.passed());
    assert!(outcome.report().contains("Passed"));
    assert_eq!(
        outcome.frames(),
        3,
        "it stops as soon as the verdict appears"
    );
}

#[test]
fn the_serial_adapter_recognizes_a_failure_and_keeps_the_diagnosis() {
    // The accumulated text says *which* sub-test broke, which is the difference between a
    // useful failure and a useless one.
    let mut system = ScriptedSystem::reporting_serial("01:ok  02:ok  03:Failed #4\n");
    let outcome = run_blargg_serial(&mut system, 20);
    assert!(!outcome.passed());
    assert!(
        outcome.report().contains("03:Failed #4"),
        "{}",
        outcome.report()
    );
}

#[test]
fn a_silent_rom_times_out_rather_than_being_called_a_failure() {
    // A timeout usually means the emulator hung or the budget was too small; reporting it as
    // a plain failure would hide which.
    let mut system = ScriptedSystem::new();
    let outcome = run_blargg_serial(&mut system, 5);
    assert!(matches!(outcome, TestOutcome::TimedOut { .. }));
    assert_eq!(outcome.frames(), 5);
}

// ---------------------------------------------------------------------------
// The memory adapter
// ---------------------------------------------------------------------------

#[test]
fn the_memory_adapter_recognizes_a_pass() {
    let mut system = ScriptedSystem::reporting_memory(0, "cpu_instrs\n\nPassed\n");
    let outcome = run_blargg_memory(&mut system, 20);
    assert!(outcome.passed());
    assert!(outcome.report().contains("Passed"));
}

#[test]
fn the_memory_adapter_reports_the_failure_code_as_a_failure() {
    let mut system = ScriptedSystem::reporting_memory(4, "Failed #4\n");
    let outcome = run_blargg_memory(&mut system, 20);
    assert!(!outcome.passed());
    assert!(outcome.report().contains("Failed"));
}

#[test]
fn the_memory_adapter_waits_while_the_status_says_running() {
    let mut system = ScriptedSystem::reporting_memory(0x80, "in progress");
    let outcome = run_blargg_memory(&mut system, 6);
    assert!(matches!(outcome, TestOutcome::TimedOut { .. }));
}

#[test]
fn the_memory_adapter_requires_the_signature() {
    // Without the signature check, cartridge RAM that happens to hold zero reads as a pass —
    // which is exactly the state uninitialized RAM is most likely to be in.
    let mut system = ScriptedSystem::new();
    system.memory[0] = 0; // a "pass" status with no signature behind it
    let outcome = run_blargg_memory(&mut system, 5);
    assert!(
        matches!(outcome, TestOutcome::TimedOut { .. }),
        "zeroed RAM must not read as a pass"
    );
}

// ---------------------------------------------------------------------------
// The Mooneye adapter
// ---------------------------------------------------------------------------

#[test]
fn the_mooneye_adapter_recognizes_the_fibonacci_pattern() {
    let mut system = ScriptedSystem::reporting_registers(
        MOONEYE_PASS_REGISTERS
            .iter()
            .map(|(name, value)| (*name, *value))
            .collect(),
    );
    assert!(run_mooneye(&mut system, 20).passed());
}

#[test]
fn one_wrong_register_is_not_a_pass() {
    let mut values: Vec<(&'static str, u64)> = MOONEYE_PASS_REGISTERS
        .iter()
        .map(|(name, value)| (*name, *value))
        .collect();
    values[3].1 = 12; // should be 13
    let mut system = ScriptedSystem::reporting_registers(values);
    let outcome = run_mooneye(&mut system, 5);
    assert!(!outcome.passed());
    assert!(outcome.report().contains("want 13"), "{}", outcome.report());
}

#[test]
fn the_mooneye_adapter_recognizes_the_failure_pattern() {
    let values = MOONEYE_PASS_REGISTERS
        .iter()
        .map(|(name, _)| (*name, 0x42u64))
        .collect();
    let mut system = ScriptedSystem::reporting_registers(values);
    let outcome = run_mooneye(&mut system, 20);
    assert!(matches!(outcome, TestOutcome::Failed { .. }));
}

#[test]
fn a_rom_that_never_sets_the_registers_times_out() {
    let mut system = ScriptedSystem::new();
    assert!(matches!(
        run_mooneye(&mut system, 4),
        TestOutcome::TimedOut { .. }
    ));
}

// ---------------------------------------------------------------------------
// The gba-suite adapter
// ---------------------------------------------------------------------------

/// A system whose `PC`/`R12` registers follow a scripted, per-frame sequence.
///
/// `ScriptedSystem` above can only reveal one fixed register snapshot at a chosen frame, which
/// is not enough to model a `gba-suite` run: that convention needs `r12` to visibly change
/// *while* the PC is still moving, then settle. The last entry in the script holds once the
/// steps run out, which is what lets the PC "settle" for the three-frame check.
struct GbaScript {
    steps: Vec<(u64, u64)>,
    frame: usize,
    framebuffer: Framebuffer,
}

impl GbaScript {
    fn new(steps: Vec<(u64, u64)>) -> Self {
        Self {
            steps,
            frame: 0,
            framebuffer: Framebuffer::new(4, 4),
        }
    }

    fn current(&self) -> (u64, u64) {
        let index = self.frame.saturating_sub(1).min(self.steps.len() - 1);
        self.steps[index]
    }
}

impl System for GbaScript {
    fn id(&self) -> &'static str {
        "gba-script"
    }
    fn set_input(&mut self, _input: InputState) {}
    fn step_instruction(&mut self) -> core_common::Cycles {
        unimplemented!("this system models frames, not instructions")
    }
    fn display_name(&self) -> &'static str {
        "GbaScript"
    }
    fn state_version(&self) -> u32 {
        1
    }
    fn reset(&mut self) {}
    fn load_cartridge(&mut self, _rom: &[u8]) -> Result<(), CartridgeError> {
        Ok(())
    }
    fn framebuffer(&self) -> &Framebuffer {
        &self.framebuffer
    }
    fn take_audio_samples(&mut self) -> &[AudioSample] {
        &[]
    }
    fn save_ram(&self) -> Option<&[u8]> {
        None
    }
    fn load_save_ram(&mut self, _data: &[u8]) -> Result<(), CartridgeError> {
        Ok(())
    }
    fn step_frame(&mut self, _input: InputState) -> FrameOutput {
        self.frame += 1;
        // A real ROM draws its report before settling; without this the framebuffer check
        // that `run_gba_suite` now performs would see an unchanging screen and call every
        // scripted run a hang, regardless of what `steps` says.
        self.framebuffer
            .set_pixel(0, 0, core_common::Rgba8::rgb(self.frame as u8, 0, 0));
        FrameOutput {
            cycles_elapsed: Cycles(100),
            ..Default::default()
        }
    }
}

impl Savable for GbaScript {
    fn save(&self, _w: &mut StateWriter) {}
    fn load(&mut self, _r: &mut StateReader) -> Result<(), StateError> {
        Ok(())
    }
}

impl TestableSystem for GbaScript {
    fn serial_output(&self) -> &[u8] {
        &[]
    }
    fn cpu_registers(&self) -> Vec<RegisterValue> {
        let (pc, r12) = self.current();
        vec![
            RegisterValue::new("PC", pc, 32),
            RegisterValue::new("R12", r12, 32),
        ]
    }
    fn read_byte(&mut self, _addr: u32) -> u8 {
        0
    }
}

#[test]
fn a_machine_hung_before_running_is_not_reported_as_a_pass() {
    // Exactly what a machine wedged in a BIOS trap before the test runner even starts looks
    // like from outside: the PC never moves and r12 — its reset value — is zero throughout.
    // Settled PC plus r12==0 alone would read this as "every sub-test passed", which is the
    // defect this test exists to catch.
    let mut system = ScriptedSystem::new();
    let outcome = run_gba_suite(&mut system, 20);
    assert!(
        !outcome.passed(),
        "a machine that never ran must not be reported as passing: {outcome:?}"
    );
}

#[test]
fn a_genuine_pass_is_still_recognized() {
    // r12 carries a nonzero sub-test index while the suite runs, then clears to zero and the
    // PC settles once every sub-test has passed.
    let mut system = GbaScript::new(vec![(0x100, 1), (0x104, 2), (0x108, 0)]);
    let outcome = run_gba_suite(&mut system, 20);
    assert!(outcome.passed(), "{outcome:?}");
}

#[test]
fn a_genuine_failure_is_still_recognized() {
    // Same shape, but r12 settles on the number of the sub-test that failed rather than zero.
    let mut system = GbaScript::new(vec![(0x100, 1), (0x104, 2), (0x108, 3)]);
    let outcome = run_gba_suite(&mut system, 20);
    assert!(!outcome.passed());
    assert!(outcome.report().contains('3'), "{}", outcome.report());
}

// ---------------------------------------------------------------------------
// Determinism utilities
// ---------------------------------------------------------------------------

/// A tiny Game Boy program that spins, for exercising the utilities against a real system.
fn gb_test_rom() -> Vec<u8> {
    let mut rom = vec![0u8; 0x8000];
    rom[0x0100] = 0xC3;
    rom[0x0101] = 0x50;
    rom[0x0102] = 0x01;
    // ld a,0 ; ld hl,0xC000 ; inc a ; ld (hl),a ; jr -4
    rom[0x0150..0x0159].copy_from_slice(&[0x3E, 0x00, 0x21, 0x00, 0xC0, 0x3C, 0x77, 0x18, 0xFC]);
    rom[0x0134..0x0139].copy_from_slice(b"HARN\0");
    rom[0x0147] = 0x03;
    rom[0x0149] = 0x02;
    rom[0x014D] = cart_common::GbHeader::header_checksum(&rom);
    rom
}

#[test]
fn the_determinism_check_passes_a_deterministic_system() {
    let result = check_deterministic(
        || system_gb::GbSystem::new(gb_test_rom(), None).unwrap(),
        &[
            InputState::default(),
            InputState {
                buttons: core_common::Buttons::A,
                touch: None,
            },
        ],
        6,
    );
    assert!(result.is_ok(), "{result:?}");
}

#[test]
fn the_determinism_check_catches_a_divergence() {
    // The check has to be able to fail, or it proves nothing. This system reports a different
    // cycle count on its second instance.
    struct Flaky {
        frames: u32,
        offset: u64,
        framebuffer: Framebuffer,
    }
    impl System for Flaky {
        fn id(&self) -> &'static str {
            "flaky"
        }
        fn step_instruction(&mut self) -> core_common::Cycles {
            unimplemented!("this system models frames, not instructions")
        }
        fn set_input(&mut self, _: InputState) {}
        fn display_name(&self) -> &'static str {
            "Flaky"
        }
        fn state_version(&self) -> u32 {
            1
        }
        fn reset(&mut self) {}
        fn load_cartridge(&mut self, _: &[u8]) -> Result<(), CartridgeError> {
            Ok(())
        }
        fn framebuffer(&self) -> &Framebuffer {
            &self.framebuffer
        }
        fn take_audio_samples(&mut self) -> &[AudioSample] {
            &[]
        }
        fn save_ram(&self) -> Option<&[u8]> {
            None
        }
        fn load_save_ram(&mut self, _: &[u8]) -> Result<(), CartridgeError> {
            Ok(())
        }
        fn step_frame(&mut self, _: InputState) -> FrameOutput {
            self.frames += 1;
            FrameOutput {
                cycles_elapsed: Cycles(100 + self.offset),
                ..Default::default()
            }
        }
    }
    impl Savable for Flaky {
        fn save(&self, _: &mut StateWriter) {}
        fn load(&mut self, _: &mut StateReader) -> Result<(), StateError> {
            Ok(())
        }
    }

    let counter = std::cell::Cell::new(0u64);
    let result = check_deterministic(
        || {
            let offset = counter.get();
            counter.set(offset + 1);
            Flaky {
                frames: 0,
                offset,
                framebuffer: Framebuffer::new(2, 2),
            }
        },
        &[InputState::default()],
        4,
    );
    let divergence = result.expect_err("the check must catch this");
    assert_eq!(divergence.frame, 0);
    assert!(divergence.detail.contains("frame output differs"));
}

#[test]
fn the_save_state_round_trip_check_passes_a_correct_system() {
    let result = check_save_state_round_trip(
        || system_gb::GbSystem::new(gb_test_rom(), None).unwrap(),
        3,
        4,
    );
    assert!(result.is_ok(), "{result:?}");
}

/// A tiny Game Boy Advance program that spins, incrementing a counter into EWRAM each pass —
/// the GBA counterpart of `gb_test_rom` above. It touches real memory rather than sitting on a
/// single `b .`, so `the_determinism_check_passes_a_deterministic_gba_system` and
/// `the_save_state_round_trip_check_passes_a_correct_gba_system` below are proving those checks
/// hold up under real bus traffic every frame, not just for a machine that never does anything.
///
/// Three instructions, hand-assembled rather than pulled from a `.gba` file so this test has no
/// dependency on the fetched corpus: `add r0, r0, #1` / `str r0, [r1]` / `b` back, with `r1`
/// loaded once from a PC-relative literal to EWRAM's base (`0x02000000`) — plain RAM with none
/// of the cartridge save window's quirks, so this exercises the CPU and memory bus rather than
/// anything this session's other findings touched. `the_gba_test_rom_actually_increments_its_counter`
/// below is the check that this hand-assembly is correct: neither `check_deterministic` nor
/// `check_save_state_round_trip` looks at EWRAM, only at the framebuffer, audio, and
/// `step_frame`'s own return value, so a wrong offset here would not fail either of them.
fn gba_test_rom() -> Vec<u8> {
    let mut rom = vec![0u8; 0x1000];
    let program: [u32; 6] = [
        0xE3A0_0000, // mov r0, #0
        0xE59F_1008, // ldr r1, [pc, #8]  -> r1 = 0x02000000 (EWRAM base)
        0xE280_0001, // loop: add r0, r0, #1
        0xE581_0000, //       str r0, [r1]
        0xEAFF_FFFC, //       b loop
        0x0200_0000, // literal: EWRAM base address
    ];
    for (index, word) in program.iter().enumerate() {
        rom[index * 4..index * 4 + 4].copy_from_slice(&word.to_le_bytes());
    }
    rom
}

#[test]
fn the_gba_test_rom_actually_increments_its_counter() {
    let mut gba = system_gba::GbaSystem::new(gba_test_rom(), None).unwrap();
    for _ in 0..2 {
        gba.step_frame(InputState::default());
    }
    use core_common::Bus;
    let counter = gba.bus_mut().read32(0x0200_0000);
    assert_ne!(
        counter, 0,
        "the loop should have incremented EWRAM's counter well past zero by now"
    );
}

#[test]
fn the_determinism_check_passes_a_deterministic_gba_system() {
    let result = check_deterministic(
        || system_gba::GbaSystem::new(gba_test_rom(), None).unwrap(),
        &[
            InputState::default(),
            InputState {
                buttons: core_common::Buttons::A,
                touch: None,
            },
        ],
        60,
    );
    assert!(result.is_ok(), "{result:?}");
}

#[test]
fn the_save_state_round_trip_check_passes_a_correct_gba_system() {
    let result = check_save_state_round_trip(
        || system_gba::GbaSystem::new(gba_test_rom(), None).unwrap(),
        30,
        60,
    );
    assert!(result.is_ok(), "{result:?}");
}

// ---------------------------------------------------------------------------
// Reporting
// ---------------------------------------------------------------------------

#[test]
fn a_report_separates_skips_from_failures() {
    // An unfetched corpus must never be mistaken for a broken emulator.
    let mut report = SuiteReport::default();
    report.record(
        "good",
        &TestOutcome::Passed {
            report: "Passed".into(),
            frames: 1,
        },
    );
    report.record(
        "bad",
        &TestOutcome::Failed {
            report: "Failed #3\nmore detail".into(),
            frames: 2,
        },
    );
    report.skip("absent");

    assert!(!report.is_success());
    let summary = report.summary();
    assert!(summary.contains("1 passed, 1 failed, 1 skipped"));
    assert!(summary.contains("FAILED bad: Failed #3"));
    assert!(summary.contains("fetch-test-roms"));

    let mut clean = SuiteReport::default();
    clean.skip("absent");
    assert!(clean.is_success(), "skips alone are not a failure");
}

// ---------------------------------------------------------------------------
// The accuracy suites
// ---------------------------------------------------------------------------

/// Run one corpus ROM, skipping if it has not been fetched.
fn run_gb_rom(rom: &TestRom) -> Option<(TestOutcome, String)> {
    let bytes = rom.load()?;
    match rom.hardware {
        Hardware::Dmg => {
            let system = system_gb::GbSystem::new(bytes, None).expect("the ROM parses");
            run_on(rom, system)
        }
        Hardware::Cgb => {
            let system = system_gbc::GbcSystem::new(bytes, None).expect("the ROM parses");
            run_on(rom, system)
        }
        Hardware::Gba => {
            let system = system_gba::GbaSystem::new(bytes, None).expect("the ROM parses");
            run_on(rom, system)
        }
    }
}

/// Apply a ROM's reporting convention to an already-built machine.
///
/// Generic over the system rather than taking `Box<dyn TestableSystem>`: the two machines share
/// every convention, and the only thing that differs is which one to construct.
fn run_on<S: TestableSystem>(rom: &TestRom, mut system: S) -> Option<(TestOutcome, String)> {
    let outcome = match rom.convention {
        Convention::BlarggSerial => run_blargg_serial(&mut system, rom.max_frames),
        Convention::BlarggMemory => run_blargg_memory(&mut system, rom.max_frames),
        Convention::Mooneye => run_mooneye(&mut system, rom.max_frames),
        Convention::GbaSuite => run_gba_suite(&mut system, rom.max_frames),
        Convention::Framebuffer => {
            let framebuffer = capture_framebuffer(&mut system, rom.max_frames);
            let hash = framebuffer.fnv1a_hex();
            match rom.expected_hash {
                Some(expected) if expected == hash => TestOutcome::Passed {
                    report: hash,
                    frames: rom.max_frames,
                },
                Some(expected) => TestOutcome::Failed {
                    report: format!("framebuffer hash {hash}, expected {expected}"),
                    frames: rom.max_frames,
                },
                // Rendering something is not rendering it *correctly*. Without a validated
                // reference this is not a result, and saying otherwise would make the whole
                // suite less trustworthy than saying nothing.
                None => TestOutcome::TimedOut {
                    report: format!("unvalidated: rendered hash {hash}, no reference recorded"),
                    frames: rom.max_frames,
                },
            }
        }
    };
    let state = system.debug_state();
    Some((outcome, state))
}

/// The whole accuracy corpus — DMG, CGB, and GBA — as one test.
///
/// One test rather than one per ROM so the output is a single report naming every failure,
/// which is what someone debugging a regression actually wants — and so an unfetched corpus
/// produces one clear skip message instead of five.
///
/// Known failures are tracked in the corpus rather than deleted from it. That keeps the suite
/// meaningful: it stays green while the known gaps are open, fails loudly if anything *else*
/// breaks, and fails just as loudly if a known-failing ROM starts passing, which means the
/// note needs removing.
#[test]
fn gb_accuracy_suite() {
    let mut report = SuiteReport::default();
    let mut known_failures = Vec::new();
    let mut unexpected_passes = Vec::new();

    for rom in corpus::all_roms() {
        let Some((outcome, state)) = run_gb_rom(rom) else {
            report.skip(rom.name);
            continue;
        };
        let mut failure_state = Some(state);
        eprintln!(
            "{:<22} {:>4}  {} frames",
            rom.name,
            if outcome.passed() { "PASS" } else { "FAIL" },
            outcome.frames()
        );
        // The whole report, not just its first line: these suites name the sub-test that
        // broke, and that is the entire diagnostic value of running them.
        if !outcome.passed() {
            for line in outcome.report().lines() {
                eprintln!("      | {}", line.trim_end());
            }
            if let Some(state) = failure_state.take() {
                eprintln!("      | cpu: {state}");
            }
        }

        match (rom.expected_failure, outcome.passed()) {
            (Some(reason), false) => known_failures.push(format!("{}: {reason}", rom.name)),
            (Some(_), true) => unexpected_passes.push(rom.name),
            (None, _) => report.record(rom.name, &outcome),
        }
    }

    if !known_failures.is_empty() {
        eprintln!("\nknown failures (tracked, not regressions):");
        for entry in &known_failures {
            eprintln!("  {entry}");
        }
    }

    let summary = report.summary();
    eprintln!("\nGame Boy accuracy suite: {summary}");

    if report.passed.is_empty() && report.failed.is_empty() && !report.skipped.is_empty() {
        eprintln!("no test ROMs present; run `cargo xtask fetch-test-roms` to enable this suite");
        return;
    }

    assert!(
        unexpected_passes.is_empty(),
        "these ROMs are marked as expected failures but passed; remove the marker: {unexpected_passes:?}"
    );
    assert!(report.is_success(), "{summary}");
}

// ---------------------------------------------------------------------------
// The GBA rendering golden manifest
// ---------------------------------------------------------------------------

/// Runs `testing/golden/gba.toml` — see [`crate::golden`] for the mechanism.
///
/// One test for the whole manifest, same reasoning as `gb_accuracy_suite`: a single report
/// naming every divergence is more useful than stopping at the first, and an unfetched corpus
/// produces one clear skip message rather than five.
#[test]
fn gba_golden_frames() {
    let summary = crate::golden::run_golden_manifest();

    if summary.checked.is_empty() && summary.pending.is_empty() && !summary.skipped.is_empty() {
        eprintln!("no golden ROMs present; run `cargo xtask fetch-test-roms` to enable this suite");
        return;
    }

    eprintln!(
        "\nGBA golden manifest: {} checked, {} pending, {} skipped",
        summary.checked.len(),
        summary.pending.len(),
        summary.skipped.len()
    );
    if !summary.pending.is_empty() {
        eprintln!(
            "  pending (no independent reference recorded yet): {}",
            summary.pending.join(", ")
        );
    }
    if !summary.skipped.is_empty() {
        eprintln!(
            "  skipped (run `cargo xtask fetch-test-roms`): {}",
            summary.skipped.join(", ")
        );
    }

    assert!(
        summary.is_success(),
        "{} golden mismatch(es); the rendered frame for each is on disk — see the path printed \
         above for each one:\n{}",
        summary.mismatches.len(),
        summary
            .mismatches
            .iter()
            .map(|m| format!(
                "  {} frame {}: expected {}, got {} -> {}",
                m.case,
                m.frame,
                m.expected,
                m.actual,
                m.png_path.display()
            ))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

// ---------------------------------------------------------------------------
// Banking
// ---------------------------------------------------------------------------

#[test]
fn mbc1_banking_reaches_every_bank_of_a_real_rom() {
    use core_common::Bus;
    // The combined cpu_instrs ROM hangs where every one of its sub-tests passes standalone,
    // which points at banking rather than at any instruction. This checks the mapper against
    // a real multi-bank cartridge directly.
    let Some(bytes) = GB_ROMS
        .iter()
        .find(|r| r.name == "blargg_cpu_instrs")
        .and_then(|r| r.load())
    else {
        return;
    };
    let banks = bytes.len() / 0x4000;
    assert!(banks >= 4, "expected a multi-bank ROM");

    let mut gb = system_gb::GbSystem::new(bytes.clone(), None).unwrap();
    for bank in 1..banks {
        gb.bus_mut().write8(0x2000, bank as u8);
        for offset in [0u16, 0x100, 0x1234, 0x3FFF] {
            let got = gb.bus_mut().read8(0x4000 + offset as u32);
            let want = bytes[bank * 0x4000 + offset as usize];
            assert_eq!(
                got, want,
                "bank {bank} offset {offset:#06X}: mapper returned {got:#04X}, ROM has {want:#04X}"
            );
        }
    }

    // And the fixed low window always shows bank 0.
    for offset in [0x100u16, 0x2000, 0x3FFF] {
        assert_eq!(
            gb.bus_mut().read8(offset as u32),
            bytes[offset as usize],
            "the low window must stay on bank 0"
        );
    }
}

/// Print every Blargg sound sub-test's result code and message.
///
/// Not an assertion — a diagnostic. Blargg's memory protocol carries the *reason* a test
/// failed ("Exiting negate mode after calculation disables channel"), and that string is what
/// turns a red line in the suite into something fixable. `#[ignore]`d because it needs the
/// fetched corpus and prints rather than checks.
///
/// Run with: `cargo test -p harness --release -- --ignored --nocapture dmg_sound_results`
#[test]
#[ignore = "diagnostic; needs the fetched ROM corpus"]
fn dmg_sound_results() {
    use core_common::{InputState, System};
    let dir = crate::corpus::corpus_dir().join("gb/blargg/dmg_sound");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        eprintln!("no corpus at {}", dir.display());
        return;
    };
    let mut names: Vec<_> = entries
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();

    for name in names {
        let rom = std::fs::read(dir.join(&name)).unwrap();
        let mut gb = system_gb::GbSystem::new(rom, None).unwrap();
        for _ in 0..4000 {
            gb.step_frame(InputState::default());
            if gb.read_byte(0xA001) == 0xDE && gb.read_byte(0xA000) != 0x80 {
                break;
            }
        }
        let mut text = String::new();
        for addr in 0xA004u32..0xA200 {
            match gb.read_byte(addr) {
                0 => break,
                byte => text.push(byte as char),
            }
        }
        println!("=== {name} -> {}\n{}", gb.read_byte(0xA000), text.trim());
    }
}
