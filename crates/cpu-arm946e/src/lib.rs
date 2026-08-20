//! ARM946E-S CPU core — the Nintendo DS's ARM9.
//!
//! # Composition, not a fork
//!
//! ARMv5TE is a superset of ARMv4T, and the overlap is the overwhelming majority of the
//! instruction set. So [`Arm946e`] *contains* an [`Arm7Tdmi`] and delegates to it: the ARM9
//! tries its own ARMv5TE encodings first and falls through to the shared implementation for
//! everything else. The register file, CPSR, exception model, and every ARMv4T instruction
//! exist exactly once, in `cpu-arm7tdmi`.
//!
//! This is the reason `cpu-arm7tdmi` was built as a standalone crate rather than folded into
//! a GBA-only one, and it is why the ARMv5TE delta in `v5.rs` is a few hundred lines instead
//! of a second complete interpreter.
//!
//! # What this core adds
//!
//! - The ARMv5TE instruction delta (`v5.rs`).
//! - CP15, the system control coprocessor (`cp15.rs`) — a *protection unit*, not an MMU.
//! - Tightly-coupled memory (`tcm.rs`), which sits between the core and the bus.
//! - High exception vectors at `0xFFFF_0000`, which is where the DS runs them.
//! - Interworking on a load into `R15` — see [`Arm7Tdmi::interworking_loads`]. Not an instruction
//!   of its own, which is exactly why it is easy to miss: it changes what three encodings the
//!   shared core already implements *mean*, and a compiler leans on all three constantly.
//!
//! # Caches
//!
//! Cache *control* is implemented; cache *storage* is not. Every access goes straight to
//! memory, so the cache and memory can never disagree and there is nothing for an invalidate
//! or clean to reconcile — software that flushes before a DMA still sees correct data. This
//! is the deliberate "functional correctness now, cycle-exact cache timing later" line; cache
//! timing is a known deep rabbit hole and blocking DS bring-up on it would be the wrong
//! trade. Consequently cache contents are not serialized, because there are none.
//!
//! # Scope
//!
//! Like `cpu-arm7tdmi`, this crate knows nothing about the system around it: no 3D core, no
//! IPC hardware, no PPU. It is the CPU, full stop.

#![deny(unsafe_code)]

mod cp15;
mod tcm;
mod v5;

#[cfg(test)]
mod tests;

pub use cp15::{control, Cp15, Cp15Effect, CACHE_TYPE, MAIN_ID};
pub use tcm::Tcm;

pub use cpu_arm7tdmi::{Arm7Tdmi, BootState, Exception, Mode, Psr, RegisterFile};

use core_common::{
    Bus, Cpu, CpuIntrospect, Cycles, RegisterValue, Savable, StateError, StateReader, StateWriter,
};

/// Physical size of the DS's instruction TCM.
pub const ITCM_SIZE: usize = 32 * 1024;
/// Physical size of the DS's data TCM.
pub const DTCM_SIZE: usize = 16 * 1024;

/// The ARM946E-S core.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Arm946e {
    /// The shared ARMv4T core: register file, CPSR, exceptions, and every instruction the two
    /// architectures have in common.
    pub core: Arm7Tdmi,
    pub cp15: Cp15,
    pub itcm: Tcm,
    pub dtcm: Tcm,
}

impl Default for Arm946e {
    fn default() -> Self {
        Self::new(BootState::default())
    }
}

impl Arm946e {
    pub fn new(boot: BootState) -> Self {
        let mut core = Arm7Tdmi::new(boot);
        // ARMv5 interworks on a load into R15. See `Arm7Tdmi::interworking_loads` — it is the
        // difference between `pop {r4, pc}` returning to an ARM caller and returning into the same
        // instruction stream decoded as THUMB.
        core.interworking_loads = true;
        Self {
            core,
            cp15: Cp15::new(),
            itcm: Tcm::new(ITCM_SIZE),
            dtcm: Tcm::new(DTCM_SIZE),
        }
    }

    /// Configure CP15 and the TCMs the way the DS firmware leaves them, for booting a ROM
    /// without running the firmware first.
    pub fn post_boot_nds(&mut self) {
        let (dtcm, itcm) = self.cp15.post_boot_nds();
        self.dtcm.configure(dtcm, false);
        self.itcm.configure(itcm, true);
        self.apply_control_register();
    }

