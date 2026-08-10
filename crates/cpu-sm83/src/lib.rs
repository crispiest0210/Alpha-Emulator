//! Sharp SM83 CPU core — the processor in the Game Boy and Game Boy Color.
//!
//! The SM83 is Z80-*derived* but not a Z80: it drops the shadow register set, the index
//! registers, and the parity flag, and several instructions have subtly different flag
//! behavior. Assuming Z80 parity is the classic way to get a Game Boy core that runs most
//! games but fails accuracy tests, so every flag rule here is written against the documented
//! SM83 behavior rather than inherited from Z80 knowledge.
//!
//! # Cycle units
//!
//! [`Cpu::step`] returns **t-cycles** (4.194304 MHz on DMG), not m-cycles. Every timing in
//! this crate is a multiple of 4.
//!
//! CGB double-speed mode is deliberately *not* implemented here. `KEY1` is a clock multiplier
//! that belongs in `system-gbc`'s scheduler wiring: that system runs this same core unmodified
//! and reinterprets what one t-cycle costs in master-clock terms. Nothing in this crate knows
//! about speed modes, which is what makes that possible.
//!
//! # What this crate does not know
//!
//! It has no idea what a PPU, APU, timer, or cartridge is. Interrupt *sources* reach it only
//! as the memory-mapped [`IF`](IF_ADDR)/[`IE`](IE_ADDR) registers, read through the [`Bus`]
//! like any other address; who set those bits is not this crate's business.

#![deny(unsafe_code)]

mod disasm;
mod exec;

#[cfg(test)]
mod tests;

pub use disasm::Sm83Disassembler;

use core_common::{
    Bus, Cpu, CpuIntrospect, Cycles, RegisterValue, Savable, StateError, StateReader, StateWriter,
};

/// Interrupt Flag register: which interrupts are currently requested.
pub const IF_ADDR: u32 = 0xFF0F;
/// Interrupt Enable register: which interrupts the game is willing to service.
pub const IE_ADDR: u32 = 0xFFFF;

/// The five interrupt sources, in hardware priority order (lowest bit wins).
///
/// The dispatch vectors are 8 bytes apart starting at `0x40`, which is why the vector is
/// computed rather than tabulated.
pub const INTERRUPT_COUNT: u8 = 5;

// ---------------------------------------------------------------------------
// Flags
// ---------------------------------------------------------------------------

/// Zero: the result was zero.
pub const FLAG_Z: u8 = 0b1000_0000;
/// Subtract: the last ALU op was a subtraction. Only `DAA` reads it.
pub const FLAG_N: u8 = 0b0100_0000;
/// Half-carry: carry out of bit 3. Only `DAA` reads it.
pub const FLAG_H: u8 = 0b0010_0000;
/// Carry: carry out of bit 7 (or bit 15 for 16-bit adds).
pub const FLAG_C: u8 = 0b0001_0000;

// ---------------------------------------------------------------------------
// Timing tables
// ---------------------------------------------------------------------------

