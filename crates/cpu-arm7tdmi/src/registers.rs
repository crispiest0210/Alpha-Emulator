//! The banked register file.
//!
//! # Why this is indexed rather than swapped
//!
//! Every register lives in exactly one place for its whole life, and a mode switch changes
//! only *which* storage a register number resolves to. The alternative — a flat `[u32; 16]`
//! working set that gets copied to and from bank arrays on every mode change — is the classic
//! source of ARM banking bugs: one exception path forgets to save or restore, and the damage
//! only appears the next time the affected mode runs, arbitrarily far from the cause.
//!
//! The banking layout, transcribed from the ARM7TDMI Technical Reference Manual:
//!
//! ```text
//! R0-R7    shared by every mode
//! R8-R12   shared, except FIQ, which has its own bank
//! R13-R14  banked per mode: usr/sys, fiq, irq, svc, abt, und
//! R15      shared (the PC)
//! SPSR     one per exception mode; usr/sys have none
//! ```

use crate::psr::{Mode, Psr};
use core_common::{StateError, StateReader, StateWriter};

/// Shared by User and System mode.
pub const BANK_USR: usize = 0;
pub const BANK_FIQ: usize = 1;
pub const BANK_IRQ: usize = 2;
pub const BANK_SVC: usize = 3;
pub const BANK_ABT: usize = 4;
pub const BANK_UND: usize = 5;
/// Number of `R13`/`R14` banks.
pub const BANK_COUNT: usize = 6;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RegisterFile {
    /// `R0`-`R7`, never banked.
    low: [u32; 8],
    /// `R8`-`R12` for every mode except FIQ.
    high: [u32; 5],
    /// `R8`-`R12` for FIQ.
    high_fiq: [u32; 5],
    /// `R13`/`R14`, one pair per bank.
    sp_lr: [[u32; 2]; BANK_COUNT],
    /// `R15`.
    pc: u32,
    /// One `SPSR` per bank. Index [`BANK_USR`] is never read: User and System have no SPSR.
    spsr: [Psr; BANK_COUNT],
}

impl RegisterFile {
    #[inline]
    pub fn pc(&self) -> u32 {
        self.pc
    }

    #[inline]
    pub fn set_pc(&mut self, value: u32) {
        self.pc = value;
    }

    /// Read register `index` as `mode` sees it.
    #[inline]
    pub fn read(&self, mode: Mode, index: usize) -> u32 {
        match index {
            0..=7 => self.low[index],
            8..=12 => {
                if mode.banks_r8_r12() {
                    self.high_fiq[index - 8]
                } else {
                    self.high[index - 8]
                }
            }
            13 | 14 => self.sp_lr[mode.bank()][index - 13],
            15 => self.pc,
            _ => panic!("register index {index} out of range"),
        }
    }

    #[inline]
    pub fn write(&mut self, mode: Mode, index: usize, value: u32) {
        match index {
            0..=7 => self.low[index] = value,
            8..=12 => {
                if mode.banks_r8_r12() {
                    self.high_fiq[index - 8] = value;
                } else {
                    self.high[index - 8] = value;
                }
            }
            13 | 14 => self.sp_lr[mode.bank()][index - 13] = value,
            15 => self.pc = value,
            _ => panic!("register index {index} out of range"),
        }
    }

    /// Read a register as **User mode** would see it, whatever the current mode is.
    ///
    /// This is what `LDM`/`STM` with the `S` bit set and no `R15` in the list does, and what
    /// `MRS`/`MSR` under a User-bank transfer needs. Getting it wrong shows up as an
    /// exception handler that saves the wrong task's stack pointer.
    #[inline]
    pub fn read_user(&self, index: usize) -> u32 {
        self.read(Mode::User, index)
    }

    #[inline]
    pub fn write_user(&mut self, index: usize, value: u32) {
        self.write(Mode::User, index, value);
    }

    /// The `SPSR` for `mode`, or `None` for User and System.
    #[inline]
    pub fn spsr(&self, mode: Mode) -> Option<Psr> {
        mode.has_spsr().then(|| self.spsr[mode.bank()])
    }

    #[inline]
    pub fn set_spsr(&mut self, mode: Mode, value: Psr) {
        if mode.has_spsr() {
            self.spsr[mode.bank()] = value;
        }
    }

    pub(crate) fn save(&self, w: &mut StateWriter) {
        for v in self.low {
            w.write_u32(v);
        }
        for v in self.high {
            w.write_u32(v);
        }
        for v in self.high_fiq {
            w.write_u32(v);
        }
        for pair in self.sp_lr {
            w.write_u32(pair[0]);
            w.write_u32(pair[1]);
        }
        w.write_u32(self.pc);
        for psr in self.spsr {
            psr.save(w);
        }
    }

