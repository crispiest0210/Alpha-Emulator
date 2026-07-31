//! The Slot-1 cartridge: header, ROM, and the card transfer interface.
//!
//! # The ROM is not in either CPU's address space
//!
//! Every other system in this project maps its cartridge somewhere a CPU can read it. The DS does
//! not. A game's ROM is reached only by writing an eight-byte command into `CARD_COMMAND`, kicking
//! `ROMCTRL`, and then reading words back out of one register — so this module is a small command
//! interpreter rather than a memory region, and a game that has not run its card driver cannot see
//! its own data at all.
//!
//! Which is also why direct boot exists. [`NdsCartridge::direct_boot`] does what the firmware
//! would: copy the two ARM binaries the header points at into RAM and report where each core
//! should start. Without it nothing runs, because this project vendors no BIOS or firmware image
//! and never will.
//!
//! # What is not implemented
//!
//! - **KEY1 encryption.** The secure area is encrypted with a key derived from the BIOS, which
//!   this project does not have. Commands are interpreted in the plain, post-`KEY2` form that
//!   direct-booted software uses; encrypted-mode commands read as `0xFF`.
//!
//! The save chip *is* implemented, in [`crate::save`], and hangs off the auxiliary SPI port this
//! module owns. Which chip a cartridge has is not in the header and is worked out from how the
//! game talks to it; see that module for how, and for what it refuses to guess.

use crate::save::SaveChip;
use core_common::{CartridgeError, Savable, StateError, StateReader, StateWriter};

/// The header is 512 bytes; the part with fields in it is the first 0x170.
pub const HEADER_SIZE: usize = 0x200;
/// Where the firmware leaves a copy of the header for software to read.
pub const HEADER_MIRROR: u32 = 0x027F_FE00;

/// Register addresses, in both cores' I/O space. Which core actually owns the slot is decided by
/// `EXMEMCNT`, which the system assembly holds.
pub mod reg {
    pub const AUXSPICNT: u32 = 0x0400_01A0;
    pub const AUXSPIDATA: u32 = 0x0400_01A2;
    pub const ROMCTRL: u32 = 0x0400_01A4;
    pub const CARD_COMMAND: u32 = 0x0400_01A8;
    /// Where transferred words are read back. Not next to the others.
    pub const CARD_DATA: u32 = 0x0410_0010;
}

/// What a header says about where the game lives and how to start it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    pub title: String,
    pub game_code: String,
    pub maker_code: String,
    pub arm9_rom_offset: u32,
    pub arm9_entry: u32,
    pub arm9_ram_address: u32,
    pub arm9_size: u32,
    pub arm7_rom_offset: u32,
    pub arm7_entry: u32,
    pub arm7_ram_address: u32,
    pub arm7_size: u32,
}

impl Header {
    pub fn parse(rom: &[u8]) -> Result<Self, CartridgeError> {
        if rom.len() < HEADER_SIZE {
            return Err(CartridgeError::TooSmall {
                len: rom.len(),
                min: HEADER_SIZE,
            });
        }
        let word = |at: usize| u32::from_le_bytes(rom[at..at + 4].try_into().unwrap());
        let text = |at: usize, len: usize| {
            String::from_utf8_lossy(&rom[at..at + len])
                .trim_end_matches(['\0', ' '])
                .to_string()
        };

        let header = Self {
            title: text(0x00, 12),
            game_code: text(0x0C, 4),
            maker_code: text(0x10, 2),
            arm9_rom_offset: word(0x20),
            arm9_entry: word(0x24),
            arm9_ram_address: word(0x28),
            arm9_size: word(0x2C),
            arm7_rom_offset: word(0x30),
            arm7_entry: word(0x34),
            arm7_ram_address: word(0x38),
            arm7_size: word(0x3C),
        };

        // A header whose binaries fall outside the file is the one failure worth rejecting up
        // front: direct boot would otherwise copy zeroes into RAM and jump into them, which
        // presents as "the game hangs on a black screen" rather than as a bad file.
        for (offset, size, which) in [
            (header.arm9_rom_offset, header.arm9_size, "ARM9"),
            (header.arm7_rom_offset, header.arm7_size, "ARM7"),
        ] {
            let end = offset as u64 + size as u64;
            if end > rom.len() as u64 {
                return Err(CartridgeError::BadHeader(format!(
                    "{which} binary runs to {end:#X}, past the end of a {:#X}-byte ROM",
                    rom.len()
                )));
            }
        }
        Ok(header)
    }
}

