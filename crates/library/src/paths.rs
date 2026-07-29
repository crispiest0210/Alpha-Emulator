//! Where the application keeps its files on each OS.
//!
//! The predecessor project used the platform's local-app-data directory and that part of its
//! design was sound, so it is kept: an emulator's library index, battery saves, and save states
//! are user data, not cache, and they must survive an application update. What changed is only
//! what *backs* the library half — a SQLite index instead of a directory rescan.
//!
//! # Why a struct rather than free functions
//!
//! Every path here is derived from one root, and tests need a root that is not the developer's
//! real library. [`AppPaths::rooted_at`] gives that with no environment variable and no `cfg`
//! branch, so the production and test paths are the same code.

use std::path::{Path, PathBuf};

/// The resolved on-disk layout.
///
/// ```text
/// <data>/library.sqlite3          the index
/// <data>/saves/<rom>.sav          battery-backed cartridge RAM
/// <data>/states/<rom>/<name>.ast  save states, one directory per ROM
/// <data>/screenshots/<name>.png
/// <config>/config.toml            settings: keybinds, volume, presentation
/// ```
///
/// Save states are grouped per ROM rather than pooled into one directory. That is the
/// "per-ROM save organization" the predecessor got right: with a hundred games in the library,
/// one flat directory of `.ast` files is unusable from a file manager, and the grouping also
/// makes "delete this ROM and its states" a directory removal rather than a query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppPaths {
    data: PathBuf,
    config: PathBuf,
}

/// Extension for a save state. Short, unambiguous, and not a real file type owned by anyone
/// else — `.state` collides with several tools and `.ss` with none of them memorably.
pub const STATE_EXTENSION: &str = "ast";

impl AppPaths {
    /// Resolve the OS-appropriate locations.
    ///
    /// Falls back to a `.alpha-emulator` directory beside the current working directory if the
    /// platform directories cannot be determined, which happens on stripped-down containers
    /// with no `HOME`. Failing to start over that would be worse than being slightly wrong
    /// about where files go.
    pub fn discover() -> Self {
        match directories::ProjectDirs::from("dev", "Alpha", "AlphaEmulator") {
            Some(dirs) => Self {
                data: dirs.data_local_dir().to_path_buf(),
                config: dirs.config_dir().to_path_buf(),
            },
            None => {
                tracing::warn!("no platform data directory; using ./.alpha-emulator");
                let root = PathBuf::from(".alpha-emulator");
                Self {
                    data: root.join("data"),
                    config: root.join("config"),
                }
            }
        }
    }

    /// Put everything under one directory. Used by tests and by a portable install.
    pub fn rooted_at(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        Self {
            data: root.join("data"),
            config: root.join("config"),
        }
    }

    pub fn data_dir(&self) -> &Path {
        &self.data
    }

    pub fn config_dir(&self) -> &Path {
        &self.config
    }

    pub fn database(&self) -> PathBuf {
        self.data.join("library.sqlite3")
    }

    pub fn config_file(&self) -> PathBuf {
        self.config.join("config.toml")
    }

    pub fn saves_dir(&self) -> PathBuf {
        self.data.join("saves")
    }

    pub fn states_dir(&self) -> PathBuf {
        self.data.join("states")
    }

    pub fn screenshots_dir(&self) -> PathBuf {
        self.data.join("screenshots")
    }

    /// Battery-backed save file for a ROM.
    pub fn save_file(&self, rom: &Path) -> PathBuf {
        self.saves_dir().join(format!("{}.sav", file_key(rom)))
    }

    /// Directory holding one ROM's save states.
    pub fn states_dir_for(&self, rom: &Path) -> PathBuf {
        self.states_dir().join(file_key(rom))
    }

    /// Path for a numbered quick-save slot.
    pub fn state_slot_file(&self, rom: &Path, slot: u8) -> PathBuf {
        self.states_dir_for(rom)
            .join(format!("slot{slot}.{STATE_EXTENSION}"))
    }

