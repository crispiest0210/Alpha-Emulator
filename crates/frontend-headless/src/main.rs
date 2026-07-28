//! Headless CLI driver used by the accuracy test harness and scripted playback.
//!
// TODO(prompt17): run ROMs for N frames, dump framebuffer hashes, replay input movies.

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

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
    },
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cli = Cli::parse();
    match cli.command {
        Command::Run { rom, frames } => {
            // TODO(prompt17): detect system from the ROM, construct it, step `frames` frames.
            tracing::info!(rom = %rom.display(), frames, "not yet implemented");
            anyhow::bail!("headless run is not implemented yet (prompt 17)");
        }
    }
}
