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

use crate::Platform;
use std::path::{Path, PathBuf};

/// The resolved on-disk layout.
///
/// ```text
/// <data>/library.sqlite3          the index
/// <data>/saves/<key>.sav          battery-backed cartridge RAM
/// <data>/states/<key>/<name>.ast  save states, one directory per ROM
///
/// <key> is the ROM's file stem plus its platform: `Zelda.gbc`, `Metroid.gba`
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

    /// Where [`Self::save_file`] pointed before the key carried a platform.
    ///
    /// Read only by the migration, which moves a file from here to the current path when exactly
    /// one destination claims it. Nothing writes here.
    pub fn legacy_save_file(&self, rom: &Path) -> PathBuf {
        self.saves_dir()
            .join(format!("{}.sav", legacy_file_key(rom)))
    }

    /// Where [`Self::states_dir_for`] pointed before the key carried a platform.
    pub fn legacy_states_dir_for(&self, rom: &Path) -> PathBuf {
        self.states_dir().join(legacy_file_key(rom))
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

/// The stable per-ROM directory and file name: the sanitised stem, a dot, and a platform tag.
///
/// Derived from the file name rather than a content hash, because a user browsing `saves/` should
/// recognise their games. The platform tag is what stops `Game.gb`, `Game.gbc` and `Game.gba` — a
/// common shape for a series released on all three — sharing one battery file and one state
/// folder, which handed each game the other's save and let `remove_rom` on one delete the other's
/// states.
///
/// Rejected, both argued in the commit that introduced this:
/// - **Stem plus a short content hash.** Collision-proof, but unreadable in a file manager, and it
///   changes when a ROM is patched — so applying a translation patch would strand the save.
/// - **Keep the stem and disambiguate only on collision.** The smallest change, but the name a
///   ROM's save lives under would then depend on which *other* ROMs were imported first, and that
///   history is not something a user can see or a backup can reproduce.
///
/// The tag is the [`Platform`] id for a recognised extension — so `.cgb` and `.gbc` agree, and
/// renaming between them keeps the save — and otherwise the extension itself. Two ROMs with the
/// same file name *and* platform in different folders still share; that is the one collision
/// left, and it is the one every emulator naming saves after the ROM has.
fn file_key(rom: &Path) -> String {
    let tag = match Platform::from_extension(rom) {
        Some(platform) => platform.id().to_string(),
        None => rom
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| sanitize(&e.to_lowercase()))
            .unwrap_or_else(|| "rom".to_string()),
    };
    format!("{}.{tag}", stem_of(rom))
}

fn stem_of(rom: &Path) -> String {
    let stem = rom
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("untitled");
    // Room for the tag and `.sav` inside the 255-byte name limit, and the 120-byte cap `sanitize`
    // gives a state label.
    truncate_bytes(sanitize(stem), 200)
}

/// The key before the platform tag existed, with the ASCII-only sanitiser it used.
///
/// Frozen: it names files already on users' disks, so it must reproduce exactly what the old
/// code produced, including mapping every non-ASCII character to `_`.
fn legacy_file_key(rom: &Path) -> String {
    let stem = rom
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("untitled");
    let out: String = stem
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
    let out = if trimmed.is_empty() {
        "untitled".to_string()
    } else {
        trimmed
    };
    out.chars().take(120).collect()
}

/// Reduce a label to something safe on every filesystem, keeping Unicode letters and digits.
///
/// Only what some mainstream filesystem actually rejects or misreads is replaced with `_`: the
/// separators, Windows' `<>:"|?*`, and control characters. Replacing rather than dropping keeps two
/// labels that differ only there from becoming the same file. Everything else is kept — the
/// previous ASCII-only rule turned every Japanese title into a row of underscores, so any two
/// Japanese ROMs of the same length shared one save.
///
/// A leading dot hides a file on Unix and is stripped; trailing dots and spaces are silently
/// dropped by Windows, so they are trimmed here rather than there. A Windows device name (`CON`,
/// `COM1`, …) is reserved *with any extension*, so one gets a trailing `_`.
///
/// Not done: Unicode normalisation. A name typed as NFC and the same name as NFD are different
/// strings here and the same file on APFS. Doing it needs a normalisation table, which means a
/// dependency, for a case that needs the same title entered two different ways.
fn sanitize(label: &str) -> String {
    let out: String = label
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '/' | '\\' | '<' | '>' | ':' | '"' | '|' | '?' | '*') {
                '_'
            } else {
                c
            }
        })
        .collect();
    let trimmed = out
        .trim()
        .trim_start_matches('.')
        .trim_end_matches(['.', ' '])
        .to_string();
    let mut out = if trimmed.is_empty() {
        "untitled".to_string()
    } else {
        trimmed
    };
    let device = out.split('.').next().unwrap_or("").to_ascii_uppercase();
    let reserved = matches!(device.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (device.len() == 4
            && (device.starts_with("COM") || device.starts_with("LPT"))
            && device.as_bytes()[3].is_ascii_digit());
    if reserved {
        out.insert(device.len(), '_');
    }
    // Long names are truncated rather than rejected, by bytes rather than characters: a CJK
    // character is three bytes, and 120 of them would overrun the 255-byte limit every mainstream
    // filesystem shares.
    truncate_bytes(out, 120)
}

