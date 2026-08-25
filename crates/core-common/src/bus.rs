//! The memory bus abstraction: [`Bus`], [`MemoryRegion`], and [`RegionMap`].
//!
//! # Widths
//!
//! [`Bus`] exposes 8/16/32-bit accesses, and all four are required — there is no default that
//! composes a wide access out of byte accesses anymore. There used to be one, and it caused two
//! separate serious bugs before it was removed:
//!
//! - `system-nds`'s `NdsBus` inherited the default, so every ARM9/ARM7 instruction fetch paid
//!   four region decodes instead of one. That was a 65% frame-time regression on the DS, found
//!   only by profiling — nothing about it looked wrong, it was just slow.
//! - `cpu-arm946e`'s `TcmBus` inherited the default too, and that one was silent rather than
//!   slow: real DS hardware *drops* an ARM9 byte write to VRAM, palette RAM, and OAM, so
//!   decomposing a word write into four byte writes made every such write vanish. The ARM9
//!   could not write to VRAM at all, and it presented as a black screen with every register set
//!   correctly — nothing about *that* looked wrong either, until someone traced it.
//!
//! Both bugs have the same shape: a bus wrapper forwards `read8`/`write8` and inherits the wide
//! methods for free, which is exactly wrong whenever the underlying system's wide accesses are
//! not decomposable byte-by-byte — which is true of every real bus in this project except the
//! Game Boy's. Making the four methods required turns that choice into something that has to
//! appear in a diff instead of something that happens by omission. An implementor whose bus
//! genuinely is byte-oriented calls [`compose_le_read16`] and friends explicitly instead of
//! inheriting them, so the "yes, really, compose this" decision is visible at the call site.
//!
//! - The Game Boy's bus is byte-oriented, so `system-gb` and every test-only bus in this
//!   codebase implement all four methods but the wide ones just call the `compose_le_*` helpers.
//! - The GBA and DS have genuinely 16/32-bit-wide buses where a word access is *one* bus
//!   transaction with its own wait-state cost, not four byte transactions, and those systems
//!   give the wide accessors native implementations instead of calling the helpers.
//!
//! # Open bus is explicit
//!
//! [`Bus::open_bus8`] is a required method. Reading unmapped address space is real, observable
//! hardware behavior that differs per system — the GBA returns prefetched instruction data,
//! the Game Boy typically returns `0xFF` — and games depend on it. Making it a required
//! method rather than a default that returns zero means no system can accidentally inherit
//! the wrong behavior by omission.

use savestate::Savable;

/// A CPU-visible address. 32 bits covers every system here; the smaller systems simply never
/// set the high bits.
pub type Addr = u32;

/// Read/write access to a system's whole address space, including MMIO side effects.
///
/// # Contract for implementers
///
/// - Reads and writes **may have side effects**. Reading an MMIO register can clear a latch,
///   advance a FIFO, or acknowledge an interrupt. That is why [`read8`](Bus::read8) takes
///   `&mut self`. Anything that must not disturb the machine — a debugger's memory view — uses
///   [`peek8`](Bus::peek8) instead.
/// - Unmapped reads must go through [`open_bus8`](Bus::open_bus8), never silently return 0.
/// - All four widths are required, on purpose — see the module docs for the two bugs a default
///   composition caused. Every system here is little-endian. An implementor whose bus really is
///   byte-oriented should implement the wide methods by calling [`compose_le_read16`],
///   [`compose_le_read32`], [`compose_le_write16`], and [`compose_le_write32`] explicitly, so
///   that choice shows up in the diff instead of being inherited invisibly.
/// - Alignment handling is the implementer's business. The ARM cores' rotate-on-unaligned-read
///   behavior belongs in the system's bus, not here.
pub trait Bus: Savable {
    fn read8(&mut self, addr: Addr) -> u8;
    fn write8(&mut self, addr: Addr, value: u8);

    /// Account for time passing inside an instruction.
    ///
    /// A CPU calls this as each memory access happens, *before* performing it, so the rest of
    /// the machine observes the access at the right moment rather than at the end of whatever
    /// instruction contained it.
    ///
    /// That distinction is measurable. A Game Boy instruction that reads a timer register
    /// takes several machine cycles, and which cycle the read lands on decides what value
    /// comes back — Blargg's `mem_timing` suite exists specifically to catch emulators that
    /// charge an instruction's cycles in one lump at its end.
    ///
    /// The default does nothing, which is right for a bus with nothing else running against
    /// it: a test harness, or a system whose subsystems are advanced by the caller instead.
    #[inline]
    fn tick(&mut self, _cycles: crate::Cycles) {}

