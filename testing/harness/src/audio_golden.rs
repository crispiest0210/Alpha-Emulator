//! Audio regression goldens: a hash of a deterministic system's own sample output, pinned so a
//! change in that output is caught.
//!
//! # Not the same guarantee prompt 2's picture goldens will carry
//!
//! Prompt 2 designs a framebuffer golden manifest — `testing/golden/gba.toml`, a runner, a
//! failure artifact — whose every hash is validated against an independent reference (a real
//! emulator's screenshot) before being committed. **That infrastructure does not exist in this
//! repository yet**, and this module does not build it; extending something that is not there
//! would mean inventing it first, which is a larger, separate task.
//!
//! What this module has instead: the hashes below are computed once from this emulator's own
//! output on a small, deterministic, hand-built ROM, using channel math that Blargg's sound
//! suites and the register-level tests elsewhere in this workspace already exercise unit by
//! unit. They are **not validated against real hardware audio capture**. What that buys is
//! regression detection — a later change that silently alters the mix, exactly the class of bug
//! that shipped GBA direct sound as exact digital silence for two weeks with nothing catching
//! it — not a claim that the pinned value is correct against a real console. That gap is real
//! and is recorded here rather than hidden behind a passing test.
//!
//! [`an_all_silent_buffer_would_fail_both_goldens`] is the check that this has teeth: a hash
//! comparison with nothing behind it can pass on an emulator that produces silence just as
//! easily as one that does not, which is exactly how direct sound's two-week outage went
//! undetected. That test does not run a system at all — it hashes an explicitly all-zero buffer
//! and confirms neither golden's pinned hash matches it, so the two golden tests below cannot be
//! quietly satisfied by a machine that fell silent.

use crate::audio_hash;
use core_common::{InputState, System};

/// A DMG ROM that plays channel 1, full volume, no envelope movement, for a fixed number of
/// frames — deterministic because nothing about it depends on timing outside the emulator's own
/// clock, and it never reads a register back, so nothing about the run depends on execution
/// speed either.
fn gb_tone_rom() -> Vec<u8> {
    let mut rom = vec![0u8; 0x8000];
    let code: &[u8] = &[
        0x3E, 0x80, //  ld a, $80
        0xE0, 0x26, //  ldh ($26), a   ; NR52: APU on
        0x3E, 0xFF, //  ld a, $FF
        0xE0, 0x25, //  ldh ($25), a   ; NR51: everything to both outputs
        0x3E, 0x77, //  ld a, $77
        0xE0, 0x24, //  ldh ($24), a   ; NR50: full volume
        0x3E, 0xF0, //  ld a, $F0
        0xE0, 0x12, //  ldh ($12), a   ; NR12: channel 1 envelope, full, no movement
        0x3E, 0x80, //  ld a, $80
        0xE0, 0x13, //  ldh ($13), a   ; NR13: frequency low byte
        0x3E, 0x87, //  ld a, $87
        0xE0, 0x14, //  ldh ($14), a   ; NR14: trigger, frequency high bits
        0x18, 0xFE, //  jr -2          ; spin
    ];
    rom[0x0100..0x0100 + code.len()].copy_from_slice(code);
    let title = b"AUDIOGOLD";
    rom[0x0134..0x0134 + title.len()].copy_from_slice(title);
    rom[0x0147] = 0x00; // ROM only
    rom[0x0148] = 0x00; // 32 KiB, matching the vector's length
    let mut checksum = 0u8;
    for byte in &rom[0x0134..0x014D] {
        checksum = checksum.wrapping_sub(*byte).wrapping_sub(1);
    }
    rom[0x014D] = checksum;
    rom
}

/// A GBA ROM that spins while its PSG channel 1 and direct-sound channel A both play, set up
/// directly through the bus rather than through ROM-resident code — the register writes
/// themselves are what this golden is pinning, not the ARM instructions that would issue them,
/// which every other GBA test in this workspace already covers.
fn gba_tone_rom() -> Vec<u8> {
    let mut rom = vec![0u8; 0x1000];
    // `b .`: a branch to the instruction's own address, so the CPU spins without needing valid
    // code beyond the entry point.
    rom[0..4].copy_from_slice(&0xEAFF_FFFEu32.to_le_bytes());
    rom
}

