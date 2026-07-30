//! Four timers per core, eight in the machine.
//!
//! # This is the GBA's timer block, and it is duplicated rather than shared
//!
//! The DS's timers are the same hardware as the Game Boy Advance's: a 16-bit up-counter per
//! channel, a reload value written through the same address the counter reads back from, a
//! four-way prescaler, cascade from the channel below, and an interrupt on overflow. The
//! implementation in `system-gba::timers` would serve unchanged.
//!
//! It is not shared, because `system-*` crates may not depend on each other and `core-common` is
//! explicitly closed to platform-specific behaviour. That leaves moving it into a new shared crate
//! — which is the same unresolved placement question `system-gb::apu`'s register layer is stuck
//! on, recorded in `AGENTS.md` under "Smaller, well-defined items". This is the second instance of
//! it, which is worth knowing when that question is finally answered: the answer should cover
//! both.
//!
//! What is genuinely different here is only the clock. The DS counts these at the 33.513982 MHz
//! system clock on *both* cores — the ARM9's doubled clock does not reach the timers — so a cycle
//! passed to [`TimerBlock::step`] is a system cycle whichever core is being stepped.
//!
//! # The reload is not the counter
//!
//! `TMxCNT_L` reads the live counter and writes the reload value. They are different registers
//! sharing an address. Writing it does not disturb a running timer; the value appears the next
//! time the timer overflows or is re-enabled.
//!
//! # Cascade is not a prescaler
//!
//! A channel in count-up mode ignores its prescaler entirely and advances once per overflow of
//! the channel below it. Channel 0 has nothing below it, so its count-up bit does nothing —
//! stored, readable, and with no effect, which is what hardware does.

use core_common::{Savable, StateError, StateReader, StateWriter};

/// Base of the timer registers, identical in both cores' I/O space.
pub const BASE: u32 = 0x0400_0100;
pub const CHANNELS: usize = 4;

/// Cycles per tick, as a shift, for each of the four prescaler settings.
const PRESCALER_SHIFT: [u32; 4] = [0, 6, 8, 10];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct Timer {
    /// The live 16-bit counter.
    counter: u16,
    /// What the counter is set to on overflow and on being enabled.
    reload: u16,
    control: u16,
    /// Cycles counted toward the next tick but not yet worth one.
    residual: u32,
}

impl Timer {
    #[inline]
    fn enabled(&self) -> bool {
        self.control & 0x80 != 0
    }

    #[inline]
    fn irq_on_overflow(&self) -> bool {
        self.control & 0x40 != 0
    }

    #[inline]
    fn count_up(&self) -> bool {
        self.control & 0x04 != 0
    }

    #[inline]
    fn shift(&self) -> u32 {
        PRESCALER_SHIFT[(self.control & 3) as usize]
    }

    /// Advance by `ticks` counts, returning how many times it wrapped.
    ///
    /// Computed rather than looped: a timer with a prescaler of 1 and a reload near `0xFFFF`
    /// overflows thousands of times in one scanline, and a loop there is measurable.
    fn advance(&mut self, ticks: u32) -> u32 {
        if ticks == 0 {
            return 0;
        }
        let total = self.counter as u32 + ticks;
        if total <= 0xFFFF {
            self.counter = total as u16;
            return 0;
        }
        // The counter runs from `reload` to `0xFFFF`, so a full lap is this many counts.
        let span = 0x1_0000 - self.reload as u32;
        let excess = total - 0x1_0000;
        self.counter = (self.reload as u32 + excess % span) as u16;
        1 + excess / span
    }
}

/// One core's four timers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TimerBlock {
    timers: [Timer; CHANNELS],
}

impl TimerBlock {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn owns(addr: u32) -> bool {
        (BASE..BASE + (CHANNELS as u32) * 4).contains(&addr)
    }