    /// What a read of unmapped address space returns on this system.
    ///
    /// Required, not defaulted: see the module docs. `&self` because determining the open-bus
    /// value must not itself perturb the machine.
    fn open_bus8(&self, addr: Addr) -> u8;

    fn read16(&mut self, addr: Addr) -> u16;
    fn read32(&mut self, addr: Addr) -> u32;
    fn write16(&mut self, addr: Addr, value: u16);
    fn write32(&mut self, addr: Addr, value: u32);

    /// Side-effect-free read for the debugger and the test harness.
    ///
    /// Returns `None` when the address cannot be inspected without disturbing the machine, so
    /// a memory viewer can render `??` rather than lying. Defaults to `None` for every
    /// address: a bus that has not thought about which of its regions are safely peekable
    /// should say so rather than claim MMIO reads are free.
    #[inline]
    fn peek8(&self, _addr: Addr) -> Option<u8> {
        None
    }

    #[inline]
    fn peek16(&self, addr: Addr) -> Option<u16> {
        Some(u16::from_le_bytes([
            self.peek8(addr)?,
            self.peek8(addr.wrapping_add(1))?,
        ]))
    }

    #[inline]
    fn peek32(&self, addr: Addr) -> Option<u32> {
        Some(u32::from_le_bytes([
            self.peek8(addr)?,
            self.peek8(addr.wrapping_add(1))?,
            self.peek8(addr.wrapping_add(2))?,
            self.peek8(addr.wrapping_add(3))?,
        ]))
    }
}

/// Composes a little-endian 16-bit read out of two [`Bus::read8`] calls, low byte first.
///
/// For implementors whose bus is genuinely byte-oriented and should get correct wide accessors
/// by explicitly opting into composition — not by leaving the method unimplemented and hoping.
/// See the module docs for why this is a named function an implementor calls, rather than a
/// trait default it inherits.
#[inline]
pub fn compose_le_read16<B: Bus + ?Sized>(bus: &mut B, addr: Addr) -> u16 {
    u16::from_le_bytes([bus.read8(addr), bus.read8(addr.wrapping_add(1))])
}

/// Composes a little-endian 32-bit read out of four [`Bus::read8`] calls, low byte first.
///
/// See [`compose_le_read16`].
#[inline]
pub fn compose_le_read32<B: Bus + ?Sized>(bus: &mut B, addr: Addr) -> u32 {
    u32::from_le_bytes([
        bus.read8(addr),
        bus.read8(addr.wrapping_add(1)),
        bus.read8(addr.wrapping_add(2)),
        bus.read8(addr.wrapping_add(3)),
    ])
}

/// Composes a little-endian 16-bit write out of two [`Bus::write8`] calls, low byte first.
///
/// See [`compose_le_read16`].
#[inline]
pub fn compose_le_write16<B: Bus + ?Sized>(bus: &mut B, addr: Addr, value: u16) {
    let b = value.to_le_bytes();
    bus.write8(addr, b[0]);
    bus.write8(addr.wrapping_add(1), b[1]);
}

/// Composes a little-endian 32-bit write out of four [`Bus::write8`] calls, low byte first.
///
/// See [`compose_le_read16`].
#[inline]
pub fn compose_le_write32<B: Bus + ?Sized>(bus: &mut B, addr: Addr, value: u32) {
    let b = value.to_le_bytes();
    bus.write8(addr, b[0]);
    bus.write8(addr.wrapping_add(1), b[1]);
    bus.write8(addr.wrapping_add(2), b[2]);
    bus.write8(addr.wrapping_add(3), b[3]);
}

/// One contiguous span of address space with its own storage and behavior.
///
/// Regions receive **offsets from their own start**, not absolute addresses, so the same
/// region type can be mapped at different bases on different systems (work RAM lives at
/// wildly different addresses on GB and GBA) without any internal address arithmetic.
pub trait MemoryRegion {
    /// First address this region answers for.
    fn start(&self) -> Addr;

    /// Size in bytes. A region never has zero length; [`RegionMap::insert`] rejects that.
    fn len(&self) -> u32;

    fn read8(&mut self, offset: u32) -> u8;
    fn write8(&mut self, offset: u32, value: u8);

    /// Side-effect-free read; see [`Bus::peek8`]. Plain RAM regions should override this to
    /// return the byte, since inspecting RAM is always safe.
    fn peek8(&self, _offset: u32) -> Option<u8> {
        None
    }

    #[inline]
    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    #[inline]
    fn end(&self) -> Addr {
        self.start().wrapping_add(self.len())
    }

