//! THUMB-state (16-bit) instruction decode and execution.
//!
//! Implemented directly against the same register file as ARM state, not lowered into
//! equivalent ARM instructions first. A translation layer would add a decode step to the
//! hottest path in the emulator and, worse, would introduce a class of bugs where the ARM
//! equivalent is *almost* right — THUMB's flag behavior differs from the corresponding ARM
//! encodings in several places (`MOV` immediate sets flags, `ADD` with high registers does
//! not), and each such difference would become a special case in the lowering.
//!
//! The nineteen instruction formats are named as in the ARM7TDMI TRM so this file can be read
//! next to it.

use crate::Arm7Tdmi;
use core_common::{Bus, Cycles};

/// `R15` reads two bytes ahead of `regs.pc` in THUMB state: `regs.pc` already points at the
/// next instruction, and the architectural value is one further still.
const PC_AHEAD: u32 = 2;

impl Arm7Tdmi {
    pub(crate) fn step_thumb<B: Bus + ?Sized>(&mut self, bus: &mut B) -> Cycles {
        let addr = self.regs.pc() & !1;
        let instr = bus.read16(addr);
        self.regs.set_pc(addr.wrapping_add(2));
        Cycles(self.execute_thumb(instr, bus) as u64)
    }

    /// The architectural `R15`, word-aligned. PC-relative addressing ignores bit 1, because
    /// the instruction stream may sit on a halfword boundary while the data does not.
    #[inline]
    fn thumb_pc_aligned(&self) -> u32 {
        self.regs.pc().wrapping_add(PC_AHEAD) & !3
    }

    /// Execute one already-fetched ARMv4T THUMB instruction. Public for the same reason as
    /// [`Arm7Tdmi::execute_arm`].
    pub fn execute_thumb<B: Bus + ?Sized>(&mut self, instr: u16, bus: &mut B) -> u32 {
        match instr >> 12 {
            0b0000 | 0b0001 => {
                if instr & 0x1800 == 0x1800 {
                    self.thumb_add_subtract(instr) // format 2
                } else {
                    self.thumb_move_shifted(instr) // format 1
                }
            }
            0b0010 | 0b0011 => self.thumb_immediate(instr), // format 3
            0b0100 => match (instr >> 10) & 3 {
                0b00 => self.thumb_alu(instr),                // format 4
                0b01 => self.thumb_high_register(instr),      // format 5
                _ => self.thumb_pc_relative_load(instr, bus), // format 6
            },
            0b0101 => {
                if instr & 0x0200 != 0 {
                    self.thumb_load_store_extended(instr, bus) // format 8
                } else {
                    self.thumb_load_store_register(instr, bus) // format 7
                }
            }
            0b0110 | 0b0111 => self.thumb_load_store_immediate(instr, bus), // format 9
            0b1000 => self.thumb_load_store_halfword(instr, bus),           // format 10
            0b1001 => self.thumb_sp_relative(instr, bus),                   // format 11
            0b1010 => self.thumb_load_address(instr),                       // format 12
            0b1011 => {
                if instr & 0x0F00 == 0x0000 {
                    self.thumb_adjust_stack_pointer(instr) // format 13
                } else if instr & 0x0600 == 0x0400 {
                    self.thumb_push_pop(instr, bus) // format 14
                } else {
                    self.undefined_instruction();
                    3
                }
            }
            0b1100 => self.thumb_block_transfer(instr, bus), // format 15
            0b1101 => {
                let cond = (instr >> 8) & 0xF;
                if cond == 0xF {
                    self.software_interrupt(); // format 17
                    3
                } else if cond == 0xE {
                    // 0b1101_1110 is an undefined encoding rather than "always branch".
                    self.undefined_instruction();
                    3
                } else {
                    self.thumb_conditional_branch(instr) // format 16
                }
            }
            0b1110 => {
                if instr & 0x0800 != 0 {
                    // BLX suffix — an ARMv5 encoding the ARM7TDMI does not implement.
                    self.undefined_instruction();
                    3
                } else {
                    self.thumb_branch(instr) // format 18
                }
            }
            _ => self.thumb_long_branch_link(instr), // format 19
        }
    }

    // -- Format 1: move shifted register --------------------------------------

