# Prompt 15 — Debugging Framework (`debugger`)

Read `00-INDEX-AND-ARCHITECTURE.md` and prompts 02–04/05 (`CpuIntrospect`) first.

## Objective

`crates/debugger`: breakpoints, step/continue/pause execution control, disassembly views,
memory/register inspection, execution tracing, and a GDB-remote-protocol-subset server for
external tooling — surfaced through `frontend-native`'s `egui` chrome (prompt 14) as an in-app
debugger panel, and independently usable headlessly (e.g. scripted trace capture for bug
reports).

## Context

The predecessor had no debugging tooling at all beyond forwarding frontend `console.log` calls to
the terminal — real bugs (the save-state corruption issues documented in `AGENTS.md`'s handoff
notes) were diagnosed by reading vendored third-party source and manual reasoning, not by
inspecting live emulator state. This project is explicitly scoped to support GB/GBC/GBA/NDS
long-term, across contributors who won't all have the original author's familiarity with the
codebase — a real debugger is load-bearing for that, not a nice-to-have.

## Architectural Decisions

- Debugger operates against the `CpuIntrospect` and `Savable`/bus-inspection surfaces already
  defined in prompts 02–05 — it does not reach into private internals of any CPU/system crate
  (this is a direct, structural rejection of the predecessor's reflection-based introspection
  approach, applied to debugging rather than save states this time).
- Breakpoints are modeled as a scheduler-adjacent concept: execution breakpoints (address match),
  read/write watchpoints (address or address-range match on bus access), and conditional
  breakpoints (an expression evaluated against register/memory state) — implemented as a layer
  that intercepts `Cpu::step`/`Bus::read`/`Bus::write` calls when debugging is active, with
  effectively zero overhead when inactive (feature-flag or trait-object-null-object pattern so
  normal gameplay doesn't pay a debugger tax — verify this with prompt 18's profiling workflow
  once both exist).
- A GDB Remote Serial Protocol subset (register read/write, memory read/write, breakpoint set/
  clear, continue/step) is exposed over a TCP socket, gated behind an explicit opt-in (not
  listening by default), enabling external tools (IDEs, existing GDB-compatible front-ends) to
  attach — this is deliberately scoped as a *subset* sufficient for basic attach-and-inspect
  workflows, not full GDB protocol parity.
- Execution tracing (instruction-level log with configurable filters — e.g. "log every write to
  this MMIO register") writes through the `tracing` infrastructure from prompt 02, not a bespoke
  logging mechanism.

## Responsibilities

1. `crates/debugger`: `Breakpoints` registry (execution/read/write/conditional), a `DebugHooks`
   layer that `system-*` crates' `Bus`/`Cpu` step loops check against (low-overhead when
   inactive), disassembly rendering built on each CPU crate's `CpuIntrospect` disassembler
   (prompts 03–05), memory-viewer data access (read arbitrary address ranges from a live or
   paused `System` for display), GDB-remote-subset TCP server.
2. `egui` debugger panel in `frontend-native` (prompt 14 provides the chrome framework this
   plugs into): register view, disassembly view with current-PC highlight and breakpoint toggle,
   memory hex viewer, execution trace log view, breakpoint list management.
3. Integration hooks in each `system-*` crate (11–13) wherever `Cpu::step`/`Bus` access happens,
   sufficient for breakpoints/watchpoints to actually intercept execution — this requires a small,
   deliberate touch point in each system crate's step loop; keep it minimal and behind the
   null-object/feature-gate pattern above.

## Interfaces

```rust
pub struct Breakpoints { /* address sets, watchpoint ranges, conditional exprs */ }
pub trait DebugHooks {
    fn on_execute(&mut self, pc: u32) -> DebugAction; // Continue | Break
    fn on_bus_read(&mut self, addr: u32, width: u8) -> DebugAction;
    fn on_bus_write(&mut self, addr: u32, width: u8, value: u64) -> DebugAction;
}
```
`system-*` crates' step loops call through a `&dyn DebugHooks` (or a null-object default with no
registered breakpoints, optimized to a no-op) at the appropriate points.

## Constraints

- Zero measurable overhead in the default (no breakpoints registered, debugger panel closed)
  case — verify this claim once prompt 18's profiling tooling exists rather than asserting it
  without evidence.
- GDB-remote server is opt-in, never listening by default (security/footgun consideration: don't
  open a network socket in a desktop game emulator without explicit user action).
- `crates/debugger` depends on `core-common` and the `CpuIntrospect`/`Bus` traits, not on
  `winit`/`wgpu`/`egui` directly — the `egui` panel implementation lives in `frontend-native`,
  consuming `debugger`'s API, keeping the same separation `frontend-core`/`frontend-native` have
  elsewhere (prompt 14).

## Deliverables

- `crates/debugger` fully implemented per Responsibilities.
- Working `egui` debugger panel in `frontend-native`.
- GDB-remote-subset server, manually verified against at least one real GDB-protocol-compatible
  client attach.

## Acceptance Criteria

- Set an execution breakpoint at a known address in a running test ROM, verify execution actually
  halts there and register/memory state displayed matches expected values.
- Set a write watchpoint on a known MMIO register, verify it triggers on the expected write and
  not on unrelated writes.
- Disassembly view correctly disassembles a representative sample of ARM/THUMB/SM83 instructions
  around the current PC, cross-checked against the CPU crates' own disassembler unit tests
  (prompts 03–05) so there's no divergence between "what the debugger shows" and "what the CPU
  crate's own disassembler test suite asserts is correct."
- No measurable frame-time regression with debugger inactive (breakpoints registered: zero,
  panel: closed) versus a build without the debugger hooks compiled in at all — if this can't be
  demonstrated, the null-object/feature-gate design needs revisiting.

## Testing Requirements

- Unit tests for breakpoint/watchpoint matching logic (address, range, conditional expression
  evaluation).
- Integration test: attach a scripted debugger session to `frontend-headless` running a known test
  ROM, set breakpoints, verify correct halt/inspect behavior end-to-end.
- Manual GDB-remote-subset verification against a real client.

## Future Compatibility

The `DebugHooks` interception points established here are also the natural foundation for
cheat-code support (memory-write interception/override) and TAS/movie-recording tooling (input-
and-state trace capture) if those are pursued later (see prompt 18's "future feature" list) —
design the hook points with that reuse in mind without building those features now.

## Notes

Prioritize the in-app `egui` panel for everyday contributor use; treat the GDB-remote server as
a lower-priority "nice to have for power users/IDE integration" within this prompt's scope if
time-constrained — the in-app panel is what most contributors debugging a PPU/CPU accuracy issue
will actually reach for day to day.
