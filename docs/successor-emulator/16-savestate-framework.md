# Prompt 16 — Save-State Framework (`savestate`)

Read `00-INDEX-AND-ARCHITECTURE.md` (especially §1 lesson 1 and §4) and prompt 02 first. This
prompt should land early relative to prompts 03–13 in practice — those prompts assume the
`Savable` trait already exists — but is documented here, in sequence with the other cross-cutting
topics, for narrative clarity. Coordinate actual implementation ordering with prompt 02 (the
`Savable` trait bound needs to exist by the time any `Cpu`/`Bus`/`System` implementation is
written against it).

## Objective

Define and implement the `Savable` trait, a versioned binary save-state container format built
on it, and the rewind-buffer mechanism `frontend-core` (prompt 14) uses for its rewind feature.

## Context

This is the single most direct architectural response to the predecessor's most concrete,
documented bug class: `exportGbaState`/`importGbaState` in the predecessor's `src/App.tsx`
manually reached into the vendored core's private object graph (`io.cpu.branchFlags.getNZCV()`,
per-background-renderer internals, `renderer.paletteRAM` fed byte-by-byte back into internal
parser functions) to construct and restore save states, because the vendored core was never
designed to be introspected. This produced real, shipped bugs (corrupted/scrambled tile visuals
on quickload) that required a workaround (force a CPU "warm reboot" after every load) rather than
a fix at the root cause. This project avoids the entire bug class by requiring every stateful
component to own its serialization from the moment it's written.

## Architectural Decisions

- `Savable` trait: `fn save(&self, w: &mut StateWriter); fn load(&mut self, r: &mut StateReader)
  -> Result<(), StateError>;`. `StateWriter`/`StateReader` are thin wrappers around a byte buffer
  (built on `bincode` + `serde` per `00-...md`'s stack decision) providing primitive read/write
  methods plus a `nested(&mut self, tag: &str, |w| ...)` or similar scoping helper so nested
  components (e.g. a `System` containing a `Cpu` containing register state) compose naturally
  without manual offset bookkeeping.
- **Every stateful struct implements `Savable` where it is defined**, not in a separate
  "savestate glue" module bolted on afterward — this is the specific, checkable rule that
  prevents lesson-1's bug class from recurring. Code review (or a lint/test, see Testing
  Requirements) should treat a new stateful field added to an existing `Savable`-implementing
  struct without a corresponding `save`/`load` update as a defect.
- **Versioning:** the container format includes a format version and a per-system schema
  version. Loading an older-version save state either migrates forward (if a migration path is
  implemented) or fails with a clear, user-facing error — never silently misinterprets bytes.
  This did not exist at all in the predecessor (its save format was whatever shape the manual
  reflection code happened to produce, with no version tag).
- **Round-trip fidelity is the correctness bar, verified by property/fuzz testing**: for every
  `Savable` implementor, `save` then `load` into a *freshly constructed* instance must produce a
  struct behaviorally identical to the original (verified by continuing emulation from both and
  comparing subsequent frames, not just by comparing raw field equality, since some fields may
  legitimately be caches/derived data that needn't round-trip exactly as long as behavior does —
  document which fields fall into that category, if any, explicitly).
- **Rewind buffer** (prompt 14 consumes this): a ring buffer of periodic full or delta save
  states. Start with periodic full snapshots (simpler, proven correct by the same `Savable`
  machinery) rather than delta/diff-based snapshots; delta-based rewind (cheaper memory, more
  complex) is an explicit candidate for prompt 18's performance-optimization pass once full-
  snapshot rewind is proven correct and its memory cost is measured to be a real problem, not
  assumed to be one upfront.
