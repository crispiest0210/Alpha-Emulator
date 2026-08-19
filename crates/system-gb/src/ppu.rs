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
//! It also answers *when* that period ends, which is not a fixed number: see
//! [`GbPpu::mode3_cycles`]. The scheduler cannot work it out, because it depends on what the
//! fetcher has to do — how far the background is scrolled within its tile, whether the window
//! opens, how the line's objects fall — and that is this module's knowledge.
//!
//! The limit of drawing a line all at once is a register rewritten *within* the line: hardware
//! splits the line, this draws it whole. That is what the `mealybug-tearoom-tests` entries in
//! the corpus measure, and closing it means a per-dot fetcher rather than a better number.
//!
//! # Register ownership
//!
//! `LCDC` is read by both this module and [`crate::timing`] — the PPU needs the layer-enable
//! bits, the timing state machine needs the LCD-enable bit. The system assembly routes a
//! write to both. That is deliberate duplication of a *read*, not of state: neither module
//! stores the other's fields.

use crate::cgb::TileAttributes;
use crate::memory::{self, GbModel};
use core_common::{Framebuffer, Rgba8, Savable, StateError, StateReader, StateWriter};
use ppu_tile2d::{
    render_sprites, render_text_background, BackgroundParams, BitDepth, MonochromePalette,
    PaletteSource, ScanlineBuffer, Sprite, SpriteRule, TileRef, TilemapSource, DMG_SHADES,
};

pub const SCREEN_WIDTH: u32 = 160;
pub const SCREEN_HEIGHT: u32 = 144;

/// The most sprites the hardware can fetch for one line.
///
/// The limit is a property of the OAM scan, which runs out of time: the first ten candidates
/// in OAM order win and the rest are simply not drawn. Games exploit it deliberately to hide
/// sprites, so raising it would be a visible inaccuracy, not a generosity.
pub const MAX_SPRITES_PER_LINE: usize = 10;

/// The shortest mode 3 can be: nothing to skip, no window, no objects.
///
/// Everything below is a *penalty* added to this. See [`GbPpu::mode3_cycles`].
pub const MODE3_MIN_CYCLES: u64 = 172;

/// The longest mode 3 can be.
///
/// Not a modelling choice — it is the point at which the fetcher has done everything it can be
/// asked to do on one line, and the sum of the penalties below is capped at it. Mode 0 is what
/// is left of the 456-cycle line, so a mode 3 that ran past this would eat into the next line.
pub const MODE3_MAX_CYCLES: u64 = 289;

/// What starting the window mid-line costs.
///
/// The background fetcher is thrown away and restarted at the window's origin, and the six
/// cycles are that restart: the fetcher's four steps plus the two the pixel FIFO stalls for.
pub const WINDOW_PENALTY_CYCLES: u64 = 6;

/// The first `WX` with no window pixel left on screen: `WX - 7` must land below 160.
const WX_LIMIT: u32 = 167;

/// What one object costs the fetcher at best, in t-cycles.
///
/// It is never free: the fetcher aborts whatever background fetch is in flight, reads the
/// object's two pattern bytes, and merges them into the FIFO, and that is six cycles even when
/// the abort costs nothing. The worst case is [`OBJECT_MIN_PENALTY_CYCLES`] + 5; see
/// [`GbPpu::object_penalty`].
pub const OBJECT_MIN_PENALTY_CYCLES: u64 = 6;

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

