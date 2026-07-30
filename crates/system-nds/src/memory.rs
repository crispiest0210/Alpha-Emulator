//! The Nintendo DS memory map — the parts made of plain RAM.
//!
//! Follows prompt 06's pattern and the shape `system-gba::memory` proved, with one structural
//! difference that drives everything below: **there are two memory maps, not one.**
//!
//! # Two CPUs, two views, one set of storage
//!
//! The ARM9 and the ARM7 see overlapping but genuinely different address spaces. Main RAM is at
//! `0x0200_0000` for both and is the same four megabytes. Nothing else lines up: the ARM7 has
//! 64 KiB of work RAM the ARM9 cannot reach at all, the ARM9 has palette RAM and OAM the ARM7
//! cannot reach, the two BIOS images are different sizes at different addresses, and the block at
//! `0x0300_0000` is *the same physical memory assigned to one core or the other at runtime*.
//!
//! So this module owns the storage once and answers two different questions about it:
//! [`NdsMemory::read8_arm9`] and [`NdsMemory::read8_arm7`]. Modelling it as two `Bus` impls over
//! two copies of the map was rejected — the shared WRAM split and main RAM would then exist twice
//! and could disagree, which is precisely the bug class the split exists to create on hardware and
//! must not exist by accident in the emulator.
//!
//! # The shared WRAM split is a register, not a constant
//!
//! 32 KiB of WRAM is divided between the two cores by `WRAMCNT`, which the ARM9 writes and the
//! ARM7 can only read. All four settings are real and games use at least three of them, including
//! the two that give one core *nothing*. See [`WramSplit`].
//!
//! # What lives elsewhere
//!
//! VRAM is not here. Its nine banks are individually assigned to one of several purposes by nine
//! separate registers, which is a mapping problem rather than a storage problem; it lives in
//! [`crate::vram`]. I/O and the cartridge are not here either, for the same reason they are not in
//! the GBA's bus: they belong to the system assembly and the mapper. Both are reported by
//! returning `None`.

use core_common::{Savable, StateError, StateReader, StateWriter};

pub const MAIN_RAM_SIZE: usize = 4 * 1024 * 1024;
pub const SHARED_WRAM_SIZE: usize = 32 * 1024;
pub const ARM7_WRAM_SIZE: usize = 64 * 1024;
pub const ARM9_BIOS_SIZE: usize = 32 * 1024;
pub const ARM7_BIOS_SIZE: usize = 16 * 1024;
/// 1 KiB for engine A, then 1 KiB for engine B, contiguous.
pub const PALETTE_SIZE: usize = 2 * 1024;
/// Likewise: engine A's 128 sprites, then engine B's.
pub const OAM_SIZE: usize = 2 * 1024;

/// Where the ARM9 BIOS is mapped. High vectors, which is why `cpu-arm946e` implements them.
pub const ARM9_BIOS_BASE: u32 = 0xFFFF_0000;

/// How the 32 KiB of shared WRAM is divided, as selected by `WRAMCNT`.
///
/// The names are written from the ARM9's point of view because the ARM9 is the core that writes
/// the register; the ARM7 only reads it back through `WRAMSTAT`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WramSplit {
    /// All 32 KiB to the ARM9. The ARM7's `0x0300_0000` window then mirrors its own 64 KiB.
    #[default]
    Arm9All,
    /// ARM9 gets the second 16 KiB, ARM7 the first.
    Arm9Second,
    /// ARM9 gets the first 16 KiB, ARM7 the second.
    Arm9First,
    /// All 32 KiB to the ARM7, and the ARM9's window answers nothing.
    Arm7All,
}

impl WramSplit {
    #[inline]
    pub fn from_bits(bits: u8) -> Self {
        match bits & 3 {
            0 => WramSplit::Arm9All,
            1 => WramSplit::Arm9Second,
            2 => WramSplit::Arm9First,
            _ => WramSplit::Arm7All,
        }
    }

    #[inline]
    pub fn bits(self) -> u8 {
        match self {
            WramSplit::Arm9All => 0,
            WramSplit::Arm9Second => 1,
            WramSplit::Arm9First => 2,
            WramSplit::Arm7All => 3,
        }
    }

