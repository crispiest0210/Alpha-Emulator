# Successor Emulator — Master Architecture & Prompt Index

**Read this document before opening any other prompt in this directory.** It is the shared
reference every other prompt in `docs/successor-emulator/` assumes you already know. Each
individual prompt is self-contained for its own deliverable but deliberately does not repeat
the decisions recorded here — it just links back to this file by section name.

This is a **prompt collection for an implementation agent**, not a design doc for humans to
review. If you are that agent: don't re-litigate the decisions below, don't propose alternative
tech stacks "for consideration," don't write a planning document restating this file. Implement.
Where a decision below is silent on a detail, use your engineering judgment and move on.

---

## 0. What this project is

A from-scratch, multi-generation Nintendo handheld emulator ("the successor project"),
written in Rust, supporting:

- **Game Boy (DMG)**
- **Game Boy Color (GBC)**
- **Game Boy Advance (GBA)**
- **Nintendo DS (NDS)**

It is a **clean-room rewrite**, not a fork of the repository these prompts were derived from.
That prior repository (`GBA-Emulator`, a Tauri + React + Rust desktop app) is referenced below
only as a source of lessons — both what to keep and what to deliberately not repeat. Nothing
from that repo is reused as code; IodineGBA (its vendored JS core) is not a dependency of this
project in any form.

## 1. Lessons from the predecessor project

The predecessor was a Tauri v2 + React 19 + TypeScript desktop shell wrapping a vendored,
unmodified third-party JavaScript GBA core (`IodineGBA`) running inside the WebView. Rust's
only job was ROM/save file I/O and process metadata; all emulation, audio mixing, and video
compositing happened in browser JS on the WebView's main thread.

### What worked and should be preserved as *product* behavior (not architecture)

- A persistent, auto-scanning ROM library and save-state library rooted in the OS local-app-data
  directory, discovered on startup rather than requiring manual re-import every session.
- Save checkpoints as full hardware snapshots (not just cartridge SRAM), loadable instantly to
  the exact frame.
- Configurable keybinds, volume/mute, fast-forward, rewind, quicksave/quickload, all reachable
  from an in-game HUD overlay without leaving gameplay.
- Treating the BIOS/boot ROM as a real dependency to pre-load deterministically rather than an
  afterthought (the predecessor had a real bug class here: async BIOS fetch races that crashed
  the CPU on reload).

Preserve these as **product requirements** for the new project (see prompt 14, Frontend/UI).
Do not preserve *how* they were implemented.

### What was architecturally wrong, and must not be repeated

1. **No real emulation core — a vendored black box.** The entire CPU/PPU/APU/memory/DMA/timer
   implementation was a third-party library treated as an opaque dependency. Save states were
   implemented by reaching into that library's *private* internal object graph
   (`io.cpu.branchFlags`, `renderer.paletteRAM`, per-background-layer renderer objects, etc.)
   and manually re-poking bytes into internal parser functions on load. This is why the project
   needed a "warm-reboot after every load" hack and still had corrupted-tile bugs on quickload.
   **Lesson:** every stateful subsystem must own an explicit, versioned, tested serialize/
   deserialize implementation from the moment it is written (see prompt 16). Never let save-state
   fidelity depend on reflecting into another module's private fields.
2. **One monolithic UI component doing everything.** `src/App.tsx` was ~2,200 lines mixing
   React state, canvas rendering, Web Audio glue, keyboard event routing, Tauri IPC calls, and
   emulator lifecycle orchestration in one function. **Lesson:** the emulation core must be a
   pure library with zero UI-framework dependency, consumed through a narrow, stable API by
   *any* frontend (native GUI, CLI/headless, future web build) — see prompt 02 (`System` trait)
   and prompt 14 (frontend).
3. **Single shared thread for UI, emulation, and audio.** There was no dedicated emulation
   thread; the run loop, canvas paint, and Web Audio buffer feeding all competed on the WebView's
   main thread. **Lesson:** the new architecture runs emulation on its own thread(s), feeds audio
   through a lock-free ring buffer consumed by the platform audio callback, and communicates with
   the UI thread only through explicit channels (see prompt 02, prompt 09, prompt 14).