    /// Push the control register's TCM-enable and vector-base bits into the places that
    /// actually act on them.
    ///
    /// CP15 deliberately does not reach into the TCMs or the core itself, so this is the one
    /// function that translates a control-register change into effects.
    pub(crate) fn apply_control_register(&mut self) {
        self.itcm.set_enabled(self.cp15.has(control::ITCM_ENABLE));
        self.itcm
            .set_load_mode(self.cp15.has(control::ITCM_LOAD_MODE));
        self.dtcm.set_enabled(self.cp15.has(control::DTCM_ENABLE));
        self.dtcm
            .set_load_mode(self.cp15.has(control::DTCM_LOAD_MODE));
        self.core.set_exception_base(self.cp15.exception_base());
    }

    #[inline]
    pub fn set_irq_line(&mut self, asserted: bool) {
        self.core.set_irq_line(asserted);
    }

    #[inline]
    pub fn set_fiq_line(&mut self, asserted: bool) {
        self.core.set_fiq_line(asserted);
    }

    #[inline]
    pub fn is_halted(&self) -> bool {
        self.core.is_halted()
    }

    #[inline]
    pub fn is_thumb(&self) -> bool {
        self.core.is_thumb()
    }

    #[inline]
    pub fn reg(&self, index: usize) -> u32 {
        self.core.reg(index)
    }

    #[inline]
    pub fn set_reg(&mut self, index: usize, value: u32) {
        self.core.set_reg(index, value);
    }
}

/// A view of the bus with the TCMs spliced in front of it.
///
/// This is where TCM actually takes effect, and modelling it as a bus wrapper rather than as
/// special cases inside the CPU mirrors the hardware: TCM physically sits between the core
/// and the bus, so *every* access the core makes — instruction fetch included — passes
/// through it without the CPU needing to know.
///
/// One known simplification: DTCM is data-only on hardware, but this view cannot distinguish
/// an instruction fetch from a data access, so it will also serve fetches from DTCM. No DS
/// software executes from DTCM, so this is unobservable in practice.
struct TcmBus<'a, B: Bus + ?Sized> {
    inner: &'a mut B,
    itcm: &'a mut Tcm,
    dtcm: &'a mut Tcm,
}

impl<B: Bus + ?Sized> TcmBus<'_, B> {
    #[inline]
    fn responds(&self, addr: u32) -> bool {
        self.itcm.responds_to_read(addr) || self.dtcm.responds_to_read(addr)
    }

    #[inline]
    fn responds_write(&self, addr: u32) -> bool {
        self.itcm.responds_to_write(addr) || self.dtcm.responds_to_write(addr)
    }
}

impl<B: Bus + ?Sized> Bus for TcmBus<'_, B> {
    #[inline]
    fn read8(&mut self, addr: u32) -> u8 {
        if self.itcm.responds_to_read(addr) {
            self.itcm.read8(addr)
        } else if self.dtcm.responds_to_read(addr) {
            self.dtcm.read8(addr)
        } else {
            self.inner.read8(addr)
        }
    }

    #[inline]
    fn write8(&mut self, addr: u32, value: u8) {
        if self.itcm.responds_to_write(addr) {
            self.itcm.write8(addr, value);
        } else if self.dtcm.responds_to_write(addr) {
            self.dtcm.write8(addr, value);
        } else {
            self.inner.write8(addr, value);
        }
    }

    // The wide accessors must be forwarded, not left to the default byte composition.
    //
    // `Bus`'s defaults turn a halfword or word access into two or four byte accesses. That is
    // correct for a bus whose byte and wide behaviour agree, and wrong for the DS: an ARM9 byte
    // write to VRAM, palette RAM, or OAM is *dropped* by hardware, and several I/O registers —
    // the IPC send FIFO among them — exist only as words. Decomposing here meant the ARM9 could
    // not write to VRAM at all, which presented as a black screen with every register set
    // correctly.
    //
    // A TCM that answers the first byte answers all of them, since the TCM regions are far
    // larger than four bytes and aligned, so the composition below stays inside one memory.
    #[inline]
    fn read16(&mut self, addr: u32) -> u16 {
        if self.responds(addr) {
            u16::from_le_bytes([self.read8(addr), self.read8(addr.wrapping_add(1))])
        } else {
            self.inner.read16(addr)
        }
    }

    #[inline]
    fn read32(&mut self, addr: u32) -> u32 {
        if self.responds(addr) {
            u32::from_le_bytes([
                self.read8(addr),
                self.read8(addr.wrapping_add(1)),
                self.read8(addr.wrapping_add(2)),
                self.read8(addr.wrapping_add(3)),
            ])
        } else {
            self.inner.read32(addr)
        }
    }

    #[inline]
    fn write16(&mut self, addr: u32, value: u16) {
        if self.responds_write(addr) {
            let b = value.to_le_bytes();
            self.write8(addr, b[0]);
            self.write8(addr.wrapping_add(1), b[1]);
        } else {
            self.inner.write16(addr, value);
        }
    }

    #[inline]
    fn write32(&mut self, addr: u32, value: u32) {
        if self.responds_write(addr) {
            let b = value.to_le_bytes();
            self.write8(addr, b[0]);
            self.write8(addr.wrapping_add(1), b[1]);
            self.write8(addr.wrapping_add(2), b[2]);
            self.write8(addr.wrapping_add(3), b[3]);
        } else {
            self.inner.write32(addr, value);
        }
    }

    #[inline]
    fn open_bus8(&self, addr: u32) -> u8 {
        self.inner.open_bus8(addr)
    }

    #[inline]
    fn peek8(&self, addr: u32) -> Option<u8> {
        if self.itcm.responds_to_read(addr) {
            Some(self.itcm.read8(addr))
        } else if self.dtcm.responds_to_read(addr) {
            Some(self.dtcm.read8(addr))
        } else {
            self.inner.peek8(addr)
        }
    }
}

