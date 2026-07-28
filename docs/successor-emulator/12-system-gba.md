# Prompt 12 — Game Boy Advance System Assembly (`system-gba`)

Read `00-INDEX-AND-ARCHITECTURE.md`, prompts 02–10, and prompt 11 first. Prompt 11's GB/GBC
implementation is the proven reference pattern for how a `System` is assembled from shared
components — follow that pattern here, adapted for GBA's larger scope, rather than starting the
assembly approach from scratch.

## Objective

A complete, accuracy-tested, playable `System` implementation for Game Boy Advance — the system
the *predecessor project itself* targeted, so this prompt's acceptance bar is implicitly "at
least as correct and complete as the predecessor's vendored IodineGBA core, but built on this
project's own shared abstractions and with real test coverage the predecessor never had."

## Context

GBA is architecturally the bridge system in this project: it shares its CPU (`cpu-arm7tdmi`,
prompt 04) with NDS's ARM7 side, shares 2D compositing (`ppu-tile2d`, prompt 08) and PSG audio
(`apu-shared`, prompt 09) with GB/GBC, and introduces GBA-only concerns (bitmap video modes,
affine backgrounds, multiple DMA channels with sound-FIFO triggering, GBA's specific interrupt
controller and wait-state memory timing) that must be implemented here without leaking into the
shared crates.

## Architectural Decisions

- CPU: `cpu-arm7tdmi::Arm7Tdmi` (prompt 04) at GBA's native clock, single core, no coprocessor
  hardware behind CP opcodes (they trap/no-op per prompt 04's spec-accurate handling).
- Memory: GBA's full memory map (BIOS, EWRAM, IWRAM, I/O, palette RAM, VRAM, OAM, cartridge ROM
  across three wait-state-configurable regions, cartridge SRAM/Flash/EEPROM/GPIO) assembled per
  prompt 06's pattern, including GBA's documented open-bus read behavior and wait-state timing
  (wait states are a real, cycle-count-affecting, test-ROM-checkable behavior — do not
  approximate them as a fixed cost).
