//! Instruction execution.
//!
//! A single `match` rather than a table of function pointers: the opcode space has enough
//! regular structure (`LD r,r'`, the ALU block, and the whole `0xCB` page are index
//! arithmetic) that a match expresses it more compactly, and it lets the optimizer inline the
//! common arms instead of going through a call per instruction.
//!
//! Cycle costs come from the tables in [`crate::CYCLES`] rather than being written into each
//! arm, so timing lives in exactly one place and can be checked against a reference.

use crate::{cb_cycles, Sm83, CYCLES, CYCLES_BRANCH_TAKEN, FLAG_C, FLAG_H, FLAG_N, FLAG_Z};
use core_common::Bus;

/// Operand register index encoded in the low 3 bits of most opcodes.
const R_HL_INDIRECT: u8 = 6;

impl Sm83 {
    // -- Operand access -------------------------------------------------------

    /// Read the register selected by a 3-bit operand field: `B C D E H L (HL) A`.
    #[inline]
    fn read_r<B: Bus + ?Sized>(&mut self, idx: u8, bus: &mut B) -> u8 {
        match idx {
            0 => self.b,
            1 => self.c,
            2 => self.d,
            3 => self.e,
            4 => self.h,
            5 => self.l,
            R_HL_INDIRECT => bus.read8(self.hl() as u32),
            _ => self.a,
        }
    }

    #[inline]
    fn write_r<B: Bus + ?Sized>(&mut self, idx: u8, bus: &mut B, value: u8) {
        match idx {
            0 => self.b = value,
            1 => self.c = value,
            2 => self.d = value,
            3 => self.e = value,
            4 => self.h = value,
            5 => self.l = value,
            R_HL_INDIRECT => bus.write8(self.hl() as u32, value),
            _ => self.a = value,
        }
    }

    /// Branch condition encoded in bits 3-4: `NZ Z NC C`.
    #[inline]
    fn condition(&self, idx: u8) -> bool {
        match idx & 3 {
            0 => !self.flag(FLAG_Z),
            1 => self.flag(FLAG_Z),
            2 => !self.flag(FLAG_C),
            _ => self.flag(FLAG_C),
        }
    }

    // -- 8-bit ALU ------------------------------------------------------------

    #[inline]
    fn alu_add(&mut self, value: u8, carry_in: bool) {
        let c = carry_in as u16;
        let a = self.a as u16;
        let v = value as u16;
        let result = a + v + c;
        let half = (a & 0x0F) + (v & 0x0F) + c > 0x0F;
        self.a = result as u8;
        self.set_flags(self.a == 0, false, half, result > 0xFF);
    }

    #[inline]
    fn alu_sub(&mut self, value: u8, carry_in: bool) {
        let c = carry_in as u16;
        let a = self.a as u16;
        let v = value as u16;
        let result = a.wrapping_sub(v).wrapping_sub(c);
        let half = (a & 0x0F) < (v & 0x0F) + c;
        self.a = result as u8;
        self.set_flags(self.a == 0, true, half, a < v + c);
    }

    /// `CP` is `SUB` with the result discarded — the flags are the entire point.
    #[inline]
    fn alu_cp(&mut self, value: u8) {
        let a = self.a;
        self.alu_sub(value, false);
        self.a = a;
    }

    #[inline]
    fn alu_and(&mut self, value: u8) {
        self.a &= value;
        // AND is the one logical op that sets H. Not a typo, and it is tested.
        self.set_flags(self.a == 0, false, true, false);
    }

    #[inline]
    fn alu_or(&mut self, value: u8) {
        self.a |= value;
        self.set_flags(self.a == 0, false, false, false);
    }

    #[inline]
    fn alu_xor(&mut self, value: u8) {
        self.a ^= value;
        self.set_flags(self.a == 0, false, false, false);
    }

    /// `INC r` leaves the carry flag alone, which is what makes 16-bit arithmetic sequences
    /// built out of 8-bit increments work.
    #[inline]
    fn alu_inc(&mut self, value: u8) -> u8 {
        let result = value.wrapping_add(1);
        self.set_flag(FLAG_Z, result == 0);
        self.set_flag(FLAG_N, false);
        self.set_flag(FLAG_H, value & 0x0F == 0x0F);
        result
    }

    #[inline]
    fn alu_dec(&mut self, value: u8) -> u8 {
        let result = value.wrapping_sub(1);
        self.set_flag(FLAG_Z, result == 0);
        self.set_flag(FLAG_N, true);
        self.set_flag(FLAG_H, value & 0x0F == 0);
        result
    }

