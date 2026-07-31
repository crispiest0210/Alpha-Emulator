//! [`DebugTarget`] for the Nintendo DS.
//!
//! # The debugger sees the ARM9
//!
//! A DS has two CPUs and the debugger interface has one of everything: one register list, one
//! program counter, one address space. Rather than invent a "which core" concept the frontend has
//! no notion of, this reports the **ARM9** — the core that runs game logic, and the one anybody
//! debugging a DS game means. The ARM7 is not reachable from the debugger at all, and that is a
//! stated limitation rather than an oversight.
//!
//! Making it selectable is a frontend change as much as a core one: `DebugTarget` would need a
//! core list and the panel a way to switch, and every breakpoint would need to say which core it
//! belongs to. That is worth doing and is not worth pretending is already done.
//!
//! # Peeks go through the TCMs
//!
//! The ARM9's tightly-coupled memories sit between the core and the bus, so an address inside one
//! answers from there and never reaches the bus at all. A memory view that read the bus directly
//! would show a game's stack — which lives in DTCM — as whatever main RAM happens to hold at that
//! address, which is plausible-looking rubbish rather than an obvious gap.
//!
//! # The disassembler is the ARMv4T one
//!
//! `cpu-arm7tdmi` supplies both decoders and `cpu-arm946e` adds no third. The ARMv5TE encodings
//! the ARM9 adds therefore render as undefined rather than as text. That is the honest failure:
//! the view says it does not know, instead of decoding `BLX` as something else.

use crate::NdsSystem;
use core_common::{DebugRegion, DebugTarget, DisasmInstruction, Disassemble, RegisterValue};
use cpu_arm7tdmi::{ArmDisassembler, ThumbDisassembler};

/// The ARM9's address space, as the memory viewer's jump list shows it.
///
/// Spans are the *physical* sizes, not the mirrored windows, for the same reason the GBA's are:
/// listing main RAM's 16 MiB window would suggest 16 MiB of distinct memory exists when there are
/// four megabytes mirrored four times.
///
/// The two TCM entries are where the firmware leaves them. Both are relocatable by CP15 and a game
/// may move either, in which case the jump list's label is where they *started* rather than where
/// they are — which is still more useful than not listing them.
const REGIONS: &[DebugRegion] = &[
    DebugRegion::new("ITCM", 0x0000_0000, 0x0000_7FFF),
    DebugRegion::new("Main RAM", 0x0200_0000, 0x023F_FFFF),
    DebugRegion::new("DTCM", 0x027C_0000, 0x027C_3FFF),
    DebugRegion::new("Shared WRAM", 0x0300_0000, 0x0300_7FFF),
    DebugRegion::new("I/O registers", 0x0400_0000, 0x0400_10FF),
    DebugRegion::new("Palette RAM", 0x0500_0000, 0x0500_07FF),
    DebugRegion::new("VRAM, engine A background", 0x0600_0000, 0x0607_FFFF),
    DebugRegion::new("VRAM, engine B background", 0x0620_0000, 0x0621_FFFF),
    DebugRegion::new("VRAM, engine A sprites", 0x0640_0000, 0x0643_FFFF),
    DebugRegion::new("VRAM, engine B sprites", 0x0660_0000, 0x0661_FFFF),
    DebugRegion::new("VRAM, direct window", 0x0680_0000, 0x068A_3FFF),
    DebugRegion::new("OAM", 0x0700_0000, 0x0700_07FF),
    DebugRegion::new("ARM9 BIOS", 0xFFFF_0000, 0xFFFF_7FFF),
];

impl DebugTarget for NdsSystem {
    fn registers(&self) -> Vec<RegisterValue> {
        core_common::CpuIntrospect::registers(self.arm9())
    }

    fn program_counter(&self) -> u32 {
        core_common::CpuIntrospect::program_counter(self.arm9())
    }

    fn set_program_counter(&mut self, pc: u32) {
        core_common::CpuIntrospect::set_program_counter(self.arm9_mut(), pc);
    }

    fn flags_summary(&self) -> String {
        core_common::CpuIntrospect::flags_summary(self.arm9())
    }

    fn is_halted(&self) -> bool {
        core_common::CpuIntrospect::is_halted(self.arm9())
    }

    fn peek8(&self, addr: u32) -> Option<u8> {
        self.peek_arm9(addr)
    }

