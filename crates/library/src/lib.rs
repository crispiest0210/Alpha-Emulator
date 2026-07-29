//! SQLite-backed ROM/save index and filesystem reconciliation.
//!
//! The durable half of the frontend's state. Everything that must survive a restart lives here —
//! which ROMs the user has, what they are called, when each was last played, and what save states
//! exist for each — while everything ephemeral (which ROM is running, whether it is paused) stays
//! in memory in `frontend-core`. That split is the design; see [`index`] for why the index rather
//! than a directory scan is the source of truth.
//!
//! # Status
//!
//! Complete for what prompt 14 asks of it: schema and migrations, ROM and save-state CRUD,
//! watched folders, and a reconciliation pass that detects files added, removed, and *moved*
//! outside the application. Unit tests drive reconciliation against a real temporary directory
//! with a real SQLite engine.
//!
//! Not done, and not required yet: save-state thumbnails (the schema has no column for one; add
//! it with a migration when the frontend can produce one), and any notion of a ROM's box art or
//! external metadata source.
//!
//! # Dependency rule
//!
//! No `winit`, `wgpu`, `egui`, or `cpal`, enforced by `cargo deny check bans`. This crate is also
//! free of `cart-common`: it names a [`Platform`] but never parses a cartridge, so the importer
//! that *does* want a header title supplies it through [`Library::set_title`].

#![deny(unsafe_code)]

pub mod index;
pub mod paths;
pub mod platform;

pub use index::{
    content_hash, Library, NewRom, Reconciliation, RomEntry, RomId, SaveId, SaveStateEntry,
};
pub use paths::{AppPaths, STATE_EXTENSION};
pub use platform::Platform;

use std::path::PathBuf;

/// Why a library operation failed.
#[derive(Debug, thiserror::Error)]
pub enum LibraryError {
    #[error("library database error: {0}")]
    Database(#[from] rusqlite::Error),

    #[error("library file error: {0}")]
    Io(#[from] std::io::Error),

    #[error("{0} is not a ROM for any system this emulator knows")]
    UnknownPlatform(PathBuf),

    #[error("{0} is not in the library")]
    Missing(PathBuf),
}

#[cfg(test)]
mod tests;
