//! [`DebugTarget`] for the Game Boy, and so also for the Game Boy Color.
//!
//! The impl lives here rather than in `debugger` because the answers are the *machine's*: which
//! disassembler applies, what `0x8000` is called, and which reads are safe to perform. `debugger`
//! is written against this trait and needs no branch per system, which is the whole arrangement.
//!
//! Every read goes through [`Bus::peek8`], which `system-gb`'s memory map already implements with
//! the right refusals — peeking is safe where it can be and answers `None` where it cannot. A
//! disassembly view scrolled past `0xFF00` therefore cannot latch the joypad.

use crate::GbSystem;
use core_common::{Bus, DebugRegion, DebugTarget, DisasmInstruction, Disassemble, RegisterValue};
use cpu_sm83::Sm83Disassembler;

/// The Game Boy's address space, as the memory viewer's jump list shows it.
///
/// Echo RAM is listed even though it is a mirror: a game really does read through it, and a
/// contributor looking at an address in that range needs to be told what they are looking at rather
/// than left to wonder why `0xE000` holds the same bytes as `0xC000`.
const REGIONS: &[DebugRegion] = &[
    DebugRegion::new("ROM bank 0", 0x0000, 0x3FFF),
    DebugRegion::new("ROM bank N", 0x4000, 0x7FFF),
    DebugRegion::new("VRAM", 0x8000, 0x9FFF),
    DebugRegion::new("Cartridge RAM", 0xA000, 0xBFFF),
    DebugRegion::new("WRAM bank 0", 0xC000, 0xCFFF),
    DebugRegion::new("WRAM bank N", 0xD000, 0xDFFF),
    DebugRegion::new("Echo RAM", 0xE000, 0xFDFF),
    DebugRegion::new("OAM", 0xFE00, 0xFE9F),
    // Named rather than skipped. A contributor who lands here needs to be told it is the hardware's
    // unusable area, not left to wonder why a gap in the jump list swallowed their address.
    DebugRegion::new("Unusable", 0xFEA0, 0xFEFF),
    DebugRegion::new("I/O registers", 0xFF00, 0xFF7F),
    DebugRegion::new("HRAM", 0xFF80, 0xFFFE),
    DebugRegion::new("IE", 0xFFFF, 0xFFFF),
];

/// The longest SM83 instruction is three bytes.
const MAX_INSTRUCTION_LEN: usize = 3;