4. **Zero automated tests.** No unit tests, no accuracy test-ROM harness, no CI. Bugs (e.g. the
   quickload tile-corruption bug) were found by manual play-testing. **Lesson:** testing
   infrastructure is not a late addition — prompt 17 must be usable from the moment prompt 02
   lands, and every system prompt (11–13) has mandatory accuracy-suite gates before being
   considered "done."
5. **Directory-scan-as-source-of-truth persistence with in-memory session state.** ROM/save
   metadata was rebuilt by re-scanning the filesystem on every launch; there was no durable
   index, so metadata like `lastPlayedAt` lived in a data structure that was itself derived from
   disk scanning plus in-memory mutation, an easy source of drift. **Lesson:** keep the "just
   files on disk, human-inspectable" property (it's good for a hobbyist/open-source project) but
   back it with an explicit, durable index (embedded SQLite) that the scanner reconciles against
   rather than replaces (see prompt 14 and prompt 16).
6. **Fragile cross-platform packaging caused by the embedded-webview architecture.** Because the
   UI ran inside WebKitGTK, the predecessor had to vendor `.pc` files and shared libraries
   (`pkgconfig/`, `lib/`) in-repo to work around Linux distros that don't ship the exact library
   names Tauri's WebView backend expects. **Lesson:** the new frontend renders natively via GPU
   surface + windowing (see prompt 14), which has no system webview dependency and removes this
   entire class of problem. Do not reintroduce an embedded browser engine as the primary
   rendering path.
7. **Architecturally single-platform.** Nothing about the predecessor's structure anticipated a
   second system — "the emulator" and "GBA" were the same concept throughout the codebase.
   **Lesson:** every shared abstraction in this project (CPU trait, bus trait, scheduler,
   savestate trait, `System` trait) is designed against *at least two* systems from day one, not
   retrofitted after GB ships (see prompt 02).

## 2. Technology stack (decided — do not re-derive)

