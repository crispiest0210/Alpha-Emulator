//! Program status registers and processor modes.

use core_common::{StateError, StateReader, StateWriter};

/// The seven ARM7TDMI processor modes.
///
/// The bit patterns are the architectural `CPSR[4:0]` encodings, transcribed from the
/// ARM7TDMI Technical Reference Manual. They are deliberately non-contiguous and must not be
/// "tidied" into a dense enum discriminant — code reads and writes them through `CPSR`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Mode {
    User = 0b1_0000,
    Fiq = 0b1_0001,
    Irq = 0b1_0010,
    Supervisor = 0b1_0011,
    Abort = 0b1_0111,
    Undefined = 0b1_1011,
    /// Privileged, but shares User's register bank. Exists so privileged code can reach the
    /// User bank without being unprivileged.
    System = 0b1_1111,
}

impl Mode {
    #[inline]
    pub const fn bits(self) -> u32 {
        self as u32
    }

    #[inline]
    pub const fn from_bits(bits: u32) -> Option<Mode> {
        match bits & 0x1F {
            0b1_0000 => Some(Mode::User),
            0b1_0001 => Some(Mode::Fiq),
            0b1_0010 => Some(Mode::Irq),
            0b1_0011 => Some(Mode::Supervisor),
            0b1_0111 => Some(Mode::Abort),
            0b1_1011 => Some(Mode::Undefined),
            0b1_1111 => Some(Mode::System),
            _ => None,
        }
    }

    /// Which register bank this mode selects for `R13`/`R14`/`SPSR`.
    ///
    /// System shares User's bank — that is the entire point of System mode.
    #[inline]
    pub const fn bank(self) -> usize {
        match self {
            Mode::User | Mode::System => crate::registers::BANK_USR,
            Mode::Fiq => crate::registers::BANK_FIQ,
            Mode::Irq => crate::registers::BANK_IRQ,
            Mode::Supervisor => crate::registers::BANK_SVC,
            Mode::Abort => crate::registers::BANK_ABT,
            Mode::Undefined => crate::registers::BANK_UND,
        }
    }

    /// Only FIQ banks `R8`–`R12`, which is what makes fast interrupt handlers fast.
    #[inline]
    pub const fn banks_r8_r12(self) -> bool {
        matches!(self, Mode::Fiq)
    }

    /// User and System have no `SPSR`: there is no exception to return from.
    #[inline]
    pub const fn has_spsr(self) -> bool {
        !matches!(self, Mode::User | Mode::System)
    }

    /// User mode cannot change the control bits of `CPSR`.
    #[inline]
    pub const fn is_privileged(self) -> bool {
        !matches!(self, Mode::User)
    }

    pub const fn name(self) -> &'static str {
        match self {
            Mode::User => "usr",
            Mode::Fiq => "fiq",
            Mode::Irq => "irq",
            Mode::Supervisor => "svc",
            Mode::Abort => "abt",
            Mode::Undefined => "und",
            Mode::System => "sys",
        }
    }
}

/// A program status register (`CPSR` or one of the banked `SPSR`s).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub struct Psr(u32);

impl Psr {
    pub const N: u32 = 1 << 31;
    pub const Z: u32 = 1 << 30;
    pub const C: u32 = 1 << 29;
    pub const V: u32 = 1 << 28;
    /// Sticky overflow, set by ARMv5TE saturating arithmetic and never cleared implicitly.
    ///
    /// Defined here rather than in `cpu-arm946e` because it lives in the same `CPSR` word;
    /// the ARM7TDMI simply never sets it.
    pub const Q: u32 = 1 << 27;
    /// IRQ disable.
    pub const I: u32 = 1 << 7;
    /// FIQ disable.
    pub const F: u32 = 1 << 6;
    /// THUMB state.
    pub const T: u32 = 1 << 5;
    pub const MODE_MASK: u32 = 0x1F;

    /// Bits an `MSR` may write in the flags byte and the control byte respectively.
    pub const FLAGS_MASK: u32 = 0xF000_0000;
    pub const CONTROL_MASK: u32 = 0x0000_00FF;

