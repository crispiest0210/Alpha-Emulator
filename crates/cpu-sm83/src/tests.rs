//! Unit tests for the SM83 core.
//!
//! These cover flag semantics, per-addressing-mode cycle counts, and the two accuracy traps
//! this core exists to get right — the `EI` delay and the `HALT` bug. They are not a
//! substitute for the accuracy ROM suite (prompt 17), which is what actually gates this
//! crate; they are what catches a regression in seconds instead of minutes.

use crate::*;
use core_common::{Bus, Cpu, CpuIntrospect, Savable, StateReader, StateWriter};

/// Flat 64 KiB of RAM. `IF` and `IE` land in it naturally at their real addresses, so the CPU
/// reads them through the bus exactly as it would on hardware.
struct TestBus {
    mem: Box<[u8; 0x1_0000]>,
}

impl TestBus {
    fn new() -> Self {
        Self {
            mem: Box::new([0; 0x1_0000]),
        }
    }

    fn load(&mut self, addr: u16, bytes: &[u8]) {
        self.mem[addr as usize..addr as usize + bytes.len()].copy_from_slice(bytes);
    }
}

impl Bus for TestBus {
    fn read8(&mut self, addr: u32) -> u8 {
        self.mem[(addr & 0xFFFF) as usize]
    }
    fn write8(&mut self, addr: u32, value: u8) {
        self.mem[(addr & 0xFFFF) as usize] = value;
    }
    fn open_bus8(&self, _addr: u32) -> u8 {
        0xFF
    }
    fn peek8(&self, addr: u32) -> Option<u8> {
        Some(self.mem[(addr & 0xFFFF) as usize])
    }
}

impl Savable for TestBus {
    fn save(&self, _w: &mut StateWriter) {}
    fn load(&mut self, _r: &mut StateReader) -> Result<(), core_common::StateError> {
        Ok(())
    }
}

const ORG: u16 = 0x0100;

/// Build a CPU and bus with `program` at `ORG` and PC pointing at it.
fn setup(program: &[u8]) -> (Sm83, TestBus) {
    let mut cpu = Sm83::new();
    let mut bus = TestBus::new();
    bus.load(ORG, program);
    cpu.pc = ORG;
    cpu.sp = 0xFFFE;
    (cpu, bus)
}

/// Execute one instruction, returning its t-cycle cost.
fn step(cpu: &mut Sm83, bus: &mut TestBus) -> u32 {
    cpu.step(bus).get() as u32
}

/// Run `program` for one instruction and return the resulting state and cycle count.
fn run1(program: &[u8]) -> (Sm83, TestBus, u32) {
    let (mut cpu, mut bus) = setup(program);
    let cycles = step(&mut cpu, &mut bus);
    (cpu, bus, cycles)
}

// ---------------------------------------------------------------------------
// Timing
// ---------------------------------------------------------------------------

#[test]
fn cycle_counts_match_the_table_for_each_addressing_mode() {
    // (program, expected t-cycles) — one representative per timing class.
    let cases: &[(&[u8], u32)] = &[
        (&[0x00], 4),              // NOP
        (&[0x3E, 0x12], 8),        // LD A,d8
        (&[0x21, 0x34, 0x12], 12), // LD HL,d16
        (&[0x08, 0x00, 0xC0], 20), // LD (a16),SP
        (&[0x47], 4),              // LD B,A
        (&[0x7E], 8),              // LD A,(HL)
        (&[0x70], 8),              // LD (HL),B
        (&[0x34], 12),             // INC (HL)
        (&[0x36, 0x55], 12),       // LD (HL),d8
        (&[0x80], 4),              // ADD A,B
        (&[0x86], 8),              // ADD A,(HL)
        (&[0xC6, 0x01], 8),        // ADD A,d8
        (&[0xC5], 16),             // PUSH BC
        (&[0xC1], 12),             // POP BC
        (&[0xC3, 0x00, 0x02], 16), // JP a16
        (&[0xE9], 4),              // JP HL
        (&[0xCD, 0x00, 0x02], 24), // CALL a16
        (&[0xC9], 16),             // RET
        (&[0xD9], 16),             // RETI
        (&[0xC7], 16),             // RST 00
        (&[0xE0, 0x80], 12),       // LDH (a8),A
        (&[0xEA, 0x00, 0xC0], 16), // LD (a16),A
        (&[0xE8, 0x01], 16),       // ADD SP,r8
        (&[0xF8, 0x01], 12),       // LD HL,SP+r8
        (&[0xF9], 8),              // LD SP,HL
        (&[0x18, 0x00], 12),       // JR r8
    ];

    for (program, expected) in cases {
        let (_, _, cycles) = run1(program);
        assert_eq!(
            cycles, *expected,
            "opcode {:#04X} should take {expected} cycles, took {cycles}",
            program[0]
        );
    }
}

