# Prompt 13 — Nintendo DS System Assembly (`system-nds`)

Read `00-INDEX-AND-ARCHITECTURE.md`, prompts 02–10, and prompts 11–12 first. This is the largest
and most complex system in the project; do not start it until GBA (prompt 12) is stable and
accuracy-tested, since NDS reuses substantial GBA-adjacent infrastructure and its own bring-up
will be significantly easier once that foundation is proven.

## Objective

A `System` implementation for Nintendo DS covering the dual-CPU architecture, dual-screen 2D
rendering (two PPU engines), NDS's 3D core, touch input, and NDS-specific audio/DMA/IPC hardware.
Given the scope, it is acceptable — and expected — for this prompt's v1 bar to be "boots and
runs a meaningful set of commercial-quality homebrew and simple commercial titles correctly,"
not "bit-perfect parity across the entire NDS software library" — NDS emulation accuracy is a
multi-year effort industry-wide; scope this prompt's acceptance criteria accordingly and treat
it as the start of NDS support, not its completion.

## Context

NDS is included in this project's scope specifically because the shared-CPU/shared-2D-compositing
architecture built in prompts 02–10 makes it tractable in a way it would not be as a bolt-on to
the predecessor's single-system design. This prompt is the payoff and the stress test of that
architecture simultaneously.

## Architectural Decisions

- **Dual CPU:** ARM9 (`cpu-arm946e`, prompt 05, "main" CPU, runs most game logic and the 3D
  engine interface) and ARM7 (`cpu-arm7tdmi`, prompt 04, reused directly, "coprocessor" CPU,
  typically handles audio mixing, wifi/hardware I/O, and some game logic) run concurrently,
  communicating via shared WRAM regions and dedicated IPC hardware (IPCSYNC, FIFO). Model this as
  **two independent scheduler-driven `Cpu` step loops interleaved at a fixed granularity**
  (e.g. step ARM9 for N cycles, then ARM7 for its proportional cycle budget, exchanging IPC
  events at defined synchronization points) rather than true parallel threads — determinism
  (required per prompt 07's constraint, and doubly important here since savestate/rewind depend
  on it) is far easier to guarantee with cooperative interleaving on one thread than with real
  CPU-level parallelism, and NDS's actual CPU-to-CPU synchronization hardware assumes tightly
  coupled timing anyway. This is a deliberate, documented deviation from "one thread per CPU";
  do not attempt real multi-threading for the two CPUs.
- **2D graphics:** two independent PPU engines (Engine A, Engine B — main and sub screens), both
  built on `ppu-tile2d` (prompt 08) for their tile/sprite compositing, following the pattern
  `system-gba` established for extending shared primitives with system-specific register/mode
  handling. Engine A has additional capability (3D-layer compositing, larger bitmap modes) that
  Engine B lacks — model this as a capability difference on top of a shared engine implementation,
  not two unrelated engine types, to the extent that's actually true to the hardware (verify
  against GBATEK's NDS section before assuming symmetry or asymmetry).
- **3D core:** NOT part of `ppu-tile2d`. Implemented as its own module in `system-nds`:
  geometry command FIFO processing, a software rasterizer (a GPU-backend rasterizer is explicitly
  out of scope for this prompt — see Constraints) implementing NDS's fixed-function 3D pipeline
  (matrix stack, per-vertex lighting, texture mapping, simple fog/edge-marking effects) closely
  enough to render common commercial titles' 3D content recognizably correctly. This is the
  single largest scope item in the whole project; if time-boxing is needed, prioritize geometry/
  texturing correctness over less commonly load-bearing effects (advanced fog/toon shading edge
  cases), and document explicitly what's deferred.
- **Audio:** NDS's PCM-focused audio hardware (16 channels, various sample formats including
  ADPCM) is different enough from GB/GBA's PSG heritage that it should be implemented directly in
  `system-nds` rather than forced through `apu-shared` — evaluate at implementation time whether
  any primitive (e.g. basic PCM mixing) is worth factoring out for potential reuse, but don't
  force a shared abstraction that doesn't fit.
- **Touch input:** wired through the `InputState.touch` field from prompt 10, mapped to the
  DS's touchscreen coordinate space by `system-nds`.
- **Cartridge:** NDS cartridge header/save-chip handling via `cart-common` (prompt 06), extended
  for NDS-specific save-chip variants as needed.

