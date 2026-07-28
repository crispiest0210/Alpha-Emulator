# Prompt 18 — Performance Optimization

Read `00-INDEX-AND-ARCHITECTURE.md` and prompt 17 first. This prompt should be undertaken only
after the relevant system(s) are already accuracy-tested and passing their suites — performance
work on an incorrect implementation just makes the incorrect behavior faster. Do not reorder this
ahead of correctness for any system.

## Objective

Establish a repeatable profiling workflow, use it to identify and fix real hot-path bottlenecks
across the emulation core, and make a deliberate, evidence-based go/no-go decision on dynamic
recompilation (dynarec/JIT) for the CPU cores versus continued investment in the interpreter
approach.

## Context

`00-INDEX-AND-ARCHITECTURE.md` deliberately deferred dynarec for all CPU cores to keep prompts
03–05 tractable and correctness-focused. This prompt is where that deferral gets revisited with
real data instead of speculation — NDS in particular (two CPUs, software 3D rasterization) is the
system most likely to actually need it, but that should be *measured*, not assumed.

## Architectural Decisions

- Profiling workflow: `criterion` benchmarks (scaffolded in prompt 17) for microbenchmarks
  (single instruction dispatch, PPU scanline compositing, APU sample generation) plus whole-
  system frame-time measurement via `frontend-headless` running representative ROMs, combined
  with a sampling profiler (`cargo flamegraph` / `perf` on Linux, platform-appropriate
  equivalents elsewhere) for whole-run hot-path identification. Document the exact commands used
  to reproduce each profiling result — a profiling claim that can't be reproduced by another
  contributor isn't useful.
- Performance targets are stated per-system, not as one blanket number: GB/GBC should run at full
  speed (native 59.7 Hz or equivalent) with significant headroom on modest hardware given how
  simple the hardware is; GBA needs to sustain full speed including fast-forward multipliers
  (2x/4x, common emulator UX) on typical desktop/laptop hardware; NDS, especially with the
  software 3D rasterizer, is the system most likely to be the actual bottleneck case and is where
  this prompt's dynarec/GPU-rasterizer investigation should focus first.
- **Dynarec decision process:** measure interpreter dispatch overhead as a fraction of total
  frame time for each CPU core under representative workloads before deciding. If CPU dispatch
  overhead is not the dominant cost (plausible for GB/GBA, where PPU/APU work or even just
  memory-access patterns may dominate), a dynarec is not justified — the correctness/maintenance
  cost of a JIT is substantial and should only be paid where profiling data shows it's the actual
  bottleneck. If pursued, scope it to the specific CPU core(s) shown to need it, implemented as an
  alternative backend behind the existing `Cpu` trait (prompt 02) rather than a parallel
  execution path bolted on separately, so `system-*` crates and the debugger/savestate machinery
  don't need to know which backend is active.
- **3D rasterizer acceleration (NDS):** similarly, measure the software rasterizer's actual cost
  before committing to a `wgpu`-backed accelerated path. If pursued, per `00-...md`/prompt 13's
  constraint, this must be architected as `frontend-native` (or a new dedicated rendering crate
  that *is* allowed to depend on `wgpu`) consuming a well-defined intermediate representation
  (e.g. a command buffer of resolved geometry/texture-state) from `system-nds`'s software 3D
  core — `system-nds` itself never gains a `wgpu` dependency.
- Memory-access-pattern optimization (cache-friendly layout of hot structures, avoiding
  unnecessary allocation in per-frame/per-instruction paths) is likely to matter more than
  algorithmic cleverness for an interpreter-based emulator and should be investigated before
  reaching for a JIT.

## Responsibilities

1. Document and script the profiling workflow (a `cargo xtask bench` / `cargo xtask profile`
   subcommand wiring `criterion` and flamegraph generation, per prompt 01's `xtask` pattern).
2. Run it against each system implemented so far, identify actual hot paths with data (not
   intuition), and fix the ones that matter for hitting the per-system targets above.
3. Produce the dynarec go/no-go decision (documented in-code/in this crate's notes, with the
   supporting profiling data referenced) for each CPU core.
4. If a dynarec or GPU-rasterizer path is greenlit by that analysis, implement it as described in
   Architectural Decisions.

## Interfaces

No new cross-cutting trait beyond what prompt 02 already defines, unless a dynarec backend is
implemented — in which case it implements the existing `Cpu` trait, not a new one.

## Constraints

- No premature optimization: this prompt does not start until the relevant system(s) pass their
  accuracy suites (prompt 17) — correctness first, always.
- Any `unsafe` code introduced for performance reasons (e.g. bypassing bounds checks on a
  proven-hot memory-access path) must be justified with a doc comment and, ideally, the profiling
  data that motivated it, per the narrow `unsafe` exception carved out in prompt 02.
- Optimization work must not regress the accuracy suite — every change in this prompt's scope
  re-runs prompt 17's full suite before being considered acceptable.

## Deliverables

- Profiling workflow tooling (`xtask` subcommands, documented usage).
- A written (in-code, not a separate prose doc) summary of findings per system: where time
  actually goes, what was optimized, and the dynarec/GPU-acceleration go/no-go decision with
  supporting rationale.
- Any resulting optimizations, each independently justified by before/after benchmark numbers.

## Acceptance Criteria

- Stated per-system performance targets are met on representative hardware (document what
  "representative" means — e.g. a specific mid-range consumer CPU class — since "fast enough"
  without a stated baseline isn't a checkable criterion).
- Every optimization change has a `criterion` benchmark showing measurable improvement and the
  full accuracy suite still passing.
- The dynarec/GPU-rasterizer decision for each CPU core/NDS 3D is explicitly documented with
  supporting data, whichever way it went — "we didn't need it because X" is an acceptable and
  valuable outcome, not a failure to deliver a JIT.

## Testing Requirements

- Regression: full accuracy suite (prompt 17) must remain green after every optimization change.
- Benchmark suite itself should be stable enough (low run-to-run variance) to be trustworthy for
  before/after comparisons — if `criterion`'s default settings produce noisy results on the CI
  hardware in use, tune sample size/warm-up accordingly rather than accepting unreliable numbers.

## Future Compatibility

If a dynarec backend is added for one CPU core, the `Cpu` trait's backend-swappable design should
make adding one for another core later a contained, incremental change rather than a redesign.

## Notes

Resist treating "we should have a JIT" as a foregone conclusion just because other emulator
projects have them — this project's own profiling data is the only valid basis for that decision
here, and for GB/GBC in particular it's plausible the interpreter is simply fast enough on modern
hardware that a JIT would be pure added complexity for no user-visible benefit.
