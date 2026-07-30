//! Unit tests for the ARM946E-S core.
//!
//! These cover the ARMv5TE delta, CP15's externally visible effects, and TCM interposition.
//! Everything inherited from ARMv4T is tested in `cpu-arm7tdmi` and deliberately not retested
//! here — one spot check that composition works is enough, and duplicating that suite would
//! only make the shared core expensive to change.
//!
//! End-to-end correctness cannot be established at this crate's boundary: a CPU with no system
//! around it has no interrupt controller, no real memory map, and nothing to run. That arrives
//! with `system-nds`, and the accuracy-ROM gate with the test harness.

use crate::*;
use core_common::{Bus, Cpu, CpuIntrospect, Savable, StateError, StateReader, StateWriter};

const ORG: u32 = 0x0200_0000;
const STACK: u32 = 0x0200_8000;

struct TestBus {
    mem: Vec<u8>,
}

impl TestBus {
    fn new() -> Self {
        Self {
            mem: vec![0; 0x1_0000],
        }
    }

    /// Fold the sparse address space onto the backing buffer, so tests can use realistic DS
    /// addresses without allocating the real map.
    fn index(&self, addr: u32) -> usize {
        (addr as usize) & 0xFFFF
    }

    fn load_words(&mut self, at: u32, words: &[u32]) {
        for (i, w) in words.iter().enumerate() {
            let a = self.index(at + i as u32 * 4);
            self.mem[a..a + 4].copy_from_slice(&w.to_le_bytes());
        }
    }

    fn word(&self, at: u32) -> u32 {
        let a = self.index(at);
        u32::from_le_bytes([
            self.mem[a],
            self.mem[a + 1],
            self.mem[a + 2],
            self.mem[a + 3],
        ])
    }
}

impl Bus for TestBus {
    fn read8(&mut self, addr: u32) -> u8 {
        self.mem[self.index(addr)]
    }
    fn write8(&mut self, addr: u32, value: u8) {
        let a = self.index(addr);
        self.mem[a] = value;
    }
    fn open_bus8(&self, _addr: u32) -> u8 {
        0
    }
    fn peek8(&self, addr: u32) -> Option<u8> {
        Some(self.mem[self.index(addr)])
    }
}

impl Savable for TestBus {
    fn save(&self, _w: &mut StateWriter) {}
    fn load(&mut self, _r: &mut StateReader) -> Result<(), core_common::StateError> {
        Ok(())
    }
}

fn boot() -> BootState {
    BootState {
        pc: ORG,
        mode: Mode::System,
        thumb: false,
        sp: STACK,
        irq_disabled: false,
        fiq_disabled: false,
    }
}

fn setup(program: &[u32]) -> (Arm946e, TestBus) {
    let mut bus = TestBus::new();
    bus.load_words(ORG, program);
    (Arm946e::new(boot()), bus)
}

fn step(cpu: &mut Arm946e, bus: &mut TestBus) -> u32 {
    cpu.step(bus).get() as u32
}

// ---------------------------------------------------------------------------
// Inheritance from the shared core
// ---------------------------------------------------------------------------

#[test]
fn armv4t_instructions_still_execute_through_the_shared_core() {
    let (mut cpu, mut bus) = setup(&[0xE3A0_0007, 0xE280_1002]); // mov r0,#7 ; add r1,r0,#2
    step(&mut cpu, &mut bus);
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.reg(0), 7);
    assert_eq!(cpu.reg(1), 9);
}

// ---------------------------------------------------------------------------
// Branches
// ---------------------------------------------------------------------------

#[test]
fn blx_immediate_is_unconditional_and_always_enters_thumb() {
    // This encoding lives in the condition field ARMv4T reads as "never", so a core that
    // checked the condition before decoding would silently skip it.
    let (mut cpu, mut bus) = setup(&[0xFA00_0002]);
    let cycles = step(&mut cpu, &mut bus);
    assert!(cpu.is_thumb());
    assert_eq!(cpu.program_counter(), ORG + 8 + 8);
    assert_eq!(cpu.reg(14), ORG + 4);
    assert_eq!(cycles, 3);
}

