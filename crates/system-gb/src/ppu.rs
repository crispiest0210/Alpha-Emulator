//! The Game Boy PPU: three layers composited one scanline at a time.
//!
//! The DMG has a background, a window, and sprites — **not** the two background layers later
//! systems have. The window is not a second background: it is the same tilemap machinery
//! drawn from its own origin over part of the line, with its own line counter that only
//! advances on lines where it was actually visible.
//!
//! # Why scanline, not frame
//!
//! Each line is composited when its drawing period ends, using the register values at that
//! instant. Games depend on this. A status bar that stays put while the world scrolls beneath
//! it is done by rewriting `SCX`/`SCY` partway down the frame, and a renderer that batched
//! everything at VBlank would draw the whole screen with whichever value happened to be last.
//! The timing module raises the events; this module answers them.
//!
//! # Register ownership
//!
//! `LCDC` is read by both this module and [`crate::timing`] — the PPU needs the layer-enable
//! bits, the timing state machine needs the LCD-enable bit. The system assembly routes a
//! write to both. That is deliberate duplication of a *read*, not of state: neither module
//! stores the other's fields.

use crate::attributes::TileAttributes;
use crate::memory::{self, GbModel};
use core_common::{Framebuffer, Rgba8, Savable, StateError, StateReader, StateWriter};
use ppu_tile2d::{
    render_sprites, render_text_background, BackgroundParams, BitDepth, MonochromePalette,
    PaletteSource, ScanlineBuffer, Sprite, TileRef, TilemapSource, DMG_SHADES,
};

pub const SCREEN_WIDTH: u32 = 160;
pub const SCREEN_HEIGHT: u32 = 144;

/// The most sprites the hardware can fetch for one line.
///
/// The limit is a property of the OAM scan, which runs out of time: the first ten candidates
/// in OAM order win and the rest are simply not drawn. Games exploit it deliberately to hide
/// sprites, so raising it would be a visible inaccuracy, not a generosity.
pub const MAX_SPRITES_PER_LINE: usize = 10;

/// `LCDC` bit assignments.
pub mod lcdc {
    pub const BG_ENABLE: u8 = 1 << 0;
    pub const OBJ_ENABLE: u8 = 1 << 1;
    /// Set selects 8x16 sprites.
    pub const OBJ_TALL: u8 = 1 << 2;
    /// Clear selects the tilemap at `0x9800`, set the one at `0x9C00`.
    pub const BG_MAP_HIGH: u8 = 1 << 3;
    /// Clear selects signed tile addressing based at `0x9000`.
    pub const TILE_DATA_LOW: u8 = 1 << 4;
    pub const WINDOW_ENABLE: u8 = 1 << 5;
    pub const WINDOW_MAP_HIGH: u8 = 1 << 6;
    pub const LCD_ENABLE: u8 = 1 << 7;
}

/// PPU register addresses.
pub mod reg {
    pub const LCDC: u16 = 0xFF40;
    pub const SCY: u16 = 0xFF42;
    pub const SCX: u16 = 0xFF43;
    pub const BGP: u16 = 0xFF47;
    pub const OBP0: u16 = 0xFF48;
    pub const OBP1: u16 = 0xFF49;
    pub const WY: u16 = 0xFF4A;
    pub const WX: u16 = 0xFF4B;
}

/// VRAM offsets, relative to the base of the region rather than to `0x8000`.
mod vram {
    /// Signed tile addressing is based here, in the middle of the tile-data region.
    pub const SIGNED_TILE_BASE: i32 = 0x1000;
    pub const MAP_LOW: usize = 0x1800;
    pub const MAP_HIGH: usize = 0x1C00;
}

/// A Game Boy tilemap: 32x32 cells of one byte each, naming a tile.
///
/// The tile *number* to *address* translation is the interesting part, and it is why
/// [`TileRef`] carries a byte offset rather than an index. With `LCDC.4` set, numbers are
/// unsigned from the bottom of tile data. With it clear, they are **signed** from the middle,
/// so tile 0 sits at `0x9000` and tile 255 sits below it at `0x8FF0`.
struct GbTilemap<'a> {
    vram: &'a [u8],
    map_base: usize,
    signed_tiles: bool,
    /// Read the CGB attribute byte that sits beside each map cell in VRAM bank 1.
    ///
    /// Off for a DMG *and* for a CGB running a DMG cartridge: in both cases nothing ever wrote
    /// that bank, so decoding it would turn uninitialised memory into palette and flip bits.
    attributes: bool,
}

