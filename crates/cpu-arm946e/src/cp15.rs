//! CP15, the ARM946E-S system control coprocessor.
//!
//! # Scope
//!
//! The ARM946E-S has a **protection unit**, not an MMU. There is no page table, no TLB, and no
//! virtual-to-physical translation: CP15 c6 configures up to eight protection *regions* with
//! access permissions and cache/write-buffer attributes, and that is all. Implementing this as
//! though it were a full ARMv5 MMU would be both wrong and a great deal of wasted work.
//!
//! What actually changes program-visible behavior on the DS, and is therefore implemented for
//! real here:
//!
//! - **c1 control**: ITCM/DTCM enable and load mode, and the high-vector bit that moves the
//!   exception vectors to `0xFFFF_0000` — which is where the DS runs them.
//! - **c9,c1**: TCM base and size.
//! - **c7,c0,4**: wait-for-interrupt, which halts the core.
//!
//! The permission and cachability registers (c2, c3, c5, c6) are stored and read back exactly
//! as written. They are honest storage rather than enforcement: the DS does not run untrusted
//! code against them, so a permission fault would only ever fire on an emulator bug, and
//! pretending to enforce them would add a fault path that never legitimately triggers.
//! Caches are not modelled as storage at all — see [`Cp15::cache_operation`].

use core_common::{StateError, StateReader, StateWriter};

/// Control register bits, per the ARM946E-S TRM.
pub mod control {
    /// Protection unit enable.
    pub const MPU: u32 = 1 << 0;
    /// Data cache enable.
    pub const DCACHE: u32 = 1 << 2;
    /// Instruction cache enable.
    pub const ICACHE: u32 = 1 << 12;
    /// Exception vectors at `0xFFFF_0000` instead of `0x0000_0000`.
    pub const HIGH_VECTORS: u32 = 1 << 13;
    /// Round-robin cache replacement instead of pseudo-random.
    pub const ROUND_ROBIN: u32 = 1 << 14;
    pub const DTCM_ENABLE: u32 = 1 << 16;
    pub const DTCM_LOAD_MODE: u32 = 1 << 17;
    pub const ITCM_ENABLE: u32 = 1 << 18;
    pub const ITCM_LOAD_MODE: u32 = 1 << 19;

    /// Bits 3-6 read as one on this core and cannot be cleared.
    pub const READ_AS_ONE: u32 = 0b0111_1000;
}

/// `0x41` = ARM Ltd, `05` = ARMv5TE, `946` = part number, `1` = revision.
pub const MAIN_ID: u32 = 0x4105_9461;
/// Cache type: 4 KiB data cache, 8 KiB instruction cache, both 4-way with 32-byte lines.
pub const CACHE_TYPE: u32 = 0x0F0D_2112;

/// What a CP15 write asked the core to do, when it is more than storing a value.
///
/// Returned rather than acted on, because the effects reach outside CP15 — into the TCM
/// configuration and the core's halt state — and CP15 has no business reaching in there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cp15Effect {
    None,
    /// The control register changed; TCM enables and the vector base may need re-applying.
    ControlChanged,
    /// c9,c1,0 was written.
    DtcmConfigured(u32),
    /// c9,c1,1 was written.
    ItcmConfigured(u32),
    /// c7,c0,4: wait for interrupt.
    WaitForInterrupt,
}

/// The CP15 register file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cp15 {
    control: u32,
    /// c2: cachability, data and instruction.
    cachability: [u32; 2],
    /// c3: write-bufferability.
    write_buffer: u32,
    /// c5: access permissions, data and instruction, in both the legacy and extended formats.
    permissions: [u32; 4],
    /// c6: eight protection region base/size registers.
    regions: [u32; 8],
    /// c9,c0: cache lockdown, data and instruction.
    lockdown: [u32; 2],
    /// c9,c1: TCM size and base, data then instruction.
    tcm_config: [u32; 2],
    /// c13: trace process ID.
    trace_process_id: u32,
}

impl Default for Cp15 {
    fn default() -> Self {
        Self::new()
    }
}