#[test]
fn conditional_branches_cost_more_when_taken() {
    // JR NZ: 8 not taken, 12 taken.
    let (mut cpu, mut bus) = setup(&[0x20, 0x02]);
    cpu.set_flag(FLAG_Z, true);
    assert_eq!(step(&mut cpu, &mut bus), 8);

    let (mut cpu, mut bus) = setup(&[0x20, 0x02]);
    cpu.set_flag(FLAG_Z, false);
    assert_eq!(step(&mut cpu, &mut bus), 12);
    assert_eq!(cpu.pc, ORG + 2 + 2);

    // JP NZ: 12 / 16.
    let (mut cpu, mut bus) = setup(&[0xC2, 0x00, 0x02]);
    cpu.set_flag(FLAG_Z, true);
    assert_eq!(step(&mut cpu, &mut bus), 12);

    let (mut cpu, mut bus) = setup(&[0xC2, 0x00, 0x02]);
    assert_eq!(step(&mut cpu, &mut bus), 16);
    assert_eq!(cpu.pc, 0x0200);

    // CALL NZ: 12 / 24.
    let (mut cpu, mut bus) = setup(&[0xC4, 0x00, 0x02]);
    cpu.set_flag(FLAG_Z, true);
    assert_eq!(step(&mut cpu, &mut bus), 12);

    let (mut cpu, mut bus) = setup(&[0xC4, 0x00, 0x02]);
    assert_eq!(step(&mut cpu, &mut bus), 24);
    assert_eq!(cpu.pc, 0x0200);

    // RET NZ: 8 / 20.
    let (mut cpu, mut bus) = setup(&[0xC0]);
    cpu.set_flag(FLAG_Z, true);
    assert_eq!(step(&mut cpu, &mut bus), 8);

    let (mut cpu, mut bus) = setup(&[0xC0]);
    bus.load(0xFFFE, &[0x34, 0x12]);
    assert_eq!(step(&mut cpu, &mut bus), 20);
    assert_eq!(cpu.pc, 0x1234);
}

#[test]
fn cb_prefixed_timings_distinguish_register_read_and_read_modify_write() {
    let (_, _, c) = run1(&[0xCB, 0x00]); // RLC B
    assert_eq!(c, 8);
    let (_, _, c) = run1(&[0xCB, 0x06]); // RLC (HL) — read + write
    assert_eq!(c, 16);
    let (_, _, c) = run1(&[0xCB, 0x46]); // BIT 0,(HL) — read only
    assert_eq!(c, 12);
    let (_, _, c) = run1(&[0xCB, 0x86]); // RES 0,(HL)
    assert_eq!(c, 16);
}

#[test]
fn every_cb_opcode_costs_what_the_formula_says() {
    for cb in 0u16..=0xFF {
        let cb = cb as u8;
        let (_, _, cycles) = run1(&[0xCB, cb]);
        assert_eq!(cycles, cb_cycles(cb), "CB {cb:#04X}");
    }
}

#[test]
fn every_opcode_executes_and_consumes_time() {
    // Guards against a missing match arm or a zero-cycle instruction, either of which would
    // hang the frame loop rather than fail visibly.
    for op in 0u16..=0xFF {
        let op = op as u8;
        let (mut cpu, mut bus) = setup(&[op, 0x00, 0x00, 0x00]);
        let cycles = step(&mut cpu, &mut bus);
        assert!(cycles >= 4, "opcode {op:#04X} returned {cycles} cycles");

        // The cycle table's zero entries and the set of opcodes that lock the CPU must be
        // exactly the same set.
        assert_eq!(
            CYCLES[op as usize] == 0,
            cpu.is_locked(),
            "opcode {op:#04X}: cycle table and lockup behavior disagree"
        );
    }
}

// ---------------------------------------------------------------------------
// ALU flags
// ---------------------------------------------------------------------------

/// Assert the flag register, written as `"ZNHC"` with lowercase meaning clear.
fn assert_flags(cpu: &Sm83, expected: &str) {
    assert_eq!(cpu.flags_summary(), expected, "flags");
}

#[test]
fn add_sets_zero_half_and_carry_correctly() {
    // 0x0F + 0x01 = 0x10: half-carry out of bit 3, no carry.
    let (mut cpu, mut bus) = setup(&[0xC6, 0x01]);
    cpu.a = 0x0F;
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.a, 0x10);
    assert_flags(&cpu, "znHc");

    // 0xFF + 0x01 = 0x00: half-carry, carry, and zero all at once.
    let (mut cpu, mut bus) = setup(&[0xC6, 0x01]);
    cpu.a = 0xFF;
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.a, 0x00);
    assert_flags(&cpu, "ZnHC");

    // 0x80 + 0x80 = 0x00: carry without half-carry.
    let (mut cpu, mut bus) = setup(&[0xC6, 0x80]);
    cpu.a = 0x80;
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.a, 0x00);
    assert_flags(&cpu, "ZnhC");
}

