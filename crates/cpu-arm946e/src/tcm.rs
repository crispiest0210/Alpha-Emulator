//! Tightly-coupled memory.
//!
//! TCM is not a cache: it is real, physically separate SRAM that sits between the core and
//! the bus and *replaces* whatever the bus would otherwise have at those addresses. Software
//! relies on that — DS code puts hot routines in ITCM and its stack in DTCM precisely because
//! access is deterministic and never goes near the bus — so this is modelled functionally
//! even though cache *timing* is deliberately out of scope.
//!
//! # Sizing
//!
//! CP15 gives each TCM a base and a size, encoded as `size = 512 << N` with `N` in bits 5:1
//! and the base in bits 31:12. The configured region can be larger than the physical SRAM, in
//! which case the SRAM mirrors throughout it. The DS relies on this: it configures ITCM's
//! region far larger than its 32 KiB of actual memory.

use core_common::{StateError, StateReader, StateWriter};

/// One tightly-coupled memory region.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tcm {
    /// The physical SRAM. Never resized; only the region it appears in changes.
    data: Box<[u8]>,
    base: u32,
    /// Region size from CP15, which may exceed `data.len()` — the SRAM then mirrors.
    region_size: u32,
    enabled: bool,
    /// CP15 "load mode": the TCM absorbs writes but reads fall through to the bus, so a
    /// region can be filled before being switched into service. The DS firmware does not use
    /// it, but it is modelled rather than ignored so that code which does behaves sanely.
    load_mode: bool,
}

impl Tcm {
    pub fn new(physical_size: usize) -> Self {
        Self {
            data: vec![0; physical_size].into_boxed_slice(),
            base: 0,
            region_size: 0,
            enabled: false,
            load_mode: false,
        }
    }

    #[inline]
    pub fn physical_size(&self) -> usize {
        self.data.len()
    }

    #[inline]
    pub fn base(&self) -> u32 {
        self.base
    }

    #[inline]
    pub fn region_size(&self) -> u32 {
        self.region_size
    }

    #[inline]
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    #[inline]
    pub fn is_load_mode(&self) -> bool {
        self.load_mode
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    pub fn set_load_mode(&mut self, load_mode: bool) {
        self.load_mode = load_mode;
    }

    /// Apply a CP15 c9,c1 size/base register write.
    ///
    /// `base_is_fixed` is true for ITCM, whose base field the hardware ignores — ITCM always
    /// starts at address zero.
    pub fn configure(&mut self, register: u32, base_is_fixed: bool) {
        let size_field = (register >> 1) & 0x1F;
        self.region_size = 512u32.checked_shl(size_field).unwrap_or(0);
        self.base = if base_is_fixed {
            0
        } else {
            register & 0xFFFF_F000
        };
    }

    /// The CP15 register value this configuration reads back as.
    pub fn size_register(&self) -> u32 {
        let size_field = if self.region_size < 512 {
            0
        } else {
            (self.region_size / 512).trailing_zeros()
        };
        (self.base & 0xFFFF_F000) | ((size_field & 0x1F) << 1)
    }

    /// Whether `addr` falls inside the configured region and the TCM is servicing reads.
    #[inline]
    pub fn responds_to_read(&self, addr: u32) -> bool {
        self.enabled && !self.load_mode && self.in_region(addr)
    }

    /// Whether `addr` falls inside the configured region and the TCM absorbs writes.
    #[inline]
    pub fn responds_to_write(&self, addr: u32) -> bool {
        self.enabled && self.in_region(addr)
    }

    #[inline]
    fn in_region(&self, addr: u32) -> bool {
        self.region_size != 0 && addr.wrapping_sub(self.base) < self.region_size
    }

    /// Index into the physical SRAM, wrapping so an oversized region mirrors.
    #[inline]
    fn offset(&self, addr: u32) -> usize {
        (addr.wrapping_sub(self.base) as usize) % self.data.len()
    }

    #[inline]
    pub fn read8(&self, addr: u32) -> u8 {
        self.data[self.offset(addr)]
    }

    #[inline]
    pub fn write8(&mut self, addr: u32, value: u8) {
        let offset = self.offset(addr);
        self.data[offset] = value;
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.data
    }

    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        &mut self.data
    }