    fn thumb_move_shifted(&mut self, instr: u16) -> u32 {
        let shift_type = ((instr >> 11) & 3) as u32;
        let amount = ((instr >> 6) & 0x1F) as u32;
        let rs = ((instr >> 3) & 7) as usize;
        let rd = (instr & 7) as usize;

        // The immediate form's zero-amount rules apply: LSR #0 means #32, ASR #0 means #32.
        let (result, carry) =
            Self::shift(shift_type, amount, self.reg(rs), self.cpsr.carry(), true);
        self.set_reg(rd, result);
        self.cpsr.set_nz(result);
        self.cpsr.set_carry(carry);
        1
    }

    // -- Format 2: add/subtract -----------------------------------------------

    fn thumb_add_subtract(&mut self, instr: u16) -> u32 {
        let immediate = instr & 0x0400 != 0;
        let subtract = instr & 0x0200 != 0;
        let operand = ((instr >> 6) & 7) as u32;
        let rs = ((instr >> 3) & 7) as usize;
        let rd = (instr & 7) as usize;

        let a = self.reg(rs);
        let b = if immediate {
            operand
        } else {
            self.reg(operand as usize)
        };
        let result = if subtract {
            self.alu_sub(a, b, true, true)
        } else {
            self.alu_add(a, b, false, true)
        };
        self.set_reg(rd, result);
        1
    }

    // -- Format 3: move/compare/add/subtract immediate ------------------------

    fn thumb_immediate(&mut self, instr: u16) -> u32 {
        let op = (instr >> 11) & 3;
        let rd = ((instr >> 8) & 7) as usize;
        let value = (instr & 0xFF) as u32;
        let a = self.reg(rd);

        match op {
            0b00 => {
                // Unlike ARM's MOV immediate, THUMB's always sets N and Z (and leaves C/V).
                self.set_reg(rd, value);
                self.cpsr.set_nz(value);
            }
            0b01 => {
                self.alu_sub(a, value, true, true); // CMP, result discarded
            }
            0b10 => {
                let result = self.alu_add(a, value, false, true);
                self.set_reg(rd, result);
            }
            _ => {
                let result = self.alu_sub(a, value, true, true);
                self.set_reg(rd, result);
            }
        }
        1
    }

    // -- Format 4: ALU operations ---------------------------------------------

    fn thumb_alu(&mut self, instr: u16) -> u32 {
        let op = (instr >> 6) & 0xF;
        let rs = ((instr >> 3) & 7) as usize;
        let rd = (instr & 7) as usize;
        let a = self.reg(rd);
        let b = self.reg(rs);
        let carry_in = self.cpsr.carry();
        let mut cycles = 1;

        // Shift amounts come from a register here, so a zero amount means "leave value and
        // carry alone" rather than the immediate form's #32 interpretation.
        let apply_shift = |cpu: &mut Self, kind: u32| {
            let (result, carry) = Self::shift(kind, b & 0xFF, a, carry_in, false);
            cpu.set_reg(rd, result);
            cpu.cpsr.set_nz(result);
            cpu.cpsr.set_carry(carry);
        };

        match op {
            0x0 => {
                let r = a & b;
                self.set_reg(rd, r);
                self.cpsr.set_nz(r);
            }
            0x1 => {
                let r = a ^ b;
                self.set_reg(rd, r);
                self.cpsr.set_nz(r);
            }
            0x2 => {
                apply_shift(self, 0);
                cycles += 1;
            }
            0x3 => {
                apply_shift(self, 1);
                cycles += 1;
            }
            0x4 => {
                apply_shift(self, 2);
                cycles += 1;
            }
            0x5 => {
                let r = self.alu_add(a, b, carry_in, true);
                self.set_reg(rd, r);
            }
            0x6 => {
                let r = self.alu_sub(a, b, carry_in, true);
                self.set_reg(rd, r);
            }
            0x7 => {
                apply_shift(self, 3);
                cycles += 1;
            }
            0x8 => {
                let r = a & b;
                self.cpsr.set_nz(r); // TST
            }
            0x9 => {
                let r = self.alu_sub(0, b, true, true); // NEG is RSB #0
                self.set_reg(rd, r);
            }
            0xA => {
                self.alu_sub(a, b, true, true); // CMP
            }
            0xB => {
                self.alu_add(a, b, false, true); // CMN
            }
            0xC => {
                let r = a | b;
                self.set_reg(rd, r);
                self.cpsr.set_nz(r);
            }
            0xD => {
                let r = a.wrapping_mul(b);
                self.set_reg(rd, r);
                self.cpsr.set_nz(r);
                cycles += Self::multiply_cycles(a);
            }
            0xE => {
                let r = a & !b;
                self.set_reg(rd, r);
                self.cpsr.set_nz(r);
            }
            _ => {
                let r = !b;
                self.set_reg(rd, r);
                self.cpsr.set_nz(r);
            }
        }
        cycles
    }

