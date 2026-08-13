//! [`PpuDebugTarget`] for the Game Boy Advance: palettes, tiles, OAM, and the registers that
//! place them.
//!
//! # Why the decode functions are free functions, not methods
//!
//! Each one — [`decode_palette`], [`decode_tiles`], [`decode_oam`], [`decode_registers`] — takes
//! plain byte slices and struct references rather than `&GbaSystem`, and is unit-tested that
//! way. A debugger view is exactly the kind of code a change elsewhere in the emulator can
//! silently break — a palette index shifted by one, a tile's row order flipped — and the
//! consequence is a wrong picture in a *tool for finding* wrong pictures. Testing the decode
//! directly against known bytes, with no live machine involved, is what catches that before a
//! contributor spends an afternoon trusting a debugger view that was itself the bug.
//!
//! # The trap this file exists partly to avoid repeating
//!
//! `BGxHOFS`/`BGxVOFS` and `BLDY` are write-only registers: a bus read of any of them answers
//! zero, because there is nothing on the other end of that read on real hardware either. Before
//! this, [`GbaSystem::graphics_dump`](crate::GbaSystem::graphics_dump) read scroll through the
//! bus and showed `scroll=(0,0)` for every layer no matter how far a game had actually scrolled
//! it — the register never lies about being read, it just always answers the same wrong thing.
//! [`decode_registers`] reads `scroll_x`/`scroll_y` and `bldy` from where the system actually
//! stores them instead.

use core_common::{
    BgRegisters, LayerOverrides, OamRow, PaletteSwatch, PpuDebugRequest, PpuDebugTarget,
    PpuRegisters, PpuSnapshot, Rgba8, TileBitDepth, TileBitmap, WindowRegisters,
};
use ppu_tile2d::{bgr555_to_rgba, decode_tile_row, BitDepth};

use crate::background::Backgrounds;
use crate::effects::{reg as effects_reg, Effects};
use crate::objects::{GraphicsMode, Object, ObjectAttributeMemory, ObjectMode};
use crate::video::VideoTiming;
use crate::GbaSystem;

impl PpuDebugTarget for GbaSystem {
    fn ppu_snapshot(&self, request: &PpuDebugRequest) -> PpuSnapshot {
        let request = request.clamped();
        let bus = self.bus();
        let palette = bus.memory.palette();
        // The sprite half of palette RAM starts 512 bytes in — 256 entries at two bytes each —
        // which is the same split `GbaPalette::lookup_sprite` in the compositor uses.
        let (bg_bytes, sprite_bytes) = palette.split_at(palette.len().min(512));

        PpuSnapshot {
            bg_palette: decode_palette(bg_bytes),
            sprite_palette: decode_palette(sprite_bytes),
            tiles: decode_tiles(bus.memory.vram(), palette, &request),
            oam: decode_oam(bus.memory.oam(), Some(bus.video.vcount() as u32)),
            registers: decode_registers(&bus.video, &bus.backgrounds, &bus.effects),
            overrides: bus.layer_overrides,
        }
    }

    fn set_layer_overrides(&mut self, overrides: LayerOverrides) {
        self.bus_mut().layer_overrides = overrides;
    }
}

/// Decode 256 palette entries (512 bytes of BGR555, little-endian) into swatches.
///
/// Pure and independent of which half of palette RAM it is given — background and sprite
/// palettes share the same 15-bit BGR encoding, only their base offset differs, and that offset
/// is the caller's business (see [`PpuDebugTarget::ppu_snapshot`]).
pub fn decode_palette(bytes: &[u8]) -> Vec<PaletteSwatch> {
    bytes
        .chunks(2)
        .map(|pair| {
            let raw = match pair {
                [low, high] => u16::from_le_bytes([*low, *high]),
                // A short final chunk only happens against a truncated slice, which a live
                // system never hands this — kept defined rather than panicking so a test with
                // deliberately short input still gets an answer.
                [low] => *low as u16,
                _ => 0,
            };
            PaletteSwatch {
                color: bgr555_to_rgba(raw),
                raw,
            }
        })
        .collect()
}

/// Decode `request.tile_count` consecutive tiles from `vram`, starting at
/// `request.tile_char_base`, at `request.tile_depth`, against BG palette bank
/// `request.tile_palette_bank`.
///
/// Colour index 0 is rendered as its actual palette colour rather than treated as transparent —
/// unlike compositing a background, where index 0 lets the layer behind show through, this is a
/// raw view of what a tile's bytes decode to, and hiding index 0 would show a different picture
/// than the tile data actually contains.
pub fn decode_tiles(vram: &[u8], palette: &[u8], request: &PpuDebugRequest) -> Vec<TileBitmap> {
    let depth = match request.tile_depth {
        TileBitDepth::Four => BitDepth::Four,
        TileBitDepth::Eight => BitDepth::Eight,
    };
    let tile_size = depth.tile_size();
    let row_size = depth.row_size();
    // 4bpp tiles are looked up in one of sixteen 16-colour banks; 8bpp tiles use the full 256-
    // entry palette directly and have no bank to select.
    let bank_offset = match depth {
        BitDepth::Four => (request.tile_palette_bank as usize & 0x0F) * 16,
        _ => 0,
    };

    (0..request.tile_count)
        .map(|tile_index| {
            let base = request.tile_char_base + tile_index * tile_size;
            let mut pixels = [Rgba8::default(); 64];
            for row in 0..8 {
                let row_start = base + row * row_size;
                let row_bytes = vram.get(row_start..row_start + row_size).unwrap_or(&[]);
                let mut indices = [0u8; 8];
                decode_tile_row(row_bytes, depth, &mut indices);
                for (col, &index) in indices.iter().enumerate() {
                    pixels[row * 8 + col] = palette_lookup(palette, bank_offset + index as usize);
                }
            }
            TileBitmap { pixels }
        })
        .collect()
}

