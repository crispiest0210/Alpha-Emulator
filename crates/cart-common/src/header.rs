//! Cartridge header parsing.
//!
//! Field offsets and encodings are taken from Pan Docs (Game Boy) and GBATEK (GBA).

use core_common::CartridgeError;

/// Which memory bank controller a Game Boy cartridge uses.
///
/// # Explicitly out of scope for v1
///
/// `MBC6` (one game), `MBC7` (accelerometer, two games), `HuC1`/`HuC3`, `MMM01`, `TAMA5`, and
/// the `MBC1M` multicart variant. Each is a handful of titles at most, and none exercises the
/// shared abstractions in a way the supported five do not. Loading one produces
/// [`CartridgeError::UnsupportedMapper`] naming the type, rather than a mapper that silently
/// misbehaves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MapperKind {
    None,
    Mbc1,
    Mbc2,
    Mbc3,
    Mbc5,
}

impl MapperKind {
    pub const fn name(self) -> &'static str {
        match self {
            MapperKind::None => "ROM only",
            MapperKind::Mbc1 => "MBC1",
            MapperKind::Mbc2 => "MBC2",
            MapperKind::Mbc3 => "MBC3",
            MapperKind::Mbc5 => "MBC5",
        }
    }
}

/// How a cartridge relates to Game Boy Color hardware.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CgbSupport {
    /// Monochrome only.
    None,
    /// Uses CGB features but still runs on a DMG.
    Enhanced,
    /// Refuses to run on a DMG.
    Required,
}

/// A parsed Game Boy / Game Boy Color cartridge header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GbHeader {
    pub title: String,
    pub mapper: MapperKind,
    pub rom_banks: usize,
    /// Cartridge RAM size in bytes. Zero when the cartridge has none.
    ///
    /// MBC2 reports zero here even though it has 512 half-bytes on the controller itself —
    /// that memory is internal to the MBC and not described by the RAM-size byte.
    pub ram_size: usize,
    pub has_battery: bool,
    pub has_rtc: bool,
    pub has_rumble: bool,
    pub cgb: CgbSupport,
    /// Whether the byte at `0x014D` matches the header's contents.
    ///
    /// Recorded rather than enforced: the boot ROM refuses to run a cartridge that fails this,
    /// but homebrew and ROM hacks routinely ship with it wrong, and refusing to load them
    /// would be unhelpful. The system decides what to do with it.
    pub header_checksum_valid: bool,
    pub cartridge_type_byte: u8,
}

/// The smallest legal Game Boy ROM: one 16 KiB bank, which must at least contain the header.
const MIN_ROM_LEN: usize = 0x4000;
const HEADER_TITLE: std::ops::Range<usize> = 0x0134..0x0144;
const HEADER_CHECKSUM_RANGE: std::ops::Range<usize> = 0x0134..0x014D;

