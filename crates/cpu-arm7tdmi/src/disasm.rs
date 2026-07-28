//! ARM and THUMB disassemblers.
//!
//! Two separate [`Disassemble`] implementations rather than one auto-detecting decoder,
//! because nothing in an instruction's encoding says which state it belongs to — only `CPSR.T`
//! does. The debugger picks based on the CPU state it is inspecting, which is the only place
//! that information exists.
//!
//! Both decode from a byte slice rather than a live bus, so a disassembly view can never
//! perturb MMIO by scrolling.

use core_common::{DisasmInstruction, Disassemble};

/// Condition-code suffixes. `AL` renders as nothing, since that is how assembly is written.
const COND: [&str; 16] = [
    "eq", "ne", "cs", "cc", "mi", "pl", "vs", "vc", "hi", "ls", "ge", "lt", "gt", "le", "", "nv",
];

const SHIFT: [&str; 4] = ["lsl", "lsr", "asr", "ror"];

const DATA_OP: [&str; 16] = [
    "and", "eor", "sub", "rsb", "add", "adc", "sbc", "rsc", "tst", "teq", "cmp", "cmn", "orr",
    "mov", "bic", "mvn",
];

fn reg_name(index: u32) -> &'static str {
    const NAMES: [&str; 16] = [
        "r0", "r1", "r2", "r3", "r4", "r5", "r6", "r7", "r8", "r9", "r10", "r11", "r12", "sp",
        "lr", "pc",
    ];
    NAMES[(index & 0xF) as usize]
}

/// Render a register list like `{r0-r3, lr}`, collapsing runs.
fn register_list(list: u32) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut reg = 0u32;
    while reg < 16 {
        if list & (1 << reg) == 0 {
            reg += 1;
            continue;
        }
        let start = reg;
        while reg < 16 && list & (1 << reg) != 0 {
            reg += 1;
        }
        let end = reg - 1;
        parts.push(match end - start {
            0 => reg_name(start).to_string(),
            1 => format!("{}, {}", reg_name(start), reg_name(end)),
            _ => format!("{}-{}", reg_name(start), reg_name(end)),
        });
    }
    format!("{{{}}}", parts.join(", "))
}

/// Render an immediate the way ARM assembly does, with a `#`.
fn imm(value: u32) -> String {
    if value < 10 {
        format!("#{value}")
    } else {
        format!("#{value:#X}")
    }
}

// ---------------------------------------------------------------------------
// ARM
// ---------------------------------------------------------------------------

/// Disassembles ARM-state (32-bit) instructions.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ArmDisassembler;

impl Disassemble for ArmDisassembler {
    fn disassemble(&self, bytes: &[u8], addr: u32) -> Option<DisasmInstruction> {
        if bytes.len() < 4 {
            return None;
        }
        let instr = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        Some(DisasmInstruction {
            text: decode_arm(instr, addr),
            length: 4,
        })
    }
}