#[test]
fn blx_immediate_encodes_a_halfword_in_its_top_bit() {
    // Bit 24 contributes a further two bytes, which is how a THUMB target on an odd halfword
    // boundary becomes reachable.
    let (mut cpu, mut bus) = setup(&[0xFB00_0002]);
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.program_counter(), ORG + 8 + 8 + 2);
}

#[test]
fn blx_register_captures_a_return_address_where_bx_does_not() {
    let (mut cpu, mut bus) = setup(&[0xE12F_FF31]); // blx r1
    cpu.set_reg(1, 0x0200_1001);
    step(&mut cpu, &mut bus);
    assert!(cpu.is_thumb());
    assert_eq!(cpu.program_counter(), 0x0200_1000);
    assert_eq!(cpu.reg(14), ORG + 4);

    // Plain BX must still decode as BX and leave LR alone.
    let (mut cpu, mut bus) = setup(&[0xE12F_FF11]);
    cpu.set_reg(1, 0x0200_1000);
    cpu.set_reg(14, 0xDEAD);
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.reg(14), 0xDEAD);
}

#[test]
fn thumb_blx_suffix_returns_to_arm_state_with_a_word_aligned_target() {
    let mut bus = TestBus::new();
    // A bl high half followed by a blx low half. The blx form uses the 0b11101 prefix where
    // the ARMv4T bl suffix uses 0b11111.
    for (i, half) in [0xF000u16, 0xE87E].iter().enumerate() {
        let a = ORG + i as u32 * 2;
        bus.write8(a, *half as u8);
        bus.write8(a + 1, (*half >> 8) as u8);
    }
    let mut cpu = Arm946e::new(BootState {
        thumb: true,
        ..boot()
    });

    step(&mut cpu, &mut bus);
    assert!(cpu.is_thumb(), "the high half only stages LR");

    step(&mut cpu, &mut bus);
    assert!(!cpu.is_thumb(), "blx leaves THUMB state");
    assert_eq!(cpu.program_counter() & 3, 0, "ARM targets are word-aligned");
    assert_eq!(cpu.reg(14), (ORG + 4) | 1);
}

// ---------------------------------------------------------------------------
// CLZ
// ---------------------------------------------------------------------------

#[test]
fn clz_counts_leading_zeros_including_both_extremes() {
    for (input, expected) in [
        (0x0000_0000u32, 32u32),
        (0x8000_0000, 0),
        (0x0000_0001, 31),
        (0x0000_FFFF, 16),
        (0x0080_0000, 8),
    ] {
        let (mut cpu, mut bus) = setup(&[0xE16F_0F11]); // clz r0, r1
        cpu.set_reg(1, input);
        step(&mut cpu, &mut bus);
        assert_eq!(cpu.reg(0), expected, "clz {input:#010X}");
    }
}

// ---------------------------------------------------------------------------
// Saturating arithmetic
// ---------------------------------------------------------------------------

/// `q{add,sub,dadd,dsub} Rd, Rm, Rn`. Note the operand order: `Rm` first, `Rn` second.
fn qop(op: u32, rd: u32, rm: u32, rn: u32) -> u32 {
    0xE100_0050 | (op << 21) | (rn << 16) | (rd << 12) | rm
}

