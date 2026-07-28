//! ARM7TDMI CPU core — the GBA's processor, and the Nintendo DS's ARM7 coprocessor.
//!
//! This crate is consumed twice: as the sole CPU of `system-gba`, and as the ARM7 side of
//! `system-nds`. Nothing here knows which. If a change to this crate needs a sentence
//! beginning "well, on the GBA…", it belongs in a system crate instead — that separation is
//! the whole reason getting this core right once pays for itself twice.
//!
//! # Cycle accounting
//!
//! [`Cpu::step`] returns the CPU's own cycle count: one per bus access (`S`/`N`) plus internal
//! (`I`) cycles. It deliberately does **not** include memory wait states, because wait-state
//! tables are a property of each system's memory map, not of this processor. The owning system
//! counts wait cycles in its own `Bus` implementation — which already sees every access — and
//! adds them to what `step` returns.
//!
//! # Interrupt lines, not interrupt registers
//!
//! Unlike a Game Boy, an ARM7TDMI has physical `nIRQ`/`nFIQ` inputs; the interrupt *controller*
//! is a separate peripheral. So this core exposes [`Arm7Tdmi::set_irq_line`] and
//! [`Arm7Tdmi::set_fiq_line`] rather than reading memory-mapped enable/flag registers. The GBA's
//! `IE`/`IF`/`IME` and the DS's equivalent live in their system crates and drive these lines.
//!
//! # Register banking
//!
//! Banked registers are stored once and *indexed* by the current mode. They are never copied in
//! and out on a mode switch: copy-on-switch is the classic source of banking bugs where one
//! rarely-taken path forgets to save or restore a register, and the corruption only shows up in
//! whichever mode is entered next.

#![deny(unsafe_code)]

mod arm;
mod disasm;
mod exception;
mod psr;
mod registers;
mod thumb;

#[cfg(test)]
mod tests;

pub use disasm::{ArmDisassembler, ThumbDisassembler};
pub use exception::Exception;
pub use psr::{Mode, Psr};
pub use registers::{RegisterFile, BANK_ABT, BANK_FIQ, BANK_IRQ, BANK_SVC, BANK_UND, BANK_USR};

use core_common::{
    Bus, Cpu, CpuIntrospect, Cycles, RegisterValue, Savable, StateError, StateReader, StateWriter,
};

/// Where the CPU starts fetching, and in which mode.
///
/// Exposed as a constructor parameter because the GBA and the DS's ARM7 enter differently: the
/// GBA begins in the BIOS at `0x0000_0000` in Supervisor mode, while a DS ARM7 handed control
/// by the firmware starts elsewhere. Neither arrangement is this crate's business to know.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BootState {
    pub pc: u32,
    pub mode: Mode,
    pub thumb: bool,
    /// Initial `SP` for the entry mode.
    pub sp: u32,
    /// Whether IRQs start masked.
    pub irq_disabled: bool,
    pub fiq_disabled: bool,
}

impl Default for BootState {
    /// Power-on reset: Supervisor mode at the reset vector with both interrupt lines masked,
    /// exactly as the hardware reset exception leaves the core.
    fn default() -> Self {
        Self {
            pc: 0x0000_0000,
            mode: Mode::Supervisor,
            thumb: false,
            sp: 0,
            irq_disabled: true,
            fiq_disabled: true,
        }
    }
}

/// The ARM7TDMI core.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Arm7Tdmi {
    pub regs: RegisterFile,
    pub cpsr: Psr,

    /// State of the external `nIRQ` input. The system drives this; the core samples it at
    /// instruction boundaries.
    irq_line: bool,
    /// State of the external `nFIQ` input.
    fiq_line: bool,

    /// The core is in a low-power wait state and will not fetch until an interrupt line is
    /// asserted. Driven by the system (GBA's BIOS `Halt`, the DS's `HALTCNT`), because what
    /// triggers it is a system-level register, not a CPU instruction.
    halted: bool,

    /// Reset state, retained so [`Cpu::reset`] can return here rather than to a hardcoded
    /// power-on state that would be wrong for one of the two consumers.
    boot: BootState,
}

impl Default for Arm7Tdmi {
    fn default() -> Self {
        Self::new(BootState::default())
    }
}