    #[inline]
    fn contains(&self, addr: Addr) -> bool {
        addr >= self.start() && (addr - self.start()) < self.len()
    }
}

/// Plain read/write memory. The common case, and what most tests want.
#[derive(Debug, Clone)]
pub struct Ram {
    start: Addr,
    /// Power-of-two masking is left to the caller: systems with mirrored RAM (the Game Boy's
    /// echo RAM, the GBA's mirrored EWRAM) map the same `Ram` at several bases or mask the
    /// address before dispatch, rather than baking mirroring into every region.
    bytes: Box<[u8]>,
}

impl Ram {
    pub fn new(start: Addr, len: usize) -> Self {
        Self {
            start,
            bytes: vec![0; len].into_boxed_slice(),
        }
    }

    pub fn from_bytes(start: Addr, bytes: Vec<u8>) -> Self {
        Self {
            start,
            bytes: bytes.into_boxed_slice(),
        }
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.bytes
    }

    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        &mut self.bytes
    }
}

impl MemoryRegion for Ram {
    fn start(&self) -> Addr {
        self.start
    }

    fn len(&self) -> u32 {
        self.bytes.len() as u32
    }

    #[inline]
    fn read8(&mut self, offset: u32) -> u8 {
        self.bytes[offset as usize]
    }

    #[inline]
    fn write8(&mut self, offset: u32, value: u8) {
        self.bytes[offset as usize] = value;
    }

    #[inline]
    fn peek8(&self, offset: u32) -> Option<u8> {
        self.bytes.get(offset as usize).copied()
    }
}

impl Savable for Ram {
    fn save(&self, w: &mut savestate::StateWriter) {
        w.write_u32(self.start);
        self.bytes.save(w);
    }
    fn load(&mut self, r: &mut savestate::StateReader) -> Result<(), savestate::StateError> {
        self.start = r.read_u32()?;
        self.bytes.load(r)
    }
}

/// Why a region could not be mapped.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MapError {
    #[error("region [{new_start:#010X}, {new_end:#010X}) overlaps mapped region [{existing_start:#010X}, {existing_end:#010X})")]
    Overlap {
        new_start: Addr,
        new_end: Addr,
        existing_start: Addr,
        existing_end: Addr,
    },

    #[error("region at {start:#010X} has zero length")]
    ZeroLength { start: Addr },

    #[error("region at {start:#010X} with length {len} wraps past the end of the address space")]
    Wraps { start: Addr, len: u32 },
}

/// A sorted, non-overlapping set of [`MemoryRegion`]s, for composing a bus out of parts.
///
/// # Overlap is rejected, not silently prioritized
///
/// [`insert`](RegionMap::insert) returns [`MapError::Overlap`] rather than picking a winner.
/// Two regions claiming the same address is a bug in the memory map, and a silent precedence
/// rule turns it into a subtle "reads come from the wrong place" bug that surfaces much later
/// as graphical corruption. Systems that genuinely need overlapping views — banked or
/// mirrored windows — express that by mapping one region and masking the offset, which makes
/// the aliasing explicit.
///
/// # Misses return `None`
///
/// Every accessor returns `Option`, forcing the owning [`Bus`] to decide what an unmapped
/// access means via [`Bus::open_bus8`] instead of inheriting a zero.
#[derive(Debug, Clone, Default)]
pub struct RegionMap<R> {
    /// Sorted by `start`, non-overlapping — the invariant `insert` maintains and `find`
    /// relies on for its binary search.
    regions: Vec<R>,
}