impl Cp15 {
    /// Power-on state.
    ///
    /// Caches, the protection unit, and both TCMs are disabled; only the read-as-one bits of
    /// the control register are set. The DS firmware configures everything else, and
    /// [`Cp15::post_boot_nds`] reproduces the result for running without firmware.
    pub fn new() -> Self {
        Self {
            control: control::READ_AS_ONE,
            cachability: [0; 2],
            write_buffer: 0,
            permissions: [0; 4],
            regions: [0; 8],
            lockdown: [0; 2],
            tcm_config: [0; 2],
            trace_process_id: 0,
        }
    }

    /// The configuration the DS firmware leaves behind, for booting without it.
    ///
    /// ITCM sits at address zero and DTCM at `0x027C_0000`, each configured to exactly its
    /// physical size (32 KiB and 16 KiB). Both caches and the protection unit are on, and the
    /// vectors are high, which is how the DS runs.
    ///
    /// Real firmware may configure an ITCM *window* larger than the physical SRAM, relying on
    /// it to mirror; that is supported (see [`crate::Tcm`]) but not assumed here, because a
    /// window that overlaps main RAM would shadow it and this is the conservative choice.
    pub fn post_boot_nds(&mut self) -> (u32, u32) {
        self.control = control::READ_AS_ONE
            | control::MPU
            | control::DCACHE
            | control::ICACHE
            | control::HIGH_VECTORS
            | control::DTCM_ENABLE
            | control::ITCM_ENABLE;
        // Size fields 5 and 6: 512 << 5 = 16 KiB of DTCM, 512 << 6 = 32 KiB of ITCM.
        self.tcm_config = [0x027C_000A, 0x0000_000C];
        (self.tcm_config[0], self.tcm_config[1])
    }

    #[inline]
    pub fn control(&self) -> u32 {
        self.control
    }

    #[inline]
    pub fn has(&self, bit: u32) -> bool {
        self.control & bit != 0
    }

    /// Where the exception vectors live, per the high-vector control bit.
    #[inline]
    pub fn exception_base(&self) -> u32 {
        if self.has(control::HIGH_VECTORS) {
            0xFFFF_0000
        } else {
            0x0000_0000
        }
    }

    #[inline]
    pub fn dtcm_config(&self) -> u32 {
        self.tcm_config[0]
    }

    #[inline]
    pub fn itcm_config(&self) -> u32 {
        self.tcm_config[1]
    }

    /// Read a CP15 register. Unimplemented encodings read as zero, matching a core that
    /// simply has nothing wired to them.
    pub fn read(&self, opcode1: u32, crn: u32, crm: u32, opcode2: u32) -> u32 {
        match (opcode1, crn, crm, opcode2) {
            (0, 0, 0, 0) => MAIN_ID,
            (0, 0, 0, 1) => CACHE_TYPE,
            (0, 0, 0, 2) => self.tcm_size_register(),
            (0, 1, 0, 0) => self.control,
            (0, 2, 0, n @ (0 | 1)) => self.cachability[n as usize],
            (0, 3, 0, 0) => self.write_buffer,
            (0, 5, 0, n @ (0..=3)) => self.permissions[n as usize],
            (0, 6, region, _) if region < 8 => self.regions[region as usize],
            (0, 9, 0, n @ (0 | 1)) => self.lockdown[n as usize],
            (0, 9, 1, n @ (0 | 1)) => self.tcm_config[n as usize],
            (0, 13, _, 1) => self.trace_process_id,
            _ => {
                tracing::debug!(
                    "read of unimplemented CP15 c{crn},c{crm},{opcode1},{opcode2} returns 0"
                );
                0
            }
        }
    }

