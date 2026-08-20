//! Unit tests for the ARM7TDMI core.
//!
//! Organized by instruction family, plus the two areas that are the usual sources of subtle
//! bugs in this core: exception link-register offsets, and register banking across mode
//! switches.

use crate::*;
use core_common::{Bus, Cpu, CpuIntrospect, Savable, StateReader, StateWriter};

const ORG: u32 = 0x1000;
const STACK: u32 = 0x8000;

struct TestBus {
    mem: Vec<u8>,
}

impl TestBus {
    fn new() -> Self {
        Self {
            mem: vec![0; 0x1_0000],
        }
    }

    fn index(&self, addr: u32) -> usize {
        (addr as usize) & 0xFFFF
    }

    fn load_words(&mut self, at: u32, words: &[u32]) {
        for (i, w) in words.iter().enumerate() {
            let a = self.index(at + i as u32 * 4);
            self.mem[a..a + 4].copy_from_slice(&w.to_le_bytes());
        }
    }

    fn load_halfwords(&mut self, at: u32, halves: &[u16]) {
        for (i, h) in halves.iter().enumerate() {
            let a = self.index(at + i as u32 * 2);
            self.mem[a..a + 2].copy_from_slice(&h.to_le_bytes());
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

/// A privileged, interrupts-enabled starting point, so tests can exercise banking without
/// each one first having to escape User mode.
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

fn setup(program: &[u32]) -> (Arm7Tdmi, TestBus) {
    let mut bus = TestBus::new();
    bus.load_words(ORG, program);
    (Arm7Tdmi::new(boot()), bus)
}

fn setup_thumb(program: &[u16]) -> (Arm7Tdmi, TestBus) {
    let mut bus = TestBus::new();
    bus.load_halfwords(ORG, program);
    let cpu = Arm7Tdmi::new(BootState {
        thumb: true,
        ..boot()
    });
    (cpu, bus)
}

fn step(cpu: &mut Arm7Tdmi, bus: &mut TestBus) -> u32 {
    cpu.step(bus).get() as u32
}

/// Flags as `"NZCV"`, uppercase for set.
fn flags(cpu: &Arm7Tdmi) -> String {
    let f = |on: bool, s: char, c: char| if on { s } else { c };
    format!(
        "{}{}{}{}",
        f(cpu.cpsr.negative(), 'N', 'n'),
        f(cpu.cpsr.zero(), 'Z', 'z'),
        f(cpu.cpsr.carry(), 'C', 'c'),
        f(cpu.cpsr.overflow(), 'V', 'v')
    )
}

// ---------------------------------------------------------------------------
// Data processing
// ---------------------------------------------------------------------------

#[test]
fn data_processing_immediate_and_register_forms() {
    // mov r0, #1 ; add r1, r0, #2 ; sub r2, r1, r0
    let (mut cpu, mut bus) = setup(&[0xE3A0_0001, 0xE280_1002, 0xE041_2000]);
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.reg(0), 1);
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.reg(1), 3);
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.reg(2), 2);
}

#[test]
fn immediate_operands_are_rotated_right_by_twice_the_field() {
    // mov r0, #0xFF ror 8
    let (mut cpu, mut bus) = setup(&[0xE3A0_04FF]);
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.reg(0), 0xFF00_0000);
}

#[test]
fn arithmetic_flags_follow_the_architecture() {
    // adds r0, r0, r1 with 0x7FFFFFFF + 1: signed overflow, no carry.
    let (mut cpu, mut bus) = setup(&[0xE090_0001]);
    cpu.set_reg(0, 0x7FFF_FFFF);
    cpu.set_reg(1, 1);
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.reg(0), 0x8000_0000);
    assert_eq!(flags(&cpu), "NzcV");

    // 0xFFFFFFFF + 1: carry out, zero result, no signed overflow.
    let (mut cpu, mut bus) = setup(&[0xE090_0001]);
    cpu.set_reg(0, 0xFFFF_FFFF);
    cpu.set_reg(1, 1);
    step(&mut cpu, &mut bus);
    assert_eq!(flags(&cpu), "nZCv");
}

#[test]
fn subtraction_carry_is_the_inverse_of_borrow() {
    // 5 - 3 does not borrow, so carry is *set*.
    let (mut cpu, mut bus) = setup(&[0xE050_0001]);
    cpu.set_reg(0, 5);
    cpu.set_reg(1, 3);
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.reg(0), 2);
    assert_eq!(flags(&cpu), "nzCv");

    // 3 - 5 borrows, so carry is clear.
    let (mut cpu, mut bus) = setup(&[0xE050_0001]);
    cpu.set_reg(0, 3);
    cpu.set_reg(1, 5);
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.reg(0), 0xFFFF_FFFE);
    assert_eq!(flags(&cpu), "Nzcv");

    // Equal operands: zero, still no borrow.
    let (mut cpu, mut bus) = setup(&[0xE050_0001]);
    cpu.set_reg(0, 7);
    cpu.set_reg(1, 7);
    step(&mut cpu, &mut bus);
    assert_eq!(flags(&cpu), "nZCv");
}

#[test]
fn adc_and_sbc_thread_the_carry_flag_through() {
    let (mut cpu, mut bus) = setup(&[0xE0B0_0001]); // adcs r0, r0, r1
    cpu.set_reg(0, 1);
    cpu.set_reg(1, 1);
    cpu.cpsr.set_carry(true);
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.reg(0), 3);

    // sbcs with carry clear borrows one extra.
    let (mut cpu, mut bus) = setup(&[0xE0D0_0001]);
    cpu.set_reg(0, 5);
    cpu.set_reg(1, 1);
    cpu.cpsr.set_carry(false);
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.reg(0), 3);
}

