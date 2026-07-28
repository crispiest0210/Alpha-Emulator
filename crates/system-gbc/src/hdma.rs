//! CGB VRAM DMA, behind `HDMA1`–`HDMA5`.
//!
//! Two transfer modes share one register block. General-purpose DMA copies everything at once
//! and stalls the CPU until it is done. HBlank DMA copies sixteen bytes at the start of each
//! horizontal blank and lets the CPU run in between — which is how a CGB game streams a new
//! tile set in mid-frame without a visible seam.
//!
//! # Why this owns no memory
//!
//! The controller decides *what* to copy and *when*, and returns the block to copy rather than
//! performing it. The copy itself crosses the cartridge, work RAM, and VRAM banking that
//! `system-gb`'s bus owns, and reaching into that from here would mean duplicating the memory
//! map. So [`Hdma::take_block`] hands back a source and destination and the caller moves the
//! bytes — the same split as the OAM DMA already in `system-gb`.
//!
//! Only the HBlank *trigger* needs a PPU hook that does not exist yet; everything below is
//! independent of it.

use core_common::{Savable, StateError, StateReader, StateWriter};

/// Register addresses.
pub mod reg {
    pub const HDMA1: u16 = 0xFF51;
    pub const HDMA2: u16 = 0xFF52;
    pub const HDMA3: u16 = 0xFF53;
    pub const HDMA4: u16 = 0xFF54;
    /// Length, mode, and start — writing it is what launches a transfer.
    pub const HDMA5: u16 = 0xFF55;
}

/// Bytes moved per HBlank, and the granularity of the length field.
pub const BLOCK_BYTES: u16 = 16;

/// A block of bytes for the caller to copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Block {
    /// Absolute address to read from.
    pub source: u16,
    /// Absolute address to write to; always inside VRAM.
    pub destination: u16,
    pub length: u16,
}

/// Which transfer is running.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Mode {
    #[default]
    Idle,
    /// Copy everything now, stalling the CPU.
    GeneralPurpose,
    /// Copy one block per HBlank.
    HBlank,
}

/// The VRAM DMA controller.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Hdma {
    source: u16,
    destination: u16,
    /// Blocks still to copy, minus one — the same encoding `HDMA5` uses.
    remaining: u8,
    mode: Mode,
}

impl Hdma {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn owns(addr: u16) -> bool {
        (reg::HDMA1..=reg::HDMA5).contains(&addr)
    }

    /// Whether an HBlank transfer is waiting for the next horizontal blank.
    pub fn is_hblank_pending(&self) -> bool {
        matches!(self.mode, Mode::HBlank)
    }

    /// `HDMA1`–`HDMA4` are write-only; only `HDMA5` reads back.
    ///
    /// Bit 7 reads *low* while a transfer is active and high when none is — inverted from the
    /// bit that starts one. A game polls this to know when its transfer finished, so getting
    /// the sense backwards hangs the poll loop forever.
    pub fn read_register(&self, addr: u16) -> Option<u8> {
        match addr {
            reg::HDMA5 => Some(match self.mode {
                Mode::Idle => 0xFF,
                _ => self.remaining & 0x7F,
            }),
            reg::HDMA1..=reg::HDMA4 => Some(0xFF),
            _ => None,
        }
    }

    /// Returns `Some(true)` when the write launched a general-purpose transfer, which the
    /// caller must complete immediately by draining [`Hdma::take_block`].
    pub fn write_register(&mut self, addr: u16, value: u8) -> Option<bool> {
        match addr {
            // The low four bits of the source are ignored — transfers are 16-byte aligned.
            reg::HDMA1 => self.source = (self.source & 0x00FF) | ((value as u16) << 8),
            reg::HDMA2 => self.source = (self.source & 0xFF00) | (value & 0xF0) as u16,
            // The destination is always in VRAM, so only its offset within 0x8000-0x9FFF is
            // configurable; the top bits are forced rather than trusted.
            reg::HDMA3 => {
                self.destination = (self.destination & 0x00FF) | (((value & 0x1F) as u16) << 8)
            }
            reg::HDMA4 => self.destination = (self.destination & 0xFF00) | (value & 0xF0) as u16,
            reg::HDMA5 => return Some(self.start(value)),
            _ => return None,
        }
        Some(false)
    }

