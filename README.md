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
| Game Boy (DMG) | ✅ | ⚠️ | ⚠️ | Runs, renders, and sounds. Passes all 11 Blargg `cpu_instrs` sub-tests, `instr_timing`, and `mem_timing`; three gaps remain — see below |
| Game Boy Color | ❌ | ❌ | ❌ | Shares the CPU core; nothing else started |
| Game Boy Advance | ❌ | ❌ | ❌ | CPU core done; no memory map, PPU, or APU yet |
| Nintendo DS | ❌ | ❌ | ❌ | Both CPU cores done; nothing else. Will be explicitly partial when it does begin |

**No system runs a ROM yet.** `cargo xtask dev` opens an empty window.

Component status:

| Component | Status |
|---|---|
| Workspace, `xtask`, CI, crate-boundary enforcement | done |
| `core-common` — scheduler, bus, CPU/system traits | done |
| `savestate` — versioned format and `Savable` | core done; rewind buffer pending (prompt 16) |
| `cpu-sm83` — Game Boy CPU | complete; passes all Blargg CPU and timing suites |
| `cpu-arm7tdmi` — GBA / DS ARM7 CPU | complete, unit-tested; **accuracy ROMs not yet run** |
| `cpu-arm946e` — DS ARM9 CPU (ARMv5TE, CP15, TCM) | complete, unit-tested; **accuracy ROMs not yet run** |
| `cart-common` — headers, MBC1/2/3/5, SRAM/Flash/EEPROM, RTCs | done for GB and GBA save chips |
| `system-gb` memory map | done (WRAM/VRAM banking, echo RAM, boot ROM) |
| `system-gb` timing — timer, PPU mode machine, APU sequencer | done as scheduled events; **Mooneye timer ROMs not yet run** |
| `ppu-tile2d` — tile decode, palettes, scanline compositing | done for GB/GBC/GBA formats |
| `system-gb` PPU — background, window, sprites | done, scanline-accurate; **dmg-acid2 output unvalidated** |
| `apu-shared` — square/wave/noise channels, envelope, sweep, mixer | done; **Blargg dmg_sound not yet run** |
| `frontend-core` audio pipeline — lock-free ring, resampler | done; cpal device binding pending (prompt 14) |
| `system-gb` APU — NR10-NR52 register layer | done; **Blargg dmg_sound does not run to completion** |
| `frontend-core` input — keybinds, conflict rule, delivery | done; keyboard only, gamepads are future work |
| `system-gb` assembly — `System` impl, joypad, OAM DMA, boot | done; save-state round-trip is frame-exact |
| `testing/harness` — accuracy runner, fetch automation | done; drives the GB suite end to end |
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
| Blargg `cpu_instrs` (combined ROM) | hangs after printing sub-test 03's name — see below |
| Blargg `dmg_sound` | no serial output at all within the frame budget |
| dmg-acid2 | renders, but the output has **not** been checked against the reference image, so it is unvalidated rather than passing |

The combined `cpu_instrs` ROM is **not** a CPU bug: every one of its eleven sub-tests passes
standalone, and MBC1 bank reads are verified against that exact ROM by a dedicated test. The
cause is somewhere in the combined ROM's own sequencing between sub-tests and is still open.

The three remaining gaps are tracked as known failures in the corpus, so the suite stays green
for *regressions* while they are open — and fails loudly if a known-failing ROM starts
passing, which means the marker needs removing.

The ARM cores have not been run against anything: `arm7wrestler` and `gba-suite` are not in
the corpus yet, and there is no GBA system to run them on.

## Setup

Requires a Rust toolchain (installed via [rustup](https://rustup.rs); the pinned version is
selected automatically from `rust-toolchain.toml`).

```sh
cargo xtask setup   # checks for required system packages, tells you exactly what to install
cargo xtask dev     # build and run the native frontend
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
| `cargo xtask dev` | Run the native frontend |
| `cargo xtask build --release` | Build the workspace optimized |
| `cargo xtask test` | `cargo test --workspace` (`--accuracy` adds the test-ROM suite) |
| `cargo xtask bench` | Run benchmarks |
| `cargo xtask fetch-test-roms` | Download the accuracy test-ROM corpus (never committed) |
| `cargo xtask lint` | `rustfmt --check` + `clippy -D warnings`, exactly as CI runs them |

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

See [CONTRIBUTING.md](CONTRIBUTING.md).

## License

Dual-licensed under MIT or Apache-2.0, at your option.