    pub(crate) fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        for v in self.low.iter_mut() {
            *v = r.read_u32()?;
        }
        for v in self.high.iter_mut() {
            *v = r.read_u32()?;
        }
        for v in self.high_fiq.iter_mut() {
            *v = r.read_u32()?;
        }
        for pair in self.sp_lr.iter_mut() {
            pair[0] = r.read_u32()?;
            pair[1] = r.read_u32()?;
        }
        self.pc = r.read_u32()?;
        for psr in self.spsr.iter_mut() {
            psr.load(r)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn low_registers_are_shared_by_every_mode() {
        let mut regs = RegisterFile::default();
        regs.write(Mode::User, 3, 0xAAAA);
        for mode in [Mode::Fiq, Mode::Irq, Mode::Supervisor, Mode::Abort] {
            assert_eq!(regs.read(mode, 3), 0xAAAA);
        }
    }

    #[test]
    fn only_fiq_sees_its_own_r8_through_r12() {
        let mut regs = RegisterFile::default();
        regs.write(Mode::User, 8, 0x1111);
        regs.write(Mode::Fiq, 8, 0x2222);

        assert_eq!(regs.read(Mode::User, 8), 0x1111);
        assert_eq!(regs.read(Mode::Irq, 8), 0x1111, "IRQ shares the main bank");
        assert_eq!(regs.read(Mode::Supervisor, 8), 0x1111);
        assert_eq!(regs.read(Mode::Fiq, 8), 0x2222);

        // R12 is the last banked one; R13 is banked for every exception mode instead.
        regs.write(Mode::Fiq, 12, 0x3333);
        assert_eq!(regs.read(Mode::User, 12), 0);
        assert_eq!(regs.read(Mode::Fiq, 12), 0x3333);
    }

    #[test]
    fn sp_and_lr_are_banked_per_exception_mode() {
        let mut regs = RegisterFile::default();
        let modes = [
            Mode::User,
            Mode::Fiq,
            Mode::Irq,
            Mode::Supervisor,
            Mode::Abort,
            Mode::Undefined,
        ];
        for (i, mode) in modes.iter().enumerate() {
            regs.write(*mode, 13, 0x1000 + i as u32);
            regs.write(*mode, 14, 0x2000 + i as u32);
        }
        for (i, mode) in modes.iter().enumerate() {
            assert_eq!(regs.read(*mode, 13), 0x1000 + i as u32, "{mode:?} sp");
            assert_eq!(regs.read(*mode, 14), 0x2000 + i as u32, "{mode:?} lr");
        }

        // System aliases User's bank rather than having one of its own.
        assert_eq!(regs.read(Mode::System, 13), regs.read(Mode::User, 13));
        regs.write(Mode::System, 13, 0xBEEF);
        assert_eq!(regs.read(Mode::User, 13), 0xBEEF);
    }

    #[test]
    fn the_program_counter_is_never_banked() {
        let mut regs = RegisterFile::default();
        regs.write(Mode::Irq, 15, 0x0800_0000);
        assert_eq!(regs.read(Mode::User, 15), 0x0800_0000);
        assert_eq!(regs.pc(), 0x0800_0000);
    }

    #[test]
    fn user_and_system_have_no_spsr() {
        let mut regs = RegisterFile::default();
        regs.set_spsr(Mode::User, Psr::new(0xDEAD));
        assert_eq!(regs.spsr(Mode::User), None);
        assert_eq!(regs.spsr(Mode::System), None);

        regs.set_spsr(Mode::Irq, Psr::new(0x1F));
        assert_eq!(regs.spsr(Mode::Irq), Some(Psr::new(0x1F)));
        assert_eq!(
            regs.spsr(Mode::Fiq),
            Some(Psr::default()),
            "each exception mode has its own SPSR"
        );
    }

    #[test]
    fn user_bank_accessors_ignore_the_current_mode() {
        let mut regs = RegisterFile::default();
        regs.write(Mode::User, 13, 0xAAA);
        regs.write(Mode::Irq, 13, 0xBBB);
        // This is what LDM/STM with the S bit needs: reach the User bank from IRQ mode.
        assert_eq!(regs.read_user(13), 0xAAA);
        regs.write_user(13, 0xCCC);
        assert_eq!(regs.read(Mode::User, 13), 0xCCC);
        assert_eq!(regs.read(Mode::Irq, 13), 0xBBB, "IRQ's bank is untouched");
    }

    #[test]
    fn round_trips_every_bank_through_a_save_state() {
        let mut regs = RegisterFile::default();
        for i in 0..15 {
            regs.write(Mode::User, i, 0x100 + i as u32);
        }
        for i in 8..15 {
            regs.write(Mode::Fiq, i, 0x200 + i as u32);
        }
        for (i, mode) in [Mode::Irq, Mode::Supervisor, Mode::Abort, Mode::Undefined]
            .iter()
            .enumerate()
        {
            regs.write(*mode, 13, 0x300 + i as u32);
            regs.write(*mode, 14, 0x400 + i as u32);
            regs.set_spsr(*mode, Psr::new(0x500 + i as u32));
        }
        regs.set_pc(0x0800_1234);

        let mut w = StateWriter::new();
        regs.save(&mut w);
        let blob = w.into_inner();

        let mut restored = RegisterFile::default();
        restored.load(&mut StateReader::new(&blob)).unwrap();
        assert_eq!(restored, regs);
    }
}