    /// `ADD HL,rr`: half-carry is out of bit 11, and Z is left untouched.
    #[inline]
    fn add_hl(&mut self, value: u16) {
        let hl = self.hl();
        let result = hl as u32 + value as u32;
        let half = (hl & 0x0FFF) + (value & 0x0FFF) > 0x0FFF;
        self.set_hl(result as u16);
        self.set_flag(FLAG_N, false);
        self.set_flag(FLAG_H, half);
        self.set_flag(FLAG_C, result > 0xFFFF);
    }

    /// Shared by `ADD SP,r8` and `LD HL,SP+r8`.
    ///
    /// The offset is signed, but the flags are computed as if adding the *unsigned* low byte:
    /// H from bit 3 and C from bit 7, both from the bottom 8 bits only. Computing them from
    /// the 16-bit result instead is a classic accuracy bug.
    #[inline]
    fn add_sp_offset(&mut self, offset: i8) -> u16 {
        let sp = self.sp;
        let delta = offset as i16 as u16;
        let half = (sp & 0x000F) + (delta & 0x000F) > 0x000F;
        let carry = (sp & 0x00FF) + (delta & 0x00FF) > 0x00FF;
        self.set_flags(false, false, half, carry);
        sp.wrapping_add(delta)
    }

    /// Decimal-adjust the accumulator after a BCD add or subtract.
    ///
    /// The SM83 version differs from the Z80's: it uses the N flag to decide whether to add
    /// or subtract the correction, rather than tracking it separately. The carry flag, once
    /// set here, is never cleared by `DAA`.
    fn daa(&mut self) {
        let mut correction = 0u8;
        let mut carry = self.flag(FLAG_C);

        if self.flag(FLAG_H) || (!self.flag(FLAG_N) && (self.a & 0x0F) > 0x09) {
            correction |= 0x06;
        }
        if self.flag(FLAG_C) || (!self.flag(FLAG_N) && self.a > 0x99) {
            correction |= 0x60;
            carry = true;
        }

        self.a = if self.flag(FLAG_N) {
            self.a.wrapping_sub(correction)
        } else {
            self.a.wrapping_add(correction)
        };

        self.set_flag(FLAG_Z, self.a == 0);
        self.set_flag(FLAG_H, false);
        self.set_flag(FLAG_C, carry);
    }

    // -- Rotates and shifts ---------------------------------------------------
    //
    // These set Z from the result. The accumulator-only forms (RLCA/RRCA/RLA/RRA) reuse them
    // and then force Z clear, which is the documented difference between `RLCA` and `CB RLC A`.

    #[inline]
    fn rlc(&mut self, value: u8) -> u8 {
        let carry = value & 0x80 != 0;
        let result = value.rotate_left(1);
        self.set_flags(result == 0, false, false, carry);
        result
    }

    #[inline]
    fn rrc(&mut self, value: u8) -> u8 {
        let carry = value & 0x01 != 0;
        let result = value.rotate_right(1);
        self.set_flags(result == 0, false, false, carry);
        result
    }

    #[inline]
    fn rl(&mut self, value: u8) -> u8 {
        let carry_in = self.flag(FLAG_C) as u8;
        let carry = value & 0x80 != 0;
        let result = (value << 1) | carry_in;
        self.set_flags(result == 0, false, false, carry);
        result
    }

    #[inline]
    fn rr(&mut self, value: u8) -> u8 {
        let carry_in = (self.flag(FLAG_C) as u8) << 7;
        let carry = value & 0x01 != 0;
        let result = (value >> 1) | carry_in;
        self.set_flags(result == 0, false, false, carry);
        result
    }

    #[inline]
    fn sla(&mut self, value: u8) -> u8 {
        let carry = value & 0x80 != 0;
        let result = value << 1;
        self.set_flags(result == 0, false, false, carry);
        result
    }

    /// Arithmetic shift right: bit 7 is replicated, preserving sign.
    #[inline]
    fn sra(&mut self, value: u8) -> u8 {
        let carry = value & 0x01 != 0;
        let result = (value >> 1) | (value & 0x80);
        self.set_flags(result == 0, false, false, carry);
        result
    }

    #[inline]
    fn srl(&mut self, value: u8) -> u8 {
        let carry = value & 0x01 != 0;
        let result = value >> 1;
        self.set_flags(result == 0, false, false, carry);
        result
    }

    #[inline]
    fn swap(&mut self, value: u8) -> u8 {
        let result = value.rotate_left(4);
        self.set_flags(result == 0, false, false, false);
        result
    }

