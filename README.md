# Alpha Emulator

A from-scratch, multi-generation Nintendo handheld emulator written in Rust, targeting the
Game Boy, Game Boy Color, Game Boy Advance, and Nintendo DS.

It is a clean-room implementation: the emulation core is original Rust, with no vendored
third-party emulator embedded as a black box, and no browser engine in the rendering path.
Rendering is native GPU (`wgpu` + `winit`), audio is `cpal` fed from a lock-free ring buffer,
and the emulation core is a pure library with zero UI-framework dependency.

## Quick start

```sh
cargo xtask setup                       # checks your machine; on Linux it names two packages to install
cargo xtask dev -- path/to/rom.gba      # build, then play
```

That is the whole thing on macOS and Windows. The first build takes a few minutes. Bring your own
ROM — none are included here and none ever will be. **[SETUP.md](SETUP.md)** has the full guide:
controls, per-OS packages, where saves go, and what to do when something goes wrong.

## Status

There are no released builds yet, but all four systems run. The table below reflects what is
**actually implemented and tested**, not what is planned, and is updated as work lands.

| System | Boots | Playable | Accuracy suite | Notes |
|---|---|---|---|---|
| Game Boy (DMG) | ✅ | ✅ | ⚠️ | Plays in the window with sound and input, measured at 100% speed. Passes all 11 Blargg `cpu_instrs` sub-tests, `instr_timing`, `mem_timing`, `dmg-acid2` pixel-exact, and 9 of 12 `dmg_sound` sub-tests — see below |
| Game Boy Color | ✅ | ✅ | ⚠️ | Plays. 11 of 12 Blargg `cgb_sound` sub-tests pass; `cgb-acid2` is pixel-exact against its reference |
| Game Boy Advance | ✅ | ✅ | ✅ | Plays, measured at 100% speed; passes all three `gba-suite` ROMs. Keypad and affine backgrounds work. No PSG mixing, mosaic, or EEPROM yet |
| Nintendo DS | ✅ | ⚠️ | ❌ | **Partial, and deliberately so.** Boots a `.nds` ROM, runs both CPUs, draws both screens in 2D and 3D, plays its sixteen sound channels, and keeps saves. Held to a lower accuracy bar than the other three; expect some games to misbehave. See below for exactly what is missing |

**All four systems boot with picture and sound; three are fully playable.** The application has a
ROM library, video, audio, keyboard and touchscreen input, quicksave and quickload, rewind, an HUD,
a rebindable keymap, screenshots, and an in-app debugger with registers, disassembly, a memory
viewer, breakpoints, and watchpoints.

### What the Nintendo DS does and does not do

DS support is real but newer, and is the start of DS emulation here rather than the end of it. The
bar it was built to is "runs good homebrew and simple commercial titles correctly", not parity with
hardware.

**Implemented:**

- **Both CPUs**, interleaved deterministically on one thread, with the runtime-configurable
  shared-WRAM split and two genuinely different views of memory.
- **All nine VRAM banks** and every mapping their control registers can select, including two banks
  claiming one address at once.
- **Both 2D engines** — background modes 0-5 (text, affine, and all three extended types), sprites
  at 4 and 8 bits per pixel in both mapping arrangements with flips, rotation, scaling, semi-
  transparency, bitmap and object-window modes, extended palettes, windows, alpha blending,
  brightness effects, and master brightness.
- **The 3D core** — the full geometry command set, four matrix stacks, four-light vertex lighting,
  clipping against all six frustum planes, a perspective-correct software rasteriser with a 24-bit
  depth buffer, all seven texture formats including 4x4 compression, and compositing into the 2D
  layers through the ordinary blend unit.
- **Sixteen-channel sound** — PCM8, PCM16, IMA-ADPCM, six square-wave channels and two noise
  channels, with per-channel rate, volume, panning, and loop modes.
- **The cartridge save chip.** Which chip a cartridge has is not in its header, so it is worked out
  from how the game talks to it — and nothing is written to disk until that is certain, because a
  save file of the wrong shape is worse than none.
