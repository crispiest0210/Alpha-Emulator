//! The GBA interrupt controller: `IE`, `IF`, `IME`.
//!
//! # Not the Game Boy's vector table
//!
//! Prompt 11's Game Boy dispatches each interrupt to its own address. The GBA does not: every
//! interrupt enters the BIOS at one fixed address, and the BIOS then calls a handler whose
//! address the *game* has left in a known word of IWRAM. Assuming the Game Boy's arrangement
//! here would produce a machine that never runs a game's interrupt code at all.
//!
//! This module answers "is an interrupt pending" and owns the three registers. Where the CPU
//! goes when one fires is the system assembly's business, because it depends on whether a BIOS
//! is present — see [`IRQ_VECTOR`] and [`HLE_HANDLER_POINTER`].
//!
//! # `IF` is acknowledged by writing ones
//!
//! Writing a 1 bit to `IF` *clears* that bit. Writing 0 leaves it alone. This is backwards from
//! every other register in the machine and is the single most common way to get a GBA emulator
//! stuck in an interrupt loop: implement it as a plain store and the handler can never
//! acknowledge anything.

use core_common::{Savable, StateError, StateReader, StateWriter};

/// Register addresses.
pub mod reg {
    pub const IE: u32 = 0x0400_0200;
    pub const IF: u32 = 0x0400_0202;
    pub const IME: u32 = 0x0400_0208;
}

/// Where the CPU jumps when an interrupt is taken.
///
/// Inside the BIOS. With no BIOS supplied the system assembly emulates what the BIOS would have
/// done rather than jumping here into nothing.
pub const IRQ_VECTOR: u32 = 0x0000_0018;

/// The word the BIOS reads to find the game's handler.
///
/// The top of IWRAM. A game writes its handler address here during setup and the BIOS jumps
/// through it; an emulator without a BIOS has to do the same, or every game's interrupt code is
/// unreachable.
pub const HLE_HANDLER_POINTER: u32 = 0x0300_7FFC;

/// Interrupt source bits, shared by `IE` and `IF`.
pub mod source {
    pub const VBLANK: u16 = 1 << 0;
    pub const HBLANK: u16 = 1 << 1;
    pub const VCOUNT: u16 = 1 << 2;
    pub const TIMER0: u16 = 1 << 3;
    pub const TIMER1: u16 = 1 << 4;
    pub const TIMER2: u16 = 1 << 5;
    pub const TIMER3: u16 = 1 << 6;
    pub const SERIAL: u16 = 1 << 7;
    pub const DMA0: u16 = 1 << 8;
    pub const DMA1: u16 = 1 << 9;
    pub const DMA2: u16 = 1 << 10;
    pub const DMA3: u16 = 1 << 11;
    pub const KEYPAD: u16 = 1 << 12;
    pub const GAMEPAK: u16 = 1 << 13;

    /// The bits that exist. Bits 14 and 15 are unused and never set.
    pub const ALL: u16 = 0x3FFF;

    /// The timer interrupt for a given channel.
    pub const fn timer(channel: usize) -> u16 {
        TIMER0 << channel
    }