    fn disassemble(&self, addr: u32) -> Option<DisasmInstruction> {
        let thumb = self.arm9().is_thumb();
        let width = if thumb { 2u32 } else { 4 };
        let aligned = addr & !(width - 1);

        let mut bytes = [0u8; 4];
        for (offset, slot) in bytes.iter_mut().take(width as usize).enumerate() {
            // A refusal partway means the instruction straddles unreadable space, and the
            // disassembler is handed the short slice so it can say so rather than being given
            // zeroes that decode to something.
            *slot = self.peek8(aligned.wrapping_add(offset as u32))?;
        }
        let bytes = &bytes[..width as usize];
        if thumb {
            ThumbDisassembler.disassemble(bytes, aligned)
        } else {
            ArmDisassembler.disassemble(bytes, aligned)
        }
    }

    fn regions(&self) -> &'static [DebugRegion] {
        REGIONS
    }

    fn address_digits(&self) -> u8 {
        8
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::system::{Arm7View, Arm9View};
    use core_common::{AccessKind, Bus, InputState, System};

    /// A ROM whose ARM9 half is `mov r0, #0` repeated, so every word decodes to something
    /// recognisable. Hand-built; no commercial ROM is used anywhere in this workspace.
    fn system() -> NdsSystem {
        let mut rom = vec![0u8; 0x8000];
        rom[..12].copy_from_slice(b"DEBUGTEST\0\0\0");
        let put = |rom: &mut Vec<u8>, at: usize, v: u32| {
            rom[at..at + 4].copy_from_slice(&v.to_le_bytes());
        };
        put(&mut rom, 0x20, 0x4000);
        put(&mut rom, 0x24, 0x0200_0000);
        put(&mut rom, 0x28, 0x0200_0000);
        put(&mut rom, 0x2C, 0x100);
        put(&mut rom, 0x30, 0x6000);
        put(&mut rom, 0x34, 0x0380_0000);
        put(&mut rom, 0x38, 0x0380_0000);
        put(&mut rom, 0x3C, 4);
        for i in 0..0x40 {
            put(&mut rom, 0x4000 + i * 4, 0xE3A0_0000);
        }
        put(&mut rom, 0x6000, 0xEAFF_FFFE);

        let mut nds = NdsSystem::default();
        nds.load_cartridge(&rom).expect("a hand-built cartridge");
        nds
    }

    #[test]
    fn the_system_offers_a_debug_target_now_rather_than_reporting_none() {
        let mut nds = system();
        assert!(nds.debug().is_some());
        assert!(nds.access_log().is_some());
    }

    #[test]
    fn the_registers_are_the_arm9s() {
        let nds = system();
        let regs = DebugTarget::registers(&nds);
        assert!(regs.iter().any(|r| r.name == "r0"));
        assert!(regs.iter().any(|r| r.name == "pc"));
        // The ARM9's CP15 registers are in the list, which is how a reader tells the two cores
        // apart at a glance.
        assert!(regs.iter().any(|r| r.name == "cp15_ctl"));
    }

    #[test]
    fn the_program_counter_is_the_architectural_one_and_can_be_moved() {
        let mut nds = system();
        assert_eq!(DebugTarget::program_counter(&nds), 0x0200_0000);
        DebugTarget::set_program_counter(&mut nds, 0x0200_0010);
        assert_eq!(DebugTarget::program_counter(&nds), 0x0200_0010);
        // And execution continues from there.
        nds.step_instruction();
        assert_eq!(DebugTarget::program_counter(&nds), 0x0200_0014);
    }

    #[test]
    fn disassembly_reads_the_code_direct_boot_loaded() {
        let nds = system();
        let decoded = DebugTarget::disassemble(&nds, 0x0200_0000).expect("an instruction");
        assert_eq!(decoded.length, 4);
        assert!(
            decoded.text.to_lowercase().starts_with("mov"),
            "got {}",
            decoded.text
        );
    }

    #[test]
    fn disassembly_rounds_to_the_current_modes_alignment() {
        let nds = system();
        // In ARM mode an address inside a word decodes the word containing it, rather than four
        // bytes straddling two instructions.
        let at_word = DebugTarget::disassemble(&nds, 0x0200_0000).unwrap();
        let inside = DebugTarget::disassemble(&nds, 0x0200_0002).unwrap();
        assert_eq!(at_word, inside);
    }

    #[test]
    fn a_peek_sees_tcm_before_the_bus() {
        // A game's stack lives in DTCM. A memory view that read the bus instead would show
        // whatever main RAM holds at that address — plausible rubbish rather than an obvious gap.
        let mut nds = system();
        let base = nds.arm9().dtcm.base();
        nds.arm9_mut().dtcm.write8(base + 0x10, 0x5A);
        assert_eq!(DebugTarget::peek8(&nds, base + 0x10), Some(0x5A));
    }

    #[test]
    fn a_peek_reads_ordinary_memory_and_refuses_io() {
        let mut nds = system();
        Arm9View(nds.bus_mut()).write8(0x0201_0000, 0x42);
        assert_eq!(DebugTarget::peek8(&nds, 0x0201_0000), Some(0x42));
        // I/O has read side effects, so a memory view must show `??` rather than advance a FIFO
        // by being scrolled past.
        assert_eq!(DebugTarget::peek8(&nds, 0x0400_0004), None);
    }

    #[test]
    fn the_region_list_covers_the_memory_a_reader_would_look_for() {
        let nds = system();
        let names: Vec<&str> = DebugTarget::regions(&nds).iter().map(|r| r.name).collect();
        for expected in ["Main RAM", "DTCM", "ITCM", "OAM", "Palette RAM"] {
            assert!(names.contains(&expected), "missing {expected}");
        }
        assert_eq!(DebugTarget::address_digits(&nds), 8);
    }

    #[test]
    fn the_access_log_records_only_when_armed() {
        let mut nds = system();
        nds.access_log().unwrap().set_armed(false);
        nds.step_instruction();
        assert_eq!(nds.access_log().unwrap().drain().count(), 0);

        nds.access_log().unwrap().set_armed(true);
        nds.step_instruction();
        let entries: Vec<_> = nds.access_log().unwrap().drain().collect();
        assert!(!entries.is_empty(), "an instruction fetch is an access");
        assert!(entries.iter().all(|e| e.kind == AccessKind::Read));
    }

    #[test]
    fn a_write_is_recorded_with_the_byte_that_was_written() {
        // Through the *view*, not the bus: the recorder sits between the CPU and the bus, which
        // is what makes it free for a core nobody is watching.
        let mut nds = system();
        nds.access_log().unwrap().set_armed(true);
        Arm9View(nds.bus_mut()).write8(0x0201_0000, 0x99);
        let entries: Vec<_> = nds.access_log().unwrap().drain().collect();
        let write = entries
            .iter()
            .find(|e| e.kind == AccessKind::Write)
            .expect("the write was recorded");
        assert_eq!(write.addr, 0x0201_0000);
        assert_eq!(write.value, 0x99);
    }

    #[test]
    fn the_arm7s_accesses_are_not_recorded() {
        // The debugger shows the ARM9, so an ARM7 access appearing in the log would fire a
        // watchpoint the user cannot see the cause of.
        let mut nds = system();
        nds.access_log().unwrap().set_armed(true);
        Arm7View(nds.bus_mut()).write8(0x0380_1000, 0x77);
        assert_eq!(nds.access_log().unwrap().drain().count(), 0);

        // The same operation through the ARM9's view does record, so the difference is the view
        // rather than the address or the arming.
        Arm9View(nds.bus_mut()).write8(0x0201_0000, 0x11);
        assert_eq!(nds.access_log().unwrap().drain().count(), 1);
    }

    #[test]
    fn stepping_an_instruction_makes_progress_the_debugger_can_see() {
        let mut nds = system();
        let before = DebugTarget::program_counter(&nds);
        let cycles = nds.step_instruction();
        assert!(cycles.0 > 0);
        assert_ne!(DebugTarget::program_counter(&nds), before);
        assert!(!DebugTarget::is_halted(&nds));
        assert!(!DebugTarget::flags_summary(&nds).is_empty());
    }

    #[test]
    fn a_full_frame_still_runs_with_the_log_armed() {
        // The recorder costs a branch per bus access whether or not anything is watching; the
        // point of this test is that arming it does not change what the machine does.
        let mut nds = system();
        let quiet = {
            nds.step_frame(InputState::default());
            nds.save_state()
        };
        let mut armed = system();
        armed.access_log().unwrap().set_armed(true);
        armed.step_frame(InputState::default());
        armed.access_log().unwrap().set_armed(false);
        assert_eq!(armed.save_state(), quiet);
    }
}
