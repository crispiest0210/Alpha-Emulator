//! Whole-system frame-time benchmarks — the numbers prompt 18's per-system targets are stated in.
//!
//! # Why whole frames and not only microbenchmarks
//!
//! A microbenchmark of instruction dispatch tells you what dispatch costs. It does not tell you
//! whether dispatch *matters*, and prompt 18's central question — whether a dynamic recompiler is
//! justified — is entirely about the second thing. So the primary measurement here is `step_frame`
//! on a whole machine, with the microbenchmarks in the CPU, PPU, and APU crates existing to
//! apportion that total rather than to stand on their own.
//!
//! # The workloads
//!
//! Two kinds, and the distinction matters when reading the results:
//!
//! - **Synthetic**, built byte by byte in this file, always available. Each one deliberately keeps a
//!   specific subsystem busy: `lcd_only` runs a two-byte loop with the background off,
//!   `rendering` adds a full tilemap, `rendering_audio` adds four sounding APU channels. Comparing
//!   them apportions a frame between CPU, PPU, and APU without a profiler.
//! - **Corpus**, real test ROMs, registered only when they have been fetched. These are what the
//!   *target* is judged against, because a synthetic loop is not a game.
//!
//! No commercial ROM is used, here or anywhere in this workspace.
//!
//! # Reproducing
//!
//! ```sh
//! cargo xtask bench                    # everything
//! cargo xtask bench --filter gb_       # one group
//! cargo xtask profile --rom <path>     # a flamegraph of a real run
//! ```
//!
//! A frame's real-time budget is 16.74 ms on every system here. Divide it by a measured frame time
//! to get the speed multiple, which is the number the targets are written in — "full speed with
//! headroom" means comfortably above 1x, and fast-forward at 4x needs at least 4.
//!
//! # Findings
//!
//! Measured on an Apple M3 (2023 laptop class, 8 performance cores), `bench` profile — `lto = "thin"`,
//! one codegen unit. That is the "representative hardware" the targets below are judged against;
//! prompt 18 asks for it to be named rather than left implied, because "fast enough" without a stated
//! machine is not a checkable claim. A 2015-era desktop would be perhaps three times slower, which
//! every margin below still absorbs.
//!
//! | workload | frame time | speed |
//! |---|---|---|
//! | `gb/lcd_only` | 210 µs | 80x |
//! | `gb/rendering` | 246 µs | 68x |
//! | `gb/rendering_audio` | 361 µs | 46x |
//! | `corpus/dmg_acid2` | 243 µs | 69x |
//! | `corpus/cgb_acid2` | 258 µs | 65x |
//! | `gba/spin` | 2 665 µs | 6.3x |
//! | `corpus/gba_suite_arm` | 269 µs | 62x |
//!
//! **Where a Game Boy frame goes.** Baseline 210 µs; tile fetch and compositing add 36 µs (+17%);
//! four sounding APU channels add 115 µs (+47%). So on a machine with music playing, **the APU costs
//! more than the PPU** — about a third of the frame against a sixth. That is not what intuition
//! suggests for a machine whose whole job is to draw a picture, and it is the first place to look if
//! the Game Boy ever needs to be faster.
//!
//! **The GBA's two numbers differ because they are different workloads**, not because one is wrong.
//! `gba/spin` executes ARM instructions back to back out of cartridge ROM, paying a wait state on
//! every fetch; `gba_suite_arm` spends most of its time in tighter loops. The pessimistic one is the
//! one the target should be judged against, and it is the 6.3x.
//!
//! **`gba/spin` was 1 486 µs and 11.3x until 2026-07-31, and that number was not real.** The machine
//! was charging every memory access three to six times over, so a frame only got through about 7 900
//! instructions where it should get through 32 855. The emulator was not fast; the emulated machine
//! was slow, and a frame is a fixed number of cycles either way. Fixing the accounting made the same
//! ROM 4.2x more work per frame. Read any GBA number recorded before that date as measuring a
//! different workload, not a faster emulator.
//!
//! ## Dynamic recompilation: no, for both cores
//!
//! Prompt 18 asks for this decision to be made on data and recorded whichever way it goes.
//!
//! A dynarec replaces instruction *dispatch* and nothing else — not memory access, not the PPU, not
//! the APU. So the most it can win is bounded by what dispatch costs, and the question is whether the
//! result would be visible.
//!
//! - **SM83: no.** The worst measured Game Boy workload runs at 46x real time. Even eliminating
//!   dispatch entirely — impossible — could not turn 46x into anything a player notices, and the
//!   apportionment above says the APU would become the bottleneck long before dispatch did.
//! - **ARM7TDMI: no, and by a smaller margin than this file used to claim.** The pessimistic,
//!   dispatch-bound `gba/spin` workload runs at 6.3x, not the 11.3x recorded before the cycle
//!   accounting was fixed — the old figure was a machine executing a quarter of the instructions it
//!   should have. Prompt 18's stated GBA target is full speed *including* 2x and 4x fast-forward, and
//!   6.3x still clears 4x. The answer does not change, but the headroom behind it is half what it
//!   was, so this is worth re-asking if the GBA ever gains work per frame.
//! - **NDS: deferred, with nothing to measure.** Prompt 18 expects this to be the case that actually
//!   needs help — two CPUs and a software 3D rasteriser — and prompt 13 has not been started. The
//!   decision is not "no", it is "not yet measurable", and it must not be inherited from the two
//!   above.
//!
//! ## The watchpoint recorder is not free, and is kept anyway
//!
//! Prompt 15 asks for "zero measurable overhead in the default case" from the debugger hooks, and says
//! that if it cannot be demonstrated the design needs revisiting. It cannot be demonstrated. Measured
//! by deleting the `record` calls from each bus and re-running against a saved baseline:
//!
//! | system | cost of the disarmed recorder |
//! |---|---|
//! | Game Boy (`gb/rendering`) | +1.7% frame time |
//! | GBA (`gba/spin`) | +4.5% frame time |
//!
//! Revisiting it, as instructed, gives no better option:
//!
//! - A **Cargo feature** does not help. On by default, every shipped build still pays it; off by
//!   default, the shipped emulator has no watchpoints. Either way the configuration people actually
//!   run is unchanged, and the repository gains a second one to keep compiling.
//! - A **null-object trait hook** is worse: a pointer load and an indirect call per access instead of
//!   a bool test and a branch.
//! - **Checking in the CPU** cannot work — the CPU does not know which addresses are watched, and
//!   teaching it would put the debugger inside the hot trait `core-common` keeps minimal.
//!
//! So the cost is accepted deliberately: 4.5% of a 6.3x margin, against watchpoints working in the
//! build people actually have. Recorded here as a **deviation from prompt 15's constraint**, with the
//! number, rather than reported as compliance. Reproduce it by commenting out the two `record` calls
//! in `system-gb`'s `read8`/`write8` (six in `system-gba`, which records its halfword I/O paths
//! separately) and running `cargo xtask bench --quick --save-baseline norecorder --filter gb/rendering`
//! before restoring them and re-running with `--baseline norecorder`.
//!
//! With a watchpoint actually set, instruction stepping costs +0.8 µs per 1 024 instructions over
//! stepping without one — under a nanosecond per instruction, which is the price of a debugging
//! session and not of playing a game.
//!
//! ## Nothing was optimised
//!
//! Deliberately. Prompt 18 requires every optimisation to be justified by a before/after benchmark,
//! and the corollary is that an optimisation with no problem behind it should not be written. Every
//! system meets its target with between 3x and 80x of margin. The workflow is here so that the first
//! change that *does* need justifying can be justified — and so the APU finding above is on record for
//! whoever eventually needs the Game Boy to be faster.