- **Input**: the keypad, the two extra buttons only the ARM7 can see, and the touchscreen.
- IPC (both FIFOs and `IPCSYNC`), both interrupt controllers, eight timers, eight DMA channels, the
  cartridge transfer interface, direct boot, and save states covering all of it.

**Not implemented, and visibly so rather than approximated:**

- **3D refinements**, ranked below getting the geometry and textures right: fog, edge marking,
  anti-aliasing, shadow polygons (which render as ordinary geometry), and the toon and highlight
  tables (which fall back to plain texturing). `BOX_TEST` always answers "visible" — answering
  "hidden" wrongly makes geometry disappear with no way to recover it, while answering "visible"
  wrongly only costs a little work. The geometry command queue is reported as never full, because
  real hardware stalls the CPU when it fills and nothing in this codebase's bus interface can
  express a bus that blocks a CPU part-way through an instruction.
- **Wifi**, and it is not planned. Its register block reads as open bus, which is what a DS with no
  card present looks like.
- Mosaic, mode 6's large bitmap, display mode 3 (main-memory display), KEY1 cartridge encryption,
  and the per-line sprite budget.

Component status:

| Component | Status |
|---|---|
| Workspace, `xtask`, CI, crate-boundary enforcement | done |
| `core-common` — scheduler, bus, CPU/system traits | done |
| `savestate` — versioned format, `Savable`, rewind ring buffer | done; rewind verified against a running machine |
| `cpu-sm83` — Game Boy CPU | complete; passes all Blargg CPU and timing suites |
| `cpu-arm7tdmi` — GBA / DS ARM7 CPU | complete; passes `gba-suite`'s ARM and Thumb instruction ROMs |
| `cpu-arm946e` — DS ARM9 CPU (ARMv5TE, CP15, TCM) | complete, unit-tested; **accuracy ROMs not yet run** |
| `cart-common` — headers, MBC1/2/3/5, SRAM/Flash/EEPROM, RTCs | done for GB and GBA save chips |
| `system-gb` memory map | done (WRAM/VRAM banking, echo RAM, boot ROM) |
| `system-gb` timing — timer, PPU mode machine, APU sequencer | done as scheduled events; **Mooneye timer ROMs not yet run** |
| `ppu-tile2d` — tile decode, palettes, scanline compositing | done for GB/GBC/GBA formats; both sprite-priority rules and both tile-mapping arrangements |
| `system-gb` PPU — background, window, sprites | done, scanline-accurate; dmg-acid2 validated pixel-exact |
| `apu-shared` — square/wave/noise channels, envelope, sweep, mixer | done; 9 of 12 Blargg `dmg_sound` sub-tests pass |
| `frontend-core` audio pipeline — lock-free ring, resampler | done and bound to a `cpal` device; fast-forward is pitch-shifted rather than dropped |
| `system-gb` APU — NR10-NR52 register layer | done; DMG wave-RAM window is machine-cycle accurate, not t-cycle |
| `frontend-core` input — keybinds, conflict rule, delivery | done and driven from the window; keyboard only, gamepads are future work |
| `system-gb` assembly — `System` impl, joypad, OAM DMA, boot | done; save-state round-trip is frame-exact |
| `system-gb` CGB blocks — palette RAM, `KEY1`, VRAM DMA, tile attributes | done and driven by the bus |
| `system-gbc` — `System` impl, model selection, compatibility boot | done; 11 of 12 `cgb_sound` sub-tests pass, cgb-acid2 validated pixel-exact; **`OPRI` not modelled** |
| `testing/harness` — accuracy runner, fetch automation | done; drives the GB suite end to end |
| `frontend-headless` — CLI driver, framebuffer hashing, determinism check | done for the Game Boy family |
| `library` — SQLite index, watched folders, reconciliation | done; a moved file is recognised by content hash and keeps its row |
| `frontend-core` session — emulation thread, commands/events, frame pipe, rewind, save-RAM flush | done; lifecycle driven end to end by tests against a real thread |
| `frontend-core` settings — TOML config, keybinds, presentation, rewind depth | done; a malformed file falls back to defaults and is left on disk |
| `frontend-native` — window, `wgpu` presentation, `egui` chrome, library browser, HUD, keybind editor | done; **no native file dialog — drag-and-drop or a pasted path** |
| `system-gba` memory map — regions, mirroring, open bus, 8-bit write quirk | done |
| `system-gba` interrupt controller, timers, 4-channel DMA | done and tested; not yet driven |
| `system-gba` video timing and bitmap modes 3/4/5 | done and tested; not yet driven |
| `system-gba` text backgrounds — four layers, map decode, draw order | done and tested; not yet driven |
| `system-gba` sprites — OAM decode, sizes, per-line selection, matrices | done and tested; not yet driven |
| `system-gba` affine transform — backgrounds and sprites | done and tested; not yet driven |
| `system-gba` direct sound — two DMA-fed FIFO channels | done and tested; PSG mixing not wired |
| `system-gba` wait states — `WAITCNT`, per-region access cost | done and charged to the CPU per access |
| `system-gba` compositor — layers, priority, palette, sprites, affine | text, bitmap, and affine backgrounds plus non-affine sprites draw; **affine sprites not yet composited** |
| `system-gba` keypad — `KEYINPUT`, `KEYCNT`, combination interrupt | done and driven |
| `system-gba` windows and colour blending | done and applied; alpha blending uses the backdrop as the lower layer |
| `system-gba` cartridge — three ROM windows, SRAM/Flash detection | done; **EEPROM reported absent rather than emulated** |
| `system-gba` assembly — `System` impl, bus routing, HLE interrupt entry | done; runs a ROM headlessly |
| `system-gba` HLE BIOS — `Div`, `Sqrt`, `ArcTan2`, `CpuSet`, the waiting calls | done; unhandled calls change nothing rather than guessing |
| `debugger` — breakpoints, watchpoints, conditions | done; execution breakpoints and watchpoints both halt a running machine; **no GDB server or tracing yet** |
| `debugger` — snapshot capture (registers, disassembly, memory) | done against `DebugTarget`, so no branch per system |
| `core-common` — `DebugTarget`, `System::step_instruction`, `AccessLog` | done; all four systems implement introspection and access recording |
| `system-nds` memory map — two views over one store, shared-WRAM split, open bus per core | done and tested |
| `system-nds` VRAM — nine banks, every `VRAMCNT` mapping, overlap, precomputed page table | done and tested |
| `system-nds` IPC — `IPCSYNC`, both FIFOs, edge-triggered interrupts | done and tested; driven end to end by two real programs on the two cores |
| `system-nds` interrupt controllers, timers, DMA | done and tested; per-core source masks, 21-bit ARM9 DMA counts |
| `system-nds` video timing — 263 lines, two `DISPSTAT`s, one `VCOUNT` | done and tested |
| `system-nds` 2D engines — modes 0-5, sprites, windows, blending, master brightness | done and tested; **mosaic, mode 6, and display mode 3 not implemented** |
| `system-nds` input — keypad, `EXTKEYIN`, touchscreen over SPI | done and tested; firmware and power-management SPI devices are stubs |
| `system-nds` cartridge — header, direct boot, card transfers | done and tested; **no KEY1 encryption** |
| `system-nds` save chip — EEPROM and FLASH over auxiliary SPI, six sizes, type detection | done and tested; write timing is not modelled, and a cartridge that only ever writes partial pages cannot be identified |
| `system-nds` audio — sixteen channels, PCM8/PCM16/ADPCM/PSG/noise, panning, loop modes | done and tested; `SOUNDBIAS`, the output filter, and sound capture are not modelled |
| `system-nds` 3D matrices — four stacks, push/pop/store/restore, the clip matrix | done and tested |
| `system-nds` 3D geometry — command FIFO, vertex assembly, lighting, frustum clipping | done and tested; shininess table and `BOX_TEST` deferred |
| `system-nds` 3D rasteriser — perspective-correct spans, depth buffer, all seven texture formats | done and tested; **no fog, edge marking, anti-aliasing, shadow polygons, or toon table** |
| `system-nds` assembly — `System` impl, two bus views, dual-core frame loop | done; boots a ROM, draws both screens, and produces audio |
| `system-nds` debugger support — `DebugTarget`, access log, region list | done and tested; **the ARM9 only** — the ARM7 is not reachable from the debugger |
| `system-nds` diagnostics — VRAM bank map, per-layer decode, dual-core state dump | done and tested; side-effect free, so a dump can be taken from anywhere |
| `frontend-native` debugger panel | registers, disassembly with PC highlight and click-to-toggle breakpoints, hex viewer, read/write watchpoints, instruction stepping |
| Everything else | not started |