    /// Byte offset into shared WRAM for an ARM9 access, or `None` when the ARM9 owns none of it.
    #[inline]
    pub fn arm9_offset(self, addr: u32) -> Option<usize> {
        Some(match self {
            WramSplit::Arm9All => (addr as usize) & (SHARED_WRAM_SIZE - 1),
            WramSplit::Arm9Second => 0x4000 + ((addr as usize) & 0x3FFF),
            WramSplit::Arm9First => (addr as usize) & 0x3FFF,
            WramSplit::Arm7All => return None,
        })
    }

    /// Byte offset into shared WRAM for an ARM7 access, or `None` when the ARM7 owns none of it —
    /// in which case the window mirrors the ARM7's own WRAM instead of reading as nothing.
    #[inline]
    pub fn arm7_offset(self, addr: u32) -> Option<usize> {
        Some(match self {
            WramSplit::Arm9All => return None,
            WramSplit::Arm9Second => (addr as usize) & 0x3FFF,
            WramSplit::Arm9First => 0x4000 + ((addr as usize) & 0x3FFF),
            WramSplit::Arm7All => (addr as usize) & (SHARED_WRAM_SIZE - 1),
        })
    }
}

/// The region an ARM9 address falls in.
///
/// Like the GBA's, selected by one byte of the address — but unlike the GBA's, the top of the
/// space is not a mirror of anything, because the ARM9's exception vectors and BIOS live at
/// `0xFFFF_0000`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arm9Region {
    /// Main RAM: 4 MiB shared with the ARM7, mirrored through the whole 16 MiB window.
    MainRam,
    /// The shared WRAM window, whose contents depend on [`WramSplit`].
    SharedWram,
    Io,
    /// Both engines' palettes: A at `0x0500_0000`, B at `0x0500_0400`.
    Palette,
    /// Bank-mapped; see [`crate::vram`].
    Vram,
    /// Both engines' OAM: A at `0x0700_0000`, B at `0x0700_0400`.
    Oam,
    /// The Slot-2 (Game Boy Advance cartridge) ROM window.
    GbaRom,
    /// The Slot-2 save-chip window.
    GbaRam,
    /// The ARM9 BIOS at `0xFFFF_0000`.
    Bios,
    Unmapped,
}

impl Arm9Region {
    #[inline]
    pub fn of(addr: u32) -> Self {
        // Checked before the nibble dispatch: the BIOS is the one region not selected by bits
        // 24-27, and `0xFFFF_0000 >> 24` is `0x0F`, which would otherwise land in `Unmapped`.
        if addr >= ARM9_BIOS_BASE {
            return Arm9Region::Bios;
        }
        match addr >> 24 {
            0x02 => Arm9Region::MainRam,
            0x03 => Arm9Region::SharedWram,
            0x04 => Arm9Region::Io,
            0x05 => Arm9Region::Palette,
            0x06 => Arm9Region::Vram,
            0x07 => Arm9Region::Oam,
            0x08 | 0x09 => Arm9Region::GbaRom,
            0x0A => Arm9Region::GbaRam,
            // `0x00` and `0x01` are ITCM's default window. `cpu-arm946e` splices the TCMs in
            // front of the bus, so an access that reaches here is one TCM declined to answer.
            _ => Arm9Region::Unmapped,
        }
    }
}

/// The region an ARM7 address falls in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arm7Region {
    /// The ARM7 BIOS: 16 KiB at address zero, where an ARMv4T core's vectors are.
    Bios,
    MainRam,
    /// `0x0300_0000`-`0x037F_FFFF`: shared WRAM, or a mirror of [`Arm7Region::Arm7Wram`] when the
    /// ARM7 has been assigned none of it.
    SharedWram,
    /// `0x0380_0000`-`0x03FF_FFFF`: 64 KiB private to the ARM7 and invisible to the ARM9.
    Arm7Wram,
    Io,
    /// The wifi hardware's register and RAM block. Not implemented; see the crate docs.
    Wifi,
    /// VRAM banks C and D when they have been assigned to the ARM7, and nothing otherwise.
    Vram,
    GbaRom,
    GbaRam,
    Unmapped,
}

impl Arm7Region {
    #[inline]
    pub fn of(addr: u32) -> Self {
        match addr >> 24 {
            0x00 => Arm7Region::Bios,
            0x02 => Arm7Region::MainRam,
            // The one place the DS's map is not one-region-per-byte: `0x03` splits in half.
            0x03 => {
                if addr & 0x0080_0000 == 0 {
                    Arm7Region::SharedWram
                } else {
                    Arm7Region::Arm7Wram
                }
            }
            0x04 => {
                if (0x0480_0000..0x0490_0000).contains(&addr) {
                    Arm7Region::Wifi
                } else {
                    Arm7Region::Io
                }
            }
            0x06 => Arm7Region::Vram,
            0x08 | 0x09 => Arm7Region::GbaRom,
            0x0A => Arm7Region::GbaRam,
            _ => Arm7Region::Unmapped,
        }
    }
}

