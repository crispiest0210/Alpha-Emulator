use super::*;

fn unit() -> MathUnits {
    MathUnits::new()
}

/// Set the numerator, denominator and mode, and read back quotient and remainder.
fn divide(mode: u16, numerator: u64, denominator: u64) -> (u64, u64, u16) {
    let mut m = unit();
    m.write32(reg::DIVCNT, mode as u32);
    m.write32(reg::DIV_NUMERATOR, numerator as u32);
    m.write32(reg::DIV_NUMERATOR + 4, (numerator >> 32) as u32);
    m.write32(reg::DIV_DENOMINATOR, denominator as u32);
    m.write32(reg::DIV_DENOMINATOR + 4, (denominator >> 32) as u32);
    let quotient =
        m.read32(reg::DIV_RESULT) as u64 | ((m.read32(reg::DIV_RESULT + 4) as u64) << 32);
    let remainder =
        m.read32(reg::DIV_REMAINDER) as u64 | ((m.read32(reg::DIV_REMAINDER + 4) as u64) << 32);
    (quotient, remainder, m.read32(reg::DIVCNT) as u16)
}

#[test]
fn the_three_modes_take_their_operands_from_different_widths() {
    // The whole point of having three: mode 0 is 32 by 32, mode 1 keeps a 64-bit numerator over a
    // 32-bit denominator, and mode 2 is 64 by 64. Decoding the mode as one width makes libnds's
    // `divf32` — which shifts its numerator up twelve places and needs mode 1 for the room —
    // divide a truncated numerator and return a number that is wrong by a factor of a million.
    assert_eq!(divide(0, 100, 7).0 as i64, 14);
    assert_eq!(divide(0, 100, 7).1 as i64, 2);

    // Mode 0 ignores the high half of both operands; mode 1 does not ignore the numerator's.
    let numerator = 0x0000_0001_0000_0000;
    assert_eq!(divide(0, numerator, 2).0 as i64, 0, "the low word is zero");
    assert_eq!(divide(1, numerator, 2).0 as i64, 0x8000_0000);
    assert_eq!(divide(2, numerator, 2).0 as i64, 0x8000_0000);
}

#[test]
fn the_operands_are_signed_and_truncate_toward_zero() {
    assert_eq!(divide(0, (-7i32) as u32 as u64, 2).0 as i32, -3);
    assert_eq!(divide(0, (-7i32) as u32 as u64, 2).1 as i32, -1);
    // Mode 0 sign-extends a 32-bit operand into the 64-bit arithmetic; not doing so turns every
    // negative numerator into a number above four billion.
    assert_eq!(divide(0, (-100i32) as u32 as u64, 7).0 as i32, -14);
    assert_eq!(divide(2, (-100i64) as u64, 7).0 as i64, -14);
}

#[test]
fn a_zero_denominator_sets_its_flag_and_returns_the_documented_answer() {
    // Not "leave the previous result alone": software checks the flag, and the values hardware
    // leaves behind are documented, so they are worth matching exactly.
    let (quotient, remainder, control) = divide(2, 42, 0);
    assert_ne!(control & DIV_BY_ZERO, 0);
    assert_eq!(remainder, 42, "the remainder is the numerator, untouched");
    assert_eq!(quotient as i64, -1, "and the quotient is minus one");

    let (quotient, _, _) = divide(2, (-42i64) as u64, 0);
    assert_eq!(
        quotient as i64, 1,
        "with the opposite sign to the numerator"
    );

    // In the 32-bit mode the high word comes back inverted.
    let (quotient, _, _) = divide(0, 42, 0);
    assert_eq!(quotient, 0x0000_0000_FFFF_FFFF);
}

#[test]
fn the_zero_flag_is_about_the_register_and_not_about_the_operand() {
    // A denominator whose low word is zero is still a real denominator in 64-bit mode, and one
    // whose high word is set is not zero however the mode reads it. Deriving the flag from the
    // mode's operand rather than from the register gets both backwards.
    let (_, _, control) = divide(2, 100, 0x0000_0001_0000_0000);
    assert_eq!(control & DIV_BY_ZERO, 0, "not zero, just large");

    let (_, _, control) = divide(0, 100, 0);
    assert_ne!(control & DIV_BY_ZERO, 0);
}

#[test]
fn the_one_division_with_no_answer_returns_the_numerator() {
    // The most negative number over minus one overflows two's complement. Hardware returns the
    // numerator; Rust panics, which would take the emulator down with it.
    let (quotient, remainder, _) = divide(2, i64::MIN as u64, (-1i64) as u64);
    assert_eq!(quotient, i64::MIN as u64);
    assert_eq!(remainder, 0);
}