#[test]
fn logical_operations_take_carry_from_the_shifter_and_leave_overflow_alone() {
    // movs r0, r1, lsl #1 with bit 31 set: the shifted-out bit becomes carry.
    let (mut cpu, mut bus) = setup(&[0xE1B0_0081]);
    cpu.set_reg(1, 0x8000_0000);
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.reg(0), 0);
    assert_eq!(flags(&cpu), "nZCv");

    let (mut cpu, mut bus) = setup(&[0xE010_0001]); // ands r0, r0, r1
    cpu.set_reg(0, 0xF0);
    cpu.set_reg(1, 0x0F);
    cpu.cpsr.set_overflow(true);
    step(&mut cpu, &mut bus);
    assert_eq!(flags(&cpu), "nZcV");
}

#[test]
fn every_shifter_operand_form() {
    // (instruction, r1 input, expected r0, expected flags)
    let cases: &[(u32, u32, u32, &str)] = &[
        (0xE1B0_0101, 0x0000_0001, 0x0000_0004, "nzcv"), // lsl #2
        (0xE1B0_0121, 0x0000_0004, 0x0000_0001, "nzcv"), // lsr #2
        (0xE1B0_0141, 0x8000_0000, 0xE000_0000, "Nzcv"), // asr #2
        (0xE1B0_0161, 0x0000_0003, 0xC000_0000, "NzCv"), // ror #2
        // A zero immediate amount means #32 for LSR and ASR, not "no shift".
        (0xE1B0_0021, 0x8000_0000, 0x0000_0000, "nZCv"), // lsr #32
        (0xE1B0_0041, 0x8000_0000, 0xFFFF_FFFF, "NzCv"), // asr #32
    ];
    for &(instr, input, expected, expected_flags) in cases {
        let (mut cpu, mut bus) = setup(&[instr]);
        cpu.set_reg(1, input);
        step(&mut cpu, &mut bus);
        assert_eq!(cpu.reg(0), expected, "instruction {instr:#010X}");
        assert_eq!(flags(&cpu), expected_flags, "instruction {instr:#010X}");
    }
}

#[test]
fn rrx_rotates_through_the_carry_flag() {
    let (mut cpu, mut bus) = setup(&[0xE1B0_0061]); // movs r0, r1, rrx
    cpu.set_reg(1, 0x0000_0003);
    cpu.cpsr.set_carry(true);
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.reg(0), 0x8000_0001);
    assert!(
        cpu.cpsr.carry(),
        "the shifted-out bit becomes the new carry"
    );
}

#[test]
fn a_register_specified_shift_of_zero_changes_nothing() {
    // The "#0 means #32" rule applies only to the immediate form; a register holding zero
    // must leave both the value and the carry untouched.
    let (mut cpu, mut bus) = setup(&[0xE1B0_0211]); // movs r0, r1, lsl r2
    cpu.set_reg(1, 0x1234_5678);
    cpu.set_reg(2, 0);
    cpu.cpsr.set_carry(true);
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.reg(0), 0x1234_5678);
    assert!(cpu.cpsr.carry());
}

#[test]
fn a_register_specified_shift_costs_an_extra_cycle() {
    let (mut cpu, mut bus) = setup(&[0xE1A0_0001]); // mov r0, r1
    assert_eq!(step(&mut cpu, &mut bus), 1);

    let (mut cpu, mut bus) = setup(&[0xE1A0_0211]); // mov r0, r1, lsl r2
    assert_eq!(step(&mut cpu, &mut bus), 2);
}

#[test]
fn reading_r15_sees_the_pipeline_two_instructions_ahead() {
    let (mut cpu, mut bus) = setup(&[0xE1A0_000F]); // mov r0, pc
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.reg(0), ORG + 8);

    // With a register-specified shift the instruction takes an extra cycle, and R15 reads one
    // instruction further along still.
    let (mut cpu, mut bus) = setup(&[0xE1A0_021F]); // mov r0, pc, lsl r2
    cpu.set_reg(2, 0);
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.reg(0), ORG + 12);
}

#[test]
fn writing_r15_branches_and_refills_the_pipeline() {
    let (mut cpu, mut bus) = setup(&[0xE1A0_F001]); // mov pc, r1
    cpu.set_reg(1, 0x2000);
    assert_eq!(step(&mut cpu, &mut bus), 3);
    assert_eq!(cpu.program_counter(), 0x2000);
}

#[test]
fn conditional_execution_skips_the_instruction_but_still_costs_a_fetch() {
    let (mut cpu, mut bus) = setup(&[0x03A0_0001]); // moveq r0, #1
    cpu.cpsr.set_zero(false);
    assert_eq!(step(&mut cpu, &mut bus), 1);
    assert_eq!(cpu.reg(0), 0);
    assert_eq!(cpu.program_counter(), ORG + 4);

    cpu.cpsr.set_zero(true);
    cpu.set_program_counter(ORG);
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.reg(0), 1);
}

// ---------------------------------------------------------------------------
// PSR transfer
// ---------------------------------------------------------------------------

