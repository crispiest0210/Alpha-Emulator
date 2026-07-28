//! The test-ROM corpus: what to fetch, from where, and how to judge it.
//!
//! # Licensing
//!
//! Nothing here is committed to the repository. Each suite is downloaded from its own
//! upstream release at setup time into a gitignored directory. The `licence` field records
//! what is known about each suite's terms, and it is deliberately *not* an assertion that
//! redistribution is permitted — this project fetches rather than redistributes precisely so
//! that question does not have to be answered on anyone's behalf.
//!
//! The predecessor committed test ROM binaries and a commercial game ROM to its repository.
//! That must not happen here under any circumstances, which is why the fetch path is the only
//! path and `testing/test-roms/` is gitignored.

use std::path::{Path, PathBuf};

/// How a suite reports its result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Convention {
    /// Prints a human-readable report over the link port.
    BlarggSerial,
    /// Writes a signature and status byte into cartridge RAM.
    BlarggMemory,
    /// Loads a fixed register pattern and halts.
    Mooneye,
    /// The rendered picture is the result; compare against a snapshot.
    Framebuffer,
}

/// One fetchable ROM.
#[derive(Debug, Clone, Copy)]
pub struct TestRom {
    /// Stable identifier used in test names and snapshot keys.
    pub name: &'static str,
    /// Where to download it.
    pub url: &'static str,
    /// Path under the corpus directory.
    pub path: &'static str,
    pub convention: Convention,
    /// Frames to run before giving up.
    ///
    /// Generous: these suites are slow, and a timeout that is too tight reads as a failure
    /// and sends someone hunting a bug that is not there.
    pub max_frames: u32,
    /// For [`Convention::Framebuffer`]: the hash a correct emulator produces.
    ///
    /// `None` means nobody has validated the output against hardware or a reference image
    /// yet, and the harness reports the ROM as **unvalidated** rather than as a pass. A
    /// picture that merely rendered is not a picture that rendered *correctly*, and calling it
    /// a pass would be the harness lying about the one thing it exists to establish.
    pub expected_hash: Option<&'static str>,
    /// A known bug, if this ROM is currently expected to fail.
    ///
    /// Recorded rather than deleted so the suite stays useful: a known failure is not a
    /// regression, and the suite still fails loudly if anything *else* breaks — or if this
    /// starts passing, which means the note should be removed.
    pub expected_failure: Option<&'static str>,
    pub licence: &'static str,
}

