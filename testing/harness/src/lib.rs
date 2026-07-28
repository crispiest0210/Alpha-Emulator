//! The accuracy test-ROM harness.
//!
//! Every accuracy claim anywhere in this project is only as trustworthy as this crate, so its
//! own adapters are unit-tested against synthetic known-pass and known-fail inputs. A bug here
//! would silently invalidate every other acceptance criterion.
//!
//! # Test ROMs are fetched, never vendored
//!
//! Nothing in `testing/test-roms/` is committed, and that directory is gitignored. The
//! predecessor project committed test ROM binaries *and a commercial game ROM* to its
//! repository; this one fetches redistributable test suites from their upstream releases at
//! setup time, with each suite's licence recorded in [`corpus`].
//!
//! A consequence worth stating plainly: **accuracy tests skip when the ROMs are absent**, they
//! do not fail. A contributor who has not run `cargo xtask fetch-test-roms` should get a
//! passing `cargo test`, not a wall of red that trains them to ignore it. CI always fetches,
//! so nothing is silently skipped where it matters — [`SuiteReport::skipped`] makes the
//! distinction visible either way.
//!
//! # One adapter per suite, not one clever guess
//!
//! Suites report results in genuinely different ways, and a single "detect the outcome"
//! function would be a pile of heuristics that silently mis-reads a suite it was not designed
//! for. Instead each convention is its own function:
//!
//! - [`run_blargg_serial`] — the ROM prints its report over the link port.
//! - [`run_blargg_memory`] — the ROM writes a signature and status code into cartridge RAM.
//! - [`run_mooneye`] — the ROM loads a fixed sequence into the registers and halts.
//! - [`capture_framebuffer`] — no self-reporting at all; the picture is the result.

#![deny(unsafe_code)]

pub mod corpus;

use core_common::{Framebuffer, InputState, RegisterValue, System};

/// A system the harness can inspect beyond what [`System`] exposes.
///
/// Test ROMs report results through channels a normal frontend has no reason to look at — the
/// serial port, cartridge RAM, the register file at the moment of a breakpoint. Rather than
/// widen [`System`] with methods only a test harness wants, this sits alongside it.
pub trait TestableSystem: System {
    /// Bytes the ROM has sent over the serial port.
    fn serial_output(&self) -> &[u8];

    /// The CPU register file.
    fn cpu_registers(&self) -> Vec<RegisterValue>;

    /// Read a byte with side effects allowed.
    ///
    /// Not `peek`: Blargg's signature lives in cartridge RAM, which a mapper may gate behind
    /// an enable latch, and a side-effect-free read cannot see it.
    fn read_byte(&mut self, addr: u32) -> u8;
}

/// What running a ROM produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TestOutcome {
    Passed {
        /// Whatever the ROM reported, for the log.
        report: String,
        frames: u32,
    },
    Failed {
        report: String,
        frames: u32,
    },
    /// The ROM never signalled either way within the frame budget.
    ///
    /// Distinct from a failure on purpose: a timeout usually means the emulator hung or the
    /// budget was too small, and reporting it as a plain failure hides which.
    TimedOut {
        report: String,
        frames: u32,
    },
}

impl TestOutcome {
    pub fn passed(&self) -> bool {
        matches!(self, TestOutcome::Passed { .. })
    }

    pub fn report(&self) -> &str {
        match self {
            TestOutcome::Passed { report, .. }
            | TestOutcome::Failed { report, .. }
            | TestOutcome::TimedOut { report, .. } => report,
        }
    }

    pub fn frames(&self) -> u32 {
        match self {
            TestOutcome::Passed { frames, .. }
            | TestOutcome::Failed { frames, .. }
            | TestOutcome::TimedOut { frames, .. } => *frames,
        }
    }
}

/// Interpret whatever a ROM has produced so far.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    Pass,
    Fail,
    /// Nothing conclusive yet; keep running.
    Pending,
}

