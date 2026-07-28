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
    Bench,
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
        Task::Bench => bench(),
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
        // TODO(prompt17): drive testing/harness against the fetched test-ROM corpus.
        println!("accuracy suite: not implemented yet (prompt 17)");
    }
    Ok(())
}

fn bench() -> Result<()> {
    run(&cargo(), &["bench", "--workspace"])
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