fn decode_arm(instr: u32, addr: u32) -> String {
    let cond = COND[(instr >> 28) as usize];

    if instr & 0x0FFF_FFF0 == 0x012F_FF10 {
        return format!("bx{cond} {}", reg_name(instr & 0xF));
    }
    if instr & 0x0F00_0000 == 0x0F00_0000 {
        return format!("swi{cond} {:#08X}", instr & 0x00FF_FFFF);
    }
    if instr & 0x0E00_0000 == 0x0A00_0000 {
        let offset = ((instr & 0x00FF_FFFF) << 8) as i32 >> 6;
        let target = addr.wrapping_add(8).wrapping_add(offset as u32);
        let link = if instr & 0x0100_0000 != 0 { "l" } else { "" };
        return format!("b{link}{cond} {target:#010X}");
    }
    if instr & 0x0C00_0000 == 0x0C00_0000 {
        return format!("cp{cond} ; undefined (no coprocessor)");
    }
    if instr & 0x0E00_0000 == 0x0800_0000 {
        return decode_arm_block(instr, cond);
    }
    if instr & 0x0E00_0010 == 0x0600_0010 {
        return format!("undefined{cond}");
    }
    if instr & 0x0C00_0000 == 0x0400_0000 {
        return decode_arm_single(instr, cond);
    }
    if instr & 0x0FC0_00F0 == 0x0000_0090 {
        let s = if instr & 0x0010_0000 != 0 { "s" } else { "" };
        let rd = reg_name(instr >> 16);
        let rn = reg_name(instr >> 12);
        let rs = reg_name(instr >> 8);
        let rm = reg_name(instr);
        return if instr & 0x0020_0000 != 0 {
            format!("mla{cond}{s} {rd}, {rm}, {rs}, {rn}")
        } else {
            format!("mul{cond}{s} {rd}, {rm}, {rs}")
        };
    }
    if instr & 0x0F80_00F0 == 0x0080_0090 {
        let s = if instr & 0x0010_0000 != 0 { "s" } else { "" };
        let signed = if instr & 0x0040_0000 != 0 { "s" } else { "u" };
        let op = if instr & 0x0020_0000 != 0 {
            "mlal"
        } else {
            "mull"
        };
        return format!(
            "{signed}{op}{cond}{s} {}, {}, {}, {}",
            reg_name(instr >> 12),
            reg_name(instr >> 16),
            reg_name(instr),
            reg_name(instr >> 8)
        );
    }
    if instr & 0x0FB0_0FF0 == 0x0100_0090 {
        let b = if instr & 0x0040_0000 != 0 { "b" } else { "" };
        return format!(
            "swp{cond}{b} {}, {}, [{}]",
            reg_name(instr >> 12),
            reg_name(instr),
            reg_name(instr >> 16)
        );
    }
    if instr & 0x0E00_0090 == 0x0000_0090 && instr & 0x60 != 0 {
        return decode_arm_halfword(instr, cond);
    }
    decode_arm_data_processing(instr, cond)
}

/// The shifter operand, as written in assembly.
fn arm_shifter_operand(instr: u32) -> String {
    if instr & 0x0200_0000 != 0 {
        let rotate = ((instr >> 8) & 0xF) * 2;
        return imm((instr & 0xFF).rotate_right(rotate));
    }

    let rm = reg_name(instr);
    let shift_type = (instr >> 5) & 3;
    if instr & 0x10 != 0 {
        return format!(
            "{rm}, {} {}",
            SHIFT[shift_type as usize],
            reg_name(instr >> 8)
        );
    }
    let amount = (instr >> 7) & 0x1F;
    if amount == 0 {
        return match shift_type {
            0 => rm.to_string(),
            // A zero immediate encodes #32 for LSR and ASR, and RRX for ROR.
            1 => format!("{rm}, lsr #32"),
            2 => format!("{rm}, asr #32"),
            _ => format!("{rm}, rrx"),
        };
    }
    format!("{rm}, {} #{amount}", SHIFT[shift_type as usize])
}

fn decode_arm_data_processing(instr: u32, cond: &str) -> String {
    let opcode = (instr >> 21) & 0xF;
    let set_flags = instr & 0x0010_0000 != 0;

    // TST/TEQ/CMP/CMN without S are PSR transfers.
    if !set_flags && (0b1000..=0b1011).contains(&opcode) {
        let psr = if instr & 0x0040_0000 != 0 {
            "spsr"
        } else {
            "cpsr"
        };
        if instr & 0x0020_0000 == 0 {
            return format!("mrs{cond} {}, {psr}", reg_name(instr >> 12));
        }
        let mut fields = String::new();
        if instr & (1 << 19) != 0 {
            fields.push('f');
        }
        if instr & (1 << 18) != 0 {
            fields.push('s');
        }
        if instr & (1 << 17) != 0 {
            fields.push('x');
        }
        if instr & (1 << 16) != 0 {
            fields.push('c');
        }
        let operand = if instr & 0x0200_0000 != 0 {
            imm((instr & 0xFF).rotate_right(((instr >> 8) & 0xF) * 2))
        } else {
            reg_name(instr).to_string()
        };
        return format!("msr{cond} {psr}_{fields}, {operand}");
    }

    let op = DATA_OP[opcode as usize];
    let s = if set_flags { "s" } else { "" };
    let rn = reg_name(instr >> 16);
    let rd = reg_name(instr >> 12);
    let operand = arm_shifter_operand(instr);

    match opcode {
        // Comparisons have no destination.
        0b1000..=0b1011 => format!("{op}{cond} {rn}, {operand}"),
        // MOV and MVN have no first operand.
        0b1101 | 0b1111 => format!("{op}{cond}{s} {rd}, {operand}"),
        _ => format!("{op}{cond}{s} {rd}, {rn}, {operand}"),
    }
}