    /// Handle a write to `HDMA5`.
    fn start(&mut self, value: u8) -> bool {
        let hblank = value & 0x80 != 0;

        // Writing with bit 7 clear while an HBlank transfer is running *cancels* it rather
        // than starting a general-purpose one. This is the register's most surprising
        // behaviour and the reason `start` exists as its own function: a game that cancels a
        // streaming transfer and one that begins a blocking copy write the same bit pattern,
        // and only the current mode tells them apart.
        if !hblank && matches!(self.mode, Mode::HBlank) {
            self.mode = Mode::Idle;
            return false;
        }

        self.remaining = value & 0x7F;
        self.mode = if hblank {
            Mode::HBlank
        } else {
            Mode::GeneralPurpose
        };
        !hblank
    }

    /// The next block to copy, advancing the transfer.
    ///
    /// Returns `None` when nothing is due — either no transfer is active, or an HBlank
    /// transfer is waiting for its next horizontal blank. Call it repeatedly for a
    /// general-purpose transfer and exactly once per HBlank for a streaming one.
    pub fn take_block(&mut self) -> Option<Block> {
        if matches!(self.mode, Mode::Idle) {
            return None;
        }

        let block = Block {
            source: self.source,
            // Kept as an offset inside VRAM and re-based on every block, so a transfer that
            // runs past the end of VRAM wraps within it instead of walking into the memory map
            // beyond — which is what the hardware's 13-bit destination counter does.
            destination: 0x8000 | (self.destination & 0x1FFF),
            length: BLOCK_BYTES,
        };

        self.source = self.source.wrapping_add(BLOCK_BYTES);
        self.destination = self.destination.wrapping_add(BLOCK_BYTES);

        // `remaining` counts blocks minus one, so zero means this was the last.
        if self.remaining == 0 {
            self.mode = Mode::Idle;
        } else {
            self.remaining -= 1;
        }
        Some(block)
    }
}

impl Savable for Hdma {
    fn save(&self, w: &mut StateWriter) {
        w.write_u16(self.source);
        w.write_u16(self.destination);
        w.write_u8(self.remaining);
        w.write_u8(match self.mode {
            Mode::Idle => 0,
            Mode::GeneralPurpose => 1,
            Mode::HBlank => 2,
        });
    }

    fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        self.source = r.read_u16()?;
        self.destination = r.read_u16()?;
        self.remaining = r.read_u8()?;
        self.mode = match r.read_u8()? {
            0 => Mode::Idle,
            1 => Mode::GeneralPurpose,
            2 => Mode::HBlank,
            other => return Err(StateError::Malformed(format!("bad HDMA mode {other}"))),
        };
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Point a transfer at a source and destination without starting it.
    fn armed(source: u16, destination: u16) -> Hdma {
        let mut h = Hdma::new();
        h.write_register(reg::HDMA1, (source >> 8) as u8);
        h.write_register(reg::HDMA2, source as u8);
        h.write_register(reg::HDMA3, (destination >> 8) as u8);
        h.write_register(reg::HDMA4, destination as u8);
        h
    }

    #[test]
    fn a_general_purpose_transfer_reports_itself_as_immediate() {
        let mut h = armed(0x4000, 0x8000);
        assert_eq!(
            h.write_register(reg::HDMA5, 0x00),
            Some(true),
            "the caller must complete it now"
        );
    }

    #[test]
    fn an_hblank_transfer_waits_rather_than_running_immediately() {
        let mut h = armed(0x4000, 0x8000);
        assert_eq!(h.write_register(reg::HDMA5, 0x80), Some(false));
        assert!(h.is_hblank_pending());
    }

    #[test]
    fn a_length_of_zero_still_copies_one_block() {
        // HDMA5 stores blocks minus one, so 0x00 means sixteen bytes, not nothing. Reading it
        // as "no work" is a silent no-op that looks like a broken game.
        let mut h = armed(0x4000, 0x8000);
        h.write_register(reg::HDMA5, 0x00);
        assert_eq!(
            h.take_block(),
            Some(Block {
                source: 0x4000,
                destination: 0x8000,
                length: BLOCK_BYTES
            })
        );
        assert_eq!(h.take_block(), None, "and then it is finished");
    }

