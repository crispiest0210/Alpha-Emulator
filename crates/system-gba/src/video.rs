//! GBA video timing and the display registers.
//!
//! # The blanking periods are not gaps
//!
//! A GBA scanline is 240 visible dots followed by 68 more, and a frame is 160 visible lines
//! followed by 68 more. Both of those trailing periods are when games do their work: HBlank is
//! when a DMA channel refills a scroll register, and VBlank is when almost everything else
//! happens. Modelling a frame as 160 lines and jumping to the next would remove the only window
//! most games have to touch video memory.
//!
//! # `DISPSTAT` is half status and half configuration
//!
//! Its low three bits are read-only flags the hardware sets, and the bits above them are enables
//! and a comparison value the game writes. A plain store would let a game clear the VBlank flag,
//! which is not something hardware allows and not something any game intends.

use core_common::{Savable, StateError, StateReader, StateWriter};

pub const SCREEN_WIDTH: u32 = 240;
pub const SCREEN_HEIGHT: u32 = 160;

/// Dots per scanline, including the 68 in horizontal blanking.
pub const DOTS_PER_LINE: u32 = 308;
/// Scanlines per frame, including the 68 in vertical blanking.
pub const LINES_PER_FRAME: u32 = 228;
/// Four cycles per dot.
pub const CYCLES_PER_DOT: u32 = 4;
pub const CYCLES_PER_LINE: u32 = DOTS_PER_LINE * CYCLES_PER_DOT;

/// When horizontal blanking begins within a line.
pub const HBLANK_START_CYCLE: u32 = SCREEN_WIDTH * CYCLES_PER_DOT;

/// Register addresses.
pub mod reg {
    pub const DISPCNT: u32 = 0x0400_0000;
    pub const DISPSTAT: u32 = 0x0400_0004;
    pub const VCOUNT: u32 = 0x0400_0006;
}

pub mod dispcnt {
    pub const MODE: u16 = 0x0007;
    /// Which of the two bitmap frames is displayed, in modes 4 and 5.
    pub const FRAME_SELECT: u16 = 1 << 4;
    pub const HBLANK_INTERVAL_FREE: u16 = 1 << 5;
    /// Set selects one-dimensional object tile mapping.
    pub const OBJ_1D_MAPPING: u16 = 1 << 6;
    /// Blanks the screen to white without disturbing any video memory.
    pub const FORCED_BLANK: u16 = 1 << 7;
    pub const BG0: u16 = 1 << 8;
    pub const BG2: u16 = 1 << 10;
    pub const OBJ: u16 = 1 << 12;
}

pub mod dispstat {
    pub const VBLANK: u16 = 1 << 0;
    pub const HBLANK: u16 = 1 << 1;
    pub const VCOUNT_MATCH: u16 = 1 << 2;
    pub const VBLANK_IRQ: u16 = 1 << 3;
    pub const HBLANK_IRQ: u16 = 1 << 4;
    pub const VCOUNT_IRQ: u16 = 1 << 5;

    /// The three the hardware owns; a game's write must not reach them.
    pub const READ_ONLY: u16 = VBLANK | HBLANK | VCOUNT_MATCH;
}

/// What the video hardware did during a slice of time.
///
/// Edges rather than levels: "entered HBlank" is what arms a DMA channel, and a caller that saw
/// only the current state would either miss the edge or act on it repeatedly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct VideoEvents {
    /// A visible scanline finished drawing and should be composited.
    pub scanline_ready: Option<u16>,
    pub entered_hblank: bool,
    pub entered_vblank: bool,
    /// `VCOUNT` wrapped to zero, so a new frame's rendering state begins.
    pub frame_started: bool,
    /// `VCOUNT` reached the value the game asked to be told about.
    pub vcount_matched: bool,
}

/// The display registers and the scanline state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct VideoTiming {
    pub dispcnt: u16,
    /// Only the writable half is stored; the flags are derived from the position below.
    dispstat: u16,
    vcount: u16,
    /// Cycles into the current scanline.
    dot_cycle: u32,
}

impl VideoTiming {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn owns(addr: u32) -> bool {
        (reg::DISPCNT..=reg::VCOUNT + 1).contains(&addr)
    }

    pub fn vcount(&self) -> u16 {
        self.vcount
    }

    pub fn mode(&self) -> u16 {
        self.dispcnt & dispcnt::MODE
    }

    pub fn in_vblank(&self) -> bool {
        self.vcount as u32 >= SCREEN_HEIGHT
    }

    pub fn in_hblank(&self) -> bool {
        self.dot_cycle >= HBLANK_START_CYCLE
    }

    /// Cycles until the next point [`Self::tick`] could report an edge: either horizontal
    /// blanking beginning on the current line, or the line itself ending, whichever comes first.
    ///
    /// `tick`'s own cap only stops at a line boundary, which is correct for its callers — a real
    /// instruction or DMA burst is always far shorter than a line, so the cap is never reached
    /// before the access it is charging for finishes. A predictor that wants to know exactly when
    /// `entered_hblank` will next be true, without stepping one cycle at a time to find out, has
    /// no such guarantee: asking `tick` for a whole frame in one call skips straight past the
    /// mid-line point where hblank actually starts to wherever the line ends, up to 272 cycles
    /// later. Capping each request to this instead keeps `tick` from ever overshooting a mid-line
    /// edge while still covering a whole line in at most two calls.
    pub fn cycles_until_next_edge(&self) -> u32 {
        if self.in_hblank() {
            CYCLES_PER_LINE - self.dot_cycle
        } else {
            HBLANK_START_CYCLE - self.dot_cycle
        }
    }

