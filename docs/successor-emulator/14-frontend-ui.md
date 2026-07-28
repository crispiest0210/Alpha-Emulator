# Prompt 14 — Frontend / UI (`frontend-core`, `frontend-native`)

Read `00-INDEX-AND-ARCHITECTURE.md` and prompts 02, 09, 10, and (for whichever systems exist by
the time this is implemented) 11–13 first.

## Objective

`frontend-core` (windowing/GPU-agnostic session orchestration: owns the emulation thread, audio
pipeline, input routing, rewind buffer, ROM/save library) and `frontend-native` (the actual
shipped desktop application: `winit` window, `wgpu` rendering of the emulated framebuffer(s) plus
`egui`-based chrome — library browser, HUD, keybind configurator, settings). This is the
user-facing product; it must deliver on the product requirements preserved from the predecessor
project per `00-...md` §1.

## Context

The predecessor's entire frontend was one ~2,200-line React component doing UI state, canvas
drawing, Web Audio glue, keyboard routing, and Tauri IPC in one place (lesson §2). This prompt is
the structural fix: `frontend-core` contains zero windowing/rendering code and is unit-testable
without a display; `frontend-native` contains only presentation and input capture, delegating all
session/emulation logic to `frontend-core`. If a future web or TUI frontend is ever built, it
should be able to reuse `frontend-core` entirely.

## Architectural Decisions

- `frontend-core` owns: the emulation thread (running whichever `System` implementation is
  active, driven by `step_frame` in a loop paced to the system's native frame rate, with
  fast-forward as an explicit uncapped or multiplier mode — not "run the UI loop faster"), the
  audio pipeline from prompt 09, input resolution from prompt 10 delivered once per frame, a
  rewind ring buffer of periodic save states (see prompt 16 for the save-state format this
  builds on; rewind depth is a configurable number of seconds/frames traded against memory use),
  and the `library` crate's SQLite-backed ROM/save index (see below).
