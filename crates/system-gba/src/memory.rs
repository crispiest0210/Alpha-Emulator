//! The Game Boy Advance memory map.
//!
//! Follows prompt 06's pattern and prompt 11's proven assembly, but the GBA's map is a
//! different shape from the Game Boy's in three ways that drive the code below.
//!
//! # Regions are selected by one nibble
//!
//! Every region sits at a distinct multiple of `0x0100_0000`, so bits 24-27 of the address name
//! the region and nothing else has to be compared. That is why this is a `match` on a shifted
//! nibble rather than the range table a flatter map would want.
//!
//! # Mirroring is not uniform
//!
//! EWRAM, IWRAM, palette RAM, and OAM each mirror their whole 16 MiB region by simple masking.
//! VRAM does not: it is 96 KiB inside a 128 KiB window, and the last 32 KiB is a second view of
//! the preceding 32 KiB rather than of the start. See [`vram_offset`].
//!
//! # Reads outside anything return the bus, not zero
//!
//! Unmapped addresses, and BIOS reads from outside the BIOS, return whatever was last driven on
//! the bus. Games read there by accident and a few read there deliberately; returning zero is a
//! visible difference. See [`GbaBus::open_bus32`] for unmapped addresses; a BIOS read from
//! outside the BIOS is a separate, sticky mechanism — see the `bios_open_bus` field's own docs.

use core_common::{Savable, StateError, StateReader, StateWriter};

pub const BIOS_SIZE: usize = 0x4000;
pub const EWRAM_SIZE: usize = 0x0004_0000;
pub const IWRAM_SIZE: usize = 0x8000;
pub const PALETTE_SIZE: usize = 0x400;
pub const VRAM_SIZE: usize = 0x0001_8000;
pub const OAM_SIZE: usize = 0x400;

/// The region a physical address falls in.
///
/// Named rather than left as a nibble so that a `match` on it reads as the memory map and the
/// compiler catches an unhandled region when one is added.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Region {
    Bios,
    /// External work RAM: larger, slower, and on a 16-bit bus.
    EWram,
    /// Internal work RAM: smaller, and the only RAM at full 32-bit speed.
    IWram,
    Io,
    Palette,
    Vram,
    Oam,
    /// Cartridge ROM. The three windows differ only in wait-state configuration; the same ROM
    /// answers all of them, which is how a game switches timing without moving its data.
    Rom {
        wait_state: u8,
    },
    /// Cartridge save chip: 8-bit only, and not the same thing as ROM.
    Sram,
    Unmapped,
}

impl Region {
    #[inline]
    pub fn of(addr: u32) -> Self {
        match addr >> 24 {
            0x00 => Region::Bios,
            0x02 => Region::EWram,
            0x03 => Region::IWram,
            0x04 => Region::Io,
            0x05 => Region::Palette,
            0x06 => Region::Vram,
            0x07 => Region::Oam,
            0x08 | 0x09 => Region::Rom { wait_state: 0 },
            0x0A | 0x0B => Region::Rom { wait_state: 1 },
            0x0C | 0x0D => Region::Rom { wait_state: 2 },
            0x0E | 0x0F => Region::Sram,
            _ => Region::Unmapped,
        }
    }
}

/// Fold a VRAM address into the 96 KiB that physically exists.
///
/// VRAM mirrors every 128 KiB, but only 96 KiB is real. The gap is not left unmapped and does
/// not wrap to the start: the last 32 KiB of each window is a second view of the 32 KiB before
/// it. Games write sprite tiles through both views, so treating the gap as a plain wrap
/// corrupts object graphics in a way that looks like a tile-decoding bug.
#[inline]
pub fn vram_offset(addr: u32) -> usize {
    let offset = (addr & 0x0001_FFFF) as usize;
    if offset >= 0x0001_8000 {
        offset - 0x8000
    } else {
        offset
    }
}

/// Everything on the board except the cartridge and the timed subsystems.
pub struct GbaBus {
    /// The BIOS, if the user supplied one. None is supported: see the crate docs.
    bios: Option<Box<[u8]>>,
    ewram: Box<[u8]>,
    iwram: Box<[u8]>,
    palette: Box<[u8]>,
    vram: Box<[u8]>,
    oam: Box<[u8]>,

    /// The last value the CPU fetched, which unmapped reads return.
    ///
    /// Tracked as a full word because open-bus reads are word-granular on hardware: a byte read
    /// from nowhere returns the matching byte *of the last word*, not the last byte.
    open_bus: u32,