#[test]
fn mrs_and_msr_move_the_status_register() {
    let (mut cpu, mut bus) = setup(&[0xE10F_0000]); // mrs r0, cpsr
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.reg(0), cpu.cpsr.bits());

    let (mut cpu, mut bus) = setup(&[0xE121_F000]); // msr cpsr_c, r0
    cpu.set_reg(0, Mode::Irq.bits());
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.mode(), Mode::Irq);
}

#[test]
fn user_mode_may_change_flags_but_not_control_bits() {
    // msr cpsr_fc, r0, attempting to leave User mode and mask interrupts at the same time.
    let (mut cpu, mut bus) = setup(&[0xE129_F000]);
    cpu.cpsr.set_mode(Mode::User);
    cpu.set_reg(0, 0xF000_0000 | Mode::System.bits() | Psr::I);
    step(&mut cpu, &mut bus);

    assert_eq!(
        cpu.mode(),
        Mode::User,
        "unprivileged code cannot change mode"
    );
    assert!(!cpu.cpsr.irq_disabled(), "nor mask interrupts");
    assert_eq!(flags(&cpu), "NZCV", "but the condition flags do change");
}

// ---------------------------------------------------------------------------
// Multiply
// ---------------------------------------------------------------------------

#[test]
fn multiply_and_multiply_accumulate() {
    let (mut cpu, mut bus) = setup(&[0xE003_0291]); // mul r3, r1, r2
    cpu.set_reg(1, 7);
    cpu.set_reg(2, 6);
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.reg(3), 42);

    let (mut cpu, mut bus) = setup(&[0xE023_4291]); // mla r3, r1, r2, r4
    cpu.set_reg(1, 7);
    cpu.set_reg(2, 6);
    cpu.set_reg(4, 8);
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.reg(3), 50);
}

#[test]
fn multiply_cycle_count_tracks_the_operand_magnitude() {
    // The multiplier early-terminates once the rest of Rs is all sign bits.
    for (rs, expected) in [
        (0x0000_0001u32, 2u32),
        (0x0000_0100, 3),
        (0x0001_0000, 4),
        (0x0100_0000, 5),
    ] {
        let (mut cpu, mut bus) = setup(&[0xE003_0291]);
        cpu.set_reg(1, 1);
        cpu.set_reg(2, rs);
        assert_eq!(step(&mut cpu, &mut bus), expected, "rs = {rs:#010X}");
    }

    // A negative operand terminates on all-ones just as a small one does on all-zeros.
    let (mut cpu, mut bus) = setup(&[0xE003_0291]);
    cpu.set_reg(1, 1);
    cpu.set_reg(2, 0xFFFF_FFFF);
    assert_eq!(step(&mut cpu, &mut bus), 2);
}

#[test]
fn long_multiply_is_signed_or_unsigned_per_the_encoding() {
    let (mut cpu, mut bus) = setup(&[0xE081_0392]); // umull r0, r1, r2, r3
    cpu.set_reg(2, 0xFFFF_FFFF);
    cpu.set_reg(3, 2);
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.reg(0), 0xFFFF_FFFE);
    assert_eq!(cpu.reg(1), 0x0000_0001);

    // smull reads the same bits as -1.
    let (mut cpu, mut bus) = setup(&[0xE0C1_0392]);
    cpu.set_reg(2, 0xFFFF_FFFF);
    cpu.set_reg(3, 2);
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.reg(0), 0xFFFF_FFFE);
    assert_eq!(cpu.reg(1), 0xFFFF_FFFF);
}

// ---------------------------------------------------------------------------
// Memory
// ---------------------------------------------------------------------------

#[test]
fn single_transfer_addressing_modes_and_writeback() {
    // Pre-indexed, no writeback.
    let (mut cpu, mut bus) = setup(&[0xE590_1004]); // ldr r1, [r0, #4]
    cpu.set_reg(0, 0x2000);
    bus.load_words(0x2004, &[0xCAFE_BABE]);
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.reg(1), 0xCAFE_BABE);
    assert_eq!(cpu.reg(0), 0x2000, "no writeback without the W bit");

    // Pre-indexed with writeback.
    let (mut cpu, mut bus) = setup(&[0xE5B0_1004]); // ldr r1, [r0, #4]!
    cpu.set_reg(0, 0x2000);
    bus.load_words(0x2004, &[0x1111_2222]);
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.reg(1), 0x1111_2222);
    assert_eq!(cpu.reg(0), 0x2004);

    // Post-indexed always writes back.
    let (mut cpu, mut bus) = setup(&[0xE490_1004]); // ldr r1, [r0], #4
    cpu.set_reg(0, 0x2000);
    bus.load_words(0x2000, &[0x3333_4444]);
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.reg(1), 0x3333_4444);
    assert_eq!(cpu.reg(0), 0x2004);

    // Down-direction offset.
    let (mut cpu, mut bus) = setup(&[0xE510_1004]); // ldr r1, [r0, -#4]
    cpu.set_reg(0, 0x2004);
    bus.load_words(0x2000, &[0x5555_6666]);
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.reg(1), 0x5555_6666);
}

#[test]
fn an_unaligned_word_load_rotates_rather_than_faulting() {
    let (mut cpu, mut bus) = setup(&[0xE590_1000]); // ldr r1, [r0]
    cpu.set_reg(0, 0x2001);
    bus.load_words(0x2000, &[0xAABB_CCDD]);
    step(&mut cpu, &mut bus);
    // The containing word is read, then rotated right eight bits per byte of misalignment.
    assert_eq!(cpu.reg(1), 0xDDAA_BBCC);
}

