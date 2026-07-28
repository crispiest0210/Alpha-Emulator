# Prompt 05 — ARM946E-S CPU Core (`cpu-arm946e`)

Read `00-INDEX-AND-ARCHITECTURE.md`, `02-core-framework.md`, and `04-cpu-arm7tdmi.md` first.
This prompt builds on the ARM7TDMI implementation rather than starting from zero — do not
reimplement ARM/THUMB decode logic from scratch.

## Objective

The NDS's ARM9 CPU core: an ARM946E-S (ARMv5TE architecture, adds the E-variant DSP extensions,
CP15 system control coprocessor, and configurable instruction/data caches + tightly-coupled
memory). Used only by `system-nds` (prompt 13) — GBA does not have this CPU.

## Context

Nintendo DS's dual-CPU design (ARM9 "main" + ARM7 "coprocessor," communicating via shared memory
and IPC hardware) is the whole reason `cpu-arm7tdmi` was built as a standalone, reusable crate in
prompt 04 rather than folded directly into a GBA-only crate. This prompt is the second consumer
that proves that decision out.

## Architectural Decisions

- **Do not duplicate the ARM/THUMB decode tables from `cpu-arm7tdmi`.** ARMv5TE is a superset of
  ARMv4T (adds e.g. `BLX`, `CLZ`, saturating arithmetic, enhanced DSP multiply instructions). The
  implementer should factor the shared decode/execute logic so `cpu-arm946e` extends rather than
  forks `cpu-arm7tdmi` — options include: (a) `cpu-arm7tdmi` exposes its core execution logic as
  a reusable module/trait that `cpu-arm946e` composes with additional ARMv5TE opcode handling, or
  (b) both crates depend on a shared internal `arm-core` set of building blocks factored out once
  the overlap is concretely known. Choose based on how much genuinely overlaps once you're
  implementing — don't over-engineer a shared-base abstraction speculatively before writing any
  ARMv5TE code, but don't copy-paste the ARMv4T tables either. If you factor out a shared crate,
  update `00-INDEX-AND-ARCHITECTURE.md §3`'s crate list to record it.
- CP15 (system control coprocessor) is implemented for real, not stubbed: cache control
  registers, protection unit / MPU-adjacent config as the NDS ARM9 actually uses it (the DS does
  not use full MMU-style virtual memory the way later ARM cores might — verify the exact CP15
  subset the NDS ARM9 exposes before implementing, do not assume a full ARMv5 MMU).
- Instruction/data cache and TCM (tightly-coupled memory) **do need functional modeling**
  (software on real hardware relies on TCM behavior and cache-control side effects, e.g. DMA-
  before-cache-flush bugs some games work around), but cycle-exact cache timing is explicitly
  **not** a v1 requirement — model functional correctness (what data is visible where) accurately;
  defer cache-timing-accurate cycle costs to a later performance/accuracy pass (cross-reference
  prompt 18) rather than blocking NDS bring-up on it.

## Responsibilities

- `Arm946e<B: Bus>` composing/extending the ARMv4T execution core with: ARMv5TE additions
  (`BLX`/`BLX2`, `CLZ`, `QADD`/`QSUB`/`QDADD`/`QDSUB`, enhanced `MLA`/`SMLAxy` DSP multiply
  family, `LDRD`/`STRD`), CP15 register set and its documented side effects (cache
  enable/disable, TCM base/size configuration, protection region config), and TCM as addressable
  memory regions exposed through the `Bus`/`MemoryRegion` machinery from prompt 02/06.
- Exception model: same shape as ARM7TDMI's but confirm any ARMv5-specific differences (e.g.
  prefetch abort handling nuances) against the ARM946E-S TRM.
- `CpuIntrospect` and `Savable`, following the pattern from prompts 03–04, extended to cover CP15
  register state and cache/TCM configuration (not full cache *contents* unless you've chosen to
  model cache timing — functional-correctness-only caches may not need their contents serialized
  at all if they're purely a performance model with no externally visible state beyond what's
  already in main memory; decide based on your actual implementation and document the reasoning
  in the code).

## Interfaces

Same shape as prompts 03/04: `impl Cpu for Arm946e`, `impl CpuIntrospect`, `impl Savable`.

## Constraints

- No NDS-system-specific behavior (no PPU/3D-core/IPC-hardware knowledge) — this crate is "the
  CPU," full stop, same rule as prompt 04.
- `#![deny(unsafe_code)]` unless justified.

## Deliverables

- `crates/cpu-arm946e` implementing the ARMv5TE instruction set delta over ARMv4T, CP15,
  functional cache/TCM modeling.
- If a shared base was factored out of `cpu-arm7tdmi`, that refactor is part of this prompt's
  deliverable too, including updating prompt 04's crate (and its own tests must still pass
  unmodified in behavior).

## Acceptance Criteria

- Passes whatever community-standard ARM9/NDS CPU accuracy test ROMs exist at implementation
  time (equivalent in spirit to `arm7wrestler` for ARM7TDMI — research current options, e.g.
  ARM9-aware test suites from the NDS homebrew/emulation community) via the accuracy harness.
- `cargo test -p cpu-arm946e` green, and `cargo test -p cpu-arm7tdmi` still green if shared code
  was refactored.

## Testing Requirements

- Unit tests for every ARMv5TE-added instruction (saturating arithmetic edge cases/flag
  behavior, `CLZ` correctness, `LDRD`/`STRD` alignment requirements).
- CP15 register read/write behavior tests, including documented reset values.
- Integration tests once `system-nds` (prompt 13) exists to actually drive this CPU against real
  memory/interrupt wiring — full end-to-end correctness can't be fully proven in isolation, note
  this explicitly rather than overclaiming completeness at this prompt's boundary.

## Future Compatibility

This is the last CPU core needed for the four target systems. If a fifth system is ever added
later (explicitly out of scope now, but the architecture should not preclude it), it should be
able to reuse or extend these three CPU crates the same way NDS reused GBA's.

## Notes

Resist scope creep into cycle-exact cache timing here — it is a known deep rabbit hole for NDS
emulators and functional correctness is the right bar for getting `system-nds` (prompt 13)
bootable and accurate at the instruction/memory-visibility level first.
