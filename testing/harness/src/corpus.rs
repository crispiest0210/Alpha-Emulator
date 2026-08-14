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
    /// `gba-suite`'s convention: run until the CPU stops making progress, then read `r12`.
    /// Zero means every sub-test passed; anything else is the number of the one that failed.
    GbaSuite,
}

/// One fetchable ROM.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hardware {
    /// Original Game Boy.
    Dmg,
    /// Game Boy Color. The cartridge header still decides colour versus compatibility mode.
    Cgb,
    /// Game Boy Advance.
    Gba,
}

pub struct TestRom {
    /// Stable identifier used in test names and snapshot keys.
    pub name: &'static str,
    /// Where to download it.
    pub url: &'static str,
    /// Path under the corpus directory.
    pub path: &'static str,
    pub convention: Convention,
    /// Which machine to run it on.
    ///
    /// Separate from the file extension because the extension is not authoritative: a `.gb`
    /// file is often a CGB-enhanced cartridge, and several CGB test ROMs ship as `.gb`. What
    /// decides is what the ROM is *testing*, which only the corpus knows.
    pub hardware: Hardware,
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
        hardware: Hardware::Dmg,
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
        hardware: Hardware::Dmg,
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
        hardware: Hardware::Dmg,
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
        hardware: Hardware::Dmg,
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
        hardware: Hardware::Dmg,
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
        hardware: Hardware::Dmg,
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
        hardware: Hardware::Dmg,
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
        hardware: Hardware::Dmg,
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
        hardware: Hardware::Dmg,
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
        hardware: Hardware::Dmg,
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
        hardware: Hardware::Dmg,
        max_frames: 4000,
        expected_hash: None,
        expected_failure: None,
        licence: "Blargg's test ROMs, published for emulator authors; freely circulated.",
    },
];

