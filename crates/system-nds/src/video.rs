//! Scanline timing, `DISPSTAT`, and `VCOUNT`.
//!
//! The DS draws 263 scanlines of 355 dots at six master cycles a dot, which is 2130 cycles a line
//! and 560,190 a frame — 59.8261 Hz against the 33.513982 MHz master clock. Those are the numbers
//! `frontend_core::platform` already carries for the DS, and they have to agree.
//!
//! # The timing drives the loop, rather than the loop polling the timing
//!
//! [`VideoTiming::cycles_until_next_event`] says how far the machine may run before something
//! visible happens, and [`VideoTiming::advance`] moves it exactly that far and reports what it
//! was. The frame loop therefore lands precisely on every scanline boundary without a per-cycle
//! check, and the same sequence of events comes out whatever quantum the two CPUs are interleaved
//! at — which is what makes the dual-core interleaving deterministic rather than merely
//! reproducible-in-practice.
//!
//! The alternative, stepping some fixed number of cycles and asking afterwards whether a boundary
//! was crossed, was rejected: a boundary crossed *inside* a step is a scanline rendered with
//! register values from after the write that was supposed to happen on the next line, which is
//! exactly the mid-frame-scroll effect prompt 08 requires to work.
//!
//! # Two `DISPSTAT`s, one `VCOUNT`
//!
//! Each core has its own `DISPSTAT` with its own interrupt enables and its own V-count match
//! target, and there is one line counter they both read. A game commonly sets the ARM7's match to
//! a different line from the ARM9's, so folding them into one register makes the two cores
//! interrupt each other's frame.

use crate::Core;
use core_common::{Savable, StateError, StateReader, StateWriter};

pub const SCREEN_WIDTH: u32 = 256;
pub const SCREEN_HEIGHT: u32 = 192;
/// Both screens, stacked: the framebuffer `frontend-core` already expects for the DS.
pub const FRAMEBUFFER_HEIGHT: u32 = SCREEN_HEIGHT * 2;

pub const DOTS_PER_LINE: u32 = 355;
pub const CYCLES_PER_DOT: u32 = 6;
pub const CYCLES_PER_LINE: u32 = DOTS_PER_LINE * CYCLES_PER_DOT;
pub const LINES_PER_FRAME: u16 = 263;
pub const CYCLES_PER_FRAME: u32 = CYCLES_PER_LINE * LINES_PER_FRAME as u32;

/// Where in a line horizontal blanking begins.
const HBLANK_CYCLE: u32 = SCREEN_WIDTH * CYCLES_PER_DOT;

/// The line vertical blanking starts on, and the line its flag is cleared again.
///
/// The flag is cleared on line 262 rather than at the wrap to line 0, which is a real one-line
/// difference software can see.
const VBLANK_START: u16 = SCREEN_HEIGHT as u16;
const VBLANK_FLAG_END: u16 = LINES_PER_FRAME - 1;

pub mod reg {
    pub const DISPSTAT: u32 = 0x0400_0004;
    pub const VCOUNT: u32 = 0x0400_0006;
}

mod stat {
    pub const VBLANK: u16 = 1 << 0;
    pub const HBLANK: u16 = 1 << 1;
    pub const VCOUNT_MATCH: u16 = 1 << 2;
    pub const VBLANK_IRQ: u16 = 1 << 3;
    pub const HBLANK_IRQ: u16 = 1 << 4;
    pub const VCOUNT_IRQ: u16 = 1 << 5;
    /// Bit 7 is bit 8 of the V-count target; bits 8-15 are its low eight.
    pub const VCOUNT_HIGH: u16 = 1 << 7;
    /// The bits software owns. The three status flags are read-only.
    pub const WRITABLE: u16 = 0xFFB8;
}

/// What just happened, as [`VideoTiming::advance`] reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoEvent {
    /// The visible part of the current line is over. This is when the line is composited: the
    /// registers hold whatever the game left them holding for *this* line.
    HBlankStart,
    /// The line is over and [`VideoTiming::line`] has moved on.
    LineEnd,
}

