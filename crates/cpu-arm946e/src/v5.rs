//! The ARMv5TE instruction set delta over ARMv4T.
//!
//! Everything the two architectures share is executed by `cpu-arm7tdmi`. This module handles
//! only what ARMv5TE adds:
//!
//! - `BLX` in both its immediate and register forms, plus the THUMB `BLX` suffix
//! - `CLZ`
//! - saturating arithmetic (`QADD`, `QSUB`, `QDADD`, `QDSUB`) and the sticky `Q` flag
//! - the enhanced DSP multiplies (`SMULxy`, `SMLAxy`, `SMULWy`, `SMLAWy`, `SMLALxy`)
//! - `LDRD`/`STRD`
//! - `PLD`, and `BKPT`
//! - `MRC`/`MCR` to CP15
//!
//! Each handler returns `Option<u32>`: `Some(cycles)` when the encoding was ARMv5TE, `None`
//! when it is not, so the caller falls through to the shared ARMv4T implementation.

use crate::cp15::Cp15Effect;
use crate::Arm946e;
use core_common::Bus;

/// Saturate a 64-bit intermediate to a signed 32-bit result, reporting whether it clamped.
#[inline]
fn saturate32(value: i64) -> (u32, bool) {
    if value > i32::MAX as i64 {
        (i32::MAX as u32, true)
    } else if value < i32::MIN as i64 {
        (i32::MIN as u32, true)
    } else {
        (value as u32, false)
    }
}

/// Select the high or low signed halfword of a register.
#[inline]
fn halfword(value: u32, high: bool) -> i32 {
    if high {
        (value >> 16) as i16 as i32
    } else {
        value as i16 as i32
    }
}

impl Arm946e {
    /// Instructions in the `cond == 0b1111` space, which ARMv4T treats as "never execute".
    ///
    /// ARMv5 reuses that encoding for genuinely unconditional instructions, so this must be
    /// checked *before* the condition test rather than after it.
    pub(crate) fn execute_unconditional<B: Bus + ?Sized>(
        &mut self,
        instr: u32,
        _bus: &mut B,
    ) -> u32 {
        // BLX (immediate): a 24-bit word offset plus a halfword bit, always switching to THUMB.
        if instr & 0x0E00_0000 == 0x0A00_0000 {
            let offset = ((instr & 0x00FF_FFFF) << 8) as i32 >> 6;
            let halfword_bit = ((instr >> 24) & 1) << 1;
            let return_address = self.core.regs.pc();
            let target = self
                .core
                .regs
                .pc()
                .wrapping_add(4)
                .wrapping_add(offset as u32)
                .wrapping_add(halfword_bit);
            self.core.set_reg(14, return_address);
            self.core.cpsr.set_thumb(true);
            self.core.regs.set_pc(target & !1);
            return 3;
        }

        // PLD: a cache preload hint. With no cache model there is nothing to preload, and the
        // architecture explicitly permits treating it as a no-op.
        if instr & 0x0D70_F000 == 0x0550_F000 {
            return 1;
        }

        // MCR2/MRC2/CDP2/LDC2/STC2 address coprocessors this core does not have.
        self.core.undefined_instruction();
        3
    }

