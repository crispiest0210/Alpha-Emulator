//! ARM-state (32-bit) instruction decode and execution.
//!
//! # Cycle counts
//!
//! Costs are given in `S`/`N`/`I` cycles per the ARM7TDMI TRM and summed as one cycle each.
//! Memory wait states are not applied here — see the crate docs.
//!
//! # Reading `R15`
//!
//! The 3-stage pipeline means `R15` reads back two instructions ahead of the one executing.
//! Because `regs.pc` has already advanced past the fetch by the time an instruction runs, the
//! architectural value is `pc + 4`. The one exception is an instruction using a
//! *register-specified* shift, which takes an extra cycle and therefore sees `pc + 8`; and
//! `STR`/`STM` of `R15`, which store `pc + 8` on this core.

use crate::{Arm7Tdmi, Exception, Mode, Psr};
use core_common::{Bus, Cycles};

/// `R15` offset for a normal operand read: `pc + 4` is the architectural `instruction + 8`.
const PC_AHEAD: u32 = 4;
/// `R15` offset when a register-specified shift adds a cycle: `instruction + 12`.
const PC_AHEAD_SHIFTED: u32 = 8;

impl Arm7Tdmi {
    pub(crate) fn step_arm<B: Bus + ?Sized>(&mut self, bus: &mut B) -> Cycles {
        let addr = self.regs.pc() & !3;
        let instr = bus.read32(addr);
        self.regs.set_pc(addr.wrapping_add(4));

        // Every ARM instruction is conditional. A failed condition still costs the fetch.
        if !self.cpsr.passes_condition(instr >> 28) {
            return Cycles(1);
        }

        Cycles(self.execute_arm(instr, bus) as u64)
    }

    /// Read a register, substituting the pipeline-adjusted value for `R15`.
    #[inline]
    fn reg_pc(&self, index: usize, pc_offset: u32) -> u32 {
        if index == 15 {
            self.regs.pc().wrapping_add(pc_offset)
        } else {
            self.reg(index)
        }
    }

    fn execute_arm<B: Bus + ?Sized>(&mut self, instr: u32, bus: &mut B) -> u32 {
        // Decode order matters: several encodings are carve-outs from the data-processing
        // space and must be tested before it.
        if instr & 0x0FFF_FFF0 == 0x012F_FF10 {
            return self.arm_bx(instr);
        }
        if instr & 0x0F00_0000 == 0x0F00_0000 {
            self.software_interrupt();
            return 3;
        }
        if instr & 0x0E00_0000 == 0x0A00_0000 {
            return self.arm_branch(instr);
        }
        if instr & 0x0C00_0000 == 0x0C00_0000 {
            // Coprocessor space. Neither the GBA nor the DS's ARM7 has a coprocessor here, and
            // the architecture specifies an absent one traps rather than silently no-opping.
            self.undefined_instruction();
            return 3;
        }
        if instr & 0x0E00_0000 == 0x0800_0000 {
            return self.arm_block_transfer(instr, bus);
        }
        if instr & 0x0E00_0010 == 0x0600_0010 {
            self.undefined_instruction();
            return 3;
        }
        if instr & 0x0C00_0000 == 0x0400_0000 {
            return self.arm_single_transfer(instr, bus);
        }

        // Remaining space is 0b00xx: data processing plus its carve-outs.
        if instr & 0x0FC0_00F0 == 0x0000_0090 {
            return self.arm_multiply(instr);
        }
        if instr & 0x0F80_00F0 == 0x0080_0090 {
            return self.arm_multiply_long(instr);
        }
        if instr & 0x0FB0_0FF0 == 0x0100_0090 {
            return self.arm_swap(instr, bus);
        }
        if instr & 0x0E00_0090 == 0x0000_0090 && instr & 0x60 != 0 {
            return self.arm_halfword_transfer(instr, bus);
        }
        self.arm_data_processing(instr)
    }

    // -- Branches -------------------------------------------------------------

