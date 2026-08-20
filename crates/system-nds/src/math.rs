//! The ARM9's hardware divider and square-root unit.
//!
//! # Why a CPU with no divide instruction has these
//!
//! ARMv5TE has no integer division, so every division a DS program performs is either a software
//! routine or these registers. libnds routes its whole fixed-point maths library through them —
//! `div32`, `divf32`, `sqrtf32`, and everything built on those: `gluPerspective`, `gluLookAt`,
//! vector normalisation, and the projection matrix every 3D program builds before it draws
//! anything.
//!
//! That makes them quiet in a way that is worth stating. They are not a graphics feature and
//! nothing about a missing one says "graphics". Unimplemented, they read as zero, so every
//! division returns zero, every matrix built from one is a matrix of zeros, and every vertex
//! multiplied by it collapses to the origin. What that looks like from outside is a 3D program
//! that submits its geometry perfectly and draws nothing — which reads as a rasteriser bug, and
//! sends someone to read the rasteriser.
//!
//! # Both are instant here, and that is visible in exactly one bit
//!
//! Hardware takes 18 or 34 cycles to divide and 13 to take a root, and reports progress in a busy
//! bit that software spins on. Both operations complete inside the write that starts them here, so
//! the busy bit always reads clear and the spin exits immediately. A program cannot tell the
//! difference except by timing, because the busy bit is the only thing it can observe — and no
//! program times these, since the whole point of them is that they are faster than the software
//! routine.
//!
//! # ARM9 only
//!
//! The ARM7 has neither, and its I/O space has nothing at these addresses. The bus decode passes
//! the core in for that reason; an ARM7 access falls through to open bus rather than being
//! answered here.

use core_common::{Savable, StateError, StateReader, StateWriter};

pub mod reg {
    pub const DIVCNT: u32 = 0x0400_0280;
    pub const DIV_NUMERATOR: u32 = 0x0400_0290;
    pub const DIV_DENOMINATOR: u32 = 0x0400_0298;
    pub const DIV_RESULT: u32 = 0x0400_02A0;
    pub const DIV_REMAINDER: u32 = 0x0400_02A8;
    pub const SQRTCNT: u32 = 0x0400_02B0;
    pub const SQRT_RESULT: u32 = 0x0400_02B4;
    pub const SQRT_PARAM: u32 = 0x0400_02B8;
    /// One past the last address either unit answers.
    pub const END: u32 = 0x0400_02C0;
}

/// `DIVCNT` bit 14: the denominator was zero.
const DIV_BY_ZERO: u16 = 1 << 14;
/// Bit 15 of both control registers. Always reads clear; see the module docs.
const BUSY: u16 = 1 << 15;

/// The divider and the square-root unit, which share nothing but an address range and an owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MathUnits {
    div_control: u16,
    numerator: u64,
    denominator: u64,
    quotient: u64,
    remainder: u64,
    sqrt_control: u16,
    sqrt_param: u64,
    sqrt_result: u32,
}

impl MathUnits {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether this address belongs to either unit.
    ///
    /// The whole span is theirs — nothing else on a DS lives between `DIVCNT` and `SQRT_PARAM`'s
    /// high half — so this is a range rather than a list, and the gaps inside it read as zero.
    pub fn owns(addr: u32) -> bool {
        (reg::DIVCNT..reg::END).contains(&addr)
    }

    pub fn read32(&self, addr: u32) -> u32 {
        match addr & !3 {
            reg::DIVCNT => (self.div_control & !BUSY) as u32,
            reg::DIV_NUMERATOR => self.numerator as u32,
            0x0400_0294 => (self.numerator >> 32) as u32,
            reg::DIV_DENOMINATOR => self.denominator as u32,
            0x0400_029C => (self.denominator >> 32) as u32,
            reg::DIV_RESULT => self.quotient as u32,
            0x0400_02A4 => (self.quotient >> 32) as u32,
            reg::DIV_REMAINDER => self.remainder as u32,
            0x0400_02AC => (self.remainder >> 32) as u32,
            reg::SQRTCNT => (self.sqrt_control & !BUSY) as u32,
            reg::SQRT_RESULT => self.sqrt_result,
            reg::SQRT_PARAM => self.sqrt_param as u32,
            0x0400_02BC => (self.sqrt_param >> 32) as u32,
            _ => 0,
        }
    }

    pub fn write32(&mut self, addr: u32, value: u32) {
        let splice_low = |current: u64, value: u32| (current & !0xFFFF_FFFF) | value as u64;
        let splice_high =
            |current: u64, value: u32| (current & 0xFFFF_FFFF) | ((value as u64) << 32);
        match addr & !3 {
            reg::DIVCNT => self.div_control = value as u16,
            reg::DIV_NUMERATOR => self.numerator = splice_low(self.numerator, value),
            0x0400_0294 => self.numerator = splice_high(self.numerator, value),
            reg::DIV_DENOMINATOR => self.denominator = splice_low(self.denominator, value),
            0x0400_029C => self.denominator = splice_high(self.denominator, value),
            reg::SQRTCNT => self.sqrt_control = value as u16,
            reg::SQRT_PARAM => self.sqrt_param = splice_low(self.sqrt_param, value),
            0x0400_02BC => self.sqrt_param = splice_high(self.sqrt_param, value),
            // The results are read-only; a write to one is a driver bug, not a register.
            _ => return,
        }
        // Hardware restarts on a write to *either* the operands or the control register, and a
        // driver relies on that: it writes the mode last as often as first. Recomputing on every
        // write costs a division nobody reads and means there is no ordering to get wrong.
        self.divide();
        self.root();
    }