/// Base t-cycle cost of each unprefixed opcode.
///
/// For conditional branches this is the **not-taken** cost; taken branches add
/// [`CYCLES_BRANCH_TAKEN`]. Zero marks an opcode that does not exist on the SM83
/// (`0xD3`, `0xDB`, `0xDD`, `0xE3`, `0xE4`, `0xEB`–`0xED`, `0xF4`, `0xFC`, `0xFD`); executing
/// one locks up real hardware, and `Sm83::execute` treats it as such.
///
/// Transcribed from the gbdev opcode tables rather than derived by hand — instruction timing
/// is invisible until a test ROM catches it, and hand-deriving 256 entries is how you get the
/// three that are wrong.
#[rustfmt::skip]
pub const CYCLES: [u8; 256] = [
//   x0  x1  x2  x3  x4  x5  x6  x7  x8  x9  xA  xB  xC  xD  xE  xF
     4, 12,  8,  8,  4,  4,  8,  4, 20,  8,  8,  8,  4,  4,  8,  4, // 0x
     4, 12,  8,  8,  4,  4,  8,  4, 12,  8,  8,  8,  4,  4,  8,  4, // 1x
     8, 12,  8,  8,  4,  4,  8,  4,  8,  8,  8,  8,  4,  4,  8,  4, // 2x
     8, 12,  8,  8, 12, 12, 12,  4,  8,  8,  8,  8,  4,  4,  8,  4, // 3x
     4,  4,  4,  4,  4,  4,  8,  4,  4,  4,  4,  4,  4,  4,  8,  4, // 4x
     4,  4,  4,  4,  4,  4,  8,  4,  4,  4,  4,  4,  4,  4,  8,  4, // 5x
     4,  4,  4,  4,  4,  4,  8,  4,  4,  4,  4,  4,  4,  4,  8,  4, // 6x
     8,  8,  8,  8,  8,  8,  4,  8,  4,  4,  4,  4,  4,  4,  8,  4, // 7x  (0x76 = HALT)
     4,  4,  4,  4,  4,  4,  8,  4,  4,  4,  4,  4,  4,  4,  8,  4, // 8x
     4,  4,  4,  4,  4,  4,  8,  4,  4,  4,  4,  4,  4,  4,  8,  4, // 9x
     4,  4,  4,  4,  4,  4,  8,  4,  4,  4,  4,  4,  4,  4,  8,  4, // Ax
     4,  4,  4,  4,  4,  4,  8,  4,  4,  4,  4,  4,  4,  4,  8,  4, // Bx
     8, 12, 12, 16, 12, 16,  8, 16,  8, 16, 12,  4, 12, 24,  8, 16, // Cx
     8, 12, 12,  0, 12, 16,  8, 16,  8, 16, 12,  0, 12,  0,  8, 16, // Dx
    12, 12,  8,  0,  0, 16,  8, 16, 16,  4, 16,  0,  0,  0,  8, 16, // Ex
    12, 12,  8,  4,  0, 16,  8, 16, 12,  8, 16,  4,  0,  0,  8, 16, // Fx
];

/// Extra t-cycles when a conditional branch is taken, added on top of [`CYCLES`].
///
/// `JR cc` costs 8/12, `JP cc` 12/16, `RET cc` 8/20, `CALL cc` 12/24 — the taken path pays
/// for the extra memory accesses it performs.
#[rustfmt::skip]
pub const CYCLES_BRANCH_TAKEN: [u8; 256] = [
//   x0  x1  x2  x3  x4  x5  x6  x7  x8  x9  xA  xB  xC  xD  xE  xF
     0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0, // 0x
     0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0, // 1x
     4,  0,  0,  0,  0,  0,  0,  0,  4,  0,  0,  0,  0,  0,  0,  0, // 2x  JR NZ / JR Z
     4,  0,  0,  0,  0,  0,  0,  0,  4,  0,  0,  0,  0,  0,  0,  0, // 3x  JR NC / JR C
     0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0, // 4x
     0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0, // 5x
     0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0, // 6x
     0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0, // 7x
     0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0, // 8x
     0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0, // 9x
     0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0, // Ax
     0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0, // Bx
    12,  0,  4,  0, 12,  0,  0,  0, 12,  0,  4,  0, 12,  0,  0,  0, // Cx  RET/JP/CALL NZ,Z
    12,  0,  4,  0, 12,  0,  0,  0, 12,  0,  4,  0, 12,  0,  0,  0, // Dx  RET/JP/CALL NC,C
     0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0, // Ex
     0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0,  0, // Fx
];

/// Total t-cycle cost of a `0xCB`-prefixed instruction, prefix fetch included.
///
/// Regular enough to compute: register operands are 8 cycles, `(HL)` operands are 16 because
/// they read *and* write memory — except `BIT b,(HL)`, which only reads, so it is 12.
pub const fn cb_cycles(op: u8) -> u32 {
    if op & 0x07 == 6 {
        if op >= 0x40 && op < 0x80 {
            12 // BIT b,(HL) — read only
        } else {
            16 // read-modify-write
        }
    } else {
        8
    }
}

// ---------------------------------------------------------------------------
// The core
// ---------------------------------------------------------------------------