    fn arm_bx(&mut self, instr: u32) -> u32 {
        let target = self.reg_pc((instr & 0xF) as usize, PC_AHEAD);
        self.branch_exchange(target);
        3 // 2S + 1N pipeline refill
    }

    fn arm_branch(&mut self, instr: u32) -> u32 {
        // 24-bit signed word offset, relative to the architectural PC (instruction + 8).
        let offset = ((instr & 0x00FF_FFFF) << 8) as i32 >> 6;
        let base = self.regs.pc().wrapping_add(PC_AHEAD);
        if instr & 0x0100_0000 != 0 {
            // BL: the return address is the instruction after the branch, which regs.pc
            // already holds.
            let lr = self.regs.pc();
            self.set_reg(14, lr);
        }
        self.regs.set_pc(base.wrapping_add(offset as u32) & !3);
        3
    }

    // -- Data processing ------------------------------------------------------

    /// Evaluate the shifter operand, returning its value and the carry it produces.
    ///
    /// The carry-out feeds `CPSR.C` for logical operations, so the "shift by zero leaves carry
    /// alone" cases below are observable, not academic.
    fn shifter_operand(&mut self, instr: u32, pc_offset: u32) -> (u32, bool) {
        let carry_in = self.cpsr.carry();

        if instr & 0x0200_0000 != 0 {
            // Immediate: an 8-bit value rotated right by twice a 4-bit field.
            let rotate = ((instr >> 8) & 0xF) * 2;
            let value = (instr & 0xFF).rotate_right(rotate);
            // A zero rotate leaves the carry untouched; any other sets it from bit 31.
            let carry = if rotate == 0 {
                carry_in
            } else {
                value & 0x8000_0000 != 0
            };
            return (value, carry);
        }

        let rm = (instr & 0xF) as usize;
        let by_register = instr & 0x10 != 0;
        let value = self.reg_pc(rm, pc_offset);
        let shift_type = (instr >> 5) & 3;
        let amount = if by_register {
            self.reg(((instr >> 8) & 0xF) as usize) & 0xFF
        } else {
            (instr >> 7) & 0x1F
        };

        Self::shift(shift_type, amount, value, carry_in, !by_register)
    }

    /// The four shift types.
    ///
    /// `immediate_form` distinguishes the two meanings of a zero shift amount: written as an
    /// immediate, zero means "32" for LSR and ASR and means RRX for ROR; supplied in a
    /// register, zero genuinely means "do nothing, including to the carry".
    pub(crate) fn shift(
        shift_type: u32,
        amount: u32,
        value: u32,
        carry_in: bool,
        immediate_form: bool,
    ) -> (u32, bool) {
        if amount == 0 {
            if !immediate_form {
                // Register-specified zero: value and carry both pass through untouched.
                return (value, carry_in);
            }
            return match shift_type {
                0 => (value, carry_in),             // LSL #0 is a plain move
                1 => (0, value & 0x8000_0000 != 0), // LSR #0 means LSR #32
                2 => {
                    // ASR #0 means ASR #32: the whole register becomes the sign bit.
                    let sign = value & 0x8000_0000 != 0;
                    (if sign { u32::MAX } else { 0 }, sign)
                }
                _ => {
                    // ROR #0 means RRX: a 33-bit rotate through the carry flag.
                    ((value >> 1) | ((carry_in as u32) << 31), value & 1 != 0)
                }
            };
        }

        match shift_type {
            0 => match amount {
                1..=31 => (value << amount, value & (1 << (32 - amount)) != 0),
                32 => (0, value & 1 != 0),
                _ => (0, false),
            },
            1 => match amount {
                1..=31 => (value >> amount, value & (1 << (amount - 1)) != 0),
                32 => (0, value & 0x8000_0000 != 0),
                _ => (0, false),
            },
            2 => {
                if amount >= 32 {
                    let sign = value & 0x8000_0000 != 0;
                    (if sign { u32::MAX } else { 0 }, sign)
                } else {
                    (
                        ((value as i32) >> amount) as u32,
                        value & (1 << (amount - 1)) != 0,
                    )
                }
            }
            _ => {
                // ROR by a multiple of 32 leaves the value alone but still updates carry.
                let rotate = amount & 31;
                if rotate == 0 {
                    (value, value & 0x8000_0000 != 0)
                } else {
                    (value.rotate_right(rotate), value & (1 << (rotate - 1)) != 0)
                }
            }
        }
    }