    /// Path for a user-labelled save state.
    pub fn state_named_file(&self, rom: &Path, label: &str) -> PathBuf {
        self.states_dir_for(rom)
            .join(format!("{}.{STATE_EXTENSION}", sanitize(label)))
    }

    /// Create every directory the application writes into.
    ///
    /// Called once at startup so no later write has to worry about a missing parent. Errors
    /// are returned rather than logged: if the data directory cannot be created, saving is
    /// going to fail too, and the user should hear it now instead of after an hour of play.
    pub fn create_all(&self) -> std::io::Result<()> {
        for dir in [
            self.data.clone(),
            self.config.clone(),
            self.saves_dir(),
            self.states_dir(),
            self.screenshots_dir(),
        ] {
            std::fs::create_dir_all(dir)?;
        }
        Ok(())
    }
}

/// The stable per-ROM directory and file name.
///
/// Derived from the file stem rather than a content hash, because a user browsing
/// `saves/` should recognise their games. Two different ROMs with the same file name therefore
/// share a save file, which is the same behaviour every emulator that names saves after the ROM
/// has — and the alternative, a directory full of hex digests, trades a rare collision for a
/// permanent usability cost.
fn file_key(rom: &Path) -> String {
    let stem = rom
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("untitled");
    sanitize(stem)
}

/// Reduce a label to something safe on every filesystem.
///
/// Windows rejects `<>:"/\|?*`, every platform rejects the separator, and a leading dot hides
/// the file on Unix. Anything outside the allowed set becomes `_` rather than being dropped, so
/// two labels that differ only in punctuation do not silently become the same file.
fn sanitize(label: &str) -> String {
    let mut out: String = label
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, ' ' | '-' | '_' | '.' | '(' | ')') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let trimmed = out.trim().trim_start_matches('.').to_string();
    out = if trimmed.is_empty() {
        "untitled".to_string()
    } else {
        trimmed
    };
    // Long names are truncated rather than rejected: 120 bytes leaves room for the extension
    // inside the 255-byte limit every mainstream filesystem shares.
    out.chars().take(120).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rooted_paths_are_all_under_the_root() {
        let paths = AppPaths::rooted_at("/tmp/alpha-test");
        assert!(paths.database().starts_with("/tmp/alpha-test"));
        assert!(paths.config_file().starts_with("/tmp/alpha-test"));
        assert!(paths.saves_dir().starts_with("/tmp/alpha-test"));
    }

    #[test]
    fn save_file_is_named_after_the_rom_stem() {
        let paths = AppPaths::rooted_at("/root");
        let save = paths.save_file(Path::new("/games/Zelda.gbc"));
        assert_eq!(save.file_name().unwrap(), "Zelda.sav");
    }

    #[test]
    fn slot_and_named_states_live_in_the_same_per_rom_directory() {
        let paths = AppPaths::rooted_at("/root");
        let rom = Path::new("/games/Metroid.gba");
        let slot = paths.state_slot_file(rom, 3);
        let named = paths.state_named_file(rom, "before boss");
        assert_eq!(slot.parent(), named.parent());
        assert_eq!(slot.file_name().unwrap(), "slot3.ast");
        assert_eq!(named.file_name().unwrap(), "before boss.ast");
    }

    #[test]
    fn path_separators_in_a_label_cannot_escape_the_directory() {
        let paths = AppPaths::rooted_at("/root");
        let evil = paths.state_named_file(Path::new("/games/x.gb"), "../../etc/passwd");
        // Every separator became `_` and the leading dots were stripped, so what is left is one
        // file name in the intended directory rather than a traversal.
        assert_eq!(evil.file_name().unwrap(), "_.._etc_passwd.ast");
        assert!(evil.starts_with("/root"));
    }

    #[test]
    fn an_empty_or_dotted_label_still_produces_a_usable_name() {
        assert_eq!(sanitize("   "), "untitled");
        assert_eq!(sanitize(".hidden"), "hidden");
        assert_eq!(sanitize(""), "untitled");
    }
}