/// The SM83 register file and execution state.
///
/// Deliberately not generic over the bus: the bus arrives as a parameter to
/// [`Cpu::step`], so one `Sm83` value works with any bus, and `system-gb`/`system-gbc` still
/// get full monomorphization because `Cpu<B>` is generic over `B`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sm83 {
    pub a: u8,
    /// Flag register. The low nibble is always zero on this CPU — writes through `POP AF`
    /// silently drop it, and code does observe that.
    pub f: u8,
    pub b: u8,
    pub c: u8,
    pub d: u8,
    pub e: u8,
    pub h: u8,
    pub l: u8,
    pub sp: u16,
    pub pc: u16,

    /// Interrupt Master Enable.
    pub(crate) ime: bool,
    /// `EI` sets IME immediately but interrupt *dispatch* is inhibited for exactly one
    /// instruction.
    ///
    /// This models the documented "the effect of EI is delayed by one instruction" behavior
    /// in the way that matches hardware on both the cases that distinguish the two plausible
    /// models: `EI; DI` must service nothing (DI runs before any dispatch), and `EI; HALT`
    /// with an interrupt pending must perform a *normal* halt rather than triggering the
    /// HALT bug, because IME really is set by the time HALT executes.
    pub(crate) ime_dispatch_inhibited: bool,

    pub(crate) halted: bool,
    pub(crate) stopped: bool,

    /// The HALT bug is armed: the next opcode fetch reads its byte without advancing PC, so
    /// that byte is fetched twice. See `Sm83::halt`.
    pub(crate) halt_bug: bool,

    /// An undefined opcode was executed. Hardware hangs until reset, and so does this — the
    /// CPU stops fetching and services no interrupts.
    pub(crate) locked: bool,

    /// Cycles already reported to the bus during the instruction in flight.
    ///
    /// Scratch, always zero at an instruction boundary, so it is deliberately not serialized.
    pub(crate) ticked: u32,
}

impl Default for Sm83 {
    fn default() -> Self {
        Self::new()
    }
}

impl Sm83 {
    pub const fn new() -> Self {
        Self {
            a: 0,
            f: 0,
            b: 0,
            c: 0,
            d: 0,
            e: 0,
            h: 0,
            l: 0,
            sp: 0,
            pc: 0,
            ime: false,
            ime_dispatch_inhibited: false,
            halted: false,
            stopped: false,
            halt_bug: false,
            locked: false,
            ticked: 0,
        }
    }

    // -- 16-bit register pairs ------------------------------------------------

    #[inline]
    pub fn af(&self) -> u16 {
        u16::from_be_bytes([self.a, self.f])
    }

    #[inline]
    pub fn set_af(&mut self, v: u16) {
        self.a = (v >> 8) as u8;
        // The low nibble of F does not exist in hardware.
        self.f = (v as u8) & 0xF0;
    }

    #[inline]
    pub fn bc(&self) -> u16 {
        u16::from_be_bytes([self.b, self.c])
    }

    #[inline]
    pub fn set_bc(&mut self, v: u16) {
        self.b = (v >> 8) as u8;
        self.c = v as u8;
    }

    #[inline]
    pub fn de(&self) -> u16 {
        u16::from_be_bytes([self.d, self.e])
    }

    #[inline]
    pub fn set_de(&mut self, v: u16) {
        self.d = (v >> 8) as u8;
        self.e = v as u8;
    }

    #[inline]
    pub fn hl(&self) -> u16 {
        u16::from_be_bytes([self.h, self.l])
    }

    #[inline]
    pub fn set_hl(&mut self, v: u16) {
        self.h = (v >> 8) as u8;
        self.l = v as u8;
    }

    // -- Flags ----------------------------------------------------------------

    #[inline]
    pub fn flag(&self, mask: u8) -> bool {
        self.f & mask != 0
    }

    #[inline]
    pub fn set_flag(&mut self, mask: u8, on: bool) {
        if on {
            self.f |= mask;
        } else {
            self.f &= !mask;
        }
    }

    #[inline]
    pub(crate) fn set_flags(&mut self, z: bool, n: bool, h: bool, c: bool) {
        self.f = (if z { FLAG_Z } else { 0 })
            | (if n { FLAG_N } else { 0 })
            | (if h { FLAG_H } else { 0 })
            | (if c { FLAG_C } else { 0 });
    }