    // -- Format 5: high registers and BX --------------------------------------

    fn thumb_high_register(&mut self, instr: u16) -> u32 {
        let op = (instr >> 8) & 3;
        let rd = ((instr & 7) | ((instr >> 4) & 8)) as usize;
        let rs = (((instr >> 3) & 7) | ((instr >> 3) & 8)) as usize;

        let source = if rs == 15 {
            self.regs.pc().wrapping_add(PC_AHEAD)
        } else {
            self.reg(rs)
        };

        match op {
            0b00 => {
                // ADD with high registers deliberately does not set flags.
                let a = if rd == 15 {
                    self.regs.pc().wrapping_add(PC_AHEAD)
                } else {
                    self.reg(rd)
                };
                let result = a.wrapping_add(source);
                if rd == 15 {
                    self.regs.set_pc(result & !1);
                    return 3;
                }
                self.set_reg(rd, result);
                1
            }
            0b01 => {
                // CMP is the only one of the three that sets flags.
                let a = if rd == 15 {
                    self.regs.pc().wrapping_add(PC_AHEAD)
                } else {
                    self.reg(rd)
                };
                self.alu_sub(a, source, true, true);
                1
            }
            0b10 => {
                if rd == 15 {
                    self.regs.set_pc(source & !1);
                    return 3;
                }
                self.set_reg(rd, source);
                1
            }
            _ => {
                self.branch_exchange(source);
                3
            }
        }
    }

    // -- Format 6: PC-relative load -------------------------------------------

    fn thumb_pc_relative_load<B: Bus + ?Sized>(&mut self, instr: u16, bus: &mut B) -> u32 {
        let rd = ((instr >> 8) & 7) as usize;
        let offset = (instr & 0xFF) as u32 * 4;
        let addr = self.thumb_pc_aligned().wrapping_add(offset);
        let value = bus.read32(addr & !3);
        self.set_reg(rd, value);
        3
    }

    // -- Formats 7 and 8: register-offset transfers ---------------------------

    fn thumb_load_store_register<B: Bus + ?Sized>(&mut self, instr: u16, bus: &mut B) -> u32 {
        let load = instr & 0x0800 != 0;
        let byte = instr & 0x0400 != 0;
        let ro = ((instr >> 6) & 7) as usize;
        let rb = ((instr >> 3) & 7) as usize;
        let rd = (instr & 7) as usize;
        let addr = self.reg(rb).wrapping_add(self.reg(ro));

        match (load, byte) {
            (true, true) => {
                let v = bus.read8(addr) as u32;
                self.set_reg(rd, v);
                3
            }
            (true, false) => {
                let v = Self::rotate_loaded_word(bus.read32(addr & !3), addr);
                self.set_reg(rd, v);
                3
            }
            (false, true) => {
                bus.write8(addr, self.reg(rd) as u8);
                2
            }
            (false, false) => {
                bus.write32(addr & !3, self.reg(rd));
                2
            }
        }
    }

    fn thumb_load_store_extended<B: Bus + ?Sized>(&mut self, instr: u16, bus: &mut B) -> u32 {
        let op = (instr >> 10) & 3;
        let ro = ((instr >> 6) & 7) as usize;
        let rb = ((instr >> 3) & 7) as usize;
        let rd = (instr & 7) as usize;
        let addr = self.reg(rb).wrapping_add(self.reg(ro));

        match op {
            0b00 => {
                bus.write16(addr & !1, self.reg(rd) as u16); // STRH
                2
            }
            0b01 => {
                let v = bus.read8(addr) as i8 as u32; // LDRSB
                self.set_reg(rd, v);
                3
            }
            0b10 => {
                let half = bus.read16(addr & !1) as u32; // LDRH
                let v = if addr & 1 != 0 {
                    half.rotate_right(8)
                } else {
                    half
                };
                self.set_reg(rd, v);
                3
            }
            _ => {
                // LDRSH from an odd address behaves as LDRSB on this core.
                let v = if addr & 1 != 0 {
                    bus.read8(addr) as i8 as u32
                } else {
                    bus.read16(addr) as i16 as u32
                };
                self.set_reg(rd, v);
                3
            }
        }
    }

