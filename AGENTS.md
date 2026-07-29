# Working on Alpha Emulator

Guidance for an agent picking this repository up. Everything here is something you cannot
derive by reading the code — for what exists and what works, read `README.md`; for the rules a
contributor is most likely to break, read `CONTRIBUTING.md`.

## What this project is

A from-scratch multi-generation Nintendo handheld emulator, built by working through the twenty
prompt files in `docs/successor-emulator/` **in order**, one per unit of work. Those files are
the specification. Read `00-INDEX-AND-ARCHITECTURE.md` first, then the prompt for whatever is
next, before writing anything.

The architecture is defined in reaction to seven documented failures of a predecessor project
(Tauri + React + a vendored JavaScript GBA core). That is why several constraints below look
stricter than a fresh project would need: each one is a mistake already paid for once.

## Standing workflow

**Every time you stop developing — end of a chunk, a natural pause, running low on budget, the
user changing subject — update all affected documentation, then `git add`/`commit`/`push`.**
This is not something to ask about. It is part of "done", like passing tests.

Affected documentation means every surface making a claim the change just falsified:

- `README.md` — the per-system status table and the component status table
- `CONTRIBUTING.md` — when a convention changes
- `SETUP.md` — when a system dependency changes (also `xtask/src/main.rs` and
  `.github/workflows/ci.yml`; those three must move together)
- Crate-level `//!` docs, especially their Status sections
- `testing/harness/src/corpus.rs` — `expected_failure` notes, added or removed

The one exception is a half-finished refactor: finish or revert it first. A non-compiling tree
cannot be handed off at all, and a half-threaded type parameter is invisible to every artifact
above.

## Hard constraints

These are not negotiable and not subject to "just for testing":

- **No commercial ROM**, ever, vendored or referenced by a fetch script pointing at an
  unauthorised source.
- **No copyrighted boot ROM or BIOS vendored.** The predecessor committed `public/gba_bios.bin`
  whose licensing status it simply assumed. Every system here runs without one and supports a
  user-supplied image.
- **No binary dependency, shared library, or `.pc` file vendored** into the repo, on any
  platform. `cargo xtask setup` prints the install command and exits non-zero instead.
- **No Tauri, Electron, or embedded webview.** Rendering is native `wgpu`; the core has zero
  UI-framework dependency, enforced by `cargo deny check bans`.
- Test ROMs are **fetched, never committed**. `testing/test-roms/` is gitignored.

## Principles this codebase has settled on

These emerged from doing the work and are worth keeping.

### A thing lives with the code that consumes it

Applied repeatedly, and each time it removed a problem rather than moving one. `GbModel` is in
`system-gb` because the memory map branches on it. The CGB attribute decode is there because
the PPU reads it. `background_wins` is in `ppu-tile2d` because the compositor applies it. The
CGB register blocks are in `system-gb::cgb` because the *bus* serves them, and `system-gbc`
re-exports rather than defining.

The alternative that keeps suggesting itself — a trait in the lower crate for the upper one to
implement — was considered and rejected for the CGB: one implementor, its shape dictated
entirely by that implementor, and dynamic dispatch in the I/O path bought nothing but a file
location. An abstraction whose only purpose is to satisfy a crate boundary is not an
abstraction.

### Parameterise rather than fork

A Game Boy Color is not a second machine; it is a Game Boy with more banks, a second palette
path, and a faster clock. `GbSystem::with_model` takes a model and the components branch on it,
so there is one frame loop, one save-state format, and one place a timing fix has to land.
Expect the same to apply to the DS's two 2D engines.

### Two machines' test suites can expect opposite things

The DMG and CGB sound suites disagree on three APU rules. "Fixing" one silently regresses the
other. When suites conflict, gate the behaviour on the model — never pick a side. Run both.

### Build tested units first, assemble last

The GBA was built as thirteen independent modules — memory map, interrupts, timers, DMA, video
timing, bitmap modes, backgrounds, sprites, affine, direct sound, wait states, compositor —
each complete with its own tests before anything was wired together. That order is deliberate:
a bug in a unit is found by its own test in seconds, while the same bug inside an assembled
machine is found by staring at a wrong picture.

### Keep the indexed form until the last moment

