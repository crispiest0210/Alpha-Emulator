//! The two 2D engines, A and B.
//!
//! # One engine with a capability flag, not two engines
//!
//! Prompt 13 asks for this to be "a capability difference on top of a shared engine
//! implementation … to the extent that's actually true to the hardware", and it is true to a
//! greater extent than the register map suggests. Engine A and engine B run the same background
//! modes, the same sprite hardware, the same window and blend units, and the same master
//! brightness. Everything that differs is expressible as a property of [`Engine`]:
//!
//! - **Which VRAM spaces they read.** A reads `BgA`/`ObjA`, B reads `BgB`/`ObjB`, and their
//!   extended palettes likewise. That is one method, not a fork.
//! - **What A has that B does not.** BG0 can be the 3D layer, background mode 6 exists, display
//!   modes 2 and 3 exist, and `DISPCNT`'s character- and screen-base offset fields exist. All
//!   four are checked against `self.engine`.
//! - **How large a tile-data window they address.** A's background space is 512 KiB, B's 128 KiB.
//!
//! So this is prompt 11's `GbModel` pattern applied again: one implementation, one save-state
//! shape, one place a rendering fix has to land.
//!
//! # Colour, not indices, once a layer is drawn
//!
//! `ppu-tile2d` keeps palette indices until a line is complete, and for the Game Boy that is a
//! correctness requirement — sprite priority is decided against the background's raw index. The
//! DS is not like that. Its priorities are explicit per layer, its blend unit operates on 15-bit
//! colours, and two of its background types produce direct colour with no palette at all. So each
//! layer here renders to 15-bit colour plus an opacity flag, and the compositor blends colours.
//!
//! That is a departure from the principle, made deliberately: keeping indices would mean carrying
//! "which of five palette sources was this?" alongside every pixel and resolving it inside the
//! blend unit, which is the same work in a less obvious place.
//!
//! # What is not implemented
//!
//! - **Mosaic**, on backgrounds and sprites alike, exactly as on the GBA.
//! - **Display mode 3**, main-memory display, which needs the capture unit.

mod background;
mod objects;

use crate::video::SCREEN_WIDTH;
use crate::vram::{Vram, VramSpace};
use core_common::{Rgba8, Savable, StateError, StateReader, StateWriter};
use ppu_tile2d::bgr555_to_rgba;

pub use background::BackgroundKind;

/// Which of the two engines. Everything that differs between them is reachable from here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Engine {
    /// The main engine: 3D compositing, mode 6, the VRAM display modes, and a 512 KiB
    /// background window.
    A,
    /// The sub engine: everything else A has, over 128 KiB.
    B,
}

impl Engine {
    /// Base of this engine's register block.
    pub const fn base(self) -> u32 {
        match self {
            Engine::A => 0x0400_0000,
            Engine::B => 0x0400_1000,
        }
    }

    pub const fn bg_space(self) -> VramSpace {
        match self {
            Engine::A => VramSpace::BgA,
            Engine::B => VramSpace::BgB,
        }
    }

    pub const fn obj_space(self) -> VramSpace {
        match self {
            Engine::A => VramSpace::ObjA,
            Engine::B => VramSpace::ObjB,
        }
    }

    pub const fn bg_ext_pal_space(self) -> VramSpace {
        match self {
            Engine::A => VramSpace::BgExtPalA,
            Engine::B => VramSpace::BgExtPalB,
        }
    }

    pub const fn obj_ext_pal_space(self) -> VramSpace {
        match self {
            Engine::A => VramSpace::ObjExtPalA,
            Engine::B => VramSpace::ObjExtPalB,
        }
    }

    /// Byte offset of this engine's palettes within the 2 KiB palette RAM, and likewise its OAM.
    pub const fn block_offset(self) -> usize {
        match self {
            Engine::A => 0,
            Engine::B => 0x400,
        }
    }
}