    /// Try to execute `instr` as an ARMv5TE-only encoding.
    pub(crate) fn execute_armv5<B: Bus + ?Sized>(
        &mut self,
        instr: u32,
        bus: &mut B,
    ) -> Option<u32> {
        // BLX (register): identical to BX but with 0b0011 in the low nibble of the opcode
        // field, and it captures a return address.
        if instr & 0x0FFF_FFF0 == 0x012F_FF30 {
            let target = self.core.reg_operand((instr & 0xF) as usize);
            let return_address = self.core.regs.pc();
            self.core.set_reg(14, return_address);
            self.core.branch_exchange(target);
            return Some(3);
        }

        // CLZ
        if instr & 0x0FFF_0FF0 == 0x016F_0F10 {
            let rd = ((instr >> 12) & 0xF) as usize;
            let value = self.core.reg((instr & 0xF) as usize);
            self.core.set_reg(rd, value.leading_zeros());
            return Some(1);
        }

        // BKPT. Encoded unconditionally as 0xE12.....7, and raises a prefetch abort.
        if instr & 0x0FF0_00F0 == 0x0120_0070 {
            self.core.raise_prefetch_abort();
            return Some(3);
        }

        // Saturating arithmetic: cond 0001 0op0 Rn Rd 0000 0101 Rm
        if instr & 0x0F90_0FF0 == 0x0100_0050 {
            return Some(self.saturating_arithmetic(instr));
        }

        // Enhanced DSP multiplies: cond 0001 0op0 Rd Rn Rs 1yx0 Rm
        if instr & 0x0F90_0090 == 0x0100_0080 {
            return Some(self.dsp_multiply(instr));
        }

        // LDRD/STRD live in the halfword-transfer encoding space, distinguished from
        // LDRSB/LDRSH by having the load bit *clear* with a signed operation selected.
        if instr & 0x0E00_0090 == 0x0000_0090
            && instr & 0x0010_0000 == 0
            && matches!((instr >> 5) & 3, 2 | 3)
        {
            return Some(self.double_word_transfer(instr, bus));
        }

        // Coprocessor register transfers. Only CP15 exists on this core.
        if instr & 0x0F00_0010 == 0x0E00_0010 {
            let coprocessor = (instr >> 8) & 0xF;
            if coprocessor == 15 {
                return Some(self.cp15_transfer(instr));
            }
        }

        None
    }

    fn saturating_arithmetic(&mut self, instr: u32) -> u32 {
        let op = (instr >> 21) & 3;
        let rn = ((instr >> 16) & 0xF) as usize;
        let rd = ((instr >> 12) & 0xF) as usize;
        let rm = (instr & 0xF) as usize;

        let a = self.core.reg(rm) as i32 as i64;
        let b = self.core.reg(rn) as i32 as i64;

        // The doubling in QDADD/QDSUB saturates on its own, *before* the add or subtract, and
        // a saturation at either stage sets Q. Folding the two steps into one 64-bit
        // expression would miss the first.
        let (operand, doubled_saturated) = match op {
            2 | 3 => saturate32(b * 2),
            _ => (b as u32, false),
        };
        let operand = operand as i32 as i64;

        let (result, saturated) = match op {
            0 | 2 => saturate32(a + operand), // QADD, QDADD
            _ => saturate32(a - operand),     // QSUB, QDSUB
        };

        self.core.set_reg(rd, result);
        if saturated || doubled_saturated {
            // Q is sticky: once set it stays set until software clears it via MSR.
            self.core.cpsr.set_sticky_overflow(true);
        }
        1
    }

    fn dsp_multiply(&mut self, instr: u32) -> u32 {
        let op = (instr >> 21) & 3;
        let rd = ((instr >> 16) & 0xF) as usize;
        let rn = ((instr >> 12) & 0xF) as usize;
        let rs = ((instr >> 8) & 0xF) as usize;
        let rm = (instr & 0xF) as usize;
        let x = instr & (1 << 5) != 0;
        let y = instr & (1 << 6) != 0;

        let m = self.core.reg(rm);
        let s = self.core.reg(rs);

        match op {
            // SMLAxy: 16 x 16 + 32, with the accumulate able to overflow and set Q.
            0b00 => {
                let product = (halfword(m, x) as i64) * (halfword(s, y) as i64);
                let accumulate = self.core.reg(rn) as i32 as i64;
                let (result, saturated) = saturate32(product + accumulate);
                self.core.set_reg(rd, result);
                if saturated {
                    self.core.cpsr.set_sticky_overflow(true);
                }
            }
            // SMLAWy / SMULWy: 32 x 16, keeping the upper 32 bits of the 48-bit product.
            0b01 => {
                let product = ((m as i32 as i64) * (halfword(s, y) as i64)) >> 16;
                if x {
                    // SMULWy has no accumulate and cannot saturate.
                    self.core.set_reg(rd, product as u32);
                } else {
                    let accumulate = self.core.reg(rn) as i32 as i64;
                    let (result, saturated) = saturate32(product + accumulate);
                    self.core.set_reg(rd, result);
                    if saturated {
                        self.core.cpsr.set_sticky_overflow(true);
                    }
                }
            }
            // SMLALxy: 16 x 16 accumulated into a 64-bit pair. Never saturates — it wraps.
            0b10 => {
                let product = (halfword(m, x) as i64) * (halfword(s, y) as i64);
                let existing = ((self.core.reg(rd) as u64) << 32) | self.core.reg(rn) as u64;
                let result = (existing as i64).wrapping_add(product) as u64;
                self.core.set_reg(rn, result as u32);
                self.core.set_reg(rd, (result >> 32) as u32);
            }
            // SMULxy: plain 16 x 16, no accumulate, no saturation.
            _ => {
                let product = (halfword(m, x) as i64) * (halfword(s, y) as i64);
                self.core.set_reg(rd, product as u32);
            }
        }
        1
    }