`ScanlineBuffer` holds palette *indices* until a line is complete, which is what let one
renderer produce both a monochrome and a colour picture with no duplicated fetch logic. Resolve
to RGBA as late as possible.

### Say what is not done

Status sections, `expected_failure` notes, and README rows are load-bearing. A tracked failure
carries the *rule that is broken*, quoted, not the word "fails". An unvalidated framebuffer is
reported as unvalidated, never as a pass — the harness asserts on an unexpected pass so a stale
marker cannot survive.

### Do not approximate a behaviour you have not modelled

Repeatedly the right move was to leave something *visibly* undone rather than approximate it.
An affine layer drawn as a text layer, or an affine sprite drawn untransformed, puts a picture
on screen that is wrong in a way that looks deliberate — much harder to notice than a gap. Each
one is skipped, with a test asserting the backdrop shows through and a comment saying why.

## Gotchas that cost real time

- **Blargg's ROMs report two different ways.** Serial for the CPU suites, a result code and
  message in cartridge RAM at `$A000` for the sound suites. Picking the wrong convention makes
  a finished ROM look like it hangs. `cargo test -p harness --release -- --ignored --nocapture
  dmg_sound_results` prints what the memory-protocol ROMs actually said.
- **`bincode` 3.0.0 on crates.io is a squatted placeholder** that only emits `compile_error!`.
  Pinned to 2.x.
- **`wgpu` is pinned to 29** to match `egui-wgpu` 0.35.
- **An inherent method silently shadows a trait method** with the same name. This bit `is_halted`
  on the SM83 core, with different semantics on each side and no warning.
- The SM83 core reports each memory access through `Bus::tick` *before* performing it, and the
  bus drains due events inside that call. Advancing the clock without draining passes
  `mem_timing` and breaks `instr_timing`.
- **A masked comparison is the wrong way to write `owns`.** It bit the GBA interrupt controller
  (`IE` and `IF` are two apart, so `& !1` misses `IF`) and the direct-sound block (`SOUNDCNT_H`
  is a halfword at an address ending in 2, so no single mask groups it correctly). Write
  explicit ranges.
- **An all-zero OAM is 128 visible sprites at the origin**, on both the Game Boy Advance and in
  any test scene that forgets to park the entries it is not using. That is what hardware shows;
  it is not a decoding bug.
- When a test fails, suspect the test first. Roughly half the failures in this project have been
  wrong expectations, and each one was worth correcting rather than working around — the
  corrected test usually says something true that the original did not.

## Where things stand

`README.md` has the authoritative status. In short: the Game Boy and Game Boy Color are
assembled and running with real accuracy coverage; the GBA has every subsystem done as a tested
unit but **nothing assembled**, so it does not run a ROM; the DS has only its two CPU cores.

Prompts 11 and 17 are complete. Prompt 12 is most of the way through. Prompts 13-16 and 18-19
are untouched.

### Picking up prompt 12

What remains is one coherent piece: the assembly. Concretely, a `GbaSystem` that owns an
`Arm7Tdmi` and a bus routing every I/O address to the module that owns it, driving the video
timing to produce scanlines, feeding timer overflows to the direct-sound channels and DMA, and
charging [`waitstates`] to the CPU. Follow `crates/system-gb/src/system.rs` — it is the proven
shape. Cartridge wiring (the three ROM windows, the save chips in `cart-common`, GPIO) goes in
at the same time.

Two smaller things are also open and are noted in the crate docs: affine layers and affine
sprites in the compositor, and mixing the four `apu-shared` PSG channels in alongside the two
FIFO channels.

The GBA is the system the *predecessor* targeted, so prompt 12 sets the bar at "at least as
correct as the vendored core it replaces, with the test coverage that core never had".
`gba-suite` and `arm7wrestler` are not in the corpus yet — worth adding once there is a machine
to run them on, since they also exercise the ARM core, which has never been run against
anything.

## Commands

```sh
cargo xtask lint              # exactly what CI runs
cargo xtask test              # unit tests
cargo xtask fetch-test-roms   # the accuracy corpus, never committed
cargo xtask test --accuracy   # the accuracy suite
cargo run -p frontend-headless -- run rom.gb --frames 600
```
