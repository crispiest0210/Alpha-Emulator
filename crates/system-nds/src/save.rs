//! The cartridge save chip on the auxiliary SPI bus.
//!
//! # Nothing in the header says which chip is fitted
//!
//! A DS cartridge carries either an EEPROM or a FLASH chip, in one of six sizes, and the header
//! does not identify it. Guessing wrong does not fail loudly: it writes a file the game cannot
//! read back, and a player loses a save without ever being told why. That is why this module
//! existed as a stub returning `None` until the approach below was agreed.
//!
//! # The chip identifies itself by how software talks to it
//!
//! Every one of these chips speaks the same command set over the same three-wire bus, and the
//! thing that distinguishes them is **how many address bytes a command carries**: one for the
//! 512-byte EEPROM, two for the larger EEPROMs, three for FLASH. So rather than guess, this
//! buffers each transaction until the chip select is released and classifies it from its *total
//! length* — at which point the address width is known rather than assumed. Two commands are
//! decisive on their own: `RDID` exists only on FLASH, and the high-half read only on the
//! 512-byte EEPROM.
//!
//! # Nothing reaches the disk until the chip is known
//!
//! [`SaveChip::save_ram`] returns `None` while the type is undetermined, so the frontend writes no
//! file at all rather than one of the wrong shape. A read before classification returns `0xFF`,
//! which is what a blank chip of every one of these types reads as — so a fresh cartridge behaves
//! correctly during exactly the window where the type is still unknown.
//!
//! And once a save file exists, its **size** identifies the chip with no inference at all:
//! [`SaveChip::load_file`] takes the type straight from the file's length. The heuristic therefore only
//! ever runs on a cartridge's first save, and every session after that is certain.
//!
//! # What is not modelled
//!
//! Write timing. Hardware holds the write-in-progress flag for milliseconds after a page write and
//! software polls it; here a write completes within the transaction and the flag reads back clear
//! immediately. A game that polls sees it clear on the first try, which is a route hardware does
//! not take to an outcome that is correct.

use core_common::{CartridgeError, Savable, StateError, StateReader, StateWriter};

/// The chips a DS cartridge is fitted with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChipKind {
    /// 4 Kbit EEPROM. One address byte, and the read command's bit 3 selects the upper half.
    Eeprom512,
    /// 64 Kbit EEPROM.
    Eeprom8K,
    /// 512 Kbit EEPROM.
    Eeprom64K,
    /// 2 Mbit FLASH.
    Flash256K,
    /// 4 Mbit FLASH, the commonest fitting.
    Flash512K,
    /// 8 Mbit FLASH.
    Flash1M,
}

use ChipKind::*;

/// Every size a save file may legitimately be, smallest first.
pub const SIZES: [(usize, ChipKind); 6] = [
    (512, Eeprom512),
    (8 * 1024, Eeprom8K),
    (64 * 1024, Eeprom64K),
    (256 * 1024, Flash256K),
    (512 * 1024, Flash512K),
    (1024 * 1024, Flash1M),
];

impl ChipKind {
    pub const fn size(self) -> usize {
        match self {
            Eeprom512 => 512,
            Eeprom8K => 8 * 1024,
            Eeprom64K => 64 * 1024,
            Flash256K => 256 * 1024,
            Flash512K => 512 * 1024,
            Flash1M => 1024 * 1024,
        }
    }

    /// How many address bytes a command carries. This is the whole basis of the detection.
    pub const fn address_bytes(self) -> usize {
        match self {
            Eeprom512 => 1,
            Eeprom8K | Eeprom64K => 2,
            Flash256K | Flash512K | Flash1M => 3,
        }
    }

    /// The largest write a single transaction can carry.
    pub const fn page_size(self) -> usize {
        match self {
            Eeprom512 => 16,
            Eeprom8K => 32,
            Eeprom64K => 128,
            _ => 256,
        }
    }

    pub const fn is_flash(self) -> bool {
        matches!(self, Flash256K | Flash512K | Flash1M)
    }

