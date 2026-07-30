//! VRAM: nine banks, each individually assignable to one of several purposes.
//!
//! This has no analogue in anything else in this project. The Game Boy, Game Boy Color, and Game
//! Boy Advance all have VRAM at a fixed address with a fixed meaning. The DS has 656 KiB in nine
//! banks — A through I, of four different sizes — and each bank carries a control register saying
//! *what it currently is*: background data for one of the two 2D engines, sprite data for one of
//! them, 3D texture image data, a texture palette, an extended palette, memory for the ARM7, or
//! nothing but a plain window the CPU can write through.
//!
//! Three consequences make this harder than a memory map:
//!
//! - **A bank's address depends on a register.** Bank A at `MST=1, OFS=2` answers
//!   `0x0604_0000`; the same bank at `OFS=0` answers `0x0600_0000`.
//! - **Two banks can claim the same address.** Hardware drives both, so a write lands in both
//!   and a read sees them ORed together. This is not a configuration error software avoids — it
//!   is a transient state during a bank reassignment, and treating it as "first bank wins" loses
//!   writes.
//! - **Most of the mapped space is not CPU-visible at all.** Texture memory, texture palettes,
//!   and extended palettes are read by the 3D core and the 2D engines but appear nowhere in
//!   either CPU's address space. They still need exactly the same bank resolution.
//!
//! # Precomputed, not resolved per access
//!
//! `AGENTS.md` flags the choice between resolving the bank mapping on every access and
//! precomputing it. This module **precomputes**: a flat page table, rebuilt whenever a `VRAMCNT`
//! register changes, mapping every 8 KiB page of every target space to the banks that serve it.
//!
//! The reasoning is that the two are not equally correct. Resolving per access means asking all
//! nine banks "is this yours?" — which is nine register decodes on the texture fetch inside the
//! rasteriser's innermost loop, the one place prompt 18 expects this project to actually need
//! optimising. Precomputing costs a 328-entry table rebuild per `VRAMCNT` write, and games write
//! those registers a handful of times per frame at most. The correctness argument is the same
//! either way, because the table stores *every* bank claiming a page rather than a winner, so
//! overlap behaves as it does on hardware in both designs.
//!
//! 8 KiB is the page size because that is the extended-palette slot size — the finest granularity
//! any mapping rule uses. Bank F, at 16 KiB, is two pages.
//!
//! # What is deliberately not modelled
//!
//! A bank with its enable bit clear is mapped nowhere at all, including out of the LCDC window.
//! That is what hardware does, and the alternative — leaving it visible through LCDC — would hide
//! a whole class of "the game forgot to enable the bank" bug behind a picture that looks right.

use core_common::{Savable, StateError, StateReader, StateWriter};

/// The nine banks in register order, with their physical sizes.
pub const BANK_SIZES: [usize; 9] = [
    0x2_0000, // A: 128 KiB
    0x2_0000, // B: 128 KiB
    0x2_0000, // C: 128 KiB
    0x2_0000, // D: 128 KiB
    0x1_0000, // E:  64 KiB
    0x0_4000, // F:  16 KiB
    0x0_4000, // G:  16 KiB
    0x0_8000, // H:  32 KiB
    0x0_4000, // I:  16 KiB
];

pub const BANK_NAMES: [&str; 9] = ["A", "B", "C", "D", "E", "F", "G", "H", "I"];

/// Total VRAM, 656 KiB.
pub const TOTAL_VRAM: usize = 0xA_4000;

/// The mapping granularity: the extended-palette slot size, and so the finest granularity any
/// `VRAMCNT` rule uses.
pub const PAGE_SIZE: u32 = 0x2000;

/// A purpose a bank can be assigned to, which is also an independent flat address space.
///
/// Four of these are windows in the ARM9's address space, one is a window in the ARM7's, and the
/// rest are read only by the graphics hardware. They are modelled identically because the bank
/// resolution problem is identical; only who reads them differs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VramSpace {
    /// Engine A background data. `0x0600_0000` on the ARM9.
    BgA,
    /// Engine B background data. `0x0620_0000` on the ARM9.
    BgB,
    /// Engine A sprite data. `0x0640_0000` on the ARM9.
    ObjA,
    /// Engine B sprite data. `0x0660_0000` on the ARM9.
    ObjB,
    /// 3D texture image data: four 128 KiB slots.
    Texture,
    /// 3D texture palettes: six 16 KiB slots.
    TexturePalette,
    /// Engine A extended background palettes: four 8 KiB slots, one per background layer.
    BgExtPalA,
    BgExtPalB,
    /// Engine A extended sprite palette: one 8 KiB slot.
    ObjExtPalA,
    ObjExtPalB,
    /// Banks handed to the ARM7, which sees them at `0x0600_0000`.
    Arm7,
    /// The direct window at `0x0680_0000`, where each enabled bank appears at a fixed offset
    /// whatever else it is doing. This is how software uploads to a bank it has assigned to the
    /// 3D core, which is otherwise unreachable.
    Lcdc,
}