use core_common::{InputState, System};
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use std::path::PathBuf;

/// One frame's real-time budget, shared by every system in the workspace (59.7275 Hz).
const FRAME_BUDGET_NANOS: u64 = 16_742_706;

// --- synthetic cartridges ---------------------------------------------------------------------

/// Finish a Game Boy header so the loader accepts the ROM.
fn finish_header(rom: &mut [u8], title: &[u8]) {
    let len = title.len().min(11);
    rom[0x0134..0x0134 + len].copy_from_slice(&title[..len]);
    rom[0x0147] = 0x00; // ROM only
    rom[0x0148] = 0x00; // 32 KiB, matching the vector's length
    let mut checksum = 0u8;
    for byte in &rom[0x0134..0x014D] {
        checksum = checksum.wrapping_sub(*byte).wrapping_sub(1);
    }
    rom[0x014D] = checksum;
}

/// A ROM that spins with the LCD on but the background off.
///
/// The floor: the PPU still runs its mode machine and reaches VBlank, but composites nothing. The
/// difference from `gb_rendering` is therefore tile fetch and compositing.
///
/// **Not** the LCD switched off, which was the first version of this and measured the wrong thing
/// entirely. With the LCD off the PPU never reaches VBlank, so `step_frame` runs to its safety bound
/// instead of to a frame — and this "idle" case benchmarked *slower* than the rendering one, which is
/// how the mistake announced itself.
fn gb_lcd_only_rom() -> Vec<u8> {
    let mut rom = vec![0u8; 0x8000];
    let code: &[u8] = &[
        0x3E, 0x80, //  ld a, $80      ; LCD on, background off
        0xE0, 0x40, //  ldh ($40), a
        0x18, 0xFE, //  jr -2
    ];
    rom[0x0100..0x0100 + code.len()].copy_from_slice(code);
    finish_header(&mut rom, b"BENCHLCD");
    rom
}

