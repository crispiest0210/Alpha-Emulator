# Prompt 10 — Input System

Read `00-INDEX-AND-ARCHITECTURE.md` and `02-core-framework.md` first.

## Objective

Define and implement the input abstraction layer: the `InputState` type's full contract (started
in prompt 02), keybind mapping from physical input devices (keyboard, and gamepad if in scope —
see Constraints) to logical per-system buttons, NDS touch-screen input, and the plumbing that
gets input from `frontend-native`'s event loop into the emulation thread each frame without
blocking it.

## Context

The predecessor's input handling was a real, specific bug source: keyboard event listeners had
to be patched to intercept emulator control keys during the bubbling phase to avoid "legacy
dual-handler event hijacking," and there was a documented conflict between the GBA SELECT button
and HUD-menu-toggle both wanting the same key before being untangled. Both are symptoms of input
routing being ad hoc rather than a designed layer with one clear owner per key. This prompt
exists so that doesn't happen again structurally.

## Architectural Decisions

- Two-layer model: **physical input** (raw keyboard scancodes / gamepad button events from
  `winit`) → **logical input** (`InputState` from prompt 02: per-system button set + optional
  touch point) via an explicit, user-configurable keybind map. Frontend chrome (HUD toggle,
  pause, debugger shortcuts) is a *separate* set of bindings that intentionally never overlaps
  with emulated-button bindings, and the mapping/precedence rule between them (e.g. "HUD toggle
  key is reserved and cannot be rebound to a GBA button, and vice versa") is enforced at the
  keybind-config layer, not discovered ad hoc at runtime the way the predecessor's SELECT/HUD
  conflict was.
- `InputState` construction happens once per frame on the UI/input thread (where `winit` events
  arrive) and is handed to the emulation thread as a plain, `Copy`-able value via the same
  channel mechanism `frontend-core` uses for other cross-thread communication (prompt 14) — no
  shared mutable input state polled from two threads.
- NDS touch input is modeled as an `Option<(u8, u8)>` (or system-appropriate coordinate type)
  field on `InputState` alongside button state, translated from mouse/trackpad/touchscreen events
  by `frontend-native`, sourced from stylus-equivalent input on desktop (mouse click-drag mapped
  to touch-down/touch-move/touch-up).
- Keybind configuration persists as part of the user's local config (TOML via the `config`
  decision in `00-...md`), keyed per logical action, loaded/saved by `frontend-core` or
  `frontend-native` (implementer's call which owns the file I/O; keep the *mapping logic* itself
  free of file-I/O concerns so it's unit-testable).

## Responsibilities

- Finalize `InputState`'s field set in `core-common` (prompt 02 sketched it; this prompt owns
  getting it right for all four systems' actual button sets, including GBA's L/R shoulder
  buttons and NDS's touch input, without over-generalizing into a format so generic it loses
  type safety — e.g. don't collapse to a single `HashMap<String, bool>`).
- Keybind map type: physical-key → logical-action, with sensible defaults matching common
  emulator conventions (documented per system), fully rebindable.
- Frontend-chrome-vs-emulated-input precedence/conflict rule, enforced in code (reject/warn on
  attempting to bind a reserved chrome action's key to an emulated button, or vice versa) not
  just by convention.
- Cross-thread delivery: input captured on the UI thread, delivered to the emulation thread once
  per frame boundary, non-blocking.

## Interfaces

```rust
// core-common
pub struct InputState {
    pub buttons: ButtonSet, // per-system logical buttons, bitflags-style
    pub touch: Option<TouchPoint>, // NDS only; None on other systems
}
```
```rust
// input layer (frontend-core or a small dedicated module — implementer's call on placement)
pub struct KeybindMap { /* physical key -> logical action */ }
impl KeybindMap {
    pub fn resolve(&self, physical_events: &[PhysicalInputEvent]) -> InputState { ... }
}
```

## Constraints

- No system-specific `if system == Gba` branching inside `core-common`'s `InputState` — model it
  as a superset struct with fields that are simply unused/`None` for systems that don't have that
  input type (this mirrors how `Framebuffer`/`AudioSample` were handled in prompt 02).
- Gamepad support: include it if `winit`'s ecosystem (via `gilrs` or similar) makes it low-cost
  to add alongside keyboard support in this same pass; if it meaningfully expands scope, it is
  acceptable to ship keyboard-only now and note gamepad support as explicit future work rather
  than blocking this prompt — implementer's judgment call, but state the decision explicitly in
  the crate's doc comments either way.

## Deliverables

- Finalized `InputState` type.
- `KeybindMap` with default bindings per system, rebind support, persisted config.
- Working keyboard input path from `winit` event loop through to the emulation thread, verified
  manually (or via an integration test that feeds synthetic `PhysicalInputEvent`s and asserts the
  resulting `InputState`).

## Acceptance Criteria

- A configured keybind conflict (same physical key bound to both a chrome action and an emulated
  button) is rejected or resolved by an explicit, tested precedence rule — not left to whichever
  handler happens to run first, which was the exact predecessor bug class this prompt exists to
  prevent.
- Input latency from physical key event to `InputState` reaching the emulation thread is at most
  one frame under normal operation (verify by reasoning about the channel/threading design, not
  necessarily an automated timing test).

## Testing Requirements

- Unit tests for `KeybindMap::resolve` against synthetic physical-event sequences.
- Unit test for the reserved-chrome-key-vs-emulated-button conflict rule.
- Manual verification in `frontend-native` once prompt 14 exists: play a GB/GBA test ROM end-to-
  end using keyboard input.

## Future Compatibility

Prompt 14 (frontend) consumes this layer directly for gameplay input and for its own HUD/menu
navigation, and prompt 15 (debugger) may add its own reserved shortcut set following the same
precedence-rule pattern established here.

## Notes

Default keybind choices should look to common conventions already validated by the predecessor
project's default layout (WASD d-pad, Space/R for A/B, etc.) as a reasonable starting point for
user familiarity, but that's a product/UX choice for prompt 14 to finalize, not an architectural
constraint of this prompt.