    #[inline]
    pub const fn new(bits: u32) -> Self {
        Psr(bits)
    }

    #[inline]
    pub const fn bits(self) -> u32 {
        self.0
    }

    #[inline]
    pub fn set_bits(&mut self, bits: u32) {
        self.0 = bits;
    }

    #[inline]
    fn get(self, mask: u32) -> bool {
        self.0 & mask != 0
    }

    #[inline]
    fn set(&mut self, mask: u32, on: bool) {
        if on {
            self.0 |= mask;
        } else {
            self.0 &= !mask;
        }
    }

    #[inline]
    pub fn negative(self) -> bool {
        self.get(Self::N)
    }
    #[inline]
    pub fn zero(self) -> bool {
        self.get(Self::Z)
    }
    #[inline]
    pub fn carry(self) -> bool {
        self.get(Self::C)
    }
    #[inline]
    pub fn overflow(self) -> bool {
        self.get(Self::V)
    }
    #[inline]
    pub fn irq_disabled(self) -> bool {
        self.get(Self::I)
    }
    #[inline]
    pub fn fiq_disabled(self) -> bool {
        self.get(Self::F)
    }
    #[inline]
    pub fn thumb(self) -> bool {
        self.get(Self::T)
    }
    #[inline]
    pub fn sticky_overflow(self) -> bool {
        self.get(Self::Q)
    }

    #[inline]
    pub fn set_negative(&mut self, on: bool) {
        self.set(Self::N, on)
    }
    #[inline]
    pub fn set_zero(&mut self, on: bool) {
        self.set(Self::Z, on)
    }
    #[inline]
    pub fn set_carry(&mut self, on: bool) {
        self.set(Self::C, on)
    }
    #[inline]
    pub fn set_overflow(&mut self, on: bool) {
        self.set(Self::V, on)
    }
    #[inline]
    pub fn set_irq_disabled(&mut self, on: bool) {
        self.set(Self::I, on)
    }
    #[inline]
    pub fn set_fiq_disabled(&mut self, on: bool) {
        self.set(Self::F, on)
    }
    #[inline]
    pub fn set_thumb(&mut self, on: bool) {
        self.set(Self::T, on)
    }
    #[inline]
    pub fn set_sticky_overflow(&mut self, on: bool) {
        self.set(Self::Q, on)
    }

    /// Set N and Z from a 32-bit result. The two flags almost always move together.
    #[inline]
    pub fn set_nz(&mut self, result: u32) {
        self.set_negative(result & 0x8000_0000 != 0);
        self.set_zero(result == 0);
    }

    /// The current mode.
    ///
    /// An invalid encoding cannot arise from `MSR` (which this core masks) but could arise
    /// from a corrupt save state, so it degrades to Supervisor rather than panicking — a
    /// wedged emulator is more debuggable than a crashed one.
    #[inline]
    pub fn mode(self) -> Mode {
        Mode::from_bits(self.0).unwrap_or(Mode::Supervisor)
    }

    #[inline]
    pub fn set_mode(&mut self, mode: Mode) {
        self.0 = (self.0 & !Self::MODE_MASK) | mode.bits();
    }

    /// Evaluate one of the 16 ARM condition codes against these flags.
    ///
    /// Condition `0b1111` (`NV`) is architecturally "never" on ARMv4 and is treated as such;
    /// ARMv5 reuses the encoding for unconditional instructions, which is an ARM9 concern
    /// handled in `cpu-arm946e`, not here.
    #[inline]
    pub fn passes_condition(self, cond: u32) -> bool {
        match cond & 0xF {
            0x0 => self.zero(),                                        // EQ
            0x1 => !self.zero(),                                       // NE
            0x2 => self.carry(),                                       // CS/HS
            0x3 => !self.carry(),                                      // CC/LO
            0x4 => self.negative(),                                    // MI
            0x5 => !self.negative(),                                   // PL
            0x6 => self.overflow(),                                    // VS
            0x7 => !self.overflow(),                                   // VC
            0x8 => self.carry() && !self.zero(),                       // HI
            0x9 => !self.carry() || self.zero(),                       // LS
            0xA => self.negative() == self.overflow(),                 // GE
            0xB => self.negative() != self.overflow(),                 // LT
            0xC => !self.zero() && self.negative() == self.overflow(), // GT
            0xD => self.zero() || self.negative() != self.overflow(),  // LE
            0xE => true,                                               // AL
            _ => false,                                                // NV
        }
    }