    #[test]
    fn each_block_advances_both_pointers() {
        let mut h = armed(0x4000, 0x8000);
        h.write_register(reg::HDMA5, 0x02); // three blocks
        for step in 0..3u16 {
            assert_eq!(
                h.take_block(),
                Some(Block {
                    source: 0x4000 + step * BLOCK_BYTES,
                    destination: 0x8000 + step * BLOCK_BYTES,
                    length: BLOCK_BYTES
                }),
                "block {step}"
            );
        }
        assert_eq!(h.take_block(), None);
    }

    #[test]
    fn the_source_low_nibble_and_destination_high_bits_are_forced() {
        // Transfers are 16-byte aligned and the destination is always inside VRAM. Trusting
        // the written values would let a game DMA into work RAM.
        let mut h = armed(0xFFFF, 0xFFFF);
        h.write_register(reg::HDMA5, 0x00);
        let block = h.take_block().unwrap();
        assert_eq!(block.source & 0x000F, 0, "source is 16-byte aligned");
        assert!(
            (0x8000..0xA000).contains(&block.destination),
            "destination landed outside VRAM: {:#06X}",
            block.destination
        );
    }

    #[test]
    fn the_destination_wraps_within_vram_rather_than_running_past_it() {
        let mut h = armed(0x4000, 0x9FF0);
        h.write_register(reg::HDMA5, 0x01); // two blocks; the second would overrun
        assert_eq!(h.take_block().unwrap().destination, 0x9FF0);
        assert_eq!(
            h.take_block().unwrap().destination,
            0x8000,
            "wrapped back into VRAM instead of into OAM"
        );
    }

    #[test]
    fn clearing_bit_seven_cancels_a_running_hblank_transfer() {
        // The register's most surprising behaviour: the same bit pattern that starts a
        // general-purpose transfer instead cancels a streaming one, and only the current mode
        // tells the two apart.
        let mut h = armed(0x4000, 0x8000);
        h.write_register(reg::HDMA5, 0x83);
        assert!(h.is_hblank_pending());

        assert_eq!(
            h.write_register(reg::HDMA5, 0x00),
            Some(false),
            "cancelling must not read as a new immediate transfer"
        );
        assert!(!h.is_hblank_pending());
        assert_eq!(h.take_block(), None);
    }

    #[test]
    fn hdma5_reports_activity_with_bit_seven_inverted() {
        // Low while running, high when idle — a game polls this to know its transfer finished,
        // so the wrong sense hangs the poll loop forever.
        let mut h = armed(0x4000, 0x8000);
        assert_eq!(h.read_register(reg::HDMA5), Some(0xFF), "idle");

        h.write_register(reg::HDMA5, 0x82);
        assert_eq!(h.read_register(reg::HDMA5), Some(0x02), "two blocks left");
        h.take_block();
        assert_eq!(h.read_register(reg::HDMA5), Some(0x01));
    }

    #[test]
    fn the_address_registers_are_write_only() {
        let h = armed(0x4000, 0x8000);
        for addr in reg::HDMA1..=reg::HDMA4 {
            assert_eq!(h.read_register(addr), Some(0xFF), "{addr:#06X}");
        }
    }

    #[test]
    fn addresses_outside_the_block_are_not_claimed() {
        let mut h = Hdma::new();
        assert!(!Hdma::owns(0xFF50));
        assert!(!Hdma::owns(0xFF56));
        assert_eq!(h.read_register(0xFF50), None);
        assert_eq!(h.write_register(0xFF56, 0), None);
    }

    #[test]
    fn a_transfer_in_progress_round_trips() {
        use savestate::{decode_state, encode_state};
        let mut h = armed(0x4000, 0x8100);
        h.write_register(reg::HDMA5, 0x85);
        h.take_block();

        let bytes = encode_state("gbc-hdma", 1, &h);
        let mut restored = Hdma::new();
        decode_state("gbc-hdma", 1, &bytes, &mut restored).unwrap();
        assert_eq!(h, restored);
        assert_eq!(
            restored.take_block(),
            h.clone().take_block(),
            "and resumes at the same block"
        );
    }
}