#[test]
fn adc_includes_the_incoming_carry_in_both_result_and_flags() {
    let (mut cpu, mut bus) = setup(&[0xCE, 0x00]); // ADC A,0
    cpu.a = 0x0F;
    cpu.set_flag(FLAG_C, true);
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.a, 0x10);
    // The carry-in alone produces the half-carry.
    assert_flags(&cpu, "znHc");

    let (mut cpu, mut bus) = setup(&[0xCE, 0xFF]);
    cpu.a = 0x00;
    cpu.set_flag(FLAG_C, true);
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.a, 0x00);
    assert_flags(&cpu, "ZnHC");
}

#[test]
fn sub_and_sbc_set_the_subtract_flag_and_borrow_semantics() {
    // 0x10 - 0x01: borrow out of bit 4 into the low nibble.
    let (mut cpu, mut bus) = setup(&[0xD6, 0x01]);
    cpu.a = 0x10;
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.a, 0x0F);
    assert_flags(&cpu, "zNHc");

    // Equal operands: zero, no borrow.
    let (mut cpu, mut bus) = setup(&[0xD6, 0x20]);
    cpu.a = 0x20;
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.a, 0x00);
    assert_flags(&cpu, "ZNhc");

    // Underflow sets carry.
    let (mut cpu, mut bus) = setup(&[0xD6, 0x01]);
    cpu.a = 0x00;
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.a, 0xFF);
    assert_flags(&cpu, "zNHC");

    // SBC with carry-in borrows one more.
    let (mut cpu, mut bus) = setup(&[0xDE, 0x00]);
    cpu.a = 0x00;
    cpu.set_flag(FLAG_C, true);
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.a, 0xFF);
    assert_flags(&cpu, "zNHC");
}

#[test]
fn cp_sets_flags_without_touching_the_accumulator() {
    let (mut cpu, mut bus) = setup(&[0xFE, 0x20]);
    cpu.a = 0x20;
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.a, 0x20, "CP must not modify A");
    assert_flags(&cpu, "ZNhc");

    // 0x20 - 0x30 borrows out of bit 7 but not bit 3: both low nibbles are zero.
    let (mut cpu, mut bus) = setup(&[0xFE, 0x30]);
    cpu.a = 0x20;
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.a, 0x20);
    assert_flags(&cpu, "zNhC");
}

#[test]
fn logical_ops_differ_only_in_the_half_carry_flag() {
    // AND is the odd one out: it sets H.
    let (mut cpu, mut bus) = setup(&[0xE6, 0x0F]);
    cpu.a = 0xF0;
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.a, 0x00);
    assert_flags(&cpu, "ZnHc");

    let (mut cpu, mut bus) = setup(&[0xE6, 0x3C]);
    cpu.a = 0xFF;
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.a, 0x3C);
    assert_flags(&cpu, "znHc");

    // OR and XOR clear everything but Z.
    let (mut cpu, mut bus) = setup(&[0xF6, 0x0F]);
    cpu.a = 0xF0;
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.a, 0xFF);
    assert_flags(&cpu, "znhc");

    let (mut cpu, mut bus) = setup(&[0xEE, 0xFF]);
    cpu.a = 0xFF;
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.a, 0x00);
    assert_flags(&cpu, "Znhc");
}

#[test]
fn inc_and_dec_leave_the_carry_flag_alone() {
    // This is what makes multi-byte increments work, so it is load-bearing, not a detail.
    let (mut cpu, mut bus) = setup(&[0x3C]); // INC A
    cpu.a = 0x0F;
    cpu.set_flag(FLAG_C, true);
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.a, 0x10);
    assert_flags(&cpu, "znHC");

    let (mut cpu, mut bus) = setup(&[0x3C]);
    cpu.a = 0xFF;
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.a, 0x00);
    assert_flags(&cpu, "ZnHc");

    let (mut cpu, mut bus) = setup(&[0x3D]); // DEC A
    cpu.a = 0x10;
    cpu.set_flag(FLAG_C, true);
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.a, 0x0F);
    assert_flags(&cpu, "zNHC");

    let (mut cpu, mut bus) = setup(&[0x3D]);
    cpu.a = 0x01;
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.a, 0x00);
    assert_flags(&cpu, "ZNhc");
}

