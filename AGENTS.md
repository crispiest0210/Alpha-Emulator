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

- **Keep tool output small.** Pipe through `head`, `tail`, or `grep`; never `cat` a large file
  — use `Read` with an offset. Bash results are one of the biggest consumers of the window, and
  every wasted line is a line of real work that does not happen later in the session.
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

Write a handoff prompt **only** when the context window is genuinely near its limit — not at
every stopping point. Below that, committing and keeping the docs current *is* the handoff, and
a prompt written early is tokens spent on something that will be stale before it is read. When
one is warranted, write it for a reader with no history at all: name the next piece of work, the
file to start in, and the tool to use, and make sure this file and `README.md` are accurate
first, since the prompt's job is only to point at them.

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

### A UI panel returns what the user asked for; it does not do it

The chrome panels are handed a borrowed `ChromeState` and return `UiAction`s. They have no access
to the session, the library, the window, or the filesystem — not by convention, by what they were
given. `app.rs` interprets every action in one `match`, which is also the complete list of side
effects the application has.

This was the third attempt at the shape. Passing `&mut App` to each panel works and is what most
egui applications do; it also means a settings checkbox *can* flush save RAM, and once one does, the
file is a God Component again regardless of how many files it is split across. The action enum is
what makes that unrepresentable rather than merely discouraged.

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
- **`egui` 0.35 replaced `SidePanel`/`TopBottomPanel` with one `Panel` type**, and panels now take
  a `&mut Ui` rather than a `&Context`: the top-level call is `ctx.run_ui(input, |root| …)`, and
  `Context::run` is gone. `Window`, `Area`, and `Modal` still take a `&Context`. Nothing in the
  compiler error points at the replacement, so this is worth ten minutes of reading
  `~/.cargo/registry/src/*/egui-0.35.0/src/containers/panel.rs` before guessing.
- **The `egui-wgpu` `winit` feature is not on by default**, and without it `egui_wgpu::winit::Painter`
  — which owns the surface, the device request, and the resize/surface-lost handling — simply does
  not exist. It is enabled explicitly in the workspace `Cargo.toml`.
- **User textures handed to `egui` must be `Rgba8Unorm`, not `Rgba8UnormSrgb`.** egui's shader picks
  an entry point from the surface format so byte values pass through unchanged; an sRGB texture
  applies the transfer function a second time and the picture comes out washed out.
- **`wgpu` 29 renamed `ImageCopyTexture`/`ImageDataLayout` to `TexelCopyTextureInfo`/
  `TexelCopyBufferLayout`**, and `SamplerDescriptor::mipmap_filter` takes a `MipmapFilterMode`
  rather than a `FilterMode`.
- **There is no async runtime in this workspace and there should not be one.** `wgpu`'s three
  `async` setup calls are driven by `frontend-native::block_on`, twenty safe lines over
  `std::task::Wake`. `pollster` is not in the lockfile and adding a runtime for three calls that
  resolve on the first poll is not worth it.
- **`RewindBuffer::new` clamps capacity up to one**, so "rewind disabled" is not expressible as a
  capacity of zero — a buffer built that way silently records snapshots. The session holds an
  `Option<RewindBuffer>` instead.
- **Under `--data-dir`, `AppPaths::rooted_at` relocates everything.** Use it whenever trying a
  frontend change; a bug in the import path should not be discovered against a real library.
- When a test fails, suspect the test first. Roughly half the failures in this project have been
  wrong expectations, and each one was worth correcting rather than working around — the
  corrected test usually says something true that the original did not.

## Where things stand

`README.md` has the authoritative status. In short: the Game Boy, Game Boy Color, and Game Boy
Advance are **playable** — window, audio, input, save states, rewind — with real accuracy coverage,
and every ROM in the corpus either passes or carries a written diagnosis. The Nintendo DS has only
its two CPU cores.

- **Complete:** prompts 11 (GB/GBC), 12 (GBA), 14 (frontend), 16 (savestate and rewind),
  17 (testing).
- **Mostly done:** prompt 15. The in-app `egui` debugger works — registers, disassembly with the
  program counter highlighted and click-to-toggle breakpoints, a hex viewer with a region jump list,
  instruction stepping, and execution breakpoints that genuinely halt. Three things remain, in the
  order they are worth doing:
  1. **Watchpoints do not halt.** `Breakpoints::check_access` is implemented and tested and nothing
     calls it from a running machine. Unlike execution breakpoints, this cannot be done between
     instructions — it needs the `DebugHooks` bus interception prompt 15 describes, which means a
     touch point in each system's bus.
  2. **Joypad input is not delivered while a breakpoint is set.** `InputState` applies to a whole
     frame by the `System` contract and the stepping loop does not run frames. Wants an input setter
     on `System`.
  3. The GDB remote-serial-protocol subset and execution tracing, neither started. Prompt 15 itself
     ranks the GDB server lowest.
