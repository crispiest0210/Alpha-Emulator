//! OAM DMA, behind `FF46`: 160 bytes into sprite memory, one byte per machine cycle.
//!
//! # Why this is a state machine and not a loop
//!
//! The copy takes 160 machine cycles on hardware and the CPU keeps running throughout — that
//! is the whole point of the transfer, and it is why every game triggers one from a routine
//! copied into HRAM and spins there until it is done. A synchronous loop lands the same bytes
//! but makes the transfer instantaneous, so nothing can observe it being *in progress*: the
//! bus conflicts below stop existing, and code that runs during the transfer sees an OAM that
//! is already finished.
//!
//! So this owns "a transfer is running and has been for N cycles" and is stepped by the normal
//! clock, exactly as [`Hdma`](crate::cgb::Hdma) is. It follows that module's split too: it
//! decides *what* to copy and the bus moves the bytes, because the source crosses cartridge,
//! work RAM, and VRAM banking that only the memory map knows how to resolve.
//!
//! # The two-cycle delay
//!
//! A write to `FF46` does not start the transfer; the transfer starts two machine cycles
//! later. Between the write and the start, OAM is still fully readable, and if a transfer was
//! already running it keeps running — a restart does not cancel the old transfer early, it
//! replaces it when the new one actually begins. Mooneye's `oam_dma_start` exists to pin down
//! exactly this, and gets the answer wrong in both directions if the delay is dropped or if a
//! restart takes effect immediately.
//!
//! # Which bus a transfer occupies
//!
//! The DMA reads through one of the two memory buses, and only the CPU accesses that would
//! contend for *that* bus are locked out. A transfer sourced from VRAM leaves the cartridge and
//! work RAM readable; one sourced from the cartridge leaves VRAM readable. OAM itself is
//! locked out either way, because the DMA is writing it. The lockout rule lives on the bus with
//! the memory map, in [`crate::system`]; what belongs here is only which bus the source is on.

use core_common::{Savable, StateError, StateReader, StateWriter};

/// `0xFF46`: writing a page number starts a transfer from that page.
pub const DMA: u16 = 0xFF46;

/// Bytes moved, which is the whole of OAM.
pub const OAM_BYTES: u16 = 0xA0;

/// T-cycles per byte. One byte per machine cycle.
const BYTE_CYCLES: u32 = 4;

/// T-cycles a whole transfer takes: 160 machine cycles.
pub const TRANSFER_CYCLES: u32 = OAM_BYTES as u32 * BYTE_CYCLES;

/// T-cycles between the write to `FF46` and the transfer actually starting.
const STARTUP_CYCLES: u32 = 2 * BYTE_CYCLES;

/// Which of the two memory buses an address sits on.
///
/// The unusable region and the whole of `FF00`-`FFFF` are on neither: I/O registers and HRAM
/// are internal to the CPU, which is why a game's wait loop can live in HRAM and still poll
/// registers while a transfer runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryBus {
    /// Cartridge, work RAM, and the echo region.
    External,
    /// VRAM and OAM.
    Video,
    /// Reached without either bus.
    Internal,
}

impl MemoryBus {
    /// Which bus the CPU uses to reach `addr`.
    pub fn of(addr: u16) -> Self {
        match addr {
            0x0000..=0x7FFF | 0xA000..=0xFDFF => Self::External,
            0x8000..=0x9FFF | 0xFE00..=0xFE9F => Self::Video,
            _ => Self::Internal,
        }
    }

    /// Which bus a transfer from `page` reads through.
    ///
    /// Pages at or above `0xE0` mirror work RAM the same way the echo region does, so they
    /// read through the external bus like the work RAM they alias.
    pub fn of_dma_source(page: u8) -> Self {
        match page {
            0x80..=0x9F => Self::Video,
            _ => Self::External,
        }
    }
}