#[test]
fn qadd_and_qsub_clamp_instead_of_wrapping() {
    // INT_MAX + 1 saturates rather than wrapping to INT_MIN.
    let (mut cpu, mut bus) = setup(&[qop(0, 0, 1, 2)]);
    cpu.set_reg(1, 0x7FFF_FFFF);
    cpu.set_reg(2, 1);
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.reg(0), 0x7FFF_FFFF);
    assert!(cpu.core.cpsr.sticky_overflow());

    // INT_MIN - 1 saturates downwards.
    let (mut cpu, mut bus) = setup(&[qop(1, 0, 1, 2)]);
    cpu.set_reg(1, 0x8000_0000);
    cpu.set_reg(2, 1);
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.reg(0), 0x8000_0000);
    assert!(cpu.core.cpsr.sticky_overflow());

    // Well inside range, nothing sticks.
    let (mut cpu, mut bus) = setup(&[qop(0, 0, 1, 2)]);
    cpu.set_reg(1, 100);
    cpu.set_reg(2, 23);
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.reg(0), 123);
    assert!(!cpu.core.cpsr.sticky_overflow());
}

#[test]
fn qdadd_saturates_the_doubling_separately_from_the_addition() {
    // The doubling overflows on its own, so Q must be set even though the final add would fit
    // had the doubling been allowed to wrap. Folding both steps into one wide expression is
    // the natural way to get this wrong.
    let (mut cpu, mut bus) = setup(&[qop(2, 0, 1, 2)]);
    cpu.set_reg(1, 0);
    cpu.set_reg(2, 0x4000_0000); // doubles to 0x8000_0000
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.reg(0), 0x7FFF_FFFF, "the doubled operand clamps first");
    assert!(cpu.core.cpsr.sticky_overflow());

    // QDSUB computes Rm - 2*Rn.
    let (mut cpu, mut bus) = setup(&[qop(3, 0, 1, 2)]);
    cpu.set_reg(1, 100);
    cpu.set_reg(2, 20);
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.reg(0), 60);
    assert!(!cpu.core.cpsr.sticky_overflow());
}

#[test]
fn the_q_flag_is_sticky() {
    let (mut cpu, mut bus) = setup(&[qop(0, 0, 1, 2), qop(0, 3, 4, 5)]);
    cpu.set_reg(1, 0x7FFF_FFFF);
    cpu.set_reg(2, 1);
    step(&mut cpu, &mut bus);
    assert!(cpu.core.cpsr.sticky_overflow());

    cpu.set_reg(4, 1);
    cpu.set_reg(5, 1);
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.reg(3), 2);
    assert!(
        cpu.core.cpsr.sticky_overflow(),
        "Q stays set until software clears it"
    );
}

// ---------------------------------------------------------------------------
// DSP multiplies
// ---------------------------------------------------------------------------

#[test]
fn smulxy_multiplies_the_selected_signed_halfwords() {
    // smul<x><y> r0, r1, r2
    let smul = |x: u32, y: u32| 0xE160_0080 | (2 << 8) | (y << 6) | (x << 5) | 1;

    let (mut cpu, mut bus) = setup(&[smul(0, 0)]); // both low halves
    cpu.set_reg(1, 0x0000_0003);
    cpu.set_reg(2, 0x0000_0005);
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.reg(0), 15);

    let (mut cpu, mut bus) = setup(&[smul(1, 1)]); // both high halves
    cpu.set_reg(1, 0x0003_0000);
    cpu.set_reg(2, 0x0005_0000);
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.reg(0), 15);

    // Halfwords are signed, not zero-extended.
    let (mut cpu, mut bus) = setup(&[smul(0, 0)]);
    cpu.set_reg(1, 0x0000_FFFF);
    cpu.set_reg(2, 0x0000_0005);
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.reg(0), (-5i32) as u32);
}

#[test]
fn smlaxy_accumulates_and_saturates_into_q() {
    // smlabb r0, r1, r2, r3
    let smla = 0xE100_0080 | (3 << 12) | (2 << 8) | 1;

    let (mut cpu, mut bus) = setup(&[smla]);
    cpu.set_reg(1, 4);
    cpu.set_reg(2, 5);
    cpu.set_reg(3, 100);
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.reg(0), 120);
    assert!(!cpu.core.cpsr.sticky_overflow());

    // Unlike a plain MLA, which wraps silently, the accumulate saturates and sets Q.
    let (mut cpu, mut bus) = setup(&[smla]);
    cpu.set_reg(1, 2);
    cpu.set_reg(2, 2);
    cpu.set_reg(3, 0x7FFF_FFFF);
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.reg(0), 0x7FFF_FFFF);
    assert!(cpu.core.cpsr.sticky_overflow());
}