    #[inline]
    pub(crate) fn alu_add(&mut self, a: u32, b: u32, carry_in: bool, set_flags: bool) -> u32 {
        let wide = a as u64 + b as u64 + carry_in as u64;
        let result = wide as u32;
        if set_flags {
            self.cpsr.set_nz(result);
            self.cpsr.set_carry(wide > u32::MAX as u64);
            // Overflow when both operands share a sign that the result does not.
            self.cpsr
                .set_overflow((a ^ result) & (b ^ result) & 0x8000_0000 != 0);
        }
        result
    }

    /// `a - b - (1 - carry_in)`. For plain `SUB`/`CMP`, pass `carry_in = true`.
    #[inline]
    pub(crate) fn alu_sub(&mut self, a: u32, b: u32, carry_in: bool, set_flags: bool) -> u32 {
        let borrow = !carry_in as u64;
        let result = (a as u64).wrapping_sub(b as u64).wrapping_sub(borrow) as u32;
        if set_flags {
            self.cpsr.set_nz(result);
            // On ARM, carry is the *inverse* of borrow: set when no borrow occurred.
            self.cpsr.set_carry(a as u64 >= b as u64 + borrow);
            self.cpsr
                .set_overflow((a ^ b) & (a ^ result) & 0x8000_0000 != 0);
        }
        result
    }

    fn arm_data_processing(&mut self, instr: u32) -> u32 {
        let opcode = (instr >> 21) & 0xF;
        let set_flags = instr & 0x0010_0000 != 0;

        // TST/TEQ/CMP/CMN with S clear are not comparisons at all — they are MRS/MSR.
        if !set_flags && (0b1000..=0b1011).contains(&opcode) {
            return self.arm_psr_transfer(instr);
        }

        // A register-specified shift costs an extra internal cycle and shifts every R15 read
        // in this instruction four bytes further along.
        let register_shift = instr & 0x0200_0000 == 0 && instr & 0x10 != 0;
        let pc_offset = if register_shift {
            PC_AHEAD_SHIFTED
        } else {
            PC_AHEAD
        };

        let rn = ((instr >> 16) & 0xF) as usize;
        let rd = ((instr >> 12) & 0xF) as usize;
        let (operand2, shifter_carry) = self.shifter_operand(instr, pc_offset);
        let a = self.reg_pc(rn, pc_offset);

        // Logical operations take C from the shifter and leave V alone; arithmetic ones
        // compute both from the operation itself.
        let logical = |cpu: &mut Self, result: u32| {
            if set_flags {
                cpu.cpsr.set_nz(result);
                cpu.cpsr.set_carry(shifter_carry);
            }
            result
        };

        let carry = self.cpsr.carry();
        let (result, writes_result) = match opcode {
            0b0000 => (logical(self, a & operand2), true), // AND
            0b0001 => (logical(self, a ^ operand2), true), // EOR
            0b0010 => (self.alu_sub(a, operand2, true, set_flags), true), // SUB
            0b0011 => (self.alu_sub(operand2, a, true, set_flags), true), // RSB
            0b0100 => (self.alu_add(a, operand2, false, set_flags), true), // ADD
            0b0101 => (self.alu_add(a, operand2, carry, set_flags), true), // ADC
            0b0110 => (self.alu_sub(a, operand2, carry, set_flags), true), // SBC
            0b0111 => (self.alu_sub(operand2, a, carry, set_flags), true), // RSC
            0b1000 => (logical(self, a & operand2), false), // TST
            0b1001 => (logical(self, a ^ operand2), false), // TEQ
            0b1010 => (self.alu_sub(a, operand2, true, true), false), // CMP
            0b1011 => (self.alu_add(a, operand2, false, true), false), // CMN
            0b1100 => (logical(self, a | operand2), true), // ORR
            0b1101 => (logical(self, operand2), true),     // MOV
            0b1110 => (logical(self, a & !operand2), true), // BIC
            _ => (logical(self, !operand2), true),         // MVN
        };

        let mut cycles = 1 + register_shift as u32;

        if writes_result {
            if rd == 15 {
                if set_flags {
                    // `S` with R15 as the destination restores CPSR from SPSR: this is how
                    // an exception handler returns, e.g. `SUBS PC, LR, #4`.
                    self.restore_cpsr_from_spsr();
                }
                // Writing PC refills the pipeline.
                let aligned = if self.cpsr.thumb() {
                    result & !1
                } else {
                    result & !3
                };
                self.regs.set_pc(aligned);
                cycles += 2;
            } else {
                self.set_reg(rd, result);
            }
        }
        cycles
    }