#[test]
fn byte_and_halfword_transfers_extend_correctly() {
    let (mut cpu, mut bus) = setup(&[0xE5D0_1000]); // ldrb r1, [r0]
    cpu.set_reg(0, 0x2000);
    bus.load_words(0x2000, &[0x0000_00FF]);
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.reg(1), 0xFF, "a byte load zero-extends");

    let (mut cpu, mut bus) = setup(&[0xE1D0_10D0]); // ldrsb r1, [r0]
    cpu.set_reg(0, 0x2000);
    bus.load_words(0x2000, &[0x0000_00FF]);
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.reg(1), 0xFFFF_FFFF, "a signed byte load sign-extends");

    let (mut cpu, mut bus) = setup(&[0xE1D0_10F0]); // ldrsh r1, [r0]
    cpu.set_reg(0, 0x2000);
    bus.load_words(0x2000, &[0x0000_8000]);
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.reg(1), 0xFFFF_8000);

    let (mut cpu, mut bus) = setup(&[0xE1C0_10B0]); // strh r1, [r0]
    cpu.set_reg(0, 0x2000);
    cpu.set_reg(1, 0xDEAD_BEEF);
    step(&mut cpu, &mut bus);
    assert_eq!(bus.word(0x2000) & 0xFFFF, 0xBEEF);
}

#[test]
fn storing_r15_stores_the_pipeline_value_plus_one_more_instruction() {
    let (mut cpu, mut bus) = setup(&[0xE580_F000]); // str pc, [r0]
    cpu.set_reg(0, 0x2000);
    step(&mut cpu, &mut bus);
    assert_eq!(bus.word(0x2000), ORG + 12);
}

#[test]
fn swap_exchanges_memory_and_register() {
    let (mut cpu, mut bus) = setup(&[0xE100_1092]); // swp r1, r2, [r0]
    cpu.set_reg(0, 0x2000);
    cpu.set_reg(2, 0xAAAA_AAAA);
    bus.load_words(0x2000, &[0x5555_5555]);
    let cycles = step(&mut cpu, &mut bus);
    assert_eq!(cpu.reg(1), 0x5555_5555);
    assert_eq!(bus.word(0x2000), 0xAAAA_AAAA);
    assert_eq!(cycles, 4);
}

#[test]
fn block_transfers_always_move_registers_in_ascending_address_order() {
    let (mut cpu, mut bus) = setup(&[0xE8A0_000E]); // stmia r0!, {r1-r3}
    cpu.set_reg(0, 0x2000);
    cpu.set_reg(1, 0x11);
    cpu.set_reg(2, 0x22);
    cpu.set_reg(3, 0x33);
    step(&mut cpu, &mut bus);
    assert_eq!(bus.word(0x2000), 0x11);
    assert_eq!(bus.word(0x2004), 0x22);
    assert_eq!(bus.word(0x2008), 0x33);
    assert_eq!(cpu.reg(0), 0x200C);

    // Decrementing: the base ends up lower, but the lowest register still lands lowest.
    let (mut cpu, mut bus) = setup(&[0xE920_000E]); // stmdb r0!, {r1-r3}
    cpu.set_reg(0, 0x200C);
    cpu.set_reg(1, 0x11);
    cpu.set_reg(2, 0x22);
    cpu.set_reg(3, 0x33);
    step(&mut cpu, &mut bus);
    assert_eq!(bus.word(0x2000), 0x11);
    assert_eq!(bus.word(0x2008), 0x33);
    assert_eq!(cpu.reg(0), 0x2000);

    let (mut cpu, mut bus) = setup(&[0xE8B0_000E]); // ldmia r0!, {r1-r3}
    cpu.set_reg(0, 0x2000);
    bus.load_words(0x2000, &[0xA, 0xB, 0xC]);
    step(&mut cpu, &mut bus);
    assert_eq!((cpu.reg(1), cpu.reg(2), cpu.reg(3)), (0xA, 0xB, 0xC));
    assert_eq!(cpu.reg(0), 0x200C);
}

#[test]
fn loading_into_the_base_register_beats_writeback() {
    let (mut cpu, mut bus) = setup(&[0xE8B0_0003]); // ldmia r0!, {r0, r1}
    cpu.set_reg(0, 0x2000);
    bus.load_words(0x2000, &[0xDEAD, 0xBEEF]);
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.reg(0), 0xDEAD);
    assert_eq!(cpu.reg(1), 0xBEEF);
}

#[test]
fn storing_the_base_register_stores_the_new_value_unless_it_goes_first() {
    // r1 is not the lowest register in the list, so the written-back base is what gets stored.
    let (mut cpu, mut bus) = setup(&[0xE8A1_0003]); // stmia r1!, {r0, r1}
    cpu.set_reg(0, 0xAAAA);
    cpu.set_reg(1, 0x2000);
    step(&mut cpu, &mut bus);
    assert_eq!(bus.word(0x2000), 0xAAAA);
    assert_eq!(
        bus.word(0x2004),
        0x2008,
        "the base stores its post-writeback value"
    );

    // r0 *is* the lowest, so its original value is stored.
    let (mut cpu, mut bus) = setup(&[0xE8A0_0003]); // stmia r0!, {r0, r1}
    cpu.set_reg(0, 0x2000);
    cpu.set_reg(1, 0xBBBB);
    step(&mut cpu, &mut bus);
    assert_eq!(
        bus.word(0x2000),
        0x2000,
        "the base stores its original value"
    );
}