/// A transient view, never serialized: the TCM contents and the underlying bus are each saved
/// by their real owners.
impl<B: Bus + ?Sized> Savable for TcmBus<'_, B> {
    fn save(&self, _w: &mut StateWriter) {}
    fn load(&mut self, _r: &mut StateReader) -> Result<(), StateError> {
        Ok(())
    }
}

impl Arm946e {
    /// Run `f` with the TCMs spliced in front of `bus`, for callers outside this crate.
    ///
    /// The wrapper type is deliberately private — it is an implementation detail of how this core
    /// reaches memory — so the closure receives it as a `&mut dyn Bus`. The dynamic dispatch is
    /// paid once per call rather than once per access on the hot instruction path, which is why
    /// this is not how [`Arm946e::step`] reaches memory.
    ///
    /// The caller that needs this is the DS's BIOS HLE. A `SWI` performed in place of the ROM
    /// must see memory the way the core does: `IntrWait`'s flag word lives at `DTCM + 0x3FF8`,
    /// and a `CpuSet` moving a libnds program's data is usually moving it to or from DTCM. Going
    /// straight to the bus finds main RAM at those addresses instead, which reads as zero and
    /// writes into nothing.
    pub fn with_bus<B: Bus + ?Sized, R>(
        &mut self,
        bus: &mut B,
        f: impl FnOnce(&mut Arm7Tdmi, &mut dyn Bus) -> R,
    ) -> R {
        self.with_tcm_bus(bus, |core, view| f(core, view))
    }