#[test]
fn daa_corrects_bcd_after_addition_and_subtraction() {
    // 0x45 + 0x38 = 0x7D, which DAA fixes up to 0x83 (45 + 38 = 83 in BCD).
    let (mut cpu, mut bus) = setup(&[0xC6, 0x38, 0x27]);
    cpu.a = 0x45;
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.a, 0x7D);
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.a, 0x83);
    assert!(!cpu.flag(FLAG_C));
    assert!(!cpu.flag(FLAG_H), "DAA always clears H");

    // 0x83 - 0x38 = 0x4B, corrected to 0x45 (83 - 38 = 45 in BCD).
    let (mut cpu, mut bus) = setup(&[0xD6, 0x38, 0x27]);
    cpu.a = 0x83;
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.a, 0x4B);
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.a, 0x45);
    assert!(!cpu.flag(FLAG_C));

    // Carrying out of the high nibble sets C, which stays set for the next BCD digit.
    let (mut cpu, mut bus) = setup(&[0xC6, 0x10, 0x27]);
    cpu.a = 0x90;
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.a, 0xA0);
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.a, 0x00);
    assert!(cpu.flag(FLAG_C));
    assert!(cpu.flag(FLAG_Z));
}

#[test]
fn cpl_scf_and_ccf_touch_only_their_own_flags() {
    let (mut cpu, mut bus) = setup(&[0x2F]); // CPL
    cpu.a = 0b1010_0101;
    cpu.set_flag(FLAG_Z, true);
    cpu.set_flag(FLAG_C, true);
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.a, 0b0101_1010);
    assert_flags(&cpu, "ZNHC");

    let (mut cpu, mut bus) = setup(&[0x37]); // SCF
    cpu.set_flag(FLAG_Z, true);
    cpu.set_flag(FLAG_N, true);
    cpu.set_flag(FLAG_H, true);
    step(&mut cpu, &mut bus);
    assert_flags(&cpu, "ZnhC");

    let (mut cpu, mut bus) = setup(&[0x3F]); // CCF
    cpu.set_flag(FLAG_C, true);
    step(&mut cpu, &mut bus);
    assert_flags(&cpu, "znhc");
    let (mut cpu, mut bus) = setup(&[0x3F]);
    step(&mut cpu, &mut bus);
    assert_flags(&cpu, "znhC");
}

// ---------------------------------------------------------------------------
// Rotates and shifts
// ---------------------------------------------------------------------------

#[test]
fn accumulator_rotates_always_clear_zero_unlike_their_cb_forms() {
    // RLCA on a zero accumulator: result is zero, but Z must be *clear*.
    let (mut cpu, mut bus) = setup(&[0x07]);
    cpu.a = 0x00;
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.a, 0x00);
    assert_flags(&cpu, "znhc");

    // CB RLC A on the same input sets Z. This difference is the whole point.
    let (mut cpu, mut bus) = setup(&[0xCB, 0x07]);
    cpu.a = 0x00;
    step(&mut cpu, &mut bus);
    assert_flags(&cpu, "Znhc");
}

#[test]
fn rotates_move_the_expected_bit_through_carry() {
    let (mut cpu, mut bus) = setup(&[0x07]); // RLCA
    cpu.a = 0b1000_0001;
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.a, 0b0000_0011);
    assert!(cpu.flag(FLAG_C));

    let (mut cpu, mut bus) = setup(&[0x0F]); // RRCA
    cpu.a = 0b1000_0001;
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.a, 0b1100_0000);
    assert!(cpu.flag(FLAG_C));

    // RLA rotates *through* carry rather than around the register.
    let (mut cpu, mut bus) = setup(&[0x17]);
    cpu.a = 0b1000_0000;
    cpu.set_flag(FLAG_C, true);
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.a, 0b0000_0001);
    assert!(cpu.flag(FLAG_C));

    let (mut cpu, mut bus) = setup(&[0x1F]); // RRA
    cpu.a = 0b0000_0001;
    cpu.set_flag(FLAG_C, true);
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.a, 0b1000_0000);
    assert!(cpu.flag(FLAG_C));
}

#[test]
fn shifts_differ_in_how_they_treat_the_sign_bit() {
    let (mut cpu, mut bus) = setup(&[0xCB, 0x27]); // SLA A
    cpu.a = 0b1000_0001;
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.a, 0b0000_0010);
    assert!(cpu.flag(FLAG_C));

    // SRA replicates bit 7.
    let (mut cpu, mut bus) = setup(&[0xCB, 0x2F]);
    cpu.a = 0b1000_0001;
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.a, 0b1100_0000);
    assert!(cpu.flag(FLAG_C));

    // SRL does not.
    let (mut cpu, mut bus) = setup(&[0xCB, 0x3F]);
    cpu.a = 0b1000_0001;
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.a, 0b0100_0000);
    assert!(cpu.flag(FLAG_C));

    let (mut cpu, mut bus) = setup(&[0xCB, 0x37]); // SWAP A
    cpu.a = 0xAB;
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.a, 0xBA);
    assert_flags(&cpu, "znhc");
}

