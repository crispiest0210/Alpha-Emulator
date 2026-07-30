//! The two interrupt controllers: `IE`, `IF`, and `IME`, one set per core.
//!
//! Structurally this is prompt 12's GBA controller with three differences, each of which matters.
//!
//! # There are two of them, and they do not have the same sources
//!
//! The ARM9 and the ARM7 each have a complete `IE`/`IF`/`IME` set at the same addresses in their
//! own I/O space. The low fourteen bits mean the same thing on both — the video, timer, DMA, and
//! keypad sources — and above that they diverge: only the ARM9 has the geometry-FIFO interrupt,
//! and only the ARM7 has the SPI, wifi, and lid interrupts. [`sources::valid`] is the mask, and it
//! is applied on `raise` so a source wired to the wrong core is dropped loudly in a test rather
//! than quietly setting a bit nothing will ever service.
//!
//! # The registers are 32 bits, and not where the GBA's are
//!
//! `IE` moved from `0x0400_0200` to `0x0400_0210` and grew to a word. Code carried over from the
//! GBA that keeps the old address compiles fine and produces a machine where no interrupt is ever
//! enabled.
//!
//! # `IF` is still acknowledged by writing ones
//!
//! Writing a 1 bit clears it; writing 0 leaves it. This is the same trap as on the GBA and is
//! still the most common way to end up in an interrupt loop that never terminates.

use crate::Core;
use core_common::{Savable, StateError, StateReader, StateWriter};

/// Register addresses, identical in both cores' I/O space.
pub mod reg {
    pub const IME: u32 = 0x0400_0208;
    pub const IE: u32 = 0x0400_0210;
    pub const IF: u32 = 0x0400_0214;
}

/// Where each core takes an interrupt.
///
/// The ARM9 runs with high vectors — CP15 puts its exception base at `0xFFFF_0000` — so the two
/// are 4 GiB apart despite being the same architectural offset.
pub const ARM9_IRQ_VECTOR: u32 = 0xFFFF_0018;
pub const ARM7_IRQ_VECTOR: u32 = 0x0000_0018;

/// Interrupt source bits, shared by `IE` and `IF`.
pub mod sources {
    use crate::Core;

    pub const VBLANK: u32 = 1 << 0;
    pub const HBLANK: u32 = 1 << 1;
    pub const VCOUNT: u32 = 1 << 2;
    pub const TIMER0: u32 = 1 << 3;
    pub const TIMER1: u32 = 1 << 4;
    pub const TIMER2: u32 = 1 << 5;
    pub const TIMER3: u32 = 1 << 6;
    /// Serial / `RCNT`. ARM7 only.
    pub const SERIAL: u32 = 1 << 7;
    pub const DMA0: u32 = 1 << 8;
    pub const DMA1: u32 = 1 << 9;
    pub const DMA2: u32 = 1 << 10;
    pub const DMA3: u32 = 1 << 11;
    pub const KEYPAD: u32 = 1 << 12;
    /// The Slot-2 (Game Boy Advance) cartridge.
    pub const GBA_SLOT: u32 = 1 << 13;
    pub const IPC_SYNC: u32 = 1 << 16;
    pub const IPC_SEND_EMPTY: u32 = 1 << 17;
    pub const IPC_RECV_NOT_EMPTY: u32 = 1 << 18;
    /// A Slot-1 cartridge transfer finished.
    pub const CARD_TRANSFER: u32 = 1 << 19;
    /// The cartridge asserted its interrupt line.
    pub const CARD_IREQ: u32 = 1 << 20;
    /// The 3D geometry command FIFO crossed its threshold. ARM9 only.
    pub const GEOMETRY_FIFO: u32 = 1 << 21;
    /// The lid was opened. ARM7 only.
    pub const LID: u32 = 1 << 22;
    /// An SPI transfer finished — the touchscreen, firmware, and power management. ARM7 only.
    pub const SPI: u32 = 1 << 23;
    /// ARM7 only, and never raised: see the crate docs on wifi.
    pub const WIFI: u32 = 1 << 24;

    /// The sources each core actually has.
    ///
    /// Everything up to the GBA slot is common; the divergence is entirely above bit 15.
    const COMMON: u32 = 0x0000_3F7F;
    pub const ARM9: u32 = COMMON | 0x003F_0000;
    pub const ARM7: u32 = COMMON | SERIAL | 0x001F_0000 | LID | SPI | WIFI;

    pub const fn valid(core: Core) -> u32 {
        match core {
            Core::Arm9 => ARM9,
            Core::Arm7 => ARM7,
        }
    }

    pub const fn timer(channel: usize) -> u32 {
        TIMER0 << channel
    }