/// A ROM that renders: a full tilemap of an opaque tile, LCD and background on.
///
/// Every scanline now fetches tiles and composites, so the difference from `gb_idle` is the PPU's
/// share of a frame.
fn gb_rendering_rom() -> Vec<u8> {
    let mut rom = vec![0u8; 0x8000];
    let code: &[u8] = &[
        0xAF, //             xor a
        0xE0, 0x40, //       ldh ($40), a      ; LCD off while VRAM is written
        0x3E, 0xE4, //       ld a, $E4
        0xE0, 0x47, //       ldh ($47), a      ; BGP
        0x21, 0x10, 0x80, // ld hl, $8010      ; tile 1
        0x06, 0x10, //       ld b, 16
        0x3E, 0xFF, //       ld a, $FF
        0x22, //        fill: ld (hl+), a
        0x05, //             dec b
        0x20, 0xFC, //       jr nz, fill
        0x21, 0x00, 0x98, // ld hl, $9800      ; tilemap
        0x01, 0x00, 0x04, // ld bc, $0400
        0x3E, 0x01, //   map: ld a, 1
        0x22, //             ld (hl+), a
        0x0B, //             dec bc
        0x78, //             ld a, b
        0xB1, //             or c
        0x20, 0xF8, //       jr nz, map
        0x3E, 0x91, //       ld a, $91         ; LCD on, BG on, tiles at $8000
        0xE0, 0x40, //       ldh ($40), a
        0x18, 0xFE, //       jr -2
    ];
    rom[0x0100..0x0100 + code.len()].copy_from_slice(code);
    finish_header(&mut rom, b"BENCHDRAW");
    rom
}