- `frontend-native` owns: `winit` event loop, `wgpu` surface presentation of the framebuffer(s)
  produced by `frontend-core` (nearest-neighbor by default; integer/shader scaling as a
  configurable presentation option — this is where predecessor-lesson-inspired "keep the raw
  framebuffer pure, do presentation scaling in the frontend" pays off), `egui` chrome: ROM
  library browser, save-state list/management UI, in-game HUD (toggle key reserved per prompt
  10's precedence rule, not overloaded onto an emulated button), keybind configurator UI.
- **Library persistence (fixes predecessor lesson §5):** `crates/library` wraps `rusqlite` to
  maintain a durable index of ROM metadata (path, title, platform, last-played) and save-state
  metadata (path, ROM association, label, timestamp, thumbnail if in scope). On startup, the
  library reconciles this index against the filesystem (detect files added/removed/moved outside
  the app) rather than *rebuilding the index from a directory scan every launch* — the index is
  the source of truth; the scan is a reconciliation pass against it, not a replacement for it.
  Session state (currently running system, paused/running) remains in-memory only, same as the
  predecessor got right — that distinction (durable library index vs. ephemeral session state)
  is worth keeping, only the *library* half needs the durability fix.
- ROM/save storage location follows the same OS-appropriate local-app-data convention the
  predecessor used (documented per-OS paths) — that part of the predecessor's design was sound
  product behavior, just backed by a better index now.
- Dual-screen NDS presentation (from prompt 13) is handled here: two framebuffers stacked/
  arranged in the native window, matching real hardware's dual-screen layout, with the coordinate
  mapping used for touch-input translation living in this crate (converting window/mouse
  coordinates on the bottom-screen region into the touch coordinates prompt 10's `InputState`
  expects).

## Responsibilities

1. `crates/library`: SQLite schema, ROM/save indexing, filesystem reconciliation, CRUD operations
   for ROM entries and save-state entries (including deletion from both index and disk, matching
   the predecessor's working delete-from-both-UI-and-disk behavior).
2. `crates/frontend-core`: emulation thread lifecycle (start/pause/resume/stop/switch-ROM),
   audio pipeline integration (prompt 09), input integration (prompt 10), rewind buffer, library
   integration, a clean command/event API consumed by `frontend-native` (e.g. an mpsc/crossbeam
   channel pair: commands in, frame-ready/status events out — no shared-mutex polling from the UI
   thread).
3. `crates/frontend-native`: window/render setup, framebuffer presentation, `egui` chrome (ROM
   library view, save management, HUD overlay, keybind configurator per prompt 10), settings
   persistence (keybinds, volume/mute, presentation scaling mode) via the TOML config decision
   from `00-...md`.
4. `crates/frontend-headless`: minimal CLI driver (load ROM, run N frames, dump framebuffer/audio
   or accept scripted input) — this exists primarily to serve prompt 17's accuracy harness and
   any future scripted/movie playback (prompt 15/18-adjacent), but should be implemented as part
   of this prompt since it shares `frontend-core` and is comparatively small once that exists.

## Interfaces

```rust
// frontend-core
pub enum SessionCommand { LoadRom(RomId), Pause, Resume, Stop, SaveState(Label), LoadState(SaveId),
    SetInput(InputState), SetFastForward(bool), Rewind }
pub enum SessionEvent { FrameReady, StatusChanged(SessionStatus), Error(String) }
pub struct Session { /* owns emulation thread handle, command sender, event receiver */ }
```
Exact shape is the implementer's call; the contract is a clean, blocking-free channel boundary
between `frontend-native`'s UI thread and `frontend-core`'s emulation thread.

## Constraints

- `frontend-core` has zero dependency on `winit`/`wgpu`/`egui`.
- No God Component: `frontend-native`'s top-level app struct should be a thin composition of
  clearly separated modules (window/render setup, chrome panels, input capture) — if any single
  file in `frontend-native` starts approaching predecessor-`App.tsx` scale (a soft signal at
  several hundred lines doing unrelated things), that's a signal to split it, not a target to
  hit exactly.
- Save-state and library UI must support the predecessor's proven-good product behaviors: list,
  load-to-exact-frame, delete-from-UI-and-disk, per-ROM save organization, `lastPlayedAt`
  tracking.

## Deliverables

- `crates/library`, `crates/frontend-core`, `crates/frontend-native`, `crates/frontend-headless`
  fully implemented.
- A working desktop application: import/drag-drop ROMs, persistent library, gameplay with audio/
  video/input, quicksave/quickload, rewind, HUD, keybind configuration, all functioning
  end-to-end for whichever systems (11–13) exist at implementation time.

## Acceptance Criteria

- Fresh install → `cargo xtask dev` → drag a ROM in → it plays with audio and correct input →
  quicksave → quickload → rewind → all functioning, manually verified (per the general project
  instruction to actually exercise UI features in a running app before calling this done, not
  just type-check/compile-check it).
- Restarting the app preserves the ROM library and save-state list without requiring re-import
  (directly verifies the library-index fix for predecessor lesson §5).
- No frame stutter/audio glitch attributable to UI-thread work blocking the emulation thread
  (verifies lesson §3's fix) under normal manual play-testing.

## Testing Requirements

- `frontend-core`: unit/integration tests for session lifecycle transitions, library reconciliation
  logic (add/remove/move a file on disk between launches, verify correct index reconciliation).
- `frontend-native`: manual UI verification is expected and required per general project
  guidelines; automated UI testing is not mandated but keybind-resolution logic (prompt 10) and
  coordinate-mapping logic (touch translation) should have unit tests since they're pure
  functions independent of the actual window.
- `frontend-headless`: integration-tested implicitly by being the harness prompt 17 relies on.

## Future Compatibility

Any future alternate frontend (web/WASM build, TUI) should be able to depend on `frontend-core`
and `library` directly, implementing only its own thin presentation layer — that reuse potential
is the entire reason for this crate split, so don't erode it by letting session/emulation logic
leak into `frontend-native`.

## Notes

This prompt is explicitly instructed by the project-wide agent guidelines to be verified by
actually running the app and exercising the golden path and edge cases in a real window, not by
compilation success alone.
