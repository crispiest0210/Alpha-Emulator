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

## When you hit a design decision the prompts do not make

This will happen, repeatedly. The twenty prompt files specify *what* to build and the principles
below constrain *how*, but neither answers every question that comes up while building — and some of
the questions that come up are genuinely open, with two or three defensible answers and no fact in
the repository that picks between them.

**Never stall on one.** An agent that stops to ask and produces nothing has spent a session
achieving less than one that picked the obvious option and said so. The work is the deliverable.

Sort the question first, because the two kinds want opposite handling:

**Derivable — just decide, and do not ask.** The prompt files, the existing code, or the principles
below already answer it, possibly after ten minutes of reading. Most questions are this. "Which
crate does this type live in?" is answered by *a thing lives with the code that consumes it*. "Should
I approximate this hardware behaviour?" is answered by *do not approximate a behaviour you have not
modelled*. Asking about these wastes a round trip and signals the reading was not done.

**Independent and unscoped — decide, keep going, and surface it.** Several answers are defensible,
nothing written picks between them, and the choice changes what gets built or what a user ends up
with. Take the best course of action *and* raise it. Do not wait for a reply before continuing:
finish everything that does not depend on the answer, state the assumption you proceeded under, and
make it easy to reverse.

### What makes a decision worth raising

Not difficulty — **blast radius and reversibility**. A hard call that one commit can undo is yours to
make. An easy call that is expensive to walk back is worth thirty seconds of the user's attention:

- it changes an **on-disk or on-the-wire format** — the save-state container, the library schema, the
  config file, the release archive layout. Anything a user's data has to migrate through.
- it **adds a dependency**, changes the licence set, or opens a network socket.
- it **trades away a stated constraint**. Prompt 15 asks for zero measurable debugger overhead; the
  recorder costs 1.7–4.5% and was kept anyway. That is a real trade someone else might make
  differently, and it was raised as one rather than reported as compliance.
- it **picks between named tools** where the prompt named one and you want another — `release.yml`
  is hand-written where prompt 19 says `cargo-dist`.
- it **defers a whole feature**, or ships something visibly partial.
- it is **already flagged here as needing a decision first**. Check that the flag is still true
  before treating it as one: the GBA PSG item carried "blocked on where a shared register layer
  should live" for months, and on inspection the layer that mattered was already shared and the
  rest was per-system anyway. A stale blocker costs more than an open question, because nobody
  re-examines it.

### How to raise one

Concretely, or not at all. "How should I handle audio?" is unanswerable and wastes the exchange.
What works is: **the specific choice, two or three real options, the recommendation, and what
actually differs.** One or two sentences each. If the user does not reply, the recommendation is what
happened anyway, so the question costs nothing.

If the user reaffirms a direction you raised a concern about, that is the decision — say so once and
build the whole thing, rather than re-litigating it or quietly building a narrowed version.

### Record it either way

A decision that is not written down was not made, it was merely done. Both places, always:

- **In the code**, next to the thing it decided, saying what was chosen and *what was rejected and
  why*. `frontend-core::platform` explains why the frame rate is a table rather than a trait method;
  `core-common::debug` explains why the debugger gets `peek8` and not the bus. Those comments exist
  so nobody re-opens a settled question, and so anyone who wants to re-open it has the argument.
- **In the commit message**, which is where a reviewer looks for the reasoning behind a diff.

A rejected alternative is worth more than the chosen one. The chosen one is visible in the code.

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
- **An inherent method silently shadows a trait method** with the same name. This has now bitten
  three times: `is_halted` on the SM83 core, `MatrixStack::load` against `Savable::load`, and
  `SaveChip::load` against the same. The two later ones are named `load_matrix` and `load_file`
  for that reason. Any type that is `Savable` and also wants a `load` of its own has this problem.
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
- **A GBA `SWI` is a different encoding in each instruction set, and almost every commercial game
  is Thumb.** The BIOS HLE originally intercepted only the ARM form, so a real game's calls fell
  through to an unmapped vector — Pokémon Emerald ran at full speed with a black screen and no
  error. There is nothing to guess at in the Thumb form: it is `1101 1111 imm8`, one encoding, no
  condition field.