#[test]
fn bit_tests_leave_carry_untouched_while_res_and_set_touch_no_flags() {
    let (mut cpu, mut bus) = setup(&[0xCB, 0x7F]); // BIT 7,A
    cpu.a = 0x00;
    cpu.set_flag(FLAG_C, true);
    step(&mut cpu, &mut bus);
    assert_flags(&cpu, "ZnHC");

    let (mut cpu, mut bus) = setup(&[0xCB, 0x7F]);
    cpu.a = 0x80;
    step(&mut cpu, &mut bus);
    assert_flags(&cpu, "znHc");

    let (mut cpu, mut bus) = setup(&[0xCB, 0xBF]); // RES 7,A
    cpu.a = 0xFF;
    cpu.f = FLAG_Z | FLAG_N | FLAG_H | FLAG_C;
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.a, 0x7F);
    assert_flags(&cpu, "ZNHC");

    let (mut cpu, mut bus) = setup(&[0xCB, 0xFF]); // SET 7,A
    cpu.a = 0x00;
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.a, 0x80);
}

// ---------------------------------------------------------------------------
// 16-bit arithmetic
// ---------------------------------------------------------------------------

#[test]
fn add_hl_carries_out_of_bit_11_and_leaves_zero_alone() {
    let (mut cpu, mut bus) = setup(&[0x09]); // ADD HL,BC
    cpu.set_hl(0x0FFF);
    cpu.set_bc(0x0001);
    cpu.set_flag(FLAG_Z, true);
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.hl(), 0x1000);
    assert_flags(&cpu, "ZnHc");

    let (mut cpu, mut bus) = setup(&[0x09]);
    cpu.set_hl(0xFFFF);
    cpu.set_bc(0x0001);
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.hl(), 0x0000);
    // Zero result, but ADD HL never sets Z.
    assert_flags(&cpu, "znHC");
}

#[test]
fn stack_pointer_offsets_compute_flags_from_the_low_byte_only() {
    // The offset is signed, but H and C come from unsigned addition of the bottom 8 bits.
    let (mut cpu, mut bus) = setup(&[0xE8, 0x01]); // ADD SP,+1
    cpu.sp = 0x000F;
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.sp, 0x0010);
    assert_flags(&cpu, "znHc");

    let (mut cpu, mut bus) = setup(&[0xE8, 0x01]);
    cpu.sp = 0x00FF;
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.sp, 0x0100);
    assert_flags(&cpu, "znHC");

    // A negative offset that borrows across the byte boundary still reports no carry,
    // because 0x00 + 0xFF does not exceed 0xFF.
    let (mut cpu, mut bus) = setup(&[0xE8, 0xFF]); // ADD SP,-1
    cpu.sp = 0x0000;
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.sp, 0xFFFF);
    assert_flags(&cpu, "znhc");

    // LD HL,SP+r8 shares the arithmetic but writes HL and leaves SP alone.
    let (mut cpu, mut bus) = setup(&[0xF8, 0x02]);
    cpu.sp = 0xFFFE;
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.hl(), 0x0000);
    assert_eq!(cpu.sp, 0xFFFE);
    assert_flags(&cpu, "znHC");
}

// ---------------------------------------------------------------------------
// Loads, stack, control flow
// ---------------------------------------------------------------------------

#[test]
fn pop_af_drops_the_nonexistent_low_nibble_of_f() {
    let (mut cpu, mut bus) = setup(&[0xF1]);
    bus.load(0xFFFE, &[0xFF, 0x12]);
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.a, 0x12);
    assert_eq!(
        cpu.f, 0xF0,
        "the low nibble of F does not exist in hardware"
    );
    assert_eq!(cpu.af(), 0x12F0);
}

#[test]
fn hl_auto_increment_and_decrement_loads() {
    let (mut cpu, mut bus) = setup(&[0x2A]); // LD A,(HL+)
    cpu.set_hl(0xC000);
    bus.load(0xC000, &[0x77]);
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.a, 0x77);
    assert_eq!(cpu.hl(), 0xC001);

    let (mut cpu, mut bus) = setup(&[0x32]); // LD (HL-),A
    cpu.a = 0x99;
    cpu.set_hl(0xC000);
    step(&mut cpu, &mut bus);
    assert_eq!(bus.read8(0xC000), 0x99);
    assert_eq!(cpu.hl(), 0xBFFF);
}

#[test]
fn call_pushes_the_return_address_and_ret_pops_it() {
    let (mut cpu, mut bus) = setup(&[0xCD, 0x00, 0x20]);
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.pc, 0x2000);
    assert_eq!(cpu.sp, 0xFFFC);
    // Return address is the instruction *after* the CALL.
    assert_eq!(bus.read8(0xFFFC), (ORG + 3) as u8);
    assert_eq!(bus.read8(0xFFFD), ((ORG + 3) >> 8) as u8);

    bus.load(0x2000, &[0xC9]); // RET
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.pc, ORG + 3);
    assert_eq!(cpu.sp, 0xFFFE);
}