/// Where a core should start after a direct boot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BootEntry {
    pub entry: u32,
    pub ram_address: u32,
    pub size: u32,
}

/// Bits of `ROMCTRL` this module acts on.
mod romctrl {
    /// Set while a word is waiting to be read.
    pub const DATA_READY: u32 = 1 << 23;
    pub const BLOCK_SIZE: u32 = 0x7 << 24;
    /// Written to begin a transfer; reads back as "a transfer is running".
    pub const START: u32 = 1 << 31;
}

/// The cartridge and its transfer state machine.
#[derive(Debug)]
pub struct NdsCartridge {
    rom: Vec<u8>,
    header: Header,
    auxspicnt: u16,
    /// The last byte the save chip drove back, which `AUXSPIDATA` reads.
    spi_data: u8,
    romctrl: u32,
    command: [u8; 8],
    /// Words still to be handed back through `CARD_DATA`.
    ///
    /// Materialised up front rather than streamed from an offset because two of the commands
    /// return something that is not a ROM slice at all — the chip ID repeats, and an unsupported
    /// command returns all ones — and one code path for all of them is worth the 512 bytes.
    pending: Vec<u32>,
    read_index: usize,
    /// The save chip on the auxiliary SPI bus.
    pub save: SaveChip,
}

impl NdsCartridge {
    pub fn new(rom: Vec<u8>) -> Result<Self, CartridgeError> {
        let header = Header::parse(&rom)?;
        Ok(Self {
            rom,
            header,
            auxspicnt: 0,
            spi_data: 0,
            romctrl: 0,
            command: [0; 8],
            pending: Vec::new(),
            read_index: 0,
            save: SaveChip::new(),
        })
    }

    /// An empty slot, which answers every command with `0xFFFF_FFFF`.
    pub fn empty() -> Self {
        Self {
            rom: Vec::new(),
            header: Header {
                title: String::new(),
                game_code: String::new(),
                maker_code: String::new(),
                arm9_rom_offset: 0,
                arm9_entry: 0,
                arm9_ram_address: 0,
                arm9_size: 0,
                arm7_rom_offset: 0,
                arm7_entry: 0,
                arm7_ram_address: 0,
                arm7_size: 0,
            },
            auxspicnt: 0,
            spi_data: 0,
            romctrl: 0,
            command: [0; 8],
            pending: Vec::new(),
            read_index: 0,
            save: SaveChip::new(),
        }
    }

    pub fn header(&self) -> &Header {
        &self.header
    }

    pub fn rom(&self) -> &[u8] {
        &self.rom
    }

    pub fn is_present(&self) -> bool {
        !self.rom.is_empty()
    }

    /// The two binaries the firmware would copy into RAM, and where each core starts.
    ///
    /// Returns the bytes rather than writing them, because the ARM9's binary may land in TCM,
    /// which lives inside the CPU crate and not in any memory this module can reach.
    pub fn direct_boot(&self) -> (BootEntry, &[u8], BootEntry, &[u8]) {
        let h = &self.header;
        let slice = |offset: u32, size: u32| {
            let start = offset as usize;
            let end = start + size as usize;
            self.rom.get(start..end).unwrap_or(&[])
        };
        (
            BootEntry {
                entry: h.arm9_entry,
                ram_address: h.arm9_ram_address,
                size: h.arm9_size,
            },
            slice(h.arm9_rom_offset, h.arm9_size),
            BootEntry {
                entry: h.arm7_entry,
                ram_address: h.arm7_ram_address,
                size: h.arm7_size,
            },
            slice(h.arm7_rom_offset, h.arm7_size),
        )
    }

    /// The first 512 bytes, which the firmware mirrors into main RAM for software to read.
    pub fn header_bytes(&self) -> &[u8] {
        self.rom.get(..HEADER_SIZE).unwrap_or(&[])
    }

    /// The save chip's contents, or `None` while its type is still undetermined.
    pub fn save_ram(&self) -> Option<&[u8]> {
        self.save.save_ram()
    }

    /// Install a save file, which settles the chip type from its size.
    pub fn load_save_ram(&mut self, data: &[u8]) -> Result<(), CartridgeError> {
        self.save.load_file(data)
    }

    pub fn owns(addr: u32) -> bool {
        (reg::AUXSPICNT..reg::CARD_COMMAND + 8).contains(&addr) || addr & !3 == reg::CARD_DATA
    }