- **HLE'ing an interrupt means emulating the BIOS's *wrapper*, not just jumping to the handler.**
  The BIOS pushes `r0-r3,r12,lr`, calls the game's handler with `LR` pointing at its own epilogue,
  and returns with `subs pc, lr, #4` — and that last step is what restores `CPSR` and unmasks
  interrupts. Jumping straight to the handler leaves its `bx lr` returning into the interrupted
  code while still in IRQ mode with interrupts masked: the machine takes exactly one interrupt,
  then runs on and wanders into unmapped memory.
- **A cycle-accounting bug fails no test and looks exactly like a hang.** The GBA charged every
  memory access three to six times over — an ARM instruction in IWRAM cost 13 cycles against
  hardware's 1, and 49 from ROM against 6. Every test passed and the emulator held a measured 100%
  speed throughout, because **a frame is a fixed number of cycles however few instructions fit
  inside it**. What a commercial game loses is nine tenths of its processor, and what that looks
  like is a frozen picture with the CPU visibly running. Three causes compounded: the `SWI`
  interception read the opcode through the bus before the CPU fetched the same word; `read32`
  charged and then charged again in each `read16` and each `read8` it decomposed into; and the
  wait-state table's cost includes the access's first cycle, which the CPU core's S/N/I count
  already had. **Charge once, at the width the CPU asked for, and charge only the waiting.**
  `an_instruction_costs_what_the_hardware_charges_for_it` pins it.
- **`GbaSystem::state_dump`** is the tool for "it runs and draws nothing": the program counter, the
  display control register, the interrupt registers, and the handler pointer, on one page.
  `TRACE_ROM=<rom> cargo test -p system-gba --release -- --ignored --nocapture dump_state` prints
  it at four points in a run, which is what turned that black screen into three specific bugs.
- When a test fails, suspect the test first. Roughly half the failures in this project have been
  wrong expectations, and each one was worth correcting rather than working around — the
  corrected test usually says something true that the original did not.

## Where things stand

`README.md` has the authoritative status. In short: the Game Boy, Game Boy Color, and Game Boy
Advance are **playable** — window, audio, input, save states, rewind — with real accuracy coverage,
and every ROM in the corpus either passes or carries a written diagnosis. The Nintendo DS **boots
and draws both screens with sound and 3D**.

- **Complete:** prompts 11 (GB/GBC), 12 (GBA), 14 (frontend), 16 (savestate and rewind),
  17 (testing).
- **Mostly done:** prompt 15. The in-app `egui` debugger works — registers, disassembly with the
  program counter highlighted and click-to-toggle breakpoints, a hex viewer with a region jump list,
  instruction stepping, execution breakpoints, and read/write/range watchpoints, all of which halt.
  What remains is the **GDB remote-serial-protocol subset and execution tracing**, neither started;
  prompt 15 itself ranks the GDB server lowest of everything it asks for.

  The two halting mechanisms are deliberately different and the asymmetry is the interesting part.
  Execution breakpoints are checked *between* `step_instruction` calls, so no system crate knows
  breakpoints exist and a detached session pays nothing at all. Watchpoints cannot work that way —
  only the bus sees accesses — so each bus owns a `core_common::AccessLog` that records when armed,
  and the session drains it after each instruction. That costs one branch per bus access whether or
  not anything is watching — **+1.7% of a Game Boy frame, +4.5% of a GBA one, and +3.7% of a DS one**, measured under
  prompt 18, and **+3.7% of a Nintendo DS one**. See "Performance" below: the cost is kept
  deliberately and recorded as a deviation from prompt 15's "zero measurable overhead" constraint
  rather than reported as compliance.