impl TilemapSource for GbTilemap<'_> {
    fn tile_at(&self, tile_x: u32, tile_y: u32) -> TileRef {
        let cell = self.map_base + (tile_y * 32 + tile_x) as usize;
        let number = self.vram.get(cell).copied().unwrap_or(0);
        let data_offset = if self.signed_tiles {
            (vram::SIGNED_TILE_BASE + (number as i8 as i32) * 16) as usize
        } else {
            number as usize * 16
        };

        if !self.attributes {
            return TileRef {
                data_offset,
                ..Default::default()
            };
        }

        // The attribute byte lives at the same offset one bank up — the two banks are parallel
        // views of the same map, which is why one index serves both.
        let raw = self
            .vram
            .get(cell + memory::VRAM_BANK_SIZE)
            .copied()
            .unwrap_or(0);
        let attributes = TileAttributes::from_byte(raw);
        TileRef {
            // A tile's *pixels* can also live in the second bank, independently of where its
            // attribute byte does.
            data_offset: data_offset + (attributes.bank as usize) * memory::VRAM_BANK_SIZE,
            palette: attributes.palette,
            flip_x: attributes.flip_x,
            flip_y: attributes.flip_y,
            // `TileRef` counts priority with lower in front, the opposite sense to the
            // hardware bit, which asks to be drawn *over* sprites.
            priority: u8::from(!attributes.priority),
        }
    }
}

/// The DMG picture processing unit.
#[derive(Debug, Clone)]
pub struct GbPpu {
    pub lcdc: u8,
    pub scy: u8,
    pub scx: u8,
    pub wy: u8,
    pub wx: u8,
    pub palette: MonochromePalette,

    framebuffer: Framebuffer,
    scanline: ScanlineBuffer,

    /// The window's own line counter.
    ///
    /// It advances only on lines where the window was actually drawn, not with `LY`. A game
    /// that switches the window off for part of the frame and back on resumes from where it
    /// left off rather than jumping — driving the window from `LY - WY` instead is a common
    /// shortcut that breaks exactly those games.
    window_line: u8,
}

impl Default for GbPpu {
    fn default() -> Self {
        Self::new()
    }
}

impl GbPpu {
    pub fn new() -> Self {
        Self {
            lcdc: lcdc::LCD_ENABLE | lcdc::BG_ENABLE,
            scy: 0,
            scx: 0,
            wy: 0,
            wx: 0,
            palette: MonochromePalette::new(),
            framebuffer: Framebuffer::new(SCREEN_WIDTH, SCREEN_HEIGHT),
            scanline: ScanlineBuffer::new(SCREEN_WIDTH as usize),
            window_line: 0,
        }
    }

    pub fn reset(&mut self) {
        *self = Self::new();
    }

    pub fn framebuffer(&self) -> &Framebuffer {
        &self.framebuffer
    }

    #[inline]
    fn has(&self, bit: u8) -> bool {
        self.lcdc & bit != 0
    }

    /// Called at the top of each frame, when `LY` wraps back to zero.
    pub fn begin_frame(&mut self) {
        self.window_line = 0;
    }

    /// Composite one scanline into the framebuffer.
    ///
    /// `vram` and `oam` are the raw regions. The PPU reads them directly rather than through
    /// the CPU bus, which is what the hardware does — and it is why the CPU is locked out of
    /// both during mode 3.
    pub fn render_scanline(&mut self, line: u8, vram: &[u8], oam: &[u8]) {
        let palette = self.palette;
        self.render_scanline_with(GbModel::Dmg, line, vram, oam, &palette);
    }