#[test]
fn rst_vectors_to_its_encoded_address() {
    for (op, vector) in [
        (0xC7u8, 0x00u16),
        (0xCF, 0x08),
        (0xD7, 0x10),
        (0xDF, 0x18),
        (0xE7, 0x20),
        (0xEF, 0x28),
        (0xF7, 0x30),
        (0xFF, 0x38),
    ] {
        let (mut cpu, mut bus) = setup(&[op]);
        step(&mut cpu, &mut bus);
        assert_eq!(cpu.pc, vector, "RST {op:#04X}");
    }
}

#[test]
fn high_page_loads_address_the_io_range() {
    let (mut cpu, mut bus) = setup(&[0xE0, 0x42]); // LDH ($42),A
    cpu.a = 0x5A;
    step(&mut cpu, &mut bus);
    assert_eq!(bus.read8(0xFF42), 0x5A);

    let (mut cpu, mut bus) = setup(&[0xF2]); // LD A,(C)
    cpu.c = 0x44;
    bus.load(0xFF44, &[0x90]);
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.a, 0x90);
}

// ---------------------------------------------------------------------------
// Interrupts
// ---------------------------------------------------------------------------

/// Arm an interrupt source by setting both its request and enable bits.
fn arm_interrupt(bus: &mut TestBus, mask: u8) {
    bus.write8(IF_ADDR, mask);
    bus.write8(IE_ADDR, mask);
}

#[test]
fn interrupt_dispatch_pushes_pc_clears_the_request_and_takes_20_cycles() {
    let (mut cpu, mut bus) = setup(&[0x00]);
    cpu.ime = true;
    arm_interrupt(&mut bus, 0x01); // VBlank

    let cycles = step(&mut cpu, &mut bus);
    assert_eq!(cycles, 20);
    assert_eq!(cpu.pc, 0x0040);
    assert_eq!(cpu.sp, 0xFFFC);
    assert_eq!(bus.read8(0xFFFC), ORG as u8);
    assert_eq!(bus.read8(0xFFFD), (ORG >> 8) as u8);
    assert!(!cpu.ime(), "dispatch must disable further interrupts");
    assert_eq!(bus.read8(IF_ADDR), 0x00, "the serviced request is cleared");
}

#[test]
fn interrupt_priority_runs_lowest_bit_first() {
    let vectors = [0x0040u16, 0x0048, 0x0050, 0x0058, 0x0060];
    for (bit, vector) in vectors.iter().enumerate() {
        let (mut cpu, mut bus) = setup(&[0x00]);
        cpu.ime = true;
        // Enable everything, request only this source plus all higher-numbered ones.
        bus.write8(IE_ADDR, 0x1F);
        bus.write8(IF_ADDR, 0x1F & !((1 << bit) - 1));
        step(&mut cpu, &mut bus);
        assert_eq!(cpu.pc, *vector, "interrupt bit {bit}");
    }
}

#[test]
fn only_the_serviced_request_bit_is_cleared() {
    let (mut cpu, mut bus) = setup(&[0x00]);
    cpu.ime = true;
    bus.write8(IE_ADDR, 0x1F);
    bus.write8(IF_ADDR, 0x05); // VBlank and Timer both pending
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.pc, 0x0040);
    assert_eq!(bus.read8(IF_ADDR), 0x04, "the Timer request must survive");
}

#[test]
fn a_disabled_or_unrequested_interrupt_is_not_serviced() {
    // Requested but not enabled.
    let (mut cpu, mut bus) = setup(&[0x00]);
    cpu.ime = true;
    bus.write8(IF_ADDR, 0x01);
    bus.write8(IE_ADDR, 0x00);
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.pc, ORG + 1);

    // Enabled and requested, but IME is clear.
    let (mut cpu, mut bus) = setup(&[0x00]);
    arm_interrupt(&mut bus, 0x01);
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.pc, ORG + 1);
}

// ---------------------------------------------------------------------------
// The EI delay
// ---------------------------------------------------------------------------

#[test]
fn ei_delays_dispatch_by_exactly_one_instruction() {
    // EI ; NOP ; NOP  — the interrupt fires before the second NOP, not the first.
    let (mut cpu, mut bus) = setup(&[0xFB, 0x00, 0x00]);
    arm_interrupt(&mut bus, 0x01);

    step(&mut cpu, &mut bus); // EI
    assert!(
        cpu.ime(),
        "EI sets IME immediately; only dispatch is delayed"
    );
    assert_eq!(cpu.pc, ORG + 1);

    step(&mut cpu, &mut bus); // NOP — still shielded
    assert_eq!(cpu.pc, ORG + 2);

    let cycles = step(&mut cpu, &mut bus); // now the interrupt is taken
    assert_eq!(cycles, 20);
    assert_eq!(cpu.pc, 0x0040);
}

