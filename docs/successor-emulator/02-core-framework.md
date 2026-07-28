# Prompt 02 — Core Framework (`core-common`)

Read `00-INDEX-AND-ARCHITECTURE.md` first, especially §4 (cross-cutting abstractions). This
prompt is where those abstractions get real definitions; every later prompt depends on the
exact trait shapes decided here, so get the signatures right before building on top of them.

## Objective

Implement `crates/core-common`: the shared, system-agnostic foundation — scheduler, bus/memory
traits, CPU trait, System trait, event types, and the logging/tracing setup shared by every
platform crate. No platform-specific behavior lives here; if you find yourself writing GB- or
GBA-specific logic in this crate, it belongs in prompt 06/08/09/11-13 instead.

## Context

The predecessor had no equivalent layer at all — "the emulator" and "GBA" were the same code,
and the run loop was a plain fixed-step JS loop with no generalized event scheduling, which is
part of why timing bugs (audio glue, DMA-adjacent visual corruption) were hard to isolate: there
was no single place that owned "what happens next and when." This crate exists specifically to
own that.

## Architectural Decisions

- **Event-driven scheduler, not fixed-cycle polling.** CPUs execute in cycle-slices bounded by
  "cycles remaining until the next scheduled event," not by ticking every subsystem every N
  cycles. This is both faster (no wasted polling of idle subsystems) and more correct for
  cross-system reuse — different systems have wildly different PPU/timer/DMA cadences, and a
  fixed-step design forces awkward LCM-of-all-periods stepping.
- **Traits over enums for extensibility, concrete generics over `dyn` on the hot path.** The
  `Cpu`/`Bus`/`System` traits exist for structural clarity and testing (mockable bus in unit
  tests), but the per-system assembly crates (11–13) should monomorphize the CPU-bus pairing
  (e.g. `Sm83<GbBus>`) rather than paying `dyn Trait` dispatch cost on every memory access in the
  emulation hot loop. Reserve `dyn` for the scheduler's event callbacks and the debugger's
  inspection API, where dispatch cost is irrelevant.
- **Cycles are the universal clock unit**, represented as a newtype (`pub struct Cycles(pub
  u64)`) around the system's own base clock (each `System` documents what one `Cycles` tick means
  for it — e.g. GB uses 4.194304 MHz t-cycles, GBA uses its own base clock). Do not use wall-clock
  time or frame count as the scheduling unit anywhere in this crate.
- **Savable is defined in the `savestate` crate, not here**, but `core-common`'s `Cpu`/`Bus`/
  `System` traits all carry a `Savable` supertrait bound so nothing implementing them can forget
  it. See prompt 16 for the trait body; here you only need the bound to exist (feature-gate or
  reorder crate dependency so `core-common` can depend on `savestate`'s trait definition — put
  `Savable` in whichever of the two crates avoids a circular dependency, and document the choice
  in this crate's `lib.rs` doc comment since prompt 16 will need to know which way you resolved
  it).

## Responsibilities

Implement in `crates/core-common/src/`:

1. `scheduler.rs` — `Scheduler` struct: binary-heap (`std::collections::BinaryHeap` with a
   `Reverse` wrapper, or a dedicated crate if you prefer) of `(Cycles, EventId)`, `schedule(&mut
   self, when: Cycles, event: EventId)`, `cancel(&mut self, event: EventId)`,
   `next_event_time(&self) -> Option<Cycles>`, `pop_due(&mut self, now: Cycles) -> Vec<EventId>`
   (or an iterator). `EventId` is an opaque, system-defined identifier (newtype around `u32` or
   an enum owned by the calling system crate — decide whichever keeps `core-common` free of
   per-system knowledge; a generic `Scheduler<E>` parameterized over the event type is
   appropriate here since it costs nothing and avoids forcing every system onto one global enum).