/// Everything on the board that is plain addressable RAM or ROM, for both cores.
pub struct NdsMemory {
    main_ram: Box<[u8]>,
    shared_wram: Box<[u8]>,
    arm7_wram: Box<[u8]>,
    /// User-supplied, like every other BIOS in this project. `None` is supported: the system
    /// boots a cartridge directly instead, and reads here return open bus.
    arm9_bios: Option<Box<[u8]>>,
    arm7_bios: Option<Box<[u8]>>,
    palette: Box<[u8]>,
    oam: Box<[u8]>,
    split: WramSplit,

    /// The last value driven on each core's bus, which its unmapped reads return.
    ///
    /// Two values, not one, because the two cores fetch independently — an ARM7 read of nothing
    /// returns whatever the *ARM7* last saw, and sharing one field would make the result depend on
    /// the interleaving quantum, which is exactly the determinism trap prompt 13 warns about.
    open_bus9: u32,
    open_bus7: u32,
}

impl Default for NdsMemory {
    fn default() -> Self {
        Self::new(None, None)
    }
}

impl NdsMemory {
    pub fn new(arm9_bios: Option<Vec<u8>>, arm7_bios: Option<Vec<u8>>) -> Self {
        Self {
            main_ram: vec![0; MAIN_RAM_SIZE].into_boxed_slice(),
            shared_wram: vec![0; SHARED_WRAM_SIZE].into_boxed_slice(),
            arm7_wram: vec![0; ARM7_WRAM_SIZE].into_boxed_slice(),
            arm9_bios: arm9_bios.map(Vec::into_boxed_slice),
            arm7_bios: arm7_bios.map(Vec::into_boxed_slice),
            palette: vec![0; PALETTE_SIZE].into_boxed_slice(),
            oam: vec![0; OAM_SIZE].into_boxed_slice(),
            split: WramSplit::default(),
            open_bus9: 0,
            open_bus7: 0,
        }
    }

    pub fn has_arm9_bios(&self) -> bool {
        self.arm9_bios.is_some()
    }

    pub fn has_arm7_bios(&self) -> bool {
        self.arm7_bios.is_some()
    }

    pub fn split(&self) -> WramSplit {
        self.split
    }

    pub fn set_split(&mut self, split: WramSplit) {
        self.split = split;
    }

    pub fn set_open_bus9(&mut self, value: u32) {
        self.open_bus9 = value;
    }

    pub fn set_open_bus7(&mut self, value: u32) {
        self.open_bus7 = value;
    }

    pub fn main_ram(&self) -> &[u8] {
        &self.main_ram
    }

    pub fn main_ram_mut(&mut self) -> &mut [u8] {
        &mut self.main_ram
    }

    pub fn palette(&self) -> &[u8] {
        &self.palette
    }

    pub fn oam(&self) -> &[u8] {
        &self.oam
    }

    pub fn oam_mut(&mut self) -> &mut [u8] {
        &mut self.oam
    }

    /// Return every byte of RAM to zero, keeping the user-supplied BIOS images.
    ///
    /// The BIOSes are kept because they are not machine state: they are files the user supplied,
    /// and re-reading them from disk on every reset would be the only alternative.
    pub fn reset(&mut self) {
        self.main_ram.fill(0);
        self.shared_wram.fill(0);
        self.arm7_wram.fill(0);
        self.palette.fill(0);
        self.oam.fill(0);
        self.split = WramSplit::default();
        self.open_bus9 = 0;
        self.open_bus7 = 0;
    }