- **Mostly done:** prompt 18. The profiling workflow exists (`cargo xtask bench`, `cargo xtask
  profile`), every implemented system is measured, and the dynarec go/no-go is recorded with the data
  behind it: **no**, for both CPU cores, because the worst workload on each already runs at 46x and
  11.3x real time. Findings live in `testing/harness/benches/systems.rs`. The NDS is now measured
  too, and prompt 18 was right about it: **3.2x real time, without a 3D core**, a fifth of the GBA's
  margin. That decision is therefore still open rather than inherited, and wants re-asking once the
  3D rasteriser exists. One optimisation has been made, with a measured problem behind it — see
  "Performance" below.
- **Done:** prompt 19. CI pins the same toolchain a fresh clone gets and covers lint, the
  crate-boundary rule, unit tests and the accuracy suite on three OSes, `cargo doc` with warnings
  denied, a release-profile build of the shipped binaries, and `cargo bench --no-run`. `release.yml`
  is tag-triggered and builds four targets; `docs.yml` publishes rustdoc to Pages. The two licence
  files finally exist — the manifest had claimed `MIT OR Apache-2.0` since prompt 01 with neither
  file in the repository.

  Prompt 19 names `cargo-dist`; `release.yml` is hand-written instead, because the constraint it
  actually sets is "reproducible from a clean checkout via CI alone" and a workflow that runs
  `cargo build --release` and uploads the result meets that without a generated file that must not
  be hand-edited. Revisit that the moment real installers are wanted — an `.msi`, a signed `.app`,
  a `.deb` — which is exactly what those tools are for.
- **Mostly done:** prompt 13 (NDS). The machine boots a `.nds` ROM through direct boot, runs both
  cores, draws both screens in 2D and 3D, plays sound, and can be single-stepped in the in-app
  debugger. Thirteen modules, all unit-tested: two memory maps over one store, all nine VRAM banks,
  both 2D engines, the 3D core, the sixteen-channel sound hardware, IPC, interrupts, timers, DMA,
  video timing, keypad and touchscreen, the card transfer interface, and save states over all of it.
  What is missing is a cartridge save chip, DS accuracy-corpus coverage, and the 3D core's rarer
  effects. See "The biggest gap".

**Every one of the twenty prompts has now been built.** What remains is finishing work, listed
under "The biggest gap"; there is no next prompt file to open.

### The four decisions prompt 13 raised, and what was chosen

Recorded here because they are the ones a reader will want to re-open, and each is argued in full in
the module that made it.

- **How the two CPUs are driven** — cooperative interleaving at a **video boundary**, in
  `system-nds::system`. `VideoTiming::cycles_until_next_event` says how far the machine may run,
  both cores run that far, then the boundary is serviced. A fixed small quantum was rejected because
  it decouples the CPUs from the renderer and reintroduces prompt 08's mid-frame-scroll bug; one
  merged scheduler was rejected as real work for a machine whose cores synchronize through polled
  registers rather than through timing. `step_frame` is the only caller, so this is reversible in
  one place.
- **Where "partial" is drawn** — a DS that runs 2D software, says so, and refuses nothing. The
  status table and `frontend-core::platform` both build a machine rather than declining one, and
  `Platform::is_runnable` is now true for everything. Greying out a row a user can play a 2D game in
  said less than `README.md` does.
- **VRAM bank mapping** — **precomputed**, into a flat 328-entry page table rebuilt on a `VRAMCNT`
  write. The alternative is nine register decodes on every access, landing in the rasteriser's
  innermost texture fetch. Correctness is identical either way, because overlap is represented
  rather than resolved.
- **Whether the 3D rasteriser stays software** — **software, and measured rather than assumed.**
  It costs about 5 ns a rasterised pixel, which is a few percent of a frame for a realistic scene.
  Prompt 13 forbids `system-nds` a `wgpu` dependency; the escape hatch if one is ever needed is
  `frontend-native` consuming `gpu3d::geometry::DisplayList`, which is already a plain description
  of triangles. Nothing needs it today.

