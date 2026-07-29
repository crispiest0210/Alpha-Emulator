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
| Game Boy (DMG) | ✅ | ⚠️ | ⚠️ | Runs, renders, and sounds. Passes all 11 Blargg `cpu_instrs` sub-tests, `instr_timing`, `mem_timing`, and 9 of 12 `dmg_sound` sub-tests — see below |
| Game Boy Color | ✅ | ⚠️ | ⚠️ | Assembled and running. 11 of 12 Blargg `cgb_sound` sub-tests pass; `cgb-acid2` renders but is unvalidated |
| Game Boy Advance | ❌ | ❌ | ❌ | CPU, memory map, interrupts, timers, DMA, video timing, bitmap modes, text backgrounds, and sprite decode done and tested. No affine transforms, APU, or assembly yet |
| Nintendo DS | ❌ | ❌ | ❌ | Both CPU cores done; nothing else. Will be explicitly partial when it does begin |

**The Game Boy core runs ROMs; the GUI does not yet.** The emulation core boots cartridges,
renders frames, and produces audio — that is what the accuracy suite below drives it through.
What is missing is the frontend that connects it to a window: `cargo xtask dev` still opens an
empty one. Wiring the two together is prompt 14.

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
| `ppu-tile2d` — tile decode, palettes, scanline compositing | done for GB/GBC/GBA formats; both sprite-priority rules |
| `system-gb` PPU — background, window, sprites | done, scanline-accurate; **dmg-acid2 output unvalidated** |
| `apu-shared` — square/wave/noise channels, envelope, sweep, mixer | done; 9 of 12 Blargg `dmg_sound` sub-tests pass |
| `frontend-core` audio pipeline — lock-free ring, resampler | done; cpal device binding pending (prompt 14) |
| `system-gb` APU — NR10-NR52 register layer | done; DMG wave-RAM window is machine-cycle accurate, not t-cycle |
| `frontend-core` input — keybinds, conflict rule, delivery | done; keyboard only, gamepads are future work |
| `system-gb` assembly — `System` impl, joypad, OAM DMA, boot | done; save-state round-trip is frame-exact |
| `system-gb` CGB blocks — palette RAM, `KEY1`, VRAM DMA, tile attributes | done and driven by the bus |
| `system-gbc` — `System` impl, model selection, compatibility boot | done; 11 of 12 `cgb_sound` sub-tests pass |
| `testing/harness` — accuracy runner, fetch automation | done; drives the GB suite end to end |
| `frontend-headless` — CLI driver, framebuffer hashing, determinism check | done for the Game Boy family |
| `system-gba` memory map — regions, mirroring, open bus, 8-bit write quirk | done |
| `system-gba` interrupt controller, timers, 4-channel DMA | done and tested; not yet driven |
| `system-gba` video timing and bitmap modes 3/4/5 | done and tested; not yet driven |
| `system-gba` text backgrounds — four layers, map decode, draw order | done and tested; not yet driven |
| `system-gba` sprites — OAM decode, sizes, per-line selection, matrices | done and tested; affine transform not applied yet |
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
| dmg-acid2, cgb-acid2 | render and complete, but unvalidated — see below |

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
- **dmg-acid2 and cgb-acid2** render and then halt waiting for interrupts, which is how they
  signal they have finished. The pictures are probably correct; they stay unvalidated only
  because nobody has compared them against the published reference images. Doing so would make
  cgb-acid2 the only end-to-end check of tile attributes, the second VRAM bank, and CGB sprite
  priority.

Blargg's sound ROMs report through cartridge RAM rather than the serial port, and the message
they leave there names the exact rule that failed. `cargo test -p harness --release --
--ignored --nocapture dmg_sound_results` prints all twelve.

All are tracked as known failures in the corpus, so the suite stays green for *regressions*
while they are open — and fails loudly if one starts passing, which means the marker needs
removing.

The Game Boy Color's colour rendering, speed switch, and VRAM DMA are still checked against
hardware documentation and unit tests rather than a reference: `cgb_sound` exercises the APU
and the boot path, but validating cgb-acid2's output is what would cover the PPU. The Mooneye
CGB suite is not in the corpus yet either.

The ARM cores have not been run against anything either: `arm7wrestler` and `gba-suite` are not
in the corpus yet, and there is no GBA system to run them on.

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

### Running a ROM without a GUI

The native frontend is not wired up yet, but the core is. `frontend-headless` runs a ROM with
no window, no audio device, and no GPU:

```sh
cargo run -p frontend-headless -- run path/to/rom.gb --frames 600
cargo run -p frontend-headless -- run path/to/rom.gb --frames 600 --trace-every 60
cargo run -p frontend-headless -- check-determinism path/to/rom.gb --frames 600
```

A `.gb` file runs on Game Boy hardware and a `.gbc` file on Game Boy Color hardware; what the
cartridge header says decides whether a colour machine runs in full colour or in
DMG-compatibility mode.

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
