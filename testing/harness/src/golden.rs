//! The GBA rendering golden manifest: `testing/golden/gba.toml`.
//!
//! # Why this exists
//!
//! Before this, not one GBA frame in this repository had ever been compared against a
//! reference. `Convention::GbaSuite` checks a register, not a picture; the only two validated
//! framebuffer hashes in the whole corpus are Game Boy ROMs (`dmg-acid2`, `cgb-acid2`). Every
//! GBA rendering bug this project has found — the missing colour-index-0 transparency, the
//! backwards blend, the Game-Boy-shaped sprite priority — was found by a person looking at a
//! picture, not by a test failing. This is the mechanism that replaces the looking.
//!
//! # A trail, not a snapshot
//!
//! Each case names several frames rather than one. A single final-frame hash tells you a run
//! diverged; it cannot tell you *when*, and "when" is most of the diagnosis. Checking
//! `[1, 2, 5, 30, 60]` instead means a regression that only shows up after a few frames of
//! warm-up is caught at the same frame it first appears, rather than lumped in with everything
//! that was already wrong three checkpoints earlier.
//!
//! # `hashes = []` means pending, not passing
//!
//! A hash this emulator produced and never checked against anything independent is not
//! evidence of anything except what this emulator currently does — it would enshrine whatever
//! bug is already present as the expected answer forever. `GoldenCase::hashes` empty is how a
//! case says "recorded, not yet validated": it still runs (so a panic or a ROM that fails to
//! parse is still caught), but nothing is asserted, and `GoldenSummary::pending` says so loudly
//! rather than silently passing. `provenance` is mandatory on every case regardless, including
//! pending ones — see the manifest file's own comments for what it must say.

use crate::corpus;
use core_common::{Framebuffer, InputState, System};
use serde::Deserialize;
use std::path::{Path, PathBuf};

/// One `[[case]]` in `testing/golden/gba.toml`.
#[derive(Debug, Deserialize)]
pub struct GoldenCase {
    /// Stable identifier, used to name a failure's PNG and to report which case a checkpoint
    /// belongs to.
    pub name: String,
    /// Path under the corpus directory — the same directory `corpus::TestRom::path` resolves
    /// against, so a ROM already in the accuracy corpus needs no second copy.
    pub rom: String,
    /// Frame numbers to hash and check, in ascending order.
    pub frames: Vec<u32>,
    /// The hash expected at each of `frames`, one-for-one. Empty means pending — see the
    /// module docs.
    #[serde(default)]
    pub hashes: Vec<String>,
    /// Mandatory free text recording how `hashes` was validated against an independent
    /// reference, or why it has not been yet. Enforced by this module's own
    /// `every_case_has_provenance` test.
    pub provenance: String,
}

impl GoldenCase {
    /// `hashes` is filled in and asserted against, rather than merely recorded.
    fn is_validated(&self) -> bool {
        !self.hashes.is_empty()
    }
}

#[derive(Debug, Deserialize)]
struct Manifest {
    case: Vec<GoldenCase>,
}

/// Where the manifest lives, resolved the same way [`corpus::corpus_dir`] resolves the ROM
/// directory: by walking up from this crate rather than from the current directory, so it
/// resolves the same whether a test runs from the workspace root or from a crate directory.
///
/// Unlike the ROM corpus, this file *is* committed — it is data about the project, not a
/// redistributed binary — so there is no "absent manifest" case to handle here the way
/// [`corpus::TestRom::load`] handles an absent ROM.
pub fn manifest_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("testing/harness has a parent")
        .join("golden")
        .join("gba.toml")
}

fn load_manifest() -> Vec<GoldenCase> {
    let path = manifest_path();
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
    let manifest: Manifest =
        toml::from_str(&text).unwrap_or_else(|e| panic!("parsing {}: {e}", path.display()));
    manifest.case
}

/// Where a mismatch's rendered frame is written so it can be looked at, rather than argued
/// about from a hex string. Matches what CI uploads as an artifact on failure.
pub fn fail_dir() -> PathBuf {
    corpus::corpus_dir()
        .parent()
        .expect("test-roms has a parent")
        .parent()
        .expect("testing has a parent")
        .join("target")
        .join("golden-fail")
}

/// One checkpoint that did not match its recorded hash.
#[derive(Debug)]
pub struct GoldenMismatch {
    pub case: String,
    pub frame: u32,
    pub expected: String,
    pub actual: String,
    /// Where the actual frame was written, so the mismatch is a picture, not just two hex
    /// strings that differ.
    pub png_path: PathBuf,
}

/// What running every case in the manifest produced.
#[derive(Debug, Default)]
pub struct GoldenSummary {
    /// `"<case>@<frame>"` for every checkpoint that matched its recorded hash.
    pub checked: Vec<String>,
    /// `"<case>@<frame>=<hash>"` for every checkpoint run under a case with no recorded hash
    /// yet — printed rather than asserted, so a future validation pass has something to start
    /// from without having to re-run anything.
    pub pending: Vec<String>,
    /// Case names whose ROM has not been fetched.
    pub skipped: Vec<String>,
    pub mismatches: Vec<GoldenMismatch>,
}