- Graphics: `ppu-tile2d` (prompt 08) for tile-based Modes 0–2 (including GBA's affine background
  math, which is GBA-specific and lives in `system-gba`, using `ppu-tile2d`'s tile-fetch/palette
  primitives underneath) and sprites (including GBA's affine sprites); Modes 3/4/5 bitmap
  rendering implemented directly in `system-gba` since it's not a tile-compositing operation and
  isn't shared with GB/GBC.
- Audio: `apu-shared`'s four PSG channels (prompt 09) reused for backward-compatible sound, plus
  two GBA-specific PCM/FIFO channels fed by DMA (Timer-triggered FIFO refill via DMA, per
  prompt 06's DMA-as-scheduled-event pattern) implemented directly in `system-gba`.
- DMA: four channels, correctly modeling immediate/VBlank/HBlank/special (sound FIFO / video
  capture) trigger modes, priority, and the cycle cost of transfers, as scheduled events (prompt
  06/07's pattern) — this is one of the more commonly under-implemented areas in hobbyist GBA
  emulators and directly affects both correctness (games that rely on precise DMA timing for
  effects) and audio quality (FIFO underrun causes audible artifacts).
- Interrupt controller: GBA's `IE`/`IF`/`IME`/`BIOS` interrupt-dispatch convention (note: GBA
  routes all interrupts through a fixed BIOS handler address that then dispatches via a
  user-installed handler table in RAM — this is architecturally different from GB's direct
  vector table and must be modeled accurately, not assumed identical to prompt 11's GB interrupt
  handling).
- BIOS: same policy as prompt 11 — support a user-supplied real GBA BIOS image, and do not vendor
  one in the repository (the predecessor committed `public/gba_bios.bin`; this project should not
  assume that's appropriate to repeat without the implementer separately confirming licensing,
  which is out of scope for this prompt to resolve — default to not vendoring it). If a HLE BIOS
  fallback is implemented for common BIOS calls (`SWI` functions used pervasively by GBA games
  for things like `VBlankIntrWait`, memory copy/fill routines, decompression), it must be
  behaviorally accurate against known BIOS call semantics (GBATEK documents the exact contract
  each `SWI` function must honor) — do not ship an approximate HLE implementation that merely
  "usually works."

## Responsibilities

1. `crates/system-gba`: full `System` implementation — memory map, DMA controller, interrupt
   controller, bitmap-mode rendering, PCM/FIFO audio channels, cartridge loading via
   `cart-common`'s GBA save-chip variants (prompt 06), `Savable` on every owned component.
2. Wire into `frontend-headless` for the accuracy harness, same as prompt 11.
3. If an HLE BIOS fallback is implemented, it lives here (GBA-specific), not in `cpu-arm7tdmi`.

## Interfaces

`impl System for GbaSystem` per prompt 02's trait. Constructor accepts ROM bytes and optional
real BIOS bytes; document the exact behavioral differences (if any) between real-BIOS and
HLE-BIOS modes so users/testers understand what they're getting.

## Constraints

- No UI/windowing dependency.
- No duplication of `cpu-arm7tdmi` or `ppu-tile2d`/`apu-shared` logic — if something feels like
  it should live in one of those shared crates instead of here, raise that rather than silently
  forking behavior (concretely: prefer extending the shared crate's public API over reimplementing
  a parallel version of its logic inside `system-gba`).
- Every stateful component must implement `Savable` — same non-negotiable rule as prompt 11.

## Deliverables

- `crates/system-gba` fully implemented per Responsibilities.
- At least one legally-obtained homebrew/public-domain GBA test ROM playable end-to-end as a
  documented manual smoke test (do not rely on or vendor commercial ROMs, unlike the
  predecessor's committed `roms/` directory).

## Acceptance Criteria

- Passes the GBA CPU/timing accuracy suites (prompt 04's criteria, now through the assembled
  system) plus GBA-suite's memory/DMA/timer sections if covered by the current tooling at
  implementation time.
- Passes AGS-aging-cartridge-derived or community-standard GBA PPU accuracy test ROMs for
  affine background/sprite correctness and bitmap-mode correctness (research current best-
  available test ROMs at implementation time — GBATEK and the gba-emulation community's current
  recommended suite are the reference, not this prompt's memory of them).
- FIFO audio DMA produces glitch-free output in a targeted test ROM/real-game smoke test — direct
  evidence the DMA-as-scheduled-event design (prompt 06/07) actually holds up under GBA's more
  demanding timing requirements versus GB.
- Save/load round-trip determinism test, same bar as prompt 11.

## Testing Requirements

- Full accuracy-suite integration tests via `testing/harness`.
- DMA trigger-mode unit tests (immediate/VBlank/HBlank/special) verifying correct timing and
  transfer-count/increment-mode behavior per GBATEK.
- Interrupt-dispatch integration test verifying the BIOS-vector-then-user-handler flow.
- Save-state round-trip determinism test.

## Future Compatibility

NDS (prompt 13) reuses `cpu-arm7tdmi` for its ARM7 side directly and reuses `ppu-tile2d`/
`apu-shared` for its 2D-layer/PSG-heritage audio; the affine-background-math and DMA-controller
patterns built here in `system-gba` are a reasonable structural template for NDS's own (larger)
DMA/interrupt controllers, even though NDS's specific hardware differs enough to need its own
implementation rather than direct reuse of `system-gba`'s types.

## Notes

GBA is the system where this project's architecture faces its first real test against a hardware
generation notably more complex than GB/GBC (multiple DMA channels, wait-state memory timing, a
BIOS-mediated interrupt model, mixed tile/bitmap rendering). Budget real accuracy-testing effort
here — this is also the system most likely to have a large number of existing community test
ROMs to validate against, so use that leverage rather than relying on manual play-testing as the
primary correctness signal, unlike the predecessor project's approach.