- **Untouched:** prompts 13 (NDS), 18 (performance), 19 (packaging).

### The biggest gap

**The Nintendo DS.** Both CPU cores are complete and unit-tested; nothing above them exists — no
memory map, no 2D or 3D engine, no bus arbitration. Everything else it needs is already in place:
`frontend-native` lists `.nds` files and greys them out, `frontend-core::platform` names the DS
explicitly rather than sweeping it into an "unsupported" arm, and the dual-screen layout and touch
coordinate mapping are written and unit-tested against a 256×384 framebuffer that nothing produces
yet. Prompt 13 is what fills that in.

Prompt 18 is the other obvious next step and is no longer blocked: there is something running at
speed to profile, the HUD already reports measured speed, dropped frames, dropped samples, and
rewind memory, and prompt 15 left it a specific claim to check — that attaching the debugger with no
breakpoints set costs nothing, because the loop only steps instruction-at-a-time when something needs
checking. A test asserts the machine still keeps up; only profiling can say what it actually costs.

### What "verified" means for the frontend

Prompt 14 is explicit that it be checked by running the application, not by compiling it. What was
actually exercised in a running window, on an M3 over Metal: window and surface creation, the GPU
texture upload path, a Game Boy and a Game Boy Advance each holding a measured 100% speed
(59.4–60.5 fps against a native 59.7275 Hz) with **zero** frames dropped to the drawing thread and
zero audio samples dropped after the ring's initial fill, the header title read for the window and
the library row, import, and a restart preserving the library without a re-import.

What was **not** clicked or pressed by hand: quicksave, quickload, rewind, the HUD toggle, and
keybind capture. macOS refuses synthetic keystrokes to an unbundled binary, so those paths are
covered instead by their pieces — `keymap` and `InputTracker` each have unit tests, and every
command a key sends is driven end to end through the real channel API against a real emulation
thread in `frontend-core/src/tests.rs`. The untested link is the two-line hop between them. Press
them yourself before touching that code.

### Smaller, well-defined items

Each is recorded in the relevant crate's `//!` docs along with why it is open:

- **PSG mixing on the GBA** needs a design decision first. The `NR10`-`NR52` register layer is
  shared by all three machines but lives in `system-gb::apu`, and `system-*` crates may not
  depend on each other. It wants moving into `apu-shared`; the obstacle is that three of its
  behaviours are gated on `GbModel`, which would have to move or be narrowed. Duplicating it is
  the copy-paste this project exists to avoid.
- **Alpha blending on the GBA** uses the backdrop as the lower layer, because the scanline buffer
  keeps only the winning pixel. The general case needs a second buffer or a second pass.
- **`dmg_sound` 09/10/12 and `cgb_sound` 09** need the APU stepped finer than one machine cycle.
- **`OPRI` is not modelled.** A real CGB can be asked through it to order sprites by X coordinate —
  the DMG rule — while running in colour mode. Nothing reads it, so a game that sets it gets
  colour-mode ordering. No corpus ROM exercises it and no known game relies on it.
- **dmg-acid2 and cgb-acid2 both pass**, pixel-exact. That comparison is worth copying for any future
  rendering ROM: `frontend-headless run --frames 60 --save-frame out.png`, fetch the reference,
  compare. Two traps, both paid for: dmg-acid2's reference is 2-bit greyscale, so a decoder must
  *scale* samples to 8 bits rather than shifting them left; and screen tile rows are not map tile
  rows once `SCY` is non-zero, which had me reading the wrong 32 bytes of tilemap for a while.
- Mosaic, EEPROM saves, and the GBA's object window are not implemented.

### Tools worth knowing about before you start

- `GbSystem::step_instruction` and `GbaSystem::step_instruction` run exactly one instruction.
- The `#[ignore]`d `trace_gba_suite_entry` in `crates/system-gba/src/system/tests.rs` runs until
  the program counter leaves ROM and prints the instructions before it. It takes a `TRACE_ROM`
  environment variable. Three separate bugs this session were found with it in minutes, after
  reasoning about them had failed.
- `dmg_sound_results` in `testing/harness/src/tests.rs` prints what the memory-protocol sound
  ROMs actually reported, rather than just pass or fail.

## Commands

```sh
cargo xtask lint              # exactly what CI runs
cargo xtask test              # unit tests
cargo xtask fetch-test-roms   # the accuracy corpus, never committed
cargo xtask test --accuracy   # the accuracy suite
cargo run -p frontend-headless -- run rom.gb --frames 600
cargo xtask dev -- rom.gb      # play a cartridge in the window
cargo run -p frontend-native -- --data-dir /tmp/scratch rom.gb   # …against a throwaway library
```

That last one is how to try a frontend change without it touching your real library, saves, or
config. `--data-dir` relocates the whole layout.