    /// The DMA interrupt for a given channel.
    pub const fn dma(channel: usize) -> u16 {
        DMA0 << channel
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct InterruptController {
    /// Which sources may interrupt.
    enable: u16,
    /// Which sources have fired and not been acknowledged.
    flags: u16,
    /// The master switch. Distinct from the CPU's own `I` bit in `CPSR`, and both must allow it.
    master_enable: bool,
}

impl InterruptController {
    pub fn new() -> Self {
        Self::default()
    }

    /// `IE` and `IF` are two halves of one word, which is why the mask is `!3` and not `!1`:
    /// grouping them by halfword would leave `IF` matching nothing.
    pub fn owns(addr: u32) -> bool {
        matches!(addr & !3, reg::IE | reg::IME)
    }

    /// Raise one or more sources.
    ///
    /// Raising a source that is not enabled still sets its `IF` bit: `IE` gates *dispatch*, not
    /// recording. Games poll `IF` for events they never want an interrupt for, so filtering
    /// here would break a common idiom.
    pub fn raise(&mut self, sources: u16) {
        self.flags |= sources & source::ALL;
    }

    /// Whether the CPU should take an interrupt now.
    ///
    /// The CPU's own `I` bit is *not* consulted here — that belongs to the core, and checking it
    /// in two places is how the two end up disagreeing.
    pub fn pending(&self) -> bool {
        self.master_enable && (self.enable & self.flags) != 0
    }

    /// The sources that are both enabled and flagged.
    pub fn active(&self) -> u16 {
        self.enable & self.flags
    }

    pub fn read16(&self, addr: u32) -> Option<u16> {
        Some(match addr & !3 {
            reg::IE => match addr & 2 {
                0 => self.enable,
                _ => self.flags,
            },
            reg::IME => self.master_enable as u16,
            _ => return None,
        })
    }

    pub fn write16(&mut self, addr: u32, value: u16) -> Option<()> {
        match addr & !3 {
            reg::IE => match addr & 2 {
                0 => self.enable = value & source::ALL,
                // Acknowledgement, not assignment: a 1 bit clears, a 0 bit leaves alone.
                _ => self.flags &= !value,
            },
            reg::IME => self.master_enable = value & 1 != 0,
            _ => return None,
        }
        Some(())
    }

    /// `IE` and `IF` are adjacent and games write both in one 32-bit store.
    pub fn write32(&mut self, addr: u32, value: u32) -> Option<()> {
        if addr & !3 != reg::IE {
            return self.write16(addr, value as u16);
        }
        self.write16(reg::IE, value as u16)?;
        self.write16(reg::IF, (value >> 16) as u16)
    }

    pub fn read32(&self, addr: u32) -> Option<u32> {
        if addr & !3 != reg::IE {
            return self.read16(addr).map(u32::from);
        }
        Some((self.read16(reg::IE)? as u32) | ((self.read16(reg::IF)? as u32) << 16))
    }
}

impl Savable for InterruptController {
    fn save(&self, w: &mut StateWriter) {
        w.write_u16(self.enable);
        w.write_u16(self.flags);
        w.write_bool(self.master_enable);
    }

    fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        self.enable = r.read_u16()?;
        self.flags = r.read_u16()?;
        self.master_enable = r.read_bool()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_is_pending_on_a_fresh_machine() {
        let irq = InterruptController::new();
        assert!(!irq.pending());
        assert_eq!(irq.active(), 0);
    }

    #[test]
    fn all_three_gates_must_agree_before_an_interrupt_is_taken() {
        let mut irq = InterruptController::new();
        irq.raise(source::VBLANK);
        assert!(!irq.pending(), "not enabled and the master switch is off");

        irq.write16(reg::IE, source::VBLANK);
        assert!(!irq.pending(), "still no master switch");

        irq.write16(reg::IME, 1);
        assert!(irq.pending());
        assert_eq!(irq.active(), source::VBLANK);
    }

    #[test]
    fn a_disabled_source_still_records_its_flag() {
        // `IE` gates dispatch, not recording. Games poll `IF` for events they never want an
        // interrupt for, so filtering at `raise` would break a common idiom.
        let mut irq = InterruptController::new();
        irq.write16(reg::IME, 1);
        irq.raise(source::HBLANK);
        assert!(!irq.pending());
        assert_eq!(irq.read16(reg::IF), Some(source::HBLANK));
    }

    #[test]
    fn writing_a_one_to_if_acknowledges_that_source() {
        // Backwards from every other register in the machine, and the most common way to get a
        // GBA emulator stuck in an interrupt loop.
        let mut irq = InterruptController::new();
        irq.raise(source::VBLANK | source::TIMER0);
        irq.write16(reg::IF, source::VBLANK);
        assert_eq!(
            irq.read16(reg::IF),
            Some(source::TIMER0),
            "the one acknowledged, and only that one"
        );
    }

    #[test]
    fn writing_a_zero_to_if_leaves_the_flag_standing() {
        let mut irq = InterruptController::new();
        irq.raise(source::VBLANK);
        irq.write16(reg::IF, 0);
        assert_eq!(irq.read16(reg::IF), Some(source::VBLANK));
    }

    #[test]
    fn the_unused_top_bits_never_set() {
        let mut irq = InterruptController::new();
        irq.write16(reg::IE, 0xFFFF);
        assert_eq!(irq.read16(reg::IE), Some(source::ALL));
        irq.raise(0xFFFF);
        assert_eq!(irq.read16(reg::IF), Some(source::ALL));
    }

    #[test]
    fn ie_and_if_can_be_written_together_as_one_word() {
        // Games do this: they are adjacent, and one store enables a source and acknowledges a
        // stale flag at once.
        let mut irq = InterruptController::new();
        irq.raise(source::VBLANK | source::HBLANK);
        irq.write32(
            reg::IE,
            (source::VBLANK as u32) | ((source::HBLANK as u32) << 16),
        );
        assert_eq!(irq.read16(reg::IE), Some(source::VBLANK));
        assert_eq!(irq.read16(reg::IF), Some(source::VBLANK), "HBlank cleared");
    }

    #[test]
    fn a_word_read_returns_both_registers() {
        let mut irq = InterruptController::new();
        irq.write16(reg::IE, source::TIMER2);
        irq.raise(source::DMA1);
        assert_eq!(
            irq.read32(reg::IE),
            Some((source::TIMER2 as u32) | ((source::DMA1 as u32) << 16))
        );
    }

    #[test]
    fn the_source_helpers_line_up_with_their_channels() {
        assert_eq!(source::timer(0), source::TIMER0);
        assert_eq!(source::timer(3), source::TIMER3);
        assert_eq!(source::dma(0), source::DMA0);
        assert_eq!(source::dma(3), source::DMA3);
    }

    #[test]
    fn the_controller_claims_only_its_own_addresses() {
        assert!(InterruptController::owns(reg::IE));
        assert!(InterruptController::owns(reg::IF));
        assert!(InterruptController::owns(reg::IME));
        assert!(!InterruptController::owns(0x0400_0204), "that is WAITCNT");
        assert!(!InterruptController::owns(0x0400_0000));
    }

    #[test]
    fn interrupt_state_round_trips() {
        use savestate::{decode_state, encode_state};
        let mut irq = InterruptController::new();
        irq.write16(reg::IE, source::VBLANK | source::TIMER1);
        irq.write16(reg::IME, 1);
        irq.raise(source::TIMER1);

        let bytes = encode_state("gba-irq", 1, &irq);
        let mut restored = InterruptController::new();
        decode_state("gba-irq", 1, &bytes, &mut restored).unwrap();
        assert_eq!(restored, irq);
        assert!(restored.pending(), "and it is still pending after a load");
    }
}