## Responsibilities

1. `crates/system-nds`: dual-CPU interleaved scheduling, IPC hardware (IPCSYNC, FIFO,
   shared-WRAM arbitration), dual 2D PPU engines via `ppu-tile2d`, 3D core (geometry FIFO,
   software rasterizer, matrix stack), NDS audio hardware, touch input wiring, NDS DMA/interrupt
   controllers (following prompt 12's DMA/interrupt patterns, adapted for NDS's larger register
   set and dual-CPU-visible interrupt sources), cartridge loading via `cart-common`.
2. `Savable` on every owned component — same non-negotiable rule as prompts 11–12, now covering
   meaningfully more state (two CPUs, two PPU engines, 3D pipeline state, IPC hardware state).
3. Wire into `frontend-headless` for accuracy testing.

## Interfaces

`impl System for NdsSystem` per prompt 02's trait. `FrameOutput`/`Framebuffer` need to account
for NDS's dual-screen output — confirm with prompt 02/14 whether `Framebuffer` should be extended
to represent two logical screens or whether `NdsSystem` exposes two `Framebuffer`s through a
system-specific extension of the trait; resolve this in coordination with prompt 14's dual-screen
display requirements rather than deciding it in isolation.

## Constraints

- **Software rasterization only for v1.** A `wgpu`-accelerated 3D rasterizer is a legitimate
  future optimization (cross-reference prompt 18) but out of scope here — and critically,
  `system-nds` itself must remain free of `wgpu`/GPU-API dependencies per the workspace-wide rule
  in `00-...md` §3; if GPU-accelerated rendering is pursued later, it must be architected as
  `frontend-native` consuming a well-defined intermediate representation from `system-nds`'s
  software 3D core, not `system-nds` calling into `wgpu` directly.
- Dual-CPU interleaving must remain deterministic (see Architectural Decisions above) — no data
  race, no thread-scheduling-dependent ordering.
- Every stateful component must implement `Savable`.

## Deliverables

- `crates/system-nds` implementing the scope described above.
- At least one legally-obtained homebrew NDS ROM (2D and, if feasible, a simple 3D homebrew demo)
  playable end-to-end as a documented manual smoke test.

## Acceptance Criteria

- Boots to a functioning home menu / boots a homebrew 2D test ROM correctly through the accuracy
  harness where suitable NDS test ROMs exist (research current community-standard NDS test ROMs
  at implementation time — this ecosystem is smaller than GB/GBA's; document what test coverage
  actually exists versus what's verified only by manual play-testing).
  correct.
- Dual-CPU IPC test: a targeted test ROM or minimal homebrew exercising IPCSYNC/FIFO
  communication behaves correctly.
- 3D core: a simple textured/lit geometry test ROM renders recognizably correctly (exact pixel
  parity is not required at this stage; visually correct geometry, texturing, and basic lighting
  is the bar — document this explicitly as a lower accuracy bar than GB/GBA/2D-NDS, matching this
  prompt's stated scope).
- Save/load round-trip determinism test, same bar as prompts 11–12, now covering dual-CPU and 3D
  pipeline state.

## Testing Requirements

- Whatever accuracy test-ROM coverage exists for NDS at implementation time, wired through
  `testing/harness`.
- Dual-CPU determinism regression test (same event-trace-diff technique as prompt 07, extended to
  both CPUs' interleaved event streams).
- 3D core unit tests for matrix stack operations, geometry command decoding, and rasterizer
  correctness against hand-constructed known-output cases (since full test-ROM coverage for the
  3D core specifically may be sparse).

## Future Compatibility

This is currently the last system in scope. If GPU-accelerated 3D rendering, cycle-accurate cache
timing (deferred from prompt 05), or real multi-threaded CPU execution are pursued later, they
should be additive to this architecture, not restructurings of it — the software-rasterizer
intermediate representation and the deterministic-interleaving CPU model are both designed to be
replaceable behind their current interfaces without changing `system-nds`'s external `System`
trait contract.

## Notes

This prompt is intentionally the least "finish this to 100% parity" of the four system prompts.
Be explicit in code comments and any status documentation about what's implemented, what's
approximate, and what's unimplemented — an honest partial NDS implementation is far more useful
to future contributors than one that silently claims more completeness than it has.
