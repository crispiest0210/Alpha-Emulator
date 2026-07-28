# Prompt 07 — Scheduler & Timing Integration

Read `00-INDEX-AND-ARCHITECTURE.md` and `02-core-framework.md` first. Prompt 02 defines the
generic `Scheduler<E>` primitive; this prompt is about *using it correctly* to wire timers, the
PPU's scanline/mode state machine, the APU frame sequencer, and DMA triggers into a coherent
per-system timing model. It is the connective tissue prompt between 02 and 08/09/12/13.

## Objective

Establish, and implement for GB (as the reference case; GBA/NDS analogues land in prompts 12–13),
the pattern by which a `System`'s `step_frame` drives the CPU in scheduler-bounded slices while
timers, PPU mode transitions, and APU sequencing are pure scheduled-event consumers — with no
subsystem polling another subsystem's state every cycle.

## Context

This is the single biggest structural difference from a naive fixed-step emulator loop (which is
effectively what the predecessor's vendored core did, opaquely). Getting this pattern right once,
here, with the simplest system (GB), is what makes prompts 12/13 (GBA/NDS, which have
significantly more scheduled event sources — multiple DMA channels, prefetch, sound FIFO DMA,
NDS's dual-CPU synchronization) tractable instead of a rewrite-the-approach-under-pressure
situation.

## Architectural Decisions

- `System::step_frame` loop shape: `while cycles_this_frame < frame_length { let slice =
  scheduler.next_event_time().map_or(remaining, |t| t - now).min(remaining); cpu.step_slice(bus,
  slice); now += actual_cycles_consumed; for event in scheduler.pop_due(now) { dispatch(event) }
  }` — the exact shape belongs to the implementer, but the *principle* (CPU never runs past the
  next scheduled event without the scheduler getting a chance to fire it) is fixed. Note real
  interpreters step one instruction at a time and instructions don't align to a target slice
  boundary exactly — decide and document how your implementation handles "instruction overshoots
  the slice boundary" (the standard approach: let the instruction complete, then process any
  events now in the past before continuing — this is normal and fine, just be consistent about
  it and don't try to force sub-instruction preemption).
- Timers (GB's `DIV`/`TIMA`/`TMA`/`TAC`) are implemented as scheduled events: schedule the next
  `TIMA` increment (or overflow) at the correct future cycle count rather than decrementing a
  counter every CPU step. Handle `TIMA`-write-during-pending-overflow and `DIV`-reset-affecting-
  TIMA-frequency quirks explicitly — these are real, test-ROM-covered SM83 timer quirks (Mooneye's
  timer test suite covers them); do not skip them as edge cases.
- PPU mode transitions (OAM scan → drawing → HBlank → ... → VBlank) are scheduled events that,
  on firing, both update PPU-visible state (STAT register, LY) and reschedule themselves for the
  next transition — this is the pattern prompt 08 builds the actual rendering on top of.
- APU frame sequencer (512 Hz clock driving length/envelope/sweep updates on GB) is likewise a
  self-rescheduling event, independent of the audio sample-generation cadence (prompt 09 handles
  sample generation as a separate concern from sequencer timing).
- Determinism: given the same ROM, input sequence, and initial state, two runs must produce
  bit-identical output. This has direct implications for save states and rewind (prompt 16) and
  for the accuracy test harness (prompt 17) — do not introduce wall-clock-derived timing,
  uninitialized-memory-dependent behavior, or hash-map-iteration-order-dependent event firing
  anywhere in this scheduling path.

## Responsibilities

- Implement GB's timer subsystem and PPU mode-transition scheduling as the reference
  implementation of this pattern (PPU *rendering* itself is prompt 08's job — this prompt covers
  the mode/timing state machine and STAT/LY register semantics that rendering depends on).
- Implement GB's APU frame sequencer scheduling (again, timing/sequencing only — waveform
  generation is prompt 09).
- Write up the pattern (a short doc comment block in `system-gb`'s scheduler-wiring module is
  sufficient — do not create a separate prose design doc) clearly enough that prompts 12/13 can
  replicate it for GBA's timer/DMA/PPU triggers and NDS's dual-CPU-synchronized equivalents
  without re-deriving the approach from prompt 02's primitives alone.

## Interfaces

Builds on `core-common::Scheduler<E>` from prompt 02; `E` for GB is a concrete enum
(`GbEvent::TimaOverflow`, `GbEvent::PpuModeTransition`, `GbEvent::ApuSequencerTick`, etc.) owned
by `system-gb`.

## Constraints

- No subsystem may poll another subsystem's register state on every CPU cycle as a substitute
  for scheduling a proper event — if you catch yourself writing `if cycles % N == 0`, stop and
  schedule an event instead.
- Must remain deterministic (see above) — no `SystemTime`/`Instant`/thread-scheduling-dependent
  logic anywhere in this path.

## Deliverables

- GB timer subsystem (`DIV`/`TIMA`/`TMA`/`TAC` with documented quirks) as scheduled events.
- GB PPU mode/STAT/LY timing state machine as scheduled events (feeding prompt 08's renderer).
- GB APU frame sequencer as a scheduled event.
- Reference documentation (in-code) of the pattern for reuse in prompts 12–13.

## Acceptance Criteria

- Passes Mooneye's timer-related acceptance tests (`tima_reload`, `tim00`–`tim11` div-write
  interaction tests, etc. — confirm exact current test names against the Mooneye suite at
  implementation time) via the accuracy harness (prompt 17).
- Two full-frame runs from identical initial state and input produce byte-identical scheduler
  event traces (a good regression test: log event `(cycle, EventId)` pairs and diff between runs).

## Testing Requirements

- Timer quirk unit tests (TIMA-write-during-overflow, DIV-reset-affects-TIMA-frequency).
- Determinism regression test as described above.
- PPU STAT/LY timing unit tests cross-checked against Pan Docs' documented mode-length table.

## Future Compatibility

GBA (prompt 12) needs this same pattern extended to multiple DMA channels with priority/timing
interactions (HBlank-DMA, VBlank-DMA, sound-FIFO-DMA) and NDS (prompt 13) needs it extended to
two CPUs whose event streams interleave through shared memory and IPC hardware — both are
substantially harder than GB's single-CPU case, which is exactly why proving the base pattern
here first matters.

## Notes

If you're unsure whether something belongs in "timing/scheduling" (this prompt) or "rendering/
sound generation" (prompts 08/09), the test is: does it need to happen at a precise cycle count
regardless of whether anyone's looking at pixels/samples yet? If yes, it's scheduling. If it's
about *what value gets computed* once triggered, it's the other prompt's job.