/// Register offsets from an engine's base.
pub mod reg {
    pub const DISPCNT: u32 = 0x000;
    pub const BGCNT: u32 = 0x008;
    pub const BGOFS: u32 = 0x010;
    pub const BG2PA: u32 = 0x020;
    pub const BG3PA: u32 = 0x030;
    pub const WIN0H: u32 = 0x040;
    pub const WININ: u32 = 0x048;
    pub const MOSAIC: u32 = 0x04C;
    pub const BLDCNT: u32 = 0x050;
    pub const BLDALPHA: u32 = 0x052;
    pub const BLDY: u32 = 0x054;
    pub const MASTER_BRIGHT: u32 = 0x06C;
    /// One past the last register either engine answers for.
    pub const END: u32 = 0x070;
}

mod dispcnt {
    pub const MODE: u32 = 0x7;
    pub const BG0_IS_3D: u32 = 1 << 3;
    pub const OBJ_1D_MAPPING: u32 = 1 << 4;
    pub const OBJ_BITMAP_WIDE: u32 = 1 << 5;
    pub const OBJ_BITMAP_1D: u32 = 1 << 6;
    pub const FORCED_BLANK: u32 = 1 << 7;
    pub const BG_ENABLE: u32 = 0xF << 8;
    pub const OBJ_ENABLE: u32 = 1 << 12;
    pub const WIN0_ENABLE: u32 = 1 << 13;
    pub const WIN1_ENABLE: u32 = 1 << 14;
    pub const OBJ_WINDOW_ENABLE: u32 = 1 << 15;
    pub const DISPLAY_MODE: u32 = 0x3 << 16;
    pub const VRAM_BLOCK: u32 = 0x3 << 18;
    pub const OBJ_1D_BOUNDARY: u32 = 0x3 << 20;
    pub const OBJ_BITMAP_1D_BOUNDARY: u32 = 1 << 22;
    pub const CHAR_BASE: u32 = 0x7 << 24;
    pub const SCREEN_BASE: u32 = 0x7 << 27;
    pub const BG_EXT_PALETTE: u32 = 1 << 30;
    pub const OBJ_EXT_PALETTE: u32 = 1 << 31;
}

/// One pixel of one layer, before compositing.
///
/// 15-bit BGR because that is what the blend unit works in; converting to 8-bit RGBA first and
/// blending there rounds differently from hardware on every blended pixel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct LinePixel {
    pub color: u16,
    pub opaque: bool,
    /// Lower is in front. Per pixel rather than per layer because a sprite's priority comes from
    /// its OAM entry, so two sprites on one line can sit either side of a background.
    pub priority: u8,
    /// A semi-transparent sprite forces alpha blending on wherever it lands, whatever `BLDCNT`
    /// selects. Only sprites set this.
    pub semi_transparent: bool,
}

/// Layer indices used by `BLDCNT`, `WININ`, and `WINOUT` alike.
const LAYER_OBJ: usize = 4;
const LAYER_BACKDROP: usize = 5;
const LAYERS: usize = 5;

/// One 2D engine.
pub struct Engine2d {
    engine: Engine,
    dispcnt: u32,
    bgcnt: [u16; 4],
    bghofs: [u16; 4],
    bgvofs: [u16; 4],
    /// Affine parameters for BG2 and BG3, indexed 0 and 1.
    bgp: [[i16; 4]; 2],
    bgx: [i32; 2],
    bgy: [i32; 2],
    /// The running reference point, reloaded from `bgx`/`bgy` at the top of each frame and
    /// advanced by one row of `pb`/`pd` per scanline. Separate from the written value because a
    /// game that writes `BG2X` mid-frame expects the change to take effect from the *next* line,
    /// not to be undone by the line after.
    bgx_internal: [i32; 2],
    bgy_internal: [i32; 2],
    winh: [u16; 2],
    winv: [u16; 2],
    winin: u16,
    winout: u16,
    mosaic: u16,
    bldcnt: u16,
    bldalpha: u16,
    bldy: u16,
    master_bright: u16,

    /// Per-layer scanlines, reused every line so nothing allocates during a frame.
    layers: [Box<[LinePixel]>; LAYERS],
    /// Which pixels the sprite pass marked as inside the object window.
    obj_window: Box<[bool]>,
    /// Whether each window's vertical range covers the line being drawn.
    ///
    /// Recomputed once per line by [`Engine2d::set_window_rows`] rather than per pixel, because
    /// the vertical test cannot vary across a line.
    window_row: [bool; 2],
}