/// Run until a verdict or the frame budget runs out.
fn run_until<S, F>(system: &mut S, max_frames: u32, mut verdict: F) -> TestOutcome
where
    S: TestableSystem + ?Sized,
    F: FnMut(&mut S) -> (Verdict, String),
{
    for frame in 1..=max_frames {
        system.step_frame(InputState::default());
        let (result, report) = verdict(system);
        match result {
            Verdict::Pass => {
                return TestOutcome::Passed {
                    report,
                    frames: frame,
                }
            }
            Verdict::Fail => {
                return TestOutcome::Failed {
                    report,
                    frames: frame,
                }
            }
            Verdict::Pending => {}
        }
    }
    let (_, report) = verdict(system);
    TestOutcome::TimedOut {
        report,
        frames: max_frames,
    }
}

// ---------------------------------------------------------------------------
// Blargg, via the serial port
// ---------------------------------------------------------------------------

/// Read a Blargg ROM's report from the serial port.
///
/// The ROM prints human-readable text and finishes with `Passed` or `Failed`. Individual
/// sub-tests print their own lines as they go, so the accumulated output doubles as a
/// diagnosis of *which* sub-test broke rather than only that something did.
pub fn run_blargg_serial<S: TestableSystem + ?Sized>(
    system: &mut S,
    max_frames: u32,
) -> TestOutcome {
    run_until(system, max_frames, |system| {
        let text = String::from_utf8_lossy(system.serial_output()).into_owned();
        let verdict = if text.contains("Passed") {
            Verdict::Pass
        } else if text.contains("Failed") {
            Verdict::Fail
        } else {
            Verdict::Pending
        };
        (verdict, text)
    })
}

// ---------------------------------------------------------------------------
// Blargg, via cartridge RAM
// ---------------------------------------------------------------------------

/// Where a Blargg ROM writes its status byte.
const BLARGG_STATUS: u32 = 0xA000;
/// Followed by this three-byte signature, which is how the harness knows the ROM is using
/// this convention at all rather than reading uninitialized RAM as a result.
const BLARGG_SIGNATURE: [u8; 3] = [0xDE, 0xB0, 0x61];
const BLARGG_TEXT: u32 = 0xA004;
/// The status while the test is still running.
const BLARGG_RUNNING: u8 = 0x80;

/// Read a Blargg ROM's report from cartridge RAM.
///
/// Some of the suites write their result to memory instead of, or as well as, the serial
/// port. The signature check matters: without it, uninitialized cartridge RAM that happens to
/// hold zero would read as a pass.
pub fn run_blargg_memory<S: TestableSystem + ?Sized>(
    system: &mut S,
    max_frames: u32,
) -> TestOutcome {
    run_until(system, max_frames, |system| {
        let signature = [
            system.read_byte(BLARGG_STATUS + 1),
            system.read_byte(BLARGG_STATUS + 2),
            system.read_byte(BLARGG_STATUS + 3),
        ];
        if signature != BLARGG_SIGNATURE {
            return (Verdict::Pending, String::new());
        }

        let status = system.read_byte(BLARGG_STATUS);
        if status == BLARGG_RUNNING {
            return (Verdict::Pending, String::new());
        }

        // A NUL-terminated message follows the status.
        let mut text = String::new();
        for offset in 0..512 {
            let byte = system.read_byte(BLARGG_TEXT + offset);
            if byte == 0 {
                break;
            }
            text.push(byte as char);
        }

        let verdict = if status == 0 {
            Verdict::Pass
        } else {
            Verdict::Fail
        };
        (verdict, text)
    })
}

// ---------------------------------------------------------------------------
// Mooneye
// ---------------------------------------------------------------------------

/// The register values a passing Mooneye test leaves behind: the start of the Fibonacci
/// sequence, in `B C D E H L`.
///
/// An arbitrary-looking pattern chosen precisely because it cannot arise by accident — a
/// crashed test is overwhelmingly unlikely to land on all six.
pub const MOONEYE_PASS_REGISTERS: [(&str, u64); 6] = [
    ("B", 3),
    ("C", 5),
    ("D", 8),
    ("E", 13),
    ("H", 21),
    ("L", 34),
];