#[test]
fn smlalxy_wraps_into_a_register_pair_rather_than_saturating() {
    // smlalbb with rn = r0 (low) and rd = r1 (high).
    let smlal = 0xE140_0080 | (1 << 16) | (3 << 8) | 2;
    let (mut cpu, mut bus) = setup(&[smlal]);
    cpu.set_reg(0, 10);
    cpu.set_reg(1, 0);
    cpu.set_reg(2, 0x0000_0004);
    cpu.set_reg(3, 0x0000_0005);
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.reg(0), 30);
    assert_eq!(cpu.reg(1), 0);
    assert!(
        !cpu.core.cpsr.sticky_overflow(),
        "a 64-bit accumulate has nothing to saturate against"
    );
}

// ---------------------------------------------------------------------------
// LDRD / STRD
// ---------------------------------------------------------------------------

#[test]
fn ldrd_and_strd_move_a_register_pair() {
    let (mut cpu, mut bus) = setup(&[0xE1C0_20F0]); // strd r2, [r0]
    cpu.set_reg(0, 0x0200_4000);
    cpu.set_reg(2, 0xAAAA_AAAA);
    cpu.set_reg(3, 0xBBBB_BBBB);
    step(&mut cpu, &mut bus);
    assert_eq!(bus.word(0x0200_4000), 0xAAAA_AAAA);
    assert_eq!(bus.word(0x0200_4004), 0xBBBB_BBBB);

    let (mut cpu, mut bus) = setup(&[0xE1C0_40D0]); // ldrd r4, [r0]
    cpu.set_reg(0, 0x0200_4000);
    bus.load_words(0x0200_4000, &[0x1111_1111, 0x2222_2222]);
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.reg(4), 0x1111_1111);
    assert_eq!(cpu.reg(5), 0x2222_2222);
}

#[test]
fn an_odd_destination_register_is_rejected_rather_than_guessed_at() {
    // Register pairs must start on an even register; an odd one is architecturally
    // unpredictable, and trapping beats inventing an interpretation.
    let (mut cpu, mut bus) = setup(&[0xE1C0_50D0]); // ldrd r5, [r0]
    cpu.set_reg(0, 0x0200_4000);
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.core.mode(), Mode::Undefined);
}

#[test]
fn ldrd_does_not_shadow_the_armv4t_signed_loads() {
    // ldrsb has the same encoding shape but with the load bit set, and must still reach the
    // shared ARMv4T implementation.
    let (mut cpu, mut bus) = setup(&[0xE1D0_10D0]);
    cpu.set_reg(0, 0x0200_4000);
    bus.load_words(0x0200_4000, &[0x0000_00FF]);
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.reg(1), 0xFFFF_FFFF);
}

// ---------------------------------------------------------------------------
// CP15
// ---------------------------------------------------------------------------

fn mrc(rd: u32, crn: u32, crm: u32, op2: u32) -> u32 {
    0xEE10_0F10 | (crn << 16) | (rd << 12) | (op2 << 5) | crm
}

fn mcr(rd: u32, crn: u32, crm: u32, op2: u32) -> u32 {
    0xEE00_0F10 | (crn << 16) | (rd << 12) | (op2 << 5) | crm
}

#[test]
fn cp15_is_reachable_through_mrc_and_mcr() {
    let (mut cpu, mut bus) = setup(&[mrc(0, 0, 0, 0)]);
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.reg(0), MAIN_ID);

    let (mut cpu, mut bus) = setup(&[mcr(0, 1, 0, 0), mrc(1, 1, 0, 0)]);
    cpu.set_reg(0, control::ICACHE | control::MPU);
    step(&mut cpu, &mut bus);
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.reg(1), cpu.cp15.control());
    assert!(cpu.cp15.has(control::ICACHE));
}