impl GbHeader {
    pub fn parse(rom: &[u8]) -> Result<Self, CartridgeError> {
        if rom.len() < MIN_ROM_LEN {
            return Err(CartridgeError::TooSmall {
                len: rom.len(),
                min: MIN_ROM_LEN,
            });
        }

        let cartridge_type_byte = rom[0x0147];
        let (mapper, has_ram, has_battery, has_rtc, has_rumble) =
            Self::decode_cartridge_type(cartridge_type_byte)?;

        // The ROM-size byte counts doublings from 32 KiB, i.e. two 16 KiB banks.
        let rom_size_byte = rom[0x0148];
        if rom_size_byte > 0x08 {
            return Err(CartridgeError::BadHeader(format!(
                "unsupported ROM size code {rom_size_byte:#04X}"
            )));
        }
        let rom_banks = 2usize << rom_size_byte;

        let ram_size = if has_ram {
            match rom[0x0149] {
                0x00 => 0,
                // 0x01 is 2 KiB in some documentation and unused in practice; treat it as a
                // single 8 KiB bank, which is what real cartridges using it behave like.
                0x01 | 0x02 => 8 * 1024,
                0x03 => 32 * 1024,
                0x04 => 128 * 1024,
                0x05 => 64 * 1024,
                other => {
                    return Err(CartridgeError::BadHeader(format!(
                        "unsupported RAM size code {other:#04X}"
                    )))
                }
            }
        } else {
            0
        };

        let cgb = match rom[0x0143] {
            0x80 => CgbSupport::Enhanced,
            0xC0 => CgbSupport::Required,
            _ => CgbSupport::None,
        };

        // The title field ran into the CGB flag and manufacturer code over time, so stop at
        // the first NUL and drop anything non-printable rather than trusting all 16 bytes.
        let title = rom[HEADER_TITLE]
            .iter()
            .take_while(|&&b| b != 0)
            .filter(|&&b| b.is_ascii_graphic() || b == b' ')
            .map(|&b| b as char)
            .collect::<String>()
            .trim_end()
            .to_string();

        Ok(Self {
            title,
            mapper,
            rom_banks,
            ram_size,
            has_battery,
            has_rtc,
            has_rumble,
            cgb,
            header_checksum_valid: Self::header_checksum(rom) == rom[0x014D],
            cartridge_type_byte,
        })
    }

    /// The `0x0147` cartridge-type byte, per Pan Docs.
    #[allow(clippy::type_complexity)]
    fn decode_cartridge_type(
        byte: u8,
    ) -> Result<(MapperKind, bool, bool, bool, bool), CartridgeError> {
        // (mapper, ram, battery, rtc, rumble)
        let decoded = match byte {
            0x00 => (MapperKind::None, false, false, false, false),
            0x08 => (MapperKind::None, true, false, false, false),
            0x09 => (MapperKind::None, true, true, false, false),

            0x01 => (MapperKind::Mbc1, false, false, false, false),
            0x02 => (MapperKind::Mbc1, true, false, false, false),
            0x03 => (MapperKind::Mbc1, true, true, false, false),

            // MBC2's memory is on the controller, so these do not set the RAM flag; the
            // battery still backs it.
            0x05 => (MapperKind::Mbc2, false, false, false, false),
            0x06 => (MapperKind::Mbc2, false, true, false, false),

            0x0F => (MapperKind::Mbc3, false, true, true, false),
            0x10 => (MapperKind::Mbc3, true, true, true, false),
            0x11 => (MapperKind::Mbc3, false, false, false, false),
            0x12 => (MapperKind::Mbc3, true, false, false, false),
            0x13 => (MapperKind::Mbc3, true, true, false, false),

            0x19 => (MapperKind::Mbc5, false, false, false, false),
            0x1A => (MapperKind::Mbc5, true, false, false, false),
            0x1B => (MapperKind::Mbc5, true, true, false, false),
            0x1C => (MapperKind::Mbc5, false, false, false, true),
            0x1D => (MapperKind::Mbc5, true, false, false, true),
            0x1E => (MapperKind::Mbc5, true, true, false, true),

            other => {
                return Err(CartridgeError::UnsupportedMapper {
                    code: other,
                    name: Self::name_unsupported(other).to_string(),
                })
            }
        };
        Ok(decoded)
    }

