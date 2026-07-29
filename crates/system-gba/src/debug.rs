//! [`DebugTarget`] for the Game Boy Advance.
//!
//! # Which disassembler
//!
//! The ARM7TDMI has two instruction sets and the choice between them is *machine* state, not an
//! argument a caller could supply: the T bit in the current program status register. That is exactly
//! why [`DebugTarget::disassemble`] takes only an address — the system knows which decoder applies
//! and the debugger cannot. Getting this wrong does not produce an error, it produces plausible
//! nonsense, which is the worst possible failure mode for a disassembly view.
//!
//! Thumb instructions are also two-byte aligned and ARM four-byte, so an address is rounded down to
//! the alignment of whichever mode is current. A disassembly view scrolled to an odd address in
//! Thumb mode would otherwise decode two halves of adjacent instructions as one.

use crate::GbaSystem;
use core_common::{Bus, DebugRegion, DebugTarget, DisasmInstruction, Disassemble, RegisterValue};
use cpu_arm7tdmi::{ArmDisassembler, ThumbDisassembler};

/// The GBA's address space, as the memory viewer's jump list shows it.
///
/// Sizes are the *physical* ones, not the mirrored spans: the hardware mirrors 32 KiB of internal
/// WRAM throughout `0x0300_0000`–`0x03FF_FFFF`, and listing the mirror as a 16 MiB region would
/// suggest 16 MiB of distinct memory exists. Three ROM windows appear because the cartridge really
/// is visible at three addresses with different wait-state settings.
const REGIONS: &[DebugRegion] = &[
    DebugRegion::new("BIOS", 0x0000_0000, 0x0000_3FFF),
    DebugRegion::new("External WRAM", 0x0200_0000, 0x0203_FFFF),
    DebugRegion::new("Internal WRAM", 0x0300_0000, 0x0300_7FFF),
    DebugRegion::new("I/O registers", 0x0400_0000, 0x0400_03FE),
    DebugRegion::new("Palette RAM", 0x0500_0000, 0x0500_03FF),
    DebugRegion::new("VRAM", 0x0600_0000, 0x0601_7FFF),
    DebugRegion::new("OAM", 0x0700_0000, 0x0700_03FF),
    DebugRegion::new("ROM (wait state 0)", 0x0800_0000, 0x09FF_FFFF),
    DebugRegion::new("ROM (wait state 1)", 0x0A00_0000, 0x0BFF_FFFF),
    DebugRegion::new("ROM (wait state 2)", 0x0C00_0000, 0x0DFF_FFFF),
    DebugRegion::new("Cartridge save", 0x0E00_0000, 0x0E00_FFFF),
];

impl DebugTarget for GbaSystem {
    fn registers(&self) -> Vec<RegisterValue> {
        core_common::CpuIntrospect::registers(self.cpu())
    }

    fn program_counter(&self) -> u32 {
        core_common::CpuIntrospect::program_counter(self.cpu())
    }

    fn set_program_counter(&mut self, pc: u32) {
        core_common::CpuIntrospect::set_program_counter(self.cpu_mut(), pc);
    }

    fn flags_summary(&self) -> String {
        core_common::CpuIntrospect::flags_summary(self.cpu())
    }

    fn is_halted(&self) -> bool {
        core_common::CpuIntrospect::is_halted(self.cpu())
    }

    fn peek8(&self, addr: u32) -> Option<u8> {
        self.bus().peek8(addr)
    }