    pub const fn dma(channel: usize) -> u32 {
        DMA0 << channel
    }
}

/// One core's `IE`, `IF`, and `IME`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InterruptController {
    core: Core,
    enable: u32,
    flags: u32,
    /// The master switch, distinct from the CPU's own `I` bit in `CPSR`; both must allow it.
    master_enable: bool,
}

impl InterruptController {
    pub fn new(core: Core) -> Self {
        Self {
            core,
            enable: 0,
            flags: 0,
            master_enable: false,
        }
    }

    pub fn core(&self) -> Core {
        self.core
    }

    /// Whether this address is one of the three registers.
    ///
    /// Written as three explicit word comparisons rather than a masked one. `IE` and `IF` are one
    /// word apart, so any mask wide enough to group them also swallows `0x0400_0218`, which is
    /// not a register here — the same shape of bug the GBA interrupt controller and its direct
    /// sound block both hit.
    pub fn owns(addr: u32) -> bool {
        matches!(addr & !3, reg::IME | reg::IE | reg::IF)
    }

    /// Raise one or more sources.
    ///
    /// A source this core does not have is dropped. `IE` gates *dispatch*, not recording, so a
    /// disabled-but-valid source still sets its `IF` bit — games poll `IF` for events they never
    /// want an interrupt for.
    pub fn raise(&mut self, sources: u32) {
        let valid = sources::valid(self.core);
        if sources & !valid != 0 {
            tracing::debug!(
                core = self.core.name(),
                dropped = format_args!("{:#010X}", sources & !valid),
                "interrupt source raised on a core that does not have it"
            );
        }
        self.flags |= sources & valid;
    }

    /// Whether this core should take an interrupt now.
    ///
    /// The CPU's own `I` bit is deliberately not consulted: that belongs to the core, and testing
    /// it in two places is how the two come to disagree.
    pub fn pending(&self) -> bool {
        self.master_enable && (self.enable & self.flags) != 0
    }

    /// The sources both enabled and flagged.
    pub fn active(&self) -> u32 {
        self.enable & self.flags
    }

    pub fn flags(&self) -> u32 {
        self.flags
    }

    pub fn read32(&self, addr: u32) -> Option<u32> {
        Some(match addr & !3 {
            reg::IME => self.master_enable as u32,
            reg::IE => self.enable,
            reg::IF => self.flags,
            _ => return None,
        })
    }

    pub fn write32(&mut self, addr: u32, value: u32) -> bool {
        match addr & !3 {
            reg::IME => self.master_enable = value & 1 != 0,
            reg::IE => self.enable = value & sources::valid(self.core),
            // Ones acknowledge. See the module docs.
            reg::IF => self.flags &= !value,
            _ => return false,
        }
        true
    }

    /// A halfword access, which is how ARM code written for the GBA touches these registers and
    /// how `IME` is usually written.
    pub fn read16(&self, addr: u32) -> Option<u16> {
        let word = self.read32(addr)?;
        Some(if addr & 2 == 0 {
            word as u16
        } else {
            (word >> 16) as u16
        })
    }

    pub fn write16(&mut self, addr: u32, value: u16) -> bool {
        let Some(current) = self.read32(addr) else {
            return false;
        };
        // `IF`'s acknowledge-by-ones semantics mean a halfword write must leave the other half
        // alone, and writing back the half that was read would acknowledge everything in it.
        let (mask, shifted) = if addr & 2 == 0 {
            (0x0000_FFFF, value as u32)
        } else {
            (0xFFFF_0000, (value as u32) << 16)
        };
        if addr & !3 == reg::IF {
            self.flags &= !shifted;
            return true;
        }
        self.write32(addr, (current & !mask) | shifted)
    }

    pub fn read8(&self, addr: u32) -> Option<u8> {
        let word = self.read32(addr)?;
        Some((word >> ((addr & 3) * 8)) as u8)
    }

    pub fn write8(&mut self, addr: u32, value: u8) -> bool {
        let Some(current) = self.read32(addr) else {
            return false;
        };
        let shift = (addr & 3) * 8;
        let shifted = (value as u32) << shift;
        if addr & !3 == reg::IF {
            self.flags &= !shifted;
            return true;
        }
        self.write32(addr, (current & !(0xFF << shift)) | shifted)
    }

    pub fn reset(&mut self) {
        self.enable = 0;
        self.flags = 0;
        self.master_enable = false;
    }
}

impl Savable for InterruptController {
    fn save(&self, w: &mut StateWriter) {
        w.write_u32(self.enable);
        w.write_u32(self.flags);
        w.write_bool(self.master_enable);
    }

    fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        self.enable = r.read_u32()?;
        self.flags = r.read_u32()?;
        self.master_enable = r.read_bool()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