fn truncate_bytes(mut s: String, max: usize) -> String {
    if s.len() > max {
        let mut end = max;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
    }
    s
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
        assert_eq!(save.file_name().unwrap(), "Zelda.gbc.sav");
        assert_eq!(
            paths
                .legacy_save_file(Path::new("/games/Zelda.gbc"))
                .file_name()
                .unwrap(),
            "Zelda.sav"
        );
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
    fn the_same_stem_on_two_platforms_gets_two_saves_and_two_state_folders() {
        let paths = AppPaths::rooted_at("/root");
        let gb = Path::new("/games/A.gb");
        let gba = Path::new("/games/A.gba");
        assert_ne!(paths.save_file(gb), paths.save_file(gba));
        assert_ne!(paths.states_dir_for(gb), paths.states_dir_for(gba));
        assert_eq!(paths.save_file(gba).file_name().unwrap(), "A.gba.sav");
        // Both alias extensions of one platform agree, so renaming between them keeps the save.
        assert_eq!(
            paths.save_file(Path::new("/games/A.cgb")),
            paths.save_file(Path::new("/games/A.gbc"))
        );
        // The legacy key is what they used to share.
        assert_eq!(paths.legacy_save_file(gb), paths.legacy_save_file(gba));
    }

    #[test]
    fn two_japanese_titles_of_the_same_length_no_longer_collide() {
        let paths = AppPaths::rooted_at("/root");
        let a = Path::new("/games/ポケモン.gba");
        let b = Path::new("/games/ゼルダの.gba");
        assert_ne!(paths.save_file(a), paths.save_file(b));
        assert_ne!(paths.states_dir_for(a), paths.states_dir_for(b));
        assert_eq!(paths.save_file(a).file_name().unwrap(), "ポケモン.gba.sav");
        // Exactly the collision the old ASCII-only rule produced.
        assert_eq!(paths.legacy_save_file(a), paths.legacy_save_file(b));
        assert_eq!(paths.legacy_save_file(a).file_name().unwrap(), "____.sav");
    }

    #[test]
    fn only_characters_some_filesystem_rejects_are_replaced() {
        assert_eq!(sanitize("Pokémon: Émeraude?"), "Pokémon_ Émeraude_");
        assert_eq!(sanitize("a<b>c|d*e\"f"), "a_b_c_d_e_f");
        assert_eq!(sanitize("tab\there"), "tab_here");
        assert_eq!(sanitize("trailing. . "), "trailing");
        assert_eq!(sanitize("Mario & Luigi!"), "Mario & Luigi!");
    }

    #[test]
    fn a_windows_device_name_is_never_produced() {
        assert_eq!(sanitize("CON"), "CON_");
        assert_eq!(sanitize("com1.gba"), "com1_.gba");
        assert_eq!(sanitize("CONSOLE"), "CONSOLE");
        assert_eq!(sanitize("COMX"), "COMX");
    }

    #[test]
    fn a_long_cjk_name_is_cut_on_a_character_boundary_within_the_limit() {
        let long = "あ".repeat(200);
        let cut = sanitize(&long);
        assert!(cut.len() <= 120);
        assert!(cut.chars().all(|c| c == 'あ'));
        let key = file_key(Path::new(&format!("/g/{long}.gba")));
        assert!(format!("{key}.sav").len() <= 255);
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