    /// Name the mappers this build deliberately does not support, so the error says what the
    /// cartridge actually is instead of just a hex byte.
    fn name_unsupported(byte: u8) -> &'static str {
        match byte {
            0x0B..=0x0D => "MMM01",
            0x20 => "MBC6",
            0x22 => "MBC7 (accelerometer)",
            0xFC => "POCKET CAMERA",
            0xFD => "BANDAI TAMA5",
            0xFE => "HuC3",
            0xFF => "HuC1",
            _ => "unknown",
        }
    }

    /// `x = 0; for each byte in 0x0134..=0x014C: x = x - byte - 1`.
    pub fn header_checksum(rom: &[u8]) -> u8 {
        rom[HEADER_CHECKSUM_RANGE]
            .iter()
            .fold(0u8, |acc, &b| acc.wrapping_sub(b).wrapping_sub(1))
    }

    /// Whether the ROM's length matches what its header claims.
    pub fn rom_size_matches(&self, rom_len: usize) -> bool {
        rom_len == self.rom_banks * 0x4000
    }

    pub fn describe(&self) -> String {
        let mut parts = vec![self.mapper.name().to_string()];
        if self.ram_size > 0 {
            parts.push("RAM".into());
        }
        if self.has_battery {
            parts.push("Battery".into());
        }
        if self.has_rtc {
            parts.push("RTC".into());
        }
        if self.has_rumble {
            parts.push("Rumble".into());
        }
        parts.join(" + ")
    }
}

/// A parsed Game Boy Advance cartridge header.
///
/// The GBA has no bank controller — ROM is flat and memory-mapped — so this carries no mapper
/// field. The only cartridge hardware that varies is the save chip, and its type is not in the
/// header at all: it is inferred by [`GbaHeader::detect_save_kind`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GbaHeader {
    pub title: String,
    /// Four-character game code, e.g. `"AXVE"`.
    pub game_code: String,
    pub maker_code: String,
    pub software_version: u8,
    pub header_checksum_valid: bool,
}

const GBA_MIN_ROM_LEN: usize = 0xC0;

impl GbaHeader {
    pub fn parse(rom: &[u8]) -> Result<Self, CartridgeError> {
        if rom.len() < GBA_MIN_ROM_LEN {
            return Err(CartridgeError::TooSmall {
                len: rom.len(),
                min: GBA_MIN_ROM_LEN,
            });
        }

        let ascii = |range: std::ops::Range<usize>| {
            rom[range]
                .iter()
                .take_while(|&&b| b != 0)
                .filter(|&&b| b.is_ascii_graphic() || b == b' ')
                .map(|&b| b as char)
                .collect::<String>()
                .trim_end()
                .to_string()
        };

        // The header checksum covers 0xA0..=0xBC: x = 0; x = x - byte; then x = x - 0x19.
        let sum = rom[0xA0..=0xBC]
            .iter()
            .fold(0u8, |acc, &b| acc.wrapping_sub(b))
            .wrapping_sub(0x19);

        Ok(Self {
            title: ascii(0xA0..0xAC),
            game_code: ascii(0xAC..0xB0),
            maker_code: ascii(0xB0..0xB2),
            software_version: rom[0xBC],
            header_checksum_valid: sum == rom[0xBD],
        })
    }

