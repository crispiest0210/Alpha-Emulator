//! Which machine a ROM is for.
//!
//! This lives in `library` rather than in `frontend-core` because the index stores it: a row in
//! the ROM table needs a platform column, and the platform of a file has to be decidable
//! *before* a system exists to run it. `frontend-core` turns one of these into a running
//! [`System`](core_common::System); this crate only names it.

use std::path::Path;

/// A supported machine.
///
/// The Nintendo DS is named here even though nothing can run one yet. Leaving it out would make
/// an imported `.nds` file look like an unrecognised file rather than a recognised file for a
/// system that is not finished, and those are different messages for the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Platform {
    Gb,
    Gbc,
    Gba,
    Nds,
}

impl Platform {
    /// Stable identifier, matching [`System::id`](core_common::System::id) so a save state and a
    /// library row agree on what they are talking about. Stored in the database, so it must not
    /// change once released.
    pub const fn id(self) -> &'static str {
        match self {
            Platform::Gb => "gb",
            Platform::Gbc => "gbc",
            Platform::Gba => "gba",
            Platform::Nds => "nds",
        }
    }

    pub const fn display_name(self) -> &'static str {
        match self {
            Platform::Gb => "Game Boy",
            Platform::Gbc => "Game Boy Color",
            Platform::Gba => "Game Boy Advance",
            Platform::Nds => "Nintendo DS",
        }
    }

    /// Whether this platform can actually be played today.
    ///
    /// The library indexes ROMs it cannot run — that is deliberate, so a user can import their
    /// whole collection — and the UI greys out the ones that are not playable yet instead of
    /// hiding them or failing on click.
    pub const fn is_runnable(self) -> bool {
        !matches!(self, Platform::Nds)
    }

    pub fn from_id(id: &str) -> Option<Self> {
        match id {
            "gb" => Some(Platform::Gb),
            "gbc" => Some(Platform::Gbc),
            "gba" => Some(Platform::Gba),
            "nds" => Some(Platform::Nds),
            _ => None,
        }
    }

    /// Decide from the file extension.
    ///
    /// The extension chooses the *hardware*, never the mode: a `.gb` file may be a
    /// CGB-enhanced cartridge and a `.gbc` file may be a plain monochrome one, and which of
    /// those it is comes from the header once the machine is built. That split is why this
    /// function is allowed to be as simple as it looks.
    pub fn from_extension(path: &Path) -> Option<Self> {
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        match ext.as_str() {
            "gb" => Some(Platform::Gb),
            "gbc" | "cgb" => Some(Platform::Gbc),
            "gba" | "agb" => Some(Platform::Gba),
            "nds" => Some(Platform::Nds),
            _ => None,
        }
    }

    /// Every extension a file dialog or a directory scan should accept.
    pub const EXTENSIONS: &'static [&'static str] = &["gb", "gbc", "cgb", "gba", "agb", "nds"];

    pub const ALL: &'static [Platform] =
        &[Platform::Gb, Platform::Gbc, Platform::Gba, Platform::Nds];
}

impl std::fmt::Display for Platform {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.display_name())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extensions_map_case_insensitively() {
        assert_eq!(
            Platform::from_extension(Path::new("A.GBA")),
            Some(Platform::Gba)
        );
        assert_eq!(
            Platform::from_extension(Path::new("a.Gb")),
            Some(Platform::Gb)
        );
    }

    #[test]
    fn an_unknown_extension_is_not_a_platform() {
        assert_eq!(Platform::from_extension(Path::new("readme.txt")), None);
        assert_eq!(Platform::from_extension(Path::new("noext")), None);
    }

    #[test]
    fn ids_round_trip() {
        for platform in Platform::ALL {
            assert_eq!(Platform::from_id(platform.id()), Some(*platform));
        }
    }

    #[test]
    fn every_listed_extension_resolves() {
        for ext in Platform::EXTENSIONS {
            let path = std::path::PathBuf::from(format!("rom.{ext}"));
            assert!(
                Platform::from_extension(&path).is_some(),
                "{ext} is advertised but does not resolve"
            );
        }
    }
}