/// The picture processing unit, for every machine in the Game Boy family.
///
/// # Where the CGB differs, and where getting it wrong is silent
///
/// Three things are gated on [`GbModel`] rather than forked into a second PPU, and all three were
/// wrong at some point in a way that produced a complete, plausible, *wrong* picture rather than an
/// error — which is why cgb-acid2's reference comparison is load-bearing:
///
/// - **Tile attributes.** A CGB reads a second byte from VRAM bank 1 beside each map cell, carrying
///   palette, bank, flips, and a priority bit. See [`crate::cgb::TileAttributes`].
/// - **Sprite attributes.** The same byte in OAM means different things: on a DMG bit 4 picks
///   OBP0 or OBP1, on a CGB bits 0-2 pick one of eight palettes and bit 3 picks the tile's VRAM
///   bank.
/// - **Sprite ordering.** A DMG puts the smaller X in front; a CGB uses OAM index alone.
///
/// **`OPRI` is not modelled.** A real CGB can be asked, through that register, to use the DMG's
/// X-coordinate ordering while running in colour mode. Nothing here reads it, so a game that sets it
/// gets colour-mode ordering. No test ROM in the corpus exercises it and no known game relies on it,
/// but it is a difference, so it is written down rather than left to be rediscovered.
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

    /// Whether `WY == LY` has been seen yet this frame.
    ///
    /// The vertical condition is a **latch**, not a comparison: hardware tests `WY == LY` once
    /// per line, at the start of the OAM scan, and once it matches the window is armed for the
    /// rest of the frame. Re-evaluating `LY >= WY` at draw time instead — the obvious reading,
    /// and what this did — gets two things wrong. A game that raises `WY` above `LY` partway
    /// down loses its window immediately, where hardware keeps drawing it; and a game that
    /// lowers `WY` past the current line gains one, where hardware waits for the next frame.
    ///
    /// Cleared by [`Self::begin_frame`], set by [`Self::begin_line`].
    window_triggered: bool,
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
            window_triggered: false,
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
        self.window_triggered = false;
    }

    /// Called when the PPU begins a line, at the start of its OAM scan.
    ///
    /// This is the moment hardware samples `WY`, and the only one: the window's vertical
    /// condition is a latch, not a comparison. Writing `WY` later in the line cannot change this
    /// frame's answer for this line, which is exactly the property the latch exists to preserve.
    pub fn begin_line(&mut self, line: u8) {
        if line == self.wy {
            self.window_triggered = true;
        }
    }

    /// Whether the window is drawn on the line currently being processed.
    ///
    /// Shared by the renderer and by [`Self::mode3_cycles`] so the picture and the timing can
    /// never disagree about whether the window appeared — a split between those two is exactly
    /// the kind of bug that shows up only as a one-line raster glitch in one game.
    pub fn window_visible(&self) -> bool {
        self.has(lcdc::WINDOW_ENABLE) && self.window_triggered && (self.wx as u32) < WX_LIMIT
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
            // A DMG has no background-priority bit to consult, so the sprite decides alone.
            let rule = if model.uses_tile_attributes() {
                SpriteRule::SpriteOrTileDecides {
                    master_priority: self.has(lcdc::BG_ENABLE),
                }
            } else {
                SpriteRule::SpriteDecides
            };
            self.render_sprites_for_line(model, line, vram, oam, rule);
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

    fn render_window(&mut self, _line: u8, vram: &[u8], attributes: bool) {
        if !self.window_visible() {
            return;
        }
        // `WX` is offset by 7, so a value of 7 puts the window's left edge at screen x = 0.
        // Values below 7 push it off the left, which hardware handles by clipping.
        let start_x = (self.wx as i32 - 7).max(0) as u32;

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
            // A Game Boy background is the bottom of the picture, not a layer over something
            // else: index 0 is an ordinary shade, and sprite priority is decided by comparing
            // against it. Skipping it here would make the background disappear entirely.
            transparent_index_zero: false,
            // The window scrolls with its own counter, not with LY or SCY.
            line: self.window_line as u32,
            scroll_x: 0,
            scroll_y: 0,
            map_width: 32,
            map_height: 32,
            depth: BitDepth::Two,
            // A Game Boy has one background, so there is no layer to distinguish.
            layer: 0,
            start_x: start_x as usize,
            origin_x: start_x,
        };
        render_text_background(&map, vram, &params, &mut self.scanline);

        // Only a line the window actually appeared on advances its counter.
        self.window_line = self.window_line.saturating_add(1);
    }

    /// The objects the OAM scan selects for `line`, as OAM entry indices in OAM order.
    ///
    /// The scan is its own step, and two very different things need its result: the renderer,
    /// which draws them, and [`Self::mode3_cycles`], which charges for them. Running the
    /// selection twice from two copies of the ten-candidate rule would let the picture and the
    /// timing disagree about which objects exist, so there is one implementation.
    fn scan_oam(&self, line: u8, oam: &[u8]) -> ([usize; MAX_SPRITES_PER_LINE], usize) {
        let height = self.sprite_height() as i32;
        let mut found = [0usize; MAX_SPRITES_PER_LINE];
        let mut count = 0;

        for index in 0..40 {
            let entry = index * 4;
            if entry + 3 >= oam.len() {
                break;
            }
            // OAM stores positions biased so a sprite can sit partly off the top or left.
            let y = oam[entry] as i32 - 16;
            if (line as i32) < y || (line as i32) >= y + height {
                continue;
            }
            found[count] = index;
            count += 1;
            if count == MAX_SPRITES_PER_LINE {
                break;
            }
        }
        (found, count)
    }

    #[inline]
    fn sprite_height(&self) -> u32 {
        if self.has(lcdc::OBJ_TALL) {
            16
        } else {
            8
        }
    }

    fn render_sprites_for_line(
        &mut self,
        model: GbModel,
        line: u8,
        vram: &[u8],
        oam: &[u8],
        rule: SpriteRule,
    ) {
        let height: u32 = self.sprite_height();

        // Which ten objects are drawn is decided by the OAM scan; which of them is in front is
        // decided below, and the two orders are different.
        let (candidates, count) = self.scan_oam(line, oam);
        let mut found: [(i32, usize, Sprite); MAX_SPRITES_PER_LINE] =
            [(0, 0, PLACEHOLDER_SPRITE); MAX_SPRITES_PER_LINE];

        for (slot, &index) in candidates[..count].iter().enumerate() {
            let entry = index * 4;
            let y = oam[entry] as i32 - 16;
            let x = oam[entry + 1] as i32 - 8;

            let mut tile = oam[entry + 2];
            if height == 16 {
                // A tall sprite's low tile bit is ignored: the pair is always even-aligned.
                tile &= 0xFE;
            }
            let attributes = oam[entry + 3];

            // The attribute byte means two different things depending on the machine, and reading
            // it the DMG way on a CGB is silent: it yields palette 0 for every sprite and ignores
            // the tile bank, so the picture is complete and wrong. That is what cgb-acid2's
            // "HELLO WORLD!" banner caught — eight sprites naming OBJ palette 3, all drawn through
            // palette 0, the right shapes in the wrong colours.
            //
            //   DMG: bit 4 selects OBP0 or OBP1. There is one VRAM bank and no others.
            //   CGB: bits 0-2 are one of eight OBJ palettes, and bit 3 selects the VRAM bank the
            //        sprite's tile data comes from.
            //
            // Bits 5, 6, and 7 — the flips and the behind-background bit — mean the same on both.
            let (palette, tile_bank) = if model.uses_tile_attributes() {
                (attributes & 0x07, ((attributes >> 3) & 1) as usize)
            } else {
                ((attributes >> 4) & 1, 0)
            };

            found[slot] = (
                x,
                index,
                Sprite {
                    // A Game Boy sprite is always two bits per pixel; there is no other mode.
                    depth: BitDepth::Two,
                    // One sprite plane, so this is never compared. `behind_background` is the
                    // Game Boy's whole answer to sprite-versus-background priority.
                    priority: 0,
                    x,
                    y,
                    width: 8,
                    height,
                    // The bank is folded into the offset, exactly as the background's attribute
                    // path does it, so nothing downstream needs to know banks exist.
                    tile_offset: tile as usize * 16 + tile_bank * memory::VRAM_BANK_SIZE,
                    palette,
                    flip_x: attributes & 0x20 != 0,
                    flip_y: attributes & 0x40 != 0,
                    behind_background: attributes & 0x80 != 0,
                    // A Game Boy sprite is one tile wide and its rows are contiguous, so
                    // there is no arrangement to describe.
                    row_stride: 0,
                },
            );
        }

        // On a DMG the sprite with the smaller X is in front, and OAM order breaks ties. A
        // stable sort on X alone gives both, since the candidates are already in OAM order.
        //
        // A CGB running a colour game does *not* use X: priority is OAM index alone, lower first,
        // which is the order the candidates are already in. So the sort is skipped rather than
        // replaced. (`OPRI` can ask a CGB for the DMG rule; it is not modelled, and a game that
        // sets it would get colour-mode priority — see the crate docs.)
        let selected = &mut found[..count];
        if !model.uses_tile_attributes() {
            selected.sort_by_key(|(x, _, _)| *x);
        }

        let sprites: Vec<Sprite> = selected.iter().map(|(_, _, sprite)| *sprite).collect();
        render_sprites(&sprites, vram, line as u32, rule, &mut self.scanline);
    }

    // -- How long mode 3 takes -----------------------------------------------

    /// How long this line's pixel transfer takes, in t-cycles.
    ///
    /// Mode 3 is not a fixed 172 cycles and never was — that was a placeholder. Its length is
    /// whatever the fetcher's work adds up to, from 172 to [`MODE3_MAX_CYCLES`], and mode 0 is
    /// only what is left of the line afterwards. Which makes this a *rendering* property that
    /// the timing module has to ask about rather than assume, and it is why this function lives
    /// here: [`crate::timing`] owns when the mode ends, this owns how much work it contains.
    ///
    /// Getting it wrong is not a subtle error. Every mid-scanline raster effect — a status bar
    /// held still while the world scrolls, a window opened halfway down, a palette swapped
    /// between lines — is a game writing a register from an `HBlank` STAT interrupt and
    /// depending on landing inside mode 0. A mode 0 that starts up to 117 cycles early hands
    /// that write to the wrong line.
    ///
    /// # The three penalties
    ///
    /// Each is documented on its own function; the model is the one in Pan Docs' "Mode 3
    /// length" section.
    ///
    /// - **Fine scroll**, `SCX % 8`: [`Self::fine_scroll_penalty`].
    /// - **Window start**, 6 cycles: [`WINDOW_PENALTY_CYCLES`].
    /// - **Objects**, 6 to 11 cycles each: [`Self::object_penalty`].
    ///
    /// # What this does not model
    ///
    /// The penalties are computed from the registers as they stand when mode 3 *begins*, which
    /// is when hardware latches `SCX` and decides the window's fate for the line. A game that
    /// rewrites `SCX` during mode 3 shifts the picture on hardware and changes the length here
    /// not at all. Modelling that needs a per-dot fetcher, which this scanline renderer is not —
    /// the `mealybug_*` entries in the test corpus carry the measured size of that gap.
    pub fn mode3_cycles(&self, line: u8, oam: &[u8]) -> u64 {
        let mut cycles = MODE3_MIN_CYCLES + self.fine_scroll_penalty();
        if self.window_visible() {
            cycles += WINDOW_PENALTY_CYCLES;
        }
        cycles += self.object_penalty(line, oam);
        // The cap is hardware's, not a guard against a bug here: ten objects all landing in
        // separate tiles with a fine scroll already applied sums past what the fetcher can
        // actually spend.
        cycles.min(MODE3_MAX_CYCLES)
    }

    /// The `SCX % 8` cycles spent on pixels that are then thrown away.
    ///
    /// The fetcher always starts on a tile boundary, so with a fine scroll set it produces up
    /// to seven pixels that are left of the screen. They are discarded one per cycle, and mode
    /// 3 is that much longer. This is why `SCX = 7` and `SCX = 8` — one pixel apart on screen —
    /// differ by seven cycles of `HBlank`.
    pub fn fine_scroll_penalty(&self) -> u64 {
        (self.scx % 8) as u64
    }

    /// What the objects on `line` cost the fetcher, in t-cycles.
    ///
    /// Only the ten the OAM scan selected can cost anything, and only those with a pixel on
    /// screen: an object parked at `X = 0` is behind the left edge, is never reached, and is
    /// free. The rest are charged as Pan Docs describes:
    ///
    /// - Six cycles each, always. That is the fetch of the object's two pattern bytes.
    /// - Plus, for the **first** object in a given background tile, `5 - min(5, (x + SCX) % 8)`
    ///   more — the wait for the background fetch already in flight to reach a point where it
    ///   can be abandoned. An object landing on a tile boundary waits the full five; one landing
    ///   five or more pixels into a tile waits none.
    ///
    /// So one object costs 6 to 11 cycles, and ten of them can add 117 to a line, which is the
    /// whole 172-to-289 range.
    ///
    /// Objects are walked in ascending `X`, because that is the order the fetcher meets them,
    /// and it is what makes "first in this tile" answerable by remembering one tile rather than
    /// a set: sorted by `X`, objects sharing a tile are adjacent.
    pub fn object_penalty(&self, line: u8, oam: &[u8]) -> u64 {
        if !self.has(lcdc::OBJ_ENABLE) {
            return 0;
        }
        let (candidates, count) = self.scan_oam(line, oam);

        let mut xs = [0i32; MAX_SPRITES_PER_LINE];
        let mut on_screen = 0;
        for &index in &candidates[..count] {
            let x = oam[index * 4 + 1] as i32 - 8;
            if x <= -8 || x >= SCREEN_WIDTH as i32 {
                continue;
            }
            xs[on_screen] = x;
            on_screen += 1;
        }
        let xs = &mut xs[..on_screen];
        xs.sort_unstable();

        let mut total = 0;
        let mut charged_tile: Option<i32> = None;
        for &x in xs.iter() {
            let position = x + self.scx as i32;
            let tile = position.div_euclid(8);
            total += if charged_tile == Some(tile) {
                OBJECT_MIN_PENALTY_CYCLES
            } else {
                OBJECT_MIN_PENALTY_CYCLES + 5 - position.rem_euclid(8).min(5) as u64
            };
            charged_tile = Some(tile);
        }
        total
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
    depth: BitDepth::Two,
    priority: 0,
    row_stride: 0,
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
        w.write_bool(self.window_triggered);
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
        self.window_triggered = r.read_bool()?;
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