    pub(crate) fn save(&self, w: &mut StateWriter) {
        w.write_u32(self.0);
    }

    pub(crate) fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        self.0 = r.read_u32()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_encodings_round_trip() {
        for mode in [
            Mode::User,
            Mode::Fiq,
            Mode::Irq,
            Mode::Supervisor,
            Mode::Abort,
            Mode::Undefined,
            Mode::System,
        ] {
            assert_eq!(Mode::from_bits(mode.bits()), Some(mode));
        }
        assert_eq!(Mode::from_bits(0b1_0100), None);
    }

    #[test]
    fn system_mode_shares_the_user_bank_but_is_privileged() {
        assert_eq!(Mode::System.bank(), Mode::User.bank());
        assert!(Mode::System.is_privileged());
        assert!(!Mode::User.is_privileged());
        assert!(!Mode::System.has_spsr());
        assert!(!Mode::User.has_spsr());
        assert!(Mode::Irq.has_spsr());
    }

    #[test]
    fn only_fiq_banks_the_upper_general_registers() {
        assert!(Mode::Fiq.banks_r8_r12());
        for mode in [Mode::Irq, Mode::Supervisor, Mode::Abort, Mode::Undefined] {
            assert!(!mode.banks_r8_r12());
        }
    }

    #[test]
    fn setting_mode_preserves_the_other_psr_bits() {
        let mut psr = Psr::new(0xF000_00D3);
        psr.set_mode(Mode::Irq);
        assert_eq!(psr.bits(), 0xF000_00D2);
        assert_eq!(psr.mode(), Mode::Irq);
        assert!(psr.negative() && psr.zero() && psr.carry() && psr.overflow());
    }

    #[test]
    fn every_condition_code_evaluates_per_the_architecture() {
        // (flags as NZCV, condition, expected)
        let cases: &[(bool, bool, bool, bool, u32, bool)] = &[
            (false, true, false, false, 0x0, true),   // EQ with Z set
            (false, false, false, false, 0x0, false), // EQ with Z clear
            (false, false, false, false, 0x1, true),  // NE
            (false, false, true, false, 0x2, true),   // CS
            (false, false, false, false, 0x3, true),  // CC
            (true, false, false, false, 0x4, true),   // MI
            (false, false, false, false, 0x5, true),  // PL
            (false, false, false, true, 0x6, true),   // VS
            (false, false, false, false, 0x7, true),  // VC
            (false, false, true, false, 0x8, true),   // HI: C set, Z clear
            (false, true, true, false, 0x8, false),   // HI fails when Z is set
            (false, true, true, false, 0x9, true),    // LS
            (true, false, false, true, 0xA, true),    // GE: N == V
            (true, false, false, false, 0xA, false),
            (true, false, false, false, 0xB, true), // LT: N != V
            (false, false, false, false, 0xC, true), // GT
            (false, true, false, false, 0xC, false), // GT fails when Z is set
            (false, true, false, false, 0xD, true), // LE
            (false, false, false, false, 0xE, true), // AL
            (true, true, true, true, 0xF, false),   // NV is never, on ARMv4
        ];

        for &(n, z, c, v, cond, expected) in cases {
            let mut psr = Psr::default();
            psr.set_negative(n);
            psr.set_zero(z);
            psr.set_carry(c);
            psr.set_overflow(v);
            assert_eq!(
                psr.passes_condition(cond),
                expected,
                "condition {cond:#X} with N={n} Z={z} C={c} V={v}"
            );
        }
    }

    #[test]
    fn a_corrupt_mode_encoding_degrades_instead_of_panicking() {
        assert_eq!(Psr::new(0b1_0100).mode(), Mode::Supervisor);
    }
}
