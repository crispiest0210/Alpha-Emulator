# Contributing

Working on this as an AI agent? Read [AGENTS.md](AGENTS.md) as well — it carries the standing
workflow and the architectural principles this codebase has settled on.

## Getting started

[SETUP.md](SETUP.md) gets you from a clone to a running emulator. Once that works:

```sh
cargo xtask build
cargo xtask test
cargo xtask lint     # run this before opening a PR; CI runs exactly the same commands
```

`cargo xtask lint --fix` applies `rustfmt` and machine-applicable clippy fixes.

## The three rules most likely to be broken

These are the invariants an unfamiliar contributor is most likely to violate, and each exists
because the predecessor project violated it and paid for it.

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

### 3. A UI panel returns what the user asked for; it does not do it

**Nothing under `crates/frontend-native/src/chrome/` may touch the session, the library, the
window, or the filesystem.** A panel is handed a `ChromeState` to draw and returns `UiAction`s;
`app.rs` is the single place that interprets them.

The predecessor's entire frontend was one ~2,200-line React component doing UI state, canvas
drawing, Web Audio glue, keyboard routing, and IPC together. Splitting it into files would not have
fixed that — a settings checkbox could still have reached into the emulator. What fixes it is that
a panel has *no way* to: `ChromeState` borrows only what is drawable, and every side effect in the
application is reachable from one `match` in `app.rs`.

Two consequences worth knowing before you add a control:

- If your panel needs something new, add a field to `ChromeState` or a variant to `UiAction` — do
  not pass it the `Session` or the `Library`.
- Session and emulation logic belongs in `frontend-core`, not here. Any future web or TUI frontend
  reuses that crate whole and writes only its own presentation layer, and that reuse is the entire
  reason for the split.

The same rule is why `frontend-core` owns the library *integration* (`catalog`) while the UI thread
owns the `rusqlite::Connection`: the emulation thread reports facts — a state file was written, at
this path, at this frame — and never waits on a lock, because it has a 16.7 ms deadline.

## Testing

- Unit tests live next to the code, run with `cargo xtask test`.
- **`frontend-native` is verified by running it, not only by compiling it.** The pure parts have
  unit tests — coordinate mapping and touch translation in `layout.rs`, key translation in
  `keymap.rs`, and the PNG encoder over in `frontend-core`'s `png.rs` — and session behaviour is
  driven end to end
  through the real channel API in `frontend-core`'s tests. Everything left is presentation, and the
  only way to check presentation is to look at it: `cargo xtask dev -- <rom>`, or
  `cargo run -p frontend-native -- --data-dir /tmp/scratch <rom>` to do it against a throwaway
  library rather than your own.
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

## Reporting a bug

When a picture is wrong or the machine behaves unexpectedly, the most useful thing to attach is a
**save state from the moment it went wrong**, because a state resumes byte-exactly: running five
frames past a loaded state gives the same framebuffer hash as running the whole way from a reset.
That turns "the graphics are wrong after an hour of play" — which no press schedule can reach —
into a two-minute diagnosis anyone can reproduce.

1. Play to the frame where the bug is visible and press `F2` (quicksave, slot 0 by default).
2. Find the state file. Save states live under the application's data directory, in a per-ROM
   subdirectory named after the ROM's file stem:

   ```text
   <data-dir>/states/<rom-file-stem>/slot0.ast
   ```

   The data directory is OS-specific and **printed at startup** — look for the `data:` line in the
   log. If you ran with `--data-dir <path>`, it is `<path>/data` instead.

3. Re-render that state headlessly and dump the frame as a PNG:

   ```sh
   cargo run -p frontend-headless -- run path/to/rom.gba \
     --state <data-dir>/states/<rom-file-stem>/slot0.ast \
     --frames 1 --save-frame out.png
   ```

4. Attach **the `.ast` state file and `out.png`** to the report, and say which ROM it is — the ROM
   itself must not be attached, and is never needed to start the diagnosis.

`run` also prints a framebuffer hash (the same FNV-1a the accuracy corpus records), so two people
can confirm they are looking at the same frame before arguing about what is in it. `--trace-every N`
prints one hash every N frames, which is how the frame where two builds diverge gets located rather
than merely established.

For a rendering bug specifically, the layer-by-layer bisect described in [AGENTS.md](AGENTS.md)
under "Gotchas" is what found four of the seven GBA rendering bugs, each in minutes.

## Licensing

Dual-licensed under MIT or Apache-2.0, at the user's option — `LICENSE-MIT` and `LICENSE-APACHE`.
Contributions are accepted under the same terms. Release archives carry both files, because a
dual-licensed project that ships neither is not actually offering the choice it claims to.

## Pull requests

- `cargo xtask lint` and `cargo xtask test` must pass locally.
- Keep documentation honest. The per-system status table in `README.md` reflects what actually
  works; if your change moves a system's status, update it in the same PR. An accurate
  "early/partial" is always preferred over an aspirational "supported".