#[test]
fn block_transfer_with_the_s_bit_reaches_the_user_bank() {
    // stmia r0, {sp}^ from IRQ mode must store User's SP, not IRQ's. Getting this wrong shows
    // up as an exception handler saving the wrong task's stack.
    let (mut cpu, mut bus) = setup(&[0xE8C0_2000]);
    cpu.regs.write(Mode::User, 13, 0x7777);
    cpu.cpsr.set_mode(Mode::Irq);
    cpu.set_reg(13, 0x8888);
    cpu.set_reg(0, 0x3000);
    step(&mut cpu, &mut bus);
    assert_eq!(bus.word(0x3000), 0x7777);
}

// ---------------------------------------------------------------------------
// Branches
// ---------------------------------------------------------------------------

#[test]
fn branch_and_branch_with_link() {
    let (mut cpu, mut bus) = setup(&[0xEA00_0002]); // b +8, relative to pc + 8
    let cycles = step(&mut cpu, &mut bus);
    assert_eq!(cpu.program_counter(), ORG + 8 + 8);
    assert_eq!(cycles, 3);

    let (mut cpu, mut bus) = setup(&[0xEB00_0002]); // bl
    step(&mut cpu, &mut bus);
    assert_eq!(
        cpu.reg(14),
        ORG + 4,
        "LR is the instruction after the branch"
    );
}

#[test]
fn bx_switches_instruction_set_from_the_low_bit() {
    let (mut cpu, mut bus) = setup(&[0xE12F_FF11]); // bx r1
    cpu.set_reg(1, 0x2001);
    step(&mut cpu, &mut bus);
    assert!(cpu.is_thumb());
    assert_eq!(
        cpu.program_counter(),
        0x2000,
        "the low bit selects the instruction set, it is not part of the address"
    );

    let (mut cpu, mut bus) = setup(&[0xE12F_FF11]);
    cpu.set_reg(1, 0x2000);
    step(&mut cpu, &mut bus);
    assert!(!cpu.is_thumb());
}

// ---------------------------------------------------------------------------
// Exceptions and banking
// ---------------------------------------------------------------------------

#[test]
fn swi_enters_supervisor_mode_with_the_return_address_in_lr() {
    let (mut cpu, mut bus) = setup(&[0xEF00_0042]);
    let entry_cpsr = cpu.cpsr;
    let cycles = step(&mut cpu, &mut bus);

    assert_eq!(cpu.program_counter(), Exception::SoftwareInterrupt.vector());
    assert_eq!(cpu.mode(), Mode::Supervisor);
    assert_eq!(cpu.reg(14), ORG + 4, "LR is the instruction after the SWI");
    assert_eq!(cpu.regs.spsr(Mode::Supervisor), Some(entry_cpsr));
    assert!(cpu.cpsr.irq_disabled());
    assert!(!cpu.cpsr.fiq_disabled(), "SWI does not mask FIQ");
    assert!(!cpu.is_thumb(), "exceptions always enter ARM state");
    assert_eq!(cycles, 3);
}

#[test]
fn undefined_instructions_trap_including_coprocessor_encodings() {
    for instr in [0xE600_0010u32, 0xEE00_0000] {
        let (mut cpu, mut bus) = setup(&[instr]);
        step(&mut cpu, &mut bus);
        assert_eq!(
            cpu.program_counter(),
            Exception::UndefinedInstruction.vector(),
            "instruction {instr:#010X}"
        );
        assert_eq!(cpu.mode(), Mode::Undefined);
        assert_eq!(cpu.reg(14), ORG + 4);
    }
}

#[test]
fn irq_entry_leaves_lr_four_bytes_past_the_interrupted_instruction() {
    // The handler returns with `subs pc, lr, #4`, so LR must be next_pc + 4.
    let (mut cpu, mut bus) = setup(&[0xE3A0_0001]);
    cpu.set_irq_line(true);
    let cycles = step(&mut cpu, &mut bus);

    assert_eq!(cpu.program_counter(), Exception::Irq.vector());
    assert_eq!(cpu.mode(), Mode::Irq);
    assert_eq!(cpu.reg(14), ORG + 4);
    assert!(cpu.cpsr.irq_disabled());
    assert!(!cpu.cpsr.fiq_disabled());
    assert_eq!(cycles, 3);
    assert_eq!(cpu.reg(0), 0, "the interrupted instruction did not execute");
}

#[test]
fn fiq_takes_priority_over_irq_and_masks_both_lines() {
    let (mut cpu, mut bus) = setup(&[0xE1A0_0000]);
    cpu.set_irq_line(true);
    cpu.set_fiq_line(true);
    step(&mut cpu, &mut bus);

    assert_eq!(cpu.program_counter(), Exception::Fiq.vector());
    assert_eq!(cpu.mode(), Mode::Fiq);
    assert!(cpu.cpsr.irq_disabled());
    assert!(cpu.cpsr.fiq_disabled(), "FIQ masks itself on entry");
}

#[test]
fn a_masked_interrupt_line_is_not_taken() {
    let (mut cpu, mut bus) = setup(&[0xE3A0_0001]);
    cpu.cpsr.set_irq_disabled(true);
    cpu.set_irq_line(true);
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.program_counter(), ORG + 4);
    assert_eq!(cpu.reg(0), 1, "the instruction ran normally");
}

