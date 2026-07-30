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

/// Whether an access read or wrote.
///
/// Defined here rather than in `debugger` because the *bus* has to name it, and a system crate may
/// not depend on `debugger`. `debugger` re-exports this, so there is one type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AccessKind {
    Read,
    Write,
}

/// One byte-wide bus access, as it happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Access {
    pub addr: u32,
    pub kind: AccessKind,
    pub value: u8,
}

/// Entries one [`AccessLog`] holds.
///
/// Sized for the widest single instruction in the workspace: an ARM `ldm`/`stm` can move sixteen
/// registers, which is sixty-four bytes, plus its own fetch. 128 leaves room without making the log
/// large enough to matter — it lives inside the bus, so it is paid for in cache footprint whether it
/// is armed or not.
const CAPACITY: usize = 128;

/// A bus's record of the accesses one instruction made.
///
/// # Why the bus records instead of the debugger checking
///
/// Watchpoints are the one thing the session's stepping trick cannot do. Execution breakpoints work
/// by checking the program counter *between* calls to
/// [`step_instruction`](crate::System::step_instruction), so no system crate learns that breakpoints
/// exist. A watchpoint has to see each access, and only the bus does.
///
/// What the bus gets is deliberately as dumb as possible: it records, it does not decide. It has no
/// idea what a watchpoint is, holds no addresses to compare against, and cannot stop execution. The
/// session drains the log after each instruction and asks `debugger`'s registry about each entry, so
/// the policy stays above the systems exactly as it does for execution breakpoints.
///
/// # What it costs when nothing is watching
///
/// One load and one branch per bus access, from [`record`](Self::record) returning immediately while
/// [`is_armed`](Self::is_armed) is false. That is not nothing and it is not claimed to be: prompt 15
/// asks for the claim to be *verified* with prompt 18's profiling rather than asserted, and it has
/// not been. What can be said is that the branch is perfectly predicted — it is false for the entire
/// lifetime of an ordinary session — and that arming happens only when a watchpoint exists.
///
/// # What it does not see
///
/// Accesses that never reach the bus. A PPU fetching tiles reads VRAM directly, and so does DMA on
/// the Game Boy family; a watchpoint on VRAM sees the CPU's writes to it and not the PPU's reads
/// from it. That matches what hardware watchpoints do — they watch the CPU bus — but it is a real
/// limitation and a watchpoint that never fires on a DMA-written address is not broken.
#[derive(Debug, Clone)]
pub struct AccessLog {
    armed: bool,
    entries: Vec<Access>,
    /// Set when an instruction made more accesses than the log can hold.
    ///
    /// Reported rather than ignored: silently dropping accesses would make a watchpoint that
    /// *should* have fired look like one that was never hit, which is the worst failure a debugger
    /// can have.
    overflowed: bool,
}

impl Default for AccessLog {
    fn default() -> Self {
        Self::new()
    }
}

impl AccessLog {
    pub fn new() -> Self {
        Self {
            armed: false,
            entries: Vec::new(),
            overflowed: false,
        }
    }

    /// Start or stop recording. Clears whatever was held.
    pub fn set_armed(&mut self, armed: bool) {
        self.armed = armed;
        self.entries.clear();
        self.entries.shrink_to_fit();
        if armed {
            self.entries.reserve(CAPACITY);
        }
        self.overflowed = false;
    }

    #[inline(always)]
    pub fn is_armed(&self) -> bool {
        self.armed
    }

    /// Record one byte-wide access.
    ///
    /// Byte-wide, always, even for a halfword or word transfer: a watchpoint covers an address
    /// *range*, and recording a word store as one entry at its base address would mean a watchpoint
    /// on the third byte of a structure never fired for the store that overwrote it.
    #[inline(always)]
    pub fn record(&mut self, addr: u32, kind: AccessKind, value: u8) {
        if !self.armed {
            return;
        }
        if self.entries.len() == CAPACITY {
            self.overflowed = true;
            return;
        }
        self.entries.push(Access { addr, kind, value });
    }

    /// Take everything recorded since the last drain.
    pub fn drain(&mut self) -> impl Iterator<Item = Access> + '_ {
        self.overflowed = false;
        self.entries.drain(..)
    }

    /// Whether the last drain lost accesses to the capacity limit.
    pub fn overflowed(&self) -> bool {
        self.overflowed
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

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
