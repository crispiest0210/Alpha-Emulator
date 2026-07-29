//! Turning a file on disk into a running machine, and knowing how fast to run it.
//!
//! One place, consumed by every frontend. Before this existed, `frontend-headless` had its own
//! extension-to-system `match` and the native frontend would have needed a second copy — the
//! exact duplication that ends with two frontends disagreeing about what a `.gbc` file is.
//!
//! # Why the frame rate is a table here rather than a method on `System`
//!
//! It could be either. A method would put each rate next to the hardware it describes, which is
//! this project's usual instinct. It is not done that way because the only sane default for such
//! a method is 60 Hz, and a system that forgot to override it would then run at a *plausible*
//! wrong speed — audio very slightly the wrong pitch, drifting out of sync over minutes. That is
//! precisely the failure this codebase refuses elsewhere as "do not approximate a behaviour you
//! have not modelled". A table with no default cannot be silently wrong: a new system either
//! appears in it or fails to compile.

use core_common::{CartridgeError, System};
use library::Platform;
use std::path::Path;
use std::time::Duration;

/// What a ROM file turned out to be, without building a machine for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RomInfo {
    pub platform: Platform,
    /// Title from the cartridge header when it has a usable one, else the file stem.
    pub title: String,
    pub size_bytes: u64,
    pub content_hash: u64,
}