    /// Read a byte from the ARM9's view, or `None` where this module owns nothing.
    ///
    /// `None` means I/O, VRAM, or the Slot-2 cartridge — all of which belong to other modules.
    pub fn read8_arm9(&self, addr: u32) -> Option<u8> {
        Some(match Arm9Region::of(addr) {
            Arm9Region::MainRam => self.main_ram[(addr as usize) & (MAIN_RAM_SIZE - 1)],
            Arm9Region::SharedWram => match self.split.arm9_offset(addr) {
                Some(offset) => self.shared_wram[offset],
                // Nothing is mapped here, and hardware does not fall back to anything.
                None => open_bus_byte(self.open_bus9, addr),
            },
            Arm9Region::Palette => self.palette[(addr as usize) & (PALETTE_SIZE - 1)],
            Arm9Region::Oam => self.oam[(addr as usize) & (OAM_SIZE - 1)],
            Arm9Region::Bios => match &self.arm9_bios {
                Some(bios) => bios
                    .get((addr - ARM9_BIOS_BASE) as usize)
                    .copied()
                    .unwrap_or_else(|| open_bus_byte(self.open_bus9, addr)),
                None => open_bus_byte(self.open_bus9, addr),
            },
            Arm9Region::Unmapped => open_bus_byte(self.open_bus9, addr),
            Arm9Region::Io | Arm9Region::Vram | Arm9Region::GbaRom | Arm9Region::GbaRam => {
                return None
            }
        })
    }

    /// A halfword or word from the ARM9's view, read from the owning region in one go.
    ///
    /// Not composed from [`read8_arm9`](Self::read8_arm9). An instruction fetch is a word read,
    /// and composing one costs four region decodes and four bounds checks for a value that lives
    /// in one array — which measured as the dominant cost of a DS frame, well ahead of the two 2D
    /// engines. Callers pass aligned addresses, and every region here is a power of two at least
    /// four bytes long, so the masked index cannot straddle the end of one.
    #[inline]
    pub fn read_wide_arm9(&self, addr: u32, bytes: u32) -> Option<u32> {
        let (slice, index) = match Arm9Region::of(addr) {
            Arm9Region::MainRam => (&self.main_ram, (addr as usize) & (MAIN_RAM_SIZE - 1)),
            Arm9Region::SharedWram => match self.split.arm9_offset(addr) {
                Some(offset) => (&self.shared_wram, offset),
                None => return Some(wide_open_bus(self.open_bus9, bytes)),
            },
            Arm9Region::Palette => (&self.palette, (addr as usize) & (PALETTE_SIZE - 1)),
            Arm9Region::Oam => (&self.oam, (addr as usize) & (OAM_SIZE - 1)),
            Arm9Region::Bios => match &self.arm9_bios {
                Some(bios) if (addr - ARM9_BIOS_BASE) as usize + bytes as usize <= bios.len() => {
                    (bios, (addr - ARM9_BIOS_BASE) as usize)
                }
                _ => return Some(wide_open_bus(self.open_bus9, bytes)),
            },
            Arm9Region::Unmapped => return Some(wide_open_bus(self.open_bus9, bytes)),
            Arm9Region::Io | Arm9Region::Vram | Arm9Region::GbaRom | Arm9Region::GbaRam => {
                return None
            }
        };
        Some(gather(slice, index, bytes))
    }

    /// The same, from the ARM7's view.
    #[inline]
    pub fn read_wide_arm7(&self, addr: u32, bytes: u32) -> Option<u32> {
        let (slice, index) = match Arm7Region::of(addr) {
            Arm7Region::Bios => match &self.arm7_bios {
                Some(bios) if addr as usize + bytes as usize <= bios.len() => (bios, addr as usize),
                _ => return Some(wide_open_bus(self.open_bus7, bytes)),
            },
            Arm7Region::MainRam => (&self.main_ram, (addr as usize) & (MAIN_RAM_SIZE - 1)),
            Arm7Region::SharedWram => match self.split.arm7_offset(addr) {
                Some(offset) => (&self.shared_wram, offset),
                None => (&self.arm7_wram, (addr as usize) & (ARM7_WRAM_SIZE - 1)),
            },
            Arm7Region::Arm7Wram => (&self.arm7_wram, (addr as usize) & (ARM7_WRAM_SIZE - 1)),
            Arm7Region::Unmapped | Arm7Region::Wifi => {
                return Some(wide_open_bus(self.open_bus7, bytes))
            }
            Arm7Region::Io | Arm7Region::Vram | Arm7Region::GbaRom | Arm7Region::GbaRam => {
                return None
            }
        };
        Some(gather(slice, index, bytes))
    }