    /// The last opcode *the BIOS itself* fetched, which a read of BIOS memory from outside it
    /// returns.
    ///
    /// Deliberately not the same field as [`Self::open_bus`]. GBATEK documents these as two
    /// separate rules: an ordinary unmapped read mirrors the pipeline's own most recent fetch
    /// (`[$+8]` of the *reading* instruction, in ARM state), which changes on every instruction
    /// regardless of where it is fetched from; a BIOS read from outside the BIOS instead returns
    /// whatever the BIOS last fetched *from within itself* — sticky across every instruction the
    /// game executes afterward, since none of them are BIOS fetches. A machine with no BIOS
    /// supplied never executes real BIOS code to update this naturally, so the four moments a
    /// real BIOS would touch it — startup, a completed `SWI`, IRQ entry, and IRQ return — are
    /// each stamped with their documented constant by the HLE path that stands in for that
    /// moment. See `system::GbaSystem::intercept_bios_call` and its neighbours.
    bios_open_bus: u32,

    /// Whether the CPU is currently executing from BIOS.
    ///
    /// The BIOS is readable only by code running inside it. A game that reads BIOS from its own
    /// code gets open bus instead — which is exactly how anti-piracy checks in some cartridges
    /// detect an emulator that maps it unconditionally.
    in_bios: bool,
}

impl Default for GbaBus {
    fn default() -> Self {
        Self::new(None)
    }
}

impl GbaBus {
    pub fn new(bios: Option<Vec<u8>>) -> Self {
        Self {
            bios: bios.map(|b| b.into_boxed_slice()),
            ewram: vec![0; EWRAM_SIZE].into_boxed_slice(),
            iwram: vec![0; IWRAM_SIZE].into_boxed_slice(),
            palette: vec![0; PALETTE_SIZE].into_boxed_slice(),
            vram: vec![0; VRAM_SIZE].into_boxed_slice(),
            oam: vec![0; OAM_SIZE].into_boxed_slice(),
            open_bus: 0,
            bios_open_bus: 0,
            in_bios: false,
        }
    }

    pub fn has_bios(&self) -> bool {
        self.bios.is_some()
    }

    /// Tell the bus whether the CPU is fetching from BIOS, which gates BIOS reads.
    pub fn set_in_bios(&mut self, in_bios: bool) {
        self.in_bios = in_bios;
    }

    pub fn set_open_bus(&mut self, value: u32) {
        self.open_bus = value;
    }

    /// What an unmapped read returns, aligned to the requested width.
    #[inline]
    pub fn open_bus32(&self) -> u32 {
        self.open_bus
    }

    /// Record that the BIOS itself fetched `value`, for the next read of BIOS memory from
    /// outside it to return. See the `bios_open_bus` field's own docs for why this is a separate
    /// mechanism from [`Self::set_open_bus`].
    pub fn set_bios_open_bus(&mut self, value: u32) {
        self.bios_open_bus = value;
    }

    pub fn palette(&self) -> &[u8] {
        &self.palette
    }

    pub fn vram(&self) -> &[u8] {
        &self.vram
    }

    pub fn oam(&self) -> &[u8] {
        &self.oam
    }

    pub fn vram_mut(&mut self) -> &mut [u8] {
        &mut self.vram
    }

    pub fn oam_mut(&mut self) -> &mut [u8] {
        &mut self.oam
    }

    /// Read a byte from anything this module owns, or `None` if it owns nothing there.
    ///
    /// Cartridge and I/O addresses return `None`: they belong to the mapper and the system
    /// assembly, which is the same split prompt 06 uses for the Game Boy.
    pub fn read8(&self, addr: u32) -> Option<u8> {
        Some(match Region::of(addr) {
            Region::Bios => match (&self.bios, self.in_bios) {
                (Some(bios), true) => bios.get(addr as usize).copied().unwrap_or(0),
                // Outside BIOS code, or with no BIOS supplied: not the general open-bus rule,
                // but the BIOS's own sticky last-fetched value — see `bios_open_bus`.
                _ => self.bios_open_bus_byte(addr),
            },
            Region::EWram => self.ewram[(addr as usize) & (EWRAM_SIZE - 1)],
            Region::IWram => self.iwram[(addr as usize) & (IWRAM_SIZE - 1)],
            Region::Palette => self.palette[(addr as usize) & (PALETTE_SIZE - 1)],
            Region::Vram => self.vram[vram_offset(addr)],
            Region::Oam => self.oam[(addr as usize) & (OAM_SIZE - 1)],
            Region::Unmapped => self.open_bus_byte(addr),
            Region::Io | Region::Rom { .. } | Region::Sram => return None,
        })
    }

