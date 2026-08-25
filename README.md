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

| System | Boots | Playable | Accuracy suite | Notes |
|---|---|---|---|---|
| Game Boy (DMG) | ✅ | ✅ | ⚠️ | Plays in the window with sound and input, measured at 100% speed. Passes all 11 Blargg `cpu_instrs` sub-tests, `instr_timing`, `mem_timing`, `dmg-acid2` pixel-exact, and 9 of 12 `dmg_sound` sub-tests |
| Game Boy Color | ✅ | ✅ | ⚠️ | Plays. 11 of 12 Blargg `cgb_sound` sub-tests pass; `cgb-acid2` is pixel-exact against its reference |
| Game Boy Advance | ✅ | ✅ | ⚠️ | Passes `gba-suite`'s ARM, Thumb, memory, and BIOS ROMs. **A commercial game plays at a measured 100% speed** with backgrounds, sprites, affine effects, mosaic, and colour blending. BIOS calls work in both instruction sets, decompressors included. **All four PSG channels are mixed alongside direct sound.** The `save/` ROMs found two real save-chip bugs, tracked as known failures; no cartridge clock or EEPROM |
| Nintendo DS | ✅ | ⚠️ | ⚠️ | **Runs real libnds homebrew**, including one that streams its data off its own cartridge and renders a textured, normal-mapped 3D scene. Two programs, not a commercial game: no retail title has been tried |

**All four systems boot with picture and sound, and three of them play commercial games at full
speed.** The fourth, the DS, runs homebrew correctly but has not been tried on a retail game. The
Game Boy and Game Boy Color are held to a full accuracy bar; see each system's notes for specifics.

The application has a ROM library, video, audio, keyboard and touchscreen input, quicksave and
quickload, rewind, an HUD, a rebindable keymap, screenshots, and an in-app debugger with registers,
disassembly, a memory viewer, breakpoints, and watchpoints.

## What actually works

For each system's implementation status, gaps worth noting, and the bugs worth knowing about before
debugging timing or graphics, see:

- **Game Boy / Game Boy Color**: The core is complete and holds a full accuracy bar against
  Blargg's test ROMs. Minor known gaps are listed in `README.md`'s accuracy suite section and in
  `system-gb`'s crate docs.
- **Game Boy Advance**: `gba-suite`'s ARM, Thumb, memory, and BIOS ROMs pass, and a commercial game
  plays at full speed with sound. Seven rendering bugs surfaced and were fixed; see
  **[AGENTS.md](AGENTS.md)** under "Gotchas" for what they looked like and how they were found.
  All four PSG channels are now mixed alongside direct sound, closing what was long the project's
  biggest gap. Still open: the `save/` ROMs expose two real save-chip protocol bugs, and there is
  no cartridge clock or EEPROM. See `system-gba`'s crate docs for the full list.
- **Nintendo DS**: Boots and draws both screens with sound and 3D. Runs real libnds homebrew.
  **No commercial title has been tried yet.** See `system-nds`'s crate docs and **[AGENTS.md](AGENTS.md)**
  under "Start here" for the gaps, the implementation summary, and what has been verified.

## Performance

Measured on an Apple M3. **[docs/performance.md](docs/performance.md)** has full numbers and the
reasoning: all systems meet their targets with 5x to 80x margin, so no dynamic recompiler has been
added. The DS is the tightest at 3.2x real time. One optimisation — the DS bus reading RAM at its
real width instead of composing wide accesses out of byte accesses — dropped a frame by 65% and is
the only one worth the risk.

## Installing

There are no released builds yet. When there is a tag, CI builds the windowed application and
CLI driver for Linux, macOS on both architectures, and Windows. Nothing is code-signed or
notarised, so macOS and Windows will warn on first run — expected for an unsigned build.

Until then, build it yourself: `cargo xtask setup` and `cargo xtask dev`. See **[SETUP.md](SETUP.md)**.

## Playing and debugging

- `cargo xtask dev` opens the native frontend. Drop a ROM onto the window or paste its path into
  the library panel.
- `cargo run -p frontend-headless -- run path/to/rom.gb --frames 600` plays without a window. Use
  `--save-frame out.png` to render the final framebuffer, `--state file.ast` to load a save state,
  and `--press button@frame` for scripted input.
- **Reporting a bug?** See **[CONTRIBUTING.md](CONTRIBUTING.md)** under "Reporting a bug" for how
  to capture the frame where it happens and make the diagnosis five-minute instead of five-hour.

## Developer guide

- **[SETUP.md](SETUP.md)**: Building, controls, where saves go
- **[CONTRIBUTING.md](CONTRIBUTING.md)**: The three rules, testing, bug reports
- **[AGENTS.md](AGENTS.md)**: Standing workflow and paid-for mistakes (read if you are an AI agent)
- **[docs/performance.md](docs/performance.md)**: Benchmarks and the optimisation policy
- **[docs/successor-emulator/](docs/successor-emulator/)**: Full architecture and design rationale

The project works through twenty prompt files in `docs/successor-emulator/` **in order**, starting
with **[00-INDEX-AND-ARCHITECTURE.md](docs/successor-emulator/00-INDEX-AND-ARCHITECTURE.md)**.

## Accuracy suite

The Game Boy suite runs: `cargo xtask fetch-test-roms` pulls the corpus, then `cargo xtask test --accuracy`
runs it. Nothing is vendored; the tests skip cleanly when ROMs are absent. **[AGENTS.md](AGENTS.md)**
documents what ROMs pass, which ones don't, and why.

## Crate architecture

Two mechanical rules are enforced by CI:

1. **Crate boundaries are a dependency direction.** No system/CPU/PPU/APU crate may depend on
   `winit`, `wgpu`, `egui`, or `cpal`. The emulation core is a pure library.
2. **`Savable` is implemented when the struct is written**, never bolted on later by reaching into
   another module's private fields.

See **[CONTRIBUTING.md](CONTRIBUTING.md)** for details and the rationale behind each.

## License

Dual-licensed under MIT or Apache-2.0, at your option.