    /// Restore `CPSR` from the current mode's `SPSR`, as an exception return does.
    ///
    /// In User or System mode there is no SPSR, and the architecture calls the result
    /// unpredictable; leaving CPSR alone is the least surprising behavior and keeps the core
    /// in a valid mode.
    pub(crate) fn restore_cpsr_from_spsr(&mut self) {
        if let Some(spsr) = self.regs.spsr(self.cpsr.mode()) {
            self.cpsr = spsr;
        } else {
            tracing::debug!("exception return attempted from a mode with no SPSR");
        }
    }

    fn arm_psr_transfer(&mut self, instr: u32) -> u32 {
        let use_spsr = instr & 0x0040_0000 != 0;

        if instr & 0x0020_0000 == 0 {
            // MRS: read CPSR or SPSR into Rd.
            let rd = ((instr >> 12) & 0xF) as usize;
            let value = if use_spsr {
                self.regs.spsr(self.cpsr.mode()).unwrap_or(self.cpsr).bits()
            } else {
                self.cpsr.bits()
            };
            self.set_reg(rd, value);
            return 1;
        }

        // MSR: write selected fields of CPSR or SPSR.
        let value = if instr & 0x0200_0000 != 0 {
            let rotate = ((instr >> 8) & 0xF) * 2;
            (instr & 0xFF).rotate_right(rotate)
        } else {
            self.reg((instr & 0xF) as usize)
        };

        // The field mask selects byte lanes. On the ARM7TDMI only the flags byte and the
        // control byte hold anything.
        let mut mask = 0u32;
        if instr & (1 << 19) != 0 {
            mask |= Psr::FLAGS_MASK;
        }
        if instr & (1 << 16) != 0 {
            mask |= Psr::CONTROL_MASK;
        }

        if use_spsr {
            let mode = self.cpsr.mode();
            if let Some(mut spsr) = self.regs.spsr(mode) {
                spsr.set_bits((spsr.bits() & !mask) | (value & mask));
                self.regs.set_spsr(mode, spsr);
            }
        } else {
            // User mode may change the condition flags but not the control bits — otherwise
            // unprivileged code could unmask interrupts or switch modes.
            if !self.cpsr.mode().is_privileged() {
                mask &= Psr::FLAGS_MASK;
            }
            let bits = (self.cpsr.bits() & !mask) | (value & mask);
            self.cpsr.set_bits(bits);
        }
        1
    }

    // -- Multiply -------------------------------------------------------------

    /// Internal cycles consumed by the multiplier, which early-terminates once the remaining
    /// bits of the multiplier operand are all the same.
    pub(crate) fn multiply_cycles(operand: u32) -> u32 {
        if operand & 0xFFFF_FF00 == 0 || operand & 0xFFFF_FF00 == 0xFFFF_FF00 {
            1
        } else if operand & 0xFFFF_0000 == 0 || operand & 0xFFFF_0000 == 0xFFFF_0000 {
            2
        } else if operand & 0xFF00_0000 == 0 || operand & 0xFF00_0000 == 0xFF00_0000 {
            3
        } else {
            4
        }
    }