use VramSpace::*;

/// Every space, in the order their page ranges are laid out.
pub const SPACES: [VramSpace; 12] = [
    BgA,
    BgB,
    ObjA,
    ObjB,
    Texture,
    TexturePalette,
    BgExtPalA,
    BgExtPalB,
    ObjExtPalA,
    ObjExtPalB,
    Arm7,
    Lcdc,
];

impl VramSpace {
    /// Size of this space in bytes.
    pub const fn size(self) -> u32 {
        match self {
            BgA => 0x8_0000,
            BgB => 0x2_0000,
            ObjA => 0x4_0000,
            ObjB => 0x2_0000,
            Texture => 0x8_0000,
            TexturePalette => 0x1_8000,
            BgExtPalA | BgExtPalB => 0x8000,
            ObjExtPalA | ObjExtPalB => 0x2000,
            Arm7 => 0x4_0000,
            Lcdc => TOTAL_VRAM as u32,
        }
    }

    pub const fn pages(self) -> usize {
        (self.size() / PAGE_SIZE) as usize
    }

    /// Index of this space's first page in the flat table.
    ///
    /// A table lookup rather than a scan: this is on the read path, and `SPACES` is ordered so
    /// that a space's discriminant is its index.
    #[inline]
    const fn first_page(self) -> usize {
        SPACE_FIRST_PAGE[self as usize]
    }
}

/// Where each space's pages start, laid out in `SPACES` order.
const SPACE_FIRST_PAGE: [usize; SPACES.len()] = {
    let mut bases = [0usize; SPACES.len()];
    let mut i = 0;
    let mut base = 0;
    while i < SPACES.len() {
        bases[i] = base;
        base += SPACES[i].pages();
        i += 1;
    }
    bases
};

/// Total pages across every space.
const TOTAL_PAGES: usize = {
    let mut total = 0;
    let mut i = 0;
    while i < SPACES.len() {
        total += SPACES[i].pages();
        i += 1;
    }
    total
};

/// How many banks may claim one page before the extras are dropped.
///
/// Four is above anything a mapping a game actually writes produces; the rules only allow banks
/// of compatible sizes to collide, and the worst real case is two. The limit exists so the table
/// is a fixed-size array rather than 328 heap allocations, and an overflow is logged rather than
/// silently dropped.
const MAX_BANKS_PER_PAGE: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Slot {
    bank: u8,
    /// Byte offset within the bank at which this page starts.
    offset: u32,
}