### Accuracy suite

The harness is in place and the Game Boy suite runs. Fetch the ROMs with
`cargo xtask fetch-test-roms`, then `cargo xtask test --accuracy`. Nothing is vendored; the
corpus directory is gitignored and tests skip cleanly when it is empty.

Current Game Boy results:

| Test ROM | Result |
|---|---|
| Blargg `cpu_instrs`, all 11 sub-tests individually | **pass** |
| Blargg `instr_timing` | **passes** |
| Blargg `mem_timing` | **passes** |
| Blargg `dmg_sound` sub-tests 01–08, 11 | **pass** |
| Blargg `cgb_sound` sub-tests 01–08, 10–12 | **pass** |
| Blargg `cpu_instrs` (combined ROM) | hangs — see below |
| Blargg `dmg_sound` sub-tests 09, 10, 12 | fail — see below |
| Blargg `cgb_sound` sub-test 09 | fails — see below |
| `gba-suite` arm, thumb, memory | **pass** |
| dmg-acid2, cgb-acid2 | **pass** — pixel-exact against the published reference images |

**Nintendo DS accuracy coverage is zero, and that is reported rather than papered over.** Prompt 13
asks for whatever test-ROM coverage exists at implementation time and for an explicit statement of
what is verified only by other means. Nothing DS-shaped is in the corpus: the community's DS test
ROMs are far fewer than the Game Boy family's, most target hardware this build does not model (3D,
wifi, the firmware), and several are distributed only as parts of emulator repositories rather than
as fetchable artifacts. What the DS *is* verified by instead:

- **339 unit tests in `system-nds`**, covering every module against the register behaviour it
  implements — including the VRAM bank table, both cores' interrupt source masks, the DMA start-
  timing decode that differs per core, and the IPC FIFO's edge-triggered interrupts.
- **End-to-end tests that assemble ARM by hand** and run it on the real machine: the ARM9 executing
  code direct boot loaded, the ARM7 writing where only it can see, the two cores exchanging a word
  through the FIFO with each side spinning on its own status flag, a vblank interrupt reaching a
  handler with no BIOS present, a program that maps a VRAM bank and puts a colour on screen, and an
  ARM7 program that starts a sound channel and is heard, and an ARM9 program that feeds a display
  list through `GXFIFO` and gets a triangle on the top screen.
- **A determinism test**: two machines given the same ROM and input agree byte for byte after four
  frames, which is prompt 13's dual-CPU constraint checked rather than asserted.
- **A save-state round trip** that is a fixed point and that continues identically from a restore.
- **Manual smoke test**: a hand-built homebrew that fills a VRAM framebuffer, run through
  `frontend-headless run --frames 5 --save-frame`, produces the expected gradient on the top screen
  and white on the bottom.

This is a lower bar than the Game Boy family's and is meant to read as one.

Open gaps, each tracked with the specific reason:

- **Combined `cpu_instrs`** executes `STOP` inside the runner it copies into work RAM. Not an
  instruction bug — all eleven sub-tests pass standalone and MBC1 bank reads are verified
  against that exact ROM by a dedicated test. `STOP` now releases correctly when a joypad line
  goes low, so this ROM reaches `STOP` for some earlier reason still to be found.
- **`dmg_sound` 09, 10, 12** exercise wave RAM while channel 3 is playing. The DMG access
  window *is* modelled — the CPU sees the byte the channel just fetched, and `0xFF` at any
  other time — but only to machine-cycle resolution, and these ROMs resolve it to single
  t-cycles. Closing them means stepping the APU finer than one machine cycle. Test 10 also
  needs the wave-RAM corruption a mid-playback trigger causes, which is not modelled at all.