impl DebugTarget for GbSystem {
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
        // Named explicitly through the trait. An inherent `is_halted` on the SM83 core shadowed
        // the trait method once already, with different semantics on each side and no warning.
        core_common::CpuIntrospect::is_halted(self.cpu())
    }

    fn peek8(&self, addr: u32) -> Option<u8> {
        // The Game Boy's address space is 16 bits. An address above that is a caller error, not a
        // wrap: answering `None` says so, where masking would quietly show `0x10000` as `0x0000`.
        if addr > 0xFFFF {
            return None;
        }
        self.bus().peek8(addr)
    }

    fn disassemble(&self, addr: u32) -> Option<DisasmInstruction> {
        // Gathered one byte at a time because a peek can refuse partway: an instruction whose
        // operand lies in unreadable space is disassembled from what is available, and the
        // disassembler returns `None` if that is not enough to decide.
        let mut bytes = [0u8; MAX_INSTRUCTION_LEN];
        let mut available = 0;
        for (offset, slot) in bytes.iter_mut().enumerate() {
            match self.peek8(addr.wrapping_add(offset as u32) & 0xFFFF) {
                Some(byte) => {
                    *slot = byte;
                    available += 1;
                }
                None => break,
            }
        }
        Sm83Disassembler.disassemble(&bytes[..available], addr)
    }

    fn regions(&self) -> &'static [DebugRegion] {
        REGIONS
    }

    fn address_digits(&self) -> u8 {
        4
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_common::System;

    /// A cartridge built here, with a recognisable instruction at the entry point. No commercial
    /// ROM is used anywhere in this workspace.
    fn rom_with(code: &[u8]) -> Vec<u8> {
        let mut rom = vec![0u8; 0x8000];
        rom[0x0100..0x0100 + code.len()].copy_from_slice(code);
        rom[0x0134..0x013D].copy_from_slice(b"DEBUGTEST");
        rom[0x0147] = 0x00;
        rom[0x0148] = 0x00;
        rom[0x014D] = cart_common::GbHeader::header_checksum(&rom);
        rom
    }

    fn system(code: &[u8]) -> GbSystem {
        GbSystem::new(rom_with(code), None).expect("a hand-built cartridge")
    }

    #[test]
    fn a_system_offers_a_debug_target() {
        let mut system = system(&[0x00]);
        assert!(
            System::debug(&mut system).is_some(),
            "the Game Boy has everything a debugger needs, so it must not report None"
        );
    }

    #[test]
    fn registers_and_the_program_counter_come_from_the_cpu() {
        let system = system(&[0x00]);
        let registers = DebugTarget::registers(&system);
        assert!(
            registers.iter().any(|r| r.name == "PC"),
            "no PC among {registers:?}"
        );
        assert!(registers.iter().any(|r| r.name == "A"));
        assert_eq!(DebugTarget::address_digits(&system), 4);
    }

    #[test]
    fn setting_the_program_counter_moves_execution() {
        let mut system = system(&[0x00]);
        DebugTarget::set_program_counter(&mut system, 0x0150);
        assert_eq!(DebugTarget::program_counter(&system), 0x0150);
    }

    #[test]
    fn disassembly_reads_through_peek_and_decodes_a_real_instruction() {
        // `LD A,$42` — two bytes, so this also checks the operand is fetched.
        let system = system(&[0x3E, 0x42]);
        let decoded = DebugTarget::disassemble(&system, 0x0100).expect("decodable");
        assert_eq!(decoded.length, 2);
        assert!(
            decoded.text.contains("42"),
            "the operand is missing: {}",
            decoded.text
        );
    }

    #[test]
    fn disassembly_matches_the_cpu_crates_own_disassembler() {
        // Prompt 15's acceptance criterion: what the debugger shows must not diverge from what the
        // CPU crate's own tests assert is correct. Asserting equality with that disassembler is
        // the only version of this check that cannot drift.
        let code = [0x3E, 0x42, 0x00, 0xC3, 0x50, 0x01, 0xCB, 0x11];
        let system = system(&code);
        let mut addr = 0x0100u32;
        for _ in 0..4 {
            let through_debugger = DebugTarget::disassemble(&system, addr).expect("decodable");
            let direct = Sm83Disassembler
                .disassemble(&code[(addr - 0x0100) as usize..], addr)
                .expect("decodable");
            assert_eq!(through_debugger, direct, "divergence at {addr:#06X}");
            addr += through_debugger.length as u32;
        }
    }

    #[test]
    fn peeking_outside_the_address_space_is_refused_rather_than_wrapped() {
        let system = system(&[0x00]);
        assert_eq!(
            DebugTarget::peek8(&system, 0x1_0000),
            None,
            "masking would show address 0x10000 as 0x0000, which is a lie"
        );
    }

    #[test]
    fn peeking_reads_rom_without_disturbing_the_machine() {
        let mut system = system(&[0x3E, 0x42]);
        let before = System::save_state(&system);
        assert_eq!(DebugTarget::peek8(&system, 0x0100), Some(0x3E));
        assert_eq!(DebugTarget::peek8(&system, 0x0101), Some(0x42));
        assert_eq!(
            System::save_state(&system),
            before,
            "a peek changed the machine, which is the one thing it must never do"
        );
        // And the machine still runs afterwards.
        System::step_frame(&mut system, Default::default());
    }

    #[test]
    fn every_region_is_ordered_and_covers_the_whole_address_space() {
        let mut expected_next = 0u32;
        for region in REGIONS {
            assert_eq!(
                region.start, expected_next,
                "{} leaves a gap or overlaps",
                region.name
            );
            assert!(region.end >= region.start);
            expected_next = region.end + 1;
        }
        assert_eq!(
            expected_next, 0x1_0000,
            "the regions must tile the Game Boy's 64 KiB exactly"
        );
    }

    #[test]
    fn every_address_resolves_to_exactly_one_named_region() {
        let system = system(&[0x00]);
        for addr in [0x0000u32, 0x4000, 0x8000, 0xC000, 0xFE00, 0xFF40, 0xFFFF] {
            let matches: Vec<_> = DebugTarget::regions(&system)
                .iter()
                .filter(|region| region.contains(addr))
                .collect();
            assert_eq!(matches.len(), 1, "{addr:#06X} matched {matches:?}");
        }
    }
}