    // -- Formats 9, 10, 11: immediate-offset transfers ------------------------

    fn thumb_load_store_immediate<B: Bus + ?Sized>(&mut self, instr: u16, bus: &mut B) -> u32 {
        let byte = instr & 0x1000 != 0;
        let load = instr & 0x0800 != 0;
        let offset5 = ((instr >> 6) & 0x1F) as u32;
        let rb = ((instr >> 3) & 7) as usize;
        let rd = (instr & 7) as usize;
        // Word offsets are scaled; byte offsets are not.
        let addr = self
            .reg(rb)
            .wrapping_add(if byte { offset5 } else { offset5 * 4 });

        match (load, byte) {
            (true, true) => {
                let v = bus.read8(addr) as u32;
                self.set_reg(rd, v);
                3
            }
            (true, false) => {
                let v = Self::rotate_loaded_word(bus.read32(addr & !3), addr);
                self.set_reg(rd, v);
                3
            }
            (false, true) => {
                bus.write8(addr, self.reg(rd) as u8);
                2
            }
            (false, false) => {
                bus.write32(addr & !3, self.reg(rd));
                2
            }
        }
    }

    fn thumb_load_store_halfword<B: Bus + ?Sized>(&mut self, instr: u16, bus: &mut B) -> u32 {
        let load = instr & 0x0800 != 0;
        let offset = ((instr >> 6) & 0x1F) as u32 * 2;
        let rb = ((instr >> 3) & 7) as usize;
        let rd = (instr & 7) as usize;
        let addr = self.reg(rb).wrapping_add(offset);

        if load {
            let half = bus.read16(addr & !1) as u32;
            let v = if addr & 1 != 0 {
                half.rotate_right(8)
            } else {
                half
            };
            self.set_reg(rd, v);
            3
        } else {
            bus.write16(addr & !1, self.reg(rd) as u16);
            2
        }
    }

    fn thumb_sp_relative<B: Bus + ?Sized>(&mut self, instr: u16, bus: &mut B) -> u32 {
        let load = instr & 0x0800 != 0;
        let rd = ((instr >> 8) & 7) as usize;
        let addr = self.reg(13).wrapping_add((instr & 0xFF) as u32 * 4);

        if load {
            let v = Self::rotate_loaded_word(bus.read32(addr & !3), addr);
            self.set_reg(rd, v);
            3
        } else {
            bus.write32(addr & !3, self.reg(rd));
            2
        }
    }

    // -- Formats 12, 13: address arithmetic -----------------------------------

    fn thumb_load_address(&mut self, instr: u16) -> u32 {
        let from_sp = instr & 0x0800 != 0;
        let rd = ((instr >> 8) & 7) as usize;
        let offset = (instr & 0xFF) as u32 * 4;
        let base = if from_sp {
            self.reg(13)
        } else {
            self.thumb_pc_aligned()
        };
        self.set_reg(rd, base.wrapping_add(offset));
        1
    }

    fn thumb_adjust_stack_pointer(&mut self, instr: u16) -> u32 {
        let offset = (instr & 0x7F) as u32 * 4;
        let sp = self.reg(13);
        let result = if instr & 0x0080 != 0 {
            sp.wrapping_sub(offset)
        } else {
            sp.wrapping_add(offset)
        };
        self.set_reg(13, result);
        1
    }

    // -- Formats 14, 15: block transfers --------------------------------------

