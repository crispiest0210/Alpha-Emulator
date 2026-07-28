# Prompt 11 — Game Boy & Game Boy Color System Assembly (`system-gb`, `system-gbc`)

Read `00-INDEX-AND-ARCHITECTURE.md` and prompts 02–10 first — this prompt assembles components
those prompts already defined; it does not invent new subsystem behavior except where noted.

## Objective

A complete, playable, accuracy-tested `System` implementation for Game Boy (`system-gb`) and
Game Boy Color (`system-gbc`, depending on and extending `system-gb`). This is the **first
system to reach full playability** in this project and functions as the proof that the shared
abstractions from prompts 02–10 actually hold together end-to-end before the much larger GBA/NDS
efforts begin.

## Context

Every architectural claim in `00-INDEX-AND-ARCHITECTURE.md` §1 is only proven once a real game
boots, runs, saves, and reloads through this exact stack. Treat this prompt as the integration
checkpoint it is — if something from prompts 02–10 doesn't fit cleanly here, that's signal to fix
the shared layer, not to work around it locally in `system-gb`.

## Architectural Decisions

- `system-gb`: DMG-only behavior — SM83 core (prompt 03) at single speed, GB memory map (prompt
  06), monochrome PPU (prompt 08), 4-channel APU (prompt 09), GB timer/scheduler wiring (prompt
  07), GB-family mappers via `cart-common` (prompt 06).
- `system-gbc`: depends on `system-gb`, does **not** copy-paste it. CGB deltas are additive:
  double-speed mode (`KEY1`, toggled via the `cpu-sm83` core's clock multiplier at the scheduler
  level per prompt 03's note), CGB color palette RAM (via `ppu-tile2d`'s `PaletteSource`, prompt
  08), additional VRAM/WRAM banking, CGB-specific boot behavior and DMG-compatibility mode
  (CGB hardware running an unmodified DMG cartridge must reproduce DMG palette/behavior — this is
  a real, test-ROM-checkable compatibility mode, not optional polish).
- `System::step_frame` for both is the concrete assembly of the scheduler pattern from prompt 07:
  drive `Sm83`, dispatch scheduled PPU/timer/APU events, accumulate audio samples, produce a
  completed `Framebuffer`.
- BIOS/boot ROM handling: the project should support running with a real boot ROM image (user-
  supplied, given the licensing situation — do not vendor a copyrighted boot ROM in the repo,
  unlike the predecessor's `public/gba_bios.bin`, whose licensing status this project should not
  assume is clear) **and** a documented fallback boot sequence when no boot ROM is supplied
  (skip straight to post-boot register/memory state, well documented from community research)
  so the emulator is usable out of the box without requiring the user to source a boot ROM.

## Responsibilities

1. `system-gb`: full `System` trait implementation, memory map, PPU/APU/timer wiring, cartridge
   loading via `cart-common`, save state via `Savable` (prompt 16 defines the format; this prompt
   ensures every owned component actually implements it, with no gaps).
2. `system-gbc`: `System` implementation building on `system-gb`'s internals (favor composition/
   reuse of `system-gb`'s types over duplicating them — if CGB truly needs a different struct
   shape for some component, prefer extending or parameterizing the `system-gb` type over a
   parallel implementation, unless the delta is large enough that a fork is genuinely clearer;
   document the choice either way), CGB-specific deltas as listed above.
3. Boot-ROM-optional startup path for both.
4. Wire into `frontend-headless` (used by prompt 17's accuracy harness) so this system is
   testable without the full native GUI.

## Interfaces

`impl System for GbSystem` / `impl System for GbcSystem` per prompt 02's trait definition.
Constructors accept ROM bytes, optional boot ROM bytes, and (for GBC) a hardware-mode flag if
needed to express "CGB running in DMG-compatibility mode" distinctly from "actual DMG hardware."

## Constraints

- No UI/windowing dependency (per the workspace-wide rule in `00-...md` §3).
- Every stateful component reachable from `GbSystem`/`GbcSystem` must implement `Savable`
  (prompt 16) with no reflection-based shortcuts — this is the direct, checkable rejection of
  predecessor lesson §1, and it's the one thing a reviewer should specifically grep for before
  accepting this prompt as complete.

## Deliverables

- `crates/system-gb` and `crates/system-gbc`, both building, both passing their respective
  accuracy suites.
- At least one real, legally-obtained homebrew or public-domain ROM (not a commercial ROM —
  do not vendor commercial game ROMs in the repository, unlike the predecessor's committed
  `roms/Pokemon - Emerald....gba`) playable end-to-end as a manual smoke test, documented in the
  crate's README/doc comments with exact steps to reproduce the smoke test.

## Acceptance Criteria

- Boots and runs Blargg's `cpu_instrs`, `instr_timing`, and `dmg_sound` suites to completion with
  correct pass output (prompts 03/09's acceptance criteria, now verified through the full
  assembled system rather than in isolation).
- Passes dmg-acid2 framebuffer comparison (prompt 08's criteria, likewise now through the full
  system).
- Passes relevant Mooneye acceptance tests for timer/PPU/HALT-bug behavior (prompts 03/07).
- CGB: passes the CGB-specific subset of the same suites where applicable, and correctly
  reproduces DMG-compatibility-mode behavior for an unmodified DMG test ROM run on `system-gbc`.
- Save/load round-trip: save state mid-gameplay, reload, verify subsequent frames are
  bit-identical to an uninterrupted run from the same point (this is the concrete regression test
  for predecessor lesson §1 — the exact bug class that caused corrupted-tile quickload bugs
  before).

## Testing Requirements

- Full accuracy-suite integration tests via `testing/harness` (prompt 17), for both `system-gb`
  and `system-gbc`.
- Save-state round-trip determinism test as described above.
- Manual playability smoke test with a real ROM, documented and reproducible.

## Future Compatibility

`system-gbc`'s pattern of "depend on and extend the smaller system rather than duplicate it" is
the template prompt 13 should evaluate for NDS-reusing-GBA-infrastructure, though NDS's
differences are large enough that full reuse may not be appropriate — that's prompt 13's call to
make once it's in front of the real delta.

## Notes

This is the prompt where "does this architecture actually work" gets answered for the first
time. If prompts 02–10 need small interface adjustments once real integration pressure hits here,
that's expected and fine — make the fix at the source (the defining prompt's crate), not as a
local workaround in `system-gb`.