/// Rendering, plus all four APU channels sounding.
///
/// The difference from `gb_rendering` is the APU's share. Channels are started before the spin loop
/// and left running, which is what a game with music does.
fn gb_rendering_audio_rom() -> Vec<u8> {
    let mut rom = gb_rendering_rom();
    // Overwrite the final `jr -2` with APU setup followed by a new spin.
    let spin = rom[0x0100..0x0140]
        .windows(2)
        .position(|w| w == [0x18, 0xFE])
        .expect("the rendering ROM ends in a spin")
        + 0x0100;
    let apu: &[u8] = &[
        0x3E, 0x80, //  ld a, $80
        0xE0, 0x26, //  ldh ($26), a   ; NR52: APU on
        0x3E, 0xFF, //  ld a, $FF
        0xE0, 0x25, //  ldh ($25), a   ; NR51: everything to both outputs
        0x3E, 0x77, //  ld a, $77
        0xE0, 0x24, //  ldh ($24), a   ; NR50: full volume
        0x3E, 0xF0, //  ld a, $F0
        0xE0, 0x12, //  ldh ($12), a   ; NR12: channel 1 envelope
        0xE0, 0x17, //  ldh ($17), a   ; NR22: channel 2 envelope
        0xE0, 0x21, //  ldh ($21), a   ; NR42: channel 4 envelope
        0x3E, 0x80, //  ld a, $80
        0xE0, 0x1A, //  ldh ($1A), a   ; NR30: channel 3 on
        0x3E, 0x87, //  ld a, $87
        0xE0, 0x14, //  ldh ($14), a   ; NR14: trigger channel 1
        0xE0, 0x19, //  ldh ($19), a   ; NR24: trigger channel 2
        0xE0, 0x1E, //  ldh ($1E), a   ; NR34: trigger channel 3
        0xE0, 0x23, //  ldh ($23), a   ; NR44: trigger channel 4
        0x18, 0xFE, //  jr -2
    ];
    rom[spin..spin + apu.len()].copy_from_slice(apu);
    finish_header(&mut rom, b"BENCHSOUND");
    rom
}

/// A GBA ROM of `mov r0, #0` repeated: ARM dispatch with the video timing running.
///
/// The GBA's PPU work happens whether or not the program sets anything up, so this is not an "idle"
/// case in the way the Game Boy's is — it is the baseline cost of a GBA frame.
///
/// # Why it ends in a branch
///
/// It did not, and for as long as the machine over-charged every memory access that did not show:
/// a frame only got through about 7 900 instructions, so the program counter never reached the end
/// of the 8 192 here. Once cycle accounting was fixed a frame ran 32 855 instructions, the run fell
/// off the end of the cartridge, and 94% of what the benchmark timed was the CPU grinding through
/// unmapped memory at one cycle an instruction. The number that produced was four times too slow
/// and measured nothing anyone cares about. The branch keeps the workload inside the cartridge,
/// which is the whole point of this case.
fn gba_spin_rom() -> Vec<u8> {
    let mut rom = Vec::with_capacity(0x8000);
    for _ in 0..0x1FFF {
        rom.extend_from_slice(&0xE3A0_0000u32.to_le_bytes());
    }
    // `b -0x8000`, back to the first instruction. One non-sequential fetch every 8 191 sequential
    // ones, so the case stays what it says it is: back-to-back sequential cartridge fetches.
    rom.extend_from_slice(&0xEAFF_DFFEu32.to_le_bytes());
    rom
}

/// A DS cartridge whose ARM9 half fills a VRAM bank through display mode 2 and then spins, and
/// whose ARM7 half spins.
///
/// Display mode 2 is the shortest path to a screen the 2D engine actually reads every line, so
/// this measures the compositor rather than an idle machine — the DS equivalent of the Game Boy's
/// `rendering` case rather than of its `lcd_only` one.
fn nds_rendering_rom() -> Vec<u8> {
    nds_rom(true)
}

/// The same machine with both displays off, so the difference between the two is what the 2D
/// compositor costs.
fn nds_idle_rom() -> Vec<u8> {
    nds_rom(false)
}

fn nds_rom(display: bool) -> Vec<u8> {
    let mut rom = vec![0u8; 0x8000];
    rom[..12].copy_from_slice(b"BENCH DS\0\0\0\0");
    let put = |rom: &mut Vec<u8>, at: usize, v: u32| {
        rom[at..at + 4].copy_from_slice(&v.to_le_bytes());
    };
    put(&mut rom, 0x20, 0x4000);
    put(&mut rom, 0x24, 0x0200_0000);
    put(&mut rom, 0x28, 0x0200_0000);
    put(&mut rom, 0x30, 0x6000);
    put(&mut rom, 0x34, 0x0380_0000);
    put(&mut rom, 0x38, 0x0380_0000);
    put(&mut rom, 0x3C, 4);

    // mov r0,#0x80 / mov r1,#0x04000240 (built up) / strb — VRAMCNT_A into the LCDC window.
    // Then DISPCNT = display mode 2, then spin.
    let mode = if display { 0xE3A0_0802 } else { 0xE3A0_0000 };
    let program: [u32; 12] = [
        0xE3A0_0080, // mov r0, #0x80
        0xE3A0_1040, // mov r1, #0x40
        0xE381_1C02, // orr r1, r1, #0x200
        0xE381_1301, // orr r1, r1, #0x04000000
        0xE5C1_0000, // strb r0, [r1]
        mode,        // mov r0, #0x00020000 (display mode 2) or #0 (display off)
        0xE3A0_1301, // mov r1, #0x04000000
        0xE581_0000, // str r0, [r1]
        0xEAFF_FFFE, // b .
        0xEAFF_FFFE,
        0xEAFF_FFFE,
        0xEAFF_FFFE,
    ];
    for (i, word) in program.iter().enumerate() {
        put(&mut rom, 0x4000 + i * 4, *word);
    }
    put(&mut rom, 0x2C, (program.len() * 4) as u32);
    put(&mut rom, 0x6000, 0xEAFF_FFFE);
    rom
}