    /// Which bitmap frame modes 4 and 5 display, as a VRAM byte offset.
    ///
    /// Double buffering is the whole reason those modes have two frames: a game draws into the
    /// one that is not showing and flips a single bit.
    pub fn bitmap_frame_offset(&self) -> usize {
        if self.dispcnt & dispcnt::FRAME_SELECT != 0 {
            0xA000
        } else {
            0
        }
    }

    pub fn forced_blank(&self) -> bool {
        self.dispcnt & dispcnt::FORCED_BLANK != 0
    }

    /// Advance by up to `cycles`, but never past the next line boundary — so one call reports at
    /// most one of each edge, which is the only way it can report every one of them: they carry
    /// no count, only whether they happened.
    ///
    /// Returns how many cycles this step actually used alongside the events, which is at most
    /// `cycles` and less than it whenever a line boundary was reached first. A long CPU
    /// instruction or a DMA burst routinely covers more than one scanline, and the caller is the
    /// one that loops — [`crate::system::GbaSystemBus::advance`] — feeding the leftover back in
    /// until none remains, so a step spanning three lines renders three scanlines and advances
    /// the affine layers three times rather than folding them into whichever line happened to be
    /// current when the whole span had been consumed.
    pub fn tick(&mut self, cycles: u32) -> (VideoEvents, u32) {
        let mut events = VideoEvents::default();
        let was_hblank = self.in_hblank();
        let step = cycles.min(CYCLES_PER_LINE - self.dot_cycle);
        self.dot_cycle += step;

        if !was_hblank && self.in_hblank() {
            events.entered_hblank = true;
            if (self.vcount as u32) < SCREEN_HEIGHT {
                events.scanline_ready = Some(self.vcount);
            }
        }

        if self.dot_cycle >= CYCLES_PER_LINE {
            self.dot_cycle -= CYCLES_PER_LINE;
            self.advance_line(&mut events);
        }
        (events, step)
    }

    fn advance_line(&mut self, events: &mut VideoEvents) {
        self.vcount += 1;
        if self.vcount as u32 >= LINES_PER_FRAME {
            self.vcount = 0;
            events.frame_started = true;
        }
        if self.vcount as u32 == SCREEN_HEIGHT {
            events.entered_vblank = true;
        }
        if self.vcount == (self.dispstat >> 8) {
            events.vcount_matched = true;
        }
    }

    /// Which of these events the game asked to be interrupted for.
    ///
    /// The enables live in `DISPSTAT` and are checked here rather than by the caller, so that
    /// "did it happen" and "does anyone care" stay in one place.
    pub fn interrupt_sources(&self, events: &VideoEvents) -> u16 {
        use crate::irq::source;
        let mut out = 0;
        if events.entered_vblank && self.dispstat & dispstat::VBLANK_IRQ != 0 {
            out |= source::VBLANK;
        }
        if events.entered_hblank && self.dispstat & dispstat::HBLANK_IRQ != 0 {
            out |= source::HBLANK;
        }
        if events.vcount_matched && self.dispstat & dispstat::VCOUNT_IRQ != 0 {
            out |= source::VCOUNT;
        }
        out
    }

    pub fn read16(&self, addr: u32) -> Option<u16> {
        Some(match addr & !1 {
            reg::DISPCNT => self.dispcnt,
            reg::DISPSTAT => {
                // The flags are derived from where the beam is rather than stored, so they
                // cannot drift out of step with it.
                let mut value = self.dispstat & !dispstat::READ_ONLY;
                if self.in_vblank() {
                    value |= dispstat::VBLANK;
                }
                if self.in_hblank() {
                    value |= dispstat::HBLANK;
                }
                if self.vcount == (self.dispstat >> 8) {
                    value |= dispstat::VCOUNT_MATCH;
                }
                value
            }
            reg::VCOUNT => self.vcount,
            _ => return None,
        })
    }

    pub fn write16(&mut self, addr: u32, value: u16) -> Option<()> {
        match addr & !1 {
            reg::DISPCNT => self.dispcnt = value,
            // The low three bits belong to the hardware. A plain store would let a game clear
            // the VBlank flag, which hardware does not allow and no game intends.
            reg::DISPSTAT => {
                self.dispstat =
                    (self.dispstat & dispstat::READ_ONLY) | (value & !dispstat::READ_ONLY)
            }
            // VCOUNT is read-only; a write is silently dropped rather than moving the beam.
            reg::VCOUNT => {}
            _ => return None,
        }
        Some(())
    }
}

impl Savable for VideoTiming {
    fn save(&self, w: &mut StateWriter) {
        w.write_u16(self.dispcnt);
        w.write_u16(self.dispstat);
        w.write_u16(self.vcount);
        w.write_u32(self.dot_cycle);
    }

    fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        self.dispcnt = r.read_u16()?;
        self.dispstat = r.read_u16()?;
        self.vcount = r.read_u16()?;
        self.dot_cycle = r.read_u32()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