    /// Write a byte, or return `false` if this module owns nothing at that address.
    ///
    /// # The 8-bit write quirk
    ///
    /// Palette RAM, VRAM, and OAM are on a 16-bit bus and cannot be written a byte at a time.
    /// A byte write to palette RAM or VRAM writes the byte to *both* halves of the containing
    /// halfword; a byte write to OAM is dropped entirely. Games do this by accident, and
    /// implementing it as a plain byte store produces single-pixel colour corruption that is
    /// very hard to trace back to the store that caused it.
    pub fn write8(&mut self, addr: u32, value: u8) -> bool {
        match Region::of(addr) {
            Region::EWram => self.ewram[(addr as usize) & (EWRAM_SIZE - 1)] = value,
            Region::IWram => self.iwram[(addr as usize) & (IWRAM_SIZE - 1)] = value,
            Region::Palette => {
                let base = ((addr as usize) & (PALETTE_SIZE - 1)) & !1;
                self.palette[base] = value;
                self.palette[base + 1] = value;
            }
            Region::Vram => {
                // Only the background/bitmap half honours the doubling; a byte write to the
                // object half is ignored, like OAM.
                let offset = vram_offset(addr);
                if offset < 0x0001_0000 {
                    let base = offset & !1;
                    self.vram[base] = value;
                    self.vram[base + 1] = value;
                }
            }
            // Dropped, not stored.
            Region::Oam => {}
            Region::Bios | Region::Unmapped => {}
            Region::Io | Region::Rom { .. } | Region::Sram => return false,
        }
        true
    }

    pub fn read16(&self, addr: u32) -> Option<u16> {
        let addr = addr & !1;
        Some(u16::from_le_bytes([
            self.read8(addr)?,
            self.read8(addr + 1)?,
        ]))
    }

    pub fn read32(&self, addr: u32) -> Option<u32> {
        let addr = addr & !3;
        Some(u32::from_le_bytes([
            self.read8(addr)?,
            self.read8(addr + 1)?,
            self.read8(addr + 2)?,
            self.read8(addr + 3)?,
        ]))
    }

    pub fn write16(&mut self, addr: u32, value: u16) -> bool {
        let addr = addr & !1;
        let [low, high] = value.to_le_bytes();
        // Written through the halfword path rather than twice through `write8`, so the 8-bit
        // doubling quirk above does not fire for a write that was never a byte write.
        match Region::of(addr) {
            Region::Palette => {
                let base = ((addr as usize) & (PALETTE_SIZE - 1)) & !1;
                self.palette[base] = low;
                self.palette[base + 1] = high;
                true
            }
            Region::Vram => {
                let base = vram_offset(addr) & !1;
                self.vram[base] = low;
                self.vram[base + 1] = high;
                true
            }
            Region::Oam => {
                let base = ((addr as usize) & (OAM_SIZE - 1)) & !1;
                self.oam[base] = low;
                self.oam[base + 1] = high;
                true
            }
            _ => self.write8(addr, low) && self.write8(addr + 1, high),
        }
    }

    pub fn write32(&mut self, addr: u32, value: u32) -> bool {
        let addr = addr & !3;
        let [a, b, c, d] = value.to_le_bytes();
        self.write16(addr, u16::from_le_bytes([a, b]))
            && self.write16(addr + 2, u16::from_le_bytes([c, d]))
    }

    /// The byte of the last bus word that corresponds to this address.
    #[inline]
    fn open_bus_byte(&self, addr: u32) -> u8 {
        (self.open_bus >> ((addr & 3) * 8)) as u8
    }

    /// The byte of the BIOS's own last-fetched word that corresponds to this address.
    #[inline]
    fn bios_open_bus_byte(&self, addr: u32) -> u8 {
        (self.bios_open_bus >> ((addr & 3) * 8)) as u8
    }
}

impl Savable for GbaBus {
    fn save(&self, w: &mut StateWriter) {
        // The BIOS is not written: it is supplied by the user, identical across runs, and
        // 16 KiB that would otherwise sit in every rewind frame.
        w.write_bytes(&self.ewram);
        w.write_bytes(&self.iwram);
        w.write_bytes(&self.palette);
        w.write_bytes(&self.vram);
        w.write_bytes(&self.oam);
        w.write_u32(self.open_bus);
        w.write_u32(self.bios_open_bus);
        w.write_bool(self.in_bios);
    }

    fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        r.read_bytes(&mut self.ewram)?;
        r.read_bytes(&mut self.iwram)?;
        r.read_bytes(&mut self.palette)?;
        r.read_bytes(&mut self.vram)?;
        r.read_bytes(&mut self.oam)?;
        self.open_bus = r.read_u32()?;
        self.bios_open_bus = r.read_u32()?;
        self.in_bios = r.read_bool()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