    fn arm_multiply(&mut self, instr: u32) -> u32 {
        let rd = ((instr >> 16) & 0xF) as usize;
        let rn = ((instr >> 12) & 0xF) as usize;
        let rs = ((instr >> 8) & 0xF) as usize;
        let rm = (instr & 0xF) as usize;
        let accumulate = instr & 0x0020_0000 != 0;
        let set_flags = instr & 0x0010_0000 != 0;

        let s_value = self.reg(rs);
        let mut result = self.reg(rm).wrapping_mul(s_value);
        if accumulate {
            result = result.wrapping_add(self.reg(rn));
        }
        self.set_reg(rd, result);

        if set_flags {
            // C is architecturally unpredictable after a multiply, and V is unaffected.
            self.cpsr.set_nz(result);
        }
        1 + Self::multiply_cycles(s_value) + accumulate as u32
    }

    fn arm_multiply_long(&mut self, instr: u32) -> u32 {
        let rd_hi = ((instr >> 16) & 0xF) as usize;
        let rd_lo = ((instr >> 12) & 0xF) as usize;
        let rs = ((instr >> 8) & 0xF) as usize;
        let rm = (instr & 0xF) as usize;
        let signed = instr & 0x0040_0000 != 0;
        let accumulate = instr & 0x0020_0000 != 0;
        let set_flags = instr & 0x0010_0000 != 0;

        let s_value = self.reg(rs);
        let m_value = self.reg(rm);

        let mut result = if signed {
            ((m_value as i32 as i64).wrapping_mul(s_value as i32 as i64)) as u64
        } else {
            (m_value as u64).wrapping_mul(s_value as u64)
        };
        if accumulate {
            let existing = ((self.reg(rd_hi) as u64) << 32) | self.reg(rd_lo) as u64;
            result = result.wrapping_add(existing);
        }

        self.set_reg(rd_lo, result as u32);
        self.set_reg(rd_hi, (result >> 32) as u32);

        if set_flags {
            self.cpsr.set_negative(result & 0x8000_0000_0000_0000 != 0);
            self.cpsr.set_zero(result == 0);
        }
        2 + Self::multiply_cycles(s_value) + accumulate as u32
    }

    // -- Memory ---------------------------------------------------------------

    fn arm_swap<B: Bus + ?Sized>(&mut self, instr: u32, bus: &mut B) -> u32 {
        let rn = ((instr >> 16) & 0xF) as usize;
        let rd = ((instr >> 12) & 0xF) as usize;
        let rm = (instr & 0xF) as usize;
        let byte = instr & 0x0040_0000 != 0;

        let addr = self.reg(rn);
        let stored = self.reg(rm);
        if byte {
            let loaded = bus.read8(addr);
            bus.write8(addr, stored as u8);
            self.set_reg(rd, loaded as u32);
        } else {
            // The read rotates for an unaligned address exactly as LDR does, but the write
            // goes to the aligned address.
            let loaded = Self::rotate_loaded_word(bus.read32(addr & !3), addr);
            bus.write32(addr & !3, stored);
            self.set_reg(rd, loaded);
        }
        4 // 1S + 2N + 1I
    }

    /// An unaligned `LDR` does not fault on this core: it reads the containing word and
    /// rotates it so the addressed byte ends up in the low lane.
    #[inline]
    pub(crate) fn rotate_loaded_word(word: u32, addr: u32) -> u32 {
        word.rotate_right((addr & 3) * 8)
    }

