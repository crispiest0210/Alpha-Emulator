# Prompt 17 — Testing Infrastructure (`testing/harness`)

Read `00-INDEX-AND-ARCHITECTURE.md` first. Prompts 03–13 all reference this harness as their
acceptance-testing mechanism; this prompt should land early enough (in practice, alongside or
just after prompt 02) that those prompts can use it rather than each inventing an ad hoc local
test runner. If ordering pressure forces a prompt to build a minimal local runner first, that
runner should be migrated into this crate once it lands, per the note in prompt 03.

## Objective

`testing/harness`: a headless test-ROM runner (built on `frontend-headless` from prompt 14, or a
minimal precursor to it if this lands first) that runs accuracy test ROMs against any `System`
implementation, captures framebuffer/register/memory output, compares against known-good
expectations (hash, exact pixel snapshot via `insta`, or documented pass/fail memory signature
per the test ROM's own convention — Blargg's ROMs, for instance, self-report pass/fail via a
memory location and serial output), and integrates with CI (prompt 19).

## Context

The predecessor shipped with **zero automated tests** — `README.md` and `AGENTS.md` say so
explicitly, and its own "Next Recommended Improvements" section names test coverage as the top
gap. Every prompt in this collection treats "passes the accuracy suite" as the literal acceptance
criterion for CPU/PPU/APU correctness specifically because this project is not repeating that
gap. This prompt is the piece of infrastructure that makes those acceptance criteria checkable at
all, so it needs to exist early and be trustworthy.

## Architectural Decisions

- **Test ROMs are fetched, not vendored.** The predecessor committed actual ROM binaries to the
  repository (`roms/gba-audio-test.gba`, and — more concerning — a commercial ROM,
  `roms/Pokemon - Emerald Version (USA, Europe).gba`). This project must not repeat that: known-
  redistributable test/homebrew ROMs (Blargg's suites, Mooneye's suite, dmg-acid2, arm7wrestler,
  gba-suite, etc. — confirm each one's actual license/redistribution terms before including it in
  any form) are either fetched from their upstream source at test-time/setup-time into a
  gitignored `testing/test-roms/` directory, or, if a given suite's license explicitly permits
  redistribution, vendored deliberately with that license noted — but never a commercial ROM
  under any circumstance, and never assumed-fine-to-vendor without checking. `cargo xtask setup`
  (prompt 01) or a dedicated `cargo xtask fetch-test-roms` should handle acquisition.
- Harness output format: for each test ROM, run for a bounded number of frames (per-ROM
  configurable, since different suites signal completion differently), then apply that ROM's
  pass/fail detection method (Blargg: read a known memory address / serial output buffer for a
  pass string; dmg-acid2-style: hash or `insta`-snapshot the final framebuffer against a
  known-correct reference image; Mooneye: similar memory-signature convention) — implement this
  as a small per-suite adapter rather than one monolithic "guess how this ROM reports results"
  function, since conventions genuinely differ across suite authors.
- Snapshot testing (framebuffer/audio-sample comparisons where no self-reporting convention
  exists) uses `insta` per `00-...md`'s stack decision, with snapshots committed to the repo as
  the "known good" reference — reviewable in PRs like any other test expectation.
- Determinism regression tests (referenced in prompts 07/11-13/16: bit-identical output across
  repeated runs from the same input) are a harness-provided utility (`assert_deterministic(system,
  input_sequence, frame_count)`), not something each system prompt reimplements independently.
- Benchmarks (`criterion`, per `00-...md`) live alongside but are conceptually separate from
  correctness tests — this prompt sets up the `criterion` harness structure; prompt 18 is where
  performance *work* driven by those benchmarks happens.

## Responsibilities

1. `testing/harness` crate: test-ROM runner driving any `System` via `frontend-headless`,
   per-suite pass/fail adapters (Blargg-style, Mooneye-style, image-snapshot-style), determinism-
   check utility, `insta` snapshot integration.
2. `cargo xtask fetch-test-roms` (or equivalent) acquisition step, with a clear per-ROM-suite
   license note recorded in the harness's own documentation.
3. Wiring so each system crate's accuracy tests (referenced throughout prompts 03–13) are real
   `#[test]`s (or a documented `cargo xtask test-accuracy` step) runnable both locally and in CI.
4. CI integration point (the actual workflow YAML is prompt 19's responsibility; this prompt
   ensures there's a single clean command CI can invoke).

## Interfaces

```rust
pub fn run_blargg_style(system: &mut dyn System, rom: &[u8], max_frames: u32) -> TestOutcome;
pub fn run_snapshot_style(system: &mut dyn System, rom: &[u8], frame_to_capture: u32) -> Framebuffer;
pub fn assert_deterministic(make_system: impl Fn() -> Box<dyn System>, inputs: &[InputState], frames: u32);
```
Exact shape is the implementer's call; the contract is: usable from any `system-*` crate's test
module with minimal per-test boilerplate.

## Constraints

- No commercial ROM, ever, under any circumstance, vendored or referenced by a fetch script
  pointing at an unauthorized source.
- Test-ROM acquisition must be scriptable/automatable (no "download this file manually and place
  it in X" instructions as the primary path) so CI can run unattended.
- Harness itself has no dependency on `winit`/`wgpu`/`egui` (it drives systems via
  `frontend-headless`, which is itself windowing-free per prompt 14's design).

## Deliverables

- `testing/harness` crate implemented per Responsibilities.
- Working test-ROM fetch automation.
- At least the GB accuracy suites (Blargg `cpu_instrs`/`instr_timing`/`dmg_sound`, Mooneye
  acceptance tests, dmg-acid2) wired end-to-end and passing once prompt 11 lands, proving the
  harness itself works before prompts 12–13 lean on it for larger suites.

## Acceptance Criteria

- `cargo xtask test` (or documented equivalent) runs the full accuracy suite for every system
  implemented so far, headlessly, with clear pass/fail output per test ROM.
- CI (prompt 19) runs this on every PR/push across all three target OSes.
- A deliberately-introduced regression (e.g. temporarily break a flag calculation in
  `cpu-sm83`) is caught by the suite in a local trial run — concrete evidence the harness would
  actually catch a real bug, not just that it runs without erroring.

## Testing Requirements

- The harness's own adapters (Blargg-style/Mooneye-style/snapshot-style pass/fail detection) need
  their own unit tests against synthetic known-pass/known-fail inputs, since a bug in the harness
  itself would silently invalidate every other prompt's acceptance criteria.

## Future Compatibility

Every future system or subsystem prompt (beyond this collection's scope) should be able to add
its own accuracy suite through this same harness without structural changes — keep the per-suite
adapter pattern extensible.

## Notes

This is infrastructure the predecessor project never had at all. Treat getting it right and
landing it early as at least as high-priority as any individual system prompt — every accuracy
claim made anywhere else in this collection is only as credible as this harness is trustworthy.
