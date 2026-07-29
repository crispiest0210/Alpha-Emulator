//! The Game Pak: ROM in three windows, and a save chip.
//!
//! # One ROM, three addresses
//!
//! `0x08000000`, `0x0A000000`, and `0x0C000000` are the *same* cartridge seen through three
//! independently timed windows. A game links its code into whichever suits and can read its own
//! data through another at a different speed. Mirroring them onto one image is not a
//! simplification — it is what the hardware does.
//!
//! # The save chip is not detected from the header
//!
//! A GBA header says nothing about which save chip is fitted. The convention every emulator
//! follows is to search the ROM image for the string the manufacturer's library leaves in it —
//! `SRAM_V`, `FLASH_V`, `EEPROM_V` — because that library is what actually talks to the chip.
//! It is a heuristic, and it is the only signal there is.

use cart_common::{BatteryBackedSave, Flash, GbaHeader, SaveKind, Sram};
use core_common::CartridgeError;
use core_common::{Savable, StateError, StateReader, StateWriter};

/// Largest cartridge the address space can hold: 32 MiB per window.
pub const MAX_ROM: usize = 0x0200_0000;

pub struct Cartridge {
    rom: Box<[u8]>,
    save: Option<Box<dyn BatteryBackedSave>>,
    pub header: GbaHeader,
}

impl Cartridge {
    pub fn new(rom: Vec<u8>) -> Result<Self, CartridgeError> {
        let header = GbaHeader::parse(&rom)?;
        let save = create_save(&rom);
        Ok(Self {
            rom: rom.into_boxed_slice(),
            save,
            header,
        })
    }

    pub fn save_kind(&self) -> SaveKind {
        self.save.as_ref().map_or(SaveKind::None, |s| s.kind())
    }

    pub fn battery_save(&self) -> Option<&dyn BatteryBackedSave> {
        self.save.as_deref()
    }

    pub fn battery_save_mut(&mut self) -> Option<&mut (dyn BatteryBackedSave + 'static)> {
        self.save.as_deref_mut()
    }

    /// Read a byte of ROM through any of the three windows.
    ///
    /// Past the end of the image, a cartridge returns a value derived from the address rather
    /// than zero — the bus floats and settles to the low half of the halfword index. Games
    /// with a small ROM read past the end during startup, and returning zero makes some of them
    /// mistake it for valid data.
    pub fn read_rom(&self, addr: u32) -> u8 {
        let offset = (addr & 0x01FF_FFFF) as usize;
        match self.rom.get(offset) {
            Some(&byte) => byte,
            None => {
                let halfword = (offset >> 1) as u16;
                if offset & 1 == 0 {
                    halfword as u8
                } else {
                    (halfword >> 8) as u8
                }
            }
        }
    }

    pub fn read_save(&mut self, addr: u32) -> u8 {
        match &mut self.save {
            Some(save) => save.read_byte(addr & 0xFFFF),
            // No chip fitted reads as all ones, which is what a floating bus settles to here.
            None => 0xFF,
        }
    }

    pub fn write_save(&mut self, addr: u32, value: u8) {
        if let Some(save) = &mut self.save {
            save.write_byte(addr & 0xFFFF, value);
        }
    }
}

/// Guess the save chip by looking for the manufacturer library's signature in the ROM.
///
/// EEPROM is deliberately not created even when its string is found: it is addressed over a
/// serial protocol through the top of the ROM window rather than as a memory, and wiring that
/// up is separate work. Returning `None` there is honest — a game will find no save rather than
/// a save that silently misbehaves.
fn create_save(rom: &[u8]) -> Option<Box<dyn BatteryBackedSave>> {
    let find = |needle: &[u8]| rom.windows(needle.len()).any(|w| w == needle);

    if find(b"SRAM_V") || find(b"SRAM_F_V") {
        return Some(Box::new(Sram::new(0x8000)));
    }
    if find(b"FLASH1M_V") {
        return Some(Box::new(Flash::new(0x2_0000)));
    }
    if find(b"FLASH_V") || find(b"FLASH512_V") {
        return Some(Box::new(Flash::new(0x1_0000)));
    }
    None
}

impl Savable for Cartridge {
    fn save(&self, w: &mut StateWriter) {
        // The ROM is not written: it is the file the user supplied and is identical across runs.
        match &self.save {
            Some(save) => {
                w.write_bool(true);
                save.save(w);
            }
            None => w.write_bool(false),
        }
    }

    fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        let present = r.read_bool()?;
        match (&mut self.save, present) {
            (Some(save), true) => save.load(r),
            (None, false) => Ok(()),
            // A state from a machine with a different cartridge fitted. Refused rather than
            // partially applied: the alternative is a save chip half-filled with another
            // game's data.
            _ => Err(StateError::Malformed(
                "the save state was taken with a different save chip fitted".into(),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A ROM with a valid header and an optional library signature.
    fn rom_with(signature: Option<&[u8]>) -> Vec<u8> {
        let mut rom = vec![0u8; 0x1000];
        // The header's fixed logo checksum byte is all `GbaHeader::parse` insists on beyond
        // length, so the rest can stay zero.
        if let Some(signature) = signature {
            rom[0x800..0x800 + signature.len()].copy_from_slice(signature);
        }
        rom
    }

    #[test]
    fn the_three_rom_windows_are_the_same_cartridge() {
        // Not a simplification: a game links code into one window and reads data through
        // another at a different speed.
        let mut rom = rom_with(None);
        rom[0x100] = 0x42;
        let cart = Cartridge::new(rom).unwrap();
        assert_eq!(cart.read_rom(0x0800_0100), 0x42);
        assert_eq!(cart.read_rom(0x0A00_0100), 0x42);
        assert_eq!(cart.read_rom(0x0C00_0100), 0x42);
    }

    #[test]
    fn reading_past_the_end_returns_the_address_rather_than_zero() {
        // Games with a small ROM read past the end during startup, and zero looks like valid
        // data to some of them.
        let cart = Cartridge::new(rom_with(None)).unwrap();
        assert_eq!(cart.read_rom(0x0800_2000), 0x00);
        assert_eq!(cart.read_rom(0x0800_2001), 0x10, "halfword index 0x1000");
        assert_eq!(cart.read_rom(0x0800_2002), 0x01);
    }

    #[test]
    fn the_save_chip_is_found_by_searching_the_rom_for_a_library_signature() {
        // The header says nothing about it; the manufacturer's library string is the only
        // signal there is.
        let cart = Cartridge::new(rom_with(Some(b"SRAM_V113"))).unwrap();
        assert_eq!(cart.save_kind(), SaveKind::Sram { size: 0x8000 });

        let cart = Cartridge::new(rom_with(Some(b"FLASH_V126"))).unwrap();
        assert_eq!(cart.save_kind(), SaveKind::Flash { size: 0x1_0000 });

        let cart = Cartridge::new(rom_with(Some(b"FLASH1M_V102"))).unwrap();
        assert_eq!(cart.save_kind(), SaveKind::Flash { size: 0x2_0000 });
    }

    #[test]
    fn a_cartridge_with_no_signature_has_no_save_chip() {
        let cart = Cartridge::new(rom_with(None)).unwrap();
        assert_eq!(cart.save_kind(), SaveKind::None);
    }

    #[test]
    fn eeprom_is_reported_as_absent_rather_than_as_a_chip_that_misbehaves() {
        // It is addressed over a serial protocol through the top of the ROM window rather than
        // as a memory. A game finding no save is better than one finding a broken save.
        let cart = Cartridge::new(rom_with(Some(b"EEPROM_V122"))).unwrap();
        assert_eq!(cart.save_kind(), SaveKind::None);
    }

    #[test]
    fn a_missing_save_chip_reads_as_all_ones() {
        let mut cart = Cartridge::new(rom_with(None)).unwrap();
        cart.write_save(0x0E00_0000, 0x42);
        assert_eq!(cart.read_save(0x0E00_0000), 0xFF, "a floating bus");
    }

    #[test]
    fn a_save_chip_holds_what_is_written_to_it() {
        let mut cart = Cartridge::new(rom_with(Some(b"SRAM_V113"))).unwrap();
        cart.write_save(0x0E00_0010, 0x42);
        assert_eq!(cart.read_save(0x0E00_0010), 0x42);
    }

    #[test]
    fn a_rom_too_short_to_hold_a_header_is_rejected() {
        assert!(Cartridge::new(vec![0u8; 8]).is_err());
    }

    #[test]
    fn save_ram_round_trips_without_carrying_the_rom() {
        use savestate::{decode_state, encode_state};
        let mut cart = Cartridge::new(rom_with(Some(b"SRAM_V113"))).unwrap();
        cart.write_save(0x0E00_0004, 0x99);

        let bytes = encode_state("gba-cart", 1, &cart);
        assert!(bytes.len() < 0x8000 + 64, "the ROM did not travel with it");

        let mut restored = Cartridge::new(rom_with(Some(b"SRAM_V113"))).unwrap();
        decode_state("gba-cart", 1, &bytes, &mut restored).unwrap();
        assert_eq!(restored.read_save(0x0E00_0004), 0x99);
    }

    #[test]
    fn a_state_from_a_differently_equipped_cartridge_is_refused() {
        // Refused rather than partially applied: the alternative is a save chip half-filled
        // with another game's data.
        use savestate::{decode_state, encode_state};
        let with_save = Cartridge::new(rom_with(Some(b"SRAM_V113"))).unwrap();
        let bytes = encode_state("gba-cart", 1, &with_save);

        let mut without = Cartridge::new(rom_with(None)).unwrap();
        assert!(decode_state("gba-cart", 1, &bytes, &mut without).is_err());
    }
}