// --- the benchmarks --------------------------------------------------------------------------

/// Run `frames` frames, draining audio as a real frontend does.
///
/// Draining matters: the sample buffer is bounded, and a benchmark that never drained would measure
/// a path no frontend takes and would slowly stop producing audio work.
fn run_frames(system: &mut dyn System, frames: u32) {
    for _ in 0..frames {
        system.step_frame(InputState::default());
        std::hint::black_box(system.take_audio_samples());
    }
    std::hint::black_box(system.framebuffer());
}

fn bench_frame(
    group: &mut criterion::BenchmarkGroup<'_, criterion::measurement::WallTime>,
    name: &str,
    mut build: impl FnMut() -> Box<dyn System>,
) {
    // One frame per iteration, so the reported time *is* the frame time and can be read straight
    // against the 16.74 ms budget without arithmetic.
    group.throughput(Throughput::Elements(1));
    group.bench_function(BenchmarkId::new(name, "frame"), |b| {
        let mut system = build();
        // Warm the machine past its boot sequence, which is not representative of steady state.
        run_frames(system.as_mut(), 8);
        b.iter(|| run_frames(system.as_mut(), 1));
    });
}

fn gb_frames(c: &mut Criterion) {
    let mut group = c.benchmark_group("gb");
    for (name, rom) in [
        ("lcd_only", gb_lcd_only_rom()),
        ("rendering", gb_rendering_rom()),
        ("rendering_audio", gb_rendering_audio_rom()),
    ] {
        bench_frame(&mut group, name, || {
            Box::new(system_gb::GbSystem::new(rom.clone(), None).expect("a hand-built cartridge"))
        });
    }
    group.finish();
}

fn gba_frames(c: &mut Criterion) {
    let mut group = c.benchmark_group("gba");
    let rom = gba_spin_rom();
    bench_frame(&mut group, "spin", || {
        Box::new(system_gba::GbaSystem::new(rom.clone(), None).expect("a hand-built cartridge"))
    });
    group.finish();
}

/// The Nintendo DS.
///
/// Prompt 18 expects this to be the first system in the project that actually needs optimising,
/// and says the dynarec decision for it must be measured rather than inherited from the two
/// already made. This is that measurement's starting point — with no 3D core yet, so it is the
/// floor rather than the answer.
///
/// **Read these against `gba/spin` from the same run, not against the numbers in `README.md`.**
/// This is the one case here whose absolute time is close enough to its budget that a laptop
/// under sustained load can move it by 70%, and it has already caused one "regression" that was
/// nothing of the kind — the untouched GBA case had moved by the same proportion.
fn nds_frames(c: &mut Criterion) {
    let mut group = c.benchmark_group("nds");
    for (name, rom) in [("idle", nds_idle_rom()), ("rendering", nds_rendering_rom())] {
        bench_frame(&mut group, name, move || {
            let mut nds = system_nds::NdsSystem::default();
            nds.load_cartridge(&rom).expect("a hand-built cartridge");
            Box::new(nds)
        });
    }
    group.finish();
}

