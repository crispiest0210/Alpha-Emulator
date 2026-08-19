//! `cargo xtask <task>` — the single, OS-uniform developer entrypoint.
//!
//! Deliberately a Rust program rather than a shell script: the predecessor project needed
//! divergent `npm run dev` / `./run.sh dev` paths and a bespoke `setup_linux_deps.sh` that
//! vendored `.pc` files into the repo. Nothing here shells out to a platform shell, and
//! `setup` never downloads or vendors a binary — it only *reports* what to install.

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use std::path::Path;
use std::process::Command;

#[derive(Parser)]
#[command(name = "xtask", about = "Alpha Emulator developer automation", version)]
struct Cli {
    #[command(subcommand)]
    task: Task,
}

#[derive(Subcommand)]
enum Task {
    /// Check the host for required toolchain and system packages.
    Setup,
    /// Run the native frontend.
    Dev {
        /// Run the optimized build instead of the dev profile.
        #[arg(long)]
        release: bool,
        /// Extra arguments forwarded to the frontend binary.
        #[arg(trailing_var_arg = true)]
        args: Vec<String>,
    },
    /// Build the whole workspace.
    Build {
        #[arg(long)]
        release: bool,
    },
    /// Run the workspace test suite.
    Test {
        /// Also run the accuracy test-ROM suite (prompt 17).
        #[arg(long)]
        accuracy: bool,
    },
    /// Run benchmarks.
    Bench {
        /// Only benchmarks whose name matches this. Criterion treats it as a regex.
        #[arg(long)]
        filter: Option<String>,
        /// Record these results as the baseline that later runs compare against.
        #[arg(long, value_name = "NAME")]
        save_baseline: Option<String>,
        /// Compare against a previously saved baseline.
        #[arg(long, value_name = "NAME")]
        baseline: Option<String>,
        /// Shorter warm-up and measurement, for a quick look rather than a number to quote.
        #[arg(long)]
        quick: bool,
    },
    /// Profile a real run and say how to turn it into a flamegraph.
    Profile {
        /// The ROM to run.
        rom: std::path::PathBuf,
        #[arg(long, default_value_t = 1800)]
        frames: u64,
    },
    /// Download the accuracy test-ROM corpus.
    FetchTestRoms {
        /// Re-download ROMs that are already present.
        #[arg(long)]
        force: bool,
    },
    /// Run rustfmt and clippy exactly as CI does.
    Lint {
        /// Apply fixes instead of only checking.
        #[arg(long)]
        fix: bool,
    },
}

fn main() -> Result<()> {
    match Cli::parse().task {
        Task::Setup => setup(),
        Task::Dev { release, args } => dev(release, &args),
        Task::Build { release } => build(release),
        Task::Test { accuracy } => test(accuracy),
        Task::Bench {
            filter,
            save_baseline,
            baseline,
            quick,
        } => bench(filter, save_baseline, baseline, quick),
        Task::Profile { rom, frames } => profile(&rom, frames),
        Task::FetchTestRoms { force } => fetch_test_roms(force),
        Task::Lint { fix } => lint(fix),
    }
}

/// Run a command, inheriting stdio, and fail if it exits non-zero.
fn run(program: &str, args: &[&str]) -> Result<()> {
    eprintln!("+ {program} {}", args.join(" "));
    let status = Command::new(program)
        .args(args)
        .status()
        .with_context(|| format!("failed to spawn `{program}`"))?;
    if !status.success() {
        bail!("`{program} {}` failed with {status}", args.join(" "));
    }
    Ok(())
}

fn cargo() -> String {
    std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string())
}

fn have(program: &str) -> bool {
    have_probe(program, "--version")
}