#[test]
fn exception_entry_banks_sp_and_lr_leaving_the_previous_mode_untouched() {
    let (mut cpu, mut bus) = setup(&[0xEF00_0000]);
    cpu.set_reg(13, 0x7000);
    cpu.set_reg(14, 0x1234);
    cpu.regs.write(Mode::Supervisor, 13, 0x9000);

    step(&mut cpu, &mut bus);

    assert_eq!(cpu.reg(13), 0x9000, "SVC has its own stack pointer");
    assert_eq!(cpu.reg(14), ORG + 4);
    assert_eq!(
        cpu.regs.read(Mode::System, 13),
        0x7000,
        "the old bank is intact"
    );
    assert_eq!(cpu.regs.read(Mode::System, 14), 0x1234);
}

#[test]
fn fiq_banks_the_upper_general_registers_too() {
    let (mut cpu, mut bus) = setup(&[0xE1A0_0000]);
    cpu.set_reg(8, 0xAAAA);
    cpu.set_fiq_line(true);
    step(&mut cpu, &mut bus);

    assert_eq!(cpu.mode(), Mode::Fiq);
    assert_eq!(cpu.reg(8), 0, "FIQ sees its own R8");
    assert_eq!(cpu.regs.read(Mode::System, 8), 0xAAAA);
}

#[test]
fn exception_return_restores_cpsr_from_spsr() {
    let (mut cpu, mut bus) = setup(&[0xE1A0_0000]);
    bus.load_words(Exception::Irq.vector(), &[0xE25E_F004]); // subs pc, lr, #4
    let original_cpsr = cpu.cpsr;

    cpu.set_irq_line(true);
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.mode(), Mode::Irq);

    cpu.set_irq_line(false);
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.cpsr, original_cpsr, "CPSR is restored wholesale");
    assert_eq!(
        cpu.program_counter(),
        ORG,
        "and execution resumes at the interrupted instruction"
    );
}

#[test]
fn ldm_with_the_s_bit_and_r15_restores_cpsr() {
    let (mut cpu, mut bus) = setup(&[0xE8FD_8001]); // ldmia sp!, {r0, pc}^
    cpu.cpsr.set_mode(Mode::Irq);
    let mut saved = Psr::default();
    saved.set_mode(Mode::System);
    saved.set_negative(true);
    cpu.regs.set_spsr(Mode::Irq, saved);
    cpu.set_reg(13, 0x3000);
    bus.load_words(0x3000, &[0x1111, 0x4000]);

    step(&mut cpu, &mut bus);

    assert_eq!(cpu.reg(0), 0x1111);
    assert_eq!(cpu.program_counter(), 0x4000);
    assert_eq!(cpu.mode(), Mode::System, "CPSR came back from SPSR");
    assert!(cpu.cpsr.negative());
}

#[test]
fn halt_waits_for_an_interrupt_line() {
    let (mut cpu, mut bus) = setup(&[0xE3A0_0001]);
    cpu.halt();
    assert_eq!(step(&mut cpu, &mut bus), 1);
    assert_eq!(cpu.reg(0), 0, "nothing executes while halted");
    assert!(cpu.is_halted());

    // An asserted line wakes the core even with the mask set: the wake signal comes from the
    // interrupt controller, not from the CPU's own mask.
    cpu.cpsr.set_irq_disabled(true);
    cpu.set_irq_line(true);
    step(&mut cpu, &mut bus);
    assert!(!cpu.is_halted());
    assert_eq!(cpu.reg(0), 1);
}

// ---------------------------------------------------------------------------
// THUMB
// ---------------------------------------------------------------------------

#[test]
fn thumb_move_shifted_and_add_subtract() {
    let (mut cpu, mut bus) = setup_thumb(&[0x0088]); // lsls r0, r1, #2
    cpu.set_reg(1, 3);
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.reg(0), 12);

    let (mut cpu, mut bus) = setup_thumb(&[0x1888]); // adds r0, r1, r2
    cpu.set_reg(1, 5);
    cpu.set_reg(2, 6);
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.reg(0), 11);

    let (mut cpu, mut bus) = setup_thumb(&[0x1E88]); // subs r0, r1, #2
    cpu.set_reg(1, 5);
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.reg(0), 3);
}

#[test]
fn thumb_mov_immediate_sets_flags_unlike_its_arm_counterpart() {
    let (mut cpu, mut bus) = setup_thumb(&[0x2000]); // movs r0, #0
    cpu.cpsr.set_overflow(true);
    cpu.cpsr.set_carry(true);
    step(&mut cpu, &mut bus);
    assert_eq!(flags(&cpu), "nZCV", "N and Z change; C and V do not");
}

#[test]
fn thumb_alu_operations() {
    let (mut cpu, mut bus) = setup_thumb(&[0x4348]); // muls r0, r1
    cpu.set_reg(0, 6);
    cpu.set_reg(1, 7);
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.reg(0), 42);

    let (mut cpu, mut bus) = setup_thumb(&[0x4248]); // negs r0, r1
    cpu.set_reg(1, 5);
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.reg(0), (-5i32) as u32);
}

#[test]
fn thumb_high_register_add_does_not_set_flags() {
    let (mut cpu, mut bus) = setup_thumb(&[0x4441]); // add r1, r8
    cpu.set_reg(1, 1);
    cpu.set_reg(8, 0xFFFF_FFFF);
    cpu.cpsr.set_zero(false);
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.reg(1), 0);
    assert!(
        !cpu.cpsr.zero(),
        "the high-register form is deliberately flag-free"
    );
}