/// Render the `[base, offset]` addressing syntax shared by the transfer instructions.
fn arm_address(instr: u32, offset: String) -> String {
    let rn = reg_name(instr >> 16);
    let sign = if instr & 0x0080_0000 != 0 { "" } else { "-" };
    let pre_index = instr & 0x0100_0000 != 0;
    let writeback = instr & 0x0020_0000 != 0;

    if offset == "#0" {
        return format!("[{rn}]");
    }
    if pre_index {
        format!("[{rn}, {sign}{offset}]{}", if writeback { "!" } else { "" })
    } else {
        format!("[{rn}], {sign}{offset}")
    }
}

fn decode_arm_single(instr: u32, cond: &str) -> String {
    let load = instr & 0x0010_0000 != 0;
    let byte = if instr & 0x0040_0000 != 0 { "b" } else { "" };
    let op = if load { "ldr" } else { "str" };
    let offset = if instr & 0x0200_0000 != 0 {
        arm_shifter_operand(instr & !0x0200_0000)
    } else {
        imm(instr & 0xFFF)
    };
    format!(
        "{op}{cond}{byte} {}, {}",
        reg_name(instr >> 12),
        arm_address(instr, offset)
    )
}

fn decode_arm_halfword(instr: u32, cond: &str) -> String {
    let load = instr & 0x0010_0000 != 0;
    let suffix = match (instr >> 5) & 3 {
        1 => "h",
        2 => "sb",
        _ => "sh",
    };
    let op = if load { "ldr" } else { "str" };
    let offset = if instr & 0x0040_0000 != 0 {
        imm(((instr >> 4) & 0xF0) | (instr & 0xF))
    } else {
        reg_name(instr).to_string()
    };
    format!(
        "{op}{cond}{suffix} {}, {}",
        reg_name(instr >> 12),
        arm_address(instr, offset)
    )
}

fn decode_arm_block(instr: u32, cond: &str) -> String {
    let load = instr & 0x0010_0000 != 0;
    let op = if load { "ldm" } else { "stm" };
    // The addressing mode is the pre/post bit paired with the up/down bit.
    let mode = match (instr & 0x0100_0000 != 0, instr & 0x0080_0000 != 0) {
        (false, true) => "ia",
        (true, true) => "ib",
        (false, false) => "da",
        (true, false) => "db",
    };
    format!(
        "{op}{cond}{mode} {}{}, {}{}",
        reg_name(instr >> 16),
        if instr & 0x0020_0000 != 0 { "!" } else { "" },
        register_list(instr & 0xFFFF),
        if instr & 0x0040_0000 != 0 { "^" } else { "" }
    )
}

// ---------------------------------------------------------------------------
// THUMB
// ---------------------------------------------------------------------------

/// Disassembles THUMB-state (16-bit) instructions.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ThumbDisassembler;

impl Disassemble for ThumbDisassembler {
    fn disassemble(&self, bytes: &[u8], addr: u32) -> Option<DisasmInstruction> {
        if bytes.len() < 2 {
            return None;
        }
        let instr = u16::from_le_bytes([bytes[0], bytes[1]]);
        Some(DisasmInstruction {
            text: decode_thumb(instr, addr),
            length: 2,
        })
    }
}