    /// Write a CP15 register, returning any effect the caller must apply.
    pub fn write(
        &mut self,
        opcode1: u32,
        crn: u32,
        crm: u32,
        opcode2: u32,
        value: u32,
    ) -> Cp15Effect {
        match (opcode1, crn, crm, opcode2) {
            (0, 1, 0, 0) => {
                // Bits 3-6 are hardwired to one and cannot be cleared by a write.
                self.control = value | control::READ_AS_ONE;
                Cp15Effect::ControlChanged
            }
            (0, 2, 0, n @ (0 | 1)) => {
                self.cachability[n as usize] = value;
                Cp15Effect::None
            }
            (0, 3, 0, 0) => {
                self.write_buffer = value;
                Cp15Effect::None
            }
            (0, 5, 0, n @ (0..=3)) => {
                self.permissions[n as usize] = value;
                Cp15Effect::None
            }
            (0, 6, region, _) if region < 8 => {
                self.regions[region as usize] = value;
                Cp15Effect::None
            }
            (0, 7, 0, 4) => Cp15Effect::WaitForInterrupt,
            (0, 7, ..) => {
                // Every other c7 encoding is a cache maintenance operation.
                self.cache_operation(crm, opcode2);
                Cp15Effect::None
            }
            (0, 9, 0, n @ (0 | 1)) => {
                self.lockdown[n as usize] = value;
                Cp15Effect::None
            }
            (0, 9, 1, 0) => {
                self.tcm_config[0] = value;
                Cp15Effect::DtcmConfigured(value)
            }
            (0, 9, 1, 1) => {
                self.tcm_config[1] = value;
                Cp15Effect::ItcmConfigured(value)
            }
            (0, 13, _, 1) => {
                self.trace_process_id = value;
                Cp15Effect::None
            }
            _ => {
                tracing::debug!("write to unimplemented CP15 c{crn},c{crm},{opcode1},{opcode2}");
                Cp15Effect::None
            }
        }
    }

    /// Cache maintenance: invalidate, clean, drain the write buffer, and so on.
    ///
    /// These are all no-ops here, and that is correct rather than a shortcut. The caches are
    /// not modelled as storage — every read and write goes straight to memory — so the cache
    /// and memory can never disagree, and there is nothing for an invalidate or a clean to
    /// reconcile. This is precisely the "functional correctness, not cycle-exact timing" bar:
    /// software that flushes before a DMA still sees the right data, because it always would.
    ///
    /// The day cache *timing* is modelled (a later performance pass), these operations grow
    /// real bodies. Until then a stub that logs is more honest than one that pretends.
    fn cache_operation(&self, crm: u32, opcode2: u32) {
        tracing::trace!(
            "CP15 cache operation c7,c{crm},{opcode2} (no-op: caches are not modelled as storage)"
        );
    }

    /// c0,c0,2 reports the TCM sizes the core was built with.
    fn tcm_size_register(&self) -> u32 {
        // ITCM in bits 3:0 shifted, DTCM in bits 21:18 — the encoding the DS reads back.
        0x0014_0180
    }

    pub(crate) fn save(&self, w: &mut StateWriter) {
        w.write_u32(self.control);
        for v in self.cachability {
            w.write_u32(v);
        }
        w.write_u32(self.write_buffer);
        for v in self.permissions {
            w.write_u32(v);
        }
        for v in self.regions {
            w.write_u32(v);
        }
        for v in self.lockdown {
            w.write_u32(v);
        }
        for v in self.tcm_config {
            w.write_u32(v);
        }
        w.write_u32(self.trace_process_id);
    }

    pub(crate) fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        self.control = r.read_u32()?;
        for v in self.cachability.iter_mut() {
            *v = r.read_u32()?;
        }
        self.write_buffer = r.read_u32()?;
        for v in self.permissions.iter_mut() {
            *v = r.read_u32()?;
        }
        for v in self.regions.iter_mut() {
            *v = r.read_u32()?;
        }
        for v in self.lockdown.iter_mut() {
            *v = r.read_u32()?;
        }
        for v in self.tcm_config.iter_mut() {
            *v = r.read_u32()?;
        }
        self.trace_process_id = r.read_u32()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identification_registers_are_read_only_and_correct() {
        let mut cp15 = Cp15::new();
        assert_eq!(cp15.read(0, 0, 0, 0), MAIN_ID);
        assert_eq!(cp15.read(0, 0, 0, 1), CACHE_TYPE);

        // Writing an ID register does nothing.
        cp15.write(0, 0, 0, 0, 0xDEAD_BEEF);
        assert_eq!(cp15.read(0, 0, 0, 0), MAIN_ID);
    }