    fn arm_single_transfer<B: Bus + ?Sized>(&mut self, instr: u32, bus: &mut B) -> u32 {
        let pre_index = instr & 0x0100_0000 != 0;
        let add = instr & 0x0080_0000 != 0;
        let byte = instr & 0x0040_0000 != 0;
        let writeback = instr & 0x0020_0000 != 0;
        let load = instr & 0x0010_0000 != 0;
        let rn = ((instr >> 16) & 0xF) as usize;
        let rd = ((instr >> 12) & 0xF) as usize;

        // Note the inverted sense of bit 25 compared to data processing: here *set* means a
        // shifted register offset, clear means an immediate.
        let offset = if instr & 0x0200_0000 != 0 {
            let (value, _) = Self::shift(
                (instr >> 5) & 3,
                (instr >> 7) & 0x1F,
                self.reg((instr & 0xF) as usize),
                self.cpsr.carry(),
                true,
            );
            value
        } else {
            instr & 0xFFF
        };

        let base = self.reg_pc(rn, PC_AHEAD);
        let offset_base = if add {
            base.wrapping_add(offset)
        } else {
            base.wrapping_sub(offset)
        };
        let addr = if pre_index { offset_base } else { base };

        let mut cycles = if load { 3 } else { 2 };

        if load {
            let value = if byte {
                bus.read8(addr) as u32
            } else {
                Self::rotate_loaded_word(bus.read32(addr & !3), addr)
            };
            // Writeback happens before the load so that a load into the base register wins.
            if !pre_index || writeback {
                self.set_reg(rn, offset_base);
            }
            if rd == 15 {
                self.regs.set_pc(value & !3);
                cycles += 2;
            } else {
                self.set_reg(rd, value);
            }
        } else {
            // Storing R15 stores the architectural PC plus one more instruction on this core.
            let value = self.reg_pc(rd, PC_AHEAD_SHIFTED);
            if byte {
                bus.write8(addr, value as u8);
            } else {
                bus.write32(addr & !3, value);
            }
            if !pre_index || writeback {
                self.set_reg(rn, offset_base);
            }
        }
        cycles
    }

    fn arm_halfword_transfer<B: Bus + ?Sized>(&mut self, instr: u32, bus: &mut B) -> u32 {
        let pre_index = instr & 0x0100_0000 != 0;
        let add = instr & 0x0080_0000 != 0;
        let immediate = instr & 0x0040_0000 != 0;
        let writeback = instr & 0x0020_0000 != 0;
        let load = instr & 0x0010_0000 != 0;
        let rn = ((instr >> 16) & 0xF) as usize;
        let rd = ((instr >> 12) & 0xF) as usize;
        let kind = (instr >> 5) & 3;

        let offset = if immediate {
            ((instr >> 4) & 0xF0) | (instr & 0xF)
        } else {
            self.reg((instr & 0xF) as usize)
        };

        let base = self.reg_pc(rn, PC_AHEAD);
        let offset_base = if add {
            base.wrapping_add(offset)
        } else {
            base.wrapping_sub(offset)
        };
        let addr = if pre_index { offset_base } else { base };

        let mut cycles = if load { 3 } else { 2 };

        if load {
            let value = match kind {
                1 => {
                    // Unsigned halfword, rotated when misaligned.
                    let half = bus.read16(addr & !1) as u32;
                    if addr & 1 != 0 {
                        half.rotate_right(8)
                    } else {
                        half
                    }
                }
                2 => bus.read8(addr) as i8 as u32, // signed byte
                _ => {
                    // LDRSH from an odd address behaves as LDRSB on this core rather than
                    // loading a misaligned halfword.
                    if addr & 1 != 0 {
                        bus.read8(addr) as i8 as u32
                    } else {
                        bus.read16(addr) as i16 as u32
                    }
                }
            };
            if !pre_index || writeback {
                self.set_reg(rn, offset_base);
            }
            if rd == 15 {
                self.regs.set_pc(value & !3);
                cycles += 2;
            } else {
                self.set_reg(rd, value);
            }
        } else {
            let value = self.reg_pc(rd, PC_AHEAD_SHIFTED);
            bus.write16(addr & !1, value as u16);
            if !pre_index || writeback {
                self.set_reg(rn, offset_base);
            }
        }
        cycles
    }