/// Blargg's `cpu_instrs`, one ROM per sub-test.
///
/// The combined ROM is also in the corpus, but a single aggregate result is a poor diagnostic:
/// when one sub-test hangs, the run stops and the other ten are neither confirmed nor denied.
/// Per-sub-test ROMs name exactly which instruction group is broken, and a hang in one does
/// not hide the rest.
pub const GB_CPU_INSTRS_SUBTESTS: &[TestRom] = &[
    TestRom {
        name: "cpu_instrs_01_special",
        url: "https://raw.githubusercontent.com/retrio/gb-test-roms/master/cpu_instrs/individual/01-special.gb",
        path: "gb/blargg/cpu_instrs/01-special.gb",
        convention: Convention::BlarggSerial,
        max_frames: 4000,
        expected_hash: None,
        expected_failure: None,
        licence: "Blargg's test ROMs, published for emulator authors; freely circulated.",
    },
    TestRom {
        name: "cpu_instrs_02_interrupts",
        url: "https://raw.githubusercontent.com/retrio/gb-test-roms/master/cpu_instrs/individual/02-interrupts.gb",
        path: "gb/blargg/cpu_instrs/02-interrupts.gb",
        convention: Convention::BlarggSerial,
        max_frames: 4000,
        expected_hash: None,
        expected_failure: None,
        licence: "Blargg's test ROMs, published for emulator authors; freely circulated.",
    },
    TestRom {
        name: "cpu_instrs_03_op_sp_hl",
        url: "https://raw.githubusercontent.com/retrio/gb-test-roms/master/cpu_instrs/individual/03-op%20sp,hl.gb",
        path: "gb/blargg/cpu_instrs/03-op_sp_hl.gb",
        convention: Convention::BlarggSerial,
        max_frames: 4000,
        expected_hash: None,
        expected_failure: None,
        licence: "Blargg's test ROMs, published for emulator authors; freely circulated.",
    },
    TestRom {
        name: "cpu_instrs_04_op_r_imm",
        url: "https://raw.githubusercontent.com/retrio/gb-test-roms/master/cpu_instrs/individual/04-op%20r,imm.gb",
        path: "gb/blargg/cpu_instrs/04-op_r_imm.gb",
        convention: Convention::BlarggSerial,
        max_frames: 4000,
        expected_hash: None,
        expected_failure: None,
        licence: "Blargg's test ROMs, published for emulator authors; freely circulated.",
    },
    TestRom {
        name: "cpu_instrs_05_op_rp",
        url: "https://raw.githubusercontent.com/retrio/gb-test-roms/master/cpu_instrs/individual/05-op%20rp.gb",
        path: "gb/blargg/cpu_instrs/05-op_rp.gb",
        convention: Convention::BlarggSerial,
        max_frames: 4000,
        expected_hash: None,
        expected_failure: None,
        licence: "Blargg's test ROMs, published for emulator authors; freely circulated.",
    },
    TestRom {
        name: "cpu_instrs_06_ld_r_r",
        url: "https://raw.githubusercontent.com/retrio/gb-test-roms/master/cpu_instrs/individual/06-ld%20r,r.gb",
        path: "gb/blargg/cpu_instrs/06-ld_r_r.gb",
        convention: Convention::BlarggSerial,
        max_frames: 4000,
        expected_hash: None,
        expected_failure: None,
        licence: "Blargg's test ROMs, published for emulator authors; freely circulated.",
    },
    TestRom {
        name: "cpu_instrs_07_jr_jp_call_ret_rst",
        url: "https://raw.githubusercontent.com/retrio/gb-test-roms/master/cpu_instrs/individual/07-jr,jp,call,ret,rst.gb",
        path: "gb/blargg/cpu_instrs/07-jr_jp_call_ret_rst.gb",
        convention: Convention::BlarggSerial,
        max_frames: 4000,
        expected_hash: None,
        expected_failure: None,
        licence: "Blargg's test ROMs, published for emulator authors; freely circulated.",
    },
    TestRom {
        name: "cpu_instrs_08_misc_instrs",
        url: "https://raw.githubusercontent.com/retrio/gb-test-roms/master/cpu_instrs/individual/08-misc%20instrs.gb",
        path: "gb/blargg/cpu_instrs/08-misc_instrs.gb",
        convention: Convention::BlarggSerial,
        max_frames: 4000,
        expected_hash: None,
        expected_failure: None,
        licence: "Blargg's test ROMs, published for emulator authors; freely circulated.",
    },
    TestRom {
        name: "cpu_instrs_09_op_r_r",
        url: "https://raw.githubusercontent.com/retrio/gb-test-roms/master/cpu_instrs/individual/09-op%20r,r.gb",
        path: "gb/blargg/cpu_instrs/09-op_r_r.gb",
        convention: Convention::BlarggSerial,
        max_frames: 4000,
        expected_hash: None,
        expected_failure: None,
        licence: "Blargg's test ROMs, published for emulator authors; freely circulated.",
    },
    TestRom {
        name: "cpu_instrs_10_bit_ops",
        url: "https://raw.githubusercontent.com/retrio/gb-test-roms/master/cpu_instrs/individual/10-bit%20ops.gb",
        path: "gb/blargg/cpu_instrs/10-bit_ops.gb",
        convention: Convention::BlarggSerial,
        max_frames: 4000,
        expected_hash: None,
        expected_failure: None,
        licence: "Blargg's test ROMs, published for emulator authors; freely circulated.",
    },
    TestRom {
        name: "cpu_instrs_11_op_a_hl",
        url: "https://raw.githubusercontent.com/retrio/gb-test-roms/master/cpu_instrs/individual/11-op%20a,(hl).gb",
        path: "gb/blargg/cpu_instrs/11-op_a_hl.gb",
        convention: Convention::BlarggSerial,
        max_frames: 4000,
        expected_hash: None,
        expected_failure: None,
        licence: "Blargg's test ROMs, published for emulator authors; freely circulated.",
    },
];

