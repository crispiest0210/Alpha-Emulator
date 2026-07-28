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
| Game Boy (DMG) | ❌ | ❌ | ❌ | CPU, memory map, and timing done; no rendering or sound yet |
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
| `cpu-sm83` — Game Boy CPU | complete, unit-tested; **accuracy ROMs not yet run** |
| `cpu-arm7tdmi` — GBA / DS ARM7 CPU | complete, unit-tested; **accuracy ROMs not yet run** |
| `cpu-arm946e` — DS ARM9 CPU (ARMv5TE, CP15, TCM) | complete, unit-tested; **accuracy ROMs not yet run** |
| `cart-common` — headers, MBC1/2/3/5, SRAM/Flash/EEPROM, RTCs | done for GB and GBA save chips |
| `system-gb` memory map | done (WRAM/VRAM banking, echo RAM, boot ROM) |
| `system-gb` timing — timer, PPU mode machine, APU sequencer | done as scheduled events; **Mooneye timer ROMs not yet run** |
| `ppu-tile2d` — tile decode, palettes, scanline compositing | done for GB/GBC/GBA formats |
| `system-gb` PPU — background, window, sprites | done, scanline-accurate; **dmg-acid2 not yet run** |
| `apu-shared` — square/wave/noise channels, envelope, sweep, mixer | done; **Blargg dmg_sound not yet run** |
| `frontend-core` audio pipeline — lock-free ring, resampler | done; cpal device binding pending (prompt 14) |
| Everything else | not started |

The CPU cores pass their unit tests but have **not** been validated against the accuracy
test-ROM suites (Blargg, Mooneye, arm7wrestler, gba-suite) that actually gate them — that
harness arrives with prompt 17. Until then, treat them as unverified against real hardware
behavior.

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