impl Arm7Tdmi {
    pub fn new(boot: BootState) -> Self {
        let mut cpu = Self {
            regs: RegisterFile::default(),
            cpsr: Psr::default(),
            irq_line: false,
            fiq_line: false,
            halted: false,
            boot,
        };
        cpu.apply_boot_state();
        cpu
    }

    fn apply_boot_state(&mut self) {
        let boot = self.boot;
        self.regs = RegisterFile::default();
        self.cpsr = Psr::default();
        self.cpsr.set_mode(boot.mode);
        self.cpsr.set_thumb(boot.thumb);
        self.cpsr.set_irq_disabled(boot.irq_disabled);
        self.cpsr.set_fiq_disabled(boot.fiq_disabled);
        self.regs.set_pc(boot.pc);
        self.regs.write(boot.mode, 13, boot.sp);
        self.irq_line = false;
        self.fiq_line = false;
        self.halted = false;
    }

    // -- External signals -----------------------------------------------------

    /// Drive the `nIRQ` input. The core takes the exception at the next instruction boundary
    /// if `CPSR.I` is clear.
    #[inline]
    pub fn set_irq_line(&mut self, asserted: bool) {
        self.irq_line = asserted;
    }

    /// Drive the `nFIQ` input.
    #[inline]
    pub fn set_fiq_line(&mut self, asserted: bool) {
        self.fiq_line = asserted;
    }

    /// Enter the low-power wait state.
    ///
    /// An asserted interrupt line wakes the core **regardless of `CPSR.I`**: the wake signal
    /// comes from the interrupt controller, not from the CPU's own mask. Systems that want
    /// masked interrupts not to wake the core simply do not assert the line.
    #[inline]
    pub fn halt(&mut self) {
        self.halted = true;
    }

    #[inline]
    pub fn is_halted(&self) -> bool {
        self.halted
    }

    // -- Convenience ----------------------------------------------------------

    #[inline]
    pub fn mode(&self) -> Mode {
        self.cpsr.mode()
    }

    #[inline]
    pub fn is_thumb(&self) -> bool {
        self.cpsr.thumb()
    }

    /// Read a register as the current mode sees it. `R15` reads back the raw PC — the
    /// pipeline offset is applied by the instruction decoders, which know whether the
    /// architectural `+8`/`+4` or the register-specified-shift `+12` applies.
    #[inline]
    pub fn reg(&self, index: usize) -> u32 {
        self.regs.read(self.cpsr.mode(), index)
    }

    #[inline]
    pub fn set_reg(&mut self, index: usize, value: u32) {
        self.regs.write(self.cpsr.mode(), index, value);
    }

    /// Branch, switching instruction set from the low bit of `target` as `BX` does.
    #[inline]
    pub(crate) fn branch_exchange(&mut self, target: u32) {
        let thumb = target & 1 != 0;
        self.cpsr.set_thumb(thumb);
        self.regs
            .set_pc(if thumb { target & !1 } else { target & !3 });
    }

    /// Switch mode, keeping banked storage indexed rather than copied.
    #[inline]
    pub(crate) fn set_mode(&mut self, mode: Mode) {
        self.cpsr.set_mode(mode);
    }
}

impl<B: Bus + ?Sized> Cpu<B> for Arm7Tdmi {
    fn step(&mut self, bus: &mut B) -> Cycles {
        // Exceptions are taken at instruction boundaries, FIQ before IRQ.
        if self.fiq_line && !self.cpsr.fiq_disabled() {
            self.halted = false;
            let lr = self.regs.pc().wrapping_add(4);
            self.enter_exception(Exception::Fiq, lr);
            return Cycles(3);
        }
        if self.irq_line && !self.cpsr.irq_disabled() {
            self.halted = false;
            let lr = self.regs.pc().wrapping_add(4);
            self.enter_exception(Exception::Irq, lr);
            return Cycles(3);
        }

        if self.halted {
            // An asserted line wakes the core even with the corresponding CPSR mask set; the
            // controller, not the CPU, decides what counts as a wake event.
            if self.irq_line || self.fiq_line {
                self.halted = false;
            } else {
                return Cycles(1);
            }
        }

        if self.cpsr.thumb() {
            self.step_thumb(bus)
        } else {
            self.step_arm(bus)
        }
    }

    fn reset(&mut self) {
        self.apply_boot_state();
    }
}