    // -- Execution state ------------------------------------------------------

    #[inline]
    pub fn ime(&self) -> bool {
        self.ime
    }

    /// Whether an undefined opcode has hung the CPU.
    ///
    /// There is deliberately no inherent `is_halted`: it would shadow
    /// [`CpuIntrospect::is_halted`], which reports the broader "not executing" state, and two
    /// same-named methods giving different answers is exactly the kind of trap that produces
    /// a bug nobody can see. Use the trait method for "is it running", and this or
    /// [`Sm83::is_stopped`] when the specific cause matters.
    #[inline]
    pub fn is_locked(&self) -> bool {
        self.locked
    }

    /// Whether `STOP` has put the CPU into low-power mode.
    ///
    /// `system-gbc` polls this to implement the `KEY1` speed switch, which is why the flag is
    /// exposed and clearable rather than handled here: what `STOP` *means* differs between
    /// DMG and CGB, and this crate deliberately knows neither.
    #[inline]
    pub fn is_stopped(&self) -> bool {
        self.stopped
    }

    #[inline]
    pub fn clear_stop(&mut self) {
        self.stopped = false;
    }

    /// Register state as it is when the DMG boot ROM hands control to the cartridge.
    ///
    /// For running without a boot ROM. Systems that execute a real boot ROM must not call
    /// this — they start from [`Cpu::reset`] with `PC = 0` and let the ROM set these up.
    pub fn post_boot_dmg(&mut self) {
        self.set_af(0x01B0);
        self.set_bc(0x0013);
        self.set_de(0x00D8);
        self.set_hl(0x014D);
        self.sp = 0xFFFE;
        self.pc = 0x0100;
        self.ime = false;
        self.ime_dispatch_inhibited = false;
        self.halted = false;
        self.stopped = false;
        self.halt_bug = false;
        self.locked = false;
    }

    /// Register state as it is when the CGB boot ROM hands control to the cartridge, running
    /// a CGB-aware cartridge.
    pub fn post_boot_cgb(&mut self) {
        self.set_af(0x1180);
        self.set_bc(0x0000);
        self.set_de(0xFF56);
        self.set_hl(0x000D);
        self.sp = 0xFFFE;
        self.pc = 0x0100;
        self.ime = false;
        self.ime_dispatch_inhibited = false;
        self.halted = false;
        self.stopped = false;
        self.halt_bug = false;
        self.locked = false;
    }

    // -- Memory helpers -------------------------------------------------------

    /// Perform one machine cycle's read.
    ///
    /// The bus is told the time first and the access happens second, because on hardware the
    /// data is latched at the *end* of the machine cycle. A timer that ticks over during this
    /// cycle is therefore already updated when the read lands, which is exactly the behaviour
    /// `mem_timing` measures.
    #[inline]
    pub(crate) fn bus_read<B: Bus + ?Sized>(&mut self, bus: &mut B, addr: u32) -> u8 {
        bus.tick(Cycles(4));
        self.ticked += 4;
        bus.read8(addr)
    }

    #[inline]
    pub(crate) fn bus_write<B: Bus + ?Sized>(&mut self, bus: &mut B, addr: u32, value: u8) {
        bus.tick(Cycles(4));
        self.ticked += 4;
        bus.write8(addr, value);
    }

    /// Report the cycles this instruction spent on internal work rather than on the bus.
    ///
    /// Called once the instruction's total is known, so the sum reaching the bus always
    /// matches what `step` returns no matter how the instruction split its time.
    #[inline]
    fn tick_remaining<B: Bus + ?Sized>(&mut self, bus: &mut B, total: u32) {
        if total > self.ticked {
            bus.tick(Cycles((total - self.ticked) as u64));
        }
        // Settling the instruction also clears the counter, so the "always zero at an
        // instruction boundary" invariant is enforced here rather than merely asserted in a
        // comment — which matters because the field participates in equality and save states.
        self.ticked = 0;
    }