    pub fn read16(&self, addr: u32) -> u16 {
        let word = self.read32(addr & !3);
        if addr & 2 == 0 {
            word as u16
        } else {
            (word >> 16) as u16
        }
    }

    pub fn write16(&mut self, addr: u32, value: u16) {
        let current = self.read32(addr & !3);
        let spliced = if addr & 2 == 0 {
            (current & 0xFFFF_0000) | value as u32
        } else {
            (current & 0xFFFF) | ((value as u32) << 16)
        };
        self.write32(addr & !3, spliced);
    }

    pub fn read8(&self, addr: u32) -> u8 {
        (self.read32(addr & !3) >> ((addr & 3) * 8)) as u8
    }

    pub fn write8(&mut self, addr: u32, value: u8) {
        let current = self.read32(addr & !3);
        let shift = (addr & 3) * 8;
        let spliced = (current & !(0xFF << shift)) | ((value as u32) << shift);
        self.write32(addr & !3, spliced);
    }

    /// Divide, in whichever of the three widths `DIVCNT` selects.
    ///
    /// The widths are not interchangeable. Mode 0 takes both operands from the low words and
    /// sign-extends them; mode 1 keeps a 64-bit numerator over a 32-bit denominator, which is what
    /// libnds's `divf32` uses because it has shifted its numerator up twelve places and needs the
    /// room; mode 2 is 64 by 64. Reading a 64-bit operand in mode 0 divides by whatever happened to
    /// be left in the high half from the previous call.
    fn divide(&mut self) {
        // The flag is about the register, not about the operand this mode uses: a denominator
        // whose low word is non-zero but whose high word makes it huge is not a division by zero,
        // and one whose low word is zero in 64-bit mode is only a division by zero if the whole of
        // it is.
        self.div_control &= !DIV_BY_ZERO;
        if self.denominator == 0 {
            self.div_control |= DIV_BY_ZERO;
        }

        let mode = self.div_control & 3;
        let (numerator, denominator) = match mode {
            0 => (
                self.numerator as u32 as i32 as i64,
                self.denominator as u32 as i32 as i64,
            ),
            1 => (self.numerator as i64, self.denominator as u32 as i32 as i64),
            _ => (self.numerator as i64, self.denominator as i64),
        };

        if denominator == 0 {
            // Documented, and worth following exactly rather than leaving the previous result in
            // place: the remainder is the numerator untouched and the quotient is ±1 with the
            // *opposite* sign to the numerator. In the 32-bit mode the high word comes back
            // inverted, which is hardware showing its working rather than a value anyone uses.
            self.remainder = numerator as u64;
            let quotient = if numerator < 0 { 1i64 } else { -1i64 } as u64;
            self.quotient = if mode == 0 {
                quotient ^ 0xFFFF_FFFF_0000_0000
            } else {
                quotient
            };
            return;
        }

        // The one division that has no answer in two's complement. Hardware returns the numerator
        // and no remainder; Rust would panic.
        if numerator == i64::MIN && denominator == -1 {
            self.quotient = numerator as u64;
            self.remainder = 0;
            return;
        }

        self.quotient = (numerator / denominator) as u64;
        self.remainder = (numerator % denominator) as u64;
    }

    /// The integer square root of an *unsigned* operand.
    ///
    /// Unsigned is the part worth stating: `SQRT_PARAM` with its top bit set is a very large
    /// number here and a negative one to the reader, and treating it as signed makes every root of
    /// a large value zero.
    fn root(&mut self) {
        let value = if self.sqrt_control & 1 != 0 {
            self.sqrt_param
        } else {
            self.sqrt_param as u32 as u64
        };
        self.sqrt_result = value.isqrt() as u32;
    }
}

impl Savable for MathUnits {
    fn save(&self, w: &mut StateWriter) {
        w.write_u16(self.div_control);
        w.write_u64(self.numerator);
        w.write_u64(self.denominator);
        w.write_u16(self.sqrt_control);
        w.write_u64(self.sqrt_param);
        // The results are written too, rather than recomputed on load from the operands above.
        // They *are* a pure function of those operands everywhere except one place — a machine
        // that has not divided yet, where the registers hold zero and the divide-by-zero flag is
        // clear even though the denominator is zero. Recomputing invents that flag at power-on and
        // makes a state that has been through a save differ from one that has not.
        w.write_u64(self.quotient);
        w.write_u64(self.remainder);
        w.write_u32(self.sqrt_result);
    }

    fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        self.div_control = r.read_u16()?;
        self.numerator = r.read_u64()?;
        self.denominator = r.read_u64()?;
        self.sqrt_control = r.read_u16()?;
        self.sqrt_param = r.read_u64()?;
        self.quotient = r.read_u64()?;
        self.remainder = r.read_u64()?;
        self.sqrt_result = r.read_u32()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
