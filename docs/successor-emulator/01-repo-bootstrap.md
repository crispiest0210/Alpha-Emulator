# Prompt 01 — Repository Bootstrap

Read `00-INDEX-AND-ARCHITECTURE.md` first. All tech-stack and workspace-layout decisions there
are final; this prompt implements the skeleton, it does not re-decide the stack.

## Objective

Stand up the empty-but-correct Cargo workspace, `xtask` automation crate, CI scaffold, and
repository tooling config so that every later prompt can `cargo build`/`cargo test` from commit
one without fighting the build system.

## Context

The predecessor project's biggest onboarding friction was platform-specific build fragility: a
Tauri/WebKitGTK dependency forced RHEL/Rocky users to run a bespoke `setup_linux_deps.sh` that
vendored `.pc` files and shared libraries into the repo (`pkgconfig/`, `lib/`), and there were
two divergent entrypoints (`npm run dev` vs `./run.sh dev`) depending on OS. This project's
stack (`wgpu`/`winit`/`cpal`, no embedded webview) removes the root cause, but the bootstrap must
still actively guarantee a single, OS-uniform entrypoint rather than relying on that being true
by accident.

## Architectural Decisions (already made, implement them)

- One Cargo workspace at repo root; every crate from the layout in `00-INDEX...md §3` exists as
  a stub (`lib.rs` with a `// TODO(promptNN)` marker or trivial placeholder, enough to compile).
- `xtask` is the *only* supported entrypoint for non-trivial developer tasks: `cargo xtask setup`,
  `cargo xtask dev`, `cargo xtask build --release`, `cargo xtask test`, `cargo xtask bench`,
  `cargo xtask lint`. No shell scripts as the primary path — a Rust program run via `cargo run -p
  xtask --` (aliased through `.cargo/config.toml` `[alias] xtask = "run -p xtask --"`) is
  identical on Linux/macOS/Windows.
- `cargo xtask setup` checks for required system packages (this is where any *unavoidable*
  platform-native dependency, e.g. ALSA dev headers on Linux for `cpal`, is detected and the user
  is told exactly what to install per-distro) — but does not silently vendor compatibility shims
  into the repo the way the predecessor did. If a dependency is missing, print the exact install
  command for apt/dnf/pacman/brew and exit non-zero; do not attempt to auto-fix by downloading
  binaries.
- Formatting/linting are enforced, not advisory: `rustfmt.toml` at workspace root, `clippy`
  configured to deny warnings in CI (`cargo clippy --workspace --all-targets -- -D warnings`).
- License and MSRV are pinned explicitly (implementer picks a reasonable MSRV given the crates
  chosen in `00-...md`; record it in `rust-toolchain.toml` so `rustup` auto-selects it — this is
  itself part of "one-command setup").
- `cargo-deny` (or equivalent) config forbidding `winit`/`wgpu`/`egui`/`cpal` as dependencies of
  the non-frontend crates listed in `00-...md §3`, enforced in CI, not just documentation.

## Responsibilities

1. Workspace `Cargo.toml` with `[workspace] members = [...]` for every crate in the layout,
   `resolver = "2"`, shared `[workspace.dependencies]` table pinning versions of `serde`,
   `bincode`, `tracing`, etc. so individual crates just say `serde.workspace = true`.
2. `xtask` crate implementing the subcommands above using `clap` for argument parsing (add
   `clap` to `[workspace.dependencies]`).
3. `.github/workflows/ci.yml`: matrix over the three OSes, running fmt-check, clippy, `cargo
   test --workspace`, and a placeholder step for the accuracy suite (wired for real in prompt 17).
4. `rust-toolchain.toml`, `rustfmt.toml`, `.gitignore` (must ignore `target/`, any fetched
   `testing/test-roms/` content, local save/library data directories used by manual testing).
5. Top-level `README.md` for the new project (distinct from this repo's own README) covering:
   what it is, supported systems (with an honest "in progress" status per system), one-command
   setup (`cargo xtask setup && cargo xtask dev`), link to `docs/successor-emulator/` for anyone
   who wants the full architecture rationale.
6. `CONTRIBUTING.md` stub describing the crate-boundary rule from `00-...md §3` and the
   fact that new stateful structs must implement `Savable` at creation time (from §1, lesson 1) —
   these are the two rules most likely to be violated by an unfamiliar contributor, so they go in
   the contributor doc, not just this prompt file.

## Interfaces

No runtime interfaces yet — this prompt is pure scaffolding. The one API surface that matters:
`xtask`'s subcommand set, since every later prompt's "how do I run this" instructions assume it
exists exactly as named above.

## Constraints

- Every crate must compile (`cargo build --workspace`) and pass `cargo test --workspace` (even
  if tests are trivial placeholders) at the end of this prompt, on all three target OSes if you
  have access to test them, or with a clear note about what's unverified if you don't.
- Do not add real emulation logic in this prompt — it belongs in prompts 02+.
- Do not vendor any binary dependency, shared library, or `.pc` file into the repo under any
  circumstance, on any platform. If a platform truly requires a system package, that's a
  `cargo xtask setup` install instruction, never a committed binary.

## Deliverables

- Full workspace skeleton compiling cleanly with all crate stubs.
- Working `xtask` with all six subcommands (later prompts fill in what they orchestrate; `dev`
  and `build` can shell out to `cargo run -p frontend-native` / `cargo build --release -p
  frontend-native` even before that crate has real content).
- Green CI on a trivial commit.

## Acceptance Criteria

- Fresh clone → `cargo xtask setup` (or a no-op success if nothing is needed on the host OS) →
  `cargo xtask dev` produces a running (even if blank-window) process, with zero manual
  environment-variable exports, on Linux, macOS, and Windows.
- `cargo clippy --workspace --all-targets -- -D warnings` is clean.
- No crate outside `frontend-native`/`frontend-headless`/`frontend-core` pulls in a GUI/windowing
  dependency (verify with `cargo tree -p system-gb -i winit` style checks, or `cargo-deny`).

## Testing Requirements

- CI must actually run on all three OSes before this prompt is considered complete, not just be
  configured to.
- A trivial `#[test]` in each crate stub is enough at this stage; real tests arrive with real
  code in later prompts.

## Future Compatibility

Every later prompt assumes this workspace layout and `xtask` interface exist verbatim. Do not
rename crates or subcommands here without updating every later prompt file to match, since they
were written assuming these exact names.

## Notes

If `cargo-dist`/`cargo-packager` (prompt 19) or `cargo-deny` are not yet installed in the dev
environment, `cargo xtask setup` is the right place to install or instruct installing them too —
don't leave that to prompt 19 to discover cold.