impl Engine2d {
    pub fn new(engine: Engine) -> Self {
        Self {
            engine,
            dispcnt: 0,
            bgcnt: [0; 4],
            bghofs: [0; 4],
            bgvofs: [0; 4],
            bgp: [[0; 4]; 2],
            bgx: [0; 2],
            bgy: [0; 2],
            bgx_internal: [0; 2],
            bgy_internal: [0; 2],
            winh: [0; 2],
            winv: [0; 2],
            winin: 0,
            winout: 0,
            mosaic: 0,
            bldcnt: 0,
            bldalpha: 0,
            bldy: 0,
            master_bright: 0,
            layers: std::array::from_fn(|_| {
                vec![LinePixel::default(); SCREEN_WIDTH as usize].into_boxed_slice()
            }),
            obj_window: vec![false; SCREEN_WIDTH as usize].into_boxed_slice(),
            window_row: [false; 2],
        }
    }

    pub fn engine(&self) -> Engine {
        self.engine
    }

    pub fn dispcnt(&self) -> u32 {
        self.dispcnt
    }

    /// Whether this address is one of this engine's registers.
    pub fn owns(&self, addr: u32) -> bool {
        let base = self.engine.base();
        (base..base + reg::END).contains(&addr)
    }

    /// Reload the affine reference points, which hardware does once per frame.
    pub fn on_frame_start(&mut self) {
        self.bgx_internal = self.bgx;
        self.bgy_internal = self.bgy;
    }

    /// Advance the affine reference points by one scanline.
    ///
    /// Called after each visible line is drawn, not before, so line 0 uses the reference point
    /// exactly as written.
    pub fn on_line_end(&mut self) {
        for i in 0..2 {
            self.bgx_internal[i] = self.bgx_internal[i].wrapping_add(self.bgp[i][1] as i32);
            self.bgy_internal[i] = self.bgy_internal[i].wrapping_add(self.bgp[i][3] as i32);
        }
    }

    /// Draw one visible scanline into `out`, which is `SCREEN_WIDTH * 4` bytes of RGBA.
    pub fn render_line(
        &mut self,
        line: u32,
        vram: &Vram,
        palette: &[u8],
        oam: &[u8],
        out: &mut [u8],
    ) {
        self.render_line_with_3d(line, vram, palette, oam, None, out)
    }

    /// The same, with the 3D core's output available for BG0.
    ///
    /// `three_d` is `Some` only for engine A with `DISPCNT` bit 3 set and the 3D core enabled.
    /// When it is `None` — engine B, or the layer switched off — BG0 draws nothing and the
    /// backdrop shows through, which is what the layer looks like with no 3D behind it.
    pub fn render_line_with_3d(
        &mut self,
        line: u32,
        vram: &Vram,
        palette: &[u8],
        oam: &[u8],
        three_d: Option<&crate::gpu3d::render::Framebuffer3d>,
        out: &mut [u8],
    ) {
        // Display mode 0 is the display being off, which shows white rather than black. A game
        // uses it during boot, and rendering it as black makes the console look broken.
        let display_mode = (self.dispcnt & dispcnt::DISPLAY_MODE) >> 16;
        if display_mode == 0 {
            fill(out, Rgba8::rgb(255, 255, 255));
            return;
        }
        if self.dispcnt & dispcnt::FORCED_BLANK != 0 {
            fill(out, Rgba8::rgb(255, 255, 255));
            return;
        }
        match (display_mode, self.engine) {
            (2, Engine::A) => {
                self.render_vram_display(line, vram, out);
                return;
            }
            (3, Engine::A) => {
                // Main-memory display needs the capture unit, which does not exist. Left blank
                // rather than approximated.
                fill(out, Rgba8::rgb(0, 0, 0));
                return;
            }
            _ => {}
        }

        for layer in &mut self.layers {
            layer.fill(LinePixel::default());
        }
        self.obj_window.fill(false);
        self.set_window_rows(line);

        if self.dispcnt & dispcnt::OBJ_ENABLE != 0 {
            self.render_objects(line, vram, palette, oam);
        }
        let enabled = (self.dispcnt & dispcnt::BG_ENABLE) >> 8;
        for bg in 0..4usize {
            if enabled & (1 << bg) == 0 {
                continue;
            }
            if bg == 0 && self.engine == Engine::A && self.dispcnt & dispcnt::BG0_IS_3D != 0 {
                if let Some(frame) = three_d {
                    self.render_3d_layer(line, frame);
                }
                continue;
            }
            self.render_background(bg, line, vram, palette);
        }

        self.composite(line, palette, out);
    }