    /// Run `f` with the TCMs spliced in front of `bus`.
    ///
    /// The fields are borrowed separately so the shared ARMv4T core can be driven while the
    /// wrapper holds mutable references to the memory it will access. `cp15` stays untouched
    /// and therefore reachable outside the closure, which is what lets `cp15_transfer` run
    /// without a second borrow.
    fn with_tcm_bus<B: Bus + ?Sized, R>(
        &mut self,
        bus: &mut B,
        f: impl FnOnce(&mut Arm7Tdmi, &mut TcmBus<'_, B>) -> R,
    ) -> R {
        let Arm946e {
            core, itcm, dtcm, ..
        } = self;
        let mut view = TcmBus {
            inner: bus,
            itcm,
            dtcm,
        };
        f(core, &mut view)
    }

    fn step_thumb<B: Bus + ?Sized>(&mut self, bus: &mut B) -> u32 {
        let addr = self.core.regs.pc() & !1;
        let instr = self.with_tcm_bus(bus, |core, view| {
            let instr = view.read16(addr);
            core.regs.set_pc(addr.wrapping_add(2));
            instr
        });

        if let Some(cycles) = self.execute_thumb_v5(instr) {
            return cycles;
        }
        self.with_tcm_bus(bus, |core, view| core.execute_thumb(instr, view))
    }

    /// The two THUMB encodings ARMv5 adds, both of which ARMv4T rejects as undefined.
    fn execute_thumb_v5(&mut self, instr: u16) -> Option<u32> {
        // BLX suffix: the low half of a long branch that lands in ARM state. Distinguished
        // from the ARMv4T BL suffix by the 0b11101 prefix rather than 0b11111.
        if instr & 0xF800 == 0xE800 {
            let return_address = self.core.regs.pc();
            let offset = (instr & 0x07FF) as u32;
            // The target is word-aligned: a BLX from THUMB always enters ARM state, and ARM
            // instructions cannot sit on a halfword boundary.
            let target = self.core.reg(14).wrapping_add(offset << 1) & !3;
            self.core.set_reg(14, return_address | 1);
            self.core.cpsr.set_thumb(false);
            self.core.regs.set_pc(target);
            return Some(3);
        }

        // BKPT
        if instr & 0xFF00 == 0xBE00 {
            self.core.raise_prefetch_abort();
            return Some(3);
        }

        None
    }
}

impl<B: Bus + ?Sized> Cpu<B> for Arm946e {
    fn step(&mut self, bus: &mut B) -> Cycles {
        // The interrupt and halt preamble mirrors the shared core's, reimplemented here
        // rather than delegated because ARM-state execution below has to intercept the fetch
        // for ARMv5TE encodings, and delegating would fetch twice.
        if self.core.fiq_line() && !self.core.cpsr.fiq_disabled() {
            self.core.set_halted(false);
            let lr = self.core.regs.pc().wrapping_add(4);
            self.core.enter_exception(Exception::Fiq, lr);
            return Cycles(3);
        }
        if self.core.irq_line() && !self.core.cpsr.irq_disabled() {
            self.core.set_halted(false);
            let lr = self.core.regs.pc().wrapping_add(4);
            self.core.enter_exception(Exception::Irq, lr);
            return Cycles(3);
        }
        if self.core.is_halted() {
            if self.core.irq_line() || self.core.fiq_line() {
                self.core.set_halted(false);
            } else {
                return Cycles(1);
            }
        }

        if self.core.is_thumb() {
            return Cycles(self.step_thumb(bus) as u64);
        }

        let addr = self.core.regs.pc() & !3;
        let instr = self.with_tcm_bus(bus, |core, view| {
            let instr = view.read32(addr);
            core.regs.set_pc(addr.wrapping_add(4));
            instr
        });

        // ARMv5 reuses the ARMv4T "never" condition encoding for genuinely unconditional
        // instructions, so this is tested before the condition check, not after it.
        if instr >> 28 == 0xF {
            return Cycles(self.execute_unconditional(instr, bus) as u64);
        }
        if !self.core.cpsr.passes_condition(instr >> 28) {
            return Cycles(1);
        }
        if let Some(cycles) = self.execute_armv5(instr, bus) {
            return Cycles(cycles as u64);
        }

        // Everything the two architectures share.
        Cycles(self.with_tcm_bus(bus, |core, view| core.execute_arm(instr, view)) as u64)
    }

    fn reset(&mut self) {
        Cpu::<B>::reset(&mut self.core);
        self.cp15 = Cp15::new();
        self.itcm = Tcm::new(ITCM_SIZE);
        self.dtcm = Tcm::new(DTCM_SIZE);
        self.apply_control_register();
    }
}

impl CpuIntrospect for Arm946e {
    fn registers(&self) -> Vec<RegisterValue> {
        let mut out = self.core.registers();
        out.push(RegisterValue::new(
            "cp15_ctl",
            self.cp15.control() as u64,
            32,
        ));
        out.push(RegisterValue::new(
            "itcm",
            self.cp15.itcm_config() as u64,
            32,
        ));
        out.push(RegisterValue::new(
            "dtcm",
            self.cp15.dtcm_config() as u64,
            32,
        ));
        out
    }

    fn program_counter(&self) -> u32 {
        self.core.program_counter()
    }

    fn set_program_counter(&mut self, pc: u32) {
        self.core.set_program_counter(pc);
    }

    fn flags_summary(&self) -> String {
        let base = self.core.flags_summary();
        if self.core.cpsr.sticky_overflow() {
            format!("{base} Q")
        } else {
            base
        }
    }

    fn is_halted(&self) -> bool {
        self.core.is_halted()
    }
}

impl Savable for Arm946e {
    fn save(&self, w: &mut StateWriter) {
        self.core.save(w);
        self.cp15.save(w);
        // TCM contents are saved because, unlike a cache, they hold data that exists nowhere
        // else in the machine.
        self.itcm.save(w);
        self.dtcm.save(w);
    }

    fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        self.core.load(r)?;
        self.cp15.load(r)?;
        self.itcm.load(r)?;
        self.dtcm.load(r)?;
        Ok(())
    }
}