    /// Advance every channel by `cycles` system cycles.
    ///
    /// Returns a bitmask of channels that overflowed with their interrupt enabled, which the
    /// caller turns into `IF` bits. Channels are stepped in order so a cascade sees the overflows
    /// its source produced during *this* call rather than the previous one.
    pub fn step(&mut self, cycles: u32) -> u8 {
        let mut irqs = 0u8;
        let mut carried = 0u32;
        for index in 0..CHANNELS {
            let timer = &mut self.timers[index];
            if !timer.enabled() {
                carried = 0;
                continue;
            }
            let overflows = if timer.count_up() && index > 0 {
                timer.advance(carried)
            } else {
                let total = timer.residual + cycles;
                let shift = timer.shift();
                timer.residual = total & ((1 << shift) - 1);
                timer.advance(total >> shift)
            };
            if overflows > 0 && timer.irq_on_overflow() {
                irqs |= 1 << index;
            }
            carried = overflows;
        }
        irqs
    }

    pub fn read16(&self, addr: u32) -> Option<u16> {
        let (channel, which) = Self::decode(addr)?;
        Some(match which {
            0 => self.timers[channel].counter,
            _ => self.timers[channel].control,
        })
    }

    pub fn write16(&mut self, addr: u32, value: u16) -> bool {
        let Some((channel, which)) = Self::decode(addr) else {
            return false;
        };
        let timer = &mut self.timers[channel];
        if which == 0 {
            // The reload, not the counter. See the module docs.
            timer.reload = value;
        } else {
            let was_enabled = timer.enabled();
            timer.control = value & 0x00C7;
            if !was_enabled && timer.enabled() {
                // A disabled-to-enabled transition loads the counter and restarts the prescaler.
                // Only the transition does this: rewriting the control register of a running
                // timer with the enable bit still set leaves the counter where it is.
                timer.counter = timer.reload;
                timer.residual = 0;
            }
        }
        true
    }

    pub fn read8(&self, addr: u32) -> Option<u8> {
        let value = self.read16(addr & !1)?;
        Some(if addr & 1 == 0 {
            value as u8
        } else {
            (value >> 8) as u8
        })
    }

    pub fn write8(&mut self, addr: u32, value: u8) -> bool {
        let Some(current) = self.read16_raw(addr & !1) else {
            return false;
        };
        let spliced = if addr & 1 == 0 {
            (current & 0xFF00) | value as u16
        } else {
            (current & 0x00FF) | ((value as u16) << 8)
        };
        self.write16(addr & !1, spliced)
    }

    /// What a halfword write would have to splice into.
    ///
    /// Not [`read16`](Self::read16): the low half reads the *counter* and writes the *reload*, so
    /// splicing a byte into what was read would set the reload to whatever the counter happened
    /// to be at the moment of the store.
    fn read16_raw(&self, addr: u32) -> Option<u16> {
        let (channel, which) = Self::decode(addr)?;
        Some(match which {
            0 => self.timers[channel].reload,
            _ => self.timers[channel].control,
        })
    }

    fn decode(addr: u32) -> Option<(usize, u32)> {
        if !Self::owns(addr) {
            return None;
        }
        let offset = addr - BASE;
        Some(((offset / 4) as usize, (offset % 4) / 2))
    }

    /// The live counter, for the debugger and for tests.
    pub fn counter(&self, channel: usize) -> u16 {
        self.timers[channel].counter
    }

    pub fn reload(&self, channel: usize) -> u16 {
        self.timers[channel].reload
    }

    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

impl Savable for TimerBlock {
    fn save(&self, w: &mut StateWriter) {
        for timer in &self.timers {
            w.write_u16(timer.counter);
            w.write_u16(timer.reload);
            w.write_u16(timer.control);
            w.write_u32(timer.residual);
        }
    }

    fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        for timer in &mut self.timers {
            timer.counter = r.read_u16()?;
            timer.reload = r.read_u16()?;
            timer.control = r.read_u16()?;
            timer.residual = r.read_u32()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
