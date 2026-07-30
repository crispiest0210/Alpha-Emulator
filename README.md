# Alpha Emulator

A from-scratch, multi-generation Nintendo handheld emulator written in Rust, targeting the
Game Boy, Game Boy Color, Game Boy Advance, and Nintendo DS.

It is a clean-room implementation: the emulation core is original Rust, with no vendored
third-party emulator embedded as a black box, and no browser engine in the rendering path.
Rendering is native GPU (`wgpu` + `winit`), audio is `cpal` fed from a lock-free ring buffer,
and the emulation core is a pure library with zero UI-framework dependency.

## Status

This project is in early development. The table below reflects what is **actually implemented
and tested**, not what is planned. It is updated as work lands.

| System | Boots | Playable | Accuracy suite | Notes |
|---|---|---|---|---|
| Game Boy (DMG) | ✅ | ✅ | ⚠️ | Plays in the window with sound and input, measured at 100% speed. Passes all 11 Blargg `cpu_instrs` sub-tests, `instr_timing`, `mem_timing`, `dmg-acid2` pixel-exact, and 9 of 12 `dmg_sound` sub-tests — see below |
| Game Boy Color | ✅ | ✅ | ⚠️ | Plays. 11 of 12 Blargg `cgb_sound` sub-tests pass; `cgb-acid2` is pixel-exact against its reference |
| Game Boy Advance | ✅ | ✅ | ✅ | Plays, measured at 100% speed; passes all three `gba-suite` ROMs. Keypad and affine backgrounds work. No PSG mixing, mosaic, or EEPROM yet |
| Nintendo DS | ❌ | ❌ | ❌ | Both CPU cores done; nothing else. The frontend already lists `.nds` files and greys them out rather than pretending otherwise |

**Three of the four systems are playable.** `cargo xtask dev` opens a window with a ROM library,
plays a cartridge with video, audio, and keyboard input, and supports quicksave, quickload, rewind,
an HUD, a keybind editor, screenshots, and an in-app debugger — registers, disassembly, memory,
execution breakpoints, and read/write watchpoints. The Nintendo DS is the remaining gap (prompt 13).

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
| `core-common` — `DebugTarget`, `System::step_instruction`, `AccessLog` | done; the GB, GBC, and GBA implement introspection and access recording, the DS reports both as unavailable |
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

## Setup

Requires a Rust toolchain (installed via [rustup](https://rustup.rs); the pinned version is
selected automatically from `rust-toolchain.toml`).

```sh
cargo xtask setup                       # checks for required system packages, tells you what to install
cargo xtask dev                         # build and run the native frontend
cargo xtask dev -- path/to/rom.gb       # …and start playing a cartridge straight away
```

`cargo xtask setup` never downloads or vendors a binary into the repository — if a system
package is missing it prints the exact `apt`/`dnf`/`pacman` command and exits non-zero.

See [SETUP.md](SETUP.md) for per-OS detail.

## Developer tasks

Everything goes through `xtask`, which is a Rust program and therefore behaves identically on
Linux, macOS, and Windows:

| Command | What it does |
|---|---|
| `cargo xtask setup` | Verify host toolchain and system packages |
| `cargo xtask dev` | Run the native frontend (`-- <rom>` to open a cartridge) |
| `cargo xtask build --release` | Build the workspace optimized |
| `cargo xtask test` | `cargo test --workspace` (`--accuracy` adds the test-ROM suite) |
| `cargo xtask bench` | Run benchmarks |
| `cargo xtask fetch-test-roms` | Download the accuracy test-ROM corpus (never committed) |
| `cargo xtask lint` | `rustfmt --check` + `clippy -D warnings`, exactly as CI runs them |

### Playing a game

`cargo xtask dev` opens the application. Drop a `.gb`, `.gbc`, or `.gba` file onto the window — or
paste its path into the library panel's import box — and it is indexed and starts playing. A ROM
named on the command line does the same in one step.

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
