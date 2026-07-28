//! Battery-backed save chips: SRAM, Flash, and EEPROM.
//!
//! All three implement [`BatteryBackedSave`], which is what the frontend's save-to-disk path
//! sees. Each owns its own bytes; nothing reaches past the trait.
//!
//! Behavior is per GBATEK for the GBA chips. Game Boy cartridge RAM is plain SRAM and uses
//! the same [`Sram`] type.

use crate::BatteryBackedSave;
use core_common::{CartridgeError, Savable, StateError, StateReader, StateWriter};

/// Which kind of save chip a cartridge carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SaveKind {
    None,
    Sram { size: usize },
    Flash { size: usize },
    Eeprom { size: usize },
}

impl SaveKind {
    pub const fn size(self) -> usize {
        match self {
            SaveKind::None => 0,
            SaveKind::Sram { size } | SaveKind::Flash { size } | SaveKind::Eeprom { size } => size,
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            SaveKind::None => "none",
            SaveKind::Sram { .. } => "SRAM",
            SaveKind::Flash { .. } => "Flash",
            SaveKind::Eeprom { .. } => "EEPROM",
        }
    }
}

/// Shared by every chip: size-checked restore from a `.sav` file.
fn restore(target: &mut [u8], data: &[u8]) -> Result<(), CartridgeError> {
    if data.len() != target.len() {
        return Err(CartridgeError::SaveSizeMismatch {
            expected: target.len(),
            found: data.len(),
        });
    }
    target.copy_from_slice(data);
    Ok(())
}

// ---------------------------------------------------------------------------
// SRAM
// ---------------------------------------------------------------------------

/// Plain battery-backed static RAM.
///
/// Used for Game Boy cartridge RAM and for GBA cartridges with an SRAM chip. The only one of
/// the three that behaves like memory rather than a device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sram {
    bytes: Box<[u8]>,
    dirty: bool,
}

impl Sram {
    pub fn new(size: usize) -> Self {
        Self {
            // Fresh SRAM is not zeroed on real hardware, but a game that reads uninitialized
            // save memory checks a magic value and reinitializes, so zeroes are safe and make
            // a fresh save reproducible.
            bytes: vec![0; size].into_boxed_slice(),
            dirty: false,
        }
    }
}

impl BatteryBackedSave for Sram {
    fn kind(&self) -> SaveKind {
        SaveKind::Sram {
            size: self.bytes.len(),
        }
    }

    fn read_byte(&mut self, addr: u32) -> u8 {
        if self.bytes.is_empty() {
            return 0xFF;
        }
        // Real cartridges decode fewer address lines than the bus provides, so the chip
        // mirrors rather than leaving gaps.
        self.bytes[addr as usize % self.bytes.len()]
    }

    fn write_byte(&mut self, addr: u32, value: u8) {
        if self.bytes.is_empty() {
            return;
        }
        let index = addr as usize % self.bytes.len();
        if self.bytes[index] != value {
            self.dirty = true;
        }
        self.bytes[index] = value;
    }

    fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    fn load_from_bytes(&mut self, data: &[u8]) -> Result<(), CartridgeError> {
        restore(&mut self.bytes, data)?;
        self.dirty = false;
        Ok(())
    }

    fn is_dirty(&self) -> bool {
        self.dirty
    }

    fn clear_dirty(&mut self) {
        self.dirty = false;
    }
}

impl Savable for Sram {
    fn save(&self, w: &mut StateWriter) {
        w.write_blob(&self.bytes);
        w.write_bool(self.dirty);
    }
    fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        let bytes = r.read_blob()?;
        if bytes.len() != self.bytes.len() {
            return Err(StateError::Malformed(format!(
                "save RAM is {} bytes in this build, {} in the state",
                self.bytes.len(),
                bytes.len()
            )));
        }
        self.bytes.copy_from_slice(bytes);
        self.dirty = r.read_bool()?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Flash
// ---------------------------------------------------------------------------

/// Where a Flash chip is in its command sequence.
///
/// Flash is not memory: every operation is a sequence of magic writes to `0x5555` and
/// `0x2AAA`, and only after the full sequence does anything happen. A read can be answering
/// an identify command rather than returning stored data, which is exactly why
/// [`BatteryBackedSave::read_byte`] takes `&mut self`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum FlashState {
    #[default]
    Ready,
    /// Saw `0xAA` at `0x5555`.
    Unlock1,
    /// Saw `0x55` at `0x2AAA`; the next write at `0x5555` is the command.
    Unlock2,
    /// An erase command was started and needs its own unlock sequence.
    EraseUnlock1,
    EraseUnlock2,
    /// The next write is a single byte of program data.
    WriteByte,
    /// The next write selects the memory bank (128 KiB chips only).
    SelectBank,
}

