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
//!
//! # Sharing the loader with the window
//!
//! The extension-to-system decision lives in [`frontend_core::platform`], not here. It used to be
//! a `match` in this file, and the native frontend would have needed a second copy — which is how
//! two frontends end up disagreeing about what a `.gbc` file is. A `.gb` file gets Game Boy
//! *hardware* and a `.gbc` file colour hardware; the header inside decides whether the colour
//! machine runs in full colour or in DMG-compatibility mode, and that is `GbcSystem`'s business.

use anyhow::{Context, Result};
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
        /// Write the final framebuffer here as a PNG.
        ///
        /// This is how a rendering test ROM gets *looked at* rather than reduced to a hash. Two of
        /// the corpus's open gaps — dmg-acid2 and cgb-acid2, which render and complete but have
        /// never been compared against their published reference images — need exactly this and
        /// nothing more.
        #[arg(long, value_name = "FILE")]
        save_frame: Option<PathBuf>,
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
    /// Report what a ROM is without running it: system, title, size, content hash.
    ///
    /// The same probe the library importer uses, so a title or hash printed here is the one that
    /// would be indexed — which is what makes "why is this listed under that name?" answerable
    /// without opening the application.
    Identify { rom: PathBuf },
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
            save_frame,
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

            if let Some(path) = save_frame {
                std::fs::write(&path, frontend_core::encode_png(system.framebuffer()))
                    .with_context(|| format!("could not write {}", path.display()))?;
                println!("wrote  {}", path.display());
            }
        }

        Command::CheckDeterminism { rom, frames } => {
            let first = run_to_hash(&rom, frames)?;
            let second = run_to_hash(&rom, frames)?;
            if first != second {
                anyhow::bail!("not deterministic: {first} then {second} over {frames} frames");
            }
            println!("deterministic over {frames} frames: {first}");
        }

        Command::Identify { rom } => {
            let info = frontend_core::platform::probe(&rom)?;
            println!("system {}", info.platform.display_name());
            println!("title  {}", info.title);
            println!("size   {} bytes", info.size_bytes);
            println!("hash   {:016x}", info.content_hash);
        }
    }
    Ok(())
}

/// Build the right system for a ROM.
///
/// A ROM for a system that is not assembled yet is reported by name — "the Nintendo DS is not
/// assembled yet" — rather than swept into one "unsupported" arm, so the error says which system a
/// ROM needs instead of leaving the user to wonder whether their file is corrupt.
fn load(path: &Path) -> Result<Box<dyn System>> {
    let (_, system) = frontend_core::platform::load(path)?;
    Ok(system)
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