    /// Fetch the next opcode.
    ///
    /// This is the one place the HALT bug is observable: when armed, the byte is read but PC
    /// is left alone, so the very next fetch reads the same byte again.
    #[inline]
    pub(crate) fn fetch_opcode<B: Bus + ?Sized>(&mut self, bus: &mut B) -> u8 {
        let op = self.bus_read(bus, self.pc as u32);
        if self.halt_bug {
            self.halt_bug = false;
        } else {
            self.pc = self.pc.wrapping_add(1);
        }
        op
    }

    #[inline]
    pub(crate) fn fetch8<B: Bus + ?Sized>(&mut self, bus: &mut B) -> u8 {
        let v = self.bus_read(bus, self.pc as u32);
        self.pc = self.pc.wrapping_add(1);
        v
    }

    #[inline]
    pub(crate) fn fetch16<B: Bus + ?Sized>(&mut self, bus: &mut B) -> u16 {
        let lo = self.fetch8(bus);
        let hi = self.fetch8(bus);
        u16::from_le_bytes([lo, hi])
    }

    #[inline]
    pub(crate) fn push16<B: Bus + ?Sized>(&mut self, bus: &mut B, value: u16) {
        let [lo, hi] = value.to_le_bytes();
        self.sp = self.sp.wrapping_sub(1);
        self.bus_write(bus, self.sp as u32, hi);
        self.sp = self.sp.wrapping_sub(1);
        self.bus_write(bus, self.sp as u32, lo);
    }

    #[inline]
    pub(crate) fn pop16<B: Bus + ?Sized>(&mut self, bus: &mut B) -> u16 {
        let lo = self.bus_read(bus, self.sp as u32);
        self.sp = self.sp.wrapping_add(1);
        let hi = self.bus_read(bus, self.sp as u32);
        self.sp = self.sp.wrapping_add(1);
        u16::from_le_bytes([lo, hi])
    }

    // -- Interrupts -----------------------------------------------------------

    /// Interrupts that are both requested and enabled.
    #[inline]
    fn pending_interrupts<B: Bus + ?Sized>(&self, bus: &mut B) -> u8 {
        // Deliberately not through `bus_read`: sampling the interrupt lines is internal to the
        // CPU and does not occupy a machine cycle on the external bus.
        bus.read8(IF_ADDR) & bus.read8(IE_ADDR) & 0x1F
    }

    /// Push PC and jump to the highest-priority pending interrupt's vector.
    ///
    /// Costs 20 t-cycles: two internal cycles, two for pushing PC, one for loading the
    /// vector. Priority runs lowest-bit-first — VBlank, LCD STAT, Timer, Serial, Joypad.
    fn dispatch_interrupt<B: Bus + ?Sized>(&mut self, bus: &mut B, pending: u8) -> u32 {
        let index = pending.trailing_zeros() as u8;
        debug_assert!(index < INTERRUPT_COUNT);

        self.ime = false;
        // Acknowledge by clearing just this source's request bit, preserving the others.
        let iflag = bus.read8(IF_ADDR);
        bus.write8(IF_ADDR, iflag & !(1 << index));

        let pc = self.pc;
        self.push16(bus, pc);
        self.pc = 0x0040 + (index as u16 * 8);
        20
    }
}

impl<B: Bus + ?Sized> Cpu<B> for Sm83 {
    fn step(&mut self, bus: &mut B) -> Cycles {
        if self.locked {
            // An undefined opcode hung the CPU. It fetches nothing and services no
            // interrupts; only a reset recovers.
            bus.tick(Cycles(4));
            return Cycles(4);
        }

        // Consume the one-instruction dispatch inhibition left by `EI`, whether or not
        // anything is pending — it expires with the next instruction either way.
        let dispatch_inhibited = std::mem::take(&mut self.ime_dispatch_inhibited);

        // Reading IF/IE costs two bus accesses, so skip it entirely when no interrupt could
        // possibly be acted upon. With IME clear and the CPU running, nothing can.
        let pending = if self.ime || self.halted || self.stopped {
            self.pending_interrupts(bus)
        } else {
            0
        };

        if pending != 0 {
            // A pending interrupt wakes the CPU regardless of IME. With IME clear it simply
            // resumes at the instruction after HALT without servicing anything.
            self.halted = false;
            self.stopped = false;

            if self.ime && !dispatch_inhibited {
                self.ticked = 0;
                let total = self.dispatch_interrupt(bus, pending);
                self.tick_remaining(bus, total);
                return Cycles(total as u64);
            }
        }

        if self.halted || self.stopped {
            // Idling still consumes time. Returning zero here would spin `step_frame`
            // forever, which is exactly what the `Cpu` trait's contract warns about.
            bus.tick(Cycles(4));
            return Cycles(4);
        }

        self.ticked = 0;
        let opcode = self.fetch_opcode(bus);
        let total = self.execute(opcode, bus);
        self.tick_remaining(bus, total);
        Cycles(total as u64)
    }