#[test]
fn thumb_bx_returns_to_arm_state() {
    let (mut cpu, mut bus) = setup_thumb(&[0x4770]); // bx lr
    cpu.set_reg(14, 0x2000);
    step(&mut cpu, &mut bus);
    assert!(!cpu.is_thumb());
    assert_eq!(cpu.program_counter(), 0x2000);
}

#[test]
fn thumb_pc_relative_load_ignores_bit_one_of_the_pc() {
    // From a halfword-but-not-word-aligned address, the literal pool base still rounds down.
    let mut bus = TestBus::new();
    bus.load_halfwords(ORG + 2, &[0x4800]); // ldr r0, [pc, #0]
    bus.load_words(ORG + 4, &[0xFEED_FACE]);
    let mut cpu = Arm7Tdmi::new(BootState {
        pc: ORG + 2,
        thumb: true,
        ..boot()
    });
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.reg(0), 0xFEED_FACE);
}

#[test]
fn thumb_push_and_pop() {
    let (mut cpu, mut bus) = setup_thumb(&[0xB50F]); // push {r0-r3, lr}
    for i in 0..4 {
        cpu.set_reg(i, 0x10 + i as u32);
    }
    cpu.set_reg(14, 0xAAAA);
    step(&mut cpu, &mut bus);

    assert_eq!(cpu.reg(13), STACK - 20);
    assert_eq!(bus.word(STACK - 20), 0x10);
    assert_eq!(bus.word(STACK - 4), 0xAAAA, "LR goes on top");

    // Popping into PC stays in THUMB state on this core rather than behaving like BX; that
    // only changes on ARMv5.
    let (mut cpu, mut bus) = setup_thumb(&[0xBD00]); // pop {pc}
    cpu.set_reg(13, 0x3000);
    bus.load_words(0x3000, &[0x2000]);
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.program_counter(), 0x2000);
    assert!(cpu.is_thumb());
    assert_eq!(cpu.reg(13), 0x3004);
}

#[test]
fn thumb_conditional_branch_costs_more_when_taken() {
    let (mut cpu, mut bus) = setup_thumb(&[0xD002]); // beq +4
    cpu.cpsr.set_zero(false);
    assert_eq!(step(&mut cpu, &mut bus), 1);
    assert_eq!(cpu.program_counter(), ORG + 2);

    let (mut cpu, mut bus) = setup_thumb(&[0xD002]);
    cpu.cpsr.set_zero(true);
    assert_eq!(step(&mut cpu, &mut bus), 3);
    assert_eq!(cpu.program_counter(), ORG + 4 + 4);
}

#[test]
fn thumb_long_branch_with_link_is_two_separate_instructions() {
    // An interrupt can land between the halves, which is why they are not modelled as one
    // 32-bit instruction.
    let (mut cpu, mut bus) = setup_thumb(&[0xF000, 0xF87E]);
    step(&mut cpu, &mut bus);
    assert_eq!(
        cpu.program_counter(),
        ORG + 2,
        "the high half only stages LR"
    );

    step(&mut cpu, &mut bus);
    assert_eq!(cpu.program_counter(), ORG + 4 + 0xFC);
    assert_eq!(
        cpu.reg(14),
        (ORG + 4) | 1,
        "the return address is marked THUMB"
    );
}

#[test]
fn thumb_block_transfer_writes_back() {
    let (mut cpu, mut bus) = setup_thumb(&[0xC803]); // ldmia r0!, {r0, r1}
    cpu.set_reg(0, 0x3000);
    bus.load_words(0x3000, &[0xAAAA, 0xBBBB]);
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.reg(0), 0xAAAA, "the loaded value beats writeback");
    assert_eq!(cpu.reg(1), 0xBBBB);
}