fn configure_gba_tone(bus: &mut system_gba::GbaSystemBus) {
    use core_common::Bus;
    bus.write16(system_gba::fifo::reg::SOUNDCNT_X, 1 << 7); // master enable
    bus.write16(
        system_gba::fifo::reg::SOUNDCNT_H,
        (1 << 2) | (1 << 8) | (1 << 9) | 2, // direct sound A full volume both sides, PSG full
    );
    bus.write16(system_gba::psg::reg::SOUNDCNT_L, 0xFF77); // PSG master volume, both sides
    bus.write16(system_gba::psg::reg::SOUND1CNT_H, 0xF080); // envelope full, no movement, duty 2
    bus.write16(system_gba::psg::reg::SOUND1CNT_X, (1 << 15) | 1500); // trigger
}

/// Run `frames` frames, draining audio as a real frontend does — a buffer nobody drains keeps
/// growing rather than reflecting steady state.
fn run_frames(system: &mut dyn System, frames: u32) -> Vec<core_common::AudioSample> {
    let mut collected = Vec::new();
    for _ in 0..frames {
        system.step_frame(InputState::default());
        collected.extend(system.take_audio_samples());
    }
    collected
}

/// Pinned by running [`gb_tone_rom`] once and reading off the result — see the module docs for
/// exactly what that provenance does and does not establish.
const GB_TONE_HASH: &str = "157051067015ad25";

/// Pinned the same way, from [`gba_tone_rom`] plus [`configure_gba_tone`].
const GBA_TONE_HASH: &str = "46b7cc07c984cfa5";

#[test]
fn a_game_boy_tone_hashes_to_the_pinned_value() {
    let mut gb = system_gb::GbSystem::new(gb_tone_rom(), None).expect("a hand-built cartridge");
    let samples = run_frames(&mut gb, 10);
    assert!(!samples.is_empty(), "no samples at all");
    assert!(
        samples.iter().any(|s| s.left != 0.0),
        "channel 1 was triggered and produced nothing"
    );
    assert_eq!(
        audio_hash(&samples),
        GB_TONE_HASH,
        "the Game Boy audio golden moved — if this is an intended change, recompute and update \
         GB_TONE_HASH, and say why in the commit"
    );
}

#[test]
fn a_gba_tone_hashes_to_the_pinned_value() {
    let mut gba = system_gba::GbaSystem::new(gba_tone_rom(), None).expect("a hand-built cartridge");
    configure_gba_tone(gba.bus_mut());
    let samples = run_frames(&mut gba, 10);
    assert!(!samples.is_empty(), "no samples at all");
    assert!(
        samples.iter().any(|s| s.left != 0.0),
        "the PSG and direct sound were both armed and produced nothing"
    );
    assert_eq!(
        audio_hash(&samples),
        GBA_TONE_HASH,
        "the GBA audio golden moved — if this is an intended change, recompute and update \
         GBA_TONE_HASH, and say why in the commit"
    );
}

#[test]
fn an_all_silent_buffer_would_fail_both_goldens() {
    // The negative control the module docs promise: a hash check with nothing behind it passes
    // on silence exactly as readily as on real output, which is how GBA direct sound shipped
    // producing exact digital silence for two weeks with every existing test still green. This
    // proves the two pinned hashes above are not that — an emulator that fell silent would not
    // quietly satisfy them.
    let silent = vec![
        core_common::AudioSample {
            left: 0.0,
            right: 0.0
        };
        4096
    ];
    let silent_hash = audio_hash(&silent);
    assert_ne!(
        silent_hash, GB_TONE_HASH,
        "an all-silent buffer must not pass the GB golden"
    );
    assert_ne!(
        silent_hash, GBA_TONE_HASH,
        "an all-silent buffer must not pass the GBA golden"
    );
}