- **`cgb_sound` 09** fails for exactly the reason its DMG counterpart does, and the other
  eleven pass. The two suites deliberately test *opposite* expectations for three behaviours —
  whether powering off clears the length counters, whether `NRx1` length writes land while the
  APU is off, and whether the CPU may reach wave RAM mid-playback — so each is gated on the
  model rather than fixed one way.
- **dmg-acid2 now passes**, compared pixel-for-pixel against its published reference: all 23 040
  matched, so the DMG background, window, sprite priority, and both tile-mapping arrangements are
  validated end to end rather than argued for. The hash is recorded in the corpus along with the
  commands to redo the comparison; the reference image is fetched, never committed, same rule as
  the ROMs.
- **cgb-acid2 now passes too**, which makes it the only end-to-end check of CGB tile attributes,
  the second VRAM bank, OBJ palettes, and CGB sprite priority. Getting there found two real bugs,
  both the same shape — a CGB read the DMG way, producing a complete and plausible wrong picture
  rather than an error. Sprite attributes were decoded with the DMG's rule, so every sprite drew
  through OBJ palette 0 with bank 0 tiles; and sprites were ordered by X coordinate, which is the
  DMG rule where a CGB orders by OAM index. Neither would have been caught by anything except a
  reference comparison.

Blargg's sound ROMs report through cartridge RAM rather than the serial port, and the message
they leave there names the exact rule that failed. `cargo test -p harness --release --
--ignored --nocapture dmg_sound_results` prints all twelve.

All are tracked as known failures in the corpus, so the suite stays green for *regressions*
while they are open — and fails loudly if one starts passing, which means the marker needs
removing.

**All three `gba-suite` ROMs pass** — the whole instruction set in both states, and the memory
suite. Between them they found two real bugs that no amount of unit testing would have caught:
the ARM7TDMI's legacy "P" form, and 32-bit writes to palette RAM and VRAM being decomposed into
bytes and so corrupted by the 16-bit bus quirk that applies to genuine byte writes.

The Game Boy Color's colour *rendering* is now validated against a reference — that is what
cgb-acid2 covers. Its speed switch and VRAM DMA are still checked against hardware documentation
and unit tests only, `OPRI` is not modelled at all, and the Mooneye CGB suite is not in the corpus
yet.

There is now a GBA to run them on, which there was not before.

## Installing

There are no published releases yet. When there is a tag, CI builds `alpha-emulator` (the windowed
application) and `alpha-headless` (the CLI driver) for Linux, macOS on both architectures, and
Windows, and attaches them to a draft GitHub release. Nothing is code-signed or notarised, so macOS
and Windows will warn on first run — expected for an unsigned build, not a sign the archive is wrong.

Until then, build it: see below.

## Setup