    /// Infer the save chip by scanning the ROM for the library ID string the cartridge's own
    /// save code contains.
    ///
    /// This is genuinely how it is done: the save type is nowhere in the header, and every
    /// commercial GBA game links one of Nintendo's save libraries, each of which leaves a
    /// recognizable marker in the binary. The alternative is a per-title database, which is
    /// worse — it needs maintaining and fails on homebrew entirely.
    ///
    /// Returns [`SaveKind::None`] when no marker is found, which is correct for the games
    /// that genuinely have no save chip.
    pub fn detect_save_kind(rom: &[u8]) -> crate::SaveKind {
        use crate::SaveKind;
        // Longest markers first: "FLASH1M_V" also contains "FLASH".
        const MARKERS: &[(&[u8], SaveKind)] = &[
            (b"EEPROM_V", SaveKind::Eeprom { size: 8 * 1024 }),
            (b"FLASH1M_V", SaveKind::Flash { size: 128 * 1024 }),
            (b"FLASH512_V", SaveKind::Flash { size: 64 * 1024 }),
            (b"FLASH_V", SaveKind::Flash { size: 64 * 1024 }),
            (b"SRAM_V", SaveKind::Sram { size: 32 * 1024 }),
            (b"SRAM_F_V", SaveKind::Sram { size: 32 * 1024 }),
        ];

        // A plain scan of every offset. This runs once at load time, so the simpler correct
        // version beats an alignment assumption that would silently miss a marker.
        for (marker, kind) in MARKERS {
            if rom.windows(marker.len()).any(|window| window == *marker) {
                return *kind;
            }
        }
        SaveKind::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal but structurally valid Game Boy ROM.
    fn gb_rom(cartridge_type: u8, rom_size: u8, ram_size: u8) -> Vec<u8> {
        let banks = 2usize << rom_size;
        let mut rom = vec![0u8; banks * 0x4000];
        rom[0x0134..0x0134 + 5].copy_from_slice(b"TEST\0");
        rom[0x0147] = cartridge_type;
        rom[0x0148] = rom_size;
        rom[0x0149] = ram_size;
        rom[0x014D] = GbHeader::header_checksum(&rom);
        rom
    }

    #[test]
    fn parses_a_plain_rom_only_cartridge() {
        let header = GbHeader::parse(&gb_rom(0x00, 0x00, 0x00)).unwrap();
        assert_eq!(header.title, "TEST");
        assert_eq!(header.mapper, MapperKind::None);
        assert_eq!(header.rom_banks, 2);
        assert_eq!(header.ram_size, 0);
        assert!(!header.has_battery);
        assert!(header.header_checksum_valid);
    }

    #[test]
    fn rom_size_code_counts_doublings_from_two_banks() {
        for (code, banks) in [(0x00u8, 2usize), (0x01, 4), (0x05, 64), (0x08, 512)] {
            let header = GbHeader::parse(&gb_rom(0x00, code, 0x00)).unwrap();
            assert_eq!(header.rom_banks, banks, "code {code:#04X}");
            assert!(header.rom_size_matches(banks * 0x4000));
        }
    }

    #[test]
    fn ram_size_codes_decode_to_byte_counts() {
        for (code, bytes) in [
            (0x00u8, 0usize),
            (0x02, 8 * 1024),
            (0x03, 32 * 1024),
            (0x04, 128 * 1024),
            (0x05, 64 * 1024),
        ] {
            // Cartridge type 0x02 is MBC1+RAM, so the RAM byte is honored.
            let header = GbHeader::parse(&gb_rom(0x02, 0x00, code)).unwrap();
            assert_eq!(header.ram_size, bytes, "code {code:#04X}");
        }
    }

    #[test]
    fn cartridge_type_decodes_every_supported_feature_combination() {
        let cases: &[(u8, MapperKind, bool, bool, bool, bool)] = &[
            // (byte, mapper, ram, battery, rtc, rumble)
            (0x00, MapperKind::None, false, false, false, false),
            (0x03, MapperKind::Mbc1, true, true, false, false),
            (0x06, MapperKind::Mbc2, false, true, false, false),
            (0x0F, MapperKind::Mbc3, false, true, true, false),
            (0x10, MapperKind::Mbc3, true, true, true, false),
            (0x13, MapperKind::Mbc3, true, true, false, false),
            (0x1E, MapperKind::Mbc5, true, true, false, true),
        ];
        for &(byte, mapper, ram, battery, rtc, rumble) in cases {
            let header = GbHeader::parse(&gb_rom(byte, 0x00, 0x02)).unwrap();
            assert_eq!(header.mapper, mapper, "type {byte:#04X}");
            assert_eq!(header.ram_size > 0, ram, "type {byte:#04X} ram");
            assert_eq!(header.has_battery, battery, "type {byte:#04X} battery");
            assert_eq!(header.has_rtc, rtc, "type {byte:#04X} rtc");
            assert_eq!(header.has_rumble, rumble, "type {byte:#04X} rumble");
        }
    }

    #[test]
    fn mbc2_reports_no_cartridge_ram_because_its_memory_is_on_the_controller() {
        let header = GbHeader::parse(&gb_rom(0x06, 0x00, 0x03)).unwrap();
        assert_eq!(header.ram_size, 0);
        assert!(header.has_battery);
    }

    #[test]
    fn unsupported_mappers_are_named_in_the_error() {
        let err = GbHeader::parse(&gb_rom(0x22, 0x00, 0x00)).unwrap_err();
        match err {
            CartridgeError::UnsupportedMapper { code, name } => {
                assert_eq!(code, 0x22);
                assert!(name.contains("MBC7"), "{name}");
            }
            other => panic!("expected UnsupportedMapper, got {other:?}"),
        }
    }

    #[test]
    fn a_bad_header_checksum_is_reported_but_not_fatal() {
        // Homebrew and ROM hacks routinely ship with this wrong, so it must not block loading.
        let mut rom = gb_rom(0x00, 0x00, 0x00);
        rom[0x014D] ^= 0xFF;
        let header = GbHeader::parse(&rom).unwrap();
        assert!(!header.header_checksum_valid);
    }

    #[test]
    fn cgb_flag_distinguishes_enhanced_from_required() {
        let mut rom = gb_rom(0x00, 0x00, 0x00);
        assert_eq!(GbHeader::parse(&rom).unwrap().cgb, CgbSupport::None);
        rom[0x0143] = 0x80;
        assert_eq!(GbHeader::parse(&rom).unwrap().cgb, CgbSupport::Enhanced);
        rom[0x0143] = 0xC0;
        assert_eq!(GbHeader::parse(&rom).unwrap().cgb, CgbSupport::Required);
    }

    #[test]
    fn a_rom_too_short_to_hold_a_header_is_rejected() {
        assert!(matches!(
            GbHeader::parse(&[0u8; 100]),
            Err(CartridgeError::TooSmall { .. })
        ));
    }

    #[test]
    fn describes_itself_for_the_ui() {
        let header = GbHeader::parse(&gb_rom(0x1E, 0x00, 0x02)).unwrap();
        assert_eq!(header.describe(), "MBC5 + RAM + Battery + Rumble");
    }

    // -- GBA -----------------------------------------------------------------

    fn gba_rom(extra: &[u8]) -> Vec<u8> {
        let mut rom = vec![0u8; 0x1000];
        rom[0xA0..0xA0 + 6].copy_from_slice(b"MYGAME");
        rom[0xAC..0xB0].copy_from_slice(b"AXVE");
        rom[0xB0..0xB2].copy_from_slice(b"01");
        let sum = rom[0xA0..=0xBC]
            .iter()
            .fold(0u8, |acc, &b| acc.wrapping_sub(b))
            .wrapping_sub(0x19);
        rom[0xBD] = sum;
        rom[0x800..0x800 + extra.len()].copy_from_slice(extra);
        rom
    }

    #[test]
    fn parses_a_gba_header() {
        let header = GbaHeader::parse(&gba_rom(b"")).unwrap();
        assert_eq!(header.title, "MYGAME");
        assert_eq!(header.game_code, "AXVE");
        assert_eq!(header.maker_code, "01");
        assert!(header.header_checksum_valid);
    }

    #[test]
    fn gba_save_type_is_detected_from_the_library_marker_in_the_rom() {
        use crate::SaveKind;
        assert_eq!(
            GbaHeader::detect_save_kind(&gba_rom(b"SRAM_V113")),
            SaveKind::Sram { size: 32 * 1024 }
        );
        assert_eq!(
            GbaHeader::detect_save_kind(&gba_rom(b"EEPROM_V122")),
            SaveKind::Eeprom { size: 8 * 1024 }
        );
        assert_eq!(
            GbaHeader::detect_save_kind(&gba_rom(b"FLASH1M_V102")),
            SaveKind::Flash { size: 128 * 1024 }
        );
        // A game with no save chip must not be given one.
        assert_eq!(GbaHeader::detect_save_kind(&gba_rom(b"")), SaveKind::None);
    }
}