    pub fn read32(&mut self, addr: u32) -> Option<u32> {
        match addr & !3 {
            reg::AUXSPICNT => Some(self.auxspicnt as u32),
            reg::ROMCTRL => Some(self.romctrl),
            reg::CARD_COMMAND => Some(u32::from_be_bytes(self.command[0..4].try_into().unwrap())),
            0x0400_01AC => Some(u32::from_be_bytes(self.command[4..8].try_into().unwrap())),
            reg::CARD_DATA => Some(self.take_word()),
            _ => None,
        }
    }

    /// Pop one word of the current transfer.
    ///
    /// Reading past the end returns `0xFFFF_FFFF` rather than repeating or panicking, and clears
    /// the ready flag so a driver that over-reads stops rather than spinning.
    fn take_word(&mut self) -> u32 {
        let word = self
            .pending
            .get(self.read_index)
            .copied()
            .unwrap_or(u32::MAX);
        if self.read_index < self.pending.len() {
            self.read_index += 1;
        }
        if self.read_index >= self.pending.len() {
            self.romctrl &= !(romctrl::DATA_READY | romctrl::START);
        }
        word
    }

    pub fn write32(&mut self, addr: u32, value: u32) -> bool {
        match addr & !3 {
            reg::AUXSPICNT => self.auxspicnt = value as u16,
            reg::ROMCTRL => self.set_romctrl(value),
            reg::CARD_COMMAND => self.command[0..4].copy_from_slice(&value.to_be_bytes()),
            0x0400_01AC => self.command[4..8].copy_from_slice(&value.to_be_bytes()),
            // `CARD_DATA` is read-only; a write to it is a driver bug, not a register.
            reg::CARD_DATA => {}
            _ => return false,
        }
        true
    }

    pub fn read16(&mut self, addr: u32) -> Option<u16> {
        if addr & !1 == reg::AUXSPIDATA {
            return Some(self.spi_read() as u16);
        }
        let word = self.read32(addr & !3)?;
        Some(if addr & 2 == 0 {
            word as u16
        } else {
            (word >> 16) as u16
        })
    }

    pub fn write16(&mut self, addr: u32, value: u16) -> bool {
        if addr & !1 == reg::AUXSPIDATA {
            self.spi_transfer(value as u8);
            return true;
        }
        if addr & !1 == reg::AUXSPICNT {
            self.auxspicnt = value;
            return true;
        }
        let Some(current) = self.peek32(addr & !3) else {
            return false;
        };
        let spliced = if addr & 2 == 0 {
            (current & 0xFFFF_0000) | value as u32
        } else {
            (current & 0xFFFF) | ((value as u32) << 16)
        };
        self.write32(addr & !3, spliced)
    }

    pub fn read8(&mut self, addr: u32) -> Option<u8> {
        let word = self.read32(addr & !3)?;
        Some((word >> ((addr & 3) * 8)) as u8)
    }

    pub fn write8(&mut self, addr: u32, value: u8) -> bool {
        let Some(current) = self.peek32(addr & !3) else {
            return false;
        };
        let shift = (addr & 3) * 8;
        let spliced = (current & !(0xFF << shift)) | ((value as u32) << shift);
        self.write32(addr & !3, spliced)
    }

    /// A read that does not pop the data FIFO, for splicing a narrow write.
    fn peek32(&self, addr: u32) -> Option<u32> {
        match addr & !3 {
            reg::AUXSPICNT => Some(self.auxspicnt as u32),
            reg::ROMCTRL => Some(self.romctrl),
            reg::CARD_COMMAND => Some(u32::from_be_bytes(self.command[0..4].try_into().unwrap())),
            0x0400_01AC => Some(u32::from_be_bytes(self.command[4..8].try_into().unwrap())),
            reg::CARD_DATA => Some(0),
            _ => None,
        }
    }

    /// Shift a byte through the save chip.
    ///
    /// `AUXSPICNT` bit 6 is the chip-select hold: while it is set the transaction continues, and
    /// the byte it is clear on is the last. That bit is the only thing telling the chip where a
    /// transaction ends, and it is what makes the length-based detection possible at all.
    fn spi_transfer(&mut self, byte: u8) {
        if self.auxspicnt & (1 << 15) == 0 {
            // The SPI bus is disabled; nothing is connected to shift through.
            return;
        }
        let hold = self.auxspicnt & (1 << 6) != 0;
        self.spi_data = self.save.transfer(byte, hold);
    }

    fn spi_read(&self) -> u8 {
        self.spi_data
    }

    fn set_romctrl(&mut self, value: u32) {
        let starting = value & romctrl::START != 0 && self.romctrl & romctrl::START == 0;
        self.romctrl = value;
        if starting {
            self.begin_transfer();
        }
    }

