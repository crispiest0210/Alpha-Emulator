//! Windows and colour blending: which layers are visible where, and how they combine.
//!
//! # A window is a per-pixel layer mask, not a clip rectangle
//!
//! Inside a window, a *different set of layers* is drawn. That is not the same as clipping: a
//! game uses window 0 to show only the background and sprites belonging to a status bar, while
//! outside it the world draws normally. Implementing it as a rectangle that hides everything
//! would blank the status bar rather than filter it.
//!
//! There are three windows and they are checked in a fixed order — window 0, then window 1,
//! then the object window — with the first match winning. A pixel in none of them uses the
//! "outside" set. The order is why a game can nest a small window inside a larger one.
//!
//! # A window's edges can be inside out
//!
//! Each boundary register holds two 8-bit coordinates, and the right one may be *smaller* than
//! the left. Hardware does not treat that as an empty window: the region wraps around the edge
//! of the screen. Games use it deliberately for a band that straddles the edge, so clamping it
//! to empty loses the effect entirely.
//!
//! # Blending has three modes and one of them is not a blend
//!
//! Alpha blending mixes two layers. Brighten and darken take one layer toward white or black.
//! The fourth setting is off. All four share the same layer-selection registers, so a game
//! switching between them does not have to rewrite which layers take part.

use core_common::{Rgba8, Savable, StateError, StateReader, StateWriter};

/// Register addresses.
pub mod reg {
    pub const WIN0H: u32 = 0x0400_0040;
    pub const WIN1H: u32 = 0x0400_0042;
    pub const WIN0V: u32 = 0x0400_0044;
    pub const WIN1V: u32 = 0x0400_0046;
    pub const WININ: u32 = 0x0400_0048;
    pub const WINOUT: u32 = 0x0400_004A;
    pub const MOSAIC: u32 = 0x0400_004C;
    pub const BLDCNT: u32 = 0x0400_0050;
    pub const BLDALPHA: u32 = 0x0400_0052;
    pub const BLDY: u32 = 0x0400_0054;
}

/// Which layer a pixel came from, for window and blend selection.
///
/// Numbered as the hardware numbers them in `WININ`, `WINOUT`, and `BLDCNT`, so one bit index
/// serves all three.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layer {
    Bg0,
    Bg1,
    Bg2,
    Bg3,
    Object,
    Backdrop,
}

impl Layer {
    pub const fn bit(self) -> u16 {
        1 << self as u16
    }

    /// Bit 5 of `WININ`/`WINOUT` is **not** a sixth layer: it says whether colour special effects
    /// apply at all inside that region. It shares its position with [`Layer::Backdrop`], which is a
    /// real target in `BLDCNT` — the same bit means two different things in two register sets.
    ///
    /// Ignoring it made every effect apply everywhere. A game that darkens the world behind a menu
    /// masks the effect off inside the menu's window; without that, the menu is darkened too, and
    /// its panels come out grey instead of white.
    pub const COLOUR_EFFECT: u16 = 1 << 5;

    pub const fn background(index: usize) -> Self {
        match index {
            0 => Layer::Bg0,
            1 => Layer::Bg1,
            2 => Layer::Bg2,
            _ => Layer::Bg3,
        }
    }
}

/// How two layers combine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlendMode {
    None,
    /// Mix the top layer with the one under it.
    Alpha,
    /// Take the top layer toward white.
    Brighten,
    /// Take the top layer toward black.
    Darken,
}

/// The window and blending registers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Effects {
    win0h: u16,
    win1h: u16,
    win0v: u16,
    win1v: u16,
    winin: u16,
    winout: u16,
    pub mosaic: u16,
    bldcnt: u16,
    bldalpha: u16,
    bldy: u16,
}

impl Effects {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn owns(addr: u32) -> bool {
        (reg::WIN0H..reg::BLDY + 2).contains(&addr)
    }

    /// Background mosaic block size in pixels, `(horizontal, vertical)`.
    ///
    /// Bits 0-3 and 4-7 of `MOSAIC`, each a size *minus one* — field zero is block size one,
    /// which is the identity: quantizing a coordinate to a block of one pixel changes nothing, so
    /// mosaic being "on" with a zero field is indistinguishable from being off.
    pub fn bg_mosaic_size(&self) -> (u32, u32) {
        (
            (self.mosaic & 0xF) as u32 + 1,
            ((self.mosaic >> 4) & 0xF) as u32 + 1,
        )
    }

    /// Object mosaic block size in pixels, `(horizontal, vertical)`.
    ///
    /// A genuinely separate field from [`Self::bg_mosaic_size`], bits 8-11 and 12-15 of the same
    /// register — the two block sizes are not required to match.
    pub fn obj_mosaic_size(&self) -> (u32, u32) {
        (
            ((self.mosaic >> 8) & 0xF) as u32 + 1,
            ((self.mosaic >> 12) & 0xF) as u32 + 1,
        )
    }

    /// Which layers may draw at this pixel, given which windows are enabled.
    ///
    /// `enabled` is the three window-enable bits of `DISPCNT`, in the order window 0, window 1,
    /// object window. With none of them on, every layer draws — the window registers are simply
    /// not consulted, which is what a game that never uses them relies on.
    pub fn visible_layers(
        &self,
        x: u32,
        y: u32,
        enabled: [bool; 3],
        in_object_window: bool,
    ) -> u16 {
        if !enabled[0] && !enabled[1] && !enabled[2] {
            return u16::MAX;
        }
        // First match wins, which is what lets a game nest a small window inside a larger one.
        if enabled[0] && self.inside(self.win0h, self.win0v, x, y) {
            return self.winin & 0x3F;
        }
        if enabled[1] && self.inside(self.win1h, self.win1v, x, y) {
            return (self.winin >> 8) & 0x3F;
        }
        if enabled[2] && in_object_window {
            return (self.winout >> 8) & 0x3F;
        }
        self.winout & 0x3F
    }