| Concern | Choice | Why |
|---|---|---|
| Language | Rust, 2021 edition, single workspace | Memory safety without GC pauses (matters for cycle-accurate timing), one toolchain across Linux/macOS/Windows, first-class cross-compilation, no embedded-runtime licensing concerns. |
| Build system | Cargo workspace + `xtask` crate for automation (no Makefiles/shell scripts as the primary entrypoint) | `cargo xtask <task>` is itself a Rust program, so build automation is portable across OSes without bash/pkgconfig shims — directly avoids the predecessor's Linux shim problem. |
| Package manager | Cargo, `Cargo.lock` committed at workspace root | Reproducible builds by default. |
| Rendering backend | `wgpu` | Cross-platform (Vulkan/Metal/DX12/GL fallback) from one API, no native SDK setup required per platform, supports later shader-based filters (scaling, CRT, palettes) needed by GB/GBC. |
| Windowing/input | `winit` | De facto standard, pairs cleanly with `wgpu`, no embedded browser engine. |
| Immediate-mode tool UI (debugger, memory viewer, library browser chrome) | `egui` (via `egui-wgpu`/`egui-winit`) | Same GPU surface as the emulator output, avoids a second UI toolkit or an embedded webview. |
| Audio output | `cpal` | Cross-platform audio callback API; pairs with a lock-free ring buffer (`rtrb` or hand-rolled) fed by the emulation thread. |
| Serialization | `serde` + `bincode` for save states/config-adjacent binary data; `serde` + `toml` for human-edited config | Explicit, versioned, fast; no reflection. |
| Local index/library DB | `rusqlite` (bundled SQLite) | Durable ROM/save metadata index; still just a file on disk, still inspectable, but no more "rebuild everything by re-scanning." |
| Logging/diagnostics | `tracing` + `tracing-subscriber` | Structured, leveled, works uniformly across the emulation core, frontend, and headless test harness. |
| Scripting/automation hook (movies, cheats, later Lua support) | Deferred to prompt 15/18; do not add a scripting dependency before the debugger/tracing foundation exists. | Avoid speculative dependencies. |
| Testing | `cargo test` per crate for unit tests; a dedicated `testing/harness` crate driving accuracy test ROMs headlessly; `insta` for snapshot tests (disassembly, framebuffer hashes); `criterion` for benchmarks | See prompt 17. |
| CI | GitHub Actions matrix: `ubuntu-latest`, `macos-latest`, `windows-latest`; `cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo test`, accuracy suite | See prompt 19. |
| Release packaging | `cargo-dist` (or `cargo-packager` if `cargo-dist` proves unsuitable at implementation time — implementer's call) | Reproducible, CI-driven artifact generation per OS without hand-maintained platform scripts. |

**Do not** introduce Tauri, Electron, or any embedded-webview UI layer for the primary
application. A future web/WASM build is plausible (the core crates are `no_std`-friendly where
practical and have zero UI dependency), but it is out of scope unless a later prompt says
otherwise.

## 3. Workspace layout (decided)

```
/
  Cargo.toml                     # workspace root
  xtask/                         # `cargo xtask setup|dev|build|test|bench|release`
  crates/
    core-common/                 # Scheduler, Bus/MemoryRegion traits, Cpu trait, Savable trait,
                                  # System trait, event types, shared logging setup
    cpu-sm83/                    # GB/GBC CPU (Sharp SM83)
    cpu-arm7tdmi/                # GBA CPU, and NDS ARM7 coprocessor CPU
    cpu-arm946e/                 # NDS ARM9 CPU (extends arm7tdmi-family core + CP15 + caches)
    ppu-tile2d/                  # Shared 2D tile/sprite/palette compositing primitives
                                  # (used by gb, gbc, gba backgrounds; NOT by NDS 3D)
    apu-shared/                  # Shared PSG/channel-mixing primitives (square/wave/noise + DAC)
    cart-common/                 # Cartridge header parsing, mapper trait, battery-backed save trait
    system-gb/                   # DMG system assembly (memory map, PPU, APU, cartridge mappers)
    system-gbc/                  # GBC system, depends on system-gb, adds double-speed + CGB PPU/APU deltas
    system-gba/                  # GBA system assembly
    system-nds/                  # NDS system assembly (dual CPU, 3D core, dual screen)
    savestate/                   # Versioned binary savestate format + Savable derive/helpers
    debugger/                    # Breakpoints, disassemblers, memory/register inspection, trace log,
                                  # GDB-remote-protocol-subset server
    library/                     # SQLite-backed ROM/save index + filesystem reconciliation
    frontend-core/               # Frontend-agnostic session/runtime glue: owns the emulation thread,
                                  # audio ring buffer, input routing, rewind buffer — no windowing/GPU code
    frontend-native/             # winit + wgpu + egui desktop application (the shipped product)
    frontend-headless/           # CLI driver used by the accuracy test harness and scripted playback
  testing/
    harness/                     # Test-ROM runner, framebuffer hashing, insta snapshots
    test-roms/                   # Fetched at test-time (see prompt 17) — never vendored as committed
                                  # binaries, unlike the predecessor's committed `roms/*.gba`
  docs/
    successor-emulator/          # this prompt collection lives here; the implementer should also
                                  # add architecture docs as the project matures (see prompt 19)
```

Crate boundaries are dependency-direction contracts, not a suggestion:
`system-*` depends on `cpu-*`, `ppu-tile2d`/`apu-shared`, `cart-common`, `core-common`.
`frontend-native`/`frontend-headless` depend on `frontend-core`, `library`, `debugger`,
`system-*`. **Nothing under `crates/system-*`, `crates/cpu-*`, `crates/ppu-*`, `crates/apu-*`,
`crates/savestate`, or `crates/core-common` may depend on `winit`, `wgpu`, `egui`, or `cpal`.**
This is the single most important rule carried over from lesson #2 above — enforce it with
`cargo deny` or a workspace lint, not just code review discipline.

## 4. Core cross-cutting abstractions (defined here, implemented in prompt 02)

These names are used without re-explanation in every later prompt:

- **`System` trait** (`core-common`): the top-level per-platform handle — `step_frame`,
  `reset`, `load_cartridge`, `set_input`, `framebuffer()`, `audio_samples()`, `save_state()`/
  `load_state()`. One implementation per platform crate.
- **`Cpu` trait** (`core-common`): `step_one_instruction(&mut self, bus: &mut dyn Bus) -> Cycles`,
  register/flag accessors for the debugger, `Savable`.
- **`Bus` / `MemoryRegion` traits** (`core-common`): byte/half/word read-write with side effects
  (MMIO), region-based dispatch, open-bus behavior is explicit, not accidental.
- **`Scheduler`** (`core-common`): a min-heap of timestamped events (`Cycles -> EventId`) driving
  everything time-sensitive (PPU mode transitions, timer overflow, DMA completion, APU frame
  sequencer) instead of naive fixed-cycle polling loops. CPUs run in variable-length slices
  bounded by "cycles until next scheduled event," which is both faster and behaviorally correct
  when instructions and events don't align to a fixed grid.
- **`Savable` trait** (`savestate`): `fn save(&self, w: &mut StateWriter)`, `fn load(&mut self,
  r: &mut StateReader) -> Result<(), StateError>`, implemented by every stateful struct at the
  point it is written — never bolted on afterward by reflection.
- **`Cartridge` / `Mapper` trait** (`cart-common`): header parsing, address translation, and a
  `BatteryBackedSave` trait separating "how the mapper banks ROM/RAM" from "how the save data is
  persisted to disk."

## 5. Prompt index

| # | File | Deliverable |
|---|---|---|
| 01 | `01-repo-bootstrap.md` | Workspace skeleton, xtask, CI scaffold, lint/format config |
| 02 | `02-core-framework.md` | `core-common`: Scheduler, Bus, Cpu trait, System trait, event types |
| 03 | `03-cpu-sm83.md` | Sharp SM83 CPU core (GB/GBC) |
| 04 | `04-cpu-arm7tdmi.md` | ARM7TDMI CPU core (GBA, NDS ARM7) |
| 05 | `05-cpu-arm946e.md` | ARM946E-S CPU core (NDS ARM9, CP15, caches) |
| 06 | `06-memory-subsystem.md` | Bus implementations, MMIO dispatch, DMA controllers per system |
| 07 | `07-scheduler-timing.md` | Scheduler implementation details, timers, frame sequencer wiring |
| 08 | `08-graphics-architecture.md` | `ppu-tile2d` shared primitives + per-system PPU/GPU backends incl. NDS 3D |
| 09 | `09-audio-architecture.md` | `apu-shared` + per-system APU backends + ring-buffer output pipeline |
| 10 | `10-input-system.md` | Input abstraction, keybind mapping, rumble/RTC-adjacent peripherals |
| 11 | `11-system-gb-gbc.md` | `system-gb` + `system-gbc` full assembly |
| 12 | `12-system-gba.md` | `system-gba` full assembly |
| 13 | `13-system-nds.md` | `system-nds` full assembly |
| 14 | `14-frontend-ui.md` | `frontend-core`, `frontend-native`, library UI, HUD, keybind UI |
| 15 | `15-debugging-framework.md` | Breakpoints, disassembly, memory/register viewers, GDB-remote server |
| 16 | `16-savestate-framework.md` | Versioned format, `Savable` conventions, rewind buffer, library index |
| 17 | `17-testing-infrastructure.md` | Accuracy test-ROM harness, snapshot tests, CI wiring |
| 18 | `18-performance-optimization.md` | Profiling workflow, JIT/interpreter tradeoffs, dynarec go/no-go |
| 19 | `19-packaging-cicd-docs.md` | Release automation, packaging, docs generation, contributor onboarding |

Implement in roughly this order; 01–02 are hard prerequisites for everything else. 03–10 can
proceed in parallel once 02 lands, provided each stays within its crate boundary. 11 (GB/GBC)
should ship completely, accuracy-tested, and playable before 12 (GBA) begins in earnest — it is
the smallest system and the place to prove the shared abstractions actually generalize before
paying the cost of a bad abstraction across four systems. 13 (NDS) is the largest and should
start only after 12 is stable, since NDS reuses the ARM7TDMI core and much of the 2D PPU
compositing logic from GBA.
