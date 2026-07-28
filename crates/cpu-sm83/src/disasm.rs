//! SM83 disassembler.
//!
//! A real disassembler, not a stub: prompt 15's debugger disassembly view, the trace log, and
//! snapshot tests all depend on it. It decodes from a byte slice rather than a live bus, so
//! scrolling a disassembly window can never trigger an MMIO read side effect, and so ROMs can
//! be disassembled without being loaded into a machine.

use core_common::{DisasmInstruction, Disassemble};

/// Encoded length in bytes of each unprefixed opcode.
///
/// Undefined opcodes get length 1 so that a disassembly walk steps over them and resynchronizes
/// instead of stopping — a disassembler that gives up at the first bad byte is useless for
/// exactly the case you need it for.
#[rustfmt::skip]
pub const LENGTHS: [u8; 256] = [
//  x0 x1 x2 x3 x4 x5 x6 x7 x8 x9 xA xB xC xD xE xF
    1, 3, 1, 1, 1, 1, 2, 1, 3, 1, 1, 1, 1, 1, 2, 1, // 0x
    2, 3, 1, 1, 1, 1, 2, 1, 2, 1, 1, 1, 1, 1, 2, 1, // 1x
    2, 3, 1, 1, 1, 1, 2, 1, 2, 1, 1, 1, 1, 1, 2, 1, // 2x
    2, 3, 1, 1, 1, 1, 2, 1, 2, 1, 1, 1, 1, 1, 2, 1, // 3x
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, // 4x
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, // 5x
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, // 6x
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, // 7x
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, // 8x
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, // 9x
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, // Ax
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, // Bx
    1, 1, 3, 3, 3, 1, 2, 1, 1, 1, 3, 2, 3, 3, 2, 1, // Cx
    1, 1, 3, 1, 3, 1, 2, 1, 1, 1, 3, 1, 3, 1, 2, 1, // Dx
    2, 1, 1, 1, 1, 1, 2, 1, 2, 1, 3, 1, 1, 1, 2, 1, // Ex
    2, 1, 1, 1, 1, 1, 2, 1, 2, 1, 3, 1, 1, 1, 2, 1, // Fx
];

/// Operand register names, indexed by the 3-bit operand field.
const R: [&str; 8] = ["B", "C", "D", "E", "H", "L", "(HL)", "A"];
/// ALU mnemonics including their destination operand, indexed by bits 3-5.
const ALU: [&str; 8] = [
    "ADD A,", "ADC A,", "SUB ", "SBC A,", "AND ", "XOR ", "OR ", "CP ",
];
/// Rotate/shift mnemonics on the `0xCB` page, indexed by bits 3-5.
const CB_SHIFT: [&str; 8] = ["RLC", "RRC", "RL", "RR", "SLA", "SRA", "SWAP", "SRL"];
/// Branch conditions, indexed by bits 3-4.
const COND: [&str; 4] = ["NZ", "Z", "NC", "C"];

/// Zero-sized disassembler for the SM83.
///
/// Stateless, so the debugger can hold one without borrowing the CPU.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Sm83Disassembler;

