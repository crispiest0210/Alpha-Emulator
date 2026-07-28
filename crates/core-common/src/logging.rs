//! One `tracing` setup shared by every entrypoint.
//!
//! `frontend-native`, `frontend-headless`, and the accuracy harness all initialize logging
//! the same way, so a `RUST_LOG` filter that works when debugging in the GUI works
//! identically in a headless test run.
//!
//! Filtering is per-crate target, using the crate name with underscores:
//!
//! ```text
//! RUST_LOG=info
//! RUST_LOG=cpu_sm83=trace,system_gb=debug
//! RUST_LOG=warn,system_gba::dma=trace
//! ```

use tracing_subscriber::filter::EnvFilter;

/// Initialize the global subscriber, or do nothing if one is already installed.
///
/// Returns whether this call installed it. Idempotent on purpose: tests and the harness can
/// both call it without a double-init panic taking down a test run for an unrelated reason.
///
/// `default_filter` applies only when `RUST_LOG` is unset or unparseable.
pub fn init(default_filter: &str) -> bool {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_filter));

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true)
        .try_init()
        .is_ok()
}

/// Like [`init`], but writes to stderr and omits timestamps.
///
/// For the headless driver and CI, where stdout carries the actual result (framebuffer
/// hashes, test verdicts) and log lines must not contaminate it or churn snapshot diffs with
/// wall-clock times.
pub fn init_for_tools(default_filter: &str) -> bool {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_filter));

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .without_time()
        .with_target(true)
        .try_init()
        .is_ok()
}