/// Run a Mooneye acceptance test.
///
/// These ROMs report by loading the sequence above and then executing `LD B,B`, which is a
/// no-op on hardware and a software breakpoint by convention. The harness cannot see the
/// breakpoint, so it checks the registers each frame — the values persist once set, so the
/// only cost of polling rather than trapping is that a test is detected at the end of the
/// frame it finished in.
pub fn run_mooneye<S: TestableSystem + ?Sized>(system: &mut S, max_frames: u32) -> TestOutcome {
    run_until(system, max_frames, |system| {
        let registers = system.cpu_registers();
        let value_of = |name: &str| {
            registers
                .iter()
                .find(|r| r.name == name)
                .map(|r| r.value)
                .unwrap_or(u64::MAX)
        };

        let matches: Vec<String> = MOONEYE_PASS_REGISTERS
            .iter()
            .map(|(name, expected)| format!("{name}={} (want {expected})", value_of(name)))
            .collect();
        let report = matches.join(" ");

        if MOONEYE_PASS_REGISTERS
            .iter()
            .all(|(name, expected)| value_of(name) == *expected)
        {
            return (Verdict::Pass, report);
        }

        // Mooneye signals failure by filling every register with 0x42.
        if MOONEYE_PASS_REGISTERS
            .iter()
            .all(|(name, _)| value_of(name) == 0x42)
        {
            return (Verdict::Fail, "all registers 0x42".to_string());
        }
        (Verdict::Pending, report)
    })
}

// ---------------------------------------------------------------------------
// Framebuffer comparison
// ---------------------------------------------------------------------------

/// Run for a fixed number of frames and return the picture.
///
/// For suites like dmg-acid2 that have no self-reporting convention because the rendered
/// image *is* the result.
pub fn capture_framebuffer<S: TestableSystem + ?Sized>(system: &mut S, frames: u32) -> Framebuffer {
    for _ in 0..frames {
        system.step_frame(InputState::default());
    }
    system.framebuffer().clone()
}

/// A stable, readable digest of a framebuffer.
///
/// FNV-1a rather than a cryptographic hash: this identifies a picture, it does not defend
/// against anyone constructing a collision. Rendered as hex so a snapshot diff shows a changed
/// value rather than a wall of binary.
pub fn framebuffer_hash(framebuffer: &Framebuffer) -> String {
    let mut hash: u64 = 0xCBF2_9CE4_8422_2325;
    for byte in framebuffer.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01B3);
    }
    format!("{hash:016x}")
}

// ---------------------------------------------------------------------------
// Determinism
// ---------------------------------------------------------------------------

/// Why two runs of the same input diverged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DivergenceReport {
    pub frame: u32,
    pub detail: String,
}

/// Check that a system produces identical output twice from the same inputs.
///
/// Provided here rather than reimplemented per system, because every system needs it and it
/// is the property save states, rewind, and this whole harness rest on. Returns the first
/// divergence, if any.
pub fn check_deterministic<S, F>(
    make_system: F,
    inputs: &[InputState],
    frames: u32,
) -> Result<(), DivergenceReport>
where
    S: System,
    F: Fn() -> S,
{
    let mut first = make_system();
    let mut second = make_system();

    for frame in 0..frames {
        // Cycling the inputs lets a short list drive a long run without being all-identical,
        // which would test far less.
        let input = inputs
            .get(frame as usize % inputs.len().max(1))
            .copied()
            .unwrap_or_default();

        let a = first.step_frame(input);
        let b = second.step_frame(input);
        if a != b {
            return Err(DivergenceReport {
                frame,
                detail: format!("frame output differs: {a:?} vs {b:?}"),
            });
        }
        if first.framebuffer() != second.framebuffer() {
            return Err(DivergenceReport {
                frame,
                detail: "framebuffers differ".to_string(),
            });
        }
        if first.take_audio_samples() != second.take_audio_samples() {
            return Err(DivergenceReport {
                frame,
                detail: "audio differs".to_string(),
            });
        }
    }
    Ok(())
}