impl<R: MemoryRegion> RegionMap<R> {
    pub fn new() -> Self {
        Self {
            regions: Vec::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.regions.len()
    }

    pub fn is_empty(&self) -> bool {
        self.regions.is_empty()
    }

    pub fn regions(&self) -> &[R] {
        &self.regions
    }

    /// Map a region, rejecting zero-length, wrapping, and overlapping spans.
    pub fn insert(&mut self, region: R) -> Result<(), MapError> {
        let start = region.start();
        let len = region.len();
        if len == 0 {
            return Err(MapError::ZeroLength { start });
        }
        if start.checked_add(len).is_none() {
            return Err(MapError::Wraps { start, len });
        }
        let end = start + len;

        for existing in &self.regions {
            let e_start = existing.start();
            let e_end = existing.end();
            if start < e_end && e_start < end {
                return Err(MapError::Overlap {
                    new_start: start,
                    new_end: end,
                    existing_start: e_start,
                    existing_end: e_end,
                });
            }
        }

        let pos = self.regions.partition_point(|r| r.start() < start);
        self.regions.insert(pos, region);
        Ok(())
    }

    /// Index of the region owning `addr`, if any.
    #[inline]
    fn find(&self, addr: Addr) -> Option<usize> {
        // `regions` is sorted and non-overlapping, so the only candidate is the last region
        // starting at or before `addr`.
        let idx = self.regions.partition_point(|r| r.start() <= addr);
        if idx == 0 {
            return None;
        }
        let candidate = idx - 1;
        self.regions[candidate].contains(addr).then_some(candidate)
    }

    #[inline]
    pub fn read8(&mut self, addr: Addr) -> Option<u8> {
        let idx = self.find(addr)?;
        let region = &mut self.regions[idx];
        let offset = addr - region.start();
        Some(region.read8(offset))
    }

    /// Returns whether the write landed in a mapped region.
    #[inline]
    pub fn write8(&mut self, addr: Addr, value: u8) -> bool {
        match self.find(addr) {
            Some(idx) => {
                let region = &mut self.regions[idx];
                let offset = addr - region.start();
                region.write8(offset, value);
                true
            }
            None => false,
        }
    }

    #[inline]
    pub fn peek8(&self, addr: Addr) -> Option<u8> {
        let idx = self.find(addr)?;
        let region = &self.regions[idx];
        region.peek8(addr - region.start())
    }

    pub fn region_at(&self, addr: Addr) -> Option<&R> {
        self.find(addr).map(|i| &self.regions[i])
    }

    pub fn region_at_mut(&mut self, addr: Addr) -> Option<&mut R> {
        self.find(addr).map(|i| &mut self.regions[i])
    }
}

/// Saves each mapped region's contents in map order.
///
/// The set of regions is *not* reconstructed from the state: a save state restores the
/// contents of the memory map this build has, and refuses a state whose map has a different
/// shape. That is deliberate — a state written by a build with a different memory layout is
/// not loadable in any meaningful sense, and pretending otherwise produces corruption that
/// looks like an emulation bug.
impl<R: MemoryRegion + Savable> Savable for RegionMap<R> {
    fn save(&self, w: &mut savestate::StateWriter) {
        w.write_u64(self.regions.len() as u64);
        for region in &self.regions {
            region.save(w);
        }
    }

