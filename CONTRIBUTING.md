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

## Pull requests

- `cargo xtask lint` and `cargo xtask test` must pass locally.
- Keep documentation honest. The per-system status table in `README.md` reflects what actually
  works; if your change moves a system's status, update it in the same PR. An accurate
  "early/partial" is always preferred over an aspirational "supported".
