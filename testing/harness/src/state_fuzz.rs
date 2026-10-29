//! A damaged save state must be refused, not crash the machine that is loading it.
//!
//! For each system, a real state from a running machine is cut off at many offsets and has bytes
//! flipped at many more, and every result is fed to `System::load_state`. The properties:
//!
//! - **A truncated state is always `Err`.** Never `Ok` — a payload that runs out early has not
//!   restored the machine, whatever the reader says.
//! - **Nothing panics.** A panic inside `load_state` is survivable since the session catches it
//!   and flushes save RAM (`frontend-core`'s `a_panic_while_handling_a_command_still_flushes_dirty_
//!   save_ram`), but it still kills the session, and a file on disk should not be able to do that.
//! - **A header error leaves the machine untouched.** `StateError::is_pre_mutation` is what the
//!   frontend decides whether to reset on, so it has to be true to its name on every system, not
//!   only on the toy type `savestate` tests it with.
//!
//! A flipped byte inside the payload may well load: most bytes of a state are RAM, and any value is
//! a valid RAM value. That is `Ok`, and correct.
//!
//! Every case reloads the untouched state afterwards, so each starts from the same machine rather
//! than from whatever the previous case left half-written.

use core_common::{InputState, System};
use std::panic::{catch_unwind, AssertUnwindSafe};

/// A 32 KiB ROM-only cartridge that spins. `cgb` sets the header's colour flag.
fn gb_spin_rom(cgb: bool) -> Vec<u8> {
    let mut rom = vec![0u8; 0x8000];
    rom[0x0100..0x0102].copy_from_slice(&[0x18, 0xFE]); // jr -2
    rom[0x0134..0x0138].copy_from_slice(b"FUZZ");
    if cgb {
        rom[0x0143] = 0x80;
    }
    let mut checksum = 0u8;
    for byte in &rom[0x0134..0x014D] {
        checksum = checksum.wrapping_sub(*byte).wrapping_sub(1);
    }
    rom[0x014D] = checksum;
    rom
}

fn gba_spin_rom() -> Vec<u8> {
    let mut rom = vec![0u8; 0x1000];
    rom[0..4].copy_from_slice(&0xEAFF_FFFEu32.to_le_bytes()); // b .
    rom
}

/// Both cores spinning at their entry points; the header fields are the minimum direct boot reads.
fn nds_spin_rom() -> Vec<u8> {
    let mut rom = vec![0u8; 0x8000];
    rom[..12].copy_from_slice(b"FUZZ DS\0\0\0\0\0");
    let put = |rom: &mut Vec<u8>, at: usize, v: u32| {
        rom[at..at + 4].copy_from_slice(&v.to_le_bytes());
    };
    put(&mut rom, 0x20, 0x4000);
    put(&mut rom, 0x24, 0x0200_0000);
    put(&mut rom, 0x28, 0x0200_0000);
    put(&mut rom, 0x2C, 4);
    put(&mut rom, 0x30, 0x6000);
    put(&mut rom, 0x34, 0x0380_0000);
    put(&mut rom, 0x38, 0x0380_0000);
    put(&mut rom, 0x3C, 4);
    put(&mut rom, 0x4000, 0xEAFF_FFFE);
    put(&mut rom, 0x6000, 0xEAFF_FFFE);
    rom
}

/// `count` offsets spread evenly across `0..len`.
fn spread(len: usize, count: usize) -> impl Iterator<Item = usize> {
    (0..count).map(move |i| i * len / count)
}

/// Run every case against `system` and return what went wrong, one line per finding.
fn fuzz(system: &mut dyn System) -> Vec<String> {
    for _ in 0..10 {
        system.step_frame(InputState::default());
        system.take_audio_samples();
    }
    let good = system.save_state();
    let mut findings = Vec::new();

    // Every byte of the container header, which is a few dozen bytes at most, then a spread
    // through the payload and its very last byte.
    let cuts: Vec<usize> = (0..64.min(good.len()))
        .chain(spread(good.len(), 96))
        .chain([good.len() - 1])
        .collect();
    let flips: Vec<usize> = (0..32.min(good.len()))
        .chain(spread(good.len(), 128))
        .collect();

    let cases = cuts
        .iter()
        .map(|&cut| (format!("truncated to {cut}"), good[..cut].to_vec(), true))
        .chain(flips.iter().map(|&at| {
            let mut bytes = good.clone();
            bytes[at] ^= 0xFF;
            (format!("byte {at} flipped"), bytes, false)
        }));

    for (what, bytes, must_fail) in cases {
        match catch_unwind(AssertUnwindSafe(|| system.load_state(&bytes))) {
            Err(_) => findings.push(format!("{what}: load_state panicked")),
            Ok(Ok(())) if must_fail => findings.push(format!("{what}: loaded as if whole")),
            Ok(Ok(())) => {}
            Ok(Err(e)) if e.is_pre_mutation() => {
                if system.save_state() != good {
                    findings.push(format!(
                        "{what}: {e} claims pre-mutation but changed the machine"
                    ));
                }
            }
            Ok(Err(_)) => {}
        }
        if let Err(e) = system.load_state(&good) {
            findings.push(format!("{what}: the good state no longer reloads ({e})"));
            break;
        }
    }
    findings
}

fn assert_clean(name: &str, findings: Vec<String>) {
    assert!(
        findings.is_empty(),
        "{name}: {} finding(s):\n{}",
        findings.len(),
        findings.join("\n")
    );
}

#[test]
fn a_damaged_game_boy_state_is_refused_without_panicking() {
    let mut gb =
        system_gb::GbSystem::new(gb_spin_rom(false), None).expect("a hand-built cartridge");
    assert_clean("gb", fuzz(&mut gb));
}

#[test]
fn a_damaged_game_boy_color_state_is_refused_without_panicking() {
    let mut gbc =
        system_gbc::GbcSystem::new(gb_spin_rom(true), None).expect("a hand-built cartridge");
    assert_clean("gbc", fuzz(&mut gbc));
}

#[test]
fn a_damaged_game_boy_advance_state_is_refused_without_panicking() {
    let mut gba = system_gba::GbaSystem::new(gba_spin_rom(), None).expect("a hand-built cartridge");
    assert_clean("gba", fuzz(&mut gba));
}

#[test]
fn a_damaged_ds_state_is_refused_without_panicking() {
    let mut nds = system_nds::NdsSystem::default();
    nds.load_cartridge(&nds_spin_rom())
        .expect("a hand-built cartridge");
    assert_clean("nds", fuzz(&mut nds));
}