- Cartridge save data (battery-backed SRAM/Flash/EEPROM from prompt 06) is included in the full
  save-state container but **also** independently flushable to its own on-disk file in the raw
  chip format (per prompt 06's decision) — the two persistence paths share the underlying
  `BatteryBackedSave::Savable`-compatible serialization but serve different purposes (instant
  full-state resume vs. portable cartridge-save-only files compatible with other tools).

## Responsibilities

1. `crates/savestate`: `Savable` trait, `StateWriter`/`StateReader`, versioned container format
   (header: magic bytes, format version, system identifier, per-system schema version, then the
   `System`'s serialized state), migration hook points for future version bumps.
2. Coordinate with prompt 02 so `core-common`'s `Cpu`/`Bus`/`System` traits carry the `Savable`
   supertrait bound as described there.
3. Rewind ring buffer implementation, consumed by `frontend-core` (prompt 14).
4. A `#[derive(Savable)]` proc macro (optional but recommended given the number of structs that
   will need straightforward field-by-field serialization — evaluate cost/benefit at
   implementation time; if it meaningfully reduces boilerplate and risk of forgetting a field
   across dozens of structs in prompts 03–13, it's worth building) that generates the trait
   impl for structs whose fields are all themselves `Savable` or primitive, falling back to
   manual implementation for structs needing custom logic (e.g. anything with derived/cached
   fields that shouldn't round-trip literally).

## Interfaces

```rust
pub trait Savable {
    fn save(&self, w: &mut StateWriter);
    fn load(&mut self, r: &mut StateReader) -> Result<(), StateError>;
}
pub struct SaveStateContainer { /* header + payload */ }
impl SaveStateContainer {
    pub fn encode<S: Savable>(system: &S, meta: SaveMeta) -> Vec<u8> { ... }
    pub fn decode<S: Savable + Default>(bytes: &[u8]) -> Result<S, StateError> { ... }
}
```

## Constraints

- No reflection, no reaching into another module's private fields to extract state for
  serialization — if a field needs to be saved, it's saved by the struct that owns it, via its
  own `Savable` impl, full stop. This is the one rule in this entire prompt collection most
  directly traceable to a real, shipped predecessor bug — treat it accordingly.
- Save-state files must be forward-diagnosable: a corrupted or version-mismatched file produces
  a clear error, never a panic or silent misload.

## Deliverables

- `crates/savestate` fully implemented per Responsibilities, including the optional derive macro
  if pursued.
- Rewind ring buffer implementation.
- Documentation (in-code) of the versioning/migration convention for future contributors adding
  new system state.

## Acceptance Criteria

- Round-trip determinism test (save → load into fresh instance → run N frames, compare against N
  frames of uninterrupted execution from the same point) passes for every system implemented by
  the time this prompt is evaluated — this is the literal regression test for the predecessor's
  quickload-corruption bug class, and it must be automated (see prompt 17), not just manually
  spot-checked the way the predecessor's fix effectively was.
- Loading a save state with a mismatched/future schema version produces a clear `StateError`, not
  a panic or corrupted load.
- Rewind works correctly across at least a full rewind-buffer-depth window in manual testing via
  `frontend-native`.

## Testing Requirements

- `savestate` crate: unit tests for the container format (version header, corruption detection),
  the derive macro (if built), `StateWriter`/`StateReader` primitive round-trips.
- Per-system round-trip determinism tests (owned by prompts 11–13 but must exist, and this
  prompt's completion should be judged partly on whether those tests are actually wired into
  `testing/harness`, per prompt 17).

## Future Compatibility

Delta/incremental rewind snapshots, cloud-sync of save states, and cross-device save portability
are all plausible future work that should layer on top of the versioned container format defined
here without requiring a format redesign — keep the header/versioning scheme forward-extensible
(e.g. a reserved-for-future-use flags field) even though those features aren't built now.

## Notes

This prompt is arguably the most important single fix relative to the predecessor project's
actual, documented failure history. Do not treat it as boilerplate infrastructure — the
round-trip determinism test described above is the concrete, checkable proof that this project
does not repeat that specific bug class, and it should be one of the first things a reviewer
looks for when evaluating whether this prompt (and, transitively, prompts 11–13) is actually done.
