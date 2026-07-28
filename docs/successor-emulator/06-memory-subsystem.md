# Prompt 06 — Memory Subsystem

Read `00-INDEX-AND-ARCHITECTURE.md` and `02-core-framework.md` first.

## Objective

Implement the concrete `Bus`/`MemoryRegion` wiring, MMIO dispatch, and DMA controllers for each
system, on top of the generic traits from prompt 02. This prompt spans code living in each
`system-*` crate (memory-map assembly) plus `cart-common` (cartridge/mapper abstraction shared
across systems). It is listed separately from prompts 11–13 because the memory-map/DMA design is
substantial enough to deserve its own focused pass before full system assembly, and because
`cart-common` is shared infrastructure, not system-specific.

## Context

Cartridge save handling was one of the more organically-grown parts of the predecessor (save
chip serialization folded into the same manual internal-reflection savestate code as everything
else). This project separates "how a mapper banks ROM/RAM" from "how battery-backed save data
persists to disk" as two distinct concerns from the start.

## Architectural Decisions

- `cart-common` defines `Cartridge` (header parsing: title, checksum validation, region/version
  fields per system's header format) and a `Mapper` trait: `read/write` against cartridge address
  space, bank-switching state. Each mapper type (GB: MBC1/MBC2/MBC3/MBC5(+ possible MBC1M multicart
  variant)/rumble variants; GBA: none needed for ROM banking but SRAM/Flash/EEPROM save variants
  plus GPIO for RTC carts; NDS: similar save-chip variants plus slot-2 GBA-mode considerations if
  in scope — confirm scope with prompt 13) implements `Mapper`.
- `BatteryBackedSave` trait, separate from `Mapper`: `read_byte`/`write_byte`/`as_bytes`/
  `load_from_bytes`, so the *file on disk* representation of save RAM/Flash/EEPROM contents is a
  clean, independently-testable concern from the *address-space banking logic* that exposes it to
  the CPU. This is the direct fix for predecessor lesson §1 as it applies to cartridge saves
  specifically: no reflection into a mapper's private fields to extract save data — the mapper
  hands you a `BatteryBackedSave` implementor with its own explicit serialization.
- RTC (real-time clock, present on some GB/GBC carts via MBC3, and on some GBA carts via GPIO) is
  modeled as its own small stateful component implementing `Savable` directly — do not fold RTC
  register state into general save-RAM bytes; it has its own semantics (BCD time fields, latch
  behavior) that deserve a real type.
- Per-system memory maps (WRAM, VRAM, OAM, palette RAM, I/O register windows, cartridge windows,
  BIOS/boot-ROM window) are assembled in each `system-*` crate using `MemoryRegion` composition
  from prompt 02 — this prompt defines the pattern with GB (the simplest map) and documents it
  well enough that prompts 11–13 can follow it for GBC/GBA/NDS without re-deriving the approach.
- DMA controllers are modeled as scheduler-driven components (prompt 02's `Scheduler`), not
  as synchronous "do the whole transfer in one CPU-visible instant" — GBA/NDS DMA has cycle-cost
  and mid-transfer timing implications (e.g. HBlank/VBlank-triggered DMA, sound FIFO DMA) that
  matter for accuracy, and modeling it as scheduled events makes it composable with the same
  scheduler PPU/timer events use.

## Responsibilities

1. `crates/cart-common`: `Cartridge`, `Mapper` trait + concrete GB mapper implementations
   (MBC1/2/3/5 at minimum — document which multicart/rare variants are explicitly out of scope
   for v1), `BatteryBackedSave` trait + SRAM/Flash/EEPROM implementations, RTC component.
2. GB/GBC memory map assembly (used by prompt 11): establish the `MemoryRegion` composition
   pattern (echo RAM mirroring, MMIO register block, VRAM banking for GBC, WRAM banking for GBC).
3. Document (in this crate's doc comments, not a separate design doc) the DMA-as-scheduled-event
   pattern with a concrete example, since prompts 12/13 implement the actual GBA/NDS DMA
   controllers against this pattern.
4. Open-bus / unmapped-region read behavior defined explicitly per system where it's
   behaviorally relevant (GBA in particular has well-documented open-bus read behavior that some
   games/test-ROMs depend on) — do not default to returning 0 silently.

## Interfaces

`Mapper`, `BatteryBackedSave`, `Cartridge` in `cart-common`; per-system `Bus` implementations in
each `system-*` crate built from `core-common::MemoryRegion`.

## Constraints

- `cart-common` has no dependency on any specific `system-*` crate — it's shared infrastructure,
  imported by them, never the reverse.
- Save-file-on-disk format for battery-backed saves should be the *raw* chip contents (e.g. exact
  SRAM bytes), independently loadable by other tools/emulators where practical — don't wrap it in
  a project-specific container format at this layer (the project's own richer save-state format,
  which does include this data among everything else, is prompt 16's concern, not this one).

## Deliverables

- `crates/cart-common` fully implemented for GB-family mappers/saves/RTC.
- GB/GBC memory map assembled and documented as the reference pattern.
- GBA/NDS mapper and save-chip variants (SRAM/Flash/EEPROM/GPIO-RTC) implemented in `cart-common`
  even though full GBA/NDS memory-map assembly happens in prompts 12/13 — the *mapper logic* is
  shared infrastructure and belongs here.

## Acceptance Criteria

- `cargo test -p cart-common` covers each mapper's bank-switching behavior against known-correct
  address-decoding rules (cross-check against Pan Docs for GB mappers, GBATEK for GBA save
  variants).
- A round-trip test: write through a mapper's `BatteryBackedSave`, serialize to bytes, reload
  into a fresh instance, verify identical subsequent read behavior.
- GB memory map: a unit test proving echo-RAM mirroring and I/O register addressing match Pan
  Docs' memory map exactly.

## Testing Requirements

- Mapper bank-switching unit tests per mapper type.
- RTC latch/BCD-field behavior unit tests.
- Save round-trip tests (this is a direct regression guard against the predecessor's save-
  corruption bug class).

## Future Compatibility

Prompts 11–13 depend on `cart-common` being complete for their respective systems' save-chip
variants before those system prompts can be considered done — but GBA/NDS *mapper logic* here
can be implemented now, ahead of full system assembly, since it has no dependency on PPU/CPU
wiring.

## Notes

GBATEK (for GBA/NDS) and Pan Docs (for GB/GBC) are the authoritative references for memory-map
and save-chip behavior — verify address ranges and mapper semantics against them, not against
recollection of how other emulators do it.
