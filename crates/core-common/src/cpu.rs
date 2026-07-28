//! The CPU abstraction: [`Cpu`] for the hot path, [`CpuIntrospect`] for the debugger.
//!
//! # Why `Cpu` is generic over the bus
//!
//! `fn step(&mut self, bus: &mut B)` with `B` a type parameter on the trait, rather than
//! `&mut dyn Bus`, so that `Sm83<GbBus>` monomorphizes: a memory access from an instruction
//! becomes a direct call the optimizer can inline, not a vtable dispatch. Memory access is
//! *the* hot path in an interpreter — a GBA runs on the order of 16 million bus cycles per
//! second — and paying dynamic dispatch there is the difference between full speed and not.
//!
//! Dynamic dispatch is fine, and used, where cost is irrelevant: scheduler event handling
//! and the introspection traits below.

use crate::{Bus, Cycles};
use savestate::Savable;
use std::fmt;

/// A CPU core that executes instructions against a bus.
///
/// # Contract for implementers
///
/// - [`step`](Cpu::step) executes **exactly one instruction** (or services one pending
///   exception/interrupt, or burns one cycle in a halted state) and returns how many cycles
///   that took, in the system's base clock units.
/// - The returned count must be the *real* cost including memory wait states, since the
///   scheduler uses it as the machine's clock. Returning a nominal instruction timing and
///   accounting for wait states elsewhere will desynchronize everything downstream.
/// - `step` must always make progress: returning `Cycles::ZERO` forever would hang the
///   system's frame loop. A halted CPU returns the cost of idling, not zero.
/// - Interrupt *delivery* is the CPU's business; interrupt *sources* are the system's. The
///   usual arrangement is that the bus owns the interrupt-enable/flag registers and the CPU
///   consults them at instruction boundaries.
pub trait Cpu<B: Bus + ?Sized>: Savable {
    /// Execute one instruction, returning the cycles consumed.
    fn step(&mut self, bus: &mut B) -> Cycles;

    /// Return to power-on state. Does not touch the bus: resetting memory is the system's
    /// job, and conflating the two is how you get a reset that half-clears RAM.
    fn reset(&mut self);
}

/// One named CPU register, as the debugger wants to display it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisterValue {
    pub name: &'static str,
    pub value: u64,
    /// Display width. 8 for the SM83's `A`, 16 for its `HL`, 32 for an ARM `r0`.
    pub width_bits: u8,
}

impl RegisterValue {
    pub const fn new(name: &'static str, value: u64, width_bits: u8) -> Self {
        Self {
            name,
            value,
            width_bits,
        }
    }
}

impl fmt::Display for RegisterValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let nibbles = (self.width_bits as usize).div_ceil(4);
        write!(f, "{}={:0>width$X}", self.name, self.value, width = nibbles)
    }
}

/// Debugger-facing inspection, kept off [`Cpu`] so the hot trait stays minimal.
///
/// Everything here is `&self` or cheap and allocation-tolerant; none of it runs during normal
/// emulation.
pub trait CpuIntrospect {
    /// The full register file, in a stable display order.
    fn registers(&self) -> Vec<RegisterValue>;

    /// Where the CPU will fetch its next instruction.
    ///
    /// Cores with a fetch pipeline must return the *architectural* PC — the address of the
    /// next instruction to execute — not the raw pipeline register, which on ARM reads ahead
    /// by two instructions. Breakpoint comparison depends on this.
    fn program_counter(&self) -> u32;

    /// Force execution to continue elsewhere. Used by the debugger's "set next statement"
    /// and by test harnesses that need to jump into a specific routine.
    fn set_program_counter(&mut self, pc: u32);

    /// Condition flags rendered compactly, e.g. `"Z-H-"` or `"nzCv"`. Empty when the core has
    /// nothing worth showing.
    fn flags_summary(&self) -> String {
        String::new()
    }

    /// Whether the core is halted/stopped and waiting for an interrupt, which a debugger
    /// needs to distinguish from "running but making no visible progress".
    fn is_halted(&self) -> bool {
        false
    }
}

/// One decoded instruction, as text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisasmInstruction {
    /// Rendered assembly, e.g. `"LD A,(HL+)"` or `"ldr r0, [r1, #4]"`.
    pub text: String,
    /// Encoded length in bytes, so a disassembler can walk forward without re-decoding.
    pub length: u8,
}

/// Decode one instruction from raw bytes.
///
/// Deliberately takes a byte slice rather than a [`Bus`]: disassembly must be usable on a
/// captured memory snapshot, on a ROM file that is not loaded into any machine, and inside
/// snapshot tests — none of which have a live bus. It also guarantees the debugger can never
/// perturb MMIO state just by scrolling a disassembly view.
pub trait Disassemble {
    /// Decode the instruction starting at `bytes[0]`, which lives at address `addr`.
    ///
    /// `addr` matters because PC-relative operands must render as absolute targets. Returns
    /// `None` when `bytes` is too short to hold a complete instruction; unknown encodings
    /// still return `Some` with text marking them undefined, since a disassembler that stops
    /// at the first bad byte is useless for exactly the cases you need it for.
    fn disassemble(&self, bytes: &[u8], addr: u32) -> Option<DisasmInstruction>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_values_render_at_their_natural_width() {
        assert_eq!(RegisterValue::new("A", 0x0F, 8).to_string(), "A=0F");
        assert_eq!(RegisterValue::new("HL", 0x1234, 16).to_string(), "HL=1234");
        assert_eq!(
            RegisterValue::new("r0", 0xDEAD_BEEF, 32).to_string(),
            "r0=DEADBEEF"
        );
    }
}