    fn load(&mut self, r: &mut savestate::StateReader) -> Result<(), savestate::StateError> {
        let count = r.read_u64()? as usize;
        if count != self.regions.len() {
            return Err(savestate::StateError::Malformed(format!(
                "memory map has {} regions in this build, {count} in the save state",
                self.regions.len()
            )));
        }
        for region in &mut self.regions {
            region.load(r)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use savestate::{StateError, StateReader, StateWriter};

    /// A bus built out of a `RegionMap`, with a system-specific open-bus value — the shape
    /// every real system crate will use.
    struct TestBus {
        map: RegionMap<Ram>,
        /// Stands in for the real thing (last prefetched value, etc).
        open_bus_value: u8,
    }

    impl Savable for TestBus {
        fn save(&self, w: &mut StateWriter) {
            w.write_u8(self.open_bus_value);
        }
        fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
            self.open_bus_value = r.read_u8()?;
            Ok(())
        }
    }

    impl Bus for TestBus {
        fn read8(&mut self, addr: Addr) -> u8 {
            self.map.read8(addr).unwrap_or_else(|| self.open_bus8(addr))
        }
        fn write8(&mut self, addr: Addr, value: u8) {
            self.map.write8(addr, value);
        }
        fn open_bus8(&self, _addr: Addr) -> u8 {
            self.open_bus_value
        }
        fn read16(&mut self, addr: Addr) -> u16 {
            compose_le_read16(self, addr)
        }
        fn read32(&mut self, addr: Addr) -> u32 {
            compose_le_read32(self, addr)
        }
        fn write16(&mut self, addr: Addr, value: u16) {
            compose_le_write16(self, addr, value)
        }
        fn write32(&mut self, addr: Addr, value: u32) {
            compose_le_write32(self, addr, value)
        }
        fn peek8(&self, addr: Addr) -> Option<u8> {
            self.map.peek8(addr)
        }
    }

    fn bus() -> TestBus {
        let mut map = RegionMap::new();
        map.insert(Ram::new(0x0000_1000, 0x100)).unwrap();
        map.insert(Ram::new(0x0000_2000, 0x100)).unwrap();
        TestBus {
            map,
            open_bus_value: 0xFF,
        }
    }

    #[test]
    fn reads_and_writes_dispatch_to_the_owning_region() {
        let mut b = bus();
        b.write8(0x1000, 0xAA);
        b.write8(0x10FF, 0xBB);
        b.write8(0x2000, 0xCC);

        assert_eq!(b.read8(0x1000), 0xAA);
        assert_eq!(b.read8(0x10FF), 0xBB);
        assert_eq!(b.read8(0x2000), 0xCC);
        // Regions are independent: same offset, different region, different value.
        assert_eq!(b.read8(0x2001), 0x00);
    }

    #[test]
    fn unmapped_reads_return_open_bus_not_zero() {
        let mut b = bus();
        assert_eq!(b.read8(0x0000), 0xFF);
        assert_eq!(b.read8(0x1100), 0xFF); // one past the end of the first region
        assert_eq!(b.read8(0xFFFF_FFFF), 0xFF);
    }

    #[test]
    fn unmapped_writes_are_dropped_rather_than_panicking() {
        let mut b = bus();
        b.write8(0x5000, 0x42);
        assert_eq!(b.read8(0x5000), 0xFF);
    }

    #[test]
    fn wide_accesses_compose_little_endian() {
        let mut b = bus();
        b.write32(0x1000, 0x1234_5678);
        assert_eq!(b.read8(0x1000), 0x78);
        assert_eq!(b.read8(0x1003), 0x12);
        assert_eq!(b.read16(0x1000), 0x5678);
        assert_eq!(b.read32(0x1000), 0x1234_5678);
    }

    #[test]
    fn wide_reads_spanning_into_unmapped_space_pick_up_open_bus() {
        let mut b = bus();
        b.write8(0x10FF, 0x11);
        // 0x1100 is unmapped, so the upper byte comes from open bus.
        assert_eq!(b.read16(0x10FF), 0xFF11);
    }

    #[test]
    fn peek_is_side_effect_free_and_reports_unmapped_as_none() {
        let mut b = bus();
        b.write8(0x1000, 0x5A);
        assert_eq!(b.peek8(0x1000), Some(0x5A));
        assert_eq!(b.peek8(0x9999), None);
        assert_eq!(b.peek16(0x1000), Some(0x005A));
        // A 16-bit peek straddling the end of a region cannot be answered honestly.
        assert_eq!(b.peek16(0x10FF), None);
    }

    #[test]
    fn overlapping_regions_are_rejected() {
        let mut map: RegionMap<Ram> = RegionMap::new();
        map.insert(Ram::new(0x1000, 0x100)).unwrap();

        // Exact duplicate.
        assert!(matches!(
            map.insert(Ram::new(0x1000, 0x100)),
            Err(MapError::Overlap { .. })
        ));
        // Straddling the start.
        assert!(matches!(
            map.insert(Ram::new(0x0F80, 0x100)),
            Err(MapError::Overlap { .. })
        ));
        // Fully contained.
        assert!(matches!(
            map.insert(Ram::new(0x1010, 0x10)),
            Err(MapError::Overlap { .. })
        ));
        // Exactly abutting is fine — [0x1000,0x1100) and [0x1100,0x1200) don't overlap.
        assert!(map.insert(Ram::new(0x1100, 0x100)).is_ok());
    }

    #[test]
    fn degenerate_regions_are_rejected() {
        let mut map: RegionMap<Ram> = RegionMap::new();
        assert!(matches!(
            map.insert(Ram::new(0x1000, 0)),
            Err(MapError::ZeroLength { .. })
        ));
        assert!(matches!(
            map.insert(Ram::from_bytes(0xFFFF_FF00, vec![0; 0x200])),
            Err(MapError::Wraps { .. })
        ));
    }

    #[test]
    fn lookup_works_regardless_of_insertion_order() {
        let mut map: RegionMap<Ram> = RegionMap::new();
        map.insert(Ram::new(0x3000, 0x100)).unwrap();
        map.insert(Ram::new(0x1000, 0x100)).unwrap();
        map.insert(Ram::new(0x2000, 0x100)).unwrap();

        map.write8(0x2050, 0x7E);
        assert_eq!(map.read8(0x2050), Some(0x7E));
        assert_eq!(map.read8(0x1000), Some(0));
        assert_eq!(map.read8(0x30FF), Some(0));
        assert_eq!(map.read8(0x0FFF), None);
        assert_eq!(map.read8(0x1100), None);
    }

    #[test]
    fn ram_round_trips_through_a_save_state() {
        use savestate::{decode_state, encode_state};
        let mut ram = Ram::new(0x1000, 8);
        ram.write8(3, 0x99);
        let blob = encode_state("test", 1, &ram);

        let mut restored = Ram::new(0, 8);
        decode_state("test", 1, &blob, &mut restored).unwrap();
        assert_eq!(restored.start(), 0x1000);
        assert_eq!(restored.peek8(3), Some(0x99));
    }
}