/// Which interrupts each core should take as a result of an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct VideoIrqs {
    pub vblank: bool,
    pub hblank: bool,
    pub vcount: bool,
}

impl VideoIrqs {
    pub fn any(self) -> bool {
        self.vblank || self.hblank || self.vcount
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoTiming {
    line: u16,
    /// Cycles elapsed within the current line, `0..CYCLES_PER_LINE`.
    cycle: u32,
    /// One per core: the interrupt enables, the V-count target, and the cached status flags.
    dispstat: [u16; 2],
    pending: [VideoIrqs; 2],
}

impl Default for VideoTiming {
    fn default() -> Self {
        Self::new()
    }
}

impl VideoTiming {
    pub fn new() -> Self {
        let mut timing = Self {
            line: 0,
            cycle: 0,
            dispstat: [0; 2],
            pending: [VideoIrqs::default(); 2],
        };
        // Line 0 matches a target of 0, which is the reset value, so the flag has to be right
        // before anything has stepped.
        timing.update_vcount_match(false);
        timing
    }

    pub fn line(&self) -> u16 {
        self.line
    }

    pub fn cycle_in_line(&self) -> u32 {
        self.cycle
    }

    /// Whether the current line is one of the 192 that are drawn.
    pub fn is_visible_line(&self) -> bool {
        self.line < SCREEN_HEIGHT as u16
    }

    pub fn in_vblank(&self) -> bool {
        (VBLANK_START..VBLANK_FLAG_END).contains(&self.line)
    }

    pub fn in_hblank(&self) -> bool {
        self.cycle >= HBLANK_CYCLE
    }

    /// How far the machine may run before the next thing worth reacting to.
    ///
    /// Never zero: [`advance`](Self::advance) always consumes what this returns, so a zero would
    /// leave the frame loop spinning.
    pub fn cycles_until_next_event(&self) -> u32 {
        if self.cycle < HBLANK_CYCLE {
            HBLANK_CYCLE - self.cycle
        } else {
            CYCLES_PER_LINE - self.cycle
        }
    }

    /// Move forward by exactly [`cycles_until_next_event`](Self::cycles_until_next_event) and say
    /// what the boundary was.
    ///
    /// Any interrupts the boundary raises are latched for [`take_pending`](Self::take_pending),
    /// which is the same arrangement the IPC hardware uses and for the same reason: this module
    /// has no access to either interrupt controller.
    pub fn advance(&mut self, cycles: u32) -> Option<VideoEvent> {
        debug_assert_eq!(
            cycles,
            self.cycles_until_next_event(),
            "the frame loop must land on boundaries, not step past them"
        );
        self.cycle += cycles;

        if self.cycle == HBLANK_CYCLE {
            // The hblank interrupt fires on every line, including during vertical blanking.
            for core in [Core::Arm9, Core::Arm7] {
                if self.dispstat[core as usize] & stat::HBLANK_IRQ != 0 {
                    self.pending[core as usize].hblank = true;
                }
            }
            return Some(VideoEvent::HBlankStart);
        }

        self.cycle = 0;
        self.line += 1;
        if self.line == LINES_PER_FRAME {
            self.line = 0;
        }
        if self.line == VBLANK_START {
            for core in [Core::Arm9, Core::Arm7] {
                if self.dispstat[core as usize] & stat::VBLANK_IRQ != 0 {
                    self.pending[core as usize].vblank = true;
                }
            }
        }
        self.update_vcount_match(true);
        Some(VideoEvent::LineEnd)
    }

    /// Recompute the V-count match flag for both cores, optionally raising interrupts.
    ///
    /// `raise` is false at construction and when software rewrites its target: hardware compares
    /// on the line transition, so setting `DISPSTAT`'s target to the line already showing updates
    /// the flag without producing an interrupt for a transition that did not happen.
    fn update_vcount_match(&mut self, raise: bool) {
        for core in [Core::Arm9, Core::Arm7] {
            let index = core as usize;
            let target = Self::vcount_target(self.dispstat[index]);
            let matched = target == self.line;
            let was = self.dispstat[index] & stat::VCOUNT_MATCH != 0;
            if matched {
                self.dispstat[index] |= stat::VCOUNT_MATCH;
            } else {
                self.dispstat[index] &= !stat::VCOUNT_MATCH;
            }
            if raise && matched && !was && self.dispstat[index] & stat::VCOUNT_IRQ != 0 {
                self.pending[index].vcount = true;
            }
        }
    }

    fn vcount_target(dispstat: u16) -> u16 {
        (dispstat >> 8) | ((dispstat & stat::VCOUNT_HIGH) << 1)
    }

    pub fn take_pending(&mut self, core: Core) -> VideoIrqs {
        std::mem::take(&mut self.pending[core as usize])
    }

    pub fn read16(&self, core: Core, addr: u32) -> Option<u16> {
        match addr & !1 {
            reg::DISPSTAT => {
                // The three status flags are computed rather than stored, so they cannot drift
                // out of step with the counters that decide them. The match flag is the one
                // exception: it is maintained on the line transition, because its interrupt is.
                let mut value = self.dispstat[core as usize] & !(stat::VBLANK | stat::HBLANK);
                if self.in_vblank() {
                    value |= stat::VBLANK;
                }
                if self.in_hblank() {
                    value |= stat::HBLANK;
                }
                Some(value)
            }
            reg::VCOUNT => Some(self.line),
            _ => None,
        }
    }

    pub fn write16(&mut self, core: Core, addr: u32, value: u16) -> bool {
        match addr & !1 {
            reg::DISPSTAT => {
                let index = core as usize;
                self.dispstat[index] =
                    (self.dispstat[index] & !stat::WRITABLE) | (value & stat::WRITABLE);
                self.update_vcount_match(false);
                true
            }
            // VCOUNT is writable on the DS, unlike on the GBA, and writing it genuinely moves
            // the line counter. Nothing this emulator runs does it, and doing it mid-frame would
            // desynchronize the renderer from the counter, so it is accepted and ignored.
            reg::VCOUNT => {
                tracing::debug!("write to VCOUNT ignored: {value:#06X}");
                true
            }
            _ => false,
        }
    }

    pub fn read8(&self, core: Core, addr: u32) -> Option<u8> {
        let value = self.read16(core, addr & !1)?;
        Some(if addr & 1 == 0 {
            value as u8
        } else {
            (value >> 8) as u8
        })
    }

    pub fn write8(&mut self, core: Core, addr: u32, value: u8) -> bool {
        let Some(current) = self.read16(core, addr & !1) else {
            return false;
        };
        let spliced = if addr & 1 == 0 {
            (current & 0xFF00) | value as u16
        } else {
            (current & 0x00FF) | ((value as u16) << 8)
        };
        self.write16(core, addr & !1, spliced)
    }

    pub fn owns(addr: u32) -> bool {
        matches!(addr & !1, reg::DISPSTAT | reg::VCOUNT)
    }

    pub fn reset(&mut self) {
        *self = Self::new();
    }
}

impl Savable for VideoTiming {
    fn save(&self, w: &mut StateWriter) {
        w.write_u16(self.line);
        w.write_u32(self.cycle);
        for value in self.dispstat {
            w.write_u16(value);
        }
        for irqs in self.pending {
            w.write_bool(irqs.vblank);
            w.write_bool(irqs.hblank);
            w.write_bool(irqs.vcount);
        }
    }

    fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        self.line = r.read_u16()?;
        self.cycle = r.read_u32()?;
        for value in &mut self.dispstat {
            *value = r.read_u16()?;
        }
        for irqs in &mut self.pending {
            irqs.vblank = r.read_bool()?;
            irqs.hblank = r.read_bool()?;
            irqs.vcount = r.read_bool()?;
        }
        if self.line >= LINES_PER_FRAME || self.cycle >= CYCLES_PER_LINE {
            return Err(StateError::Malformed(
                "video timing is outside the frame".into(),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