    fn thumb_push_pop<B: Bus + ?Sized>(&mut self, instr: u16, bus: &mut B) -> u32 {
        let load = instr & 0x0800 != 0;
        let extra = instr & 0x0100 != 0; // LR on push, PC on pop
        let list = (instr & 0xFF) as u32;
        let count = list.count_ones() + extra as u32;
        let mut sp = self.reg(13);

        if load {
            // POP: registers come off in increasing address order, low register first.
            for reg in 0..8 {
                if list & (1 << reg) != 0 {
                    let v = bus.read32(sp & !3);
                    self.set_reg(reg, v);
                    sp = sp.wrapping_add(4);
                }
            }
            let mut cycles = count + 2;
            if extra {
                let v = bus.read32(sp & !3);
                sp = sp.wrapping_add(4);
                // POP {PC} on the ARM7TDMI stays in THUMB state: bit 0 is ignored rather than
                // being interpreted as an instruction-set switch. That only becomes BX-like
                // behavior on ARMv5.
                self.regs.set_pc(v & !1);
                cycles += 2;
            }
            self.set_reg(13, sp);
            cycles
        } else {
            // PUSH: decrement first, then store in increasing address order.
            sp = sp.wrapping_sub(count * 4);
            self.set_reg(13, sp);
            let mut addr = sp;
            for reg in 0..8 {
                if list & (1 << reg) != 0 {
                    bus.write32(addr & !3, self.reg(reg));
                    addr = addr.wrapping_add(4);
                }
            }
            if extra {
                bus.write32(addr & !3, self.reg(14));
            }
            count + 1
        }
    }

    fn thumb_block_transfer<B: Bus + ?Sized>(&mut self, instr: u16, bus: &mut B) -> u32 {
        let load = instr & 0x0800 != 0;
        let rb = ((instr >> 8) & 7) as usize;
        let list = (instr & 0xFF) as u32;
        let mut addr = self.reg(rb);

        if list == 0 {
            // Architecturally unpredictable; this core transfers R15 and moves the base by a
            // full block, matching the ARM-state behavior.
            if load {
                let v = bus.read32(addr & !3);
                self.regs.set_pc(v & !1);
            } else {
                bus.write32(addr & !3, self.regs.pc().wrapping_add(2));
            }
            self.set_reg(rb, addr.wrapping_add(0x40));
            return if load { 5 } else { 2 };
        }

        let count = list.count_ones();
        let writeback = addr.wrapping_add(count * 4);

        if load {
            // Writeback happens first so a load into the base register wins.
            self.set_reg(rb, writeback);
            for reg in 0..8 {
                if list & (1 << reg) != 0 {
                    let v = bus.read32(addr & !3);
                    self.set_reg(reg, v);
                    addr = addr.wrapping_add(4);
                }
            }
            count + 2
        } else {
            let mut first = true;
            for reg in 0..8 {
                if list & (1 << reg) == 0 {
                    continue;
                }
                // Storing the base register anywhere but first stores the new value.
                let value = if reg == rb && !first {
                    writeback
                } else {
                    self.reg(reg)
                };
                bus.write32(addr & !3, value);
                addr = addr.wrapping_add(4);
                first = false;
            }
            self.set_reg(rb, writeback);
            count + 1
        }
    }

    // -- Formats 16, 18, 19: branches -----------------------------------------

    fn thumb_conditional_branch(&mut self, instr: u16) -> u32 {
        if !self.cpsr.passes_condition(((instr >> 8) & 0xF) as u32) {
            return 1;
        }
        let offset = ((instr & 0xFF) as u8 as i8 as i32) * 2;
        let target = self
            .regs
            .pc()
            .wrapping_add(PC_AHEAD)
            .wrapping_add(offset as u32);
        self.regs.set_pc(target & !1);
        3
    }

    fn thumb_branch(&mut self, instr: u16) -> u32 {
        // 11-bit signed halfword offset.
        let offset = (((instr & 0x07FF) as i32) << 21 >> 20) as u32;
        let target = self.regs.pc().wrapping_add(PC_AHEAD).wrapping_add(offset);
        self.regs.set_pc(target & !1);
        3
    }

    /// `BL` is two instructions: the first stashes the high half of the offset in `LR`, the
    /// second adds the low half and swaps `LR` for the return address.
    ///
    /// They are genuinely separate instructions — an interrupt can land between them — which
    /// is why this is not modelled as one 32-bit instruction.
    fn thumb_long_branch_link(&mut self, instr: u16) -> u32 {
        let offset = (instr & 0x07FF) as u32;

        if instr & 0x0800 == 0 {
            // High half: sign-extended and shifted into place.
            let high = ((offset << 21) as i32 >> 9) as u32;
            let lr = self.regs.pc().wrapping_add(PC_AHEAD).wrapping_add(high);
            self.set_reg(14, lr);
            1
        } else {
            let return_address = self.regs.pc();
            let target = self.reg(14).wrapping_add(offset << 1);
            self.regs.set_pc(target & !1);
            // The low bit marks the return address as THUMB for the eventual BX.
            self.set_reg(14, return_address | 1);
            3
        }
    }
}