fn decode_thumb(instr: u16, addr: u32) -> String {
    let lo = |shift: u32| reg_name(((instr >> shift) & 7) as u32);
    let pc_aligned = addr.wrapping_add(4) & !3;

    match instr >> 12 {
        0b0000 | 0b0001 => {
            if instr & 0x1800 == 0x1800 {
                // Format 2: add/subtract
                let op = if instr & 0x0200 != 0 { "sub" } else { "add" };
                let operand = if instr & 0x0400 != 0 {
                    imm(((instr >> 6) & 7) as u32)
                } else {
                    reg_name(((instr >> 6) & 7) as u32).to_string()
                };
                format!("{op}s {}, {}, {operand}", lo(0), lo(3))
            } else {
                // Format 1: move shifted register
                let kind = SHIFT[((instr >> 11) & 3) as usize];
                format!("{kind}s {}, {}, #{}", lo(0), lo(3), (instr >> 6) & 0x1F)
            }
        }
        0b0010 | 0b0011 => {
            // Format 3
            let op = ["mov", "cmp", "add", "sub"][((instr >> 11) & 3) as usize];
            format!("{op}s {}, {}", lo(8), imm((instr & 0xFF) as u32))
        }
        0b0100 => match (instr >> 10) & 3 {
            0b00 => {
                // Format 4
                const OPS: [&str; 16] = [
                    "and", "eor", "lsl", "lsr", "asr", "adc", "sbc", "ror", "tst", "neg", "cmp",
                    "cmn", "orr", "mul", "bic", "mvn",
                ];
                format!(
                    "{}s {}, {}",
                    OPS[((instr >> 6) & 0xF) as usize],
                    lo(0),
                    lo(3)
                )
            }
            0b01 => {
                // Format 5: high registers
                let rd = ((instr & 7) | ((instr >> 4) & 8)) as u32;
                let rs = (((instr >> 3) & 7) | ((instr >> 3) & 8)) as u32;
                match (instr >> 8) & 3 {
                    0b00 => format!("add {}, {}", reg_name(rd), reg_name(rs)),
                    0b01 => format!("cmp {}, {}", reg_name(rd), reg_name(rs)),
                    0b10 => format!("mov {}, {}", reg_name(rd), reg_name(rs)),
                    _ => format!("bx {}", reg_name(rs)),
                }
            }
            _ => {
                // Format 6: PC-relative load
                let target = pc_aligned.wrapping_add((instr & 0xFF) as u32 * 4);
                format!(
                    "ldr {}, [pc, #{}]  ; {target:#010X}",
                    lo(8),
                    (instr & 0xFF) * 4
                )
            }
        },
        0b0101 => {
            if instr & 0x0200 != 0 {
                // Format 8
                let op = ["strh", "ldrsb", "ldrh", "ldrsh"][((instr >> 10) & 3) as usize];
                format!("{op} {}, [{}, {}]", lo(0), lo(3), lo(6))
            } else {
                // Format 7
                let op = match (instr & 0x0800 != 0, instr & 0x0400 != 0) {
                    (true, true) => "ldrb",
                    (true, false) => "ldr",
                    (false, true) => "strb",
                    (false, false) => "str",
                };
                format!("{op} {}, [{}, {}]", lo(0), lo(3), lo(6))
            }
        }
        0b0110 | 0b0111 => {
            // Format 9
            let byte = instr & 0x1000 != 0;
            let load = instr & 0x0800 != 0;
            let op = match (load, byte) {
                (true, true) => "ldrb",
                (true, false) => "ldr",
                (false, true) => "strb",
                (false, false) => "str",
            };
            let offset = ((instr >> 6) & 0x1F) as u32 * if byte { 1 } else { 4 };
            format!("{op} {}, [{}, {}]", lo(0), lo(3), imm(offset))
        }
        0b1000 => {
            // Format 10
            let op = if instr & 0x0800 != 0 { "ldrh" } else { "strh" };
            let offset = ((instr >> 6) & 0x1F) as u32 * 2;
            format!("{op} {}, [{}, {}]", lo(0), lo(3), imm(offset))
        }
        0b1001 => {
            // Format 11
            let op = if instr & 0x0800 != 0 { "ldr" } else { "str" };
            format!("{op} {}, [sp, {}]", lo(8), imm((instr & 0xFF) as u32 * 4))
        }
        0b1010 => {
            // Format 12
            let offset = (instr & 0xFF) as u32 * 4;
            if instr & 0x0800 != 0 {
                format!("add {}, sp, {}", lo(8), imm(offset))
            } else {
                format!(
                    "add {}, pc, {}  ; {:#010X}",
                    lo(8),
                    imm(offset),
                    pc_aligned.wrapping_add(offset)
                )
            }
        }
        0b1011 => {
            if instr & 0x0F00 == 0 {
                // Format 13
                let offset = (instr & 0x7F) as u32 * 4;
                let op = if instr & 0x0080 != 0 { "sub" } else { "add" };
                format!("{op} sp, {}", imm(offset))
            } else if instr & 0x0600 == 0x0400 {
                // Format 14
                let mut list = (instr & 0xFF) as u32;
                if instr & 0x0100 != 0 {
                    list |= if instr & 0x0800 != 0 {
                        1 << 15
                    } else {
                        1 << 14
                    };
                }
                let op = if instr & 0x0800 != 0 { "pop" } else { "push" };
                format!("{op} {}", register_list(list))
            } else {
                "undefined".to_string()
            }
        }
        0b1100 => {
            // Format 15
            let op = if instr & 0x0800 != 0 {
                "ldmia"
            } else {
                "stmia"
            };
            format!("{op} {}!, {}", lo(8), register_list((instr & 0xFF) as u32))
        }
        0b1101 => {
            let cond = (instr >> 8) & 0xF;
            if cond == 0xF {
                format!("swi {:#04X}", instr & 0xFF)
            } else if cond == 0xE {
                "undefined".to_string()
            } else {
                // Format 16
                let offset = ((instr & 0xFF) as u8 as i8 as i32) * 2;
                let target = addr.wrapping_add(4).wrapping_add(offset as u32);
                format!("b{} {target:#010X}", COND[cond as usize])
            }
        }
        0b1110 => {
            if instr & 0x0800 != 0 {
                "undefined  ; blx is ARMv5".to_string()
            } else {
                // Format 18
                let offset = (((instr & 0x07FF) as i32) << 21 >> 20) as u32;
                let target = addr.wrapping_add(4).wrapping_add(offset);
                format!("b {target:#010X}")
            }
        }
        _ => {
            // Format 19: the two halves of a long BL, which really are separate instructions.
            let offset = (instr & 0x07FF) as u32;
            if instr & 0x0800 == 0 {
                let high = ((offset << 21) as i32 >> 9) as u32;
                format!(
                    "bl  ; hi half, lr = {:#010X}",
                    addr.wrapping_add(4).wrapping_add(high)
                )
            } else {
                format!("bl  ; lo half, +{:#X}", offset << 1)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arm(instr: u32, addr: u32) -> String {
        ArmDisassembler
            .disassemble(&instr.to_le_bytes(), addr)
            .unwrap()
            .text
    }

    fn thumb(instr: u16, addr: u32) -> String {
        ThumbDisassembler
            .disassemble(&instr.to_le_bytes(), addr)
            .unwrap()
            .text
    }

    #[test]
    fn arm_data_processing_forms() {
        assert_eq!(arm(0xE3A0_0001, 0), "mov r0, #1");
        assert_eq!(arm(0xE281_2002, 0), "add r2, r1, #2");
        assert_eq!(arm(0xE041_2003, 0), "sub r2, r1, r3");
        assert_eq!(arm(0xE151_0002, 0), "cmp r1, r2");
        assert_eq!(arm(0xE1A0_1102, 0), "mov r1, r2, lsl #2");
        assert_eq!(arm(0xE1A0_1312, 0), "mov r1, r2, lsl r3");
        // A zero immediate shift means #32 for LSR/ASR and RRX for ROR.
        assert_eq!(arm(0xE1A0_1022, 0), "mov r1, r2, lsr #32");
        assert_eq!(arm(0xE1A0_1062, 0), "mov r1, r2, rrx");
    }

    #[test]
    fn arm_condition_codes_render_as_suffixes() {
        assert_eq!(arm(0x03A0_0001, 0), "moveq r0, #1");
        assert_eq!(arm(0x13A0_0001, 0), "movne r0, #1");
        // AL renders as nothing at all, the way assembly is written.
        assert_eq!(arm(0xE3A0_0001, 0), "mov r0, #1");
    }

    #[test]
    fn arm_branches_render_absolute_targets() {
        // b +8 from 0x1000: 0x1000 + 8 + 0 = 0x1008
        assert_eq!(arm(0xEA00_0000, 0x1000), "b 0x00001008");
        assert_eq!(arm(0xEB00_0000, 0x1000), "bl 0x00001008");
        // Backwards branch.
        assert_eq!(arm(0xEAFF_FFFE, 0x1000), "b 0x00001000");
        assert_eq!(arm(0xE12F_FF11, 0), "bx r1");
    }

    #[test]
    fn arm_transfers_render_addressing_modes() {
        assert_eq!(arm(0xE590_1000, 0), "ldr r1, [r0]");
        assert_eq!(arm(0xE590_1004, 0), "ldr r1, [r0, #4]");
        assert_eq!(arm(0xE5B0_1004, 0), "ldr r1, [r0, #4]!");
        assert_eq!(arm(0xE490_1004, 0), "ldr r1, [r0], #4");
        assert_eq!(arm(0xE510_1004, 0), "ldr r1, [r0, -#4]");
        assert_eq!(arm(0xE5C0_1000, 0), "strb r1, [r0]");
        assert_eq!(arm(0xE1D0_10B2, 0), "ldrh r1, [r0, #2]");
    }

    #[test]
    fn arm_block_transfers_collapse_register_runs() {
        assert_eq!(arm(0xE8BD_000F, 0), "ldmia sp!, {r0-r3}");
        assert_eq!(arm(0xE92D_4010, 0), "stmdb sp!, {r4, lr}");
        assert_eq!(arm(0xE8FD_8000, 0), "ldmia sp!, {pc}^");
    }

    #[test]
    fn arm_multiply_and_psr_forms() {
        assert_eq!(arm(0xE003_0291, 0), "mul r3, r1, r2");
        assert_eq!(arm(0xE023_4291, 0), "mla r3, r1, r2, r4");
        assert_eq!(arm(0xE10F_0000, 0), "mrs r0, cpsr");
        assert_eq!(arm(0xE121_F000, 0), "msr cpsr_c, r0");
        assert_eq!(arm(0xE129_F000, 0), "msr cpsr_fc, r0");
    }

    #[test]
    fn arm_coprocessor_and_undefined_encodings_are_marked() {
        assert!(arm(0xEE00_0000, 0).contains("undefined"));
        assert!(arm(0xE600_0010, 0).contains("undefined"));
    }

    #[test]
    fn thumb_basic_formats() {
        assert_eq!(thumb(0x0088, 0), "lsls r0, r1, #2");
        assert_eq!(thumb(0x1888, 0), "adds r0, r1, r2");
        assert_eq!(thumb(0x1E88, 0), "subs r0, r1, #2");
        assert_eq!(thumb(0x2005, 0), "movs r0, #5");
        assert_eq!(thumb(0x2805, 0), "cmps r0, #5");
        assert_eq!(thumb(0x4008, 0), "ands r0, r1");
        assert_eq!(thumb(0x4348, 0), "muls r0, r1");
    }

    #[test]
    fn thumb_high_register_and_branch_forms() {
        assert_eq!(thumb(0x4770, 0), "bx lr");
        assert_eq!(thumb(0x4688, 0), "mov r8, r1");
        assert_eq!(thumb(0x4408, 0), "add r0, r1");
        // Conditional branch: 0x1000 + 4 + 2*2
        assert_eq!(thumb(0xD002, 0x1000), "beq 0x00001008");
        assert_eq!(thumb(0xE7FE, 0x1000), "b 0x00001000");
    }

    #[test]
    fn thumb_stack_and_block_forms() {
        assert_eq!(thumb(0xB500, 0), "push {lr}");
        assert_eq!(thumb(0xB40F, 0), "push {r0-r3}");
        assert_eq!(thumb(0xBD01, 0), "pop {r0, pc}");
        assert_eq!(thumb(0xC803, 0), "ldmia r0!, {r0, r1}");
        assert_eq!(thumb(0xB002, 0), "add sp, #8");
        assert_eq!(thumb(0xB082, 0), "sub sp, #8");
    }

    #[test]
    fn thumb_pc_relative_load_resolves_its_pool_entry() {
        // At 0x1000, pc-aligned is 0x1004; +4 words = 0x1014.
        assert!(thumb(0x4804, 0x1000).contains("0x00001014"));
    }

    #[test]
    fn every_encoding_decodes_without_panicking() {
        // The debugger must be able to render whatever is in memory, including garbage.
        for high in 0..=0xFFu32 {
            let instr = (high << 24) | 0x00AB_CDEF;
            assert!(!decode_arm(instr, 0x1000).is_empty());
        }
        for instr in 0..=0xFFFFu32 {
            assert!(!decode_thumb(instr as u16, 0x1000).is_empty());
        }
    }

    #[test]
    fn truncated_input_yields_none() {
        assert!(ArmDisassembler.disassemble(&[0, 0, 0], 0).is_none());
        assert!(ThumbDisassembler.disassemble(&[0], 0).is_none());
    }
}
