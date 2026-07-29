//! Game Boy memory bank controllers.
//!
//! Address decoding follows Pan Docs. The recurring shape is that writes into the ROM window
//! are register writes, split into 8 KiB sub-windows that each control something different.
//!
//! Every mapper here holds its ROM as a shared slice-free owned buffer and its cartridge RAM
//! as an [`Sram`], which is the [`BatteryBackedSave`] the frontend persists — no mapper
//! exposes its save bytes any other way.

use crate::{BatteryBackedSave, GbHeader, Mapper, MapperKind, Mbc3Rtc, Sram};
use core_common::{CartridgeError, Savable, StateError, StateReader, StateWriter};

const ROM_BANK_SIZE: usize = 0x4000;
const RAM_BANK_SIZE: usize = 0x2000;

/// Build the mapper a header describes.
pub fn create_mapper(rom: Vec<u8>, header: &GbHeader) -> Result<Box<dyn Mapper>, CartridgeError> {
    if !header.rom_size_matches(rom.len()) {
        return Err(CartridgeError::BadSize { len: rom.len() });
    }
    Ok(match header.mapper {
        MapperKind::None => Box::new(NoMbc::new(rom, header)),
        MapperKind::Mbc1 => Box::new(Mbc1::new(rom, header)),
        MapperKind::Mbc2 => Box::new(Mbc2::new(rom, header)),
        MapperKind::Mbc3 => Box::new(Mbc3::new(rom, header)),
        MapperKind::Mbc5 => Box::new(Mbc5::new(rom, header)),
    })
}

/// Storage every mapper needs: the ROM, the cartridge RAM, and the RAM enable latch.
#[derive(Debug)]
struct Common {
    rom: Vec<u8>,
    ram: Sram,
    /// Cartridge RAM is disabled at power-on and must be explicitly enabled. Games disable it
    /// again between accesses so a power-off mid-write cannot corrupt the save, and a mapper
    /// that ignores this lets a crashing game scribble over its own save file.
    ram_enabled: bool,
    description: String,
}

impl Common {
    fn new(rom: Vec<u8>, header: &GbHeader, ram_size: usize) -> Self {
        Self {
            rom,
            ram: Sram::new(ram_size),
            ram_enabled: false,
            description: header.describe(),
        }
    }

    #[inline]
    fn rom_banks(&self) -> usize {
        (self.rom.len() / ROM_BANK_SIZE).max(1)
    }

    /// Read from a ROM bank, wrapping the bank number the way real address decoding does.
    #[inline]
    fn read_rom(&self, bank: usize, offset: u16) -> u8 {
        let bank = bank % self.rom_banks();
        let index = bank * ROM_BANK_SIZE + (offset as usize & (ROM_BANK_SIZE - 1));
        self.rom.get(index).copied().unwrap_or(0xFF)
    }

    fn save(&self, w: &mut StateWriter) {
        // The ROM is not serialized: it comes from the cartridge file, not the machine, and
        // writing tens of megabytes into every save state would be absurd.
        self.ram.save(w);
        w.write_bool(self.ram_enabled);
    }

    fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        self.ram.load(r)?;
        self.ram_enabled = r.read_bool()?;
        Ok(())
    }
}

/// Cartridge RAM is enabled by writing a value whose low nibble is `0xA`.
#[inline]
fn is_ram_enable_value(value: u8) -> bool {
    value & 0x0F == 0x0A
}

// ---------------------------------------------------------------------------
// No MBC
// ---------------------------------------------------------------------------

/// A cartridge with no bank controller: 32 KiB of ROM, optionally 8 KiB of RAM.
#[derive(Debug)]
pub struct NoMbc {
    common: Common,
}

impl NoMbc {
    pub fn new(rom: Vec<u8>, header: &GbHeader) -> Self {
        Self {
            common: Common::new(rom, header, header.ram_size),
        }
    }
}

impl Mapper for NoMbc {
    /// ROM only. Bank selection is pure address arithmetic, so it is safe; cartridge RAM is not,
    /// because [`BatteryBackedSave::read_byte`] takes `&mut self` for the Flash and EEPROM chips
    /// whose reads really are commands. `None` there is the honest answer.
    fn peek(&self, addr: u16) -> Option<u8> {
        match addr {
            0x0000..=0x7FFF => Some(self.common.rom.get(addr as usize).copied().unwrap_or(0xFF)),
            _ => None,
        }
    }

    fn read(&mut self, addr: u16) -> u8 {
        match addr {
            0x0000..=0x7FFF => self.common.rom.get(addr as usize).copied().unwrap_or(0xFF),
            0xA000..=0xBFFF if self.common.ram.size() > 0 => {
                self.common.ram.read_byte((addr - 0xA000) as u32)
            }
            _ => 0xFF,
        }
    }

