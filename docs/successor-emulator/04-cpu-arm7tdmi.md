# Prompt 04 — ARM7TDMI CPU Core (`cpu-arm7tdmi`)

Read `00-INDEX-AND-ARCHITECTURE.md` and `02-core-framework.md` first.

## Objective

A complete ARM7TDMI interpreter supporting both the ARM (32-bit) and THUMB (16-bit) instruction
sets, all seven operating modes and banked registers, and the exception model. This crate is
consumed twice: as the sole CPU of `system-gba` (prompt 12), and as the **ARM7 coprocessor CPU**
of `system-nds` (prompt 13) — NDS's second CPU is a close relative of the GBA's, and sharing this
crate between them is a deliberate architectural payoff of building GBA before NDS.

## Context

The predecessor's core (IodineGBA) implemented exactly this CPU, opaquely, as part of an
undifferentiated blob with no reuse story — it could not have been shared with an NDS
implementation even in principle, because nothing about it was factored as "the CPU" versus "the
GBA." This crate is the concrete payoff of not repeating that: get the ARM7TDMI implementation
right once, and prompt 13 gets its ARM7 side essentially for free.

## Architectural Decisions

- Interpreter (see prompt 03's note on dynarec being out of scope for v1).
- Two dispatch tables: ARM-mode (32-bit-aligned, condition-coded) and THUMB-mode (16-bit), with
  mode switching driven by the `T` bit in CPSR and `BX`/exception entry, exactly as hardware
  defines it. Do not implement THUMB as "a lowering to ARM equivalents" — implement it directly
  against the same register file for both correctness and performance reasons; a translation
  layer adds a whole class of subtle bugs for no benefit here.
- Register banking (FIQ/IRQ/SVC/ABT/UND each bank a subset of R8–R14 + SPSR) implemented as
  storage indexed by current mode, not by copying registers in and out on mode switch — copying
  is the classic source of hard-to-find banking bugs.
- The **prefetch/pipeline timing model is explicit, not incidental.** ARM7TDMI has a 3-stage
  pipeline visible in cycle timing (PC reads are 8/12 bytes ahead depending on mode) and in a few
  documented edge cases (e.g. reading PC as an operand). Model PC-relative behavior correctly per
  the ARM7TDMI datasheet; don't guess.
- This crate does **not** implement the GBA's memory wait-state model, DMA, or interrupt
  controller — those are bus/system concerns (prompts 06, 12). This crate only implements the CPU
  proper and the `Bus` calls it issues.

## Responsibilities

- `Arm7Tdmi<B: Bus>`: full register file with banking, CPSR/SPSR, ARM+THUMB decode/execute for
  the complete instruction sets (data processing, multiply/multiply-long, single/multiple
  data transfer (LDR/STR/LDM/STM), branch/branch-exchange, software interrupt, coprocessor stubs
  — GBA/NDS-ARM7 have no coprocessor hardware behind these opcodes, so they should trap/no-op per
  spec, not be silently unimplemented).
- Exception entry/exit for reset, undefined instruction, SWI, prefetch abort, data abort, IRQ,
  FIQ — correct mode switch, link register adjustment (the offset varies by exception type —
  verify against the ARM7TDMI datasheet, this is a common off-by-N-bytes bug), and CPSR
  save/restore via SPSR.
- Condition-code evaluation for all 16 ARM condition codes, applied uniformly across the
  instruction set including conditional execution of otherwise-unconditional-looking THUMB
  wrapper cases (branches).
- `CpuIntrospect`: register dump (including banked-but-inactive registers, useful for the
  debugger), disassembler for both ARM and THUMB encodings.
- `Savable`: full register file (all banks), CPSR/SPSR, pipeline-visible state if your
  implementation models the pipeline explicitly enough that it has observable state beyond "next
  PC to fetch" (most interpreter implementations don't need to serialize pipeline stages
  separately — only serialize what actually varies your `step` behavior across a save/load
  boundary).

## Interfaces

Same shape as prompt 03: `impl Cpu for Arm7Tdmi`, `impl CpuIntrospect`, `impl Savable`. Expose a
constructor parameterized on initial CPU mode/entry state, since GBA boot (starts in a specific
mode/PC per BIOS) and NDS ARM7 boot differ.

## Constraints

- No GBA- or NDS-specific behavior (no BIOS HLE calls, no PPU/DMA knowledge). If you find
  yourself special-casing "well on GBA this register does X," stop — that belongs in
  `system-gba`.
- `#![deny(unsafe_code)]` unless justified.

## Deliverables

- `crates/cpu-arm7tdmi` with full ARM+THUMB coverage, exception model, register banking,
  disassembler.

## Acceptance Criteria

- Passes `arm7wrestler` (or the current community-standard ARM7TDMI instruction-correctness test
  ROM at implementation time) via the accuracy harness (prompt 17).
- Passes the GBA-suite CPU timing tests (`gba-tests`/`gba-suite`'s CPU section, or equivalent
  current tooling) for both ARM and THUMB cycle counts.
- `cargo test -p cpu-arm7tdmi` green with per-instruction-family unit tests.

## Testing Requirements

- Unit tests per instruction family (data processing with each shifter-operand type, LDM/STM
  addressing-mode variants and writeback, multiply cycle-count variants, branch/BX mode
  switching) for both ARM and THUMB where applicable.
- Exception-entry tests verifying correct link-register offset and mode/CPSR transition per
  exception type.
- Accuracy-ROM integration tests via `testing/harness`.

## Future Compatibility

Prompt 13 (NDS) instantiates this exact crate for the ARM7 side of the dual-CPU system — any
GBA-specific assumption smuggled in here becomes NDS's problem later. When in doubt, keep it
generic and let `system-gba`/`system-nds` supply the difference via the `Bus` implementation and
boot-state constructor arguments.

## Notes

The register-banking and exception-offset details are the single most common source of subtle
ARM7TDMI emulator bugs (they're easy to get "mostly right" and wrong in one mode). Write the
banking table and exception-offset table as literal, commented constants cross-checked against
the ARM7TDMI Technical Reference Manual, not derived from memory or from reading another
emulator's source.