/// The DS's 3D rasteriser, measured directly rather than through a ROM.
///
/// Prompt 18 expects this to be the thing that actually needs optimising, so it is worth a number
/// that is not confounded by the two CPUs. It is measured directly for a second reason: a display
/// list is *pushed* by software at whatever rate software chooses, and a ROM that feeds one in a
/// tight loop fills polygon RAM every frame and produces an unbounded benchmark rather than a
/// representative one. Driving the engine from here fixes the geometry at a known amount.
///
/// The scene is three screen-filling quads, the front one translucent: enough overdraw to
/// exercise the depth test and the blend path rather than only the setup.
fn nds_rasteriser(c: &mut Criterion) {
    use system_nds::gpu3d::geometry::Geometry;
    use system_nds::gpu3d::render::{render, Framebuffer3d};

    let vertex = |x: f32, y: f32, z: f32| {
        let f = |v: f32| ((v * 4096.0) as i32 as u32) & 0xFFFF;
        [f(x) | (f(y) << 16), f(z)]
    };
    let mut geometry = Geometry::new();
    geometry.execute(0x60, &[(255 << 16) | (191 << 24)]); // VIEWPORT
    let attr = (1u32 << 6) | (1 << 7);
    for (index, (z, color, alpha)) in [
        (0.5f32, 0x001Fu32, 0u32),
        (0.0, 0x03E0, 0),
        (-0.5, 0x7C00, 15),
    ]
    .into_iter()
    .enumerate()
    {
        geometry.execute(0x29, &[attr | (alpha << 16) | ((index as u32) << 24)]);
        geometry.execute(0x20, &[color]);
        geometry.execute(0x40, &[1]); // BEGIN_VTXS quads
        for (x, y) in [(-0.9f32, -0.9f32), (0.9, -0.9), (0.9, 0.9), (-0.9, 0.9)] {
            geometry.execute(0x23, &vertex(x, y, z));
        }
    }
    geometry.execute(0x50, &[0]); // SWAP_BUFFERS
    let list = geometry.take_display_list();
    let vram = system_nds::Vram::new();

    let mut group = c.benchmark_group("nds");
    group.throughput(Throughput::Elements(1));
    group.bench_function(BenchmarkId::new("rasteriser", "frame"), |b| {
        let mut out = Framebuffer3d::new();
        b.iter(|| {
            render(&list, &vram, 0, 0x7FFF, &mut out);
            std::hint::black_box(&out.color);
        });
    });
    group.finish();
}

/// The cost of the watchpoint recorder that sits on every bus access.
///
/// Prompt 15 left this as an explicit claim to check rather than assert: `AccessLog::record` returns
/// immediately while disarmed, but it is still a load and a branch on *every memory access in the
/// emulator*. Three measurements, so the claim is decidable rather than plausible:
///
/// - `disarmed/frame` — the ordinary case, what every player pays.
/// - `stepping_disarmed` and `stepping_armed` — instruction stepping with and without a watchpoint,
///   draining after each instruction as the session does. Their difference is what recording costs.
///
/// The same ROM throughout, so nothing but the recorder differs. What this cannot measure is
/// `disarmed` against a build with no recorder compiled in at all; for that, comment out the two
/// `record` calls in `system-gb`'s `read8`/`write8` and compare. That is deliberately manual — a
/// feature flag to make it automatic would be a second configuration to keep correct forever.
fn watchpoint_overhead(c: &mut Criterion) {
    let mut group = c.benchmark_group("watch_overhead");
    group.throughput(Throughput::Elements(1));
    let rom = gb_rendering_rom();

    // Ordinary play: whole frames, recorder disarmed. This is the number every player pays.
    group.bench_function(BenchmarkId::new("disarmed", "frame"), |b| {
        let mut system =
            system_gb::GbSystem::new(rom.clone(), None).expect("a hand-built cartridge");
        run_frames(&mut system, 8);
        b.iter(|| run_frames(&mut system, 1));
    });

    // A debugging session with a watchpoint set: instruction stepping with a drain after each one,
    // which is exactly what `frontend-core`'s loop does.
    //
    // Draining per instruction rather than per frame is the whole point. The first version of this
    // drained once per frame, so the log was full for all but the first 128 accesses and the
    // benchmark measured how quickly a full log *rejects* entries — which came out identical to
    // disarmed and looked like a wonderful result.
    group.throughput(Throughput::Elements(1024));
    for (name, armed) in [("stepping_disarmed", false), ("stepping_armed", true)] {
        group.bench_function(BenchmarkId::new(name, "1024_instructions"), |b| {
            let mut system =
                system_gb::GbSystem::new(rom.clone(), None).expect("a hand-built cartridge");
            run_frames(&mut system, 8);
            if let Some(log) = system.access_log() {
                log.set_armed(armed);
            }
            b.iter(|| {
                for _ in 0..1024 {
                    std::hint::black_box(system.step_instruction());
                    if let Some(log) = system.access_log() {
                        std::hint::black_box(log.drain().count());
                    }
                }
            });
        });
    }
    group.finish();
}