    fn write(&mut self, addr: u16, value: u8) {
        // There is no bank controller, so writes into the ROM window go nowhere at all.
        if let 0xA000..=0xBFFF = addr {
            if self.common.ram.size() > 0 {
                self.common.ram.write_byte((addr - 0xA000) as u32, value);
            }
        }
    }

    fn battery_save(&self) -> Option<&dyn BatteryBackedSave> {
        (self.common.ram.size() > 0).then_some(&self.common.ram as &dyn BatteryBackedSave)
    }

    fn battery_save_mut(&mut self) -> Option<&mut dyn BatteryBackedSave> {
        if self.common.ram.size() > 0 {
            Some(&mut self.common.ram as &mut dyn BatteryBackedSave)
        } else {
            None
        }
    }

    fn describe(&self) -> String {
        self.common.description.clone()
    }
}

impl Savable for NoMbc {
    fn save(&self, w: &mut StateWriter) {
        self.common.save(w);
    }
    fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        self.common.load(r)
    }
}

// ---------------------------------------------------------------------------
// MBC1
// ---------------------------------------------------------------------------

/// MBC1: up to 2 MiB of ROM or 32 KiB of RAM, but not both at full size.
///
/// The two-bit "bank 2" register is the interesting part: depending on the mode select it is
/// either the upper ROM bank bits or the RAM bank number, and in advanced mode it also
/// applies to the *fixed* bank at `0x0000`–`0x3FFF`. That last detail is what large multicart
/// and 1 MiB+ cartridges depend on, and it is the piece most often missed.
#[derive(Debug)]
pub struct Mbc1 {
    common: Common,
    /// The low five bits of the ROM bank number.
    bank1: u8,
    /// Two bits: upper ROM bank bits, or the RAM bank.
    bank2: u8,
    /// False: `bank2` only extends the ROM bank. True: it also selects the RAM bank and
    /// remaps the low ROM window.
    advanced_mode: bool,
}

impl Mbc1 {
    pub fn new(rom: Vec<u8>, header: &GbHeader) -> Self {
        Self {
            common: Common::new(rom, header, header.ram_size),
            // A zero bank1 always reads as one, so the register powers on as one.
            bank1: 1,
            bank2: 0,
            advanced_mode: false,
        }
    }

    /// The bank visible at `0x0000`–`0x3FFF`.
    fn low_bank(&self) -> usize {
        if self.advanced_mode {
            (self.bank2 as usize) << 5
        } else {
            0
        }
    }

    /// The bank visible at `0x4000`–`0x7FFF`.
    fn high_bank(&self) -> usize {
        ((self.bank2 as usize) << 5) | self.bank1 as usize
    }

    fn ram_bank(&self) -> usize {
        if self.advanced_mode {
            self.bank2 as usize
        } else {
            0
        }
    }

    fn ram_offset(&self, addr: u16) -> u32 {
        (self.ram_bank() * RAM_BANK_SIZE + (addr - 0xA000) as usize) as u32
    }
}

impl Mapper for Mbc1 {
    /// ROM only. Bank selection is pure address arithmetic, so it is safe; cartridge RAM is not,
    /// because [`BatteryBackedSave::read_byte`] takes `&mut self` for the Flash and EEPROM chips
    /// whose reads really are commands. `None` there is the honest answer.
    fn peek(&self, addr: u16) -> Option<u8> {
        match addr {
            0x0000..=0x3FFF => Some(self.common.read_rom(self.low_bank(), addr)),
            0x4000..=0x7FFF => Some(self.common.read_rom(self.high_bank(), addr)),
            _ => None,
        }
    }

    fn read(&mut self, addr: u16) -> u8 {
        match addr {
            0x0000..=0x3FFF => self.common.read_rom(self.low_bank(), addr),
            0x4000..=0x7FFF => self.common.read_rom(self.high_bank(), addr),
            // Disabled cartridge RAM falls through to open bus below, not to zero.
            0xA000..=0xBFFF if self.common.ram_enabled && self.common.ram.size() > 0 => {
                let offset = self.ram_offset(addr);
                self.common.ram.read_byte(offset)
            }
            _ => 0xFF,
        }
    }

    fn write(&mut self, addr: u16, value: u8) {
        match addr {
            0x0000..=0x1FFF => self.common.ram_enabled = is_ram_enable_value(value),
            0x2000..=0x3FFF => {
                // Only five bits, and zero reads as one — which is why bank 0x20 cannot be
                // selected at 0x4000 and shows up as 0x21 instead.
                let bank = value & 0x1F;
                self.bank1 = if bank == 0 { 1 } else { bank };
            }
            0x4000..=0x5FFF => self.bank2 = value & 0x03,
            0x6000..=0x7FFF => self.advanced_mode = value & 1 != 0,
            0xA000..=0xBFFF if self.common.ram_enabled && self.common.ram.size() > 0 => {
                let offset = self.ram_offset(addr);
                self.common.ram.write_byte(offset, value);
            }
            _ => {}
        }
    }