impl Disassemble for Sm83Disassembler {
    fn disassemble(&self, bytes: &[u8], addr: u32) -> Option<DisasmInstruction> {
        let op = *bytes.first()?;
        let length = LENGTHS[op as usize] as usize;
        if bytes.len() < length {
            return None;
        }

        let d8 = || bytes[1];
        let d16 = || u16::from_le_bytes([bytes[1], bytes[2]]);
        // JR targets are relative to the address *after* the instruction.
        let jr_target = || {
            (addr as u16)
                .wrapping_add(2)
                .wrapping_add(bytes[1] as i8 as i16 as u16)
        };

        let text = match op {
            0x00 => "NOP".to_string(),
            0x10 => "STOP".to_string(),
            0x76 => "HALT".to_string(),
            0xF3 => "DI".to_string(),
            0xFB => "EI".to_string(),

            0x01 => format!("LD BC,${:04X}", d16()),
            0x11 => format!("LD DE,${:04X}", d16()),
            0x21 => format!("LD HL,${:04X}", d16()),
            0x31 => format!("LD SP,${:04X}", d16()),
            0x08 => format!("LD (${:04X}),SP", d16()),

            0x02 => "LD (BC),A".to_string(),
            0x12 => "LD (DE),A".to_string(),
            0x22 => "LD (HL+),A".to_string(),
            0x32 => "LD (HL-),A".to_string(),
            0x0A => "LD A,(BC)".to_string(),
            0x1A => "LD A,(DE)".to_string(),
            0x2A => "LD A,(HL+)".to_string(),
            0x3A => "LD A,(HL-)".to_string(),

            0x03 => "INC BC".to_string(),
            0x13 => "INC DE".to_string(),
            0x23 => "INC HL".to_string(),
            0x33 => "INC SP".to_string(),
            0x0B => "DEC BC".to_string(),
            0x1B => "DEC DE".to_string(),
            0x2B => "DEC HL".to_string(),
            0x3B => "DEC SP".to_string(),

            0x09 => "ADD HL,BC".to_string(),
            0x19 => "ADD HL,DE".to_string(),
            0x29 => "ADD HL,HL".to_string(),
            0x39 => "ADD HL,SP".to_string(),

            0x07 => "RLCA".to_string(),
            0x0F => "RRCA".to_string(),
            0x17 => "RLA".to_string(),
            0x1F => "RRA".to_string(),
            0x27 => "DAA".to_string(),
            0x2F => "CPL".to_string(),
            0x37 => "SCF".to_string(),
            0x3F => "CCF".to_string(),

            0x18 => format!("JR ${:04X}", jr_target()),
            0x20 | 0x28 | 0x30 | 0x38 => {
                format!("JR {},${:04X}", COND[((op >> 3) & 3) as usize], jr_target())
            }

            // INC r / DEC r / LD r,d8 across all eight operand registers.
            0x04 | 0x0C | 0x14 | 0x1C | 0x24 | 0x2C | 0x34 | 0x3C => {
                format!("INC {}", R[((op >> 3) & 7) as usize])
            }
            0x05 | 0x0D | 0x15 | 0x1D | 0x25 | 0x2D | 0x35 | 0x3D => {
                format!("DEC {}", R[((op >> 3) & 7) as usize])
            }
            0x06 | 0x0E | 0x16 | 0x1E | 0x26 | 0x2E | 0x36 | 0x3E => {
                format!("LD {},${:02X}", R[((op >> 3) & 7) as usize], d8())
            }

            0x40..=0x7F => format!(
                "LD {},{}",
                R[((op >> 3) & 7) as usize],
                R[(op & 7) as usize]
            ),
            0x80..=0xBF => format!("{}{}", ALU[((op >> 3) & 7) as usize], R[(op & 7) as usize]),
            0xC6 | 0xCE | 0xD6 | 0xDE | 0xE6 | 0xEE | 0xF6 | 0xFE => {
                format!("{}${:02X}", ALU[((op >> 3) & 7) as usize], d8())
            }

            0xC1 => "POP BC".to_string(),
            0xD1 => "POP DE".to_string(),
            0xE1 => "POP HL".to_string(),
            0xF1 => "POP AF".to_string(),
            0xC5 => "PUSH BC".to_string(),
            0xD5 => "PUSH DE".to_string(),
            0xE5 => "PUSH HL".to_string(),
            0xF5 => "PUSH AF".to_string(),

            0xC3 => format!("JP ${:04X}", d16()),
            0xC2 | 0xCA | 0xD2 | 0xDA => {
                format!("JP {},${:04X}", COND[((op >> 3) & 3) as usize], d16())
            }
            0xE9 => "JP HL".to_string(),

            0xCD => format!("CALL ${:04X}", d16()),
            0xC4 | 0xCC | 0xD4 | 0xDC => {
                format!("CALL {},${:04X}", COND[((op >> 3) & 3) as usize], d16())
            }

            0xC9 => "RET".to_string(),
            0xD9 => "RETI".to_string(),
            0xC0 | 0xC8 | 0xD0 | 0xD8 => format!("RET {}", COND[((op >> 3) & 3) as usize]),

            0xC7 | 0xCF | 0xD7 | 0xDF | 0xE7 | 0xEF | 0xF7 | 0xFF => {
                format!("RST ${:02X}", op & 0x38)
            }

            0xE0 => format!("LDH (${:02X}),A", d8()),
            0xF0 => format!("LDH A,(${:02X})", d8()),
            0xE2 => "LD (C),A".to_string(),
            0xF2 => "LD A,(C)".to_string(),
            0xEA => format!("LD (${:04X}),A", d16()),
            0xFA => format!("LD A,(${:04X})", d16()),

            0xE8 => format!("ADD SP,{}", signed(d8())),
            0xF8 => format!("LD HL,SP{}", signed_with_sign(d8())),
            0xF9 => "LD SP,HL".to_string(),

            0xCB => {
                let cb = bytes[1];
                let reg = R[(cb & 7) as usize];
                let bit = (cb >> 3) & 7;
                match cb {
                    0x00..=0x3F => format!("{} {}", CB_SHIFT[(cb >> 3) as usize], reg),
                    0x40..=0x7F => format!("BIT {bit},{reg}"),
                    0x80..=0xBF => format!("RES {bit},{reg}"),
                    0xC0..=0xFF => format!("SET {bit},{reg}"),
                }
            }

            _ => format!("DB ${op:02X}  ; undefined"),
        };

        Some(DisasmInstruction {
            text,
            length: length as u8,
        })
    }
}

/// Render a signed 8-bit displacement as a decimal value, e.g. `-3`.
fn signed(v: u8) -> String {
    format!("{}", v as i8)
}

