//! [`DebugTarget`]: the one surface a debugger sees into a running machine through.
//!
//! # Why this is not just `CpuIntrospect`
//!
//! [`CpuIntrospect`](crate::CpuIntrospect) describes a CPU. A debugger needs a *machine*: the
//! registers, but also the memory around them, the instruction at the program counter, and the
//! names of the regions an address might be in. Those last three are the system's knowledge, not
//! the core's — only the system knows that `0x8000` is video RAM on a Game Boy and cartridge ROM on
//! a Game Boy Advance, and only the system knows which of its two disassemblers applies at the
//! current PC.
//!
//! # Why the debugger does not get the bus
//!
//! The tempting shape is `System::bus(&mut self) -> &mut dyn Bus`. It is rejected for the same
//! reason [`System`](crate::System) exposes no internals at all: the predecessor implemented save
//! states by reaching into a third-party core's private object graph, and every subsequent bug came
//! from something else having done the same. A handle to the live bus also hands the debugger
//! `read8`, which on a Game Boy has side effects — reading `0xFF44` mid-frame is fine, but reading
//! the joypad register latches, and a memory view that scrolls past MMIO would silently change what
//! the game sees.
//!
//! So the read path is [`peek8`](DebugTarget::peek8), which returns `Option<u8>` and is allowed —
//! required — to answer `None` where a side-effect-free read is not possible. A hex viewer showing
//! `--` for two bytes is correct; a hex viewer that perturbed the machine to avoid showing `--`
//! would be a debugger that changes the bug it is being used to find.
//!
//! # Cost
//!
//! Nothing here is called during emulation. [`System::debug`](crate::System::debug) returns `None`
//! by default, so a system that does not implement any of it pays one null check per debugger
//! request, which happens a few times a second at most.

use crate::{DisasmInstruction, RegisterValue};

/// A named span of a machine's address space, for a memory viewer's jump list.
///
/// Static because these are properties of the hardware, not of a session: a Game Boy's video RAM is
/// at `0x8000` on every Game Boy that will ever exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DebugRegion {
    pub name: &'static str,
    pub start: u32,
    /// Inclusive end, so a region reaching `0xFFFF_FFFF` is expressible.
    pub end: u32,
}

impl DebugRegion {
    pub const fn new(name: &'static str, start: u32, end: u32) -> Self {
        Self { name, start, end }
    }

    pub const fn contains(&self, addr: u32) -> bool {
        addr >= self.start && addr <= self.end
    }

    pub const fn len(&self) -> u64 {
        (self.end as u64 - self.start as u64) + 1
    }

    pub const fn is_empty(&self) -> bool {
        false
    }
}

/// Everything a debugger can learn about a live machine.
///
/// Object-safe and flat on purpose. A supertrait tower would be tidier to write and worse to use:
/// the consumer is an `egui` panel that wants seven facts, and `&mut dyn DebugTarget` is what lets
/// the session hand it those facts without knowing which machine it is talking to.
pub trait DebugTarget {
    /// The register file, in a stable display order.
    fn registers(&self) -> Vec<RegisterValue>;

    /// The *architectural* program counter — the address of the next instruction, not a pipeline
    /// register reading two instructions ahead. Breakpoint comparison and the disassembly
    /// highlight both depend on this being the former.
    fn program_counter(&self) -> u32;

    /// Jump execution elsewhere.
    fn set_program_counter(&mut self, pc: u32);

    /// Condition flags rendered compactly, e.g. `"Z-H-"`. Empty when there is nothing to show.
    fn flags_summary(&self) -> String;

    /// Whether the core is halted waiting for an interrupt, which a debugger must distinguish from
    /// "running but making no visible progress".
    fn is_halted(&self) -> bool;

    /// Read one byte **without side effects**, or `None` where that is not possible.
    ///
    /// `None` is a real answer and must be shown as one. See the module docs.
    fn peek8(&self, addr: u32) -> Option<u8>;

    /// Decode the instruction at `addr`, using whichever disassembler the machine is currently in
    /// the mode for — ARM or Thumb on a GBA, which the system knows and the caller cannot.
    ///
    /// Reads through [`peek8`](Self::peek8), so a disassembly view can never perturb MMIO.
    fn disassemble(&self, addr: u32) -> Option<DisasmInstruction>;

    /// Named regions of the address space, for a jump list.
    fn regions(&self) -> &'static [DebugRegion];

    /// How many hex digits an address of this machine takes: 4 for a Game Boy, 8 for a GBA.
    ///
    /// Presentation, but it belongs here because it is a fact about the hardware. A Game Boy
    /// address printed as `0000C000` is harder to read than `C000` and invites the reader to
    /// wonder what the leading zeroes mean.
    fn address_digits(&self) -> u8;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_region_covers_its_inclusive_end() {
        let vram = DebugRegion::new("VRAM", 0x8000, 0x9FFF);
        assert!(vram.contains(0x8000));
        assert!(vram.contains(0x9FFF), "the end is inside the region");
        assert!(!vram.contains(0xA000));
        assert_eq!(vram.len(), 0x2000);
    }

    #[test]
    fn a_region_reaching_the_top_of_the_address_space_does_not_overflow() {
        // The obvious `end - start + 1` in u32 wraps to zero here, which would report the largest
        // possible region as empty.
        let all = DebugRegion::new("everything", 0, u32::MAX);
        assert_eq!(all.len(), 0x1_0000_0000);
        assert!(all.contains(u32::MAX));
    }

    #[test]
    fn a_single_byte_region_is_one_byte_long() {
        let register = DebugRegion::new("IE", 0xFFFF, 0xFFFF);
        assert_eq!(register.len(), 1);
        assert!(register.contains(0xFFFF));
    }
}