    fn reset(&mut self) {
        *self = Sm83::new();
    }
}

impl CpuIntrospect for Sm83 {
    fn registers(&self) -> Vec<RegisterValue> {
        vec![
            RegisterValue::new("A", self.a as u64, 8),
            RegisterValue::new("F", self.f as u64, 8),
            RegisterValue::new("B", self.b as u64, 8),
            RegisterValue::new("C", self.c as u64, 8),
            RegisterValue::new("D", self.d as u64, 8),
            RegisterValue::new("E", self.e as u64, 8),
            RegisterValue::new("H", self.h as u64, 8),
            RegisterValue::new("L", self.l as u64, 8),
            RegisterValue::new("AF", self.af() as u64, 16),
            RegisterValue::new("BC", self.bc() as u64, 16),
            RegisterValue::new("DE", self.de() as u64, 16),
            RegisterValue::new("HL", self.hl() as u64, 16),
            RegisterValue::new("SP", self.sp as u64, 16),
            RegisterValue::new("PC", self.pc as u64, 16),
        ]
    }

    fn program_counter(&self) -> u32 {
        self.pc as u32
    }

    fn set_program_counter(&mut self, pc: u32) {
        self.pc = pc as u16;
    }

    /// Set flags render uppercase, clear flags lowercase — compact enough for a status line
    /// and unambiguous at a glance.
    fn flags_summary(&self) -> String {
        let f = |mask: u8, set: char, clear: char| if self.flag(mask) { set } else { clear };
        format!(
            "{}{}{}{}",
            f(FLAG_Z, 'Z', 'z'),
            f(FLAG_N, 'N', 'n'),
            f(FLAG_H, 'H', 'h'),
            f(FLAG_C, 'C', 'c')
        )
    }

    fn is_halted(&self) -> bool {
        self.halted || self.stopped || self.locked
    }
}

impl Savable for Sm83 {
    fn save(&self, w: &mut StateWriter) {
        w.write_u8(self.a);
        w.write_u8(self.f);
        w.write_u8(self.b);
        w.write_u8(self.c);
        w.write_u8(self.d);
        w.write_u8(self.e);
        w.write_u8(self.h);
        w.write_u8(self.l);
        w.write_u16(self.sp);
        w.write_u16(self.pc);
        w.write_bool(self.ime);
        w.write_bool(self.ime_dispatch_inhibited);
        w.write_bool(self.halted);
        w.write_bool(self.stopped);
        w.write_bool(self.halt_bug);
        w.write_bool(self.locked);
    }

    fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        self.a = r.read_u8()?;
        // Masked on load as well as on write: a hand-edited or corrupt state must not be able
        // to introduce a low-nibble F value that cannot exist in hardware.
        self.f = r.read_u8()? & 0xF0;
        self.b = r.read_u8()?;
        self.c = r.read_u8()?;
        self.d = r.read_u8()?;
        self.e = r.read_u8()?;
        self.h = r.read_u8()?;
        self.l = r.read_u8()?;
        self.sp = r.read_u16()?;
        self.pc = r.read_u16()?;
        self.ime = r.read_bool()?;
        self.ime_dispatch_inhibited = r.read_bool()?;
        self.halted = r.read_bool()?;
        self.stopped = r.read_bool()?;
        self.halt_bug = r.read_bool()?;
        self.locked = r.read_bool()?;
        Ok(())
    }
}