    /// Whether a pixel is inside a window's bounds.
    fn inside(&self, horizontal: u16, vertical: u16, x: u32, y: u32) -> bool {
        let (left, right) = ((horizontal >> 8) as u32, (horizontal & 0xFF) as u32);
        let (top, bottom) = ((vertical >> 8) as u32, (vertical & 0xFF) as u32);
        within(x, left, right) && within(y, top, bottom)
    }

    /// The four window-bound registers, for diagnostics. They are write-only to a game.
    pub fn window_bounds(&self) -> (u16, u16, u16, u16) {
        (self.win0h, self.win1h, self.win0v, self.win1v)
    }

    pub fn blend_mode(&self) -> BlendMode {
        match (self.bldcnt >> 6) & 3 {
            1 => BlendMode::Alpha,
            2 => BlendMode::Brighten,
            3 => BlendMode::Darken,
            _ => BlendMode::None,
        }
    }

    /// Whether a layer is a blend source — the one that gets modified.
    pub fn is_first_target(&self, layer: Layer) -> bool {
        self.bldcnt & layer.bit() != 0
    }

    /// Whether a layer is what an alpha blend mixes *into*.
    pub fn is_second_target(&self, layer: Layer) -> bool {
        (self.bldcnt >> 8) & layer.bit() != 0
    }

    /// Apply the configured effect to a pixel.
    ///
    /// `under` is the colour of whatever is behind it, needed only by an alpha blend. The
    /// weights are 5-bit fractions of 16, and both saturate at 16 — so a game can set a weight
    /// above 1.0 to brighten while blending, and clamping them to 1.0 would lose that.
    pub fn blend(&self, mode: BlendMode, top: Rgba8, under: Rgba8) -> Rgba8 {
        match mode {
            BlendMode::None => top,
            BlendMode::Alpha => {
                let eva = ((self.bldalpha & 0x1F) as u32).min(16);
                let evb = (((self.bldalpha >> 8) & 0x1F) as u32).min(16);
                let mix =
                    |a: u8, b: u8| (((a as u32 * eva) + (b as u32 * evb)) / 16).min(255) as u8;
                Rgba8 {
                    r: mix(top.r, under.r),
                    g: mix(top.g, under.g),
                    b: mix(top.b, under.b),
                    a: 0xFF,
                }
            }
            BlendMode::Brighten => {
                let evy = ((self.bldy & 0x1F) as u32).min(16);
                let up = |c: u8| (c as u32 + ((255 - c as u32) * evy) / 16).min(255) as u8;
                Rgba8 {
                    r: up(top.r),
                    g: up(top.g),
                    b: up(top.b),
                    a: 0xFF,
                }
            }
            BlendMode::Darken => {
                let evy = ((self.bldy & 0x1F) as u32).min(16);
                let down = |c: u8| (c as u32 - (c as u32 * evy) / 16) as u8;
                Rgba8 {
                    r: down(top.r),
                    g: down(top.g),
                    b: down(top.b),
                    a: 0xFF,
                }
            }
        }
    }

    pub fn read16(&self, addr: u32) -> Option<u16> {
        Some(match addr {
            // The boundary and mosaic registers are write-only.
            reg::WIN0H | reg::WIN1H | reg::WIN0V | reg::WIN1V | reg::MOSAIC => 0,
            reg::WININ => self.winin,
            reg::WINOUT => self.winout,
            reg::BLDCNT => self.bldcnt,
            reg::BLDALPHA => self.bldalpha,
            // `BLDY` is write-only too, unlike the two registers beside it.
            reg::BLDY => 0,
            _ => return None,
        })
    }

    pub fn write16(&mut self, addr: u32, value: u16) -> Option<()> {
        match addr {
            reg::WIN0H => self.win0h = value,
            reg::WIN1H => self.win1h = value,
            reg::WIN0V => self.win0v = value,
            reg::WIN1V => self.win1v = value,
            reg::WININ => self.winin = value & 0x3F3F,
            reg::WINOUT => self.winout = value & 0x3F3F,
            reg::MOSAIC => self.mosaic = value,
            reg::BLDCNT => self.bldcnt = value & 0x3FFF,
            reg::BLDALPHA => self.bldalpha = value & 0x1F1F,
            reg::BLDY => self.bldy = value & 0x1F,
            _ => return None,
        }
        Some(())
    }
}

/// Whether a coordinate falls within a window boundary.
///
/// The end may be smaller than the start, and hardware does not treat that as empty — the
/// region wraps around the edge of the screen. Games use it for a band that straddles the edge,
/// so clamping to empty loses the effect entirely.
fn within(value: u32, start: u32, end: u32) -> bool {
    if start <= end {
        value >= start && value < end
    } else {
        value >= start || value < end
    }
}

impl Savable for Effects {
    fn save(&self, w: &mut StateWriter) {
        for value in [
            self.win0h,
            self.win1h,
            self.win0v,
            self.win1v,
            self.winin,
            self.winout,
            self.mosaic,
            self.bldcnt,
            self.bldalpha,
            self.bldy,
        ] {
            w.write_u16(value);
        }
    }

    fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        self.win0h = r.read_u16()?;
        self.win1h = r.read_u16()?;
        self.win0v = r.read_u16()?;
        self.win1v = r.read_u16()?;
        self.winin = r.read_u16()?;
        self.winout = r.read_u16()?;
        self.mosaic = r.read_u16()?;
        self.bldcnt = r.read_u16()?;
        self.bldalpha = r.read_u16()?;
        self.bldy = r.read_u16()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