    /// Write a halfword or word through the ARM9's view, in one go.
    ///
    /// Palette RAM and OAM are included here, and *not* in the byte path: the DS drops byte
    /// writes to both, so a wide write composed from byte writes would be dropped as well.
    #[inline]
    pub fn write_wide_arm9(&mut self, addr: u32, value: u32, bytes: u32) -> bool {
        let (slice, index) = match Arm9Region::of(addr) {
            Arm9Region::MainRam => (&mut self.main_ram, (addr as usize) & (MAIN_RAM_SIZE - 1)),
            Arm9Region::SharedWram => match self.split.arm9_offset(addr) {
                Some(offset) => (&mut self.shared_wram, offset),
                None => return true,
            },
            Arm9Region::Palette => (&mut self.palette, (addr as usize) & (PALETTE_SIZE - 1)),
            Arm9Region::Oam => (&mut self.oam, (addr as usize) & (OAM_SIZE - 1)),
            Arm9Region::Bios | Arm9Region::Unmapped => return true,
            Arm9Region::Io | Arm9Region::Vram | Arm9Region::GbaRom | Arm9Region::GbaRam => {
                return false
            }
        };
        scatter(slice, index, value, bytes);
        true
    }

    /// The same, from the ARM7's view.
    #[inline]
    pub fn write_wide_arm7(&mut self, addr: u32, value: u32, bytes: u32) -> bool {
        let (slice, index) = match Arm7Region::of(addr) {
            Arm7Region::MainRam => (&mut self.main_ram, (addr as usize) & (MAIN_RAM_SIZE - 1)),
            Arm7Region::SharedWram => match self.split.arm7_offset(addr) {
                Some(offset) => (&mut self.shared_wram, offset),
                None => (&mut self.arm7_wram, (addr as usize) & (ARM7_WRAM_SIZE - 1)),
            },
            Arm7Region::Arm7Wram => (&mut self.arm7_wram, (addr as usize) & (ARM7_WRAM_SIZE - 1)),
            Arm7Region::Bios | Arm7Region::Unmapped | Arm7Region::Wifi => return true,
            Arm7Region::Io | Arm7Region::Vram | Arm7Region::GbaRom | Arm7Region::GbaRam => {
                return false
            }
        };
        scatter(slice, index, value, bytes);
        true
    }

    /// Read a byte from the ARM7's view, or `None` where this module owns nothing.
    pub fn read8_arm7(&self, addr: u32) -> Option<u8> {
        Some(match Arm7Region::of(addr) {
            Arm7Region::Bios => match &self.arm7_bios {
                Some(bios) => bios
                    .get(addr as usize)
                    .copied()
                    .unwrap_or_else(|| open_bus_byte(self.open_bus7, addr)),
                None => open_bus_byte(self.open_bus7, addr),
            },
            Arm7Region::MainRam => self.main_ram[(addr as usize) & (MAIN_RAM_SIZE - 1)],
            Arm7Region::SharedWram => match self.split.arm7_offset(addr) {
                Some(offset) => self.shared_wram[offset],
                // With no share assigned, the window is a second view of the ARM7's own WRAM.
                None => self.arm7_wram[(addr as usize) & (ARM7_WRAM_SIZE - 1)],
            },
            Arm7Region::Arm7Wram => self.arm7_wram[(addr as usize) & (ARM7_WRAM_SIZE - 1)],
            Arm7Region::Unmapped | Arm7Region::Wifi => open_bus_byte(self.open_bus7, addr),
            Arm7Region::Io | Arm7Region::Vram | Arm7Region::GbaRom | Arm7Region::GbaRam => {
                return None
            }
        })
    }

    /// Write a byte through the ARM9's view; `false` when this module owns nothing there.
    ///
    /// # The 8-bit write quirk, and where the DS differs from the GBA
    ///
    /// Palette RAM and OAM are on a 16-bit bus. On the GBA a byte write to palette RAM is
    /// doubled into both halves of the containing halfword and a byte write to OAM is dropped.
    /// On the DS **both are dropped** — the ARM9 simply cannot write a byte to palette RAM, OAM,
    /// or VRAM. Carrying the GBA's doubling over would silently corrupt one colour of every
    /// palette a game touches with a byte store.
    pub fn write8_arm9(&mut self, addr: u32, value: u8) -> bool {
        match Arm9Region::of(addr) {
            Arm9Region::MainRam => self.main_ram[(addr as usize) & (MAIN_RAM_SIZE - 1)] = value,
            Arm9Region::SharedWram => {
                if let Some(offset) = self.split.arm9_offset(addr) {
                    self.shared_wram[offset] = value;
                }
            }
            // Dropped, not doubled. See the note above.
            Arm9Region::Palette | Arm9Region::Oam => {}
            Arm9Region::Bios | Arm9Region::Unmapped => {}
            Arm9Region::Io | Arm9Region::Vram | Arm9Region::GbaRom | Arm9Region::GbaRam => {
                return false
            }
        }
        true
    }