impl Default for Slot {
    fn default() -> Self {
        Self {
            bank: u8::MAX,
            offset: 0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Page {
    slots: [Slot; MAX_BANKS_PER_PAGE],
    count: u8,
}

impl Default for Page {
    fn default() -> Self {
        Self {
            slots: [Slot {
                bank: u8::MAX,
                offset: 0,
            }; MAX_BANKS_PER_PAGE],
            count: 0,
        }
    }
}

impl Page {
    fn push(&mut self, bank: u8, offset: u32) -> bool {
        if self.count as usize == MAX_BANKS_PER_PAGE {
            return false;
        }
        self.slots[self.count as usize] = Slot { bank, offset };
        self.count += 1;
        true
    }

    fn active(&self) -> &[Slot] {
        &self.slots[..self.count as usize]
    }
}

/// The nine banks and the mapping currently in force.
pub struct Vram {
    banks: [Box<[u8]>; 9],
    /// `VRAMCNT_A` through `VRAMCNT_I`, exactly as written.
    cnt: [u8; 9],
    /// Rebuilt from `cnt` whenever it changes; never serialized, always derived.
    pages: Box<[Page; TOTAL_PAGES]>,
}

impl Default for Vram {
    fn default() -> Self {
        Self::new()
    }
}

impl Vram {
    pub fn new() -> Self {
        let banks = std::array::from_fn(|i| vec![0u8; BANK_SIZES[i]].into_boxed_slice());
        let mut vram = Self {
            banks,
            cnt: [0; 9],
            pages: Box::new([Page::default(); TOTAL_PAGES]),
        };
        vram.rebuild();
        vram
    }

    /// The raw `VRAMCNT` byte for a bank, as read back.
    pub fn control(&self, bank: usize) -> u8 {
        self.cnt[bank]
    }

    /// Write a `VRAMCNT` register and rebuild the mapping.
    pub fn set_control(&mut self, bank: usize, value: u8) {
        if self.cnt[bank] == value {
            return;
        }
        self.cnt[bank] = value;
        self.rebuild();
    }

    /// The whole of one bank, for the debugger and for tests that want to seed contents without
    /// caring where the bank is currently mapped.
    pub fn bank(&self, bank: usize) -> &[u8] {
        &self.banks[bank]
    }

    pub fn bank_mut(&mut self, bank: usize) -> &mut [u8] {
        &mut self.banks[bank]
    }

    /// Read a byte from a space, ORing every bank mapped there.
    ///
    /// Returns 0 for an offset no bank claims. Unmapped VRAM reads as zero on hardware rather
    /// than as open bus — VRAM is on the graphics side of the bus, not the CPU's.
    #[inline]
    pub fn read8(&self, space: VramSpace, offset: u32) -> u8 {
        if offset >= space.size() {
            return 0;
        }
        let page = &self.pages[space.first_page() + (offset / PAGE_SIZE) as usize];
        let within = offset % PAGE_SIZE;
        let mut value = 0;
        for slot in page.active() {
            value |= self.banks[slot.bank as usize][(slot.offset + within) as usize];
        }
        value
    }

    #[inline]
    pub fn read16(&self, space: VramSpace, offset: u32) -> u16 {
        u16::from_le_bytes([self.read8(space, offset), self.read8(space, offset + 1)])
    }

    #[inline]
    pub fn read32(&self, space: VramSpace, offset: u32) -> u32 {
        u32::from_le_bytes([
            self.read8(space, offset),
            self.read8(space, offset + 1),
            self.read8(space, offset + 2),
            self.read8(space, offset + 3),
        ])
    }

    /// Write a byte into every bank mapped at this offset.
    ///
    /// Returns whether any bank took it, which is what lets the caller distinguish "written" from
    /// "fell into a hole in the mapping".
    #[inline]
    pub fn write8(&mut self, space: VramSpace, offset: u32, value: u8) -> bool {
        if offset >= space.size() {
            return false;
        }
        let page = self.pages[space.first_page() + (offset / PAGE_SIZE) as usize];
        let within = offset % PAGE_SIZE;
        for slot in page.active() {
            self.banks[slot.bank as usize][(slot.offset + within) as usize] = value;
        }
        page.count > 0
    }

    #[inline]
    pub fn write16(&mut self, space: VramSpace, offset: u32, value: u16) -> bool {
        let [low, high] = value.to_le_bytes();
        self.write8(space, offset, low) | self.write8(space, offset + 1, high)
    }

    /// Which banks currently serve a byte of a space, for tests and the debugger.
    pub fn banks_at(&self, space: VramSpace, offset: u32) -> Vec<usize> {
        if offset >= space.size() {
            return Vec::new();
        }
        let page = &self.pages[space.first_page() + (offset / PAGE_SIZE) as usize];
        page.active().iter().map(|s| s.bank as usize).collect()
    }

    /// Whether any bank at all is mapped into a space, which is how an engine decides whether an
    /// extended palette is available rather than reading zeroes and drawing black.
    pub fn space_is_mapped(&self, space: VramSpace) -> bool {
        let base = space.first_page();
        self.pages[base..base + space.pages()]
            .iter()
            .any(|p| p.count > 0)
    }

    fn rebuild(&mut self) {
        self.pages.fill(Page::default());
        for bank in 0..9u8 {
            let cnt = self.cnt[bank as usize];
            // Bit 7 is the enable. A disabled bank is mapped nowhere, LCDC included.
            if cnt & 0x80 == 0 {
                continue;
            }
            let mst = cnt & 0x07;
            let ofs = (cnt >> 3) & 0x03;
            let Some((space, base)) = bank_mapping(bank, mst, ofs) else {
                tracing::debug!(
                    bank = BANK_NAMES[bank as usize],
                    mst,
                    ofs,
                    "VRAMCNT selects a mapping this bank does not have; treated as unmapped"
                );
                continue;
            };

            let size = BANK_SIZES[bank as usize] as u32;
            // A bank can be larger than the space it is assigned to — bank E is 64 KiB and an
            // extended-palette space is 32 KiB — in which case only the part that fits is used.
            let usable = size.min(space.size().saturating_sub(base));
            let first = space.first_page() + (base / PAGE_SIZE) as usize;
            for page in 0..(usable / PAGE_SIZE) {
                let entry = &mut self.pages[first + page as usize];
                if !entry.push(bank, page * PAGE_SIZE) {
                    tracing::warn!(
                        bank = BANK_NAMES[bank as usize],
                        "more than {MAX_BANKS_PER_PAGE} banks mapped to one VRAM page; dropped"
                    );
                }
            }
        }
    }
}

/// Where a bank lands, given its `MST` and `OFS` fields.
///
/// This is the whole of the DS's VRAM assignment logic and the single place a mistake here shows
/// up as "the graphics are drawn from the wrong memory". It is written as one table rather than
/// nine because the shape — same `MST` meaning different things per bank — is exactly the thing
/// that is easy to get subtly wrong when it is spread out.
///
/// Returns `None` for an `MST` the bank does not define, which hardware leaves undefined and this
/// treats as unmapped.
fn bank_mapping(bank: u8, mst: u8, ofs: u8) -> Option<(VramSpace, u32)> {
    let ofs = ofs as u32;
    Some(match bank {
        // A and B differ only in where they sit in the LCDC window.
        0 | 1 => match mst {
            0 => (Lcdc, if bank == 0 { 0x0_0000 } else { 0x2_0000 }),
            1 => (BgA, 0x2_0000 * ofs),
            2 => (ObjA, 0x2_0000 * (ofs & 1)),
            3 => (Texture, 0x2_0000 * ofs),
            _ => return None,
        },
        // C and D add the ARM7 assignment and a second-engine role, and differ in which.
        2 | 3 => match mst {
            0 => (Lcdc, if bank == 2 { 0x4_0000 } else { 0x6_0000 }),
            1 => (BgA, 0x2_0000 * ofs),
            2 => (Arm7, 0x2_0000 * (ofs & 1)),
            3 => (Texture, 0x2_0000 * ofs),
            4 => {
                if bank == 2 {
                    (BgB, 0)
                } else {
                    (ObjB, 0)
                }
            }
            _ => return None,
        },
        4 => match mst {
            0 => (Lcdc, 0x8_0000),
            1 => (BgA, 0),
            2 => (ObjA, 0),
            3 => (TexturePalette, 0),
            4 => (BgExtPalA, 0),
            _ => return None,
        },
        // F and G are 16 KiB, so their OFS splits into a 16 KiB step and a 64 KiB step — the two
        // bits do not form one number here, which is the trap in this table.
        5 | 6 => {
            let fine = 0x4000 * (ofs & 1) + 0x1_0000 * (ofs >> 1);
            match mst {
                0 => (Lcdc, if bank == 5 { 0x9_0000 } else { 0x9_4000 }),
                1 => (BgA, fine),
                2 => (ObjA, fine),
                3 => (TexturePalette, fine),
                // The extended-palette space is only 32 KiB, so only the low OFS bit selects.
                4 => (BgExtPalA, 0x4000 * (ofs & 1)),
                5 => (ObjExtPalA, 0),
                _ => return None,
            }
        }
        7 => match mst {
            0 => (Lcdc, 0x9_8000),
            1 => (BgB, 0),
            2 => (BgExtPalB, 0),
            _ => return None,
        },
        8 => match mst {
            0 => (Lcdc, 0xA_0000),
            // I sits above H in engine B's background space, not at its start.
            1 => (BgB, 0x0_8000),
            2 => (ObjB, 0),
            3 => (ObjExtPalB, 0),
            _ => return None,
        },
        _ => return None,
    })
}

/// Decode an ARM9 VRAM address into a space and an offset.
///
/// Each of the four engine windows is 2 MiB of address space holding at most 512 KiB of mapping,
/// so each mirrors within its window. The LCDC window does not mirror: past the 656 KiB that
/// exists, nothing answers.
#[inline]
pub fn arm9_space(addr: u32) -> Option<(VramSpace, u32)> {
    match (addr >> 21) & 0x7 {
        0 => Some((BgA, addr & 0x7_FFFF)),
        1 => Some((BgB, addr & 0x1_FFFF)),
        2 => Some((ObjA, addr & 0x3_FFFF)),
        3 => Some((ObjB, addr & 0x1_FFFF)),
        _ => {
            let offset = addr & 0xF_FFFF;
            (offset < TOTAL_VRAM as u32).then_some((Lcdc, offset))
        }
    }
}

/// Decode an ARM7 VRAM address. The ARM7 sees only whichever of banks C and D were given to it,
/// as one 256 KiB window mirrored through the region.
#[inline]
pub fn arm7_space(addr: u32) -> (VramSpace, u32) {
    (Arm7, addr & 0x3_FFFF)
}

impl Savable for Vram {
    fn save(&self, w: &mut StateWriter) {
        for bank in &self.banks {
            w.write_bytes(bank);
        }
        w.write_bytes(&self.cnt);
    }

    fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        for bank in &mut self.banks {
            r.read_bytes(bank)?;
        }
        r.read_bytes(&mut self.cnt)?;
        // The page table is derived, never stored: a state written by a build with a different
        // page layout would otherwise restore a mapping this build cannot interpret.
        self.rebuild();
        Ok(())
    }
}

#[cfg(test)]
mod tests;