/// Corpus ROMs, when they have been fetched.
///
/// Real code, and so the workload the per-system targets are actually judged against. Registered
/// conditionally rather than failing: a contributor without the corpus still gets every synthetic
/// benchmark, and `cargo xtask fetch-test-roms` adds these.
fn corpus_frames(c: &mut Criterion) {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("testing/harness has a parent")
        .join("test-roms");

    let candidates: &[(&str, &str)] = &[
        ("dmg_acid2", "gb/dmg-acid2.gb"),
        ("cgb_acid2", "gbc/cgb-acid2.gbc"),
        ("gba_suite_arm", "gba/gba-suite/arm.gba"),
    ];

    let mut group = c.benchmark_group("corpus");
    for (name, relative) in candidates {
        let path = root.join(relative);
        let Ok(bytes) = std::fs::read(&path) else {
            eprintln!("skipping {name}: {} is not fetched", path.display());
            continue;
        };
        let is_gba = relative.ends_with(".gba");
        let is_cgb = relative.ends_with(".gbc");
        bench_frame(&mut group, name, move || {
            let bytes = bytes.clone();
            if is_gba {
                Box::new(system_gba::GbaSystem::new(bytes, None).expect("the ROM parses"))
            } else if is_cgb {
                Box::new(system_gbc::GbcSystem::new(bytes, None).expect("the ROM parses"))
            } else {
                Box::new(system_gb::GbSystem::new(bytes, None).expect("the ROM parses"))
            }
        });
    }
    group.finish();
}

/// Per-instruction cost, for apportioning a frame between dispatch and everything else.
///
/// This is the number prompt 18's dynarec decision turns on: a dynamic recompiler replaces *dispatch*
/// and nothing else, so if dispatch is a small fraction of a frame then a JIT cannot help however
/// well it is written.
fn instruction_dispatch(c: &mut Criterion) {
    let mut group = c.benchmark_group("dispatch");
    group.throughput(Throughput::Elements(1024));

    let gb_rom = gb_rendering_rom();
    group.bench_function("sm83", |b| {
        let mut system = system_gb::GbSystem::new(gb_rom.clone(), None).expect("cartridge");
        run_frames(&mut system, 8);
        b.iter(|| {
            for _ in 0..1024 {
                std::hint::black_box(system.step_instruction());
            }
        });
    });

    let gba_rom = gba_spin_rom();
    group.bench_function("arm7tdmi", |b| {
        let mut system = system_gba::GbaSystem::new(gba_rom.clone(), None).expect("cartridge");
        run_frames(&mut system, 8);
        b.iter(|| {
            for _ in 0..1024 {
                std::hint::black_box(system.step_instruction());
            }
        });
    });
    group.finish();
}

/// A frame's real-time budget, printed once so a reader has the divisor to hand.
fn budget(c: &mut Criterion) {
    let mut group = c.benchmark_group("reference");
    group.bench_function("frame_budget_16.74ms", |b| {
        b.iter(|| std::hint::black_box(FRAME_BUDGET_NANOS));
    });
    group.finish();
}

criterion_group!(
    benches,
    gb_frames,
    gba_frames,
    nds_frames,
    nds_rasteriser,
    corpus_frames,
    watchpoint_overhead,
    instruction_dispatch,
    budget
);
criterion_main!(benches);