#[test]
fn ei_immediately_followed_by_di_services_nothing() {
    // The documented reason the delay exists: `EI; DI` must not let an interrupt slip in.
    let (mut cpu, mut bus) = setup(&[0xFB, 0xF3, 0x00]);
    arm_interrupt(&mut bus, 0x01);

    step(&mut cpu, &mut bus); // EI
    step(&mut cpu, &mut bus); // DI — runs before any dispatch, and cancels the pending enable
    assert!(!cpu.ime());
    assert_eq!(cpu.pc, ORG + 2);

    step(&mut cpu, &mut bus); // NOP, still no interrupt
    assert_eq!(cpu.pc, ORG + 3);
    assert_eq!(bus.read8(IF_ADDR), 0x01, "the request is still pending");
}

#[test]
fn reti_enables_interrupts_without_the_delay() {
    let (mut cpu, mut bus) = setup(&[0xD9]);
    // Keep the stack clear of 0xFFFF, which is the IE register.
    cpu.sp = 0xFFF0;
    bus.load(0xFFF0, &[0x00, 0x02]);
    arm_interrupt(&mut bus, 0x01);

    step(&mut cpu, &mut bus); // RETI
    assert!(cpu.ime());
    assert_eq!(cpu.pc, 0x0200);

    // Unlike EI, the very next step dispatches.
    let cycles = step(&mut cpu, &mut bus);
    assert_eq!(cycles, 20);
    assert_eq!(cpu.pc, 0x0040);
}

// ---------------------------------------------------------------------------
// HALT
// ---------------------------------------------------------------------------

#[test]
fn halt_with_ime_set_sleeps_until_an_interrupt_then_services_it() {
    let (mut cpu, mut bus) = setup(&[0x76, 0x00]);
    cpu.ime = true;
    bus.write8(IE_ADDR, 0x01);

    step(&mut cpu, &mut bus); // HALT
    assert!(cpu.is_halted());

    // Idling burns time rather than spinning at zero cycles.
    assert_eq!(step(&mut cpu, &mut bus), 4);
    assert!(cpu.is_halted());

    bus.write8(IF_ADDR, 0x01);
    let cycles = step(&mut cpu, &mut bus);
    assert_eq!(cycles, 20);
    assert!(!cpu.is_halted());
    assert_eq!(cpu.pc, 0x0040);
}

#[test]
fn halt_with_ime_clear_wakes_without_servicing() {
    let (mut cpu, mut bus) = setup(&[0x76, 0x3C]); // HALT ; INC A
    bus.write8(IE_ADDR, 0x01);
    bus.write8(IF_ADDR, 0x00); // nothing pending yet, so this is a normal halt

    step(&mut cpu, &mut bus);
    assert!(cpu.is_halted());

    bus.write8(IF_ADDR, 0x01);
    // Waking is free: the CPU resumes and runs the instruction after HALT in the same step,
    // servicing nothing because IME is clear.
    step(&mut cpu, &mut bus);
    assert!(!cpu.is_halted());
    assert_eq!(cpu.a, 1, "INC A ran");
    assert_eq!(cpu.pc, ORG + 2);
    assert_eq!(bus.read8(IF_ADDR), 0x01, "the request stays pending");
}

#[test]
fn the_halt_bug_reads_the_following_byte_twice() {
    // IME clear with an interrupt already pending: HALT does not halt, and the next opcode
    // fetch fails to advance PC.
    let (mut cpu, mut bus) = setup(&[0x76, 0x3C, 0x00]); // HALT ; INC A ; NOP
    arm_interrupt(&mut bus, 0x01);
    assert!(!cpu.ime());

    step(&mut cpu, &mut bus); // HALT — arms the bug instead of halting
    assert!(!cpu.is_halted());
    assert_eq!(cpu.pc, ORG + 1);

    step(&mut cpu, &mut bus); // INC A, PC does not advance
    assert_eq!(cpu.a, 1);
    assert_eq!(cpu.pc, ORG + 1, "PC must not advance on the bugged fetch");

    step(&mut cpu, &mut bus); // the same byte is fetched again
    assert_eq!(cpu.a, 2, "the byte after HALT executes twice");
    assert_eq!(cpu.pc, ORG + 2);

    step(&mut cpu, &mut bus); // and now execution continues normally
    assert_eq!(cpu.a, 2);
    assert_eq!(cpu.pc, ORG + 3);
}

#[test]
fn the_halt_bug_corrupts_a_following_two_byte_instruction() {
    // `LD A,$42` after a bugged HALT decodes as `LD A,$3E` — the opcode byte is re-read as
    // its own operand. This is the observable consequence emulators most often miss.
    let (mut cpu, mut bus) = setup(&[0x76, 0x3E, 0x42]);
    arm_interrupt(&mut bus, 0x01);

    step(&mut cpu, &mut bus); // HALT
    step(&mut cpu, &mut bus); // LD A,d8 — reads its opcode byte as the immediate
    assert_eq!(cpu.a, 0x3E);
}