    fn battery_save(&self) -> Option<&dyn BatteryBackedSave> {
        (self.common.ram.size() > 0).then_some(&self.common.ram as &dyn BatteryBackedSave)
    }

    fn battery_save_mut(&mut self) -> Option<&mut dyn BatteryBackedSave> {
        if self.common.ram.size() > 0 {
            Some(&mut self.common.ram as &mut dyn BatteryBackedSave)
        } else {
            None
        }
    }

    fn describe(&self) -> String {
        self.common.description.clone()
    }
}

impl Savable for Mbc1 {
    fn save(&self, w: &mut StateWriter) {
        self.common.save(w);
        w.write_u8(self.bank1);
        w.write_u8(self.bank2);
        w.write_bool(self.advanced_mode);
    }
    fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        self.common.load(r)?;
        self.bank1 = r.read_u8()?;
        self.bank2 = r.read_u8()?;
        self.advanced_mode = r.read_bool()?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// MBC2
// ---------------------------------------------------------------------------

/// MBC2: 256 KiB of ROM and 512 four-bit values of RAM built into the controller.
///
/// Two things make it unlike the others: the RAM is nibbles, so the upper half of every byte
/// reads back as ones, and bit 8 of the *address* selects which register a write hits rather
/// than the address falling in a different sub-window.
#[derive(Debug)]
pub struct Mbc2 {
    common: Common,
    rom_bank: u8,
}

/// 512 nibbles, stored one per byte.
const MBC2_RAM_SIZE: usize = 512;

impl Mbc2 {
    pub fn new(rom: Vec<u8>, header: &GbHeader) -> Self {
        Self {
            // The header reports no cartridge RAM because this memory is on the controller.
            common: Common::new(rom, header, MBC2_RAM_SIZE),
            rom_bank: 1,
        }
    }
}

impl Mapper for Mbc2 {
    /// ROM only. Bank selection is pure address arithmetic, so it is safe; cartridge RAM is not,
    /// because [`BatteryBackedSave::read_byte`] takes `&mut self` for the Flash and EEPROM chips
    /// whose reads really are commands. `None` there is the honest answer.
    fn peek(&self, addr: u16) -> Option<u8> {
        match addr {
            0x0000..=0x3FFF => Some(self.common.read_rom(0, addr)),
            0x4000..=0x7FFF => Some(self.common.read_rom(self.rom_bank as usize, addr)),
            _ => None,
        }
    }

    fn read(&mut self, addr: u16) -> u8 {
        match addr {
            0x0000..=0x3FFF => self.common.read_rom(0, addr),
            0x4000..=0x7FFF => self.common.read_rom(self.rom_bank as usize, addr),
            0xA000..=0xBFFF => {
                if !self.common.ram_enabled {
                    return 0xFF;
                }
                // Only 512 addressable nibbles, mirrored through the whole 8 KiB window.
                let offset = ((addr - 0xA000) as usize % MBC2_RAM_SIZE) as u32;
                // The upper nibble is not physically present and reads as ones.
                0xF0 | (self.common.ram.read_byte(offset) & 0x0F)
            }
            _ => 0xFF,
        }
    }

    fn write(&mut self, addr: u16, value: u8) {
        match addr {
            0x0000..=0x3FFF => {
                // Bit 8 of the address, not the address range, picks the register.
                if addr & 0x0100 == 0 {
                    self.common.ram_enabled = is_ram_enable_value(value);
                } else {
                    let bank = value & 0x0F;
                    self.rom_bank = if bank == 0 { 1 } else { bank };
                }
            }
            0xA000..=0xBFFF if self.common.ram_enabled => {
                let offset = ((addr - 0xA000) as usize % MBC2_RAM_SIZE) as u32;
                self.common.ram.write_byte(offset, value & 0x0F);
            }
            _ => {}
        }
    }

    fn battery_save(&self) -> Option<&dyn BatteryBackedSave> {
        Some(&self.common.ram as &dyn BatteryBackedSave)
    }

    fn battery_save_mut(&mut self) -> Option<&mut dyn BatteryBackedSave> {
        Some(&mut self.common.ram as &mut dyn BatteryBackedSave)
    }

    fn describe(&self) -> String {
        self.common.description.clone()
    }
}

impl Savable for Mbc2 {
    fn save(&self, w: &mut StateWriter) {
        self.common.save(w);
        w.write_u8(self.rom_bank);
    }
    fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        self.common.load(r)?;
        self.rom_bank = r.read_u8()?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// MBC3
// ---------------------------------------------------------------------------

/// MBC3: 2 MiB of ROM, 32 KiB of RAM, and optionally a real-time clock.
///
/// The RAM bank register does double duty: values `0x00`–`0x03` pick a RAM bank, and
/// `0x08`–`0x0C` map an RTC register into the same window instead.
#[derive(Debug)]
pub struct Mbc3 {
    common: Common,
    rom_bank: u8,
    /// Either a RAM bank (`0x00`–`0x03`) or an RTC register (`0x08`–`0x0C`).
    ram_bank_or_rtc: u8,
    rtc: Option<Mbc3Rtc>,
}

impl Mbc3 {
    pub fn new(rom: Vec<u8>, header: &GbHeader) -> Self {
        Self {
            common: Common::new(rom, header, header.ram_size),
            rom_bank: 1,
            ram_bank_or_rtc: 0,
            rtc: header.has_rtc.then(Mbc3Rtc::new),
        }
    }

