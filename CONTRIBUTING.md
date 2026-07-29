# Contributing

## Getting started

```sh
cargo xtask setup    # verifies host system packages, prints install commands if any are missing
cargo xtask build
cargo xtask test
cargo xtask lint     # run this before opening a PR; CI runs exactly the same commands
```

`cargo xtask lint --fix` applies `rustfmt` and machine-applicable clippy fixes.

## The two rules most likely to be broken

These are the two invariants that an unfamiliar contributor is most likely to violate, and both
exist because the predecessor project violated them and paid for it.

### 1. Crate boundaries are a dependency-direction contract

**No crate under `crates/core-common`, `crates/cpu-*`, `crates/ppu-*`, `crates/apu-*`,
`crates/system-*`, or `crates/savestate` may depend on `winit`, `wgpu`, `egui`, or `cpal`.**

The emulation core is a pure library, consumable by the native frontend, the headless CLI, the
test harness, and any future frontend, with zero UI-framework dependency. If you find yourself
wanting a windowing or audio-output type inside a system crate, the design is wrong: the core
produces a framebuffer and a sample buffer, and the frontend decides what to do with them.

This is enforced by `cargo deny check bans` in CI (see `deny.toml`), not by review alone. If your
PR fails that job, do not add your crate to the `wrappers` allowlist — fix the dependency.

Allowed direction:

```
frontend-native / frontend-headless  ->  frontend-core, library, debugger, system-*
system-*                             ->  cpu-*, ppu-tile2d, apu-shared, cart-common, core-common
cpu-* / ppu-* / apu-* / cart-common  ->  core-common, savestate
```

### 2. `Savable` is implemented when the struct is written, not later

**Every stateful struct implements `Savable` (save/load) at the moment it is created.**

Save-state fidelity must never depend on one module reflecting into another module's private
fields. The predecessor implemented save states by reaching into a third-party library's internal
object graph and re-poking bytes on load; the result was a mandatory "warm reboot after every
load" workaround that still corrupted tile data.

Concretely: if you add a field that affects emulated behavior, it belongs in that struct's
`save`/`load` and in the save-state format version bump. A PR that adds emulated state without
touching serialization will be asked to fix that before anything else.

## Testing

- Unit tests live next to the code, run with `cargo xtask test`.
- The accuracy test-ROM suite runs with `cargo xtask test --accuracy`. Test ROMs are **fetched at
  test time and never committed to this repository** — see `testing/harness/`.
- Adding a new accuracy test ROM: register it with the harness rather than adding a bespoke test
  binary, so it participates in the CI suite and per-system status reporting automatically.

### Known failures are tracked, not silenced

A ROM that does not pass yet carries an `expected_failure` note in `testing/harness/src/corpus.rs`.
The suite then stays green for *regressions* while that gap is open — and **fails loudly if the
ROM starts passing**, because a stale marker is a lie about what works.

Two rules follow, and both matter:

- **The note says why, specifically.** "Fails" is useless. The note should name the rule that is
  broken and what has been ruled out — the existing entries quote the failing check verbatim
  ("Exiting negate mode after calculation disables channel") or state the limit that blocks it
  ("the window resolves to one machine cycle here, and this ROM resolves it to single t-cycles").
  A future reader should be able to start fixing it without re-deriving the diagnosis.
- **If your change makes a ROM pass, delete its marker in the same PR.** The suite will tell
  you: an unexpected pass is a failure.

Do not add a marker to make a red suite go green without a diagnosis behind it. A marker is a
record of understood, deferred work, not a mute button.

Test ROMs for the same subsystem on different machines can expect **opposite** behaviour. The
DMG and CGB sound suites disagree on three APU rules, and "fixing" one silently regresses the
other. If a change makes one suite pass, run both — and if they conflict, the answer is to gate
the behaviour on the model, never to pick a side.

Blargg's ROMs report in two different ways and the harness has a convention for each —
`BlarggSerial` writes to the link port, `BlarggMemory` writes a result code and message to
cartridge RAM. Picking the wrong one makes a ROM look like it hangs when it has actually
finished and reported. `cargo test -p harness --release -- --ignored --nocapture
dmg_sound_results` prints what the memory-protocol ROMs actually said.

## Pull requests

- `cargo xtask lint` and `cargo xtask test` must pass locally.
- Keep documentation honest. The per-system status table in `README.md` reflects what actually
  works; if your change moves a system's status, update it in the same PR. An accurate
  "early/partial" is always preferred over an aspirational "supported".
