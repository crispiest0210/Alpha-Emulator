//! Exception entry.
//!
//! # The table
//!
//! Transcribed from the ARM7TDMI Technical Reference Manual. The link-register column is the
//! part most often gotten "mostly right": the offsets differ per exception, and a wrong one
//! produces a handler that returns to the wrong instruction — which looks like random
//! corruption arbitrarily far from the actual bug.
//!
//! | Exception | Vector | Mode entered | `R14_<mode>` | Masks set |
//! |---|---|---|---|---|
//! | Reset               | `0x00` | Supervisor | unpredictable            | I, F |
//! | Undefined instr.    | `0x04` | Undefined  | address of next instr.   | I    |
//! | Software interrupt  | `0x08` | Supervisor | address of next instr.   | I    |
//! | Prefetch abort      | `0x0C` | Abort      | aborted instr. + 4       | I    |
//! | Data abort          | `0x10` | Abort      | aborted instr. + 8       | I    |
//! | IRQ                 | `0x18` | IRQ        | next instr. + 4          | I    |
//! | FIQ                 | `0x1C` | FIQ        | next instr. + 4          | I, F |
//!
//! Two different reference points appear in that column, which is exactly why callers pass the
//! computed value in rather than this module deriving it from a single offset:
//!
//! - `SWI` and undefined-instruction are raised *during* an instruction, so "next instruction"
//!   is already `regs.pc` — the fetch advanced it. Their handlers return with `MOVS PC, R14`.
//! - `IRQ` and `FIQ` are taken *between* instructions, before the fetch, so `regs.pc` is the
//!   instruction that did not run and the handler returns with `SUBS PC, R14, #4`. Hence `+4`.

use crate::{Arm7Tdmi, Mode};

/// The seven ARM7TDMI exceptions, in hardware priority order (highest first).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Exception {
    Reset,
    DataAbort,
    Fiq,
    Irq,
    PrefetchAbort,
    SoftwareInterrupt,
    UndefinedInstruction,
}

impl Exception {
    /// Address the core branches to. These are the low vectors; the ARM7TDMI has no
    /// high-vector option (that arrives with CP15 on the ARM9, in `cpu-arm946e`).
    pub const fn vector(self) -> u32 {
        match self {
            Exception::Reset => 0x0000_0000,
            Exception::UndefinedInstruction => 0x0000_0004,
            Exception::SoftwareInterrupt => 0x0000_0008,
            Exception::PrefetchAbort => 0x0000_000C,
            Exception::DataAbort => 0x0000_0010,
            // 0x14 is reserved (it was address-exception on the ARM2/3).
            Exception::Irq => 0x0000_0018,
            Exception::Fiq => 0x0000_001C,
        }
    }

    /// Mode the core switches into.
    pub const fn mode(self) -> Mode {
        match self {
            Exception::Reset | Exception::SoftwareInterrupt => Mode::Supervisor,
            Exception::UndefinedInstruction => Mode::Undefined,
            Exception::PrefetchAbort | Exception::DataAbort => Mode::Abort,
            Exception::Irq => Mode::Irq,
            Exception::Fiq => Mode::Fiq,
        }
    }

    /// Every exception masks IRQ; only reset and FIQ also mask FIQ.
    ///
    /// FIQ masking itself on entry is what makes it *fast*: the handler is not re-entered.
    pub const fn masks_fiq(self) -> bool {
        matches!(self, Exception::Reset | Exception::Fiq)
    }

    pub const fn name(self) -> &'static str {
        match self {
            Exception::Reset => "reset",
            Exception::DataAbort => "data abort",
            Exception::Fiq => "FIQ",
            Exception::Irq => "IRQ",
            Exception::PrefetchAbort => "prefetch abort",
            Exception::SoftwareInterrupt => "SWI",
            Exception::UndefinedInstruction => "undefined instruction",
        }
    }
}

impl Arm7Tdmi {
    /// Take `exception`, with `lr` already computed per the table in this module's docs.
    ///
    /// Order matters: `SPSR` must capture `CPSR` *before* the mode changes, and `R14` must be
    /// written *after*, so it lands in the new mode's bank rather than the old one's.
    pub fn enter_exception(&mut self, exception: Exception, lr: u32) {
        let saved_cpsr = self.cpsr;
        let new_mode = exception.mode();

        self.cpsr.set_mode(new_mode);
        // The SPSR write targets the bank we just switched into.
        self.regs.set_spsr(new_mode, saved_cpsr);
        self.regs.write(new_mode, 14, lr);

        // Exceptions always enter ARM state, whatever the interrupted code was running.
        self.cpsr.set_thumb(false);
        self.cpsr.set_irq_disabled(true);
        if exception.masks_fiq() {
            self.cpsr.set_fiq_disabled(true);
        }

        self.regs
            .set_pc(self.exception_base.wrapping_add(exception.vector()));
    }

    /// Raise an undefined-instruction exception for the instruction currently executing.
    ///
    /// Coprocessor instructions land here: neither the GBA nor the DS's ARM7 has a
    /// coprocessor behind those opcodes, and the architecture specifies that an absent
    /// coprocessor traps rather than silently doing nothing.
    pub fn undefined_instruction(&mut self) {
        let lr = self.regs.pc();
        tracing::debug!(
            pc = format_args!("{:#010X}", lr),
            "undefined instruction trap"
        );
        self.enter_exception(Exception::UndefinedInstruction, lr);
    }

    /// Raise a software interrupt. `regs.pc` already points past the `SWI`, which is exactly
    /// the value `R14_svc` needs.
    pub fn software_interrupt(&mut self) {
        let lr = self.regs.pc();
        self.enter_exception(Exception::SoftwareInterrupt, lr);
    }
}