    /// Write a byte through the ARM7's view; `false` when this module owns nothing there.
    pub fn write8_arm7(&mut self, addr: u32, value: u8) -> bool {
        match Arm7Region::of(addr) {
            Arm7Region::MainRam => self.main_ram[(addr as usize) & (MAIN_RAM_SIZE - 1)] = value,
            Arm7Region::SharedWram => match self.split.arm7_offset(addr) {
                Some(offset) => self.shared_wram[offset] = value,
                None => self.arm7_wram[(addr as usize) & (ARM7_WRAM_SIZE - 1)] = value,
            },
            Arm7Region::Arm7Wram => self.arm7_wram[(addr as usize) & (ARM7_WRAM_SIZE - 1)] = value,
            Arm7Region::Bios | Arm7Region::Unmapped | Arm7Region::Wifi => {}
            Arm7Region::Io | Arm7Region::Vram | Arm7Region::GbaRom | Arm7Region::GbaRam => {
                return false
            }
        }
        true
    }

    /// Halfword write through the ARM9's view, which is the narrowest write palette RAM and OAM
    /// accept. Returns `false` when this module owns nothing there.
    pub fn write16_arm9(&mut self, addr: u32, value: u16) -> bool {
        let addr = addr & !1;
        let [low, high] = value.to_le_bytes();
        match Arm9Region::of(addr) {
            Arm9Region::Palette => {
                let base = (addr as usize) & (PALETTE_SIZE - 1);
                self.palette[base] = low;
                self.palette[base + 1] = high;
                true
            }
            Arm9Region::Oam => {
                let base = (addr as usize) & (OAM_SIZE - 1);
                self.oam[base] = low;
                self.oam[base + 1] = high;
                true
            }
            _ => self.write8_arm9(addr, low) && self.write8_arm9(addr + 1, high),
        }
    }
}

/// Little-endian gather of two or four bytes.
#[inline]
fn gather(slice: &[u8], index: usize, bytes: u32) -> u32 {
    if bytes == 2 {
        u16::from_le_bytes([slice[index], slice[index + 1]]) as u32
    } else {
        u32::from_le_bytes([
            slice[index],
            slice[index + 1],
            slice[index + 2],
            slice[index + 3],
        ])
    }
}

#[inline]
fn scatter(slice: &mut [u8], index: usize, value: u32, bytes: u32) {
    for i in 0..bytes as usize {
        slice[index + i] = (value >> (i * 8)) as u8;
    }
}

/// Open bus, widened. A word read of nothing returns the whole last word; a halfword read returns
/// the half that lines up with the address, which the caller has already aligned.
#[inline]
fn wide_open_bus(word: u32, bytes: u32) -> u32 {
    if bytes == 2 {
        word & 0xFFFF
    } else {
        word
    }
}

/// The byte of a bus word that corresponds to this address.
///
/// Open bus is word-granular on hardware: a byte read from nowhere returns the matching byte of
/// the last word, not the last byte.
#[inline]
fn open_bus_byte(word: u32, addr: u32) -> u8 {
    (word >> ((addr & 3) * 8)) as u8
}

impl Savable for NdsMemory {
    fn save(&self, w: &mut StateWriter) {
        // Neither BIOS is written: both are user-supplied, identical across runs, and 48 KiB
        // that would otherwise sit in every rewind frame.
        w.write_bytes(&self.main_ram);
        w.write_bytes(&self.shared_wram);
        w.write_bytes(&self.arm7_wram);
        w.write_bytes(&self.palette);
        w.write_bytes(&self.oam);
        w.write_u8(self.split.bits());
        w.write_u32(self.open_bus9);
        w.write_u32(self.open_bus7);
    }

    fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        r.read_bytes(&mut self.main_ram)?;
        r.read_bytes(&mut self.shared_wram)?;
        r.read_bytes(&mut self.arm7_wram)?;
        r.read_bytes(&mut self.palette)?;
        r.read_bytes(&mut self.oam)?;
        self.split = WramSplit::from_bits(r.read_u8()?);
        self.open_bus9 = r.read_u32()?;
        self.open_bus7 = r.read_u32()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