    fn arm_block_transfer<B: Bus + ?Sized>(&mut self, instr: u32, bus: &mut B) -> u32 {
        let pre_index = instr & 0x0100_0000 != 0;
        let add = instr & 0x0080_0000 != 0;
        let psr_or_user = instr & 0x0040_0000 != 0;
        let writeback = instr & 0x0020_0000 != 0;
        let load = instr & 0x0010_0000 != 0;
        let rn = ((instr >> 16) & 0xF) as usize;
        let mut list = instr & 0xFFFF;

        let base = self.reg(rn);

        // An empty register list is architecturally unpredictable; this core transfers R15
        // alone and adjusts the base by a full 16-register block.
        let empty = list == 0;
        if empty {
            list = 0x8000;
        }
        let count = list.count_ones();
        let block = if empty { 0x40 } else { count * 4 };

        // Registers always move in increasing address order regardless of direction, so both
        // decrementing modes are expressed as an ascending walk from the lowest address.
        let lowest = if add { base } else { base.wrapping_sub(block) };
        // Pre-increment and post-decrement both shift the first slot up by one word.
        let mut addr = if pre_index == add {
            lowest.wrapping_add(4)
        } else {
            lowest
        };
        let writeback_value = if add {
            base.wrapping_add(block)
        } else {
            base.wrapping_sub(block)
        };

        // With S set and R15 absent, transfers hit the User bank instead of the current mode's.
        let transfers_user_bank = psr_or_user && !(load && list & 0x8000 != 0);
        let mode = self.cpsr.mode();

        let mut cycles = if load { count + 2 } else { count + 1 };

        if load {
            // Writeback first, so that loading into the base register overrides it.
            if writeback {
                self.set_reg(rn, writeback_value);
            }
            for reg in 0..16 {
                if list & (1 << reg) == 0 {
                    continue;
                }
                let value = bus.read32(addr & !3);
                if transfers_user_bank {
                    self.regs.write_user(reg, value);
                } else if reg == 15 {
                    // With S set, loading R15 also restores CPSR — the classic exception
                    // return, `LDMFD SP!, {..., PC}^`.
                    if psr_or_user {
                        self.restore_cpsr_from_spsr();
                    }
                    let thumb = self.cpsr.thumb();
                    self.regs
                        .set_pc(if thumb { value & !1 } else { value & !3 });
                    cycles += 2;
                } else {
                    self.regs.write(mode, reg, value);
                }
                addr = addr.wrapping_add(4);
            }
        } else {
            let mut first = true;
            for reg in 0..16 {
                if list & (1 << reg) == 0 {
                    continue;
                }
                let value = if reg == 15 {
                    self.regs.pc().wrapping_add(PC_AHEAD_SHIFTED)
                } else if reg == rn && !first && writeback {
                    // Storing the base register anywhere but first stores the already
                    // written-back value.
                    writeback_value
                } else if transfers_user_bank {
                    self.regs.read_user(reg)
                } else {
                    self.regs.read(mode, reg)
                };
                bus.write32(addr & !3, value);
                addr = addr.wrapping_add(4);
                first = false;
            }
            if writeback {
                self.set_reg(rn, writeback_value);
            }
        }
        cycles
    }

    /// Raise a data abort. Exposed for systems that can detect a faulting access.
    pub fn raise_data_abort(&mut self) {
        let lr = self.regs.pc().wrapping_add(4);
        self.enter_exception(Exception::DataAbort, lr);
    }

    /// Raise a prefetch abort for the instruction currently being fetched.
    pub fn raise_prefetch_abort(&mut self) {
        let lr = self.regs.pc();
        self.enter_exception(Exception::PrefetchAbort, lr);
    }

    /// Switch to `mode` from privileged code, as an exception entry would.
    pub fn force_mode(&mut self, mode: Mode) {
        self.set_mode(mode);
    }
}