    fn disassemble(&self, addr: u32) -> Option<DisasmInstruction> {
        let thumb = self.cpu().is_thumb();
        let width = if thumb { 2 } else { 4 };
        let aligned = addr & !(width - 1);

        let mut bytes = [0u8; 4];
        for (offset, slot) in bytes.iter_mut().take(width as usize).enumerate() {
            // A refusal partway means the instruction straddles unreadable space — I/O or the
            // cartridge save window — and the disassembler is given the short slice so it can say
            // so rather than being handed zeroes that decode to something.
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
    use core_common::System;

    /// A ROM of `mov r0, #0` repeated, so every ARM word decodes to something recognisable. Built
    /// here; no commercial ROM is used anywhere in this workspace.
    fn arm_rom() -> Vec<u8> {
        // 0xE3A00000 = mov r0, #0, little-endian.
        let mut rom = Vec::with_capacity(0x8000);
        for _ in 0..0x2000 {
            rom.extend_from_slice(&0xE3A0_0000u32.to_le_bytes());
        }
        rom
    }

    fn system() -> GbaSystem {
        GbaSystem::new(arm_rom(), None).expect("a hand-built cartridge")
    }

    #[test]
    fn a_system_offers_a_debug_target() {
        let mut system = system();
        assert!(System::debug(&mut system).is_some());
    }

    #[test]
    fn registers_include_the_arm_register_file() {
        let system = system();
        let registers = DebugTarget::registers(&system);
        // The core names the last three `sp`, `lr`, `pc` rather than r13-r15, and lower-cases the
        // status register — which is what the disassembler prints, so they match.
        for name in ["r0", "r12", "sp", "lr", "pc", "cpsr"] {
            assert!(
                registers.iter().any(|r| r.name == name),
                "{name} missing from {registers:?}"
            );
        }
        assert_eq!(DebugTarget::address_digits(&system), 8);
    }

    #[test]
    fn rom_is_peekable_and_io_is_not() {
        let system = system();
        assert_eq!(
            DebugTarget::peek8(&system, 0x0800_0000),
            Some(0x00),
            "the first byte of `mov r0, #0` little-endian"
        );
        assert_eq!(
            DebugTarget::peek8(&system, 0x0800_0003),
            Some(0xE3),
            "and its last"
        );
        assert_eq!(
            DebugTarget::peek8(&system, 0x0400_0000),
            None,
            "I/O reads have side effects, so a debugger must not perform them"
        );
    }

    #[test]
    fn peeking_does_not_disturb_the_machine() {
        let system = system();
        let before = System::save_state(&system);
        for addr in 0x0800_0000..0x0800_0040u32 {
            let _ = DebugTarget::peek8(&system, addr);
        }
        assert_eq!(System::save_state(&system), before);
    }

    #[test]
    fn arm_disassembly_matches_the_cpu_crates_own_disassembler() {
        // Prompt 15's acceptance criterion: no divergence between what the debugger shows and what
        // the CPU crate's own disassembler tests assert is correct.
        let system = system();
        assert!(!system.cpu().is_thumb(), "the GBA boots into ARM state");

        let word = 0xE3A0_0000u32.to_le_bytes();
        for addr in [0x0800_0000u32, 0x0800_0004, 0x0800_0100] {
            let through_debugger = DebugTarget::disassemble(&system, addr).expect("decodable");
            let direct = ArmDisassembler.disassemble(&word, addr).expect("decodable");
            assert_eq!(through_debugger, direct, "divergence at {addr:#010X}");
        }
    }

    #[test]
    fn an_unaligned_address_is_rounded_down_to_the_instruction_it_is_inside() {
        // Decoding from a misaligned address would splice two adjacent instructions into one and
        // render confident nonsense.
        let system = system();
        let aligned = DebugTarget::disassemble(&system, 0x0800_0000).expect("decodable");
        for offset in 1..4u32 {
            assert_eq!(
                DebugTarget::disassemble(&system, 0x0800_0000 + offset),
                Some(aligned.clone()),
                "offset {offset} did not round down to the containing instruction"
            );
        }
    }

    #[test]
    fn disassembling_unreadable_space_refuses_rather_than_inventing() {
        let system = system();
        assert_eq!(
            DebugTarget::disassemble(&system, 0x0400_0000),
            None,
            "I/O cannot be peeked, so it cannot be disassembled either"
        );
    }

    #[test]
    fn regions_are_ordered_and_do_not_overlap() {
        // The GBA's map has real gaps between regions — unmapped space between `0x0000_4000` and
        // `0x0200_0000`, for instance — so this checks ordering rather than tiling.
        for pair in REGIONS.windows(2) {
            assert!(
                pair[0].end < pair[1].start,
                "{} and {} overlap or are out of order",
                pair[0].name,
                pair[1].name
            );
        }
    }

    #[test]
    fn every_named_region_resolves_uniquely() {
        let system = system();
        for addr in [
            0x0000_0000u32,
            0x0200_0000,
            0x0300_0000,
            0x0500_0000,
            0x0600_0000,
            0x0800_0000,
            0x0E00_0000,
        ] {
            let matches: Vec<_> = DebugTarget::regions(&system)
                .iter()
                .filter(|region| region.contains(addr))
                .collect();
            assert_eq!(matches.len(), 1, "{addr:#010X} matched {matches:?}");
        }
    }
}