#[test]
fn the_square_root_is_unsigned_and_integer() {
    let mut m = unit();
    for (mode, param, expected) in [
        (0u32, 0u64, 0u32),
        (0, 16, 4),
        (0, 17, 4),
        (0, 0xFFFF_FFFF, 0xFFFF),
        // Mode 1 is the 64-bit input, and its whole reason for existing is values this large.
        (1, 0xFFFF_FFFF_FFFF_FFFF, 0xFFFF_FFFF),
        (1, 0x0000_0001_0000_0000, 0x0001_0000),
    ] {
        m.write32(reg::SQRTCNT, mode);
        m.write32(reg::SQRT_PARAM, param as u32);
        m.write32(reg::SQRT_PARAM + 4, (param >> 32) as u32);
        assert_eq!(
            m.read32(reg::SQRT_RESULT),
            expected,
            "sqrt({param:#X}) in mode {mode}"
        );
    }

    // A parameter with its top bit set is a large number, not a negative one. Reading it as
    // signed makes the root of anything above two billion zero.
    m.write32(reg::SQRTCNT, 0);
    m.write32(reg::SQRT_PARAM, 0x8000_0000);
    assert_eq!(m.read32(reg::SQRT_RESULT), 0xB504);
}

#[test]
fn the_busy_bit_always_reads_clear() {
    // Software spins on it. Hardware sets it for a few dozen cycles; both operations finish inside
    // the write that starts them here, so a spin has to exit on its first read or never.
    let mut m = unit();
    m.write32(reg::DIVCNT, BUSY as u32 | 2);
    assert_eq!(m.read32(reg::DIVCNT) as u16 & BUSY, 0);
    m.write32(reg::SQRTCNT, BUSY as u32 | 1);
    assert_eq!(m.read32(reg::SQRTCNT) as u16 & BUSY, 0);
}

#[test]
fn writing_the_mode_after_the_operands_still_divides() {
    // A driver writes the control register first as often as last, and hardware restarts on either.
    // Recomputing only when an operand changes leaves the answer from the previous mode in place.
    let mut m = unit();
    m.write32(reg::DIV_NUMERATOR, 100);
    m.write32(reg::DIV_DENOMINATOR, 7);
    m.write32(reg::DIVCNT, 0);
    assert_eq!(m.read32(reg::DIV_RESULT), 14);
}

#[test]
fn narrow_accesses_reach_the_operands() {
    // libnds writes `DIVCNT` as a halfword and the operands as words, and a driver that writes a
    // 64-bit operand as two halfwords must not have the first one divided against a stale second.
    let mut m = unit();
    m.write16(reg::DIVCNT, 2);
    assert_eq!(
        m.read16(reg::DIVCNT) & 3,
        2,
        "the mode came through a halfword write"
    );
    m.write16(reg::DIVCNT, 0);
    m.write32(reg::DIV_NUMERATOR, 0);
    m.write16(reg::DIV_NUMERATOR, 100);
    m.write32(reg::DIV_DENOMINATOR, 7);
    assert_eq!(m.read32(reg::DIV_RESULT), 14);
    m.write8(reg::DIV_DENOMINATOR, 5);
    assert_eq!(m.read32(reg::DIV_RESULT), 20);
}

#[test]
fn the_results_are_read_only() {
    let mut m = unit();
    m.write32(reg::DIV_NUMERATOR, 100);
    m.write32(reg::DIV_DENOMINATOR, 7);
    m.write32(reg::DIV_RESULT, 0xDEAD_BEEF);
    m.write32(reg::SQRT_RESULT, 0xDEAD_BEEF);
    assert_eq!(m.read32(reg::DIV_RESULT), 14);
    assert_eq!(m.read32(reg::SQRT_RESULT), 0);
}

#[test]
fn a_save_state_carries_the_operands_and_the_answers() {
    use savestate::{decode_state, encode_state};
    let mut m = unit();
    m.write32(reg::DIVCNT, 1);
    m.write32(reg::DIV_NUMERATOR, 0x0033_2000);
    m.write32(reg::DIV_DENOMINATOR, 0x2FB);
    m.write32(reg::SQRTCNT, 0);
    m.write32(reg::SQRT_PARAM, 10_000);
    let expected = m.read32(reg::DIV_RESULT);

    let blob = encode_state("nds", 1, &m);
    let mut restored = unit();
    decode_state("nds", 1, &blob, &mut restored).unwrap();
    assert_eq!(restored.read32(reg::DIV_RESULT), expected);
    assert_eq!(restored.read32(reg::SQRT_RESULT), 100);
    assert_eq!(restored, m);

    // And a machine that has not divided yet round-trips to itself rather than acquiring a
    // divide-by-zero flag it never earned — which is why the answers are saved rather than
    // recomputed from the operands.
    let fresh = unit();
    let blob = encode_state("nds", 1, &fresh);
    let mut restored = unit();
    decode_state("nds", 1, &blob, &mut restored).unwrap();
    assert_eq!(restored, fresh);
    assert_eq!(restored.read32(reg::DIVCNT) as u16 & DIV_BY_ZERO, 0);
}

#[test]
fn the_block_is_one_contiguous_range() {
    assert!(MathUnits::owns(reg::DIVCNT));
    assert!(MathUnits::owns(reg::SQRT_PARAM + 4));
    assert!(!MathUnits::owns(reg::DIVCNT - 4));
    assert!(!MathUnits::owns(reg::END));
}