One more, not foreseen and worth knowing: **the timer block is duplicated from `system-gba`** rather
than shared. `system-*` crates may not depend on each other and `core-common` is closed to
platform-specific behaviour, so sharing needs a new crate. This was filed as the same question the
GBA's PSG register layer was stuck on; that one turned out not to be a placement question at all
(see "The biggest gap"), so this is the only live instance. It is also the weaker case: the two
timer blocks are similar but not identical, and a shared crate for one duplicated module is a lot
of structure for a little reuse. Worth doing only when a third caller appears.

### The biggest gap

**Graphics that break across a screen change.** Reported against a commercial game on 2026-08-10,
with a screenshot: a screen that is mostly right but missing one background layer's content. Two
severe bugs were found and fixed while chasing it, and it is not yet known whether they were the
cause — re-test before assuming anything:

- **The sound FIFOs had no write path at all.** `DirectSound::owns` claimed both FIFO addresses and
  `write16` answered `None` for them, so the bus accepted every byte a DMA channel delivered and
  dropped it. The machine produced *exact digital silence* — 0 non-zero samples out of 964140 —
  for every game that uses direct sound, which is every commercial game.
- **A sound-FIFO DMA read its own destination step and word count.** Hardware fixes the shape of
  that transfer — four 32-bit words into a destination that does not move — precisely because a
  game does not bother writing settings the hardware overrides. Honouring them marched a refill
  out of `FIFO_A` and up through the **DMA control registers** above it, thousands of times a
  second, writing audio samples over the machine's own configuration. That is a very good candidate
  for "graphics fall apart on a screen change" and it is what to rule out first.

What is known about the remaining report: at the screen in question VRAM *is* populated and the
windows *do* cover the screen, so it is neither a failed upload nor a window-mask bug. The layer
whose char base is empty is the one with nothing to draw, which is expected. Reproduce with
`frontend-headless run --press` and `graphics_dump`, which now prints the window rectangles too.

**PSG audio** is the other known gap: the GBA makes sound two ways and only direct sound is wired,

This was recorded for a long time as blocked on a design decision, and **that framing was wrong**.
The claim was that the `NR10`-`NR52` register layer is shared, lives in `system-gb::apu`, cannot be
reached across a `system-*` boundary, and wants moving to `apu-shared` with its `GbModel` gating
resolved. Checking it: the *channels* — `SquareChannel`, `WaveChannel`, `NoiseChannel`, `Envelope`,
`Sweep`, `LengthCounter`, `Mixer` — are **already in `apu-shared` and directly usable**. What sits
in `system-gb::apu` is the **address decode**, and the GBA's is genuinely a different thing: its
registers are halfwords from `0x0400_0060` laid out with gaps rather than the Game Boy's contiguous
block, its wave RAM is two banks of sixteen bytes with the CPU seeing whichever is not playing, and
it has a 75% volume step the Game Boy lacks. So the work is a new register layer over shared
channels, not a copy of an existing one, and the `GbModel` gating does not apply at all — the GBA
follows the CGB rule throughout. No decision is needed; write `system-gba::psg`, mix its output into
`generate_samples` alongside `DirectSound::output`, and scale by `SOUNDCNT_H` bits 0-1.

The one judgement call inside it: the 64-sample wave mode spans both banks, and `apu_shared::WaveChannel`
holds sixteen bytes and walks 32 samples. Either extend it with a sample count or implement the
32-sample mode and say the other is not modelled. Prefer extending it — the Game Boy never changes
the default, and "do not approximate a behaviour you have not modelled" applies to audio too.

Then, in order:

- **Saving has never been verified end to end.** The chip is right — a real cartridge's `FLASH1M`
  string is found, `save_ram` is the correct 131072 bytes, and a raw write to the save window is
  correctly *refused* because FLASH wants its command sequence first — but no game has been played
  far enough to write a file, so nothing has confirmed a save reaches the disk and loads back. Use
  `frontend-headless run --press` to get there rather than a window.