    fn rtc_register_selected(&self) -> Option<u8> {
        crate::rtc::mbc3_register::RANGE
            .contains(&self.ram_bank_or_rtc)
            .then_some(self.ram_bank_or_rtc)
    }

    fn ram_offset(&self, addr: u16) -> u32 {
        (self.ram_bank_or_rtc as usize * RAM_BANK_SIZE + (addr - 0xA000) as usize) as u32
    }
}

impl Mapper for Mbc3 {
    /// ROM only. Bank selection is pure address arithmetic, so it is safe; cartridge RAM is not,
    /// because [`BatteryBackedSave::read_byte`] takes `&mut self` for the Flash and EEPROM chips
    /// whose reads really are commands. `None` there is the honest answer.
    fn peek(&self, addr: u16) -> Option<u8> {
        match addr {
            0x0000..=0x3FFF => Some(self.common.read_rom(0, addr)),
            0x4000..=0x7FFF => Some(self.common.read_rom(self.rom_bank as usize, addr)),
            _ => None,
        }
    }

    fn read(&mut self, addr: u16) -> u8 {
        match addr {
            0x0000..=0x3FFF => self.common.read_rom(0, addr),
            0x4000..=0x7FFF => self.common.read_rom(self.rom_bank as usize, addr),
            0xA000..=0xBFFF => {
                if !self.common.ram_enabled {
                    return 0xFF;
                }
                if let Some(register) = self.rtc_register_selected() {
                    return match &self.rtc {
                        Some(rtc) => rtc.read_register(register),
                        None => 0xFF,
                    };
                }
                if self.common.ram.size() == 0 {
                    return 0xFF;
                }
                let offset = self.ram_offset(addr);
                self.common.ram.read_byte(offset)
            }
            _ => 0xFF,
        }
    }

    fn write(&mut self, addr: u16, value: u8) {
        match addr {
            // A single 8 KiB window enables both cartridge RAM and the RTC registers.
            0x0000..=0x1FFF => self.common.ram_enabled = is_ram_enable_value(value),
            0x2000..=0x3FFF => {
                // Seven bits here, unlike MBC1's five. Zero still reads as one.
                let bank = value & 0x7F;
                self.rom_bank = if bank == 0 { 1 } else { bank };
            }
            0x4000..=0x5FFF => self.ram_bank_or_rtc = value,
            0x6000..=0x7FFF => {
                if let Some(rtc) = &mut self.rtc {
                    rtc.write_latch(value);
                }
            }
            0xA000..=0xBFFF => {
                if !self.common.ram_enabled {
                    return;
                }
                if let Some(register) = self.rtc_register_selected() {
                    if let Some(rtc) = &mut self.rtc {
                        rtc.write_register(register, value);
                    }
                    return;
                }
                if self.common.ram.size() > 0 {
                    let offset = self.ram_offset(addr);
                    self.common.ram.write_byte(offset, value);
                }
            }
            _ => {}
        }
    }

    fn battery_save(&self) -> Option<&dyn BatteryBackedSave> {
        (self.common.ram.size() > 0).then_some(&self.common.ram as &dyn BatteryBackedSave)
    }

    fn battery_save_mut(&mut self) -> Option<&mut dyn BatteryBackedSave> {
        if self.common.ram.size() > 0 {
            Some(&mut self.common.ram as &mut dyn BatteryBackedSave)
        } else {
            None
        }
    }

    fn rtc(&self) -> Option<&Mbc3Rtc> {
        self.rtc.as_ref()
    }

    fn rtc_mut(&mut self) -> Option<&mut Mbc3Rtc> {
        self.rtc.as_mut()
    }

    fn tick(&mut self, cycles: u64, cycles_per_second: u64) {
        if let Some(rtc) = &mut self.rtc {
            rtc.tick(cycles, cycles_per_second);
        }
    }

    fn describe(&self) -> String {
        self.common.description.clone()
    }
}

impl Savable for Mbc3 {
    fn save(&self, w: &mut StateWriter) {
        self.common.save(w);
        w.write_u8(self.rom_bank);
        w.write_u8(self.ram_bank_or_rtc);
        // The RTC is its own Savable rather than bytes folded into save RAM, because it has
        // real semantics — a latch, a halt bit, a sub-second divider — that raw bytes lose.
        w.write_bool(self.rtc.is_some());
        if let Some(rtc) = &self.rtc {
            rtc.save(w);
        }
    }

    fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        self.common.load(r)?;
        self.rom_bank = r.read_u8()?;
        self.ram_bank_or_rtc = r.read_u8()?;
        if r.read_bool()? {
            let rtc = self.rtc.get_or_insert_with(Mbc3Rtc::new);
            rtc.load(r)?;
        } else {
            self.rtc = None;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// MBC5
// ---------------------------------------------------------------------------

/// MBC5: 8 MiB of ROM, 128 KiB of RAM, and optionally a rumble motor.
///
/// The ROM bank number is nine bits split across two registers, and unlike MBC1 and MBC3,
/// **bank 0 is genuinely selectable** at `0x4000` — a game can map bank 0 into both windows.
/// Applying the older "zero means one" rule here breaks that.
#[derive(Debug)]
pub struct Mbc5 {
    common: Common,
    rom_bank: u16,
    ram_bank: u8,
    has_rumble: bool,
    rumble_on: bool,
}

impl Mbc5 {
    pub fn new(rom: Vec<u8>, header: &GbHeader) -> Self {
        Self {
            common: Common::new(rom, header, header.ram_size),
            rom_bank: 1,
            ram_bank: 0,
            has_rumble: header.has_rumble,
            rumble_on: false,
        }
    }

    fn ram_offset(&self, addr: u16) -> u32 {
        (self.ram_bank as usize * RAM_BANK_SIZE + (addr - 0xA000) as usize) as u32
    }
}

impl Mapper for Mbc5 {
    /// ROM only. Bank selection is pure address arithmetic, so it is safe; cartridge RAM is not,
    /// because [`BatteryBackedSave::read_byte`] takes `&mut self` for the Flash and EEPROM chips
    /// whose reads really are commands. `None` there is the honest answer.
    fn peek(&self, addr: u16) -> Option<u8> {
        match addr {
            0x0000..=0x3FFF => Some(self.common.read_rom(0, addr)),
            0x4000..=0x7FFF => Some(self.common.read_rom(self.rom_bank as usize, addr)),
            _ => None,
        }
    }

    fn read(&mut self, addr: u16) -> u8 {
        match addr {
            0x0000..=0x3FFF => self.common.read_rom(0, addr),
            0x4000..=0x7FFF => self.common.read_rom(self.rom_bank as usize, addr),
            0xA000..=0xBFFF if self.common.ram_enabled && self.common.ram.size() > 0 => {
                let offset = self.ram_offset(addr);
                self.common.ram.read_byte(offset)
            }
            _ => 0xFF,
        }
    }

    fn write(&mut self, addr: u16, value: u8) {
        match addr {
            0x0000..=0x1FFF => self.common.ram_enabled = is_ram_enable_value(value),
            // The bank number is split: eight bits here, the ninth in the next window.
            0x2000..=0x2FFF => self.rom_bank = (self.rom_bank & 0x100) | value as u16,
            0x3000..=0x3FFF => {
                self.rom_bank = (self.rom_bank & 0x0FF) | (((value & 1) as u16) << 8)
            }
            0x4000..=0x5FFF => {
                if self.has_rumble {
                    // On a rumble cartridge bit 3 drives the motor instead of the bank.
                    self.rumble_on = value & 0x08 != 0;
                    self.ram_bank = value & 0x07;
                } else {
                    self.ram_bank = value & 0x0F;
                }
            }
            0xA000..=0xBFFF if self.common.ram_enabled && self.common.ram.size() > 0 => {
                let offset = self.ram_offset(addr);
                self.common.ram.write_byte(offset, value);
            }
            _ => {}
        }
    }

    fn battery_save(&self) -> Option<&dyn BatteryBackedSave> {
        (self.common.ram.size() > 0).then_some(&self.common.ram as &dyn BatteryBackedSave)
    }

    fn battery_save_mut(&mut self) -> Option<&mut dyn BatteryBackedSave> {
        if self.common.ram.size() > 0 {
            Some(&mut self.common.ram as &mut dyn BatteryBackedSave)
        } else {
            None
        }
    }

    fn rumble(&self) -> bool {
        self.rumble_on
    }

    fn describe(&self) -> String {
        self.common.description.clone()
    }
}

impl Savable for Mbc5 {
    fn save(&self, w: &mut StateWriter) {
        self.common.save(w);
        w.write_u16(self.rom_bank);
        w.write_u8(self.ram_bank);
        w.write_bool(self.rumble_on);
    }
    fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        self.common.load(r)?;
        self.rom_bank = r.read_u16()?;
        self.ram_bank = r.read_u8()?;
        self.rumble_on = r.read_bool()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rtc::mbc3_register;

    /// Build a ROM whose every bank is filled with its own bank number, so a read tells you
    /// exactly which bank answered.
    fn banked_rom(cartridge_type: u8, rom_size_code: u8, ram_size_code: u8) -> (Vec<u8>, GbHeader) {
        let banks = 2usize << rom_size_code;
        let mut rom = vec![0u8; banks * ROM_BANK_SIZE];
        for bank in 0..banks {
            let marker = bank as u8;
            for byte in rom[bank * ROM_BANK_SIZE..(bank + 1) * ROM_BANK_SIZE].iter_mut() {
                *byte = marker;
            }
        }
        rom[0x0134..0x0139].copy_from_slice(b"TEST\0");
        rom[0x0147] = cartridge_type;
        rom[0x0148] = rom_size_code;
        rom[0x0149] = ram_size_code;
        rom[0x014D] = GbHeader::header_checksum(&rom);

        // Restore the bank markers the header overwrote so bank 0 still reads as 0 elsewhere.
        let header = GbHeader::parse(&rom).unwrap();
        (rom, header)
    }

    fn build(cartridge_type: u8, rom_size_code: u8, ram_size_code: u8) -> Box<dyn Mapper> {
        let (rom, header) = banked_rom(cartridge_type, rom_size_code, ram_size_code);
        create_mapper(rom, &header).unwrap()
    }

    /// Which bank is currently visible at `0x4000`.
    fn high_bank(mapper: &mut dyn Mapper) -> u8 {
        mapper.read(0x4000)
    }

    fn low_bank(mapper: &mut dyn Mapper) -> u8 {
        // Read past the header so the marker byte is intact.
        mapper.read(0x2000)
    }

    fn enable_ram(mapper: &mut dyn Mapper) {
        mapper.write(0x0000, 0x0A);
    }

    // -- No MBC --------------------------------------------------------------

    #[test]
    fn rom_only_cartridges_ignore_bank_writes() {
        let mut mapper = build(0x00, 0x00, 0x00);
        assert_eq!(low_bank(&mut *mapper), 0);
        assert_eq!(high_bank(&mut *mapper), 1);

        mapper.write(0x2000, 0x01);
        assert_eq!(high_bank(&mut *mapper), 1, "there is nothing to switch");
    }

    // -- MBC1 ----------------------------------------------------------------

    #[test]
    fn mbc1_switches_the_high_rom_window() {
        let mut mapper = build(0x01, 0x04, 0x00); // 32 banks
        for bank in 1..32u8 {
            mapper.write(0x2000, bank);
            assert_eq!(high_bank(&mut *mapper), bank, "bank {bank}");
        }
    }

    #[test]
    fn mbc1_maps_bank_zero_to_one_which_makes_some_banks_unreachable() {
        let mut mapper = build(0x01, 0x04, 0x00);
        mapper.write(0x2000, 0x00);
        assert_eq!(high_bank(&mut *mapper), 1, "bank 0 reads as bank 1");

        // The same rule makes 0x20, 0x40 and 0x60 unreachable on larger cartridges: the low
        // five bits are zero, so they become 0x21, 0x41 and 0x61.
        let mut mapper = build(0x01, 0x06, 0x00); // 128 banks
        mapper.write(0x4000, 0x01); // bank2 = 1
        mapper.write(0x2000, 0x00);
        assert_eq!(high_bank(&mut *mapper), 0x21);
    }

    #[test]
    fn mbc1_bank2_extends_the_rom_bank_number() {
        let mut mapper = build(0x01, 0x06, 0x00); // 128 banks
        mapper.write(0x2000, 0x05);
        mapper.write(0x4000, 0x02);
        assert_eq!(high_bank(&mut *mapper), (2 << 5) | 5);
    }

    #[test]
    fn mbc1_advanced_mode_also_remaps_the_low_window() {
        // The detail large cartridges depend on and most implementations miss.
        let mut mapper = build(0x01, 0x06, 0x00);
        mapper.write(0x4000, 0x02);
        assert_eq!(
            low_bank(&mut *mapper),
            0,
            "simple mode pins the low window to 0"
        );

        mapper.write(0x6000, 0x01); // advanced mode
        assert_eq!(
            low_bank(&mut *mapper),
            0x40,
            "bank2 now applies to both windows"
        );
    }

    #[test]
    fn mbc1_ram_banking_only_applies_in_advanced_mode() {
        let mut mapper = build(0x03, 0x00, 0x03); // MBC1+RAM+battery, 32 KiB
        enable_ram(&mut *mapper);

        mapper.write(0x6000, 0x01); // advanced mode
        for bank in 0..4u8 {
            mapper.write(0x4000, bank);
            mapper.write(0xA000, 0x10 + bank);
        }
        for bank in 0..4u8 {
            mapper.write(0x4000, bank);
            assert_eq!(mapper.read(0xA000), 0x10 + bank, "ram bank {bank}");
        }

        // In simple mode every bank select collapses onto bank 0.
        mapper.write(0x6000, 0x00);
        mapper.write(0x4000, 0x03);
        assert_eq!(mapper.read(0xA000), 0x10);
    }

    #[test]
    fn cartridge_ram_must_be_enabled_before_it_responds() {
        // Games disable RAM between accesses so a power-off cannot corrupt the save. A mapper
        // that ignores the latch lets a crashing game scribble over its own save file.
        let mut mapper = build(0x03, 0x00, 0x02);
        mapper.write(0xA000, 0x42);
        assert_eq!(mapper.read(0xA000), 0xFF, "disabled RAM reads as open bus");

        enable_ram(&mut *mapper);
        mapper.write(0xA000, 0x42);
        assert_eq!(mapper.read(0xA000), 0x42);

        mapper.write(0x0000, 0x00); // disable again
        assert_eq!(mapper.read(0xA000), 0xFF);
    }

    // -- MBC2 ----------------------------------------------------------------

    #[test]
    fn mbc2_selects_its_register_by_address_bit_eight() {
        let mut mapper = build(0x06, 0x03, 0x00); // 16 banks

        // Bit 8 clear: RAM enable.
        mapper.write(0x0000, 0x0A);
        mapper.write(0xA000, 0x05);
        assert_eq!(mapper.read(0xA000) & 0x0F, 0x05);

        // Bit 8 set: ROM bank select, even though the address is in the same range.
        mapper.write(0x0100, 0x03);
        assert_eq!(high_bank(&mut *mapper), 3);

        // And back: bit 8 clear disables RAM again.
        mapper.write(0x0000, 0x00);
        assert_eq!(mapper.read(0xA000), 0xFF);
    }

    #[test]
    fn mbc2_ram_is_nibbles_mirrored_across_the_window() {
        let mut mapper = build(0x06, 0x00, 0x00);
        mapper.write(0x0000, 0x0A);

        mapper.write(0xA000, 0xFF);
        assert_eq!(
            mapper.read(0xA000),
            0xFF,
            "the upper nibble is absent and reads as ones"
        );
        mapper.write(0xA001, 0x03);
        assert_eq!(mapper.read(0xA001), 0xF3, "only four bits are stored");

        // 512 nibbles mirrored through the whole 8 KiB window.
        assert_eq!(mapper.read(0xA000 + 512), 0xFF);
        assert_eq!(mapper.read(0xA001 + 512), 0xF3);
    }

    // -- MBC3 ----------------------------------------------------------------

    #[test]
    fn mbc3_uses_seven_bank_bits() {
        let mut mapper = build(0x13, 0x06, 0x03); // 128 banks
        for bank in [1u8, 0x1F, 0x20, 0x40, 0x7F] {
            mapper.write(0x2000, bank);
            assert_eq!(high_bank(&mut *mapper), bank, "bank {bank:#04X}");
        }
        // Unlike MBC1, bank 0x20 is directly reachable.
        mapper.write(0x2000, 0x00);
        assert_eq!(high_bank(&mut *mapper), 1, "zero still reads as one");
    }

    #[test]
    fn mbc3_maps_rtc_registers_into_the_ram_window() {
        let mut mapper = build(0x10, 0x00, 0x02); // MBC3 + RAM + RTC + battery
        enable_ram(&mut *mapper);

        // Bank 0 is cartridge RAM.
        mapper.write(0x4000, 0x00);
        mapper.write(0xA000, 0x77);
        assert_eq!(mapper.read(0xA000), 0x77);

        // Bank 0x08 replaces it with the seconds register.
        mapper.write(0x4000, mbc3_register::SECONDS);
        mapper.write(0xA000, 42);
        assert_eq!(mapper.read(0xA000), 42);

        // And the RAM underneath is untouched.
        mapper.write(0x4000, 0x00);
        assert_eq!(mapper.read(0xA000), 0x77);
    }

    #[test]
    fn mbc3_latches_the_clock_through_the_mapper() {
        let mut mapper = build(0x10, 0x00, 0x02);
        enable_ram(&mut *mapper);
        mapper.write(0x4000, mbc3_register::SECONDS);

        mapper.tick(4_194_304 * 7, 4_194_304);
        assert_eq!(mapper.read(0xA000), 0, "unlatched reads are stale");

        mapper.write(0x6000, 0x00);
        mapper.write(0x6000, 0x01);
        assert_eq!(mapper.read(0xA000), 7);
    }

    #[test]
    fn a_cartridge_without_an_rtc_has_none() {
        let mut mapper = build(0x13, 0x00, 0x02); // MBC3 + RAM + battery, no clock
        assert!(mapper.rtc().is_none());
        enable_ram(&mut *mapper);
        mapper.write(0x4000, mbc3_register::SECONDS);
        assert_eq!(mapper.read(0xA000), 0xFF);
    }

    // -- MBC5 ----------------------------------------------------------------

    #[test]
    fn mbc5_bank_zero_is_selectable_unlike_the_earlier_controllers() {
        let mut mapper = build(0x19, 0x04, 0x00);
        mapper.write(0x2000, 0x00);
        assert_eq!(
            high_bank(&mut *mapper),
            0,
            "MBC5 can map bank 0 into the high window"
        );
    }

    #[test]
    fn mbc5_assembles_a_nine_bit_bank_number_from_two_registers() {
        let mut mapper = build(0x19, 0x08, 0x00); // 512 banks
        mapper.write(0x2000, 0x34);
        mapper.write(0x3000, 0x01);
        // Bank 0x134 is beyond a u8, which is the point of the ninth bit.
        assert_eq!(high_bank(&mut *mapper), 0x34, "the marker is the low byte");

        mapper.write(0x3000, 0x00);
        mapper.write(0x2000, 0xFF);
        assert_eq!(high_bank(&mut *mapper), 0xFF);
    }

    #[test]
    fn mbc5_rumble_carts_steal_a_bit_from_the_ram_bank_register() {
        let mut mapper = build(0x1E, 0x00, 0x03); // MBC5 + RAM + battery + rumble
        assert!(!mapper.rumble());

        mapper.write(0x4000, 0x08);
        assert!(mapper.rumble(), "bit 3 drives the motor");

        mapper.write(0x4000, 0x00);
        assert!(!mapper.rumble());

        // A non-rumble cartridge uses all four bits for banking instead.
        let mut plain = build(0x1B, 0x00, 0x04); // 128 KiB of RAM, 16 banks
        enable_ram(&mut *plain);
        plain.write(0x4000, 0x0F);
        plain.write(0xA000, 0x99);
        plain.write(0x4000, 0x00);
        assert_ne!(plain.read(0xA000), 0x99, "a different bank");
        plain.write(0x4000, 0x0F);
        assert_eq!(plain.read(0xA000), 0x99);
    }

    // -- Cross-cutting -------------------------------------------------------

    #[test]
    fn every_mapper_exposes_its_save_through_the_trait_and_nowhere_else() {
        for cartridge_type in [0x03u8, 0x06, 0x13, 0x1B] {
            let mut mapper = build(cartridge_type, 0x00, 0x02);
            assert!(
                mapper.battery_save().is_some(),
                "type {cartridge_type:#04X} should have a save"
            );
            enable_ram(&mut *mapper);
            mapper.write(0xA000, 0x5A);
            assert!(mapper.battery_save().unwrap().is_dirty());
        }

        // And a cartridge with no save chip reports none rather than an empty one.
        let mapper = build(0x00, 0x00, 0x00);
        assert!(mapper.battery_save().is_none());
    }

    #[test]
    fn a_save_survives_a_write_dump_reload_cycle() {
        // The direct regression guard for the predecessor's save-corruption bug class.
        let mut mapper = build(0x03, 0x00, 0x03);
        enable_ram(&mut *mapper);
        mapper.write(0x6000, 0x01);
        for bank in 0..4u8 {
            mapper.write(0x4000, bank);
            for i in 0..16u16 {
                mapper.write(0xA000 + i, bank.wrapping_mul(16).wrapping_add(i as u8));
            }
        }
        let dumped = mapper.battery_save().unwrap().as_bytes().to_vec();

        let mut fresh = build(0x03, 0x00, 0x03);
        fresh
            .battery_save_mut()
            .unwrap()
            .load_from_bytes(&dumped)
            .unwrap();
        enable_ram(&mut *fresh);
        fresh.write(0x6000, 0x01);
        for bank in 0..4u8 {
            fresh.write(0x4000, bank);
            for i in 0..16u16 {
                assert_eq!(
                    fresh.read(0xA000 + i),
                    bank.wrapping_mul(16).wrapping_add(i as u8),
                    "bank {bank} byte {i}"
                );
            }
        }
    }

    #[test]
    fn mapper_state_round_trips_through_a_save_state() {
        let mut mapper = build(0x10, 0x04, 0x03);
        enable_ram(&mut *mapper);
        mapper.write(0x2000, 0x0B);
        mapper.write(0x4000, 0x02);
        mapper.write(0xA000, 0x5A);
        mapper.tick(4_194_304 * 90, 4_194_304);

        let mut w = StateWriter::new();
        mapper.save(&mut w);
        let blob = w.into_inner();

        let mut restored = build(0x10, 0x04, 0x03);
        restored.load(&mut StateReader::new(&blob)).unwrap();

        assert_eq!(high_bank(&mut *restored), 0x0B, "the ROM bank survived");
        assert_eq!(restored.read(0xA000), 0x5A, "as did RAM and its bank");
        assert_eq!(restored.rtc().unwrap().now().minutes, 1, "and the clock");
    }

    #[test]
    fn a_rom_whose_length_disagrees_with_its_header_is_rejected() {
        let (mut rom, header) = banked_rom(0x01, 0x04, 0x00);
        rom.truncate(rom.len() / 2);
        assert!(matches!(
            create_mapper(rom, &header),
            Err(CartridgeError::BadSize { .. })
        ));
    }
}