/// Same, but always carrying an explicit sign so it reads as an addend: `SP+4`, `SP-3`.
fn signed_with_sign(v: u8) -> String {
    format!("{:+}", v as i8)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dis(bytes: &[u8], addr: u32) -> (String, u8) {
        let d = Sm83Disassembler
            .disassemble(bytes, addr)
            .expect("should decode");
        (d.text, d.length)
    }

    #[test]
    fn decodes_simple_and_immediate_forms() {
        assert_eq!(dis(&[0x00], 0), ("NOP".into(), 1));
        assert_eq!(dis(&[0x3E, 0x42], 0), ("LD A,$42".into(), 2));
        assert_eq!(dis(&[0x21, 0x34, 0x12], 0), ("LD HL,$1234".into(), 3));
        assert_eq!(dis(&[0xC3, 0x00, 0x01], 0), ("JP $0100".into(), 3));
    }

    #[test]
    fn decodes_register_to_register_loads() {
        assert_eq!(dis(&[0x47], 0), ("LD B,A".into(), 1));
        assert_eq!(dis(&[0x7E], 0), ("LD A,(HL)".into(), 1));
        assert_eq!(dis(&[0x70], 0), ("LD (HL),B".into(), 1));
        // 0x76 sits inside the LD block but is HALT.
        assert_eq!(dis(&[0x76], 0), ("HALT".into(), 1));
    }

    #[test]
    fn decodes_the_alu_block() {
        assert_eq!(dis(&[0x80], 0), ("ADD A,B".into(), 1));
        assert_eq!(dis(&[0x8E], 0), ("ADC A,(HL)".into(), 1));
        assert_eq!(dis(&[0x90], 0), ("SUB B".into(), 1));
        assert_eq!(dis(&[0xA7], 0), ("AND A".into(), 1));
        assert_eq!(dis(&[0xBE], 0), ("CP (HL)".into(), 1));
        assert_eq!(dis(&[0xFE, 0x90], 0), ("CP $90".into(), 2));
    }

    #[test]
    fn renders_relative_jumps_as_absolute_targets() {
        // JR +2 at 0x0100: target is 0x0100 + 2 + 2.
        assert_eq!(dis(&[0x18, 0x02], 0x0100), ("JR $0104".into(), 2));
        // Backwards branch.
        assert_eq!(dis(&[0x20, 0xFC], 0x0100), ("JR NZ,$00FE".into(), 2));
    }

    #[test]
    fn decodes_the_cb_page() {
        assert_eq!(dis(&[0xCB, 0x00], 0), ("RLC B".into(), 2));
        assert_eq!(dis(&[0xCB, 0x36], 0), ("SWAP (HL)".into(), 2));
        assert_eq!(dis(&[0xCB, 0x7C], 0), ("BIT 7,H".into(), 2));
        assert_eq!(dis(&[0xCB, 0x86], 0), ("RES 0,(HL)".into(), 2));
        assert_eq!(dis(&[0xCB, 0xFF], 0), ("SET 7,A".into(), 2));
    }

    #[test]
    fn renders_signed_stack_displacements() {
        assert_eq!(dis(&[0xE8, 0xFD], 0), ("ADD SP,-3".into(), 2));
        assert_eq!(dis(&[0xF8, 0x04], 0), ("LD HL,SP+4".into(), 2));
        assert_eq!(dis(&[0xF8, 0xFE], 0), ("LD HL,SP-2".into(), 2));
    }

    #[test]
    fn undefined_opcodes_decode_as_data_and_advance_one_byte() {
        let (text, len) = dis(&[0xD3], 0);
        assert!(text.contains("undefined"), "{text}");
        assert_eq!(len, 1, "a walk must be able to step past and resynchronize");
    }

    #[test]
    fn truncated_input_yields_none_rather_than_a_wrong_decode() {
        assert!(Sm83Disassembler.disassemble(&[], 0).is_none());
        assert!(Sm83Disassembler.disassemble(&[0x21, 0x34], 0).is_none());
        assert!(Sm83Disassembler.disassemble(&[0xCB], 0).is_none());
    }

    #[test]
    fn every_opcode_decodes_to_something_with_a_sane_length() {
        // Guards against a gap in the match: a panic or a `None` for a defined opcode would
        // break the debugger's disassembly view at exactly the wrong moment.
        for op in 0u16..=0xFF {
            let op = op as u8;
            let bytes = [op, 0x00, 0x00];
            let d = Sm83Disassembler
                .disassemble(&bytes, 0x1000)
                .unwrap_or_else(|| panic!("opcode {op:#04X} failed to decode"));
            assert!(!d.text.is_empty());
            assert!((1..=3).contains(&d.length), "opcode {op:#04X}");
            assert_eq!(d.length, LENGTHS[op as usize]);
        }
    }

    #[test]
    fn lengths_agree_with_the_cycle_table_on_which_opcodes_exist() {
        // Every opcode the cycle table marks nonexistent must disassemble as undefined data,
        // and vice versa. Keeping the two tables consistent is what stops a typo in one from
        // silently disagreeing with the other.
        for op in 0u16..=0xFF {
            let op = op as u8;
            let undefined_by_cycles = crate::CYCLES[op as usize] == 0;
            let text = Sm83Disassembler.disassemble(&[op, 0, 0], 0).unwrap().text;
            assert_eq!(
                undefined_by_cycles,
                text.contains("undefined"),
                "opcode {op:#04X}: cycle table and disassembler disagree ({text})"
            );
        }
    }
}