    /// Interpret the eight-byte command and fill the data FIFO.
    fn begin_transfer(&mut self) {
        let words = match (self.romctrl & romctrl::BLOCK_SIZE) >> 24 {
            0 => 0,
            // Setting 7 is one word, not 0x8000. Reading the field as a plain shift makes every
            // chip-ID read a 32 KiB transfer that never completes.
            7 => 1,
            n => (0x100u32 << n) / 4,
        } as usize;

        self.pending.clear();
        self.read_index = 0;
        if words == 0 {
            self.romctrl &= !(romctrl::DATA_READY | romctrl::START);
            return;
        }

        let command = self.command[0];
        let address = u32::from_be_bytes(self.command[1..5].try_into().unwrap()) as usize & !3usize;
        match command {
            // Read the header, which is also what a freshly reset card answers.
            0x00 => self.fill_from_rom(0, words),
            // The main data read. The card wraps within a 4 KiB block, which is a real behaviour
            // a driver relies on for the last partial block of a file.
            0xB7 => self.fill_from_rom(address, words),
            // Chip ID, repeated for the whole transfer.
            0x90 | 0xB8 => {
                let id = self.chip_id();
                self.pending = vec![id; words];
            }
            // Dummy / unrecognised: all ones, which is what an absent or busy card drives.
            _ => self.pending = vec![u32::MAX; words],
        }
        self.romctrl |= romctrl::DATA_READY | romctrl::START;
    }

    fn fill_from_rom(&mut self, address: usize, words: usize) {
        self.pending.reserve(words);
        for i in 0..words {
            // Reads wrap inside the 4 KiB block containing the start address.
            let offset = if address < 0x8000 {
                address + i * 4
            } else {
                (address & !0xFFF) | ((address + i * 4) & 0xFFF)
            };
            let word = self
                .rom
                .get(offset..offset + 4)
                .map(|b| u32::from_le_bytes(b.try_into().unwrap()))
                .unwrap_or(u32::MAX);
            self.pending.push(word);
        }
    }

    /// A plausible chip ID: 1 MiB-unit capacity in the middle byte, as real carts report.
    fn chip_id(&self) -> u32 {
        if !self.is_present() {
            return u32::MAX;
        }
        let megabytes = (self.rom.len() / (1024 * 1024)).clamp(1, 0x7F) as u32;
        0xC2 | ((megabytes - 1) << 8)
    }

    /// Whether a transfer just finished, which is what raises the card interrupt.
    pub fn transfer_complete(&self) -> bool {
        self.romctrl & romctrl::START == 0 && !self.pending.is_empty()
    }

    /// Whether a word is waiting, which is what arms a card DMA.
    pub fn data_ready(&self) -> bool {
        self.romctrl & romctrl::DATA_READY != 0
    }

    pub fn reset(&mut self) {
        // The save chip is *not* reset: it is the cartridge's battery-backed memory, and a reset
        // is a reset of the console rather than a new cartridge.
        self.save.reset();
        self.auxspicnt = 0;
        self.spi_data = 0;
        self.romctrl = 0;
        self.command = [0; 8];
        self.pending.clear();
        self.read_index = 0;
    }
}

impl Savable for NdsCartridge {
    fn save(&self, w: &mut StateWriter) {
        // The ROM is not written: it is the file on disk, identical across runs, and up to
        // 256 MiB that would otherwise sit in every rewind frame.
        w.write_u16(self.auxspicnt);
        w.write_u8(self.spi_data);
        self.save.save(w);
        w.write_u32(self.romctrl);
        w.write_bytes(&self.command);
        w.write_u64(self.pending.len() as u64);
        for word in &self.pending {
            w.write_u32(*word);
        }
        w.write_u64(self.read_index as u64);
    }

    fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        self.auxspicnt = r.read_u16()?;
        self.spi_data = r.read_u8()?;
        Savable::load(&mut self.save, r)?;
        self.romctrl = r.read_u32()?;
        r.read_bytes(&mut self.command)?;
        let count = r.read_u64()? as usize;
        // A block transfer is at most 0x1000 bytes, so anything larger is a corrupt state
        // rather than a big read, and allocating from it would be taking the file's word for it.
        if count > 0x400 {
            return Err(StateError::Malformed(format!(
                "card transfer of {count} words is larger than any block"
            )));
        }
        self.pending.clear();
        for _ in 0..count {
            self.pending.push(r.read_u32()?);
        }
        self.read_index = (r.read_u64()? as usize).min(self.pending.len());
        Ok(())
    }
}

#[cfg(test)]
mod tests;
