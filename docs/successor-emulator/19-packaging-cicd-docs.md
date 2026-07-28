# Prompt 19 — Packaging, Release Automation, CI/CD, and Documentation

Read `00-INDEX-AND-ARCHITECTURE.md` and prompt 01 first. This prompt matures the CI scaffold from
prompt 01 into full release automation and establishes the documentation contributors and users
actually need — it should be revisited/extended incrementally as new systems (11–13) land, not
treated as a single one-time task done only at the very end.

## Objective

Full CI (accuracy suite + lint + build across three OSes, per prompt 17), automated cross-
platform release packaging via `cargo-dist` (or `cargo-packager`, per `00-...md`'s stack
decision), generated API documentation, and contributor/user-facing documentation that reflects
this project's actual state honestly (per-system completeness, known limitations) rather than
aspirational claims.

## Context

The predecessor's documentation (`README.md`, `SETUP.md`, `AGENTS.md`) was actually a relative
strength worth preserving as a *practice*: clear, current, explicit about what was and wasn't
implemented ("No automated test suite is currently configured," stated plainly). This project
should keep that honesty norm while fixing the underlying problems those docs were honestly
describing (no tests, fragile Linux packaging via vendored shims). Do not let this project's docs
regress into overclaiming completeness just because the architecture is more ambitious.

## Architectural Decisions

- CI (extending prompt 01's scaffold): on every push/PR — `cargo fmt --check`, `cargo clippy
  --workspace --all-targets -- -D warnings`, `cargo test --workspace`, the accuracy suite (prompt
  17) for every implemented system, across `ubuntu-latest`/`macos-latest`/`windows-latest`. Cache
  `cargo` registry/build artifacts appropriately for reasonable CI turnaround (fast incremental
  builds matter for contributor experience, per `00-...md`'s stated priorities).
- Release automation: tag-triggered CI job using `cargo-dist`/`cargo-packager` to produce
  installers/archives per OS (no manual, per-platform release scripts — this is the release-time
  analogue of prompt 01's "no shell scripts as the primary entrypoint" rule, and directly avoids
  the predecessor's need for hand-maintained `run.sh`/`setup_linux_deps.sh` at *build* time; do
  not reintroduce that class of fragility at *release* time instead).
- No system-package vendoring in the release artifacts or the repo, matching the constraint
  already established for the build itself in prompt 01 — if `cpal`/`wgpu`/`winit` require any
  runtime system library on a given OS, document the install requirement in user-facing setup
  docs rather than vendoring a shim; note that this project's stack was specifically chosen in
  `00-...md` to minimize this class of dependency versus the predecessor's WebKitGTK requirement.
- Generated API docs (`cargo doc --workspace --no-deps`) published (e.g. via GitHub Pages from
  CI) so the trait contracts defined in prompts 02–10 are browsable without reading source,
  important given how many crates depend on getting those contracts right.
- User-facing docs: top-level `README.md` (what it is, current per-system status — be explicit
  that, say, NDS support is early/partial if that's true at the time, mirroring the predecessor's
  good habit of an honest "Testing Status"/"Known Constraints" section), `SETUP.md` (one-command
  setup path per OS, matching prompt 01's `xtask` interface), and a docs section covering
  controls/keybind configuration, save-state/rewind usage, and debugger usage (prompt 15).
- Contributor-facing docs: `CONTRIBUTING.md` (extended from prompt 01's stub) covering the crate-
  boundary rule, the `Savable`-at-creation-time rule, how to run the accuracy suite locally, and
  how to add a new accuracy test ROM to the harness (prompt 17).

## Responsibilities

1. Extend `.github/workflows/ci.yml` from prompt 01 into the full matrix described above,
   including prompt 17's accuracy suite once it exists.
2. Add a release workflow (`.github/workflows/release.yml` or equivalent) using `cargo-dist`/
   `cargo-packager`, triggered on version tags.
3. Wire `cargo doc` publishing into CI.
4. Write/maintain `README.md`, `SETUP.md`, `CONTRIBUTING.md` for the new project, kept current as
   later prompts land (this is an ongoing responsibility, not a one-time deliverable — note this
   explicitly for whichever agent/contributor picks this prompt up).
5. A per-system status table (which systems boot, which accuracy suites pass, which known
   limitations exist) maintained in `README.md`, updated as prompts 11–13 progress — directly
   modeled on the predecessor's honest "Current Behavior Notes"/"Known Constraints" sections,
   which were a genuine strength worth keeping as a documentation pattern even though the
   underlying product changed completely.

## Interfaces

N/A — this prompt is tooling/process/documentation, not library code.

## Constraints

- Release artifacts must be reproducible from a clean checkout via CI alone — no manually-
  assembled release step.
- Documentation must not overclaim completeness, especially for NDS (prompt 13's explicitly
  partial scope) — a false "fully supported" claim is worse than an accurate "early/partial"
  one.

## Deliverables

- Mature CI workflow covering lint/test/accuracy-suite across three OSes.
- Working tag-triggered release automation producing installable artifacts for Linux/macOS/
  Windows.
- Published generated API docs.
- `README.md`, `SETUP.md`, `CONTRIBUTING.md` for the new project, accurate as of whatever prompts
  are complete at the time this is executed.

## Acceptance Criteria

- A test release tag produces working, installable artifacts on all three target OSes via CI
  alone, verified by actually installing and running at least one artifact.
- CI fails correctly on a deliberately introduced lint violation, test failure, and accuracy-
  suite regression (verify all three failure modes are actually caught, not just configured).
- A fresh contributor, following only `SETUP.md` and `CONTRIBUTING.md`, can get from clone to a
  running dev build and a passing local test run without needing undocumented tribal knowledge.

## Testing Requirements

- This prompt's "testing" is largely process verification: confirm each CI job actually fails
  when it should (the three failure-mode checks above) rather than just existing as YAML that's
  never been proven to catch anything.

## Future Compatibility

As new systems or major features (dynarec backend from prompt 18, GDB-remote debugger from
prompt 15) land, this prompt's documentation responsibilities extend to cover them — treat
`README.md`'s per-system status table and `CONTRIBUTING.md` as living documents, not one-time
deliverables closed out when this prompt is first executed.

## Notes

The predecessor's documentation practice (explicit, current, unafraid to say "not implemented
yet") is worth explicitly praising and continuing here even though everything else about the
predecessor's engineering is being deliberately improved upon — good documentation hygiene was
never the problem; the problem was what the documentation was honestly describing.