    /// Copy one line of the 3D core's output into BG0.
    ///
    /// The 3D layer's priority is BG0's, and its per-pixel alpha comes from the 3D engine rather
    /// than from `BLDALPHA` — a translucent polygon blends against the 2D layers underneath it
    /// through the ordinary blend unit, which is why the alpha is carried per pixel rather than
    /// resolved inside the rasteriser.
    fn render_3d_layer(&mut self, line: u32, frame: &crate::gpu3d::render::Framebuffer3d) {
        let priority = (self.bgcnt[0] & 3) as u8;
        for x in 0..SCREEN_WIDTH {
            let alpha = frame.alpha_at(x, line);
            if alpha == 0 {
                continue;
            }
            self.layers[0][x as usize] = LinePixel {
                color: frame.color_at(x, line),
                opaque: true,
                priority,
                // A translucent 3D pixel forces the blend on, the same way a semi-transparent
                // sprite does, so the layers beneath it show through without `BLDCNT` selecting
                // the 3D layer as a target.
                semi_transparent: alpha < 31,
            };
        }
    }

    /// Display mode 2: a raw 15-bit framebuffer read straight out of one 128 KiB VRAM bank,
    /// bypassing every layer. Engine A only.
    fn render_vram_display(&self, line: u32, vram: &Vram, out: &mut [u8]) {
        let block = (self.dispcnt & dispcnt::VRAM_BLOCK) >> 18;
        // The banks are addressed through the LCDC window, where A-D sit at 128 KiB intervals.
        let base = block * 0x2_0000 + line * SCREEN_WIDTH * 2;
        for x in 0..SCREEN_WIDTH {
            let color = vram.read16(VramSpace::Lcdc, base + x * 2);
            write_pixel(out, x as usize, self.apply_master_brightness(color));
        }
    }

    /// Resolve every pixel of the line from the layer buffers.
    fn composite(&mut self, _line: u32, palette: &[u8], out: &mut [u8]) {
        let backdrop = read_palette(palette, self.engine.block_offset(), 0);
        let effect = (self.bldcnt >> 6) & 3;

        for x in 0..SCREEN_WIDTH as usize {
            let (enabled, effects_here) = self.window_at(x);

            // Front-most and second-most opaque layers, by priority then by the fixed layer
            // order. Sprites beat a background of equal priority, which is why they are tested
            // first inside each priority level.
            let mut first: Option<(usize, LinePixel)> = None;
            let mut second: Option<(usize, LinePixel)> = None;
            for priority in 0..4u8 {
                for layer in [LAYER_OBJ, 0, 1, 2, 3] {
                    let pixel = self.layers[layer][x];
                    if !pixel.opaque || pixel.priority != priority || !enabled[layer] {
                        continue;
                    }
                    if first.is_none() {
                        first = Some((layer, pixel));
                    } else if second.is_none() {
                        second = Some((layer, pixel));
                    }
                }
                if second.is_some() {
                    break;
                }
            }

            let (top_layer, top) = first.unwrap_or((
                LAYER_BACKDROP,
                LinePixel {
                    color: backdrop,
                    opaque: true,
                    priority: 3,
                    semi_transparent: false,
                },
            ));
            let (below_layer, below_color) = match second {
                Some((layer, pixel)) => (layer, pixel.color),
                None => (LAYER_BACKDROP, backdrop),
            };

            let first_target = self.bldcnt & (1 << top_layer) != 0;
            let second_target = self.bldcnt & (1 << (below_layer + 8)) != 0;

            let color = if top.semi_transparent && second_target {
                // A semi-transparent sprite blends whatever `BLDCNT` says, and does so even
                // where the colour effect is switched off by a window.
                alpha_blend(top.color, below_color, self.bldalpha)
            } else if !effects_here {
                top.color
            } else {
                match effect {
                    1 if first_target && second_target => {
                        alpha_blend(top.color, below_color, self.bldalpha)
                    }
                    2 if first_target => brightness(top.color, self.bldy & 0x1F, true),
                    3 if first_target => brightness(top.color, self.bldy & 0x1F, false),
                    _ => top.color,
                }
            };
            write_pixel(out, x, self.apply_master_brightness(color));
        }
    }

