//! Human-readable dumps of a running machine, for when reasoning from the picture has stalled.
//!
//! `AGENTS.md` singles this kind of tool out: `cgb_acid2_attribute_dump` cracked cgb-acid2 in
//! about a minute after staring at pixels had failed, and the advice is to reach for one *before*
//! reading pixels rather than after. The DS has far more state to get wrong than a Game Boy Color
//! — nine VRAM banks, two engines, eight background layers, a 3D pipeline — so the same trick is
//! worth more here, not less.
//!
//! Everything here is `&self` and side-effect free, so a dump can be taken from a test, from the
//! debugger, or from an `#[ignore]`d probe without disturbing the machine. There is a test
//! asserting exactly that, because a diagnostic that perturbs what it is diagnosing is worse than
//! none at all.

use crate::engine2d::{BackgroundKind, Engine, Engine2d};
use crate::vram::{BANK_NAMES, SPACES};
use crate::{Core, NdsSystem};
use std::fmt::Write as _;

impl NdsSystem {
    /// Everything about the machine's graphics configuration, in one page.
    ///
    /// Returned as a string rather than printed, so a test can assert on it and a caller can put
    /// it wherever it wants.
    pub fn graphics_dump(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(out, "== VRAM banks ==");
        for (index, name) in BANK_NAMES.iter().enumerate() {
            let control = self.bus().vram.control(index);
            if control & 0x80 == 0 {
                let _ = writeln!(out, "  {name}: disabled");
                continue;
            }
            let _ = writeln!(
                out,
                "  {name}: MST={} OFS={} ({control:#04X})",
                control & 7,
                (control >> 3) & 3
            );
        }

        let _ = writeln!(out, "\n== VRAM spaces with a bank in them ==");
        for space in SPACES {
            if !self.bus().vram.space_is_mapped(space) {
                continue;
            }
            let banks: Vec<&str> = (0..9)
                .filter(|bank| {
                    (0..space.size())
                        .step_by(0x2000)
                        .any(|offset| self.bus().vram.banks_at(space, offset).contains(bank))
                })
                .map(|bank| BANK_NAMES[bank])
                .collect();
            let _ = writeln!(out, "  {space:?}: {}", banks.join(", "));
        }

        let _ = writeln!(
            out,
            "\n== Shared WRAM ==\n  split: {:?}",
            self.bus().memory.split()
        );

        for (engine, name) in [(Engine::A, "A"), (Engine::B, "B")] {
            let e = match engine {
                Engine::A => &self.bus().engine_a,
                Engine::B => &self.bus().engine_b,
            };
            let _ = write!(out, "\n{}", engine_dump(e, name));
        }

        let _ = writeln!(out, "\n== 3D ==");
        let _ = writeln!(out, "  layer enabled: {}", self.bus().gpu3d.enabled());
        let _ = writeln!(
            out,
            "  polygons queued: {}  vertices queued: {}",
            self.bus().gpu3d.geometry.polygon_count(),
            self.bus().gpu3d.geometry.vertex_count()
        );
        let _ = writeln!(
            out,
            "  matrix stack level: {}  viewport: {:?}",
            self.bus().gpu3d.geometry.matrices.stack_pointer(),
            self.bus().gpu3d.geometry.viewport()
        );
        out
    }

    /// Where both cores are and what each is waiting for.
    ///
    /// The first thing to look at when a DS ROM appears to hang: nine times in ten one core is
    /// spinning on a flag the other was supposed to set, and this says which.
    pub fn cores_dump(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(
            out,
            "ARM9 pc={:08X} thumb={} halted={}",
            core_common::CpuIntrospect::program_counter(self.arm9()),
            self.arm9().is_thumb(),
            self.arm9().is_halted()
        );
        let _ = writeln!(
            out,
            "ARM7 pc={:08X} thumb={} halted={}",
            core_common::CpuIntrospect::program_counter(self.arm7()),
            self.arm7().is_thumb(),
            self.arm7().is_halted()
        );
        for core in [Core::Arm9, Core::Arm7] {
            let irq = &self.bus().irq[core as usize];
            let _ = writeln!(
                out,
                "{} IE={:08X} IF={:08X} active={:08X}",
                core.name(),
                irq.read32(crate::irq::reg::IE).unwrap_or(0),
                irq.flags(),
                irq.active()
            );
        }
        let _ = writeln!(
            out,
            "IPC: ARM9 recv {} words, ARM7 recv {} words",
            self.bus().ipc.receive_len(Core::Arm9),
            self.bus().ipc.receive_len(Core::Arm7)
        );
        let _ = writeln!(
            out,
            "video: line {} cycle {}",
            self.bus().video.line(),
            self.bus().video.cycle_in_line()
        );
        out
    }
}