/// Why a ROM could not be loaded.
#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error("could not read {path}: {source}")]
    Io {
        path: std::path::PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error(
        "{0} is not a ROM for any system this emulator knows (expected .gb, .gbc, .gba, or .nds)"
    )]
    UnknownPlatform(std::path::PathBuf),

    #[error("the {0} is not assembled yet, so this ROM cannot run — the library still lists it")]
    NotImplemented(&'static str),

    #[error("{0}")]
    Cartridge(#[from] CartridgeError),
}

/// Read a ROM and report what it is, without constructing a system.
///
/// Used by the importer: the library wants a title and a hash for a file the user may never
/// play, and building a whole machine to learn its name would mean a GBA's 32 MiB of ROM in
/// memory per import.
///
/// The header is *parsed*, not merely searched for a title, so a truncated or corrupt dump is
/// reported here rather than being indexed as a playable game that fails the moment it is
/// clicked. Only the title is allowed to be absent — a header that reduces to no readable name
/// falls back to the file stem, which is never blank.
pub fn probe(path: &Path) -> Result<RomInfo, LoadError> {
    let platform = Platform::from_extension(path)
        .ok_or_else(|| LoadError::UnknownPlatform(path.to_owned()))?;
    let bytes = std::fs::read(path).map_err(|source| LoadError::Io {
        path: path.to_owned(),
        source,
    })?;
    let title = read_title(platform, &bytes)?.unwrap_or_else(|| {
        path.file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("Untitled")
            .to_string()
    });
    Ok(RomInfo {
        platform,
        title,
        size_bytes: bytes.len() as u64,
        content_hash: library::content_hash(&bytes),
    })
}

/// Parse the cartridge header and return the title it carries, if any.
///
/// `Err` means the header itself did not parse — the file is not a cartridge for this platform.
/// `Ok(None)` means it parsed and simply has no usable name, which is ordinary: headers are
/// routinely padded with spaces or zeroes, and homebrew often leaves the field empty.
pub fn read_title(platform: Platform, rom: &[u8]) -> Result<Option<String>, LoadError> {
    let raw = match platform {
        Platform::Gb | Platform::Gbc => cart_common::GbHeader::parse(rom)?.title,
        Platform::Gba => cart_common::GbaHeader::parse(rom)?.title,
        // The DS header has a twelve-byte game title at offset 0. Read directly rather than
        // pretending `cart-common` models a DS cartridge, which it does not.
        Platform::Nds => {
            const NDS_HEADER_LEN: usize = 0x200;
            if rom.len() < NDS_HEADER_LEN {
                return Err(CartridgeError::TooSmall {
                    len: rom.len(),
                    min: NDS_HEADER_LEN,
                }
                .into());
            }
            String::from_utf8_lossy(&rom[0x00..0x0C]).into_owned()
        }
    };
    let cleaned: String = raw
        .chars()
        .filter(|c| !c.is_control())
        .collect::<String>()
        .trim()
        .to_string();
    Ok((!cleaned.is_empty()).then_some(cleaned))
}

/// The title a header carries, or `None` for any reason at all.
///
/// For callers that have already built the machine and so know the header is valid — the session
/// wants a name for the window title and has no use for a second copy of an error it has already
/// passed.
pub fn header_title(platform: Platform, rom: &[u8]) -> Option<String> {
    read_title(platform, rom).ok().flatten()
}

/// Build the machine a ROM needs and install the ROM in it.
pub fn build_system(platform: Platform, rom: Vec<u8>) -> Result<Box<dyn System>, LoadError> {
    match platform {
        Platform::Gb => Ok(Box::new(system_gb::GbSystem::new(rom, None)?)),
        // A `.gbc` file gets colour *hardware*; whether that hardware runs in full colour or in
        // DMG-compatibility mode is decided by the cartridge header inside `GbcSystem`.
        Platform::Gbc => Ok(Box::new(system_gbc::GbcSystem::new(rom, None)?)),
        Platform::Gba => Ok(Box::new(system_gba::GbaSystem::new(rom, None)?)),
        // Named explicitly, so the message says which system is missing rather than leaving the
        // user to wonder whether their file is corrupt.
        Platform::Nds => Err(LoadError::NotImplemented("Nintendo DS")),
    }
}

/// Read a ROM from disk and build its machine.
pub fn load(path: &Path) -> Result<(Platform, Box<dyn System>), LoadError> {
    let platform = Platform::from_extension(path)
        .ok_or_else(|| LoadError::UnknownPlatform(path.to_owned()))?;
    let bytes = std::fs::read(path).map_err(|source| LoadError::Io {
        path: path.to_owned(),
        source,
    })?;
    let system = build_system(platform, bytes)?;
    Ok((platform, system))
}

/// The master clock and the cycles one video frame takes, per platform.
///
/// Written as the two numbers the hardware actually has rather than as a pre-divided rate, so
/// the derivation is checkable against a hardware reference instead of being a magic constant.
/// The Game Boy's 4.194304 MHz over 154 lines of 456 cycles gives 59.7275 Hz; the Game Boy
/// Advance's 16.777216 MHz over 228 lines of 1232 cycles gives the same, which is not a
/// coincidence — the GBA's video timing was chosen to match.
const fn clock_and_frame_cycles(platform: Platform) -> (u64, u64) {
    match platform {
        Platform::Gb | Platform::Gbc => (4_194_304, 154 * 456),
        Platform::Gba => (16_777_216, 228 * 1232),
        // 33.513982 MHz, 263 lines of 2130 cycles: 59.8261 Hz. Slightly faster than the Game
        // Boy family, which is why it is not folded in with them.
        Platform::Nds => (33_513_982, 263 * 2130),
    }
}

/// How long one frame of this platform lasts on real hardware.
pub fn frame_duration(platform: Platform) -> Duration {
    let (clock, cycles) = clock_and_frame_cycles(platform);
    Duration::from_nanos((cycles * 1_000_000_000) / clock)
}

/// Frames per second, for display.
pub fn frame_rate(platform: Platform) -> f64 {
    let (clock, cycles) = clock_and_frame_cycles(platform);
    clock as f64 / cycles as f64
}

/// The framebuffer size a platform produces, known before a ROM is loaded.
///
/// The window needs a size at startup, when there is no system to ask. The value is checked
/// against the real framebuffer once one exists — see [`crate::session`], which presents whatever
/// the system reports rather than what this function claimed.
///
/// The Nintendo DS is one framebuffer of two stacked screens: 256×192 twice, plus no gap. The
/// gap between the physical screens is a presentation choice and belongs in the frontend's
/// layout code, not in a framebuffer the emulation core has to leave holes in.
pub const fn screen_size(platform: Platform) -> (u32, u32) {
    match platform {
        Platform::Gb | Platform::Gbc => (160, 144),
        Platform::Gba => (240, 160),
        Platform::Nds => (256, 384),
    }
}

/// Whether this platform's framebuffer holds two stacked screens.
pub const fn is_dual_screen(platform: Platform) -> bool {
    matches!(platform, Platform::Nds)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_rates_match_documented_hardware() {
        // Both Game Boy generations and the GBA share 59.7275 Hz.
        for platform in [Platform::Gb, Platform::Gbc, Platform::Gba] {
            let hz = frame_rate(platform);
            assert!(
                (hz - 59.7275).abs() < 0.001,
                "{platform} runs at {hz} Hz, expected 59.7275"
            );
        }
        let nds = frame_rate(Platform::Nds);
        assert!((nds - 59.8261).abs() < 0.001, "DS runs at {nds} Hz");
    }

    #[test]
    fn a_frame_is_about_sixteen_and_three_quarter_milliseconds() {
        let gb = frame_duration(Platform::Gb);
        assert_eq!(gb.as_nanos(), 16_742_706);
        // Never zero, which would make the pacing loop spin.
        for platform in Platform::ALL {
            assert!(frame_duration(*platform).as_nanos() > 1_000_000);
        }
    }

    #[test]
    fn an_unrecognised_extension_names_the_ones_that_work() {
        // `Box<dyn System>` is not `Debug`, so the success arm cannot be unwrapped for a message.
        let Err(err) = load(Path::new("/nowhere/save.zip")) else {
            panic!("a .zip is not a ROM");
        };
        let text = err.to_string();
        assert!(text.contains(".gba"), "unhelpful message: {text}");
    }

    #[test]
    fn a_ds_rom_is_refused_by_name_not_as_a_corrupt_file() {
        let Err(err) = build_system(Platform::Nds, vec![0; 4096]) else {
            panic!("the Nintendo DS is not assembled");
        };
        assert!(
            matches!(err, LoadError::NotImplemented("Nintendo DS")),
            "got {err}"
        );
    }

    #[test]
    fn a_blank_header_title_falls_back_rather_than_returning_an_empty_string() {
        // A GB header of spaces has a syntactically valid but useless title.
        let mut rom = vec![0u8; 0x8000];
        rom[0x0134..0x0144].fill(b' ');
        rom[0x0147] = 0x00; // ROM only
        rom[0x0148] = 0x00; // 32 KiB, matching the vector above
        assert_eq!(header_title(Platform::Gb, &rom), None);
    }

    #[test]
    fn only_the_ds_is_dual_screen() {
        assert!(is_dual_screen(Platform::Nds));
        for platform in [Platform::Gb, Platform::Gbc, Platform::Gba] {
            assert!(!is_dual_screen(platform));
        }
    }

    #[test]
    fn declared_screen_sizes_match_what_the_systems_actually_produce() {
        // The window is sized from `screen_size` before any ROM exists, so a mismatch would show
        // as a one-frame resize on every launch. Assert against the real thing.
        let gb = system_gb::GbSystem::new(minimal_gb_rom(), None).unwrap();
        let fb = core_common::System::framebuffer(&gb);
        assert_eq!(screen_size(Platform::Gb), (fb.width(), fb.height()));

        let gba = system_gba::GbaSystem::new(vec![0u8; 0x8000], None).unwrap();
        let fb = core_common::System::framebuffer(&gba);
        assert_eq!(screen_size(Platform::Gba), (fb.width(), fb.height()));
    }

    /// The smallest byte sequence `GbHeader::parse` accepts, built rather than fetched — no
    /// commercial ROM is involved in any test in this workspace.
    fn minimal_gb_rom() -> Vec<u8> {
        let mut rom = vec![0u8; 0x8000];
        rom[0x0134..0x0140].copy_from_slice(b"TESTCART\0\0\0\0");
        rom[0x0147] = 0x00;
        rom[0x0148] = 0x00;
        rom[0x014D] = cart_common::GbHeader::header_checksum(&rom);
        rom
    }
}