    /// Composite one scanline, looking colour up somewhere other than this PPU's own
    /// monochrome registers.
    ///
    /// The CGB resolves the same indexed pixels through its palette RAM, which lives in
    /// `system-gbc` — a crate that depends on this one, so this PPU cannot name the type. It
    /// does not need to: prompt 08 made [`ScanlineBuffer`] hold palette *indices* until the
    /// line is complete precisely so the lookup could be swapped, and
    /// [`PaletteSource`] is the swap point. Everything before it — tile fetch, window line
    /// counter, sprite priority — is identical on both machines and is not duplicated.
    ///
    /// `model` is separate from the palette because one thing genuinely differs earlier than
    /// the lookup: see [`GbModel::bg_enable_blanks_background`].
    pub fn render_scanline_with(
        &mut self,
        model: GbModel,
        line: u8,
        vram: &[u8],
        oam: &[u8],
        palettes: &dyn PaletteSource,
    ) {
        if line as u32 >= SCREEN_HEIGHT {
            return;
        }
        self.scanline.clear();

        // On a CGB the background always draws; `LCDC` bit 0 only drops its priority.
        let draw_background = self.has(lcdc::BG_ENABLE) || !model.bg_enable_blanks_background();
        if draw_background {
            let attributes = model.uses_tile_attributes();
            self.render_background(line, vram, attributes);
            self.render_window(line, vram, attributes);
        }
        if self.has(lcdc::OBJ_ENABLE) {
            self.render_sprites_for_line(line, vram, oam);
        }

        // With the background blanked a DMG shows white, not the palette's colour 0: the layer
        // is off, so `BGP` does not apply to it at all.
        let backdrop = if draw_background {
            palettes.lookup_bg(0, 0)
        } else {
            DMG_SHADES[0]
        };
        let row = self.framebuffer.row_mut(line as u32);
        self.scanline.resolve_into(palettes, backdrop, row);
    }

    fn render_background(&mut self, line: u8, vram: &[u8], attributes: bool) {
        let map = GbTilemap {
            vram,
            map_base: if self.has(lcdc::BG_MAP_HIGH) {
                vram::MAP_HIGH
            } else {
                vram::MAP_LOW
            },
            signed_tiles: !self.has(lcdc::TILE_DATA_LOW),
            attributes,
        };
        let params = BackgroundParams::full_line(
            line as u32,
            self.scx as u32,
            self.scy as u32,
            BitDepth::Two,
        );
        render_text_background(&map, vram, &params, &mut self.scanline);
    }

    fn render_window(&mut self, line: u8, vram: &[u8], attributes: bool) {
        if !self.has(lcdc::WINDOW_ENABLE) || line < self.wy {
            return;
        }
        // `WX` is offset by 7, so a value of 7 puts the window's left edge at screen x = 0.
        // Values below 7 push it off the left, which hardware handles by clipping.
        let start_x = (self.wx as i32 - 7).max(0) as u32;
        if start_x >= SCREEN_WIDTH {
            return;
        }

        let map = GbTilemap {
            vram,
            map_base: if self.has(lcdc::WINDOW_MAP_HIGH) {
                vram::MAP_HIGH
            } else {
                vram::MAP_LOW
            },
            signed_tiles: !self.has(lcdc::TILE_DATA_LOW),
            attributes,
        };
        let params = BackgroundParams {
            // The window scrolls with its own counter, not with LY or SCY.
            line: self.window_line as u32,
            scroll_x: 0,
            scroll_y: 0,
            map_width: 32,
            map_height: 32,
            depth: BitDepth::Two,
            start_x: start_x as usize,
            origin_x: start_x,
        };
        render_text_background(&map, vram, &params, &mut self.scanline);

        // Only a line the window actually appeared on advances its counter.
        self.window_line = self.window_line.saturating_add(1);
    }