    /// The chip a save file of this length came from.
    ///
    /// The most reliable signal there is, and the reason the heuristic only ever runs once per
    /// cartridge: after the first save exists, its size settles the question outright.
    pub fn from_file_size(len: usize) -> Option<Self> {
        SIZES.iter().find(|(size, _)| *size == len).map(|(_, k)| *k)
    }

    /// The chip a write of this total transaction length came from, or `None` when more than one
    /// is still possible.
    ///
    /// A write transaction is one command byte, then the address, then some data. Two things make
    /// a length decisive:
    ///
    /// - **The payload is exactly a page.** Save libraries write page-aligned blocks, and the four
    ///   page sizes at their three address widths give four distinct total lengths — 18, 35, 131,
    ///   260 — that no other chip can produce.
    /// - **The payload is too large for any narrower chip's page.** Past 131 bytes only the
    ///   three-byte address width fits at all.
    ///
    /// Everything else is genuinely ambiguous. A five-byte transaction is a one-byte write on all
    /// three widths, and there is no way to tell from it — so this says `None` and the caller
    /// holds the write rather than committing to a guess. Guessing wrong here is not a cosmetic
    /// error: it parses part of the address as data, or part of the data as address, and writes to
    /// the wrong place.
    fn from_write_length(total: usize) -> Option<Self> {
        // A payload that is exactly one chip's page, which is how software actually writes.
        for kind in [Eeprom512, Eeprom8K, Eeprom64K, Flash256K] {
            if total == 1 + kind.address_bytes() + kind.page_size() {
                return Some(kind);
            }
        }
        // Too much data for two address bytes to be carrying a whole page.
        if total > 1 + 2 + Eeprom64K.page_size() {
            return Some(Flash256K);
        }
        None
    }
}

mod command {
    pub const WRITE_ENABLE: u8 = 0x06;
    pub const WRITE_DISABLE: u8 = 0x04;
    pub const READ_STATUS: u8 = 0x05;
    pub const WRITE_STATUS: u8 = 0x01;
    pub const READ: u8 = 0x03;
    /// The 512-byte EEPROM's upper half, and nothing else's command.
    pub const READ_HIGH: u8 = 0x0B;
    pub const WRITE: u8 = 0x02;
    /// Likewise, the 512-byte EEPROM's upper half.
    pub const WRITE_HIGH: u8 = 0x0A;
    /// FLASH only. Software probes with it, which makes it decisive.
    pub const READ_ID: u8 = 0x9F;
    pub const PAGE_ERASE: u8 = 0xDB;
    pub const SECTOR_ERASE: u8 = 0xD8;
    pub const CHIP_ERASE: u8 = 0xC7;
}

/// Status register bits.
const STATUS_WRITE_IN_PROGRESS: u8 = 1 << 0;
const STATUS_WRITE_ENABLED: u8 = 1 << 1;

/// How many ambiguous writes to hold before giving up on them.
///
/// A bound rather than an unbounded queue, because a cartridge that never writes a full page and
/// never probes would otherwise grow this forever. Reaching it is logged, since it means the
/// detection has genuinely failed rather than merely not finished.
const HELD_WRITE_LIMIT: usize = 64;

/// The save chip and the transaction currently in flight.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SaveChip {
    kind: Option<ChipKind>,
    data: Vec<u8>,
    status: u8,
    /// Bytes shifted in since the chip select was asserted, command byte included.
    transaction: Vec<u8>,
    /// Writes that arrived before the chip type was known and whose length did not settle it.
    ///
    /// Held rather than applied, and replayed once something decisive arrives. Guessing the
    /// address width wrong writes to the wrong place, so the only safe thing to do with an
    /// ambiguous write is to wait — and a write nobody can place yet is one nobody can read back
    /// yet either, so holding it is invisible to the game.
    held: Vec<Vec<u8>>,
    /// Set by any write that changed the data, and cleared by the frontend after a flush.
    dirty: bool,
}

impl Default for SaveChip {
    fn default() -> Self {
        Self::new()
    }
}

