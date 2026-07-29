//! Headless CLI driver: run a ROM with no window, no audio device, and no GPU.
//!
//! # Why this exists separately from the test harness
//!
//! `testing/harness` drives systems in-process and asserts. This drives one from a shell and
//! prints. They are not the same job: a bisect over emulator commits, a CI step that diffs a
//! framebuffer hash, or a bug report that needs "run this ROM for 600 frames and tell me what
//! you see" all want a binary, not a `#[test]`.
//!
//! It also proves something the harness cannot: that a system is usable through the [`System`]
//! trait alone, with nothing from `frontend-native` and no UI framework linked in. If that ever
//! stops being true, this binary stops compiling — which is the crate-boundary rule enforcing
//! itself at the place it matters most.

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use core_common::{Framebuffer, InputState, System};
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(name = "alpha-headless", about = "Headless emulator driver", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run a ROM for a fixed number of frames and report a framebuffer hash.
    Run {
        rom: PathBuf,
        #[arg(long, default_value_t = 600)]
        frames: u64,
        /// Also print a hash every N frames, to locate where two runs diverge.
        #[arg(long)]
        trace_every: Option<u64>,
    },
    /// Run a ROM twice from the same start and report whether the two runs match.
    ///
    /// Determinism is the property every save state, rewind, and replay depends on. It is
    /// cheap to check and easy to lose — one `HashMap` iteration order or one unseeded RNG in
    /// the wrong place is enough.
    CheckDeterminism {
        rom: PathBuf,
        #[arg(long, default_value_t = 600)]
        frames: u64,
    },
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    match Cli::parse().command {
        Command::Run {
            rom,
            frames,
            trace_every,
        } => {
            let mut system = load(&rom)?;
            for frame in 1..=frames {
                system.step_frame(InputState::default());
                // Draining audio matters even with nothing to play it: the buffer is bounded,
                // and a run that never drains does not exercise the path a real frontend does.
                let _ = system.take_audio_samples();
                if let Some(every) = trace_every {
                    if every > 0 && frame.is_multiple_of(every) {
                        println!("{frame:>8}  {}", hash(system.framebuffer()));
                    }
                }
            }
            println!("frames {frames}");
            println!("hash   {}", hash(system.framebuffer()));
        }

        Command::CheckDeterminism { rom, frames } => {
            let first = run_to_hash(&rom, frames)?;
            let second = run_to_hash(&rom, frames)?;
            if first != second {
                bail!("not deterministic: {first} then {second} over {frames} frames");
            }
            println!("deterministic over {frames} frames: {first}");
        }
    }
    Ok(())
}

/// Build the right system for a ROM.
///
/// Only the Game Boy family is assembled so far. The others are named explicitly rather than
/// swept into one "unsupported" arm, so the error says which system a ROM needs instead of
/// leaving the user to wonder whether their file is corrupt.
fn load(path: &Path) -> Result<Box<dyn System>> {
    let bytes = std::fs::read(path)
        .with_context(|| format!("could not read the ROM at {}", path.display()))?;

    match path.extension().and_then(|e| e.to_str()) {
        // A `.gb` file may still be a CGB-enhanced cartridge and a `.gbc` file may be a plain
        // DMG one, so the extension only chooses the *hardware*; the header inside chooses the
        // mode. That is `GbcSystem`'s job, which is why `.gbc` does not get a different branch
        // so much as different hardware to run on.
        Some("gb") => {
            let system = system_gb::GbSystem::new(bytes, None)
                .map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
            Ok(Box::new(system))
        }
        Some("gbc") => {
            let system = system_gbc::GbcSystem::new(bytes, None)
                .map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;
            Ok(Box::new(system))
        }
        Some("gba") => bail!("the Game Boy Advance system is not assembled yet (prompt 12)"),
        Some("nds") => bail!("the Nintendo DS system is not assembled yet (prompt 13)"),
        other => bail!(
            "unrecognised ROM extension {:?}; expected .gb, .gbc, .gba, or .nds",
            other.unwrap_or("(none)")
        ),
    }
}

fn run_to_hash(path: &Path, frames: u64) -> Result<String> {
    let mut system = load(path)?;
    for _ in 0..frames {
        system.step_frame(InputState::default());
        let _ = system.take_audio_samples();
    }
    Ok(hash(system.framebuffer()))
}

/// FNV-1a over the raw framebuffer.
///
/// Deliberately the same function the test harness uses, so a hash printed here can be pasted
/// into a corpus entry without a conversion step. It is not cryptographic and does not need to
/// be: it compares two runs of the same emulator, it does not defend against a forgery.
fn hash(framebuffer: &Framebuffer) -> String {
    let mut hash: u64 = 0xCBF2_9CE4_8422_2325;
    for byte in framebuffer.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01B3);
    }
    format!("{hash:016x}")
}