2. `bus.rs` — `Bus` trait: `read8/16/32`, `write8/16/32` (systems without 32-bit buses simply
   don't implement those methods as meaningful, or the trait is generic — implementer's call
   based on what prompt 06 actually needs; GB is 8-bit-bus-dominant, GBA/NDS are 32-bit — don't
   force a lowest-common-denominator interface that makes GBA's bus slow). `MemoryRegion` trait
   for composing a bus out of mapped regions (RAM, MMIO ranges, cartridge ROM/RAM windows) with
   explicit open-bus / unmapped-read behavior as a required method, not a default that silently
   returns 0.
3. `cpu.rs` — `Cpu` trait: `fn step(&mut self, bus: &mut impl Bus) -> Cycles` (or `&mut dyn Bus`
   if monomorphization proves impractical for a given implementation — see decision above),
   `fn reset(&mut self)`, plus a debugger-facing extension trait `CpuIntrospect` (register file
   as a `Vec<(&str, u64)>` or similar, disassemble-one-instruction-at hook) kept separate so the
   hot trait stays lean.
4. `system.rs` — `System` trait: `step_frame(&mut self, input: InputState) ->
   FrameOutput`, `reset(&mut self)`, `load_cartridge(&mut self, rom: &[u8]) -> Result<(),
   CartridgeError>`, `framebuffer(&self) -> &Framebuffer`, `take_audio_samples(&mut self) ->
   &[AudioSample]`, `save_state(&self) -> Vec<u8>`, `load_state(&mut self, data: &[u8]) ->
   Result<(), StateError>`. `FrameOutput` is a small struct bundling anything the frontend needs
   per frame beyond the framebuffer/audio (e.g. "did the game just write to save RAM" for
   library-side save-flush scheduling).
5. `event_types.rs` — shared primitive types: `Cycles`, `InputState` (a bitflags-style struct
   covering the union of buttons across all four systems — GB has fewer buttons than GBA; NDS
   adds touch — model this as a struct with `Option`/default-false fields per button plus an
   optional touch-point field, not four incompatible enums), `Framebuffer` (owns pixel storage,
   width/height, pixel format — decide one canonical internal format, e.g. RGBA8888, and let each
   PPU backend convert into it), `AudioSample` (interleaved stereo `i16` or `f32` — pick one and
   document the sample rate contract each system must resample to).
6. `logging.rs` — one `tracing_subscriber` initialization function used by both
   `frontend-native` and `frontend-headless`/the test harness, with per-crate target filtering
   (e.g. `RUST_LOG=cpu_sm83=trace,system_gb=debug`).

## Interfaces

This *is* the interface layer — its whole job is to be depended on by every other crate. Treat
every public signature here as something 15+ other prompts will consume without renegotiation.
If a signature feels wrong once you're implementing prompt 03 or 06, it is cheaper to fix it now
than after four systems depend on it — but do not gold-plate speculative flexibility either;
build exactly what prompts 03–13 need, which you can infer from reading their Responsibilities
sections before finalizing these signatures.

## Constraints

- Zero dependency on `winit`/`wgpu`/`egui`/`cpal` (enforced by prompt 01's `cargo-deny` config —
  don't violate it here of all places).
- `#![deny(unsafe_code)]` at the crate root unless a specific, documented performance need
  justifies an `unsafe` block (unlikely at this layer; more plausible in prompt 08/18).
- No per-system conditionals (`if system == Gb`) anywhere in this crate — that is the definition
  of a leaky abstraction at this layer and should be caught in review.

## Deliverables

- `crates/core-common` fully implemented per Responsibilities, compiling, with unit tests for
  the scheduler (event ordering, cancellation, ties broken deterministically) and bus composition
  (region mapping, open-bus behavior).
- Doc comments on every public trait explaining the contract a per-system implementer must honor
  (this doc is what prompts 03–13 will actually be read against, so write it for that audience).

## Acceptance Criteria

- `cargo test -p core-common` green, including scheduler ordering/determinism tests.
- A trivial mock `System`/`Cpu`/`Bus` implementation (in `core-common`'s own test module, not a
  real system) can be constructed and driven through `step_frame` in a test, proving the traits
  are actually implementable and not just theoretically coherent.
- No `unsafe` without a doc comment justifying it.

## Testing Requirements

- Scheduler: property-test or table-driven tests for out-of-order scheduling, same-timestamp
  ties, cancellation of a not-yet-due event, and "schedule from within a callback" (an event
  handler scheduling a future event) since every real system does this constantly (PPU mode
  transitions rescheduling themselves).
- Bus composition: overlapping-region rejection or explicit precedence rule, out-of-range access
  behavior.

## Future Compatibility

Prompts 03–05 (CPU cores) implement `Cpu`; prompts 06–07 build real buses and drive the
scheduler; prompts 11–13 implement `System`. Any breaking change to these traits after prompt 03
starts should be treated as expensive — coordinate before doing so rather than silently drifting.

## Notes

Resist the temptation to make `Scheduler` or `Bus` "smart" about specific hardware quirks (e.g.
GBA's prefetch buffer, NDS's dual-bus arbitration) — those are prompt 06/07/12/13's job, built on
top of these primitives, not folded into them.