impl SaveChip {
    pub fn new() -> Self {
        Self {
            kind: None,
            data: Vec::new(),
            status: 0,
            transaction: Vec::new(),
            held: Vec::new(),
            dirty: false,
        }
    }

    pub fn kind(&self) -> Option<ChipKind> {
        self.kind
    }

    /// The save data, or `None` while the chip type is still unknown.
    ///
    /// `None` is what keeps a wrong guess off the disk: the frontend writes no file rather than
    /// one of a shape the game will later fail to read.
    pub fn save_ram(&self) -> Option<&[u8]> {
        self.kind.map(|_| self.data.as_slice())
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    pub fn clear_dirty(&mut self) {
        self.dirty = false;
    }

    /// Install a save file, taking the chip type from its size.
    ///
    /// Named `load_file` rather than `load` because an inherent method silently shadows a trait
    /// method of the same name, and `Savable::load` is the other one. This project has now paid
    /// for that trap three times — the SM83's `is_halted`, `MatrixStack::load`, and here.
    pub fn load_file(&mut self, data: &[u8]) -> Result<(), CartridgeError> {
        let Some(kind) = ChipKind::from_file_size(data.len()) else {
            return Err(CartridgeError::SaveSizeMismatch {
                expected: Flash512K.size(),
                found: data.len(),
            });
        };
        self.adopt(kind);
        self.data.copy_from_slice(data);
        self.dirty = false;
        Ok(())
    }

    /// Settle on a chip type, size the backing store, and apply anything that was waiting.
    fn adopt(&mut self, kind: ChipKind) {
        self.kind = Some(kind);
        // Erased is all ones on both technologies, so a chip nobody has written reads blank
        // rather than as zeroes a game might mistake for real data.
        self.data.resize(kind.size(), 0xFF);

        // The replay borrows `self.transaction`, and this may be called from the middle of a
        // transaction — a `RDID` probe identifies the chip on its second byte, with three more
        // still to come. Stash and restore it, or the transaction being served is destroyed by
        // the act of identifying the chip it is being served from.
        let in_flight = std::mem::take(&mut self.transaction);
        for transaction in std::mem::take(&mut self.held) {
            let command = transaction[0];
            self.transaction = transaction;
            self.status |= STATUS_WRITE_ENABLED;
            self.apply_write(command);
        }
        self.transaction = in_flight;
    }

    /// How many writes are waiting for the chip type to be settled.
    ///
    /// Exposed so a test can assert the holding happens, and so a future diagnostic can say "this
    /// game has written twice and still not told us what chip it has".
    pub fn held_writes(&self) -> usize {
        self.held.len()
    }

    /// Grow a FLASH chip to cover an address a game has actually touched.
    ///
    /// Only ever upward, and only between the three FLASH sizes. The alternative — committing to
    /// 512 KiB because it is the commonest — silently drops the upper half of a 1 MiB cartridge's
    /// save, which is the failure this whole module is arranged to avoid.
    fn grow_for(&mut self, addr: usize) {
        let Some(kind) = self.kind else { return };
        if !kind.is_flash() || addr < kind.size() {
            return;
        }
        let larger = SIZES
            .iter()
            .find(|(size, k)| k.is_flash() && *size > addr)
            .map(|(_, k)| *k);
        if let Some(larger) = larger {
            tracing::debug!(
                "save chip grew from {:?} to {larger:?} for an access at {addr:#X}",
                kind
            );
            self.kind = Some(larger);
            self.data.resize(larger.size(), 0xFF);
        }
    }

    /// Shift one byte through the chip.
    ///
    /// `hold` is the chip-select line: while it is set the transaction continues, and the byte it
    /// is clear on is the last. Classification happens on that final byte, because that is the
    /// first moment the transaction's total length — and so the address width — is known.
    pub fn transfer(&mut self, byte: u8, hold: bool) -> u8 {
        self.transaction.push(byte);
        let out = self.respond(byte);
        if !hold {
            self.finish();
        }
        out
    }

    /// What the chip drives back for the byte just shifted in.
    fn respond(&mut self, _byte: u8) -> u8 {
        let command = self.transaction[0];
        let position = self.transaction.len() - 1;
        if position == 0 {
            // The command byte itself is answered with nothing.
            return 0xFF;
        }
        match command {
            command::READ_STATUS => self.status & !STATUS_WRITE_IN_PROGRESS,
            // A JEDEC identifier. Only FLASH answers this, so being asked is itself the answer to
            // "which technology is fitted".
            command::READ_ID => {
                if self.kind.is_none() {
                    self.adopt(Flash256K);
                }
                match position {
                    1 => 0x20,
                    2 => 0x40,
                    _ => 0x12,
                }
            }
            command::READ | command::READ_HIGH => {
                if command == command::READ_HIGH && self.kind.is_none() {
                    // The upper-half read exists only on the 512-byte EEPROM.
                    self.adopt(Eeprom512);
                }
                let Some(kind) = self.kind else {
                    // Blank is all ones on every one of these chips, so this is the right answer
                    // during exactly the window where the type is still unknown.
                    return 0xFF;
                };
                let address_bytes = kind.address_bytes();
                if position <= address_bytes {
                    return 0xFF;
                }
                let mut addr = self.address(address_bytes);
                if command == command::READ_HIGH {
                    addr += 0x100;
                }
                let offset = addr + (position - address_bytes - 1);
                self.data
                    .get(offset % kind.size().max(1))
                    .copied()
                    .unwrap_or(0xFF)
            }
            _ => 0xFF,
        }
    }

    /// The address the current transaction carries, big-endian, as these chips send it.
    fn address(&self, bytes: usize) -> usize {
        let mut addr = 0usize;
        for i in 0..bytes {
            addr = (addr << 8) | self.transaction.get(1 + i).copied().unwrap_or(0) as usize;
        }
        addr
    }

    /// The chip select went low: apply whatever the transaction asked for.
    fn finish(&mut self) {
        let command = self.transaction.first().copied().unwrap_or(0);
        match command {
            command::WRITE_ENABLE => self.status |= STATUS_WRITE_ENABLED,
            command::WRITE_DISABLE => self.status &= !STATUS_WRITE_ENABLED,
            command::WRITE_STATUS => {
                if let Some(value) = self.transaction.get(1) {
                    // Only the block-protect bits are writable; the two status flags are the
                    // chip's own.
                    self.status = (self.status & 0x03) | (value & 0x0C);
                }
            }
            command::WRITE | command::WRITE_HIGH => self.apply_write(command),
            command::PAGE_ERASE | command::SECTOR_ERASE => self.apply_erase(command),
            command::CHIP_ERASE => {
                self.data.fill(0xFF);
                self.dirty = true;
                self.status &= !STATUS_WRITE_ENABLED;
            }
            _ => {}
        }
        self.transaction.clear();
    }

    fn apply_write(&mut self, command: u8) {
        if self.status & STATUS_WRITE_ENABLED == 0 {
            // A chip with the write-enable latch clear ignores the write, and software that
            // forgot `WREN` gets nothing rather than a save.
            self.transaction.clear();
            return;
        }
        if self.kind.is_none() {
            // The transaction is complete, so its total length is known — which is the first
            // moment the address width can be read off it at all.
            let detected = if command == command::WRITE_HIGH {
                Some(Eeprom512)
            } else {
                ChipKind::from_write_length(self.transaction.len())
            };
            match detected {
                Some(kind) => {
                    tracing::info!(
                        "save chip detected as {kind:?} from a {}-byte write",
                        self.transaction.len()
                    );
                    self.adopt(kind);
                }
                None => {
                    // Ambiguous. Hold it rather than guess; see `from_write_length`.
                    if self.held.len() < HELD_WRITE_LIMIT {
                        self.held.push(std::mem::take(&mut self.transaction));
                    } else {
                        tracing::warn!(
                            "dropping a save write: {} writes held and none has identified the \
                             chip. The cartridge writes only partial pages and never probes, \
                             which this detection cannot resolve.",
                            self.held.len()
                        );
                    }
                    self.status &= !STATUS_WRITE_ENABLED;
                    self.transaction.clear();
                    return;
                }
            }
        }
        let kind = self.kind.expect("just adopted");
        let address_bytes = kind.address_bytes();
        let mut addr = self.address(address_bytes);
        if command == command::WRITE_HIGH {
            addr += 0x100;
        }
        let payload: Vec<u8> =
            self.transaction[(1 + address_bytes).min(self.transaction.len())..].to_vec();
        if let Some(last) = payload.len().checked_sub(1) {
            self.grow_for(addr + last);
        }
        let kind = self.kind.expect("still adopted");
        for (i, value) in payload.iter().enumerate() {
            // A page write wraps within its page rather than running into the next one, which is
            // how these chips behave and what a game relies on when it writes a partial page.
            let page = kind.page_size();
            let offset = (addr & !(page - 1)) + ((addr + i) & (page - 1));
            if let Some(slot) = self.data.get_mut(offset % kind.size().max(1)) {
                if *slot != *value {
                    *slot = *value;
                    self.dirty = true;
                }
            }
        }
        self.status &= !STATUS_WRITE_ENABLED;
    }

    fn apply_erase(&mut self, command: u8) {
        let Some(kind) = self.kind else { return };
        if self.status & STATUS_WRITE_ENABLED == 0 {
            return;
        }
        let address_bytes = kind.address_bytes();
        let addr = self.address(address_bytes);
        let span = if command == command::PAGE_ERASE {
            256
        } else {
            0x1_0000
        };
        let start = addr & !(span - 1);
        for offset in start..(start + span).min(self.data.len()) {
            if self.data[offset] != 0xFF {
                self.data[offset] = 0xFF;
                self.dirty = true;
            }
        }
        self.status &= !STATUS_WRITE_ENABLED;
    }

    /// Abandon a transaction, which is what a reset does mid-flight.
    pub fn reset(&mut self) {
        self.transaction.clear();
        self.status = 0;
    }
}

impl Savable for SaveChip {
    fn save(&self, w: &mut StateWriter) {
        w.write_u8(match self.kind {
            None => 0,
            Some(kind) => SIZES.iter().position(|(_, k)| *k == kind).unwrap() as u8 + 1,
        });
        w.write_blob(&self.data);
        w.write_u8(self.status);
        w.write_bool(self.dirty);
        // Held writes *are* data the game wrote, unlike the in-flight transaction, so they have
        // to survive: a state taken before the chip identified itself would otherwise lose them.
        w.write_u64(self.held.len() as u64);
        for transaction in &self.held {
            w.write_blob(transaction);
        }
        // The in-flight transaction is not saved. A save state is taken between instructions and
        // a SPI transaction spans several, but restoring one would restore the emulator's idea of
        // a bus rather than the machine — and an abandoned transaction is what a reset produces
        // on hardware anyway.
    }

    fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        let index = r.read_u8()?;
        self.kind = match index {
            0 => None,
            n if (n as usize) <= SIZES.len() => Some(SIZES[n as usize - 1].1),
            n => {
                return Err(StateError::Malformed(format!(
                    "save chip kind {n} is not one this build knows"
                )))
            }
        };
        self.data = r.read_blob()?.to_vec();
        if let Some(kind) = self.kind {
            if self.data.len() != kind.size() {
                return Err(StateError::Malformed(format!(
                    "save chip is {:?} but its data is {} bytes",
                    kind,
                    self.data.len()
                )));
            }
        }
        self.status = r.read_u8()?;
        self.dirty = r.read_bool()?;
        let held = r.read_u64()? as usize;
        if held > HELD_WRITE_LIMIT {
            return Err(StateError::Malformed(format!(
                "{held} held save writes is more than the {HELD_WRITE_LIMIT} limit"
            )));
        }
        self.held.clear();
        for _ in 0..held {
            self.held.push(r.read_blob()?.to_vec());
        }
        self.transaction.clear();
        Ok(())
    }
}

#[cfg(test)]
mod tests;