/// Whether `program` is on `PATH`, asked with the flag *it* answers to.
///
/// The flag is a parameter because tools disagree about it: `unzip --version` prints its usage
/// and exits non-zero, so probing every program the same way would report a perfectly good
/// `unzip` as missing.
fn have_probe(program: &str, arg: &str) -> bool {
    Command::new(program)
        .arg(arg)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// setup
// ---------------------------------------------------------------------------

/// A host requirement that cannot be satisfied by cargo alone.
struct Requirement {
    what: &'static str,
    ok: bool,
    /// Per-package-manager install hints, only shown when `ok` is false.
    hints: &'static [(&'static str, &'static str)],
}

fn setup() -> Result<()> {
    println!("Checking host environment for Alpha Emulator development...\n");

    let mut reqs: Vec<Requirement> = Vec::new();

    reqs.push(Requirement {
        what: "Rust toolchain (cargo)",
        ok: have("cargo"),
        hints: &[("all", "install rustup from https://rustup.rs")],
    });

    // cpal needs ALSA development headers on Linux; wgpu wants a Vulkan loader.
    // These are the only genuinely unavoidable native dependencies in the stack.
    if cfg!(target_os = "linux") {
        reqs.push(Requirement {
            what: "ALSA development headers (cpal)",
            ok: pkg_config_has("alsa"),
            hints: &[
                ("apt", "sudo apt install libasound2-dev pkg-config"),
                ("dnf", "sudo dnf install alsa-lib-devel pkgconf-pkg-config"),
                ("pacman", "sudo pacman -S alsa-lib pkgconf"),
            ],
        });
        reqs.push(Requirement {
            what: "X11/Wayland client headers (winit)",
            ok: pkg_config_has("x11") || pkg_config_has("wayland-client"),
            hints: &[
                (
                    "apt",
                    "sudo apt install libx11-dev libxkbcommon-dev libwayland-dev",
                ),
                (
                    "dnf",
                    "sudo dnf install libX11-devel libxkbcommon-devel wayland-devel",
                ),
                ("pacman", "sudo pacman -S libx11 libxkbcommon wayland"),
            ],
        });
    }

    let mut missing = 0usize;
    for r in &reqs {
        if r.ok {
            println!("  ok      {}", r.what);
        } else {
            missing += 1;
            println!("  MISSING {}", r.what);
            for (pm, cmd) in r.hints {
                println!("            {pm:>7}: {cmd}");
            }
        }
    }

    // Optional cargo subcommands used by CI (prompt 19) and the boundary lint.
    // These are advisory: their absence does not fail setup, but the exact
    // install command is printed so prompt 19 does not discover it cold.
    println!("\nOptional developer tooling:");
    for (bin, install) in [
        ("cargo-deny", "cargo install cargo-deny --locked"),
        ("cargo-dist", "cargo install cargo-dist --locked"),
        ("cargo-nextest", "cargo install cargo-nextest --locked"),
    ] {
        if have(bin) {
            println!("  ok      {bin}");
        } else {
            println!("  absent  {bin}  ->  {install}");
        }
    }

    if missing > 0 {
        println!();
        bail!(
            "{missing} required system package(s) missing. Install them with the command above \
             and re-run `cargo xtask setup`. Nothing will be vendored into this repository."
        );
    }

    println!("\nAll required dependencies present. Next: `cargo xtask dev`.");
    Ok(())
}

/// Ask `pkg-config` whether a library is present. Absent `pkg-config` counts as "not found",
/// which is correct: on Linux the build itself needs it.
fn pkg_config_has(lib: &str) -> bool {
    Command::new("pkg-config")
        .args(["--exists", lib])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// build / run / test
// ---------------------------------------------------------------------------

fn dev(release: bool, extra: &[String]) -> Result<()> {
    let mut args = vec!["run", "-p", "frontend-native"];
    if release {
        args.push("--release");
    }
    if !extra.is_empty() {
        args.push("--");
        args.extend(extra.iter().map(String::as_str));
    }
    run(&cargo(), &args)
}

fn build(release: bool) -> Result<()> {
    let mut args = vec!["build", "--workspace", "--all-targets"];
    if release {
        args.push("--release");
    }
    run(&cargo(), &args)
}

fn test(accuracy: bool) -> Result<()> {
    run(&cargo(), &["test", "--workspace"])?;
    if accuracy {
        // The suite lives in `testing/harness` and skips any ROM that has not been fetched,
        // so this is safe to run on a fresh checkout.
        run(&cargo(), &["test", "-p", "harness", "--", "--nocapture"])?;
    }
    Ok(())
}

/// Run the criterion benchmarks.
///
/// Everything after `--` goes to criterion, which is why the flags are forwarded rather than
/// reimplemented: `--save-baseline` and `--baseline` are how a before/after claim is made, and prompt
/// 18 requires every optimisation to come with one.
fn bench(
    filter: Option<String>,
    save_baseline: Option<String>,
    baseline: Option<String>,
    quick: bool,
) -> Result<()> {
    let mut args: Vec<String> = vec!["bench".into(), "--workspace".into(), "--".into()];
    if quick {
        // Enough to see a change of a few percent, not enough to quote. The defaults take minutes.
        args.extend(["--warm-up-time".into(), "1".into()]);
        args.extend(["--measurement-time".into(), "3".into()]);
    }
    if let Some(name) = save_baseline {
        args.extend(["--save-baseline".into(), name]);
    }
    if let Some(name) = baseline {
        args.extend(["--baseline".into(), name]);
    }
    // Last, because criterion takes the filter as a positional argument.
    if let Some(filter) = filter {
        args.push(filter);
    }
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    run(&cargo(), &refs)
}

/// Run a ROM under the profiler, or explain how to.
///
/// Deliberately does not install anything. `cargo flamegraph` needs `cargo-flamegraph` and, on Linux,
/// `perf` — both of which are the user's to install, exactly as `cargo xtask setup` treats every other
/// system dependency. What this does is build the release binary and print the one command that
/// profiles it, so the command in the docs and the command that runs are the same command.
fn profile(rom: &std::path::Path, frames: u64) -> Result<()> {
    if !rom.is_file() {
        bail!("{} is not a file", rom.display());
    }
    run(&cargo(), &["build", "--release", "-p", "frontend-headless"])?;

    let binary = "target/release/frontend-headless";
    let frames = frames.to_string();
    let args = ["run", rom.to_str().unwrap_or_default(), "--frames", &frames];

    println!("\nProfiling target built. To capture a flamegraph:\n");
    if have("flamegraph") || have("cargo-flamegraph") {
        println!(
            "  cargo flamegraph --release -p frontend-headless -- {}",
            args.join(" ")
        );
    } else {
        println!("  cargo install flamegraph      # once");
        println!(
            "  cargo flamegraph --release -p frontend-headless -- {}",
            args.join(" ")
        );
    }
    #[cfg(target_os = "macos")]
    println!(
        "\nOr, without installing anything (macOS):\n  \
         sample $({binary} {} & echo $!) 5 -f /tmp/alpha.sample && cat /tmp/alpha.sample",
        args.join(" ")
    );
    #[cfg(target_os = "linux")]
    println!(
        "\nOr, with perf:\n  \
         perf record -g {binary} {} && perf report",
        args.join(" ")
    );

    println!("\nRunning it once now, for a wall-clock figure:\n");
    run(binary, &args)
}

// ---------------------------------------------------------------------------
// Test ROMs
// ---------------------------------------------------------------------------

/// Download the accuracy corpus into `testing/test-roms/`.
///
/// That directory is gitignored and nothing in it is ever committed. The predecessor project
/// checked test ROM binaries — and a commercial game ROM — into its repository; fetching is
/// the only path here so that cannot happen by habit.
///
/// Uses `curl`, which ships with macOS, every Linux distribution worth supporting, and Windows
/// 10 onward. Shelling out to it beats adding a TLS stack to the build for a step that runs
/// once per checkout.
fn fetch_test_roms(force: bool) -> Result<()> {
    if !have("curl") {
        bail!("curl is required to fetch test ROMs; install it and re-run");
    }

    let root = workspace_root()?;
    let corpus = root.join("testing").join("test-roms");

    let mut fetched = 0usize;
    let mut skipped = 0usize;
    let mut failed: Vec<String> = Vec::new();

    for (path, url) in TEST_ROMS {
        let target = corpus.join(path);
        if target.is_file() && !force {
            skipped += 1;
            continue;
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }

        println!("fetching {path}");
        let status = Command::new("curl")
            .args([
                "--location", // these are redirects to a CDN
                "--fail",     // a 404 must not leave an HTML error page on disk
                "--silent",
                "--show-error",
                "--max-time",
                "120",
                "--output",
            ])
            .arg(&target)
            .arg(url)
            .status()
            .with_context(|| format!("running curl for {path}"))?;

        if status.success() {
            fetched += 1;
        } else {
            // Leave nothing half-written behind: a truncated ROM would fail the suite in a
            // way that looks like an emulator bug.
            let _ = std::fs::remove_file(&target);
            failed.push((*path).to_string());
        }
    }

    let (archived, archive_skipped) = fetch_archives(&corpus, force, &mut failed)?;
    fetched += archived;
    skipped += archive_skipped;

    println!(
        "\n{fetched} fetched, {skipped} already present, {} failed",
        failed.len()
    );
    if !failed.is_empty() {
        for path in &failed {
            println!("  failed: {path}");
        }
        bail!("some test ROMs could not be fetched; the accuracy suite will skip them");
    }
    println!("Run `cargo xtask test --accuracy` to use them.");
    Ok(())
}

/// The corpus, mirroring `testing/harness`'s own list.
///
/// Duplicated rather than shared because `xtask` deliberately does not depend on the
/// workspace's crates — it must build and run even when they do not.
const TEST_ROMS: &[(&str, &str)] = &[
    (
        "gb/blargg/cpu_instrs.gb",
        "https://raw.githubusercontent.com/retrio/gb-test-roms/master/cpu_instrs/cpu_instrs.gb",
    ),
    (
        "gb/blargg/instr_timing.gb",
        "https://raw.githubusercontent.com/retrio/gb-test-roms/master/instr_timing/instr_timing.gb",
    ),
    (
        "gb/blargg/mem_timing.gb",
        "https://raw.githubusercontent.com/retrio/gb-test-roms/master/mem_timing/mem_timing.gb",
    ),
    (
        "gb/blargg/dmg_sound.gb",
        "https://raw.githubusercontent.com/retrio/gb-test-roms/master/dmg_sound/dmg_sound.gb",
    ),
    ("gb/blargg/cpu_instrs/01-special.gb", "https://raw.githubusercontent.com/retrio/gb-test-roms/master/cpu_instrs/individual/01-special.gb"),
    ("gb/blargg/cpu_instrs/02-interrupts.gb", "https://raw.githubusercontent.com/retrio/gb-test-roms/master/cpu_instrs/individual/02-interrupts.gb"),
    ("gb/blargg/cpu_instrs/03-op_sp_hl.gb", "https://raw.githubusercontent.com/retrio/gb-test-roms/master/cpu_instrs/individual/03-op%20sp,hl.gb"),
    ("gb/blargg/cpu_instrs/04-op_r_imm.gb", "https://raw.githubusercontent.com/retrio/gb-test-roms/master/cpu_instrs/individual/04-op%20r,imm.gb"),
    ("gb/blargg/cpu_instrs/05-op_rp.gb", "https://raw.githubusercontent.com/retrio/gb-test-roms/master/cpu_instrs/individual/05-op%20rp.gb"),
    ("gb/blargg/cpu_instrs/06-ld_r_r.gb", "https://raw.githubusercontent.com/retrio/gb-test-roms/master/cpu_instrs/individual/06-ld%20r,r.gb"),
    ("gb/blargg/cpu_instrs/07-jr_jp_call_ret_rst.gb", "https://raw.githubusercontent.com/retrio/gb-test-roms/master/cpu_instrs/individual/07-jr,jp,call,ret,rst.gb"),
    ("gb/blargg/cpu_instrs/08-misc_instrs.gb", "https://raw.githubusercontent.com/retrio/gb-test-roms/master/cpu_instrs/individual/08-misc%20instrs.gb"),
    ("gb/blargg/cpu_instrs/09-op_r_r.gb", "https://raw.githubusercontent.com/retrio/gb-test-roms/master/cpu_instrs/individual/09-op%20r,r.gb"),
    ("gb/blargg/cpu_instrs/10-bit_ops.gb", "https://raw.githubusercontent.com/retrio/gb-test-roms/master/cpu_instrs/individual/10-bit%20ops.gb"),
    ("gb/blargg/cpu_instrs/11-op_a_hl.gb", "https://raw.githubusercontent.com/retrio/gb-test-roms/master/cpu_instrs/individual/11-op%20a,(hl).gb"),
    ("gb/blargg/dmg_sound/01-registers.gb", "https://raw.githubusercontent.com/retrio/gb-test-roms/master/dmg_sound/rom_singles/01-registers.gb"),
    ("gb/blargg/dmg_sound/02-len_ctr.gb", "https://raw.githubusercontent.com/retrio/gb-test-roms/master/dmg_sound/rom_singles/02-len%20ctr.gb"),
    ("gb/blargg/dmg_sound/03-trigger.gb", "https://raw.githubusercontent.com/retrio/gb-test-roms/master/dmg_sound/rom_singles/03-trigger.gb"),
    ("gb/blargg/dmg_sound/04-sweep.gb", "https://raw.githubusercontent.com/retrio/gb-test-roms/master/dmg_sound/rom_singles/04-sweep.gb"),
    ("gb/blargg/dmg_sound/05-sweep_details.gb", "https://raw.githubusercontent.com/retrio/gb-test-roms/master/dmg_sound/rom_singles/05-sweep%20details.gb"),
    ("gb/blargg/dmg_sound/06-overflow_on_trigger.gb", "https://raw.githubusercontent.com/retrio/gb-test-roms/master/dmg_sound/rom_singles/06-overflow%20on%20trigger.gb"),
    ("gb/blargg/dmg_sound/07-len_sweep_period_sync.gb", "https://raw.githubusercontent.com/retrio/gb-test-roms/master/dmg_sound/rom_singles/07-len%20sweep%20period%20sync.gb"),
    ("gb/blargg/dmg_sound/08-len_ctr_during_power.gb", "https://raw.githubusercontent.com/retrio/gb-test-roms/master/dmg_sound/rom_singles/08-len%20ctr%20during%20power.gb"),
    ("gb/blargg/dmg_sound/09-wave_read_while_on.gb", "https://raw.githubusercontent.com/retrio/gb-test-roms/master/dmg_sound/rom_singles/09-wave%20read%20while%20on.gb"),
    ("gb/blargg/dmg_sound/10-wave_trigger_while_on.gb", "https://raw.githubusercontent.com/retrio/gb-test-roms/master/dmg_sound/rom_singles/10-wave%20trigger%20while%20on.gb"),
    ("gb/blargg/dmg_sound/11-regs_after_power.gb", "https://raw.githubusercontent.com/retrio/gb-test-roms/master/dmg_sound/rom_singles/11-regs%20after%20power.gb"),
    ("gb/blargg/dmg_sound/12-wave_write_while_on.gb", "https://raw.githubusercontent.com/retrio/gb-test-roms/master/dmg_sound/rom_singles/12-wave%20write%20while%20on.gb"),
    (
        "gb/dmg-acid2.gb",
        "https://github.com/mattcurrie/dmg-acid2/releases/download/v1.0/dmg-acid2.gb",
    ),
    // Game Boy Color.
    (
        "gbc/cgb-acid2.gbc",
        "https://github.com/mattcurrie/cgb-acid2/releases/download/v1.1/cgb-acid2.gbc",
    ),
    (
        "gbc/blargg/cgb_sound.gb",
        "https://raw.githubusercontent.com/retrio/gb-test-roms/master/cgb_sound/cgb_sound.gb",
    ),
    // Game Boy Advance. Also the first real test of the ARM7TDMI core.
    (
        "gba/gba-suite/arm.gba",
        "https://github.com/jsmolka/gba-tests/raw/master/arm/arm.gba",
    ),
    (
        "gba/gba-suite/thumb.gba",
        "https://github.com/jsmolka/gba-tests/raw/master/thumb/thumb.gba",
    ),
    (
        "gba/gba-suite/memory.gba",
        "https://github.com/jsmolka/gba-tests/raw/master/memory/memory.gba",
    ),
];

/// Suites that ship only as an archive, and which member of it goes where.
///
/// Neither the Mooneye test suite nor mealybug-tearoom-tests publishes built ROMs as separate
/// downloads — Mooneye's repository is source and a `Makefile`, mealybug's binaries live inside
/// a zip. `game-boy-test-roms` is the community bundle of both, versioned and released, so this
/// is one download and one pinned version instead of a build toolchain. The version is in the
/// URL on purpose: an unpinned "latest" would silently change what the accuracy claims mean.
const TEST_ROM_ARCHIVES: &[(&str, &[(&str, &str)])] = &[(
    "https://github.com/c-sp/game-boy-test-roms/releases/download/v7.0/game-boy-test-roms-v7.0.zip",
    &[
        (
            "gb/mooneye/ppu/intr_1_2_timing-GS.gb",
            "mooneye-test-suite/acceptance/ppu/intr_1_2_timing-GS.gb",
        ),
        (
            "gb/mooneye/ppu/intr_2_0_timing.gb",
            "mooneye-test-suite/acceptance/ppu/intr_2_0_timing.gb",
        ),
        (
            "gb/mooneye/ppu/intr_2_mode0_timing.gb",
            "mooneye-test-suite/acceptance/ppu/intr_2_mode0_timing.gb",
        ),
        (
            "gb/mooneye/ppu/intr_2_mode0_timing_sprites.gb",
            "mooneye-test-suite/acceptance/ppu/intr_2_mode0_timing_sprites.gb",
        ),
        (
            "gb/mooneye/ppu/intr_2_mode3_timing.gb",
            "mooneye-test-suite/acceptance/ppu/intr_2_mode3_timing.gb",
        ),
        (
            "gb/mooneye/ppu/intr_2_oam_ok_timing.gb",
            "mooneye-test-suite/acceptance/ppu/intr_2_oam_ok_timing.gb",
        ),
        (
            "gb/mooneye/ppu/hblank_ly_scx_timing-GS.gb",
            "mooneye-test-suite/acceptance/ppu/hblank_ly_scx_timing-GS.gb",
        ),
        (
            "gb/mooneye/ppu/lcdon_timing-GS.gb",
            "mooneye-test-suite/acceptance/ppu/lcdon_timing-GS.gb",
        ),
        (
            "gb/mooneye/ppu/lcdon_write_timing-GS.gb",
            "mooneye-test-suite/acceptance/ppu/lcdon_write_timing-GS.gb",
        ),
        (
            "gb/mooneye/ppu/stat_lyc_onoff.gb",
            "mooneye-test-suite/acceptance/ppu/stat_lyc_onoff.gb",
        ),
        (
            "gb/mooneye/ppu/stat_irq_blocking.gb",
            "mooneye-test-suite/acceptance/ppu/stat_irq_blocking.gb",
        ),
        (
            "gb/mooneye/ppu/vblank_stat_intr-GS.gb",
            "mooneye-test-suite/acceptance/ppu/vblank_stat_intr-GS.gb",
        ),
        (
            "gb/mealybug/m3_scx_low_3_bits.gb",
            "mealybug-tearoom-tests/ppu/m3_scx_low_3_bits.gb",
        ),
        (
            "gb/mealybug/m3_bgp_change.gb",
            "mealybug-tearoom-tests/ppu/m3_bgp_change.gb",
        ),
        (
            "gb/mealybug/m3_wx_4_change.gb",
            "mealybug-tearoom-tests/ppu/m3_wx_4_change.gb",
        ),
        (
            "gb/mealybug/m3_wx_5_change.gb",
            "mealybug-tearoom-tests/ppu/m3_wx_5_change.gb",
        ),
        (
            "gb/mealybug/m3_wx_6_change.gb",
            "mealybug-tearoom-tests/ppu/m3_wx_6_change.gb",
        ),
    ],
)];

/// Fetch each archive and extract only the members the corpus names.
///
/// Only the named members: the bundle is 3.7 MB of suites this project does not run, and
/// unpacking all of it would make `testing/test-roms/` a pile nobody can map back to a test.
///
/// Uses `unzip`, on the same reasoning as `curl` above — it is present on macOS, on every Linux
/// distribution, and in Windows' own toolchain since 10 — and a zip decoder is a lot of
/// dependency for a step that runs once per checkout.
fn fetch_archives(corpus: &Path, force: bool, failed: &mut Vec<String>) -> Result<(usize, usize)> {
    let mut fetched = 0usize;
    let mut skipped = 0usize;

    for (url, members) in TEST_ROM_ARCHIVES {
        let wanted: Vec<&(&str, &str)> = members
            .iter()
            .filter(|(path, _)| force || !corpus.join(path).is_file())
            .collect();
        skipped += members.len() - wanted.len();
        if wanted.is_empty() {
            continue;
        }
        if !have_probe("unzip", "-v") {
            bail!("unzip is required to extract {url}; install it and re-run");
        }

        // Into a temporary directory beside the corpus, so a failed extraction leaves nothing
        // half-written where the harness would find it and read it as a ROM.
        let staging = corpus.join(".archive-staging");
        let _ = std::fs::remove_dir_all(&staging);
        std::fs::create_dir_all(&staging)
            .with_context(|| format!("creating {}", staging.display()))?;
        let archive = staging.join("archive.zip");

        println!("fetching {url}");
        let status = Command::new("curl")
            .args([
                "--location",
                "--fail",
                "--silent",
                "--show-error",
                "--max-time",
                "600",
                "--output",
            ])
            .arg(&archive)
            .arg(*url)
            .status()
            .with_context(|| format!("running curl for {url}"))?;
        if !status.success() {
            let _ = std::fs::remove_dir_all(&staging);
            for (path, _) in &wanted {
                failed.push((*path).to_string());
            }
            continue;
        }

        for (path, member) in &wanted {
            let target = corpus.join(path);
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("creating {}", parent.display()))?;
            }
            // `-j` flattens the archive's directories; the corpus decides its own layout.
            let status = Command::new("unzip")
                .args(["-o", "-j", "-q"])
                .arg(&archive)
                .arg(member)
                .arg("-d")
                .arg(&staging)
                .status()
                .with_context(|| format!("running unzip for {member}"))?;

            let extracted = staging.join(Path::new(member).file_name().unwrap_or_default());
            if status.success() && extracted.is_file() {
                std::fs::rename(&extracted, &target)
                    .with_context(|| format!("moving {member} into place"))?;
                fetched += 1;
            } else {
                failed.push((*path).to_string());
            }
        }
        let _ = std::fs::remove_dir_all(&staging);
    }
    Ok((fetched, skipped))
}

/// The workspace root, found from this crate rather than the current directory.
fn workspace_root() -> Result<std::path::PathBuf> {
    Ok(std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .context("xtask should live inside the workspace")?
        .to_path_buf())
}

fn lint(fix: bool) -> Result<()> {
    if fix {
        run(&cargo(), &["fmt", "--all"])?;
        run(
            &cargo(),
            &[
                "clippy",
                "--workspace",
                "--all-targets",
                "--fix",
                "--allow-dirty",
            ],
        )
    } else {
        run(&cargo(), &["fmt", "--all", "--check"])?;
        run(
            &cargo(),
            &[
                "clippy",
                "--workspace",
                "--all-targets",
                "--",
                "-D",
                "warnings",
            ],
        )
    }
}