/// One 2D engine's configuration, including what each background layer currently *is*.
///
/// That last part is the point of the whole function. Which renderer a layer uses depends on the
/// background mode *and* on two `BGxCNT` bits whose meaning changes with it, and getting it wrong
/// produces a layer drawn by the wrong renderer — which looks like corrupt tile data rather than
/// like a mode mix-up. Printing the decoded answer turns ten minutes of squinting into a glance.
fn engine_dump(engine: &Engine2d, name: &str) -> String {
    let mut out = String::new();
    let dispcnt = engine.dispcnt();
    let mode = dispcnt & 7;
    let _ = writeln!(out, "== Engine {name} ==");
    let _ = writeln!(
        out,
        "  DISPCNT={dispcnt:08X} display_mode={} bg_mode={mode}",
        (dispcnt >> 16) & 3
    );
    let _ = writeln!(
        out,
        "  BG enable={:04b} OBJ={} 1D_map={} win0={} win1={} objwin={}",
        (dispcnt >> 8) & 0xF,
        dispcnt & (1 << 12) != 0,
        dispcnt & (1 << 4) != 0,
        dispcnt & (1 << 13) != 0,
        dispcnt & (1 << 14) != 0,
        dispcnt & (1 << 15) != 0
    );
    for layer in 0..4usize {
        let base = engine.engine().base();
        let bgcnt = engine.read16(base + 0x008 + layer as u32 * 2).unwrap_or(0);
        let kind = BackgroundKind::of(engine.engine(), mode, layer, dispcnt, bgcnt);
        let _ = writeln!(
            out,
            "  BG{layer}: {kind:?} priority={} char={} screen={} size={} ({bgcnt:04X})",
            bgcnt & 3,
            (bgcnt >> 2) & 0xF,
            (bgcnt >> 8) & 0x1F,
            (bgcnt >> 14) & 3
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_common::System;

    fn system() -> NdsSystem {
        let mut rom = vec![0u8; 0x8000];
        rom[..12].copy_from_slice(b"DUMPTEST\0\0\0\0");
        let put = |rom: &mut Vec<u8>, at: usize, v: u32| {
            rom[at..at + 4].copy_from_slice(&v.to_le_bytes());
        };
        put(&mut rom, 0x20, 0x4000);
        put(&mut rom, 0x24, 0x0200_0000);
        put(&mut rom, 0x28, 0x0200_0000);
        put(&mut rom, 0x2C, 4);
        put(&mut rom, 0x30, 0x6000);
        put(&mut rom, 0x34, 0x0380_0000);
        put(&mut rom, 0x38, 0x0380_0000);
        put(&mut rom, 0x3C, 4);
        put(&mut rom, 0x4000, 0xEAFF_FFFE);
        put(&mut rom, 0x6000, 0xEAFF_FFFE);
        let mut nds = NdsSystem::default();
        nds.load_cartridge(&rom).expect("a hand-built cartridge");
        nds
    }

    #[test]
    fn the_graphics_dump_says_where_every_bank_went() {
        let mut nds = system();
        // Bank A into engine A's background space at OFS 1, bank H into engine B's.
        nds.bus_mut().vram.set_control(0, 0x80 | (1 << 3) | 1);
        nds.bus_mut().vram.set_control(7, 0x80 | 1);

        let dump = nds.graphics_dump();
        assert!(dump.contains("A: MST=1 OFS=1"), "{dump}");
        assert!(dump.contains("B: disabled"), "{dump}");
        assert!(dump.contains("BgA: A"), "{dump}");
        assert!(dump.contains("BgB: H"), "{dump}");
    }

    #[test]
    fn the_graphics_dump_names_what_each_background_layer_currently_is() {
        // The thing most easily got wrong: which renderer a layer uses depends on the mode *and*
        // on two BGxCNT bits whose meaning changes with it.
        let mut nds = system();
        let base = Engine::A.base();
        // Mode 5 makes BG2 and BG3 extended, and BGxCNT bits 7 and 2 pick the sub-type.
        nds.bus_mut().engine_a.write32(base, (1 << 16) | 5);
        nds.bus_mut().engine_a.write16(base + 0x00C, 0x0084);

        let dump = nds.graphics_dump();
        assert!(dump.contains("bg_mode=5"), "{dump}");
        assert!(dump.contains("BG0: Text"), "{dump}");
        assert!(dump.contains("BG2: ExtendedDirectBitmap"), "{dump}");
    }

    #[test]
    fn the_graphics_dump_covers_both_engines_and_the_3d_state() {
        let nds = system();
        let dump = nds.graphics_dump();
        assert!(dump.contains("== Engine A =="));
        assert!(dump.contains("== Engine B =="));
        assert!(dump.contains("layer enabled: false"));
        assert!(dump.contains("Shared WRAM"));
    }

    #[test]
    fn the_cores_dump_says_where_both_cores_are() {
        let mut nds = system();
        nds.step_frame(core_common::InputState::default());
        let dump = nds.cores_dump();
        assert!(dump.contains("ARM9 pc=0200"), "{dump}");
        assert!(dump.contains("ARM7 pc=0380"), "{dump}");
        assert!(dump.contains("IPC:"), "{dump}");
        assert!(dump.contains("video: line"), "{dump}");
    }

    #[test]
    fn the_cores_dump_shows_a_word_waiting_in_the_fifo() {
        // The first thing to look at when a DS ROM hangs: one core spinning on a flag the other
        // was supposed to set.
        let mut nds = system();
        nds.bus_mut().ipc.write_control(Core::Arm9, 1 << 15);
        nds.bus_mut().ipc.send(Core::Arm9, 0xDEAD_BEEF);
        assert!(nds.cores_dump().contains("ARM7 recv 1 words"));
    }

    #[test]
    fn a_dump_does_not_disturb_the_machine() {
        // A diagnostic that perturbs what it is diagnosing is worse than none at all.
        let mut nds = system();
        nds.step_frame(core_common::InputState::default());
        let before = nds.save_state();
        let _ = nds.graphics_dump();
        let _ = nds.cores_dump();
        assert_eq!(nds.save_state(), before);
    }
}
