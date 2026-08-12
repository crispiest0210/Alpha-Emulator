//! `cargo xtask <task>` — the single, OS-uniform developer entrypoint.
//!
//! Deliberately a Rust program rather than a shell script: the predecessor project needed
//! divergent `npm run dev` / `./run.sh dev` paths and a bespoke `setup_linux_deps.sh` that
//! vendored `.pc` files into the repo. Nothing here shells out to a platform shell, and
//! `setup` never downloads or vendors a binary — it only *reports* what to install.

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
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
    Command::new(program)
        .arg("--version")
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
    let corpus_dir = root.join("testing").join("test-roms");

    let mut fetched = 0usize;
    let mut skipped = 0usize;
    let mut failed = Vec::new();

    for rom in corpus::all_roms() {
        let target = corpus_dir.join(rom.path);
        if target.is_file() && !force {
            skipped += 1;
            continue;
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }

        println!("fetching {}", rom.path);
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
            .arg(rom.url)
            .status()
            .with_context(|| format!("running curl for {}", rom.path))?;

        if status.success() {
            fetched += 1;
        } else {
            // Leave nothing half-written behind: a truncated ROM would fail the suite in a
            // way that looks like an emulator bug.
            let _ = std::fs::remove_file(&target);
            failed.push(rom.path);
        }
    }

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