/// Check that saving, diverging, and reloading lands back on an identical timeline.
///
/// The direct regression test for the predecessor's corrupted-quickload bug class: a state
/// that restores *almost* everything produces frames that drift apart a few frames after the
/// load, which is exactly what this catches.
pub fn check_save_state_round_trip<S, F>(
    make_system: F,
    frames_before: u32,
    frames_after: u32,
) -> Result<(), DivergenceReport>
where
    S: System,
    F: Fn() -> S,
{
    let mut system = make_system();
    for _ in 0..frames_before {
        system.step_frame(InputState::default());
    }
    system.take_audio_samples();
    let state = system.save_state();

    // Reference: carry straight on.
    let mut reference = Vec::new();
    for _ in 0..frames_after {
        system.step_frame(InputState::default());
        reference.push(system.framebuffer().clone());
    }

    // Diverge, reload, replay.
    for _ in 0..frames_after * 2 {
        system.step_frame(InputState {
            buttons: core_common::Buttons::all(),
            touch: None,
        });
    }
    if system.load_state(&state).is_err() {
        return Err(DivergenceReport {
            frame: frames_before,
            detail: "the state failed to load".to_string(),
        });
    }
    system.take_audio_samples();

    for (index, expected) in reference.iter().enumerate() {
        system.step_frame(InputState::default());
        if system.framebuffer() != expected {
            return Err(DivergenceReport {
                frame: frames_before + index as u32,
                detail: format!("frame {index} after the load differs from the reference"),
            });
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Reporting
// ---------------------------------------------------------------------------

/// The result of running one suite, for a human and for CI.
#[derive(Debug, Clone, Default)]
pub struct SuiteReport {
    pub passed: Vec<String>,
    pub failed: Vec<(String, String)>,
    /// ROMs that were not present. Reported separately from failures so an unfetched corpus
    /// is never mistaken for a broken emulator.
    pub skipped: Vec<String>,
}

impl SuiteReport {
    pub fn record(&mut self, name: &str, outcome: &TestOutcome) {
        match outcome {
            TestOutcome::Passed { .. } => self.passed.push(name.to_string()),
            other => self
                .failed
                .push((name.to_string(), other.report().to_string())),
        }
    }

    pub fn skip(&mut self, name: &str) {
        self.skipped.push(name.to_string());
    }

    pub fn is_success(&self) -> bool {
        self.failed.is_empty()
    }

    /// A summary suitable for CI output.
    pub fn summary(&self) -> String {
        let mut lines = vec![format!(
            "{} passed, {} failed, {} skipped",
            self.passed.len(),
            self.failed.len(),
            self.skipped.len()
        )];
        for (name, report) in &self.failed {
            let first_line = report.lines().next().unwrap_or("").trim();
            lines.push(format!("  FAILED {name}: {first_line}"));
        }
        if !self.skipped.is_empty() {
            lines.push(format!(
                "  skipped (run `cargo xtask fetch-test-roms`): {}",
                self.skipped.join(", ")
            ));
        }
        lines.join("\n")
    }
}

#[cfg(test)]
mod tests;

// ---------------------------------------------------------------------------
// System adapters
// ---------------------------------------------------------------------------

impl TestableSystem for system_gb::GbSystem {
    fn serial_output(&self) -> &[u8] {
        &self.bus().serial_output
    }

    fn cpu_registers(&self) -> Vec<RegisterValue> {
        use core_common::CpuIntrospect;
        self.cpu().registers()
    }

    fn read_byte(&mut self, addr: u32) -> u8 {
        use core_common::Bus;
        self.bus_mut().read8(addr)
    }
}