/// The OAM DMA controller.
///
/// `Copy` and eight bytes wide: the bus keeps one inline rather than behind a pointer, and the
/// idle case — every cycle of every frame that is not one of the 160 — costs one branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct OamDma {
    /// A write to `FF46` that has not taken effect yet: the page, and the t-cycles left before
    /// the transfer it asked for begins.
    starting: Option<(u8, u32)>,
    /// The transfer currently moving bytes: the page, and the t-cycles it has been running.
    running: Option<(u8, u32)>,
}

/// One byte the bus must move, produced by [`OamDma::step`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Copy {
    /// Absolute address to read.
    pub source: u16,
    /// Offset within OAM to write.
    pub offset: u16,
}

impl OamDma {
    pub fn new() -> Self {
        Self::default()
    }

    /// Handle a write to `FF46`.
    ///
    /// Only ever *schedules*: any transfer already running is left alone until this one starts.
    pub fn request(&mut self, page: u8) {
        self.starting = Some((page, STARTUP_CYCLES));
    }

    /// Whether anything is scheduled or running, so the bus can skip the stepping entirely.
    pub fn is_idle(&self) -> bool {
        self.starting.is_none() && self.running.is_none()
    }

    /// The bus a running transfer is occupying, if one is running.
    ///
    /// `None` during the startup delay, which is the point of the delay: nothing is locked out
    /// until the transfer really begins.
    pub fn busy_bus(&self) -> Option<MemoryBus> {
        self.running.map(|(page, _)| MemoryBus::of_dma_source(page))
    }

    /// Whether a transfer is moving bytes right now.
    pub fn is_running(&self) -> bool {
        self.running.is_some()
    }

    /// Advance by one t-cycle, returning the byte to copy if one falls due on this cycle.
    ///
    /// Stepped a single t-cycle at a time rather than in lumps, because the CPU reports a
    /// partial machine cycle at the end of some instructions and a lump would have to reason
    /// about where inside a byte it landed. It only runs while a transfer is in flight — 648
    /// t-cycles per frame that uses one — so the exactness is free.
    pub fn step(&mut self) -> Option<Copy> {
        if let Some((page, delay)) = self.starting {
            let delay = delay - 1;
            if delay == 0 {
                self.starting = None;
                self.running = Some((page, 0));
                // The transfer exists as of this cycle but has not run one yet, so it moves
                // no byte here. Any transfer it replaced is over.
                return None;
            }
            self.starting = Some((page, delay));
        }

        let (page, elapsed) = self.running?;
        let elapsed = elapsed + 1;
        self.running = (elapsed < TRANSFER_CYCLES).then_some((page, elapsed));

        (elapsed % BYTE_CYCLES == 0).then(|| {
            let offset = elapsed / BYTE_CYCLES - 1;
            Copy {
                source: ((page as u16) << 8) | offset as u16,
                offset: offset as u16,
            }
        })
    }
}

impl Savable for OamDma {
    fn save(&self, w: &mut StateWriter) {
        let (page, delay) = self.starting.unwrap_or((0, 0));
        w.write_u8(page);
        w.write_u32(delay);
        let (page, elapsed) = self.running.unwrap_or((0, 0));
        w.write_u8(page);
        w.write_bool(self.running.is_some());
        w.write_u32(elapsed);
    }

    fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        let page = r.read_u8()?;
        let delay = r.read_u32()?;
        // A zero delay is unrepresentable while starting — the transfer begins at zero — so it
        // encodes "nothing scheduled" without a second flag.
        self.starting = (delay > 0).then_some((page, delay));
        let page = r.read_u8()?;
        // A running transfer *can* legitimately be at elapsed zero, for the one cycle between
        // the delay expiring and its first byte, so this one needs its own flag.
        let running = r.read_bool()?;
        let elapsed = r.read_u32()?;
        self.running = running.then_some((page, elapsed));
        Ok(())
    }
}

#[cfg(test)]
mod tests;