    /// Which layers may draw at this pixel, and whether the colour effect applies there.
    ///
    /// Returns everything enabled when no window is on, which is the common case and the one
    /// worth not paying for.
    fn window_at(&self, x: usize) -> ([bool; LAYERS], bool) {
        let any_window = self.dispcnt
            & (dispcnt::WIN0_ENABLE | dispcnt::WIN1_ENABLE | dispcnt::OBJ_WINDOW_ENABLE)
            != 0;
        if !any_window {
            return ([true; LAYERS], true);
        }
        let control = if self.dispcnt & dispcnt::WIN0_ENABLE != 0 && self.in_window(0, x) {
            self.winin & 0x3F
        } else if self.dispcnt & dispcnt::WIN1_ENABLE != 0 && self.in_window(1, x) {
            (self.winin >> 8) & 0x3F
        } else if self.dispcnt & dispcnt::OBJ_WINDOW_ENABLE != 0 && self.obj_window[x] {
            (self.winout >> 8) & 0x3F
        } else {
            self.winout & 0x3F
        };
        let mut enabled = [false; LAYERS];
        for (layer, slot) in enabled.iter_mut().enumerate() {
            *slot = control & (1 << layer) != 0;
        }
        (enabled, control & 0x20 != 0)
    }

    /// Whether this x is inside window 0 or 1, given the current line's vertical test.
    ///
    /// The vertical test is folded into `set_window_rows` rather than repeated per pixel.
    fn in_window(&self, index: usize, x: usize) -> bool {
        if !self.window_row[index] {
            return false;
        }
        let left = (self.winh[index] >> 8) as usize;
        let right = (self.winh[index] & 0xFF) as usize;
        // A window whose right edge is at or before its left wraps around the screen edge,
        // which is how software makes a window that spans the seam.
        if left <= right {
            x >= left && x < right
        } else {
            x >= left || x < right
        }
    }

    pub fn read32(&self, addr: u32) -> Option<u32> {
        Some(self.read16(addr)? as u32 | ((self.read16(addr + 2)? as u32) << 16))
    }

    pub fn read16(&self, addr: u32) -> Option<u16> {
        if !self.owns(addr) {
            return None;
        }
        let offset = (addr - self.engine.base()) & !1;
        Some(match offset {
            0x000 => self.dispcnt as u16,
            0x002 => (self.dispcnt >> 16) as u16,
            0x008..=0x00E => self.bgcnt[((offset - reg::BGCNT) / 2) as usize],
            // Scroll registers and the affine parameters are write-only, and read as zero.
            0x010..=0x03F => 0,
            0x040..=0x047 => 0,
            0x048 => self.winin,
            0x04A => self.winout,
            0x04C => self.mosaic,
            0x050 => self.bldcnt,
            0x052 => self.bldalpha,
            0x054 => 0,
            0x06C => self.master_bright,
            _ => 0,
        })
    }