    #[test]
    fn reset_leaves_everything_disabled_but_the_hardwired_bits() {
        let cp15 = Cp15::new();
        assert_eq!(cp15.control(), control::READ_AS_ONE);
        assert!(!cp15.has(control::MPU));
        assert!(!cp15.has(control::ICACHE));
        assert!(!cp15.has(control::ITCM_ENABLE));
        assert_eq!(cp15.exception_base(), 0, "low vectors at reset");
    }

    #[test]
    fn the_hardwired_control_bits_cannot_be_cleared() {
        let mut cp15 = Cp15::new();
        cp15.write(0, 1, 0, 0, 0);
        assert_eq!(cp15.control() & control::READ_AS_ONE, control::READ_AS_ONE);
    }

    #[test]
    fn the_high_vector_bit_moves_the_exception_base() {
        let mut cp15 = Cp15::new();
        assert_eq!(
            cp15.write(0, 1, 0, 0, control::HIGH_VECTORS),
            Cp15Effect::ControlChanged
        );
        assert_eq!(cp15.exception_base(), 0xFFFF_0000);
    }

    #[test]
    fn tcm_configuration_writes_report_their_effect() {
        let mut cp15 = Cp15::new();
        assert_eq!(
            cp15.write(0, 9, 1, 0, 0x027C_000A),
            Cp15Effect::DtcmConfigured(0x027C_000A)
        );
        assert_eq!(
            cp15.write(0, 9, 1, 1, 0x0000_0020),
            Cp15Effect::ItcmConfigured(0x0000_0020)
        );
        assert_eq!(cp15.read(0, 9, 1, 0), 0x027C_000A);
        assert_eq!(cp15.read(0, 9, 1, 1), 0x0000_0020);
    }

    #[test]
    fn wait_for_interrupt_is_reported_rather_than_stored() {
        let mut cp15 = Cp15::new();
        assert_eq!(cp15.write(0, 7, 0, 4, 0), Cp15Effect::WaitForInterrupt);
    }

    #[test]
    fn cache_maintenance_operations_are_accepted_and_ignored() {
        let mut cp15 = Cp15::new();
        // Invalidate I-cache, clean D-cache, drain write buffer — all no-ops because the
        // caches are not modelled as storage.
        for (crm, op2) in [(5u32, 0u32), (10, 0), (10, 4), (6, 0)] {
            assert_eq!(cp15.write(0, 7, crm, op2, 0), Cp15Effect::None);
        }
    }

    #[test]
    fn permission_and_region_registers_store_what_was_written() {
        let mut cp15 = Cp15::new();
        cp15.write(0, 5, 0, 0, 0x1234_5678);
        assert_eq!(cp15.read(0, 5, 0, 0), 0x1234_5678);

        for region in 0..8 {
            cp15.write(0, 6, region, 0, 0x1000 + region);
            assert_eq!(cp15.read(0, 6, region, 0), 0x1000 + region);
        }
    }

    #[test]
    fn unimplemented_registers_read_as_zero_instead_of_panicking() {
        let mut cp15 = Cp15::new();
        assert_eq!(cp15.read(0, 4, 0, 0), 0);
        assert_eq!(cp15.write(0, 4, 0, 0, 0xFFFF), Cp15Effect::None);
    }

    #[test]
    fn post_boot_configuration_matches_what_the_firmware_leaves() {
        let mut cp15 = Cp15::new();
        let (dtcm, itcm) = cp15.post_boot_nds();
        assert!(cp15.has(control::ITCM_ENABLE));
        assert!(cp15.has(control::DTCM_ENABLE));
        assert_eq!(
            cp15.exception_base(),
            0xFFFF_0000,
            "the DS runs high vectors"
        );
        assert_eq!(dtcm & 0xFFFF_F000, 0x027C_0000);
        assert_eq!(itcm, 0x0000_000C);
    }

    #[test]
    fn round_trips_through_a_save_state() {
        let mut cp15 = Cp15::new();
        cp15.post_boot_nds();
        cp15.write(0, 5, 0, 2, 0xAAAA);
        cp15.write(0, 6, 3, 0, 0xBBBB);

        let mut w = StateWriter::new();
        cp15.save(&mut w);
        let blob = w.into_inner();

        let mut restored = Cp15::new();
        restored.load(&mut StateReader::new(&blob)).unwrap();
        assert_eq!(restored, cp15);
    }
}