    // -- HALT -----------------------------------------------------------------

    /// `HALT`, including the hardware bug.
    ///
    /// With IME clear and an interrupt already pending, the CPU does not halt at all. Instead
    /// the next opcode fetch fails to advance PC, so that byte is read twice — a two-byte
    /// instruction after `HALT` therefore decodes as something else entirely. Real games do
    /// hit this, and the Mooneye suite tests it directly, so it is implemented deliberately
    /// rather than being something the code happens to do.
    ///
    /// Note that `EI; HALT` does *not* trigger the bug: `EI` sets IME immediately (only
    /// dispatch is delayed), so by the time `HALT` runs, IME is set and this takes the normal
    /// path.
    fn halt<B: Bus + ?Sized>(&mut self, bus: &mut B) {
        let pending = bus.read8(crate::IF_ADDR) & bus.read8(crate::IE_ADDR) & 0x1F;
        if !self.ime() && pending != 0 {
            self.halt_bug = true;
        } else {
            self.halted = true;
        }
    }

    // -- Dispatch -------------------------------------------------------------

    /// Execute one already-fetched opcode, returning its t-cycle cost.
    pub(crate) fn execute<B: Bus + ?Sized>(&mut self, op: u8, bus: &mut B) -> u32 {
        let base = CYCLES[op as usize] as u32;

        match op {
            // -- 0x00-0x3F: misc, 16-bit loads, INC/DEC, rotates ---------------
            0x00 => {} // NOP
            0x10 => {
                // STOP is two bytes; the second is ignored by the CPU itself.
                let _ = self.fetch8(bus);
                self.stopped = true;
            }
            0x76 => self.halt(bus),

            0x01 | 0x11 | 0x21 | 0x31 => {
                let v = self.fetch16(bus);
                match op >> 4 {
                    0 => self.set_bc(v),
                    1 => self.set_de(v),
                    2 => self.set_hl(v),
                    _ => self.sp = v,
                }
            }

            0x02 => bus.write8(self.bc() as u32, self.a),
            0x12 => bus.write8(self.de() as u32, self.a),
            0x22 => {
                let hl = self.hl();
                bus.write8(hl as u32, self.a);
                self.set_hl(hl.wrapping_add(1));
            }
            0x32 => {
                let hl = self.hl();
                bus.write8(hl as u32, self.a);
                self.set_hl(hl.wrapping_sub(1));
            }
            0x0A => self.a = bus.read8(self.bc() as u32),
            0x1A => self.a = bus.read8(self.de() as u32),
            0x2A => {
                let hl = self.hl();
                self.a = bus.read8(hl as u32);
                self.set_hl(hl.wrapping_add(1));
            }
            0x3A => {
                let hl = self.hl();
                self.a = bus.read8(hl as u32);
                self.set_hl(hl.wrapping_sub(1));
            }

            0x08 => {
                let addr = self.fetch16(bus) as u32;
                let [lo, hi] = self.sp.to_le_bytes();
                bus.write8(addr, lo);
                bus.write8(addr.wrapping_add(1), hi);
            }

            0x03 => self.set_bc(self.bc().wrapping_add(1)),
            0x13 => self.set_de(self.de().wrapping_add(1)),
            0x23 => self.set_hl(self.hl().wrapping_add(1)),
            0x33 => self.sp = self.sp.wrapping_add(1),
            0x0B => self.set_bc(self.bc().wrapping_sub(1)),
            0x1B => self.set_de(self.de().wrapping_sub(1)),
            0x2B => self.set_hl(self.hl().wrapping_sub(1)),
            0x3B => self.sp = self.sp.wrapping_sub(1),

            0x09 => self.add_hl(self.bc()),
            0x19 => self.add_hl(self.de()),
            0x29 => self.add_hl(self.hl()),
            0x39 => self.add_hl(self.sp),

            // INC r / DEC r / LD r,d8 for every operand register, including (HL).
            0x04 | 0x0C | 0x14 | 0x1C | 0x24 | 0x2C | 0x34 | 0x3C => {
                let idx = (op >> 3) & 7;
                let v = self.read_r(idx, bus);
                let v = self.alu_inc(v);
                self.write_r(idx, bus, v);
            }
            0x05 | 0x0D | 0x15 | 0x1D | 0x25 | 0x2D | 0x35 | 0x3D => {
                let idx = (op >> 3) & 7;
                let v = self.read_r(idx, bus);
                let v = self.alu_dec(v);
                self.write_r(idx, bus, v);
            }
            0x06 | 0x0E | 0x16 | 0x1E | 0x26 | 0x2E | 0x36 | 0x3E => {
                let idx = (op >> 3) & 7;
                let v = self.fetch8(bus);
                self.write_r(idx, bus, v);
            }

            // Accumulator rotates: same operation as the CB forms but Z is always cleared.
            0x07 => {
                self.a = self.rlc(self.a);
                self.set_flag(FLAG_Z, false);
            }
            0x0F => {
                self.a = self.rrc(self.a);
                self.set_flag(FLAG_Z, false);
            }
            0x17 => {
                self.a = self.rl(self.a);
                self.set_flag(FLAG_Z, false);
            }
            0x1F => {
                self.a = self.rr(self.a);
                self.set_flag(FLAG_Z, false);
            }

            0x27 => self.daa(),
            0x2F => {
                self.a = !self.a;
                self.set_flag(FLAG_N, true);
                self.set_flag(FLAG_H, true);
            }
            0x37 => {
                self.set_flag(FLAG_N, false);
                self.set_flag(FLAG_H, false);
                self.set_flag(FLAG_C, true);
            }
            0x3F => {
                let c = self.flag(FLAG_C);
                self.set_flag(FLAG_N, false);
                self.set_flag(FLAG_H, false);
                self.set_flag(FLAG_C, !c);
            }

            // JR r8 / JR cc,r8. The offset is always consumed, taken or not.
            0x18 => {
                let offset = self.fetch8(bus) as i8;
                self.pc = self.pc.wrapping_add(offset as i16 as u16);
            }
            0x20 | 0x28 | 0x30 | 0x38 => {
                let offset = self.fetch8(bus) as i8;
                if self.condition((op >> 3) & 3) {
                    self.pc = self.pc.wrapping_add(offset as i16 as u16);
                    return base + CYCLES_BRANCH_TAKEN[op as usize] as u32;
                }
            }

            // -- 0x40-0x7F: LD r,r' (0x76 handled above as HALT) ---------------
            0x40..=0x7F => {
                let value = self.read_r(op & 7, bus);
                self.write_r((op >> 3) & 7, bus, value);
            }

            // -- 0x80-0xBF: ALU A,r --------------------------------------------
            0x80..=0xBF => {
                let value = self.read_r(op & 7, bus);
                self.alu_op((op >> 3) & 7, value);
            }

            // -- 0xC0-0xFF -----------------------------------------------------
            0xC6 | 0xCE | 0xD6 | 0xDE | 0xE6 | 0xEE | 0xF6 | 0xFE => {
                let value = self.fetch8(bus);
                self.alu_op((op >> 3) & 7, value);
            }

            0xC1 => {
                let v = self.pop16(bus);
                self.set_bc(v);
            }
            0xD1 => {
                let v = self.pop16(bus);
                self.set_de(v);
            }
            0xE1 => {
                let v = self.pop16(bus);
                self.set_hl(v);
            }
            0xF1 => {
                let v = self.pop16(bus);
                // `set_af` masks off the low nibble of F, which hardware cannot store.
                self.set_af(v);
            }

            0xC5 => self.push16(bus, self.bc()),
            0xD5 => self.push16(bus, self.de()),
            0xE5 => self.push16(bus, self.hl()),
            0xF5 => self.push16(bus, self.af()),

            0xC3 => self.pc = self.fetch16(bus),
            0xC2 | 0xCA | 0xD2 | 0xDA => {
                let target = self.fetch16(bus);
                if self.condition((op >> 3) & 3) {
                    self.pc = target;
                    return base + CYCLES_BRANCH_TAKEN[op as usize] as u32;
                }
            }
            0xE9 => self.pc = self.hl(), // JP HL — a register move, despite the `JP (HL)` mnemonic

            0xCD => {
                let target = self.fetch16(bus);
                let ret = self.pc;
                self.push16(bus, ret);
                self.pc = target;
            }
            0xC4 | 0xCC | 0xD4 | 0xDC => {
                let target = self.fetch16(bus);
                if self.condition((op >> 3) & 3) {
                    let ret = self.pc;
                    self.push16(bus, ret);
                    self.pc = target;
                    return base + CYCLES_BRANCH_TAKEN[op as usize] as u32;
                }
            }

            0xC9 => self.pc = self.pop16(bus),
            0xD9 => {
                // RETI enables interrupts immediately — no one-instruction delay, unlike EI.
                self.pc = self.pop16(bus);
                self.ime = true;
                self.ime_dispatch_inhibited = false;
            }
            0xC0 | 0xC8 | 0xD0 | 0xD8 => {
                if self.condition((op >> 3) & 3) {
                    self.pc = self.pop16(bus);
                    return base + CYCLES_BRANCH_TAKEN[op as usize] as u32;
                }
            }

            0xC7 | 0xCF | 0xD7 | 0xDF | 0xE7 | 0xEF | 0xF7 | 0xFF => {
                let ret = self.pc;
                self.push16(bus, ret);
                self.pc = (op & 0x38) as u16;
            }

            0xE0 => {
                let offset = self.fetch8(bus) as u32;
                bus.write8(0xFF00 + offset, self.a);
            }
            0xF0 => {
                let offset = self.fetch8(bus) as u32;
                self.a = bus.read8(0xFF00 + offset);
            }
            0xE2 => bus.write8(0xFF00 + self.c as u32, self.a),
            0xF2 => self.a = bus.read8(0xFF00 + self.c as u32),
            0xEA => {
                let addr = self.fetch16(bus) as u32;
                bus.write8(addr, self.a);
            }
            0xFA => {
                let addr = self.fetch16(bus) as u32;
                self.a = bus.read8(addr);
            }

            0xE8 => {
                let offset = self.fetch8(bus) as i8;
                self.sp = self.add_sp_offset(offset);
            }
            0xF8 => {
                let offset = self.fetch8(bus) as i8;
                let v = self.add_sp_offset(offset);
                self.set_hl(v);
            }
            0xF9 => self.sp = self.hl(),

            0xF3 => {
                // DI takes effect immediately and cancels a pending EI.
                self.ime = false;
                self.ime_dispatch_inhibited = false;
            }
            0xFB => {
                self.ime = true;
                self.ime_dispatch_inhibited = true;
            }

            0xCB => {
                let cb_op = self.fetch8(bus);
                self.execute_cb(cb_op, bus);
                return cb_cycles(cb_op);
            }

            // Opcodes that do not exist on this CPU. On hardware these hang the machine until
            // reset, so this does the same rather than pretending they are NOPs — a PC that
            // has wandered into garbage should stop loudly, not corrupt state quietly.
            0xD3 | 0xDB | 0xDD | 0xE3 | 0xE4 | 0xEB | 0xEC | 0xED | 0xF4 | 0xFC | 0xFD => {
                tracing::error!(
                    opcode = format_args!("{op:#04X}"),
                    pc = format_args!("{:#06X}", self.pc.wrapping_sub(1)),
                    "executed an undefined SM83 opcode; CPU locked, as on hardware"
                );
                self.locked = true;
                return 4;
            }
        }

        base
    }