    pub fn write16(&mut self, addr: u32, value: u16) -> bool {
        if !self.owns(addr) {
            return false;
        }
        let offset = (addr - self.engine.base()) & !1;
        match offset {
            0x000 => self.set_dispcnt((self.dispcnt & 0xFFFF_0000) | value as u32),
            0x002 => self.set_dispcnt((self.dispcnt & 0xFFFF) | ((value as u32) << 16)),
            0x008..=0x00E => self.bgcnt[((offset - reg::BGCNT) / 2) as usize] = value,
            0x010..=0x01F => {
                let index = ((offset - reg::BGOFS) / 4) as usize;
                if offset & 2 == 0 {
                    self.bghofs[index] = value & 0x1FF;
                } else {
                    self.bgvofs[index] = value & 0x1FF;
                }
            }
            0x020..=0x03F => self.write_affine(offset, value),
            0x040 | 0x042 => self.winh[((offset - reg::WIN0H) / 2) as usize] = value,
            0x044 | 0x046 => self.winv[((offset - reg::WIN0H - 4) / 2) as usize] = value,
            0x048 => self.winin = value & 0x3F3F,
            0x04A => self.winout = value & 0x3F3F,
            0x04C => self.mosaic = value,
            0x050 => self.bldcnt = value & 0x3FFF,
            0x052 => self.bldalpha = value & 0x1F1F,
            0x054 => self.bldy = value & 0x1F,
            0x06C => self.master_bright = value & 0xC01F,
            // DISP3DCNT, DISPCAPCNT, and the main-memory FIFO. Accepted and ignored: neither the
            // 3D core nor the capture unit exists, and silently dropping the write is better
            // than an unmapped-write warning on every frame of a game that sets them once.
            _ => {}
        }
        true
    }

    fn write_affine(&mut self, offset: u32, value: u16) {
        let block = if offset < reg::BG3PA { 0 } else { 1 };
        let within = offset - if block == 0 { reg::BG2PA } else { reg::BG3PA };
        match within {
            0 | 2 | 4 | 6 => self.bgp[block][(within / 2) as usize] = value as i16,
            8 | 10 => {
                let word = splice(self.bgx[block] as u32, within == 10, value);
                self.bgx[block] = sign_extend_28(word);
                self.bgx_internal[block] = self.bgx[block];
            }
            _ => {
                let word = splice(self.bgy[block] as u32, within == 14, value);
                self.bgy[block] = sign_extend_28(word);
                self.bgy_internal[block] = self.bgy[block];
            }
        }
    }

    fn set_dispcnt(&mut self, value: u32) {
        // Engine B has no character or screen base offset, no 3D layer, and only display modes
        // 0 and 1. Masking here rather than at every use keeps the difference in one place.
        self.dispcnt = match self.engine {
            Engine::A => value,
            Engine::B => value & !(dispcnt::CHAR_BASE | dispcnt::SCREEN_BASE | dispcnt::BG0_IS_3D),
        };
        if self.engine == Engine::B {
            // Display mode is one bit wide on engine B.
            self.dispcnt &= !(0x2 << 16);
        }
    }

    pub fn write32(&mut self, addr: u32, value: u32) -> bool {
        self.write16(addr, value as u16) && self.write16(addr + 2, (value >> 16) as u16)
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
        let Some(current) = self.read16(addr & !1) else {
            return false;
        };
        let spliced = if addr & 1 == 0 {
            (current & 0xFF00) | value as u16
        } else {
            (current & 0x00FF) | ((value as u16) << 8)
        };
        self.write16(addr & !1, spliced)
    }

    /// Decide, once per line, whether each window's vertical range contains it.
    pub fn set_window_rows(&mut self, line: u32) {
        for index in 0..2 {
            let top = (self.winv[index] >> 8) as u32;
            let bottom = (self.winv[index] & 0xFF) as u32;
            self.window_row[index] = if top <= bottom {
                line >= top && line < bottom
            } else {
                line >= top || line < bottom
            };
        }
    }

    /// `MASTER_BRIGHT` scales the finished pixel toward white or black, after every other effect
    /// and outside the blend unit entirely. It is the DS's fade-to-black.
    fn apply_master_brightness(&self, color: u16) -> u16 {
        let factor = (self.master_bright & 0x1F).min(16);
        match (self.master_bright >> 14) & 3 {
            1 => brightness(color, factor, true),
            2 => brightness(color, factor, false),
            _ => color,
        }
    }
}

fn splice(current: u32, high: bool, value: u16) -> u32 {
    if high {
        (current & 0xFFFF) | ((value as u32) << 16)
    } else {
        (current & 0xFFFF_0000) | value as u32
    }
}

/// The affine reference points are 28-bit signed with 8 fractional bits.
fn sign_extend_28(value: u32) -> i32 {
    ((value & 0x0FFF_FFFF) << 4) as i32 >> 4
}