    pub(crate) fn save(&self, w: &mut StateWriter) {
        // The contents are saved: unlike a cache, TCM holds data that exists nowhere else.
        w.write_blob(&self.data);
        w.write_u32(self.base);
        w.write_u32(self.region_size);
        w.write_bool(self.enabled);
        w.write_bool(self.load_mode);
    }

    pub(crate) fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        let bytes = r.read_blob()?;
        if bytes.len() != self.data.len() {
            return Err(StateError::Malformed(format!(
                "TCM is {} bytes in this build, {} in the save state",
                self.data.len(),
                bytes.len()
            )));
        }
        self.data.copy_from_slice(bytes);
        self.base = r.read_u32()?;
        self.region_size = r.read_u32()?;
        self.enabled = r.read_bool()?;
        self.load_mode = r.read_bool()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn size_field_decodes_as_512_shifted() {
        let mut tcm = Tcm::new(0x8000);
        // N = 5 gives 512 << 5 = 16 KiB.
        tcm.configure(0x0270_000A, false);
        assert_eq!(tcm.region_size(), 16 * 1024);
        assert_eq!(tcm.base(), 0x0270_0000);

        // N = 6 gives 32 KiB.
        tcm.configure(0x0000_000C, false);
        assert_eq!(tcm.region_size(), 32 * 1024);
    }

    #[test]
    fn itcm_ignores_the_base_field() {
        let mut tcm = Tcm::new(0x8000);
        tcm.configure(0x0270_000C, true);
        assert_eq!(tcm.base(), 0, "ITCM is always at address zero");
        assert_eq!(tcm.region_size(), 32 * 1024);
    }

    #[test]
    fn the_size_register_round_trips() {
        let mut tcm = Tcm::new(0x4000);
        tcm.configure(0x027C_000A, false);
        assert_eq!(tcm.size_register(), 0x027C_000A);
    }

    #[test]
    fn a_region_larger_than_the_sram_mirrors() {
        let mut tcm = Tcm::new(512);
        tcm.configure(0x0000_0006, false); // 512 << 3 = 4 KiB region over 512 bytes of SRAM
        tcm.set_enabled(true);
        assert_eq!(tcm.region_size(), 4096);

        tcm.write8(0, 0xAA);
        assert_eq!(tcm.read8(512), 0xAA, "the SRAM repeats through the region");
        assert_eq!(tcm.read8(1024), 0xAA);
    }

    #[test]
    fn a_disabled_tcm_responds_to_nothing() {
        let mut tcm = Tcm::new(512);
        tcm.configure(0x0000_0002, false);
        assert!(!tcm.responds_to_read(0));
        tcm.set_enabled(true);
        assert!(tcm.responds_to_read(0));
        assert!(!tcm.responds_to_read(0x1_0000), "outside the region");
    }

    #[test]
    fn load_mode_absorbs_writes_while_reads_fall_through() {
        let mut tcm = Tcm::new(512);
        tcm.configure(0x0000_0002, false);
        tcm.set_enabled(true);
        tcm.set_load_mode(true);
        assert!(tcm.responds_to_write(0), "writes still land in the TCM");
        assert!(!tcm.responds_to_read(0), "but reads go to the bus");
    }

    #[test]
    fn contents_round_trip_through_a_save_state() {
        let mut tcm = Tcm::new(512);
        tcm.configure(0x0000_0002, false);
        tcm.set_enabled(true);
        tcm.write8(16, 0x5A);

        let mut w = StateWriter::new();
        tcm.save(&mut w);
        let blob = w.into_inner();

        let mut restored = Tcm::new(512);
        restored.load(&mut StateReader::new(&blob)).unwrap();
        assert_eq!(restored, tcm);
    }
}