/// Flash memory, 64 KiB (one bank) or 128 KiB (two banks).
///
/// The CPU sees a 64 KiB window; on a 128 KiB chip a bank-select command chooses which half
/// appears in it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Flash {
    bytes: Box<[u8]>,
    state: FlashState,
    /// Whether an identify command is active, so reads return the chip ID instead of data.
    identify: bool,
    bank: usize,
    dirty: bool,
    /// Manufacturer and device ID, which games check to decide the chip's size.
    id: [u8; 2],
}

const FLASH_BANK_SIZE: usize = 64 * 1024;

impl Flash {
    pub fn new(size: usize) -> Self {
        // Games identify the chip before using it and take a different code path per size, so
        // the reported ID has to match the capacity.
        let id = if size > FLASH_BANK_SIZE {
            [0xC2, 0x09] // Macronix 128 KiB
        } else {
            [0x32, 0x1B] // Panasonic 64 KiB
        };
        Self {
            bytes: vec![0xFF; size].into_boxed_slice(), // erased Flash reads as all ones
            state: FlashState::Ready,
            identify: false,
            bank: 0,
            dirty: false,
            id,
        }
    }

    #[inline]
    fn offset(&self, addr: u32) -> usize {
        (self.bank * FLASH_BANK_SIZE + (addr as usize & 0xFFFF)) % self.bytes.len()
    }

    fn erase_all(&mut self) {
        self.bytes.fill(0xFF);
        self.dirty = true;
    }

    /// Erase one 4 KiB sector, the granularity the chip actually supports.
    fn erase_sector(&mut self, addr: u32) {
        let base = self.offset(addr) & !0x0FFF;
        let end = (base + 0x1000).min(self.bytes.len());
        self.bytes[base..end].fill(0xFF);
        self.dirty = true;
    }
}

impl BatteryBackedSave for Flash {
    fn kind(&self) -> SaveKind {
        SaveKind::Flash {
            size: self.bytes.len(),
        }
    }

    fn read_byte(&mut self, addr: u32) -> u8 {
        if self.identify {
            // While identify is active the first two bytes of the window are the chip ID.
            match addr & 0xFFFF {
                0x0000 => return self.id[0],
                0x0001 => return self.id[1],
                _ => {}
            }
        }
        self.bytes[self.offset(addr)]
    }

    fn write_byte(&mut self, addr: u32, value: u8) {
        let low = addr & 0xFFFF;

        match self.state {
            FlashState::WriteByte => {
                // Flash can only clear bits; setting one requires an erase first. Programming
                // is therefore an AND, not an assignment — a game that relies on this to patch
                // a record in place would otherwise see the wrong result.
                let index = self.offset(addr);
                self.bytes[index] &= value;
                self.dirty = true;
                self.state = FlashState::Ready;
                return;
            }
            FlashState::SelectBank => {
                self.bank = (value as usize) & 1;
                self.state = FlashState::Ready;
                return;
            }
            _ => {}
        }

        match (self.state, low, value) {
            (FlashState::Ready, 0x5555, 0xAA) => self.state = FlashState::Unlock1,
            (FlashState::Unlock1, 0x2AAA, 0x55) => self.state = FlashState::Unlock2,
            (FlashState::Unlock2, 0x5555, command) => {
                self.state = FlashState::Ready;
                match command {
                    0x90 => self.identify = true,
                    0xF0 => self.identify = false,
                    0xA0 => self.state = FlashState::WriteByte,
                    0xB0 => self.state = FlashState::SelectBank,
                    0x80 => self.state = FlashState::EraseUnlock1,
                    _ => {}
                }
            }
            (FlashState::EraseUnlock1, 0x5555, 0xAA) => self.state = FlashState::EraseUnlock2,
            (FlashState::EraseUnlock2, 0x2AAA, 0x55) => self.state = FlashState::Ready,
            // An erase command lands after its own unlock; 0x10 wipes the chip, 0x30 a sector.
            (FlashState::Ready, 0x5555, 0x10) => self.erase_all(),
            (FlashState::Ready, _, 0x30) => self.erase_sector(addr),
            _ => self.state = FlashState::Ready,
        }
    }

    fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    fn load_from_bytes(&mut self, data: &[u8]) -> Result<(), CartridgeError> {
        restore(&mut self.bytes, data)?;
        self.dirty = false;
        Ok(())
    }

    fn is_dirty(&self) -> bool {
        self.dirty
    }

    fn clear_dirty(&mut self) {
        self.dirty = false;
    }
}

impl Savable for Flash {
    fn save(&self, w: &mut StateWriter) {
        w.write_blob(&self.bytes);
        w.write_u8(self.state as u8);
        w.write_bool(self.identify);
        w.write_u32(self.bank as u32);
        w.write_bool(self.dirty);
    }
    fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        let bytes = r.read_blob()?;
        if bytes.len() != self.bytes.len() {
            return Err(StateError::Malformed("flash size mismatch".into()));
        }
        self.bytes.copy_from_slice(bytes);
        self.state = match r.read_u8()? {
            0 => FlashState::Ready,
            1 => FlashState::Unlock1,
            2 => FlashState::Unlock2,
            3 => FlashState::EraseUnlock1,
            4 => FlashState::EraseUnlock2,
            5 => FlashState::WriteByte,
            6 => FlashState::SelectBank,
            other => return Err(StateError::Malformed(format!("bad flash state {other}"))),
        };
        self.identify = r.read_bool()?;
        self.bank = r.read_u32()? as usize & 1;
        self.dirty = r.read_bool()?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// EEPROM
// ---------------------------------------------------------------------------

/// What the EEPROM is currently doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum EepromState {
    /// Collecting a command's bits.
    #[default]
    ReceivingCommand,
    /// A read was requested; shifting out four ignored bits then 64 data bits.
    SendingData,
    /// A write was requested; collecting 64 data bits.
    ReceivingData,
}

/// Serial EEPROM, 512 bytes or 8 KiB.
///
/// Unlike SRAM and Flash, EEPROM is not addressed as memory at all: the CPU talks to it one
/// **bit** at a time through a single address, and transfers are normally driven by DMA. A
/// request is a bit stream — two command bits, an address, then either 64 data bits out or 64
/// in — so this is a shift register with a state machine, not an array with an index.
///
/// The address width depends on capacity (6 bits for 512 bytes, 14 for 8 KiB), and games
/// discover it by trying one and seeing whether it works.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Eeprom {
    bytes: Box<[u8]>,
    state: EepromState,
    /// Bits received so far, most significant first.
    shift_in: u64,
    bits_received: u32,
    /// Bits waiting to be read out.
    shift_out: u64,
    bits_sent: u32,
    /// Address of the 8-byte block being read or written.
    block: usize,
    dirty: bool,
}

impl Eeprom {
    pub fn new(size: usize) -> Self {
        Self {
            bytes: vec![0xFF; size].into_boxed_slice(),
            state: EepromState::ReceivingCommand,
            shift_in: 0,
            bits_received: 0,
            shift_out: 0,
            bits_sent: 0,
            block: 0,
            dirty: false,
        }
    }

    /// 6 address bits for a 512-byte chip, 14 for an 8 KiB one.
    fn address_bits(&self) -> u32 {
        if self.bytes.len() > 512 {
            14
        } else {
            6
        }
    }

    /// Total command length: two command bits plus the address, plus a trailing bit.
    fn command_bits(&self) -> u32 {
        2 + self.address_bits() + 1
    }