/// Look up one background palette entry directly, for the tile viewer — which wants a single
/// colour per call rather than [`decode_palette`]'s whole-palette pass.
fn palette_lookup(palette: &[u8], index: usize) -> Rgba8 {
    let offset = index * 2;
    match (palette.get(offset), palette.get(offset + 1)) {
        (Some(&low), Some(&high)) => bgr555_to_rgba(u16::from_le_bytes([low, high])),
        _ => Rgba8::BLACK,
    }
}

/// Decode every OAM entry into a viewer row.
///
/// `current_line` is `None` for a system with no meaningful "current scanline" to report — this
/// stays a pure function of its inputs either way.
pub fn decode_oam(oam_bytes: &[u8], current_line: Option<u32>) -> Vec<OamRow> {
    let table = ObjectAttributeMemory::decode(oam_bytes);
    table
        .objects
        .iter()
        .enumerate()
        .map(|(index, object)| decode_oam_row(object, index, current_line))
        .collect()
}

/// Decode one OAM entry. Split out from [`decode_oam`] so a test can check a single row without
/// building all 128.
pub fn decode_oam_row(object: &Object, index: usize, current_line: Option<u32>) -> OamRow {
    OamRow {
        index,
        x: object.x,
        y: object.y,
        width: object.width,
        height: object.height,
        priority: object.priority,
        palette: object.palette,
        tile: object.tile,
        affine_index: matches!(object.mode, ObjectMode::Affine | ObjectMode::AffineDouble)
            .then_some(object.matrix),
        graphics_mode: match object.graphics_mode {
            GraphicsMode::Normal => "Normal",
            GraphicsMode::SemiTransparent => "SemiTransparent",
            GraphicsMode::ObjectWindow => "ObjectWindow",
        },
        mode: match object.mode {
            ObjectMode::Normal => "Normal",
            ObjectMode::Affine => "Affine",
            ObjectMode::Hidden => "Hidden",
            ObjectMode::AffineDouble => "AffineDouble",
        },
        on_current_scanline: current_line
            .is_some_and(|line| object.visible() && object.covers_line(line as i32)),
    }
}

/// Decode every register the debugger's PPU register view names.
///
/// Reads `video`, `backgrounds`, and `effects` directly rather than through `Bus::read16` —
/// see the module docs for why that distinction matters for `BGxHOFS`/`BGxVOFS` and `BLDY`.
pub fn decode_registers(
    video: &VideoTiming,
    backgrounds: &Backgrounds,
    effects: &Effects,
) -> PpuRegisters {
    let dispcnt = video.dispcnt;
    let mode = video.mode();

    let backgrounds_out = std::array::from_fn(|index| {
        let layer = backgrounds.layers[index];
        // The same affine-layer test `apply_effects` and `graphics_dump` both use: which layers
        // are affine depends on the display mode, not on anything the layer's own register says.
        let affine = matches!((mode, index), (1, 2) | (1, 3) | (2, 2) | (2, 3));
        BgRegisters {
            control: layer.control,
            enabled: dispcnt & (1 << (8 + index)) != 0,
            priority: layer.priority(),
            char_base: layer.char_base() as u32,
            screen_base: layer.screen_base() as u32,
            bpp: match layer.bit_depth() {
                BitDepth::Eight => 8,
                _ => 4,
            },
            size_tiles: layer.size_in_tiles(affine),
            scroll_x: layer.scroll_x,
            scroll_y: layer.scroll_y,
            mosaic: layer.mosaic(),
        }
    });

    let (win0h, win1h, win0v, win1v) = effects.window_bounds();
    let winin = effects.read16(effects_reg::WININ).unwrap_or(0);
    let winout = effects.read16(effects_reg::WINOUT).unwrap_or(0);
    let bounds = |h: u16, v: u16| {
        (
            (h >> 8) as u8,
            (h & 0xFF) as u8,
            (v >> 8) as u8,
            (v & 0xFF) as u8,
        )
    };
    let (left0, right0, top0, bottom0) = bounds(win0h, win0v);
    let (left1, right1, top1, bottom1) = bounds(win1h, win1v);

    PpuRegisters {
        dispcnt,
        mode,
        forced_blank: video.forced_blank(),
        obj_1d_mapping: dispcnt & crate::video::dispcnt::OBJ_1D_MAPPING != 0,
        dispstat: video.read16(crate::video::reg::DISPSTAT).unwrap_or(0),
        vcount: video.vcount(),
        backgrounds: backgrounds_out,
        windows: [
            WindowRegisters {
                enabled: dispcnt & (1 << 13) != 0,
                left: left0,
                right: right0,
                top: top0,
                bottom: bottom0,
                layers_in: (winin & 0x3F) as u8,
            },
            WindowRegisters {
                enabled: dispcnt & (1 << 14) != 0,
                left: left1,
                right: right1,
                top: top1,
                bottom: bottom1,
                layers_in: ((winin >> 8) & 0x3F) as u8,
            },
        ],
        winout: (winout & 0x3F) as u8,
        obj_window_layers: ((winout >> 8) & 0x3F) as u8,
        bldcnt: effects.read16(effects_reg::BLDCNT).unwrap_or(0),
        bldalpha: effects.read16(effects_reg::BLDALPHA).unwrap_or(0),
        bldy: effects.bldy(),
    }
}

#[cfg(test)]
mod tests;