You need a Rust toolchain ([rustup](https://rustup.rs) — the pinned version is selected
automatically) and a ROM file you own. On Linux you also need two sets of development headers,
which `cargo xtask setup` names for you.

```sh
cargo xtask setup                       # checks your machine, prints anything missing
cargo xtask dev -- path/to/rom.gba      # build, then open that cartridge
cargo xtask dev                         # …or open with no cartridge and drag one in
```

The first build takes a few minutes; later ones take seconds. `cargo xtask setup` never downloads
or vendors a binary into the repository — if a system package is missing it prints the exact
`apt`/`dnf`/`pacman` command and exits non-zero.

**[SETUP.md](SETUP.md) is the full guide**: per-OS packages, controls, where your saves go, and what
to do when something goes wrong.

## Developer tasks

Everything goes through `xtask`, which is a Rust program and therefore behaves identically on
Linux, macOS, and Windows:

| Command | What it does |
|---|---|
| `cargo xtask setup` | Verify host toolchain and system packages |
| `cargo xtask dev` | Run the native frontend (`-- <rom>` to open a cartridge) |
| `cargo xtask build --release` | Build the workspace optimized |
| `cargo xtask test` | `cargo test --workspace` (`--accuracy` adds the test-ROM suite) |
| `cargo xtask bench` | Criterion benchmarks (`--quick`, `--filter`, `--save-baseline`, `--baseline`) |
| `cargo xtask profile <rom>` | Build the release driver and print the flamegraph command |
| `cargo xtask fetch-test-roms` | Download the accuracy test-ROM corpus (never committed) |
| `cargo xtask lint` | `rustfmt --check` + `clippy -D warnings`, exactly as CI runs them |

### What CI checks

`ci.yml` runs on every push and pull request: `rustfmt` and `clippy`, the crate-boundary rule via
`cargo deny`, unit tests and the full accuracy suite on Linux, macOS, and Windows, `cargo doc` with
warnings denied, a release-profile build of the two shipped binaries, and `cargo bench --no-run`.

The last two are there because both fail in ways nothing else catches. `panic = "abort"` and thin LTO
apply only to the release profile, so a release-only compile error is real and would otherwise be
discovered while cutting a tag. And nothing references the benchmarks, so they would rot silently
until the next person needed a measurement — CI compiles them but never times them, because timings
on a shared runner are noise and a check people learn to ignore is worse than no check.

Every job pins the same toolchain `rust-toolchain.toml` gives a fresh clone. A CI that quietly ran a
newer compiler would let a lint land that nobody local sees, or reject one everybody local passes.

`docs.yml` publishes the rendered API documentation to GitHub Pages from `main`. Most of what a
contributor needs here is `//!` prose beside the code it describes, so it is worth reading rendered.

### Playing a game

`cargo xtask dev` opens the application. Drop a `.gb`, `.gbc`, `.gba`, or `.nds` file onto the window — or
paste its path into the library panel's import box — and it is indexed and starts playing. A ROM
named on the command line does the same in one step. On a DS game the lower screen is the
touchscreen: click and drag on it with the mouse.

The library is a SQLite index, not a directory scan, and that distinction is load-bearing: a title
you correct, a play count, and a save-state list all survive the file being moved to another folder,
because reconciliation recognises it again by content hash. Files that have genuinely gone are
greyed out and keep their history rather than vanishing.

| Default key | Action |
|---|---|
| `W` `A` `S` `D` | D-pad |
| `Space` / `R` | A / B |
| `T` / `G` / `Q` / `E` | X / Y / L / R (GBA and DS) |
| `Enter` / `Left Shift` | Start / Select |
| `P` | Pause |
| `Tab` (held) | Fast-forward |
| `Backspace` (held) | Rewind |
| `F1` | HUD |
| `F2` / `F3` | Quicksave / quickload (slot 0) |
| `F9` | Debugger panel |
| `F11` / `F12` | Fullscreen / screenshot |
| `Escape` | Reset |

All of them are rebindable in the **Keys** panel, and the bindings are physical key *positions*
rather than letters, so a non-QWERTY layout gets the keys under the same fingers. Settings and
keybinds are written as hand-editable TOML; run with `--data-dir <path>` to keep a whole separate
library, saves, and config, which is what to use when trying something out.

Everything the emulator writes goes to the OS-appropriate local-app-data directory — the paths are
printed at startup.

### Running a ROM without a GUI

`frontend-headless` runs a ROM with no window, no audio device, and no GPU:

```sh
cargo run -p frontend-headless -- run path/to/rom.gb --frames 600
cargo run -p frontend-headless -- run path/to/rom.gb --frames 600 --trace-every 60
cargo run -p frontend-headless -- run path/to/rom.gb --frames 120 --save-frame out.png
cargo run -p frontend-headless -- check-determinism path/to/rom.gb --frames 600
cargo run -p frontend-headless -- identify path/to/rom.gb
```

A `.gb` file runs on Game Boy hardware and a `.gbc` file on Game Boy Color hardware; what the
cartridge header says decides whether a colour machine runs in full colour or in
DMG-compatibility mode.

`--save-frame` writes the final framebuffer as a PNG, which is how a rendering test ROM gets
*looked at* rather than reduced to a hash. `identify` runs the same probe the library importer
does, so the title and content hash it prints are the ones that would be indexed.

`run` prints a framebuffer hash — the same FNV-1a the accuracy corpus records, so a hash
printed here can be pasted straight into a corpus entry. `--trace-every` prints one per N
frames, which is how you locate the frame where two builds diverge rather than just learning
that they did. `check-determinism` runs the same ROM twice from a fresh machine and compares:
determinism is what save states, rewind, and replay all rest on, and it is cheap to check and
easy to lose.

## Performance

Measured on an Apple M3, `bench` profile. Full numbers, the per-frame apportionment, and the
dynamic-recompilation decision live in `testing/harness/benches/systems.rs` — beside the benchmarks
that produced them rather than in prose that can drift from them.

| workload | frame time | speed |
|---|---|---|
| Game Boy, rendering | 246 µs | 68x |
| Game Boy, rendering + four APU channels | 361 µs | 46x |
| Game Boy Advance, ARM instructions straight from ROM | 1 486 µs | 11.3x |
| dmg-acid2 / cgb-acid2 | 243 / 258 µs | 69x / 65x |
| Nintendo DS, both cores spinning, displays off | 5 161 µs | 3.2x |
| Nintendo DS, engine A reading a VRAM framebuffer | 5 276 µs | 3.2x |
| Nintendo DS, the same with the sound hardware wired in | ≈5 440 µs | ≈3.1x |
| Nintendo DS 3D rasteriser, three screen-filling quads with overdraw | ≈730 µs | — |

Three findings worth surfacing:

- **The APU costs more than the PPU on a Game Boy frame** — about a third of it against a sixth. Not
  what you would guess for a machine whose job is drawing a picture, and the first place to look if
  the Game Boy ever needs to be faster.
- **No dynamic recompiler, for either CPU core**, on the evidence rather than by preference. A dynarec
  replaces dispatch and nothing else, and the worst measured workload on each system already runs at
  46x and 11.3x real time.
- **The Nintendo DS has a fifth of the margin the GBA does, and prompt 18 was right about it.** At
  3.2x real time against the other systems' 11x to 80x, it is by a wide margin the tightest — and
  that is *without* the 3D core. The dynarec question stays open for it rather than being inherited
  from the two answers above; it needs re-asking once the 3D rasteriser exists, since that is the
  workload prompt 18 expects to be the real problem.

  The first measurement was 15.0 ms against a 16.71 ms budget — 1.1x, barely real time. The cause
  was not the two 2D engines: turning both displays off saved 1.5%. It was that `NdsBus` composed
  every halfword and word access out of byte accesses, so each instruction fetch cost four region
  decodes. Reading and writing RAM at its real width dropped a frame from 15.2 ms to 5.28 ms, a
  **65% reduction**, measured before and after with `cargo bench -p harness --bench systems`. That
  is the only optimisation in the project so far, and it had a measured problem behind it.
- **The debugger's watchpoint recorder is not free**: +1.7% of a Game Boy frame and +4.5% of a GBA
  one and +3.7% of a DS one, even disarmed, because it is a branch on every bus access. That fails the "zero measurable
  overhead" constraint it was written against, and it is kept anyway — a Cargo feature would either
  leave the shipped build paying it or leave the shipped build without watchpoints. Documented as a
  deliberate deviation with the number, not as compliance.

Nothing has been optimised, deliberately: every system meets its target with 11x to 80x of margin, and
an optimisation with no problem behind it is not worth its own risk.

## Architecture

The full design rationale — why this stack, what the crate boundaries mean, and what was
deliberately learned from the predecessor project — lives in
[`docs/successor-emulator/`](docs/successor-emulator/), starting with
[`00-INDEX-AND-ARCHITECTURE.md`](docs/successor-emulator/00-INDEX-AND-ARCHITECTURE.md).

Two rules are load-bearing and enforced mechanically rather than by review:

1. **Crate boundaries.** No crate under `crates/system-*`, `crates/cpu-*`, `crates/ppu-*`,
   `crates/apu-*`, `savestate`, or `core-common` may depend on `winit`, `wgpu`, `egui`, or
   `cpal`. Enforced by `cargo deny check bans` in CI.
2. **`Savable` at creation time.** Every stateful struct implements save/load when it is
   written, never bolted on later by reaching into another module's private fields.

See [CONTRIBUTING.md](CONTRIBUTING.md), and [AGENTS.md](AGENTS.md) if you are an AI agent
picking this up — it carries the conventions and the paid-for mistakes that the code itself
cannot tell you.

## License

Dual-licensed under MIT or Apache-2.0, at your option.