/// The Game Boy corpus.
///
/// Deliberately small and end-to-end rather than exhaustive: these four are what prompts 03,
/// 07, 08, 09, and 11 name as their acceptance criteria, and getting them running proves the
/// harness before the much larger GBA and DS suites lean on it.
pub const GB_ROMS: &[TestRom] = &[
    TestRom {
        name: "blargg_cpu_instrs",
        url: "https://raw.githubusercontent.com/retrio/gb-test-roms/master/cpu_instrs/cpu_instrs.gb",
        path: "gb/blargg/cpu_instrs.gb",
        convention: Convention::BlarggSerial,
        // Eleven sub-tests back to back; the slowest alone needs several hundred frames.
        max_frames: 20000,
        expected_hash: None,
        expected_failure: Some(
            "hangs after printing sub-test 03's name. Not a CPU bug: all eleven sub-tests \
             pass individually below, and MBC1 bank reads are verified against this exact \
             ROM by `mbc1_banking_reaches_every_bank_of_a_real_rom`. Cause still unknown — \
             something in the combined ROM's own sequencing between sub-tests",
        ),
        licence: "Blargg's test ROMs, published for emulator authors; freely circulated.",
    },
    TestRom {
        name: "blargg_instr_timing",
        url: "https://raw.githubusercontent.com/retrio/gb-test-roms/master/instr_timing/instr_timing.gb",
        path: "gb/blargg/instr_timing.gb",
        convention: Convention::BlarggSerial,
        max_frames: 1200,
        expected_hash: None,
        expected_failure: None,
        licence: "Blargg's test ROMs, published for emulator authors; freely circulated.",
    },
    TestRom {
        name: "blargg_mem_timing",
        url: "https://raw.githubusercontent.com/retrio/gb-test-roms/master/mem_timing/mem_timing.gb",
        path: "gb/blargg/mem_timing.gb",
        convention: Convention::BlarggSerial,
        max_frames: 1200,
        expected_hash: None,
        expected_failure: None,
        licence: "Blargg's test ROMs, published for emulator authors; freely circulated.",
    },
    TestRom {
        name: "blargg_dmg_sound",
        url: "https://raw.githubusercontent.com/retrio/gb-test-roms/master/dmg_sound/dmg_sound.gb",
        path: "gb/blargg/dmg_sound.gb",
        convention: Convention::BlarggSerial,
        max_frames: 6000,
        expected_hash: None,
        expected_failure: Some("produces no serial output at all within the frame budget"),
        licence: "Blargg's test ROMs, published for emulator authors; freely circulated.",
    },
    TestRom {
        name: "dmg_acid2",
        url: "https://github.com/mattcurrie/dmg-acid2/releases/download/v1.0/dmg-acid2.gb",
        path: "gb/dmg-acid2.gb",
        convention: Convention::Framebuffer,
        // It draws its face within a few frames.
        max_frames: 60,
        // Unvalidated: the emulator renders *something*, but nobody has checked it against
        // the reference image. Filling this in requires comparing to dmg-acid2's published
        // reference, and until then the harness must not claim this passes.
        expected_hash: None,
        expected_failure: Some(
            "no validated reference hash recorded yet; the rendered output has not been \
             compared against dmg-acid2's published reference image",
        ),
        licence: "MIT (Matt Currie).",
    },
];

/// The corpus directory, resolved relative to the workspace root.
///
/// Found by walking up from this crate rather than from the current directory, so it resolves
/// the same whether a test runs from the workspace root or from a crate directory.
pub fn corpus_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("testing/harness has a parent")
        .join("test-roms")
}

impl TestRom {
    pub fn local_path(&self) -> PathBuf {
        corpus_dir().join(self.path)
    }

    pub fn is_present(&self) -> bool {
        self.local_path().is_file()
    }

    /// Load the ROM, or `None` if it has not been fetched.
    ///
    /// `None` rather than an error: an absent corpus makes a test skip, not fail. A
    /// contributor who has not run the fetch step should still get a green `cargo test`.
    pub fn load(&self) -> Option<Vec<u8>> {
        std::fs::read(self.local_path()).ok()
    }
}

/// Every ROM across every system.
pub fn all_roms() -> impl Iterator<Item = &'static TestRom> {
    GB_ROMS.iter().chain(GB_CPU_INSTRS_SUBTESTS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_rom_is_fully_specified() {
        for rom in all_roms() {
            assert!(!rom.name.is_empty());
            assert!(
                rom.url.starts_with("https://"),
                "{}: insecure URL",
                rom.name
            );
            assert!(
                rom.path.contains('/'),
                "{}: path needs a system prefix",
                rom.name
            );
            assert!(rom.max_frames > 0, "{}", rom.name);
            assert!(
                !rom.licence.is_empty(),
                "{}: every suite records what is known about its terms",
                rom.name
            );
        }
    }

    #[test]
    fn rom_names_are_unique() {
        // Names key snapshots and test output, so a duplicate would silently overwrite.
        let mut names: Vec<&str> = all_roms().map(|r| r.name).collect();
        let count = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), count, "duplicate ROM name");
    }

    #[test]
    fn the_corpus_directory_is_outside_the_crate_and_gitignored() {
        let dir = corpus_dir();
        assert!(dir.ends_with("test-roms"));
        // The .gitignore entry is what actually keeps ROMs out of the repository, so check it
        // is still there rather than trusting that nobody removed it.
        let gitignore = Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)
            .expect("workspace root")
            .join(".gitignore");
        let text = std::fs::read_to_string(gitignore).expect("the workspace has a .gitignore");
        assert!(
            text.contains("testing/test-roms"),
            "test ROMs must stay out of the repository"
        );
    }

    #[test]
    fn an_absent_rom_loads_as_none_rather_than_erroring() {
        let missing = TestRom {
            name: "nope",
            url: "https://example.invalid/nope.gb",
            path: "gb/definitely-not-here.gb",
            convention: Convention::BlarggSerial,
            max_frames: 1,
            expected_hash: None,
            expected_failure: None,
            licence: "n/a",
        };
        assert!(!missing.is_present());
        assert_eq!(missing.load(), None);
    }
}