    /// The eight ALU operations selected by bits 3-5 of an ALU opcode.
    #[inline]
    fn alu_op(&mut self, which: u8, value: u8) {
        match which {
            0 => self.alu_add(value, false),
            1 => self.alu_add(value, self.flag(FLAG_C)),
            2 => self.alu_sub(value, false),
            3 => self.alu_sub(value, self.flag(FLAG_C)),
            4 => self.alu_and(value),
            5 => self.alu_xor(value),
            6 => self.alu_or(value),
            _ => self.alu_cp(value),
        }
    }

    /// The `0xCB` page: rotates/shifts, then `BIT`/`RES`/`SET`.
    fn execute_cb<B: Bus + ?Sized>(&mut self, op: u8, bus: &mut B) {
        let idx = op & 7;
        let bit = (op >> 3) & 7;

        match op {
            // Rotates and shifts, 0x00-0x3F.
            0x00..=0x3F => {
                let value = self.read_r(idx, bus);
                let result = match op >> 3 {
                    0 => self.rlc(value),
                    1 => self.rrc(value),
                    2 => self.rl(value),
                    3 => self.rr(value),
                    4 => self.sla(value),
                    5 => self.sra(value),
                    6 => self.swap(value),
                    _ => self.srl(value),
                };
                self.write_r(idx, bus, result);
            }

            // BIT b,r — tests only, and notably leaves the carry flag alone.
            0x40..=0x7F => {
                let value = self.read_r(idx, bus);
                self.set_flag(FLAG_Z, value & (1 << bit) == 0);
                self.set_flag(FLAG_N, false);
                self.set_flag(FLAG_H, true);
            }

            // RES b,r
            0x80..=0xBF => {
                let value = self.read_r(idx, bus) & !(1 << bit);
                self.write_r(idx, bus, value);
            }

            // SET b,r
            0xC0..=0xFF => {
                let value = self.read_r(idx, bus) | (1 << bit);
                self.write_r(idx, bus, value);
            }
        }
    }
}
