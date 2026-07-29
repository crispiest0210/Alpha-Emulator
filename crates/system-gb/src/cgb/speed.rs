//! CGB double-speed mode, behind `KEY1`.
//!
//! # What actually doubles
//!
//! The CPU clock doubles. Nothing else does. The PPU keeps drawing at the same rate, the APU
//! keeps generating at the same rate, and — the part that is easy to get wrong — the divider
//! that feeds `TIMA` and the APU frame sequencer keeps counting at the same *real* rate, so
//! `DIV` appears to tick twice as fast relative to instructions executed.
//!
//! That is why this type exposes a multiplier for the CPU rather than a flag the scheduler
//! consults: everything scheduled stays on the same cycle grid, and only the amount of work
//! the CPU gets through between two events changes. Modelling it the other way round — halving
//! every scheduled interval — would double the timer and audio rates too, which is exactly the
//! bug the [`SpeedSwitch::switch`] documentation warns about.

use core_common::{Savable, StateError, StateReader, StateWriter};

/// Register address.
pub const KEY1: u16 = 0xFF4D;

/// Cycles the CPU is stopped for while the clock changes.
///
/// Real hardware halts for 2050 machine cycles (8200 t-cycles) while the PLL relocks. Games do
/// not depend on the exact figure, but they *do* depend on it being non-zero: the switch
/// routine is entered with interrupts disabled and the elapsed time is how a game's timing
/// calibration notices the change.
pub const SWITCH_STALL_CYCLES: u64 = 8200;

/// `KEY1`, and the speed it selects.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SpeedSwitch {
    double: bool,
    armed: bool,
}

impl SpeedSwitch {
    pub fn new() -> Self {
        Self::default()
    }

    /// How many CPU cycles pass per cycle of everything else.
    #[inline]
    pub fn cpu_multiplier(&self) -> u64 {
        if self.double {
            2
        } else {
            1
        }
    }

    pub fn is_double_speed(&self) -> bool {
        self.double
    }

    /// Whether a `STOP` should switch the speed rather than enter low-power mode.
    pub fn is_armed(&self) -> bool {
        self.armed
    }

    pub fn read(&self) -> u8 {
        // Bit 7 is the current speed, bit 0 the arm flag, and every bit between reads high.
        ((self.double as u8) << 7) | 0x7E | (self.armed as u8)
    }

    /// Only bit 0 is writable — the speed itself cannot be set directly.
    pub fn write(&mut self, value: u8) {
        self.armed = value & 0x01 != 0;
    }

    /// Perform the switch a `STOP` triggers, if one is armed.
    ///
    /// Returns the cycles the CPU is stalled for, or `None` when nothing was armed — in which
    /// case the `STOP` is an ordinary one and the caller must treat it as low-power mode.
    ///
    /// The arm bit is cleared either way, so a second `STOP` does not switch again.
    pub fn switch(&mut self) -> Option<u64> {
        if !self.armed {
            return None;
        }
        self.armed = false;
        self.double = !self.double;
        Some(SWITCH_STALL_CYCLES)
    }
}

impl Savable for SpeedSwitch {
    fn save(&self, w: &mut StateWriter) {
        w.write_bool(self.double);
        w.write_bool(self.armed);
    }

    fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        self.double = r.read_bool()?;
        self.armed = r.read_bool()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_machine_runs_at_single_speed() {
        let s = SpeedSwitch::new();
        assert_eq!(s.cpu_multiplier(), 1);
        assert!(!s.is_double_speed());
        assert_eq!(s.read(), 0x7E);
    }

    #[test]
    fn stop_switches_speed_only_when_armed() {
        let mut s = SpeedSwitch::new();
        assert_eq!(s.switch(), None, "an unarmed STOP is an ordinary STOP");
        assert!(!s.is_double_speed());

        s.write(0x01);
        assert!(s.is_armed());
        assert_eq!(s.switch(), Some(SWITCH_STALL_CYCLES));
        assert!(s.is_double_speed());
        assert_eq!(s.cpu_multiplier(), 2);
    }

    #[test]
    fn the_arm_bit_clears_on_the_switch_so_a_later_stop_does_not_switch_again() {
        let mut s = SpeedSwitch::new();
        s.write(0x01);
        s.switch();
        assert!(!s.is_armed());
        assert_eq!(s.switch(), None);
        assert!(s.is_double_speed(), "and the speed did not flip back");
    }

    #[test]
    fn switching_twice_returns_to_single_speed() {
        let mut s = SpeedSwitch::new();
        s.write(0x01);
        s.switch();
        s.write(0x01);
        s.switch();
        assert!(!s.is_double_speed());
        assert_eq!(s.cpu_multiplier(), 1);
    }

    #[test]
    fn the_speed_bit_cannot_be_written_directly() {
        // Games do write 0x80 by accident when they read-modify-write KEY1; letting that
        // through would change speed without the STOP the hardware requires.
        let mut s = SpeedSwitch::new();
        s.write(0x80);
        assert!(!s.is_double_speed());
        assert!(!s.is_armed());
    }

    #[test]
    fn key1_reads_back_with_its_unused_bits_set() {
        let mut s = SpeedSwitch::new();
        s.write(0x01);
        assert_eq!(s.read(), 0x7F);
        s.switch();
        assert_eq!(s.read(), 0xFE);
    }

    #[test]
    fn speed_state_round_trips() {
        use savestate::{decode_state, encode_state};
        let mut s = SpeedSwitch::new();
        s.write(0x01);
        s.switch();
        s.write(0x01);

        let bytes = encode_state("gbc-speed", 1, &s);
        let mut restored = SpeedSwitch::new();
        decode_state("gbc-speed", 1, &bytes, &mut restored).unwrap();
        assert_eq!(s, restored);
    }
}