    fn begin_read(&mut self, block: usize) {
        self.block = block;
        let base = (block * 8) % self.bytes.len();
        let mut value = 0u64;
        for i in 0..8 {
            value = (value << 8) | self.bytes[(base + i) % self.bytes.len()] as u64;
        }
        self.shift_out = value;
        // Four dummy bits precede the data on a read.
        self.bits_sent = 0;
        self.state = EepromState::SendingData;
    }

    fn commit_write(&mut self) {
        let base = (self.block * 8) % self.bytes.len();
        for i in 0..8 {
            let byte = (self.shift_in >> (56 - i * 8)) as u8;
            let index = (base + i as usize) % self.bytes.len();
            self.bytes[index] = byte;
        }
        self.dirty = true;
        self.state = EepromState::ReceivingCommand;
        self.shift_in = 0;
        self.bits_received = 0;
    }
}

impl BatteryBackedSave for Eeprom {
    fn kind(&self) -> SaveKind {
        SaveKind::Eeprom {
            size: self.bytes.len(),
        }
    }

    /// Shift one bit out. The address is ignored: the chip has exactly one data line.
    fn read_byte(&mut self, _addr: u32) -> u8 {
        match self.state {
            EepromState::SendingData => {
                let bit = if self.bits_sent < 4 {
                    // Four dummy bits precede the payload.
                    0
                } else {
                    ((self.shift_out >> (63 - (self.bits_sent - 4))) & 1) as u8
                };
                self.bits_sent += 1;
                if self.bits_sent >= 68 {
                    self.state = EepromState::ReceivingCommand;
                    self.shift_in = 0;
                    self.bits_received = 0;
                }
                bit
            }
            // Outside a read the data line idles high, which games poll to detect readiness.
            _ => 1,
        }
    }

    /// Shift one bit in, taking only bit 0 of the written value.
    fn write_byte(&mut self, _addr: u32, value: u8) {
        let bit = (value & 1) as u64;

        match self.state {
            EepromState::ReceivingCommand => {
                self.shift_in = (self.shift_in << 1) | bit;
                self.bits_received += 1;

                if self.bits_received == self.command_bits() {
                    let address_bits = self.address_bits();
                    // Layout: [command:2][address:n][1]
                    let command = (self.shift_in >> (address_bits + 1)) & 0b11;
                    let block = ((self.shift_in >> 1) & ((1 << address_bits) - 1)) as usize;

                    self.shift_in = 0;
                    self.bits_received = 0;
                    match command {
                        0b11 => self.begin_read(block),
                        0b10 => {
                            self.block = block;
                            self.state = EepromState::ReceivingData;
                        }
                        _ => self.state = EepromState::ReceivingCommand,
                    }
                }
            }
            EepromState::ReceivingData => {
                self.shift_in = (self.shift_in << 1) | bit;
                self.bits_received += 1;
                // 64 data bits, then a terminating bit.
                if self.bits_received == 64 {
                    self.commit_write();
                }
            }
            EepromState::SendingData => {}
        }
    }

    fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    fn load_from_bytes(&mut self, data: &[u8]) -> Result<(), CartridgeError> {
        restore(&mut self.bytes, data)?;
        self.dirty = false;
        Ok(())
    }

    fn is_dirty(&self) -> bool {
        self.dirty
    }

    fn clear_dirty(&mut self) {
        self.dirty = false;
    }
}

impl Savable for Eeprom {
    fn save(&self, w: &mut StateWriter) {
        w.write_blob(&self.bytes);
        w.write_u8(self.state as u8);
        w.write_u64(self.shift_in);
        w.write_u32(self.bits_received);
        w.write_u64(self.shift_out);
        w.write_u32(self.bits_sent);
        w.write_u32(self.block as u32);
        w.write_bool(self.dirty);
    }
    fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        let bytes = r.read_blob()?;
        if bytes.len() != self.bytes.len() {
            return Err(StateError::Malformed("eeprom size mismatch".into()));
        }
        self.bytes.copy_from_slice(bytes);
        self.state = match r.read_u8()? {
            0 => EepromState::ReceivingCommand,
            1 => EepromState::SendingData,
            2 => EepromState::ReceivingData,
            other => return Err(StateError::Malformed(format!("bad eeprom state {other}"))),
        };
        self.shift_in = r.read_u64()?;
        self.bits_received = r.read_u32()?;
        self.shift_out = r.read_u64()?;
        self.bits_sent = r.read_u32()?;
        self.block = r.read_u32()? as usize;
        self.dirty = r.read_bool()?;
        Ok(())
    }
}