impl GoldenSummary {
    pub fn is_success(&self) -> bool {
        self.mismatches.is_empty()
    }
}

/// Run every case in `testing/golden/gba.toml` and report what happened.
///
/// Never panics on a mismatch itself — [`GoldenSummary::mismatches`] carries them so the
/// caller can report every divergence in one go rather than stopping at the first, the same
/// reasoning [`crate::SuiteReport`] uses for the other suites.
pub fn run_golden_manifest() -> GoldenSummary {
    let mut summary = GoldenSummary::default();
    for case in load_manifest() {
        run_case(&case, &mut summary);
    }
    summary
}

fn run_case(case: &GoldenCase, summary: &mut GoldenSummary) {
    let rom_path = corpus::corpus_dir().join(&case.rom);
    let Ok(bytes) = std::fs::read(&rom_path) else {
        summary.skipped.push(case.name.clone());
        return;
    };

    let mut system = system_gba::GbaSystem::new(bytes, None)
        .unwrap_or_else(|e| panic!("{}: {} did not parse: {e:?}", case.name, case.rom));

    let max_frame = *case
        .frames
        .iter()
        .max()
        .unwrap_or_else(|| panic!("{}: `frames` is empty", case.name));

    for frame_no in 1..=max_frame {
        system.step_frame(InputState::default());
        let Some(index) = case.frames.iter().position(|&f| f == frame_no) else {
            continue;
        };

        let framebuffer = system.framebuffer();
        let actual = framebuffer.fnv1a_hex();

        if !case.is_validated() {
            summary
                .pending
                .push(format!("{}@{frame_no}={actual}", case.name));
            continue;
        }

        let expected = &case.hashes[index];
        if &actual == expected {
            summary.checked.push(format!("{}@{frame_no}", case.name));
        } else {
            let png_path = write_mismatch_png(&case.name, frame_no, framebuffer);
            eprintln!(
                "GOLDEN MISMATCH {} frame {frame_no}: expected {expected}, got {actual}\n  \
                 wrote {}",
                case.name,
                png_path.display()
            );
            summary.mismatches.push(GoldenMismatch {
                case: case.name.clone(),
                frame: frame_no,
                expected: expected.clone(),
                actual,
                png_path,
            });
        }
    }
}

fn write_mismatch_png(name: &str, frame: u32, framebuffer: &Framebuffer) -> PathBuf {
    let dir = fail_dir();
    std::fs::create_dir_all(&dir).unwrap_or_else(|e| panic!("creating {}: {e}", dir.display()));
    let path = dir.join(format!("{name}-{frame}.png"));
    std::fs::write(&path, frontend_core::encode_png(framebuffer))
        .unwrap_or_else(|e| panic!("writing {}: {e}", path.display()));
    path
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_case_has_provenance() {
        // A hash with no provenance is a guess with authority — see the module docs. This
        // holds even for a pending case: the note there has to say *why* it is pending, not
        // just that it is.
        for case in load_manifest() {
            assert!(
                !case.provenance.trim().is_empty(),
                "{}: every golden case needs a provenance line",
                case.name
            );
        }
    }

    #[test]
    fn hashes_are_empty_or_match_frames_one_for_one() {
        for case in load_manifest() {
            assert!(
                case.hashes.is_empty() || case.hashes.len() == case.frames.len(),
                "{}: {} frames but {} hashes — pending means all of them, not some",
                case.name,
                case.frames.len(),
                case.hashes.len()
            );
        }
    }

    #[test]
    fn frames_are_sorted_and_non_empty() {
        for case in load_manifest() {
            assert!(
                !case.frames.is_empty(),
                "{}: no frame checkpoints",
                case.name
            );
            let mut sorted = case.frames.clone();
            sorted.sort_unstable();
            sorted.dedup();
            assert_eq!(
                case.frames, sorted,
                "{}: frames should be ascending with no repeats, so the trail reads in order",
                case.name
            );
        }
    }

    #[test]
    fn case_names_are_unique() {
        let mut names: Vec<String> = load_manifest().into_iter().map(|c| c.name).collect();
        let count = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), count, "duplicate golden case name");
    }

    #[test]
    fn every_rom_path_stays_inside_the_corpus_directory() {
        // The same reasoning as corpus::tests::the_corpus_directory_is_outside_the_crate: this
        // manifest fetches through the existing, gitignored corpus rather than opening a second
        // path for something to be vendored through by accident.
        for case in load_manifest() {
            assert!(
                !case.rom.starts_with('/') && !case.rom.contains(".."),
                "{}: `rom` must be a path inside the corpus directory, got `{}`",
                case.name,
                case.rom
            );
        }
    }
}
