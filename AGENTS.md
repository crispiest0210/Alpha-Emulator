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

**Keep working until a limit actually forces a stop.** Not until a task feels finished, not at a
tidy-looking boundary, and not on a guess about how much budget is left. When one of the two
budgets — the context window or the session token allowance — is genuinely close to exhausted,
*then* run the handoff below. Until then, pick up the next piece.

Two things follow from that:

- **Do not estimate remaining context; read it.** Estimates in this project have been wrong by
  thirty points in both directions, and stopping early on a bad guess wastes a whole session's
  worth of budget. `/context` is the measurement.
- **The two budgets are different.** Context is this window and refills on a new session; the
  session token allowance is cumulative spend and does not. Whichever is closer to its limit is
  the one that decides when to stop.

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

When the stop is forced by the *context* limit specifically, also leave a handoff prompt for
the next session naming the exact next piece of work — see the end of this file for the shape.

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

`README.md` has the authoritative status. In short: the Game Boy, Game Boy Color, and Game Boy
Advance all boot cartridges and run headlessly with real accuracy coverage; the DS has only its
two CPU cores. Prompts 11 and 17 are complete, 12 is nearly so, and 13-16 and 18-19 are
untouched.

### The single most valuable open item

**`gba-suite`'s `arm.gba` runs off into unmapped memory with the CPU in FIQ mode.** Its Thumb
counterpart passes outright and `memory.gba` reaches and reports a specific sub-test, so the
core is broadly sound — which makes this a narrow bug worth chasing, and the exception path the
first place to look. `GbaSystem::step_instruction` exists precisely for tracing this; the
diagnostic in `crates/system-gba/src/system/tests.rs` runs until the program counter leaves ROM
and prints the twelve instructions before it, which is how the last two bugs here were found in
minutes rather than by inspection.

`memory.gba`'s sub-test 3 is the other concrete one. Read `gba-tests/memory/memory.asm` for what
check 3 is.

### Also open on prompt 12

Affine layers and affine sprites are decoded and transformed but not composited. Wait states are
computed but not charged to the CPU. The four `apu-shared` PSG channels are not mixed alongside
the two FIFO channels. Windows, blending, and mosaic are not implemented, nor is keypad input or
EEPROM. Each is noted in `crates/system-gba/src/lib.rs`'s status table.

### After that

Prompt 13 (Nintendo DS) is the next unstarted one and is by far the largest. Prompts 14-16 and
18-19 are smaller and independent — the frontend, the debugger, rewind, performance, and
packaging — and any of them is a reasonable session on its own if the DS looks too big to start.

## Commands

```sh
cargo xtask lint              # exactly what CI runs
cargo xtask test              # unit tests
cargo xtask fetch-test-roms   # the accuracy corpus, never committed
cargo xtask test --accuracy   # the accuracy suite
cargo run -p frontend-headless -- run rom.gb --frames 600
```