/// Build the save chip a [`SaveKind`] describes.
pub fn create_save(kind: SaveKind) -> Option<Box<dyn BatteryBackedSave>> {
    match kind {
        SaveKind::None => None,
        SaveKind::Sram { size } => Some(Box::new(Sram::new(size))),
        SaveKind::Flash { size } => Some(Box::new(Flash::new(size))),
        SaveKind::Eeprom { size } => Some(Box::new(Eeprom::new(size))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The regression guard the predecessor's save-corruption bug class needed: write, dump,
    /// reload into a fresh chip, and confirm every subsequent read agrees.
    fn assert_round_trips(mut chip: Box<dyn BatteryBackedSave>, fresh: Box<dyn BatteryBackedSave>) {
        for i in 0..64u32 {
            chip.write_byte(i, (i as u8).wrapping_mul(7));
        }
        let dumped = chip.as_bytes().to_vec();

        let mut fresh = fresh;
        fresh.load_from_bytes(&dumped).unwrap();
        assert_eq!(fresh.as_bytes(), chip.as_bytes());
        for i in 0..64u32 {
            assert_eq!(fresh.read_byte(i), chip.read_byte(i), "byte {i}");
        }
        assert!(!fresh.is_dirty(), "a freshly loaded save is not dirty");
    }

    #[test]
    fn sram_round_trips_to_a_file_and_back() {
        assert_round_trips(Box::new(Sram::new(8192)), Box::new(Sram::new(8192)));
    }

    #[test]
    fn sram_reads_back_what_was_written_and_mirrors() {
        let mut sram = Sram::new(1024);
        sram.write_byte(0, 0xAB);
        assert_eq!(sram.read_byte(0), 0xAB);
        // Cartridges decode fewer address lines than the bus provides.
        assert_eq!(sram.read_byte(1024), 0xAB);
    }

    #[test]
    fn writes_mark_the_chip_dirty_so_the_frontend_can_flush() {
        let mut sram = Sram::new(64);
        assert!(!sram.is_dirty());
        sram.write_byte(0, 0x01);
        assert!(sram.is_dirty());
        sram.clear_dirty();
        assert!(!sram.is_dirty());
        // Rewriting the same value is not a change.
        sram.write_byte(0, 0x01);
        assert!(!sram.is_dirty());
    }

    #[test]
    fn a_save_of_the_wrong_size_is_refused() {
        let mut sram = Sram::new(8192);
        assert!(matches!(
            sram.load_from_bytes(&[0u8; 32768]),
            Err(CartridgeError::SaveSizeMismatch {
                expected: 8192,
                found: 32768
            })
        ));
    }

    // -- Flash ---------------------------------------------------------------

    /// Drive the unlock sequence and issue `command`.
    fn flash_command(flash: &mut Flash, command: u8) {
        flash.write_byte(0x5555, 0xAA);
        flash.write_byte(0x2AAA, 0x55);
        flash.write_byte(0x5555, command);
    }

    #[test]
    fn flash_starts_erased() {
        let mut flash = Flash::new(65536);
        assert_eq!(flash.read_byte(0), 0xFF);
    }

    #[test]
    fn flash_reports_a_chip_id_matching_its_capacity() {
        let mut flash = Flash::new(65536);
        flash_command(&mut flash, 0x90);
        assert_eq!((flash.read_byte(0), flash.read_byte(1)), (0x32, 0x1B));

        // Leaving identify mode restores normal reads.
        flash_command(&mut flash, 0xF0);
        assert_eq!(flash.read_byte(0), 0xFF);

        let mut big = Flash::new(131072);
        flash_command(&mut big, 0x90);
        assert_eq!((big.read_byte(0), big.read_byte(1)), (0xC2, 0x09));
    }

    #[test]
    fn flash_programming_clears_bits_but_cannot_set_them() {
        let mut flash = Flash::new(65536);
        flash_command(&mut flash, 0xA0);
        flash.write_byte(0x100, 0x0F);
        assert_eq!(flash.read_byte(0x100), 0x0F);

        // Programming again can only clear further; 0xF0 cannot restore the high nibble.
        flash_command(&mut flash, 0xA0);
        flash.write_byte(0x100, 0xF0);
        assert_eq!(
            flash.read_byte(0x100),
            0x00,
            "programming ANDs; setting a bit needs an erase"
        );
    }

    #[test]
    fn flash_sector_erase_affects_only_its_own_4k() {
        let mut flash = Flash::new(65536);
        for addr in [0x0100u32, 0x1100] {
            flash_command(&mut flash, 0xA0);
            flash.write_byte(addr, 0x00);
        }
        assert_eq!(flash.read_byte(0x0100), 0x00);

        // Erase the first sector only.
        flash_command(&mut flash, 0x80);
        flash.write_byte(0x5555, 0xAA);
        flash.write_byte(0x2AAA, 0x55);
        flash.write_byte(0x0100, 0x30);

        assert_eq!(flash.read_byte(0x0100), 0xFF, "erased");
        assert_eq!(flash.read_byte(0x1100), 0x00, "a different sector survives");
    }

    #[test]
    fn flash_chip_erase_wipes_everything() {
        let mut flash = Flash::new(65536);
        flash_command(&mut flash, 0xA0);
        flash.write_byte(0x2000, 0x12);

        flash_command(&mut flash, 0x80);
        flash.write_byte(0x5555, 0xAA);
        flash.write_byte(0x2AAA, 0x55);
        flash.write_byte(0x5555, 0x10);

        assert_eq!(flash.read_byte(0x2000), 0xFF);
    }

    #[test]
    fn flash_bank_switching_exposes_the_second_half() {
        let mut flash = Flash::new(131072);
        flash_command(&mut flash, 0xA0);
        flash.write_byte(0x0000, 0xAA); // bank 0

        flash_command(&mut flash, 0xB0);
        flash.write_byte(0x0000, 0x01); // select bank 1
        assert_eq!(flash.read_byte(0x0000), 0xFF, "bank 1 is still erased");

        flash_command(&mut flash, 0xA0);
        flash.write_byte(0x0000, 0xBB);
        assert_eq!(flash.read_byte(0x0000), 0xBB);

        flash_command(&mut flash, 0xB0);
        flash.write_byte(0x0000, 0x00);
        assert_eq!(flash.read_byte(0x0000), 0xAA, "bank 0 is unchanged");
    }

    #[test]
    fn a_stray_write_does_not_program_flash() {
        // Without the unlock sequence nothing happens; this is what makes Flash resistant to
        // accidental writes, and treating it as memory would break that.
        let mut flash = Flash::new(65536);
        flash.write_byte(0x100, 0x00);
        assert_eq!(flash.read_byte(0x100), 0xFF);
    }

    #[test]
    fn flash_round_trips_to_a_file_and_back() {
        let mut chip = Flash::new(65536);
        for i in 0..64u32 {
            flash_command(&mut chip, 0xA0);
            chip.write_byte(i, i as u8);
        }
        let dumped = chip.as_bytes().to_vec();
        let mut fresh = Flash::new(65536);
        fresh.load_from_bytes(&dumped).unwrap();
        assert_eq!(fresh.as_bytes(), chip.as_bytes());
    }

    // -- EEPROM --------------------------------------------------------------

    fn eeprom_send(chip: &mut Eeprom, bits: &[u8]) {
        for &bit in bits {
            chip.write_byte(0, bit);
        }
    }

    /// Encode a command as its bit sequence: two command bits, the address, then a stop bit.
    fn eeprom_command(command: u8, block: usize, address_bits: u32) -> Vec<u8> {
        let mut bits = vec![(command >> 1) & 1, command & 1];
        for i in (0..address_bits).rev() {
            bits.push(((block >> i) & 1) as u8);
        }
        bits.push(0);
        bits
    }

    #[test]
    fn eeprom_writes_and_reads_back_a_block() {
        let mut chip = Eeprom::new(8192);

        // Write command (0b10) to block 3, followed by 64 data bits.
        eeprom_send(&mut chip, &eeprom_command(0b10, 3, 14));
        let payload: u64 = 0x0123_4567_89AB_CDEF;
        for i in (0..64).rev() {
            chip.write_byte(0, ((payload >> i) & 1) as u8);
        }

        // Read command (0b11) for the same block.
        eeprom_send(&mut chip, &eeprom_command(0b11, 3, 14));
        // Four dummy bits, then the payload most significant bit first.
        for _ in 0..4 {
            assert_eq!(chip.read_byte(0), 0);
        }
        let mut received = 0u64;
        for _ in 0..64 {
            received = (received << 1) | chip.read_byte(0) as u64;
        }
        assert_eq!(received, payload);
    }

    #[test]
    fn eeprom_address_width_follows_capacity() {
        // A 512-byte chip uses 6 address bits, so the same command encoded for 14 would
        // desynchronize it.
        let mut small = Eeprom::new(512);
        eeprom_send(&mut small, &eeprom_command(0b10, 1, 6));
        for i in (0..64).rev() {
            small.write_byte(0, ((0xFFu64 >> (i % 8)) & 1) as u8);
        }
        assert!(small.is_dirty(), "the write completed with a 6-bit address");
    }

    #[test]
    fn eeprom_idles_high_between_transfers() {
        // Games poll the data line to see whether the chip is ready.
        let mut chip = Eeprom::new(512);
        assert_eq!(chip.read_byte(0), 1);
    }

    #[test]
    fn eeprom_round_trips_to_a_file_and_back() {
        let mut chip = Eeprom::new(512);
        eeprom_send(&mut chip, &eeprom_command(0b10, 0, 6));
        for _ in 0..64 {
            chip.write_byte(0, 1);
        }
        let dumped = chip.as_bytes().to_vec();

        let mut fresh = Eeprom::new(512);
        fresh.load_from_bytes(&dumped).unwrap();
        assert_eq!(fresh.as_bytes(), chip.as_bytes());
        assert_eq!(&fresh.as_bytes()[..8], &[0xFF; 8]);
    }

    // -- Save states ---------------------------------------------------------

    #[test]
    fn every_chip_round_trips_through_a_save_state_including_its_protocol_state() {
        use core_common::{StateReader, StateWriter};

        // Mid-sequence state matters: a state saved between the unlock and the command must
        // resume into the same place, not reset to Ready.
        let mut flash = Flash::new(65536);
        flash.write_byte(0x5555, 0xAA);
        flash.write_byte(0x2AAA, 0x55);

        let mut w = StateWriter::new();
        flash.save(&mut w);
        let blob = w.into_inner();
        let mut restored = Flash::new(65536);
        restored.load(&mut StateReader::new(&blob)).unwrap();
        assert_eq!(restored, flash);

        // Finishing the sequence on the restored chip must work.
        restored.write_byte(0x5555, 0xA0);
        restored.write_byte(0x10, 0x00);
        assert_eq!(restored.read_byte(0x10), 0x00);

        let mut eeprom = Eeprom::new(512);
        eeprom.write_byte(0, 1);
        eeprom.write_byte(0, 1);
        let mut w = StateWriter::new();
        eeprom.save(&mut w);
        let blob = w.into_inner();
        let mut restored = Eeprom::new(512);
        restored.load(&mut StateReader::new(&blob)).unwrap();
        assert_eq!(restored, eeprom);
    }

    #[test]
    fn create_save_builds_the_right_chip_or_none() {
        assert!(create_save(SaveKind::None).is_none());
        assert_eq!(
            create_save(SaveKind::Sram { size: 8192 }).unwrap().kind(),
            SaveKind::Sram { size: 8192 }
        );
        assert_eq!(
            create_save(SaveKind::Flash { size: 65536 }).unwrap().kind(),
            SaveKind::Flash { size: 65536 }
        );
        assert_eq!(
            create_save(SaveKind::Eeprom { size: 512 }).unwrap().kind(),
            SaveKind::Eeprom { size: 512 }
        );
    }
}