#[test]
fn ei_before_halt_does_not_trigger_the_halt_bug() {
    // Because EI sets IME immediately (only dispatch is delayed), HALT sees IME set and takes
    // the normal path. This is the case that distinguishes the two plausible EI models.
    let (mut cpu, mut bus) = setup(&[0xFB, 0x76, 0x3C]);
    arm_interrupt(&mut bus, 0x01);

    step(&mut cpu, &mut bus); // EI
    step(&mut cpu, &mut bus); // HALT — normal halt, bug not armed
    assert!(cpu.is_halted());

    let cycles = step(&mut cpu, &mut bus);
    assert_eq!(cycles, 20, "the interrupt is serviced on wake");
    assert_eq!(cpu.pc, 0x0040);
}

// ---------------------------------------------------------------------------
// STOP and lockup
// ---------------------------------------------------------------------------

#[test]
fn stop_consumes_its_second_byte_and_is_visible_to_the_system() {
    let (mut cpu, mut bus) = setup(&[0x10, 0x00, 0x3C]);
    step(&mut cpu, &mut bus);
    assert!(cpu.is_stopped());
    assert_eq!(cpu.pc, ORG + 2, "STOP is a two-byte instruction");

    // The system decides what STOP means (on CGB it is a speed switch), so it clears the flag.
    cpu.clear_stop();
    assert!(!cpu.is_stopped());
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.a, 1);
}

#[test]
fn an_undefined_opcode_locks_the_cpu_until_reset() {
    let (mut cpu, mut bus) = setup(&[0xD3, 0x3C]);
    step(&mut cpu, &mut bus);
    assert!(cpu.is_locked());
    assert!(cpu.is_halted(), "a locked CPU reports as not running");

    // It stays locked, ignores pending interrupts, and never fetches again.
    cpu.ime = true;
    arm_interrupt(&mut bus, 0x01);
    for _ in 0..4 {
        assert_eq!(step(&mut cpu, &mut bus), 4);
    }
    assert_eq!(cpu.a, 0);
    assert_eq!(cpu.pc, ORG + 1);

    Cpu::<TestBus>::reset(&mut cpu);
    assert!(!cpu.is_halted());
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

#[test]
fn reset_and_post_boot_states() {
    let mut cpu = Sm83::new();
    cpu.a = 0x55;
    cpu.pc = 0x1234;
    Cpu::<TestBus>::reset(&mut cpu);
    assert_eq!(cpu, Sm83::new());
    assert_eq!(cpu.pc, 0x0000, "a true reset starts at the boot ROM");

    cpu.post_boot_dmg();
    assert_eq!(cpu.af(), 0x01B0);
    assert_eq!(cpu.pc, 0x0100);
    assert_eq!(cpu.sp, 0xFFFE);

    cpu.post_boot_cgb();
    assert_eq!(
        cpu.a, 0x11,
        "the CGB boot ROM leaves A=0x11 to identify the model"
    );
    assert_eq!(cpu.pc, 0x0100);
}

#[test]
fn save_state_round_trips_every_field() {
    let (mut cpu, mut bus) = setup(&[0xFB, 0x76]);
    arm_interrupt(&mut bus, 0x02);
    step(&mut cpu, &mut bus); // EI, leaving IME set and dispatch inhibited
    cpu.set_bc(0x1234);
    cpu.set_de(0x5678);
    cpu.set_hl(0x9ABC);

    let mut w = StateWriter::new();
    cpu.save(&mut w);
    let blob = w.into_inner();

    let mut restored = Sm83::new();
    restored.load(&mut StateReader::new(&blob)).unwrap();
    assert_eq!(restored, cpu);
}

#[test]
fn a_corrupt_flag_register_cannot_be_loaded_from_a_state() {
    let mut w = StateWriter::new();
    let mut cpu = Sm83::new();
    cpu.f = 0xF0;
    cpu.save(&mut w);
    let mut blob = w.into_inner();
    blob[1] = 0xFF; // hand-edit F to a value hardware cannot hold

    let mut restored = Sm83::new();
    restored.load(&mut StateReader::new(&blob)).unwrap();
    assert_eq!(restored.f, 0xF0, "the impossible low nibble is masked off");
}

#[test]
fn introspection_reports_the_register_file_and_flags() {
    let mut cpu = Sm83::new();
    cpu.set_af(0x12B0);
    cpu.set_hl(0x9ABC);
    cpu.pc = 0x0150;

    let regs = cpu.registers();
    let find = |name: &str| regs.iter().find(|r| r.name == name).unwrap().value;
    assert_eq!(find("A"), 0x12);
    assert_eq!(find("HL"), 0x9ABC);
    assert_eq!(find("PC"), 0x0150);
    assert_eq!(cpu.program_counter(), 0x0150);
    assert_eq!(cpu.flags_summary(), "ZnHC");

    cpu.set_program_counter(0x0200);
    assert_eq!(cpu.pc, 0x0200);
}
