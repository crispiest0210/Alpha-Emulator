# Prompt 08 — Graphics Architecture (`ppu-tile2d` + per-system PPU/GPU backends)

Read `00-INDEX-AND-ARCHITECTURE.md`, `02-core-framework.md`, and `07-scheduler-timing.md` first.

## Objective

Implement `crates/ppu-tile2d` (shared 2D tile/sprite/palette compositing primitives reused by
GB, GBC, and GBA's background/sprite layers) and the GB PPU backend that consumes it (GBC/GBA/NDS
backends are prompts 11–13, building on this crate). NDS's 3D core is explicitly **not** part of
`ppu-tile2d` — it gets its own module inside `system-nds` (prompt 13) — but NDS's 2D layers
(which coexist with 3D on real hardware) do reuse this crate.

## Context

The predecessor's rendering was entirely internal to the vendored JS core, with no shared
primitives even within a single system (it didn't need any — one system, one renderer). This
project needs GB/GBC/GBA background/sprite compositing to actually share code, both because
they're genuinely similar hardware generations and because triplicating tile-fetch/palette-
lookup/priority-compositing logic three times is exactly the kind of complexity this project is
explicitly trying to avoid versus the predecessor's approach (per `00-...md`'s stated goal of
avoiding retrofitted, per-system-siloed complexity).

## Architectural Decisions

- `ppu-tile2d` provides: tile-data fetch/decode (2bpp for GB/GBC, 4bpp/8bpp for GBA), background
  layer scanline compositing (text-mode tilemaps; GBA's affine/rotation background mode is GBA-
  specific math but can still use this crate's tile-fetch primitives), sprite/OAM scanline
  compositing with priority and per-sprite palette handling, and a palette-lookup abstraction that
  works for GB's fixed grayscale/4-shade palette, GBC's CGB color palette RAM, and GBA's 15-bit
  BGR555 palette RAM — via a small `PaletteSource` trait rather than hardcoding one palette format.
- Output target is the `Framebuffer` type from prompt 02 (canonical internal pixel format decided
  there) — `ppu-tile2d` writes into it scanline-by-scanline, driven by the PPU mode-transition
  events from prompt 07 (an "HBlank reached, composite this scanline now" event, not a "render
  the whole frame at VBlank" batch approach — scanline-accurate rendering is required because
  real games rely on mid-frame register writes, e.g. raster effects/split-scroll, being visible).
- GBA-specific bitmap modes (Mode 3/4/5, direct framebuffer rather than tile-based) are **not**
  part of `ppu-tile2d` (they're not shared with GB/GBC) — implemented directly in `system-gba`'s
  PPU backend (prompt 12), which otherwise reuses `ppu-tile2d` for its tile-based modes and
  sprites.
- Frontend presentation (scaling, shader filters, VSync) is explicitly out of scope for this
  crate — it operates purely on the internal `Framebuffer`; `frontend-native` (prompt 14) owns
  taking that buffer and putting it on screen via `wgpu`.

## Responsibilities

1. `crates/ppu-tile2d`: tile decode, background compositing, sprite compositing, `PaletteSource`
   trait + implementations for GB/GBC/GBA palette formats, scanline-output API consumed by a
   system's PPU backend.
2. GB PPU backend (in `system-gb`, but implemented as part of this prompt since it's the
   reference consumer of `ppu-tile2d` and proves the crate boundary is right before GBC/GBA build
   on it): DMG's two background layers... actually GB has one BG + window + sprites (verify exact
   layer count/behavior against Pan Docs — do not assume GBC/GBA's layer count applies), STAT-
   driven scanline rendering hooked to prompt 07's PPU timing events, DMG's 4-shade monochrome
   palette (`BGP`/`OBP0`/`OBP1`) via `PaletteSource`.

## Interfaces

```rust
pub trait PaletteSource {
    fn lookup_bg(&self, palette_index: u8, color_index: u8) -> Rgba8888;
    fn lookup_sprite(&self, palette_index: u8, color_index: u8) -> Rgba8888;
}
pub struct TileCompositor { /* ... */ }
impl TileCompositor {
    pub fn render_bg_scanline(&mut self, ..., out: &mut ScanlineBuffer) { ... }
    pub fn render_sprite_scanline(&mut self, ..., out: &mut ScanlineBuffer) { ... }
}
```
Exact shape is the implementer's call; the contract that matters is: takes raw VRAM/OAM/palette
bytes (via the system's `Bus`, or a narrower borrowed view the caller constructs) plus per-scanline
register state, produces one scanline's worth of composited, prioritized pixel data.

## Constraints

- No dependency on `wgpu`/`winit`/`egui` (enforced by prompt 01's `cargo-deny` config) — this
  crate produces pixels into a CPU-side buffer only.
- No GBA-bitmap-mode or NDS-3D-specific code in `ppu-tile2d` — keep the shared crate honestly
  shared, not a dumping ground with per-system `if` branches.

## Deliverables

- `crates/ppu-tile2d` implemented and unit-tested against known tile-decode/compositing
  correctness (hand-constructed VRAM fixtures with known expected pixel output).
- GB PPU backend fully implemented, scanline-accurate, hooked to prompt 07's timing events.

## Acceptance Criteria

- Passes relevant Mooneye/dmg-acid2-style PPU accuracy test ROMs (dmg-acid2 specifically is a
  well-known pixel-perfect DMG PPU rendering test — the harness in prompt 17 should compare
  framebuffer hash/pixel output against the known-correct reference image) via the accuracy
  harness.
- Mid-frame register write effects (e.g. changing `SCX`/`SCY` mid-scanline via a timed write)
  produce correct raster-split visual behavior in a targeted test ROM.

## Testing Requirements

- `ppu-tile2d` unit tests: tile decode correctness for each supported bit depth, palette lookup
  correctness per `PaletteSource` implementation, sprite priority/overlap resolution rules.
- GB PPU integration test: dmg-acid2 (or current equivalent) framebuffer-hash comparison via
  `testing/harness`'s snapshot mechanism (prompt 17).

## Future Compatibility

Prompt 11 (GBC) extends the GB PPU backend with CGB's additional background layer capabilities/
color palette RAM (still via `ppu-tile2d`'s `PaletteSource`). Prompt 12 (GBA) adds bitmap modes
alongside continued use of `ppu-tile2d` for tile modes and sprites, plus GBA's affine background
math. Prompt 13 (NDS) reuses `ppu-tile2d` for both PPU engines' 2D layers and adds an entirely
separate 3D core module.

## Notes

dmg-acid2 (or the current de facto standard pixel-accuracy test ROM for DMG PPU behavior) is the
right bar for "is this PPU actually correct" versus "looks plausible in casual play-testing" —
insist on it before calling GB PPU work done.