#[test]
fn thumb_swi_enters_supervisor_mode_in_arm_state() {
    let (mut cpu, mut bus) = setup_thumb(&[0xDF12]); // swi 0x12
    step(&mut cpu, &mut bus);
    assert_eq!(cpu.mode(), Mode::Supervisor);
    assert!(!cpu.is_thumb());
    assert_eq!(cpu.reg(14), ORG + 2, "LR is the instruction after the SWI");
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

#[test]
fn save_state_round_trips_every_bank_and_signal() {
    let (mut cpu, mut bus) = setup(&[0xEF00_0000]);
    cpu.set_reg(5, 0x1234);
    step(&mut cpu, &mut bus); // into Supervisor, populating a bank and an SPSR
    cpu.set_reg(13, 0x9999);
    cpu.set_irq_line(true);
    cpu.halt();

    let mut w = StateWriter::new();
    cpu.save(&mut w);
    let blob = w.into_inner();

    let mut restored = Arm7Tdmi::default();
    restored.load(&mut StateReader::new(&blob)).unwrap();
    assert_eq!(restored, cpu);
}

#[test]
fn reset_returns_to_the_configured_boot_state() {
    let (mut cpu, mut bus) = setup(&[0xE3A0_0001]);
    step(&mut cpu, &mut bus);
    cpu.cpsr.set_mode(Mode::Irq);

    Cpu::<TestBus>::reset(&mut cpu);
    assert_eq!(cpu.program_counter(), ORG);
    assert_eq!(cpu.mode(), Mode::System);
    assert_eq!(cpu.reg(13), STACK);
    assert_eq!(cpu.reg(0), 0);
}

#[test]
fn introspection_exposes_the_inactive_banks() {
    // "What did the mode I came from have in R13?" is the most common ARM debugging question,
    // and a register view limited to the current mode cannot answer it.
    let (mut cpu, mut bus) = setup(&[0xEF00_0000]);
    cpu.regs.write(Mode::Irq, 13, 0x5000);
    step(&mut cpu, &mut bus);

    let regs = cpu.registers();
    let named = |name: &str| regs.iter().find(|r| r.name == name);
    assert_eq!(named("sp_irq").map(|r| r.value), Some(0x5000));
    assert!(named("cpsr").is_some());
    assert!(named("spsr").is_some(), "Supervisor mode has an SPSR");
    assert!(
        named("sp_svc").is_none(),
        "the active bank is already listed as sp, not duplicated"
    );
}

// ---------------------------------------------------------------------------
// The legacy "P" form
// ---------------------------------------------------------------------------

#[test]
fn a_comparison_with_r15_as_its_destination_restores_cpsr_from_spsr() {
    // The ARM7TDMI's legacy "P" form. A comparison writes no result, so its `Rd` field is
    // otherwise unused; naming R15 there means "copy SPSR into CPSR" — a mode change with no
    // branch. `gba-suite` uses `CMPP PC, R0` to leave FIQ mode after testing banked registers,
    // and without this the machine stays in FIQ, pops a return address off the wrong stack, and
    // jumps to zero. That is the bug this test exists to prevent coming back.
    let (mut cpu, mut bus) = setup(&[]);

    let mut spsr = cpu.cpsr;
    spsr.set_mode(Mode::System);
    cpu.cpsr.set_mode(Mode::Fiq);
    cpu.regs.set_spsr(Mode::Fiq, spsr);
    assert_eq!(cpu.mode(), Mode::Fiq);

    // `CMP PC, R0` with S set and Rd = R15.
    cpu.execute_arm(0xE15F_F000, &mut bus);
    assert_eq!(cpu.mode(), Mode::System, "the SPSR was copied back");
}

#[test]
fn an_ordinary_comparison_leaves_the_mode_alone() {
    // The P form is selected by `Rd`, so a normal comparison must not trip it.
    let (mut cpu, mut bus) = setup(&[]);
    let mut spsr = cpu.cpsr;
    spsr.set_mode(Mode::System);
    cpu.cpsr.set_mode(Mode::Fiq);
    cpu.regs.set_spsr(Mode::Fiq, spsr);

    // `CMP R1, R0` — `Rd` is R0, not R15.
    cpu.execute_arm(0xE151_0000, &mut bus);
    assert_eq!(cpu.mode(), Mode::Fiq);
}

#[test]
fn every_comparison_opcode_supports_the_p_form() {
    // TST, TEQ, CMP, and CMN all write no result, so all four have the spare `Rd` field.
    for opcode in [0b1000u32, 0b1001, 0b1010, 0b1011] {
        let (mut cpu, mut bus) = setup(&[]);
        let mut spsr = cpu.cpsr;
        spsr.set_mode(Mode::System);
        cpu.cpsr.set_mode(Mode::Fiq);
        cpu.regs.set_spsr(Mode::Fiq, spsr);

        let instr = 0xE000_0000 | (opcode << 21) | (1 << 20) | (15 << 12);
        cpu.execute_arm(instr, &mut bus);
        assert_eq!(cpu.mode(), Mode::System, "opcode {opcode:#06b}");
    }
}

#[test]
fn a_load_into_pc_discards_bit_zero_on_this_core() {
    // The ARMv4T half of the decision `Arm7Tdmi::interworking_loads` records. From ARMv5 on,
    // `LDR pc`, `LDM {..., pc}` and `POP {..., pc}` all take their instruction set from bit 0 of
    // the loaded address, exactly as `BX` does. This core predates that: the bit is discarded and
    // the state does not change.
    //
    // Asserted here rather than only where the ARM9 turns it on, because this is the behaviour a
    // GBA depends on — and the flag exists precisely so that adding the ARM9's behaviour could not
    // quietly become the GBA's.
    //
    // `ldr pc, [r0]`, with the loaded address carrying the low bit set.
    let (mut cpu, mut bus) = setup(&[0xE590_F000]);
    bus.load_words(0x2000, &[0x0000_3001]);
    cpu.set_reg(0, 0x2000);
    step(&mut cpu, &mut bus);
    assert!(!cpu.is_thumb(), "bit 0 does not select THUMB here");
    assert_eq!(cpu.program_counter(), 0x3000, "and it is masked off the PC");

    // `ldmia r0, {pc}`, same story.
    let (mut cpu, mut bus) = setup(&[0xE890_8000]);
    bus.load_words(0x2000, &[0x0000_3001]);
    cpu.set_reg(0, 0x2000);
    step(&mut cpu, &mut bus);
    assert!(!cpu.is_thumb());
    assert_eq!(cpu.program_counter(), 0x3000);

    // And `bx`, which *is* an ARMv4T instruction, still interworks — so this is a property of the
    // load rather than of the core being unable to enter THUMB at all.
    let (mut cpu, mut bus) = setup(&[0xE12F_FF10]);
    cpu.set_reg(0, 0x0000_3001);
    step(&mut cpu, &mut bus);
    assert!(cpu.is_thumb());
    assert_eq!(cpu.program_counter(), 0x3000);
}
