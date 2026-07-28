# Prompt 03 — Sharp SM83 CPU Core (`cpu-sm83`)

Read `00-INDEX-AND-ARCHITECTURE.md` and `02-core-framework.md` first. This prompt implements
`crates/cpu-sm83` against the `Cpu`, `Bus`, and `Savable` traits defined in prompt 02 — it does
not redefine them.

## Objective

A complete, cycle-accurate-enough Sharp SM83 (the GB/GBC CPU, a hybrid Z80/8080-derived core)
instruction interpreter, usable by both `system-gb` (prompt 11) and `system-gbc` (also prompt
11, as it reuses this crate with a double-speed mode flag).

## Context

This is the smallest, best-understood core of the four systems and is deliberately first: it is
where the team proves out the `Cpu`/`Bus`/`Scheduler` abstractions from prompt 02 cheaply, before
those abstractions get load-bearing across three more, much larger cores. If the abstraction is
wrong, this is the affordable place to discover it.

## Architectural Decisions

- **Interpreter, not a dynarec, for all four CPU cores in this project's first version.**
  Dynamic recompilation is a legitimate later optimization (see prompt 18) but is out of scope
  until correctness and the shared-abstraction story are proven across all four systems.
- Implement as a table-driven interpreter: an opcode dispatch table (array of function
  pointers/closures or a big `match`, implementer's choice — a `match` is usually fine in Rust
  and lets the optimizer inline aggressively, but a table is easier to unit-test opcode-by-opcode;
  pick one and be consistent) covering the full unprefixed + `0xCB`-prefixed instruction sets.
- Per-instruction cycle costs must come from a single authoritative table checked against a
  known-accurate reference (e.g. the Pan Docs / gbdev opcode tables), not hand-derived from
  memory — CPU timing bugs are exactly the kind of thing that's invisible until a test ROM catches
  it, and cheap to get right up front.
- CGB double-speed mode (`KEY1` register) is *not* implemented in this crate — it's a
  clock-multiplier concept that belongs in `system-gbc`'s scheduler wiring (prompt 11), which
  should be able to run this same `Sm83` core unmodified at 2x the base clock. Keep any
  speed-mode awareness out of `cpu-sm83` itself.

## Responsibilities

- `Sm83<B: Bus>` struct: registers (AF/BC/DE/HL/SP/PC, with flag-register bit accessors), an
  `Halted`/`Stopped`/IME (interrupt master enable) state machine, and the EI-delay-by-one-
  instruction quirk (a well-known SM83 accuracy trap — get it right, it's tested by common
  accuracy ROMs).
- Full opcode coverage: 8-bit/16-bit loads, ALU ops (with correct half-carry/carry flag
  semantics — SM83 flag behavior differs subtly from Z80 in a few opcodes, e.g. `DAA` — verify
  against a reference table, don't assume Z80 parity), rotates/shifts, bit ops, jumps/calls/
  returns/rst, `HALT` bug (the documented hardware quirk where `HALT` under certain IME/IF
  conditions causes the next byte to be read twice) — this is a specific, well-known,
  test-ROM-covered piece of behavior; implement it deliberately, not by accident.
- Interrupt handling: the 5-cycle interrupt dispatch sequence, correct priority ordering
  (VBlank > LCD STAT > Timer > Serial > Joypad), and the interaction between `HALT` and pending
  interrupts.
- `CpuIntrospect` implementation for the debugger (prompt 15): register dump, single-instruction
  disassembly at an arbitrary address (needed for the debugger's disassembly view — implement a
  real disassembler here, not a stub, since prompt 15 depends on it).
- `Savable` implementation: every register, IME, halt/stop state — this core has no cache/
  pipeline state to worry about (unlike ARM cores), so its savestate surface is small; get it
  fully correct here since it sets the pattern prompts 04/05 will follow for larger register sets.

## Interfaces

```rust
pub struct Sm83 { /* registers, ime, halted, ... */ }
impl Cpu for Sm83 {
    fn step(&mut self, bus: &mut impl Bus) -> Cycles { ... }
    fn reset(&mut self) { ... }
}
impl CpuIntrospect for Sm83 { ... }
impl Savable for Sm83 { ... }
```
Exact generic bound on `bus` should match whatever prompt 02 settled on (concrete generic vs.
`dyn Bus`) — do not silently diverge from that decision.

## Constraints

- No knowledge of PPU/APU/timer/cartridge specifics — this crate only knows about the CPU and
  the `Bus` trait it's handed. Interrupt *sources* are the memory-mapped IF/IE registers read
  through the bus like anything else; this crate does not know who set IF.
- `#![deny(unsafe_code)]`.

## Deliverables

- `crates/cpu-sm83` fully implemented, all 500-ish SM83 opcodes (unprefixed + CB-prefixed)
  covered, with per-opcode unit tests for cycle counts and flag behavior on representative cases.
- A disassembler sufficient for the debugger.

## Acceptance Criteria

- Passes Blargg's `cpu_instrs` test ROM suite (all 11 sub-tests) and `instr_timing` when run
  through the accuracy harness from prompt 17 — this is the concrete, checkable bar, not "looks
  right."
- Passes the relevant Mooneye Test Suite acceptance tests for CPU timing and the `HALT` bug
  specifically (`halt_ime0_ei`, `halt_ime1_timing`, etc. — check the current Mooneye suite's
  actual test names at implementation time, they are the ground truth, not this prompt).
- `cargo test -p cpu-sm83` green.

## Testing Requirements

- Unit tests: flag behavior for every ALU opcode family (ADD/ADC/SUB/SBC/AND/OR/XOR/CP,
  INC/DEC, `DAA`, `CPL`, `SCF`/`CCF`), cycle counts for every addressing-mode variant, the EI-
  delay quirk, the HALT bug under each documented IME/pending-interrupt combination.
- Integration: full accuracy-ROM runs via `testing/harness` (prompt 17) — this crate's
  acceptance criteria is *defined* by those ROMs, so the harness must exist (or a minimal slice
  of it) before this prompt can be marked complete; coordinate ordering with prompt 17 if it
  hasn't landed yet, or build a temporary local test-ROM runner here and migrate it into
  `testing/harness` once that prompt lands.

## Future Compatibility

`system-gbc` (prompt 11) reuses this crate verbatim at 2x clock — do not bake single-speed
assumptions into cycle math (return costs in this CPU's own native cycle unit; let the caller's
scheduler decide what a "cycle" costs in wall-clock/master-clock terms).

## Notes

The HALT bug and EI-delay quirk are the two most commonly-missed SM83 accuracy details in
hobbyist emulators — Pan Docs and Mooneye's test suite both document them precisely. Do not
skip them as "edge cases"; real commercial games rely on the EI-delay behavior for interrupt
timing.