fn fill(out: &mut [u8], color: Rgba8) {
    for chunk in out.chunks_exact_mut(4) {
        chunk[0] = color.r;
        chunk[1] = color.g;
        chunk[2] = color.b;
        chunk[3] = color.a;
    }
}

fn write_pixel(out: &mut [u8], x: usize, color: u16) {
    let rgba = bgr555_to_rgba(color);
    let base = x * 4;
    out[base] = rgba.r;
    out[base + 1] = rgba.g;
    out[base + 2] = rgba.b;
    out[base + 3] = rgba.a;
}

/// One 15-bit colour from a palette block.
fn read_palette(palette: &[u8], base: usize, index: usize) -> u16 {
    let offset = base + index * 2;
    let low = palette.get(offset).copied().unwrap_or(0) as u16;
    let high = palette.get(offset + 1).copied().unwrap_or(0) as u16;
    low | (high << 8)
}

/// The blend unit's alpha mix, per channel and saturating at 31.
fn alpha_blend(top: u16, bottom: u16, bldalpha: u16) -> u16 {
    let eva = (bldalpha & 0x1F).min(16) as u32;
    let evb = ((bldalpha >> 8) & 0x1F).min(16) as u32;
    let mut out = 0u16;
    for shift in [0, 5, 10] {
        let a = ((top >> shift) & 0x1F) as u32;
        let b = ((bottom >> shift) & 0x1F) as u32;
        let mixed = ((a * eva + b * evb) / 16).min(31) as u16;
        out |= mixed << shift;
    }
    out
}

/// Brightness increase or decrease, which is a blend against white or black.
fn brightness(color: u16, evy: u16, up: bool) -> u16 {
    let evy = evy.min(16) as u32;
    let mut out = 0u16;
    for shift in [0, 5, 10] {
        let c = ((color >> shift) & 0x1F) as u32;
        let value = if up {
            c + (31 - c) * evy / 16
        } else {
            c - c * evy / 16
        };
        out |= (value as u16) << shift;
    }
    out
}

impl Savable for Engine2d {
    fn save(&self, w: &mut StateWriter) {
        w.write_u32(self.dispcnt);
        for value in self.bgcnt {
            w.write_u16(value);
        }
        for value in self.bghofs {
            w.write_u16(value);
        }
        for value in self.bgvofs {
            w.write_u16(value);
        }
        for block in self.bgp {
            for value in block {
                w.write_i16(value);
            }
        }
        for i in 0..2 {
            w.write_i32(self.bgx[i]);
            w.write_i32(self.bgy[i]);
            w.write_i32(self.bgx_internal[i]);
            w.write_i32(self.bgy_internal[i]);
        }
        for value in self.winh {
            w.write_u16(value);
        }
        for value in self.winv {
            w.write_u16(value);
        }
        for value in [
            self.winin,
            self.winout,
            self.mosaic,
            self.bldcnt,
            self.bldalpha,
            self.bldy,
            self.master_bright,
        ] {
            w.write_u16(value);
        }
    }

    fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        self.dispcnt = r.read_u32()?;
        for value in &mut self.bgcnt {
            *value = r.read_u16()?;
        }
        for value in &mut self.bghofs {
            *value = r.read_u16()?;
        }
        for value in &mut self.bgvofs {
            *value = r.read_u16()?;
        }
        for block in &mut self.bgp {
            for value in block {
                *value = r.read_i16()?;
            }
        }
        for i in 0..2 {
            self.bgx[i] = r.read_i32()?;
            self.bgy[i] = r.read_i32()?;
            self.bgx_internal[i] = r.read_i32()?;
            self.bgy_internal[i] = r.read_i32()?;
        }
        for value in &mut self.winh {
            *value = r.read_u16()?;
        }
        for value in &mut self.winv {
            *value = r.read_u16()?;
        }
        self.winin = r.read_u16()?;
        self.winout = r.read_u16()?;
        self.mosaic = r.read_u16()?;
        self.bldcnt = r.read_u16()?;
        self.bldalpha = r.read_u16()?;
        self.bldy = r.read_u16()?;
        self.master_bright = r.read_u16()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