impl CpuIntrospect for Arm7Tdmi {
    /// The active register file, then CPSR/SPSR, then every banked register that is *not*
    /// currently visible.
    ///
    /// The inactive banks are included on purpose: the most common ARM debugging question is
    /// "what did the mode I came from have in R13?", and a debugger that can only show the
    /// current mode cannot answer it.
    fn registers(&self) -> Vec<RegisterValue> {
        const NAMES: [&str; 16] = [
            "r0", "r1", "r2", "r3", "r4", "r5", "r6", "r7", "r8", "r9", "r10", "r11", "r12", "sp",
            "lr", "pc",
        ];
        let mode = self.cpsr.mode();
        let mut out: Vec<RegisterValue> = NAMES
            .iter()
            .enumerate()
            .map(|(i, name)| RegisterValue::new(name, self.regs.read(mode, i) as u64, 32))
            .collect();

        out.push(RegisterValue::new("cpsr", self.cpsr.bits() as u64, 32));
        if let Some(spsr) = self.regs.spsr(mode) {
            out.push(RegisterValue::new("spsr", spsr.bits() as u64, 32));
        }

        for (bank_name, bank_mode) in [
            ("fiq", Mode::Fiq),
            ("irq", Mode::Irq),
            ("svc", Mode::Supervisor),
            ("abt", Mode::Abort),
            ("und", Mode::Undefined),
        ] {
            if bank_mode == mode {
                continue;
            }
            let (sp_name, lr_name) = match bank_name {
                "fiq" => ("sp_fiq", "lr_fiq"),
                "irq" => ("sp_irq", "lr_irq"),
                "svc" => ("sp_svc", "lr_svc"),
                "abt" => ("sp_abt", "lr_abt"),
                _ => ("sp_und", "lr_und"),
            };
            out.push(RegisterValue::new(
                sp_name,
                self.regs.read(bank_mode, 13) as u64,
                32,
            ));
            out.push(RegisterValue::new(
                lr_name,
                self.regs.read(bank_mode, 14) as u64,
                32,
            ));
        }
        out
    }

    /// The address of the next instruction to execute — not the pipeline-adjusted `R15` an
    /// instruction would observe. Breakpoint comparison depends on this distinction.
    fn program_counter(&self) -> u32 {
        self.regs.pc()
    }

    fn set_program_counter(&mut self, pc: u32) {
        self.regs.set_pc(pc);
    }

    fn flags_summary(&self) -> String {
        let f = |on: bool, set: char, clear: char| if on { set } else { clear };
        format!(
            "{}{}{}{} {}{}{} {}",
            f(self.cpsr.negative(), 'N', 'n'),
            f(self.cpsr.zero(), 'Z', 'z'),
            f(self.cpsr.carry(), 'C', 'c'),
            f(self.cpsr.overflow(), 'V', 'v'),
            f(self.cpsr.irq_disabled(), 'I', 'i'),
            f(self.cpsr.fiq_disabled(), 'F', 'f'),
            f(self.cpsr.thumb(), 'T', 't'),
            self.cpsr.mode().name(),
        )
    }

    fn is_halted(&self) -> bool {
        self.halted
    }
}

impl Savable for Arm7Tdmi {
    fn save(&self, w: &mut StateWriter) {
        self.regs.save(w);
        w.write_u32(self.cpsr.bits());
        w.write_bool(self.irq_line);
        w.write_bool(self.fiq_line);
        w.write_bool(self.halted);
        // The boot state is configuration rather than emulated state, but it is saved so that
        // a `reset` after a state load behaves the same as one before it.
        w.write_u32(self.boot.pc);
        w.write_u32(self.boot.mode.bits());
        w.write_bool(self.boot.thumb);
        w.write_u32(self.boot.sp);
        w.write_bool(self.boot.irq_disabled);
        w.write_bool(self.boot.fiq_disabled);
    }

    fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        self.regs.load(r)?;
        self.cpsr = Psr::new(r.read_u32()?);
        self.irq_line = r.read_bool()?;
        self.fiq_line = r.read_bool()?;
        self.halted = r.read_bool()?;
        self.boot.pc = r.read_u32()?;
        self.boot.mode = Mode::from_bits(r.read_u32()?).ok_or_else(|| {
            StateError::Malformed("save state holds an invalid boot mode".to_string())
        })?;
        self.boot.thumb = r.read_bool()?;
        self.boot.sp = r.read_u32()?;
        self.boot.irq_disabled = r.read_bool()?;
        self.boot.fiq_disabled = r.read_bool()?;
        Ok(())
    }
}