- **No cartridge GPIO**, so a game with a real-time clock finds none and reports a flat battery.
  That is what real hardware with a dead battery does, so it is an accurate outcome rather than a
  bug, but time-of-day events never fire. The pins are at `0x080000C4`-`0x080000C8`, currently
  reading as ordinary ROM.
- **EEPROM saves are reported absent rather than emulated**, and mosaic and the object window are
  not implemented.

### The gaps behind it

**Accuracy coverage for the DS, and DS polish.** Prompt 13's scope is built; what is missing there
is evidence rather than hardware.

- **Nothing DS-shaped is in the accuracy corpus.** `README.md` says why and lists what stands in
  for it. That is honest but it is not the same bar the Game Boy family is held to, and the next
  person to work on the DS should look for community test ROMs again — the ecosystem moves.
- **The 3D core's rarer effects**, all listed in `README.md` and each documented where it is
  skipped: fog, edge marking, anti-aliasing, shadow polygons, the toon and highlight tables, the
  shininess table, and `BOX_TEST`. Prompt 13 explicitly ranks these below geometry and texturing,
  which is the order they were done in.

Everything above is small next to what has landed. `system-nds` is 339 tests over fourteen modules
and the machine boots, draws both screens in 2D and 3D, plays sound, and can be single-stepped in
the in-app debugger.

One thing the debugger does *not* do, and it is a frontend question as much as a core one: it shows
the **ARM9 only**. `DebugTarget` has one register list, one program counter, one address space, and
inventing a "which core" concept means changing the panel and making every breakpoint say which
core it belongs to. Worth doing; not worth pretending is done. See `system-nds::debug`.

Smaller DS gaps, all recorded in the crate docs where they are made: no KEY1 cartridge encryption,
no wifi and none planned, no mosaic, no mode 6 large bitmap, no display mode 3, no per-line sprite
budget, and no save-chip write timing — a game polling for "write finished" is satisfied at once.

### Start here

**All twenty prompts are now built.** There is no next prompt to open; what is left is the list
under "The biggest gap" above, and it is finishing work rather than new hardware.

`crates/system-nds/src/lib.rs`'s Status section is the shortest accurate summary of what the DS
has, and `README.md`'s "What the Nintendo DS does and does not do" is the longer one. Both are kept
current.

Two things to know before picking any of it up:

- **The save chip is done, and it is worth knowing why it looks the way it does.** Nothing in a
  DS header says whether a cartridge carries EEPROM or FLASH, or which of six sizes. The choice
  between a game-code database, a heuristic, and asking the user was put to the user and the
  heuristic was picked. `system-nds::save` therefore works the chip out from how software talks
  to it, and — the part that matters — **refuses to guess**: an ambiguous write is held rather
  than applied, and `save_ram` returns `None` until the type is settled, so a file of the wrong
  shape never reaches the disk. Once a save file exists its size settles the question outright,
  so the heuristic only ever runs on a cartridge's first save.
- **The DS's margin is a fifth of the GBA's** — about 3.1x real time — and the reason is the CPU
  interpreters, not the rasteriser. See "Performance" below; prompt 18's expectation about the 3D
  core was measured and turned out backwards. If the DS needs to be faster, CPU dispatch is where
  to look, and that is the one dynarec question still open.

If the 3D rasteriser ever *does* need replacing, `system-nds` has no `wgpu` dependency and prompt 13
forbids it one: the answer is `frontend-native` consuming `gpu3d::geometry::DisplayList`, which is
already a plain description of triangles with no rendering in it.

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

### Performance, and what it means for the smaller items

`testing/harness/benches/systems.rs` has the numbers and the reasoning. Two findings that should
change where anyone looks next:

- **On a Game Boy frame with music, the APU costs more than the PPU** — about a third of the frame
  against a sixth. Not what you would guess for a machine whose job is drawing a picture.
- **The DS is the tight one, at about 3.1x real time**, against 11x to 80x everywhere else. Read
  `nds/` benchmark numbers against `gba/spin` from the *same run*: the DS case is close enough to
  its budget that a laptop under sustained load moves it by 70%, which has already produced one
  "regression" that was nothing of the kind.
- **Prompt 18 expected the 3D rasteriser to be the DS's bottleneck, and the measurement says it is
  not.** Three screen-filling quads with overdraw cost about 0.73 ms — roughly 5 ns a rasterised
  pixel — against the 5.3 ms the two CPU interpreters cost with both cores doing nothing. If the DS
  ever needs to be faster, CPU dispatch is where to look. That is the dynarec question, and it is
  still open for this system alone. Its first measurement was 1.1x, and the cause was not the two 2D
  engines: turning both displays off saved 1.5%. It was `NdsBus` composing every wide access out of
  byte accesses, so each instruction fetch paid four region decodes. Reading RAM at its real width
  cut a frame by 65%. That is the only optimisation in this project, and it had a measured problem
  behind it — which is the bar for the next one too.

`cargo xtask bench --quick --filter gb/` is the fast loop; `--save-baseline` and `--baseline` are how a
before/after claim gets made, and prompt 18 requires one for every optimisation.

### Smaller, well-defined items

Each is recorded in the relevant crate's `//!` docs along with why it is open:

- **PSG mixing on the GBA** is now the biggest gap and is *not* blocked on a decision, though it
  was recorded that way here for a long time. See "The biggest gap" above for why the blocker
  dissolved on inspection: the channels are already shared, and only the address decode is
  per-system — which the GBA needs its own of regardless.
- **Alpha blending on the GBA** now composes its real lower layer, as a second pass over the line
  with the first-target layers left out. A second `ScanlineBuffer` slot was rejected: that type is
  shared with the Game Boy, which has no colour effects and would carry the runner-up pixel on every
  line to no purpose. The pass runs only when an alpha blend is configured. It is exact unless two
  first-target layers stack, where hardware blends the top with the second and this skips to the
  third; nothing in the corpus does that.
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
- **Sprite bit depth is per sprite, not per call.** It rides on `ppu_tile2d::Sprite` because bit 13
  of a GBA OAM entry selects 16 or 256 colours and one scanline can hold both. It used to be an
  argument to `render_sprites` and every GBA sprite was rendered as 16-colour; a 256-colour one came
  out as a stretched checkerboard, one byte read as two indices, which looks like a corrupt tile
  rather than an unimplemented feature. Pokémon Emerald's title wordmark is the case that found it.
- **`cpu-arm946e`'s `TcmBus` used to decompose wide accesses**, because `Bus`'s default `read32`
  composes byte accesses and the wrapper did not override it. Invisible on a bus whose byte and wide
  behaviour agree, and fatal on the DS, where an ARM9 byte write to VRAM, palette RAM, or OAM is
  *dropped* by hardware. The ARM9 could not write to VRAM at all and it presented as a black screen
  with every register set correctly. Fixed, with two tests; worth remembering as a shape of bug,
  because any future bus wrapper can reintroduce it.

### Tools worth knowing about before you start

- `GbSystem::step_instruction` and `GbaSystem::step_instruction` run exactly one instruction. So
  does `NdsSystem`'s, on the ARM9.
- **`system-nds`'s tests assemble ARM by hand.** `crates/system-nds/src/system/tests.rs` has a
  `load(rd, value)` helper that emits however many instructions a 32-bit constant needs, which is
  what makes an end-to-end test — "this program sets these registers and a pixel appears" — cheap to
  write. Most DS addresses are not expressible as a single ARM immediate, and forgetting that is the
  first thing that will bite when adding one.