    fn render_sprites_for_line(&mut self, line: u8, vram: &[u8], oam: &[u8]) {
        let height: u32 = if self.has(lcdc::OBJ_TALL) { 16 } else { 8 };

        // The OAM scan takes the first ten candidates in OAM order and stops. Which ten are
        // chosen is decided here; which of them is in front is decided below, and the two
        // orders are different.
        let mut found: [(i32, usize, Sprite); MAX_SPRITES_PER_LINE] =
            [(0, 0, PLACEHOLDER_SPRITE); MAX_SPRITES_PER_LINE];
        let mut count = 0;

        for index in 0..40 {
            let entry = index * 4;
            if entry + 3 >= oam.len() {
                break;
            }
            // OAM stores positions biased so a sprite can sit partly off the top or left.
            let y = oam[entry] as i32 - 16;
            let x = oam[entry + 1] as i32 - 8;
            if (line as i32) < y || (line as i32) >= y + height as i32 {
                continue;
            }

            let mut tile = oam[entry + 2];
            if height == 16 {
                // A tall sprite's low tile bit is ignored: the pair is always even-aligned.
                tile &= 0xFE;
            }
            let attributes = oam[entry + 3];

            found[count] = (
                x,
                index,
                Sprite {
                    x,
                    y,
                    width: 8,
                    height,
                    tile_offset: tile as usize * 16,
                    palette: (attributes >> 4) & 1,
                    flip_x: attributes & 0x20 != 0,
                    flip_y: attributes & 0x40 != 0,
                    behind_background: attributes & 0x80 != 0,
                },
            );
            count += 1;
            if count == MAX_SPRITES_PER_LINE {
                break;
            }
        }

        // On a DMG the sprite with the smaller X is in front, and OAM order breaks ties. A
        // stable sort on X alone gives both, since the candidates are already in OAM order.
        let selected = &mut found[..count];
        selected.sort_by_key(|(x, _, _)| *x);

        let sprites: Vec<Sprite> = selected.iter().map(|(_, _, sprite)| *sprite).collect();
        render_sprites(
            &sprites,
            vram,
            BitDepth::Two,
            line as u32,
            &mut self.scanline,
        );
    }

    /// Read a PPU register, or `None` if this module does not own the address.
    pub fn read_register(&self, addr: u16) -> Option<u8> {
        Some(match addr {
            reg::LCDC => self.lcdc,
            reg::SCY => self.scy,
            reg::SCX => self.scx,
            reg::BGP => self.palette.bgp,
            reg::OBP0 => self.palette.obp[0],
            reg::OBP1 => self.palette.obp[1],
            reg::WY => self.wy,
            reg::WX => self.wx,
            _ => return None,
        })
    }

    /// Write a PPU register. Returns `None` if this module does not own the address.
    ///
    /// `LCDC` is also consumed by the timing module; the system routes it to both.
    pub fn write_register(&mut self, addr: u16, value: u8) -> Option<()> {
        match addr {
            reg::LCDC => {
                let was_on = self.has(lcdc::LCD_ENABLE);
                self.lcdc = value;
                if was_on && !self.has(lcdc::LCD_ENABLE) {
                    // Switching the LCD off blanks the panel and restarts the window.
                    self.framebuffer.fill(DMG_SHADES[0]);
                    self.window_line = 0;
                }
            }
            reg::SCY => self.scy = value,
            reg::SCX => self.scx = value,
            reg::BGP => self.palette.bgp = value,
            reg::OBP0 => self.palette.obp[0] = value,
            reg::OBP1 => self.palette.obp[1] = value,
            reg::WY => self.wy = value,
            reg::WX => self.wx = value,
            _ => return None,
        }
        Some(())
    }
}

/// Filler for the fixed-size candidate array, never rendered.
const PLACEHOLDER_SPRITE: Sprite = Sprite {
    x: 0,
    y: 0,
    width: 8,
    height: 8,
    tile_offset: 0,
    palette: 0,
    flip_x: false,
    flip_y: false,
    behind_background: false,
};

impl Savable for GbPpu {
    fn save(&self, w: &mut StateWriter) {
        w.write_u8(self.lcdc);
        w.write_u8(self.scy);
        w.write_u8(self.scx);
        w.write_u8(self.wy);
        w.write_u8(self.wx);
        self.palette.save(w);
        w.write_u8(self.window_line);
        // The framebuffer is saved so a loaded state shows the right picture immediately
        // rather than a stale or blank frame until the next one finishes rendering. The
        // scanline buffer is scratch and is not.
        self.framebuffer.save(w);
    }

    fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        self.lcdc = r.read_u8()?;
        self.scy = r.read_u8()?;
        self.scx = r.read_u8()?;
        self.wy = r.read_u8()?;
        self.wx = r.read_u8()?;
        self.palette.load(r)?;
        self.window_line = r.read_u8()?;
        self.framebuffer.load(r)?;
        self.scanline.clear();
        Ok(())
    }
}

/// The colour a pixel resolved to, for tests and for the debugger's tile viewer.
pub fn pixel_at(framebuffer: &Framebuffer, x: u32, y: u32) -> Rgba8 {
    framebuffer.pixel(x, y)
}

#[cfg(test)]
mod tests;