#[test]
fn writing_the_high_vector_bit_relocates_the_exception_vectors() {
    let (mut cpu, mut bus) = setup(&[mcr(0, 1, 0, 0), 0xEF00_0000]); // mcr ; swi
    cpu.set_reg(0, control::HIGH_VECTORS);
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.core.exception_base(), 0xFFFF_0000);

    step(&mut cpu, &mut bus);
    assert_eq!(
        cpu.program_counter(),
        0xFFFF_0008,
        "the SWI vector moved with the base"
    );
}

#[test]
fn wait_for_interrupt_halts_the_core_until_a_line_is_asserted() {
    let (mut cpu, mut bus) = setup(&[mcr(0, 7, 0, 4), 0xE3A0_0001]);
    step(&mut cpu, &mut bus);
    assert!(cpu.is_halted());

    assert_eq!(step(&mut cpu, &mut bus), 1, "idling still costs time");
    assert_eq!(cpu.reg(0), 0);

    // The line wakes the core even with the CPSR mask set.
    cpu.core.cpsr.set_irq_disabled(true);
    cpu.set_irq_line(true);
    step(&mut cpu, &mut bus);
    assert!(!cpu.is_halted());
    assert_eq!(cpu.reg(0), 1);
}

#[test]
fn cache_maintenance_operations_execute_without_disturbing_anything() {
    // Invalidate the I-cache, then clean and clean-invalidate the D-cache. With no cache
    // storage these are no-ops, and memory must be untouched.
    let (mut cpu, mut bus) = setup(&[mcr(0, 7, 5, 0), mcr(0, 7, 10, 0), mcr(0, 7, 10, 4)]);
    bus.load_words(0x0200_4000, &[0x1234_5678]);
    for _ in 0..3 {
        step(&mut cpu, &mut bus);
    }
    assert_eq!(bus.word(0x0200_4000), 0x1234_5678);
    assert!(!cpu.is_halted(), "only c7,c0,4 halts");
}

// ---------------------------------------------------------------------------
// TCM
// ---------------------------------------------------------------------------

#[test]
fn an_enabled_tcm_intercepts_accesses_before_they_reach_the_bus() {
    let (mut cpu, mut bus) = setup(&[
        mcr(0, 9, 1, 1), // configure ITCM
        mcr(1, 1, 0, 0), // enable it
        0xE583_2000,     // str r2, [r3]
        0xE594_5000,     // ldr r5, [r4]
    ]);
    cpu.set_reg(0, 0x0000_000C); // 512 << 6 = 32 KiB at base 0
    cpu.set_reg(1, control::ITCM_ENABLE);
    cpu.set_reg(2, 0xFEED_FACE);
    cpu.set_reg(3, 0x0000_0100);
    cpu.set_reg(4, 0x0000_0100);

    step(&mut cpu, &mut bus);
    step(&mut cpu, &mut bus);
    assert!(cpu.itcm.is_enabled());

    step(&mut cpu, &mut bus);
    assert_eq!(
        bus.word(0x0000_0100),
        0,
        "the store must not have reached the bus"
    );
    assert_eq!(
        cpu.itcm.as_slice()[0x100..0x104],
        0xFEED_FACEu32.to_le_bytes()
    );

    step(&mut cpu, &mut bus);
    assert_eq!(cpu.reg(5), 0xFEED_FACE, "and reads come back from the TCM");
}

#[test]
fn a_disabled_tcm_is_transparent() {
    let (mut cpu, mut bus) = setup(&[0xE583_2000]); // str r2, [r3]
    cpu.itcm.configure(0x0000_000C, true); // configured but never enabled
    cpu.set_reg(2, 0xAAAA_AAAA);
    cpu.set_reg(3, 0x0000_0100);
    step(&mut cpu, &mut bus);
    assert_eq!(bus.word(0x0000_0100), 0xAAAA_AAAA);
}