    fn double_word_transfer<B: Bus + ?Sized>(&mut self, instr: u32, bus: &mut B) -> u32 {
        let pre_index = instr & 0x0100_0000 != 0;
        let add = instr & 0x0080_0000 != 0;
        let immediate = instr & 0x0040_0000 != 0;
        let writeback = instr & 0x0020_0000 != 0;
        let store = (instr >> 5) & 3 == 3;
        let rn = ((instr >> 16) & 0xF) as usize;
        let rd = ((instr >> 12) & 0xF) as usize;

        // The register pair must start on an even register; an odd Rd is unpredictable.
        if rd & 1 != 0 || rd == 14 {
            tracing::debug!(rd, "LDRD/STRD with an invalid register pair");
            self.core.undefined_instruction();
            return 3;
        }

        let offset = if immediate {
            ((instr >> 4) & 0xF0) | (instr & 0xF)
        } else {
            self.core.reg((instr & 0xF) as usize)
        };

        let base = self.core.reg_operand(rn);
        let offset_base = if add {
            base.wrapping_add(offset)
        } else {
            base.wrapping_sub(offset)
        };
        let addr = if pre_index { offset_base } else { base };

        self.with_tcm_bus(bus, |core, view| {
            if store {
                view.write32(addr & !3, core.reg(rd));
                view.write32(addr.wrapping_add(4) & !3, core.reg(rd + 1));
            } else {
                let low = view.read32(addr & !3);
                let high = view.read32(addr.wrapping_add(4) & !3);
                core.set_reg(rd, low);
                core.set_reg(rd + 1, high);
            }
        });

        if !pre_index || writeback {
            self.core.set_reg(rn, offset_base);
        }
        if store {
            3
        } else {
            4
        }
    }

    fn cp15_transfer(&mut self, instr: u32) -> u32 {
        let opcode1 = (instr >> 21) & 7;
        let load = instr & 0x0010_0000 != 0;
        let crn = (instr >> 16) & 0xF;
        let rd = ((instr >> 12) & 0xF) as usize;
        let opcode2 = (instr >> 5) & 7;
        let crm = instr & 0xF;

        if load {
            let value = self.cp15.read(opcode1, crn, crm, opcode2);
            if rd == 15 {
                // MRC to R15 writes the flag bits rather than branching.
                let bits = (self.core.cpsr.bits() & 0x0FFF_FFFF) | (value & 0xF000_0000);
                self.core.cpsr.set_bits(bits);
            } else {
                self.core.set_reg(rd, value);
            }
        } else {
            let value = self.core.reg(rd);
            match self.cp15.write(opcode1, crn, crm, opcode2, value) {
                Cp15Effect::None => {}
                Cp15Effect::ControlChanged => self.apply_control_register(),
                Cp15Effect::DtcmConfigured(v) => self.dtcm.configure(v, false),
                Cp15Effect::ItcmConfigured(v) => self.itcm.configure(v, true),
                Cp15Effect::WaitForInterrupt => self.core.halt(),
            }
        }
        1
    }
}
