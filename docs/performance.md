# Performance

Measured on an Apple M3, `bench` profile. Full numbers, the per-frame apportionment, and the
dynamic-recompilation decision live in `testing/harness/benches/systems.rs` — beside the benchmarks
that produced them rather than in prose that can drift from them.

| workload | frame time | speed |
|---|---|---|
| Game Boy, rendering | 246 µs | 68x |
| Game Boy, rendering + four APU channels | 361 µs | 46x |
| Game Boy Advance, ARM instructions straight from ROM | 1 372 µs | 12.2x |
| Game Boy Advance, **a commercial game** (measured outside the bench suite) | ≈3 050 µs | ≈5x |
| dmg-acid2 / cgb-acid2 | 243 / 258 µs | 69x / 65x |
| Nintendo DS, both cores spinning, displays off | 5 161 µs | 3.2x |
| Nintendo DS, engine A reading a VRAM framebuffer | 5 276 µs | 3.2x |
| Nintendo DS, the same with the sound hardware wired in | ≈5 440 µs | ≈3.1x |
| Nintendo DS 3D rasteriser, three screen-filling quads with overdraw | ≈730 µs | — |

## Key findings

- **The APU costs more than the PPU on a Game Boy frame** — about a third of it against a sixth. Not
  what you would guess for a machine whose job is drawing a picture, and the first place to look if
  the Game Boy ever needs to be faster.

- **No dynamic recompiler, for either CPU core**, on the evidence rather than by preference. A dynarec
  replaces dispatch and nothing else, and the worst measured workload on each system already runs at
  46x and 12.2x real time — though a *real* GBA game is about 5x, and that is the figure to hold.

  The GBA figure was 11.3x until 2026-07-31 and that number was not real: the machine charged every
  memory access three to six times over, so the same benchmark ROM got through a quarter of the
  instructions it should have. The emulator was not fast, the emulated machine was slow, and a frame
  is a fixed number of cycles either way. A real game at 5x still clears the 4x fast-forward target,
  but this is the one system where more per-instruction work would change the answer.

- **The Nintendo DS has a fifth of the margin the GBA does**, and prompt 18 was right about it. At
  3.2x real time against the other systems' 11x to 80x, it is by a wide margin the tightest — and
  that is *without* the 3D core. The dynarec question stays open for it rather than being inherited
  from the two answers above; it needs re-asking once the 3D rasteriser exists, since that is the
  workload prompt 18 expects to be the real problem.

  The first measurement was 15.0 ms against a 16.71 ms budget — 1.1x, barely real time. The cause
  was not the two 2D engines: turning both displays off saved 1.5%. It was that `NdsBus` composed
  every halfword and word access out of byte accesses, so each instruction fetch cost four region
  decodes. Reading and writing RAM at its real width dropped a frame from 15.2 ms to 5.28 ms, a
  **65% reduction**, measured before and after with `cargo bench -p harness --bench systems`. That
  is the only optimisation in the project so far, and it had a measured problem behind it.

- **The debugger's watchpoint recorder is not free**: +1.7% of a Game Boy frame, +4.5% of a GBA one,
  and +3.7% of a DS one, even disarmed, because it is a branch on every bus access. That fails the
  "zero measurable overhead" constraint it was written against, and it is kept anyway — a Cargo
  feature would either leave the shipped build paying it or leave the shipped build without
  watchpoints. Documented as a deliberate deviation with the number, not as compliance.

## Optimisation policy

Two things have been optimised, each with a profiled problem behind it and a before/after to show
for it — the DS bus composing wide accesses out of byte accesses (−65% of a frame), and the GBA's
per-instruction scheduler poll, which a `sample` profile of a real game put at **39% of a frame**
against 7% for all of rendering (−8.2% of `gba/spin`, hash unchanged). Nothing else has been:
every system meets its target with 5x to 80x of margin, and an optimisation with no problem behind
it is not worth its own risk.

**Benchmark figures here move by up to 2x with the machine's thermal state.** `gba/spin` measured
2 665 µs during a long working session and 1 372 µs on identical code cooled down. Make a
before/after claim from one `--baseline` run, never against a number written down on another day.

## Measuring performance

```sh
cargo xtask bench --quick --filter gb/       # quick look at Game Boy, fast warm-up
cargo xtask bench --save-baseline before     # save a baseline
cargo xtask bench --baseline before          # compare against saved baseline
```

Every optimisation must come with a before/after measurement using `--save-baseline` and
`--baseline`, never just a number written down on a different day.