- The `#[ignore]`d `trace_gba_suite_entry` in `crates/system-gba/src/system/tests.rs` runs until
  the program counter leaves ROM and prints the instructions before it. It takes a `TRACE_ROM`
  environment variable. Three separate bugs this session were found with it in minutes, after
  reasoning about them had failed.
- **`trace_stall`**, beside it, is for the case `state_dump` cannot answer: a machine that is
  executing happily and getting nowhere. `TRACE_ROM=<rom> TRACE_FRAMES=600 cargo test -p system-gba
  --release -- --ignored --nocapture trace_stall` runs to a chosen frame and profiles the next
  200 000 instructions — hottest addresses disassembled in the instruction set they were *fetched*
  in, a breakdown by 4 KiB page, the loop body around the hottest address, how many steps went to a
  halted CPU, and **the average cycle cost of each instruction**. Three of those columns each
  answered a different question in one run: the page breakdown said the game's main loop was
  running seven times where it should run a hundred and sixty, and the cycle column said why.
  Recording the instruction set alongside the address is not optional — `DebugTarget::disassemble`
  uses the CPU's *current* T bit, so a profile printed after the fact decodes half of it wrongly
  and the result is plausible rather than obviously broken.
- `dmg_sound_results` in `testing/harness/src/tests.rs` prints what the memory-protocol sound
  ROMs actually reported, rather than just pass or fail.
- **The debugger panel** (`cargo xtask dev -- <rom>`, then "Debugger") shows registers, disassembly
  with click-to-toggle breakpoints, a hex viewer, and watchpoints. Attaching with no breakpoints set
  costs nothing, so it is safe to leave open while a game runs.
- **`NdsSystem::graphics_dump` and `NdsSystem::cores_dump`**, in `crates/system-nds/src/diagnostics.rs`,
  are the DS's version of the trick below and the first thing to reach for when a DS ROM misbehaves.
  The graphics dump prints where all nine VRAM banks went, which spaces have a bank in them, the
  shared-WRAM split, both engines' `DISPCNT` decode, and — the useful part — **what each background
  layer currently is**, which depends on the mode *and* on two `BGxCNT` bits whose meaning changes
  with it. The cores dump prints both program counters, both interrupt states, how many words are
  waiting in each FIFO, and the video position: when a DS ROM appears to hang, nine times in ten
  one core is spinning on a flag the other was supposed to set, and that tells you which.
- **`cgb_acid2_attribute_dump`** in `crates/system-gbc/src/lib.rs` dumps VRAM maps, OAM flags, and
  both palette sets from a running machine. It is what cracked cgb-acid2 in about a minute after
  reasoning from the rendered picture had stalled — reach for it before staring at pixels. Its rows
  are *map* rows, which with `SCY` non-zero are not screen rows.
- **`frontend-headless run --press <button>@<frame>[:<frames>]`** holds a button, repeatable for a
  sequence. Everything a commercial game does past its title screen is on the far side of one
  button press, and before this there was no way to reach any of it without a window. Emerald's
  whole opening — start, the dead-battery notice, NEW GAME, Professor Birch — is four `--press`
  flags and a frame count.
- **`frontend-headless run --state <file>`** loads a save state before running, and `--save-state`
  writes one after. This is how an unreproducible "the graphics are wrong after an hour" becomes a
  two-minute diagnosis: take the user's state file, load it here, and run `graphics_dump` on the
  exact frame. A state resumes byte-exactly, so five frames past a loaded state hash the same as
  the whole run from a reset.
- **`frontend-headless run --save-frame out.png`** writes a framebuffer as a PNG, and `identify`
  prints what the library importer would record for a ROM. Comparing that PNG against a published
  reference image is what turned dmg-acid2 and cgb-acid2 from "renders something" into passes.
- **`cargo xtask bench --quick --filter <name>`** for a fast measurement, `--save-baseline` and
  `--baseline` for a before/after claim. `cargo xtask profile <rom>` prints the flamegraph command.

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