/// Blargg's `dmg_sound`, one ROM per sub-test.
///
/// Same reasoning as the `cpu_instrs` split: the combined ROM produces no output at all, and
/// an aggregate silence says nothing about which of twelve areas is at fault.
pub const GB_DMG_SOUND_SINGLES: &[TestRom] = &[
    TestRom {
        name: "dmg_sound_01_registers",
        url: "https://raw.githubusercontent.com/retrio/gb-test-roms/master/dmg_sound/rom_singles/01-registers.gb",
        path: "gb/blargg/dmg_sound/01-registers.gb",
        convention: Convention::BlarggMemory,
        hardware: Hardware::Dmg,
        max_frames: 4000,
        expected_hash: None,
        expected_failure: None,
        licence: "Blargg's test ROMs, published for emulator authors; freely circulated.",
    },
    TestRom {
        name: "dmg_sound_02_len_ctr",
        url: "https://raw.githubusercontent.com/retrio/gb-test-roms/master/dmg_sound/rom_singles/02-len%20ctr.gb",
        path: "gb/blargg/dmg_sound/02-len_ctr.gb",
        convention: Convention::BlarggMemory,
        hardware: Hardware::Dmg,
        max_frames: 4000,
        expected_hash: None,
        expected_failure: None,
        licence: "Blargg's test ROMs, published for emulator authors; freely circulated.",
    },
    TestRom {
        name: "dmg_sound_03_trigger",
        url: "https://raw.githubusercontent.com/retrio/gb-test-roms/master/dmg_sound/rom_singles/03-trigger.gb",
        path: "gb/blargg/dmg_sound/03-trigger.gb",
        convention: Convention::BlarggMemory,
        hardware: Hardware::Dmg,
        max_frames: 4000,
        expected_hash: None,
        expected_failure: None,
        licence: "Blargg's test ROMs, published for emulator authors; freely circulated.",
    },
    TestRom {
        name: "dmg_sound_04_sweep",
        url: "https://raw.githubusercontent.com/retrio/gb-test-roms/master/dmg_sound/rom_singles/04-sweep.gb",
        path: "gb/blargg/dmg_sound/04-sweep.gb",
        convention: Convention::BlarggMemory,
        hardware: Hardware::Dmg,
        max_frames: 4000,
        expected_hash: None,
        expected_failure: None,
        licence: "Blargg's test ROMs, published for emulator authors; freely circulated.",
    },
    TestRom {
        name: "dmg_sound_05_sweep_details",
        url: "https://raw.githubusercontent.com/retrio/gb-test-roms/master/dmg_sound/rom_singles/05-sweep%20details.gb",
        path: "gb/blargg/dmg_sound/05-sweep_details.gb",
        convention: Convention::BlarggMemory,
        hardware: Hardware::Dmg,
        max_frames: 4000,
        expected_hash: None,
        expected_failure: None,
        licence: "Blargg's test ROMs, published for emulator authors; freely circulated.",
    },
    TestRom {
        name: "dmg_sound_06_overflow_on_trigger",
        url: "https://raw.githubusercontent.com/retrio/gb-test-roms/master/dmg_sound/rom_singles/06-overflow%20on%20trigger.gb",
        path: "gb/blargg/dmg_sound/06-overflow_on_trigger.gb",
        convention: Convention::BlarggMemory,
        hardware: Hardware::Dmg,
        max_frames: 4000,
        expected_hash: None,
        expected_failure: None,
        licence: "Blargg's test ROMs, published for emulator authors; freely circulated.",
    },
    TestRom {
        name: "dmg_sound_07_len_sweep_period_sync",
        url: "https://raw.githubusercontent.com/retrio/gb-test-roms/master/dmg_sound/rom_singles/07-len%20sweep%20period%20sync.gb",
        path: "gb/blargg/dmg_sound/07-len_sweep_period_sync.gb",
        convention: Convention::BlarggMemory,
        hardware: Hardware::Dmg,
        max_frames: 4000,
        expected_hash: None,
        expected_failure: None,
        licence: "Blargg's test ROMs, published for emulator authors; freely circulated.",
    },
    TestRom {
        name: "dmg_sound_08_len_ctr_during_power",
        url: "https://raw.githubusercontent.com/retrio/gb-test-roms/master/dmg_sound/rom_singles/08-len%20ctr%20during%20power.gb",
        path: "gb/blargg/dmg_sound/08-len_ctr_during_power.gb",
        convention: Convention::BlarggMemory,
        hardware: Hardware::Dmg,
        max_frames: 4000,
        expected_hash: None,
        expected_failure: None,
        licence: "Blargg's test ROMs, published for emulator authors; freely circulated.",
    },
    TestRom {
        name: "dmg_sound_09_wave_read_while_on",
        url: "https://raw.githubusercontent.com/retrio/gb-test-roms/master/dmg_sound/rom_singles/09-wave%20read%20while%20on.gb",
        path: "gb/blargg/dmg_sound/09-wave_read_while_on.gb",
        convention: Convention::BlarggMemory,
        hardware: Hardware::Dmg,
        max_frames: 4000,
        expected_hash: None,
        expected_failure: Some(
            "the DMG wave-RAM access window is modelled but only to M-cycle resolution: the CPU sees the byte when channel 3 advanced during the same machine cycle, and 0xFF otherwise. This ROM resolves the window to single t-cycles, so closing it needs the APU stepped at finer than M-cycle granularity",
        ),
        licence: "Blargg's test ROMs, published for emulator authors; freely circulated.",
    },
    TestRom {
        name: "dmg_sound_10_wave_trigger_while_on",
        url: "https://raw.githubusercontent.com/retrio/gb-test-roms/master/dmg_sound/rom_singles/10-wave%20trigger%20while%20on.gb",
        path: "gb/blargg/dmg_sound/10-wave_trigger_while_on.gb",
        convention: Convention::BlarggMemory,
        hardware: Hardware::Dmg,
        max_frames: 4000,
        expected_hash: None,
        expected_failure: Some(
            "triggering channel 3 while it plays corrupts the first bytes of wave RAM on a DMG. That corruption is not modelled",
        ),
        licence: "Blargg's test ROMs, published for emulator authors; freely circulated.",
    },
    TestRom {
        name: "dmg_sound_11_regs_after_power",
        url: "https://raw.githubusercontent.com/retrio/gb-test-roms/master/dmg_sound/rom_singles/11-regs%20after%20power.gb",
        path: "gb/blargg/dmg_sound/11-regs_after_power.gb",
        convention: Convention::BlarggMemory,
        hardware: Hardware::Dmg,
        max_frames: 4000,
        expected_hash: None,
        expected_failure: None,
        licence: "Blargg's test ROMs, published for emulator authors; freely circulated.",
    },
    TestRom {
        name: "dmg_sound_12_wave_write_while_on",
        url: "https://raw.githubusercontent.com/retrio/gb-test-roms/master/dmg_sound/rom_singles/12-wave%20write%20while%20on.gb",
        path: "gb/blargg/dmg_sound/12-wave_write_while_on.gb",
        convention: Convention::BlarggMemory,
        hardware: Hardware::Dmg,
        max_frames: 4000,
        expected_hash: None,
        expected_failure: Some(
            "the write half of the same M-cycle-resolution limit as 09",
        ),
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
        hardware: Hardware::Dmg,
        // Eleven sub-tests back to back; the slowest alone needs several hundred frames.
        max_frames: 20000,
        expected_hash: None,
        expected_failure: Some(
            "executes STOP while running its copied-to-WRAM runner at pc=0xC304, after \
             printing sub-test 03's name. Not an instruction bug: all eleven sub-tests pass \
             individually below, and MBC1 bank reads are verified against this exact ROM. \
             Either STOP is reached because something earlier went wrong, or STOP itself is \
             mishandled — a DMG resumes from STOP on a joypad line going low",
        ),
        licence: "Blargg's test ROMs, published for emulator authors; freely circulated.",
    },
    TestRom {
        name: "blargg_instr_timing",
        url: "https://raw.githubusercontent.com/retrio/gb-test-roms/master/instr_timing/instr_timing.gb",
        path: "gb/blargg/instr_timing.gb",
        convention: Convention::BlarggSerial,
        hardware: Hardware::Dmg,
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
        hardware: Hardware::Dmg,
        max_frames: 1200,
        expected_hash: None,
        expected_failure: None,
        licence: "Blargg's test ROMs, published for emulator authors; freely circulated.",
    },
    TestRom {
        name: "blargg_dmg_sound",
        url: "https://raw.githubusercontent.com/retrio/gb-test-roms/master/dmg_sound/dmg_sound.gb",
        path: "gb/blargg/dmg_sound.gb",
        convention: Convention::BlarggMemory,
        hardware: Hardware::Dmg,
        max_frames: 6000,
        expected_hash: None,
        expected_failure: Some(
            "the combined ROM stops at the first sub-test that fails; see 09, 10, and 12 below",
        ),
        licence: "Blargg's test ROMs, published for emulator authors; freely circulated.",
    },
    TestRom {
        name: "dmg_acid2",
        url: "https://github.com/mattcurrie/dmg-acid2/releases/download/v1.0/dmg-acid2.gb",
        path: "gb/dmg-acid2.gb",
        convention: Convention::Framebuffer,
        hardware: Hardware::Dmg,
        // It draws its face within a few frames.
        max_frames: 60,
        // **Validated against the published reference image**, and the only PPU check in the
        // corpus that is. The comparison was done once, offline, and this hash is its result:
        //
        //   cargo run -p frontend-headless -- run testing/test-roms/gb/dmg-acid2.gb \
        //       --frames 60 --save-frame /tmp/mine.png
        //   curl -sSfLO https://raw.githubusercontent.com/mattcurrie/dmg-acid2/master/img/reference-dmg.png
        //   # then compare pixel-for-pixel; the reference is 2-bit greyscale, so a decoder has to
        //   # scale samples to 8 bits rather than shifting them left
        //
        // All 23 040 pixels matched. The reference image is *not* committed — same rule as the
        // ROMs — so redoing this means fetching it again, which is why the commands are here.
        //
        // This ROM is designed so each individual PPU bug corrupts one small region of the face,
        // which is why an exact match is worth so much more than "the picture looks right": it
        // covers sprite priority, both tile-mapping arrangements, the window, and BG-to-OBJ
        // priority in one assertion.
        expected_hash: Some("17a0f9970ac4d084"),
        expected_failure: None,
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
    GB_ROMS
        .iter()
        .chain(GB_CPU_INSTRS_SUBTESTS)
        .chain(GB_DMG_SOUND_SINGLES)
}

/// Game Boy Color ROMs.
///
/// The CGB had no accuracy coverage at all until these landed, which made it the project's
/// largest testing hole: colour rendering, the speed switch, and VRAM DMA were all checked
/// against hardware documentation and unit tests rather than against a reference.
///
/// Blargg's `cgb_sound` is the same suite as `dmg_sound` rebuilt for colour hardware, so it
/// exercises the APU *and* the CGB boot path that gets it there. `cgb-acid2` is the colour
/// counterpart of `dmg-acid2` and is the only thing that checks tile attributes, the second
/// VRAM bank, and CGB sprite priority end to end.
pub const CGB_ROMS: &[TestRom] = &[
    TestRom {
        name: "cgb_acid2",
        url: "https://github.com/mattcurrie/cgb-acid2/releases/download/v1.1/cgb-acid2.gbc",
        path: "gbc/cgb-acid2.gbc",
        convention: Convention::Framebuffer,
        hardware: Hardware::Cgb,
        max_frames: 60,
        // **Validated against the published reference image**: all 23 040 pixels match. Same
        // procedure as dmg-acid2 above, with the reference at
        // `https://raw.githubusercontent.com/mattcurrie/cgb-acid2/master/img/reference.png`.
        //
        // Getting here found two bugs, both of them the same shape: a CGB read the DMG way. Neither
        // produced an error, a panic, or a blank screen — each produced a complete, plausible,
        // wrong picture, which is why only a reference comparison could catch them.
        //
        //   1. Sprite attributes were decoded with the DMG's rule. Bit 4 selects OBP0 or OBP1 on a
        //      DMG; on a CGB bits 0-2 are one of eight OBJ palettes and bit 3 selects the VRAM bank
        //      the tile data comes from. Every sprite therefore drew through OBJ palette 0 with
        //      bank 0 tiles. This ROM's "HELLO WORLD!" banner is eight sprites naming palette 3,
        //      and they came out the right shapes in the wrong colours.
        //   2. Sprites were ordered by X coordinate. That is the DMG rule; a CGB running a colour
        //      game orders by OAM index alone. Worth 12 pixels here — a small dot that should have
        //      been hidden behind another sprite.
        //
        // This is now the only end-to-end check of CGB tile attributes, the second VRAM bank, OBJ
        // palettes, and CGB sprite priority, which is exactly what it was predicted to become.
        expected_hash: Some("71a4a863fe5bcde0"),
        expected_failure: None,
        licence: "MIT (Matt Currie)",
    },
    TestRom {
        name: "cgb_sound",
        url: "https://raw.githubusercontent.com/retrio/gb-test-roms/master/cgb_sound/cgb_sound.gb",
        path: "gbc/blargg/cgb_sound.gb",
        convention: Convention::BlarggMemory,
        hardware: Hardware::Cgb,
        max_frames: 4000,
        expected_hash: None,
        expected_failure: Some(
            "eleven of twelve sub-tests pass. Only 09 (wave read while on) fails, and for the \
             same reason its DMG counterpart does: the wave-RAM access window is modelled to \
             machine-cycle resolution and this ROM resolves it to single t-cycles. Closing it \
             means stepping the APU finer than one machine cycle",
        ),
        licence: "public domain (Shay Green / blargg)",
    },
];

/// Game Boy Advance ROMs.
///
/// `gba-suite` is the first accuracy coverage this project has for either the GBA or the
/// ARM7TDMI core underneath it — the CPU passed its own unit tests but had never been run
/// against a reference. Its sub-suites are separate ROMs so a failure names the instruction
/// class rather than only "the CPU".
pub const GBA_ROMS: &[TestRom] = &[
    TestRom {
        name: "gba_suite_arm",
        url: "https://github.com/jsmolka/gba-tests/raw/master/arm/arm.gba",
        path: "gba/gba-suite/arm.gba",
        convention: Convention::GbaSuite,
        hardware: Hardware::Gba,
        max_frames: 600,
        expected_hash: None,
        expected_failure: None,
        licence: "MIT (Julian Smolka)",
    },
    TestRom {
        name: "gba_suite_thumb",
        url: "https://github.com/jsmolka/gba-tests/raw/master/thumb/thumb.gba",
        path: "gba/gba-suite/thumb.gba",
        convention: Convention::GbaSuite,
        hardware: Hardware::Gba,
        max_frames: 600,
        expected_hash: None,
        expected_failure: None,
        licence: "MIT (Julian Smolka)",
    },
    TestRom {
        name: "gba_suite_memory",
        url: "https://github.com/jsmolka/gba-tests/raw/master/memory/memory.gba",
        path: "gba/gba-suite/memory.gba",
        convention: Convention::GbaSuite,
        hardware: Hardware::Gba,
        max_frames: 600,
        expected_hash: None,
        expected_failure: None,
        licence: "MIT (Julian Smolka)",
    },
    TestRom {
        name: "gba_suite_bios",
        url: "https://github.com/jsmolka/gba-tests/raw/master/bios/bios.gba",
        path: "gba/gba-suite/bios.gba",
        convention: Convention::GbaSuite,
        hardware: Hardware::Gba,
        max_frames: 600,
        expected_hash: None,
        expected_failure: Some(
            "sub-test 1: reading BIOS memory from outside the BIOS right after startup must \
             return the specific opcode a real BIOS's boot sequence last fetched, not zero. This \
             machine never executes real BIOS code with no BIOS supplied, so nothing produces \
             that trace yet; the fix is a documented per-checkpoint constant, not something this \
             call implementation touches",
        ),
        licence: "MIT (Julian Smolka)",
    },
];

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
            hardware: Hardware::Dmg,
            max_frames: 1,
            expected_hash: None,
            expected_failure: None,
            licence: "n/a",
        };
        assert!(!missing.is_present());
        assert_eq!(missing.load(), None);
    }
}