#[test]
fn dtcm_sits_where_cp15_puts_it() {
    let (mut cpu, mut bus) = setup(&[0xE583_2000]);
    cpu.cp15.write(0, 9, 1, 0, 0x0300_000A); // 16 KiB at 0x03000000
    cpu.dtcm.configure(0x0300_000A, false);
    cpu.dtcm.set_enabled(true);

    cpu.set_reg(2, 0x1234_5678);
    cpu.set_reg(3, 0x0300_0004);
    step(&mut cpu, &mut bus);

    assert_eq!(bus.word(0x0300_0004), 0, "the bus never saw it");
    assert_eq!(cpu.dtcm.as_slice()[4..8], 0x1234_5678u32.to_le_bytes());
}

#[test]
fn post_boot_configuration_enables_both_tcms_and_high_vectors() {
    let mut cpu = Arm946e::new(boot());
    cpu.post_boot_nds();

    assert!(cpu.itcm.is_enabled());
    assert_eq!(cpu.itcm.base(), 0);
    assert_eq!(cpu.itcm.region_size() as usize, ITCM_SIZE);
    assert!(cpu.dtcm.is_enabled());
    assert_eq!(cpu.dtcm.base(), 0x027C_0000);
    assert_eq!(cpu.dtcm.region_size() as usize, DTCM_SIZE);
    assert_eq!(cpu.core.exception_base(), 0xFFFF_0000);
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

#[test]
fn save_state_round_trips_cp15_and_tcm_contents() {
    let (mut cpu, mut bus) = setup(&[0xE3A0_0001]);
    cpu.post_boot_nds();
    step(&mut cpu, &mut bus);
    cpu.itcm.write8(0x40, 0x5A);
    cpu.dtcm.write8(0x027C_0010, 0xA5);

    let mut w = StateWriter::new();
    cpu.save(&mut w);
    let blob = w.into_inner();

    let mut restored = Arm946e::default();
    restored.load(&mut StateReader::new(&blob)).unwrap();
    assert_eq!(restored, cpu);
    assert_eq!(restored.itcm.read8(0x40), 0x5A);
}

#[test]
fn reset_returns_cp15_and_the_tcms_to_power_on() {
    let mut cpu = Arm946e::new(boot());
    cpu.post_boot_nds();
    cpu.itcm.write8(0, 0xFF);

    Cpu::<TestBus>::reset(&mut cpu);
    assert!(!cpu.itcm.is_enabled());
    assert_eq!(cpu.core.exception_base(), 0, "back to low vectors");
    assert_eq!(cpu.itcm.read8(0), 0);
}

#[test]
fn introspection_reports_cp15_alongside_the_register_file() {
    let mut cpu = Arm946e::new(boot());
    cpu.post_boot_nds();
    cpu.core.cpsr.set_sticky_overflow(true);

    let regs = cpu.registers();
    assert!(regs.iter().any(|r| r.name == "r0"));
    assert!(regs.iter().any(|r| r.name == "cp15_ctl"));
    assert!(regs.iter().any(|r| r.name == "itcm"));
    assert!(
        cpu.flags_summary().ends_with('Q'),
        "the sticky overflow flag is visible: {}",
        cpu.flags_summary()
    );
}

/// A bus that records how wide each access was, to prove the TCM view forwards rather than
/// decomposing.
struct WidthBus {
    inner: TestBus,
    widths: Vec<(&'static str, u32)>,
}

impl WidthBus {
    fn new() -> Self {
        Self {
            inner: TestBus::new(),
            widths: Vec::new(),
        }
    }
}

impl Savable for WidthBus {
    fn save(&self, w: &mut StateWriter) {
        self.inner.save(w);
    }
    fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        self.inner.load(r)
    }
}

impl Bus for WidthBus {
    fn read8(&mut self, addr: u32) -> u8 {
        self.widths.push(("read8", addr));
        self.inner.read8(addr)
    }
    fn write8(&mut self, addr: u32, value: u8) {
        self.widths.push(("write8", addr));
        self.inner.write8(addr, value);
    }
    fn read16(&mut self, addr: u32) -> u16 {
        self.widths.push(("read16", addr));
        u16::from_le_bytes([self.inner.read8(addr), self.inner.read8(addr + 1)])
    }
    fn write16(&mut self, addr: u32, value: u16) {
        self.widths.push(("write16", addr));
        let b = value.to_le_bytes();
        self.inner.write8(addr, b[0]);
        self.inner.write8(addr + 1, b[1]);
    }
    fn read32(&mut self, addr: u32) -> u32 {
        self.widths.push(("read32", addr));
        u32::from_le_bytes(std::array::from_fn(|i| self.inner.read8(addr + i as u32)))
    }
    fn write32(&mut self, addr: u32, value: u32) {
        self.widths.push(("write32", addr));
        let b = value.to_le_bytes();
        for (i, byte) in b.iter().enumerate() {
            self.inner.write8(addr + i as u32, *byte);
        }
    }
    fn open_bus8(&self, _addr: u32) -> u8 {
        0
    }
    fn peek8(&self, addr: u32) -> Option<u8> {
        self.inner.peek8(addr)
    }
}

#[test]
fn a_wide_access_that_misses_tcm_stays_wide_on_the_way_out() {
    // The `Bus` defaults decompose a halfword or word into byte accesses. That is wrong for the
    // DS, where an ARM9 byte write to VRAM, palette RAM, or OAM is *dropped* by hardware and
    // several I/O registers exist only as words — so the TCM view has to forward the width it
    // was given. Getting this wrong made the ARM9 unable to write to VRAM at all, which
    // presented as a black screen with every register set correctly.
    let mut cpu = Arm946e::new(boot());
    cpu.post_boot_nds();
    let mut bus = WidthBus::new();

    // 0x06000000 is VRAM on a real DS: outside both TCM regions.
    let mut view = TcmBusProbe(&mut cpu, &mut bus);
    view.write16(0x0600_0000, 0x1234);
    view.write32(0x0600_0004, 0x89AB_CDEF);
    view.read32(0x0600_0004);

    let widths: Vec<&str> = bus.widths.iter().map(|(w, _)| *w).collect();
    assert_eq!(widths, ["write16", "write32", "read32"]);
}

/// Drives `TcmBus` from outside the crate's private plumbing.
struct TcmBusProbe<'a>(&'a mut Arm946e, &'a mut WidthBus);

impl TcmBusProbe<'_> {
    fn write16(&mut self, addr: u32, value: u16) {
        let bus = &mut *self.1;
        self.0
            .with_tcm_bus(bus, |_, view| view.write16(addr, value));
    }
    fn write32(&mut self, addr: u32, value: u32) {
        let bus = &mut *self.1;
        self.0
            .with_tcm_bus(bus, |_, view| view.write32(addr, value));
    }
    fn read32(&mut self, addr: u32) -> u32 {
        let bus = &mut *self.1;
        self.0.with_tcm_bus(bus, |_, view| view.read32(addr))
    }
}

#[test]
fn a_wide_access_inside_tcm_is_served_by_tcm_and_never_reaches_the_bus() {
    let mut cpu = Arm946e::new(boot());
    cpu.post_boot_nds();
    let mut bus = WidthBus::new();

    // ITCM starts at zero after the post-boot configuration.
    let mut view = TcmBusProbe(&mut cpu, &mut bus);
    view.write32(0x0000_0100, 0xDEAD_BEEF);
    assert_eq!(view.read32(0x0000_0100), 0xDEAD_BEEF);
    assert!(bus.widths.is_empty(), "{:?}", bus.widths);
}
