//! High-level emulation of the BIOS calls both cores reach through `SWI`.
//!
//! # Why this exists at all
//!
//! This crate vendors no BIOS image, and without one a `SWI` takes the ordinary ARMv4 exception
//! path to `exception_base + 8` — `0xFFFF_0008` on the ARM9, `0x0000_0008` on the ARM7. Both are
//! inside the BIOS region, both read open bus, and open bus is zero, which decodes as
//! `andeq r0, r0, r0`. A core that calls a BIOS function therefore executes nothing, forever, at
//! full speed.
//!
//! That is not a corner case. `swiWaitForVBlank`, `swiDivide`, `swiCopy` and
//! `swiDecompressLZSSVram` appear in the first thousand instructions of essentially every libnds
//! homebrew and every retail game, so before this module the DS could parse a header, load two
//! binaries, and start two cores — and then run no software at all.
//!
//! # This is `system-gba`'s `bios` module, with the DS's differences made explicit
//!
//! The shape is deliberately the same: an enum numbered as the hardware numbers it, one `dispatch`
//! that takes the core and the bus, and a rule that an unimplemented call *logs and returns*
//! rather than guessing. Four things genuinely differ:
//!
//! - **The call numbers are not the GBA's.** `Div` is `SWI 0x09` here and `SWI 0x06` there;
//!   `Sqrt` is `0x0D` here and `0x08` there. `SWI 0x06` on a DS is `Halt`. Carrying the GBA table
//!   over compiles perfectly and produces a machine that halts where a game asked to divide,
//!   which is why [`BiosCall::from_comment`] takes the numbers from the DS's own table and the
//!   tests below assert the two most confusable entries by number.
//! - **There are two tables**, because the two cores do not have the same calls. The ARM7 has the
//!   sleep and sound entries; the ARM9 has the two difference filters. A call made on the core
//!   that lacks it is [`BiosCall::Unhandled`], and is logged as such rather than quietly working.
//! - **`IntrWait` is answered against a flag word in memory**, not by halting once. See
//!   [`Context`].
//! - **The `ReadByCallback` decompression entries ignore their callback structure**, because the
//!   callbacks libnds supplies do exactly what a direct read does. See `unpack` below.
//!
//! # The bar is behavioural accuracy, not "usually works"
//!
//! Each call below implements the documented contract exactly, including the parts that look like
//! mistakes: `Div` returns three results and truncates toward zero, `CpuSet`'s length counts
//! *units* chosen by a bit in a different field, and `Sqrt` is an integer square root so callers
//! scale their input beforehand.

use crate::Core;
use core_common::Bus;
use cpu_arm7tdmi::Arm7Tdmi;

/// The calls this module answers, numbered as the hardware numbers them.
///
/// A trace showing `SWI 0x09` maps to `Div` without a lookup — and, just as importantly, a reader
/// who knows the GBA's numbers can see at a glance that they are not these.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BiosCall {
    SoftReset,
    /// A calibrated delay loop. Returns immediately here; see [`dispatch`].
    WaitByLoop,
    IntrWait,
    VBlankIntrWait,
    Halt,
    /// ARM7 only. Powers the machine down until a wake-up source fires.
    Sleep,
    /// ARM7 only. Ramps `SOUNDBIAS` to the level the amplifier expects.
    SoundBias,
    Div,
    CpuSet,
    CpuFastSet,
    Sqrt,
    GetCrc16,
    IsDebugger,
    /// Expand a packed bit-depth image. Not implemented; see [`dispatch`].
    BitUnPack,
    /// The decompressors, in the two destination flavours each has.
    ///
    /// Split by destination because a byte write to VRAM is dropped on the ARM9: the `Vram`
    /// variants must emit halfwords or nothing arrives at all. That distinction is the whole
    /// reason the hardware has two of each.
    Lz77UnCompWram,
    Lz77UnCompVram,
    HuffUnComp,
    RlUnCompWram,
    RlUnCompVram,
    /// ARM9 only.
    Diff8bitUnFilterWram,
    /// ARM9 only.
    Diff16bitUnFilter,
    /// ARM7 only. `HALTCNT` written from `r2`, so it can select sleep as well as halt.
    CustomHalt,
    /// ARM7 only. The three sound tables the BIOS keeps in ROM. Not implemented; see [`dispatch`].
    GetSineTable,
    GetPitchTable,
    GetVolumeTable,
    Unhandled(u8),
}

impl BiosCall {
    /// Decode a `SWI` comment field for one core.
    ///
    /// Two tables rather than one, because a call the other core owns must come out as
    /// [`BiosCall::Unhandled`] and be logged. Answering an ARM7 sound call on the ARM9 would be a
    /// plausible wrong answer, which is exactly the failure this module is written to avoid.
    pub fn from_comment(core: Core, comment: u8) -> Self {
        // Everything both cores have. Note 0x06: on a GBA this number is `Div`.
        let shared = match comment {
            0x00 => Some(BiosCall::SoftReset),
            0x03 => Some(BiosCall::WaitByLoop),
            0x04 => Some(BiosCall::IntrWait),
            0x05 => Some(BiosCall::VBlankIntrWait),
            0x06 => Some(BiosCall::Halt),
            0x09 => Some(BiosCall::Div),
            0x0B => Some(BiosCall::CpuSet),
            0x0C => Some(BiosCall::CpuFastSet),
            0x0D => Some(BiosCall::Sqrt),
            0x0E => Some(BiosCall::GetCrc16),
            0x0F => Some(BiosCall::IsDebugger),
            0x10 => Some(BiosCall::BitUnPack),
            0x11 => Some(BiosCall::Lz77UnCompWram),
            0x12 => Some(BiosCall::Lz77UnCompVram),
            0x13 => Some(BiosCall::HuffUnComp),
            0x14 => Some(BiosCall::RlUnCompWram),
            0x15 => Some(BiosCall::RlUnCompVram),
            _ => None,
        };
        if let Some(call) = shared {
            return call;
        }
        let core_specific = match core {
            // The two difference filters are ARM9-only: the ARM7's BIOS has no entry for them.
            Core::Arm9 => match comment {
                0x16 => Some(BiosCall::Diff8bitUnFilterWram),
                0x18 => Some(BiosCall::Diff16bitUnFilter),
                _ => None,
            },
            Core::Arm7 => match comment {
                0x07 => Some(BiosCall::Sleep),
                0x08 => Some(BiosCall::SoundBias),
                // The three sound tables, and they are 0x1A-0x1C rather than the 0x20-0x22 that
                // was here before. Pokemon Platinum's own SWI thunk table settles it: `svc #0x1a`,
                // `svc #0x1b`, `svc #0x1c` sit immediately after the decompression thunks, and its
                // sound driver calls the last two thousands of times a second — with arguments in
                // exactly the ranges those two tables are indexed by. At 0x20-0x22 they arrived as
                // `Unhandled` and answered nothing.
                0x1A => Some(BiosCall::GetSineTable),
                0x1B => Some(BiosCall::GetPitchTable),
                0x1C => Some(BiosCall::GetVolumeTable),
                0x1F => Some(BiosCall::CustomHalt),
                _ => None,
            },
        };
        core_specific.unwrap_or(BiosCall::Unhandled(comment))
    }
}

/// What the caller must do after the call, beyond returning.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BiosEffect {
    /// The core should stop until an interrupt arrives.
    pub halt: bool,
    /// The program counter must be left *on* the `SWI` so the call runs again when the core wakes.
    ///
    /// Only [`BiosCall::IntrWait`] and [`BiosCall::VBlankIntrWait`] set this, and only while their
    /// condition is still unmet. See [`Context`] for why the wait is a loop rather than one halt.
    pub repeat: bool,
    /// Extra cycles this call cost, on top of the flat charge every intercepted call pays.
    ///
    /// Almost every call leaves this at zero: the flat charge is a guess and refining it per call
    /// would be a guess with more digits. [`BiosCall::WaitByLoop`] is the exception, and it is not
    /// a refinement — it is the whole meaning of the call. See there.
    pub extra_cycles: u32,
}

/// The per-core state `IntrWait` needs, which is the one thing the two cores keep in different
/// places.
///
/// # Why `IntrWait` is not just a halt
///
/// The GBA's HLE halts once and returns, and for a game whose only interrupt is vertical blank
/// that is indistinguishable from the real thing. It is not good enough here. A DS runs two cores
/// that talk over an interrupt-driven FIFO, so the ARM9 sitting in `swiWaitForVBlank` is woken
/// several times per frame by IPC interrupts that are not what it asked for. Returning on those
/// makes a game's main loop run many times per frame instead of once — visible as animation
/// running at the wrong speed, and as a game that consumes input faster than it is given.
///
/// So the call is answered the way the hardware answers it, against a word of memory that the
/// game's own interrupt handler updates:
///
/// - On entry, if `r0` is non-zero, the bits named by `r1` are cleared from the flag word. That is
///   what "wait for a *new* interrupt" means, and it happens exactly once per call.
/// - The flag word is then tested. If any awaited bit is set it is cleared and the call returns.
/// - Otherwise the core halts with the program counter still on the `SWI`, so waking re-runs the
///   test. The BIOS's own loop is `halt; test; repeat`, and this is that loop.
///
/// [`waiting`](Self::waiting) is what keeps the re-run from discarding the flag the handler has
/// just set: it records that this particular call has already done its one discard. It cannot be
/// inferred from `r0`, because `VBlankIntrWait` is defined as `IntrWait(1, 1)` and re-executing it
/// sets `r0` back to 1 every time.
pub struct Context<'a> {
    pub core: Core,
    /// Where this core's BIOS keeps the word `IntrWait` tests.
    ///
    /// `0x0380_FFF8` for the ARM7, at the top of its private work RAM. For the ARM9 it is
    /// `DTCM + 0x3FF8` — wherever CP15 has put DTCM, which is why this is passed in rather than
    /// being a constant. libnds calls it `__irq_flags`, and its interrupt handler ORs each
    /// acknowledged source into it.
    pub flags: u32,
    /// Whether the `IntrWait` at the program counter has already performed its one discard.
    pub waiting: &'a mut bool,
}

/// Perform a BIOS call in place of the ROM.
///
/// `bus` must be the view the calling core actually sees — for the ARM9 that means TCM spliced in
/// front, since `IntrWait`'s flag word and most of a libnds program's stack live in DTCM and never
/// reach the bus at all.
pub fn dispatch<B: Bus + ?Sized>(
    cpu: &mut Arm7Tdmi,
    bus: &mut B,
    comment: u8,
    ctx: Context<'_>,
) -> BiosEffect {
    let mut effect = BiosEffect::default();
    let call = BiosCall::from_comment(ctx.core, comment);

    match call {
        BiosCall::Div => divide(cpu, cpu.reg(0) as i32, cpu.reg(1) as i32),
        BiosCall::Sqrt => {
            // An *integer* square root: callers that want fractional precision scale their input
            // up first and scale the result back down themselves.
            cpu.set_reg(0, cpu.reg(0).isqrt());
        }
        BiosCall::CpuSet => cpu_set(cpu, bus, false),
        BiosCall::CpuFastSet => cpu_set(cpu, bus, true),
        BiosCall::GetCrc16 => crc16_call(cpu, bus),
        BiosCall::IsDebugger => {
            // Zero means a retail unit. Answering "debugger" sends libnds down a path that expects
            // the extra 4 MiB of RAM a development unit has and this machine does not.
            cpu.set_reg(0, 0);
        }
        BiosCall::Halt => effect.halt = true,
        BiosCall::CustomHalt => {
            // `r2` is the `HALTCNT` value, which selects halt or sleep. Writing it through the bus
            // rather than acting on it here means one place decides what each encoding does.
            bus.write8(0x0400_0301, cpu.reg(2) as u8);
        }
        BiosCall::IntrWait => {
            let discard = cpu.reg(0) != 0;
            intr_wait(bus, &mut effect, ctx, discard, cpu.reg(1));
        }
        // Defined by the hardware as `IntrWait(1, 1)`: discard whatever is already flagged, then
        // wait for a *new* vertical blank.
        BiosCall::VBlankIntrWait => intr_wait(bus, &mut effect, ctx, true, 1),
        BiosCall::WaitByLoop => {
            // A delay of `r0` iterations, and the delay is the entire point of the call — so it
            // has to cost the time it asks for rather than returning at once.
            //
            // The BIOS loop is `SUBS r0, r0, #1` / `BGT`, four cycles an iteration, and it is how
            // DS software spells "wait for the *other* core to notice what I just wrote". Pokemon
            // Platinum's ARM7 boot handshake is exactly that: write a nibble to `IPCSYNC`,
            // `SWI 3` for a thousand cycles, then read back what the ARM9 echoed. A call that
            // returns instantly reads the register back before the other core has run a single
            // instruction, the compare fails every round, and both cores handshake forever at a
            // white screen — with no BIOS call failing, no register wrong, and nothing in any
            // unit test to say so.
            //
            // Charging the time cannot make a machine slower than hardware here, because
            // [`Self::extra_cycles`] is spent by the caller's own slice, which is what the loop
            // would have spent anyway.
            effect.extra_cycles = cpu.reg(0).saturating_mul(4);
            cpu.set_reg(0, 0);
        }
        BiosCall::SoundBias => {
            // `r0` selects the target level and `r1` the number of steps to ramp over. The ramp is
            // not modelled: it exists so the amplifier does not click, which is an artefact of the
            // analogue output rather than of anything a game can observe.
            let target = if cpu.reg(0) != 0 { 0x200 } else { 0x000 };
            bus.write32(0x0400_0504, target);
        }
        BiosCall::Lz77UnCompWram => unpack(cpu, bus, Decoder::Lz77, false),
        BiosCall::Lz77UnCompVram => unpack(cpu, bus, Decoder::Lz77, true),
        BiosCall::RlUnCompWram => unpack(cpu, bus, Decoder::RunLength, false),
        BiosCall::RlUnCompVram => unpack(cpu, bus, Decoder::RunLength, true),
        BiosCall::HuffUnComp => unpack(cpu, bus, Decoder::Huffman, true),
        BiosCall::Diff8bitUnFilterWram => unpack(cpu, bus, Decoder::Diff8, false),
        BiosCall::Diff16bitUnFilter => unpack(cpu, bus, Decoder::Diff16, true),
        // A reboot back into the firmware menu. Nothing here can present a firmware menu, and
        // pretending to reset would leave a game staring at its own uninitialised state.
        BiosCall::SoftReset => {
            tracing::warn!(core = ctx.core.name(), "SoftReset is not implemented");
        }
        // Doing nothing leaves the caller's registers unchanged, which shows up in a trace.
        // Guessing would produce a plausible wrong answer that surfaces far from its cause.
        //
        // But it is *logged*. A call that silently does nothing is exactly how a GBA game ran at
        // full speed with a black screen for a whole session: every graphic it owned was
        // LZ77-compressed, the decompressor was missing, and nothing said so.
        BiosCall::GetPitchTable => cpu.set_reg(0, pitch_table(cpu.reg(0)) as u32),
        BiosCall::GetVolumeTable => cpu.set_reg(0, volume_table(cpu.reg(0)) as u32),
        BiosCall::BitUnPack
        | BiosCall::Sleep
        // The sine table is the one of the three that could not be reconstructed: see
        // [`pitch_table`] for what made the other two knowable, and why guessing this one's
        // amplitude would be worse than answering nothing.
        | BiosCall::GetSineTable
        | BiosCall::Unhandled(_) => {
            tracing::warn!(
                core = ctx.core.name(),
                "unimplemented BIOS call SWI {comment:#04X} ({call:?}); it did nothing, and \
                 whatever the caller expected it to produce is missing"
            );
        }
    }
    effect
}

/// `IntrWait` and `VBlankIntrWait`, against the flag word the game's own handler maintains.
///
/// See [`Context`] for why this is a loop rather than a single halt, and for what `waiting`
/// protects against.
fn intr_wait<B: Bus + ?Sized>(
    bus: &mut B,
    effect: &mut BiosEffect,
    ctx: Context<'_>,
    discard: bool,
    mask: u32,
) {
    if !*ctx.waiting {
        *ctx.waiting = true;
        if discard {
            let flags = bus.read32(ctx.flags);
            bus.write32(ctx.flags, flags & !mask);
        }
    }

    let flags = bus.read32(ctx.flags);
    let matched = flags & mask;
    if matched != 0 {
        // Acknowledge only what was awaited. A handler may have set several bits, and clearing the
        // whole word would lose the ones another `IntrWait` is about to ask for.
        bus.write32(ctx.flags, flags & !matched);
        *ctx.waiting = false;
        return;
    }

    effect.halt = true;
    effect.repeat = true;
}

/// `Div`: quotient in `r0`, remainder in `r1`, absolute quotient in `r3`.
///
/// Truncates toward zero rather than flooring, so `-7 / 2` is `-3` with remainder `-1` and not
/// `-4` with remainder `1`. The remainder takes the sign of the *dividend*, which is what Rust's
/// `%` already does — but it is worth stating, because the other convention is common enough that
/// a reader may assume it.
fn divide(cpu: &mut Arm7Tdmi, numerator: i32, denominator: i32) {
    if denominator == 0 {
        // Hardware hangs here. Returning leaves the registers alone, which is a debuggable outcome
        // rather than an emulator that stops responding.
        return;
    }
    // `i32::MIN / -1` overflows; the hardware wraps, so this does too.
    let quotient = numerator.wrapping_div(denominator);
    let remainder = numerator.wrapping_rem(denominator);
    cpu.set_reg(0, quotient as u32);
    cpu.set_reg(1, remainder as u32);
    cpu.set_reg(3, quotient.unsigned_abs());
}

/// `CpuSet` and `CpuFastSet`: copy or fill memory.
///
/// `r2` is not a byte count. Its low 21 bits are a count of *units*, and bit 26 chooses whether a
/// unit is a halfword or a word — so the same value means different amounts depending on a bit in
/// a different field. Bit 24 switches from copying to filling, where the source is read once and
/// written repeatedly.
///
/// `CpuFastSet` is word-only and works in blocks of eight, but the observable result is the same
/// as `CpuSet` with the word bit set, so they share this.
fn cpu_set<B: Bus + ?Sized>(cpu: &mut Arm7Tdmi, bus: &mut B, fast: bool) {
    let source = cpu.reg(0);
    let destination = cpu.reg(1);
    let control = cpu.reg(2);

    let count = control & 0x1F_FFFF;
    let fill = control & (1 << 24) != 0;
    let words = fast || control & (1 << 26) != 0;

    if words {
        let value = bus.read32(source & !3);
        for index in 0..count {
            let word = if fill {
                value
            } else {
                bus.read32((source & !3).wrapping_add(index * 4))
            };
            bus.write32((destination & !3).wrapping_add(index * 4), word);
        }
    } else {
        let value = bus.read16(source & !1);
        for index in 0..count {
            let half = if fill {
                value
            } else {
                bus.read16((source & !1).wrapping_add(index * 2))
            };
            bus.write16((destination & !1).wrapping_add(index * 2), half);
        }
    }
}

/// `GetCRC16`: `r0` is the running value, `r1` the data, `r2` the length in bytes.
///
/// This is CRC-16/ARC — reflected, polynomial `0xA001` — which is the algorithm that produces the
/// checksums in a `.nds` header, so it can be checked against a real cartridge rather than only
/// against itself. Software calls it with an initial value of `0xFFFF`; the parameter exists so a
/// checksum can be continued across several buffers.
fn crc16_call<B: Bus + ?Sized>(cpu: &mut Arm7Tdmi, bus: &mut B) {
    let mut crc = cpu.reg(0) as u16;
    let address = cpu.reg(1);
    let length = cpu.reg(2);
    for offset in 0..length {
        crc = crc16_step(crc, bus.read8(address.wrapping_add(offset)));
    }
    cpu.set_reg(0, crc as u32);
}

/// `GetPitchTable`: the frequency multiplier for a pitch offset, in 1/64ths of a semitone.
///
/// # These two tables are computed, not dumped
///
/// They live in the ARM7 BIOS, which this project does not vendor, so the numbers here are
/// reconstructed. That is only defensible because the caller's own arithmetic says what they have
/// to be, and Pokemon Platinum's sound driver is sitting in memory saying it:
///
/// - The driver adds `0x10000` to what this returns and multiplies a timer reload by the result,
///   normalising a pitch offset into `0..0x300` — 768, twelve semitones of sixty-four steps —
///   and counting the octaves it removed as a shift. So the table is `2^(index/768)` in 16.16,
///   less the one that the `+0x10000` puts back.
/// - For [`volume_table`], the driver indexes it with `attenuation + 723` and separately picks the
///   hardware's volume divider at `attenuation < -60`, `< -120`, and `< -240`. Those three
///   thresholds are the divider's own steps — halve, quarter, sixteenth, which is -6.02, -12.04
///   and -24.08 decibels — so the units are tenths of a decibel, the table spans -72.3 dB to 0,
///   and it holds the 7-bit volume that is *left* once the divider has taken its share.
///
/// Getting the last bit of either wrong is inaudible: one step of the volume table is 0.07 dB and
/// one of the pitch table is 1/64th of a semitone. Getting them *absent* is not, which is what was
/// happening — they were mapped to the wrong `SWI` numbers, arrived as unhandled, and answered
/// zero, which is silence and no pitch at all.
///
/// The third table, `GetSineTable`, is deliberately still unanswered. Nothing in reach says what
/// amplitude its entries are scaled to, and a vibrato depth wrong by a factor of two hundred is a
/// worse thing to ship than a vibrato that is missing. It is logged instead.
fn pitch_table(index: u32) -> u16 {
    // 768 entries; hardware's table has no more, and the driver never asks past it.
    let index = index.min(767) as f64;
    let multiplier = (index / 768.0).exp2();
    // Less the one the driver's own `+0x10000` puts back, which is why an index of zero — no
    // pitch change at all — is stored as zero rather than as unity.
    let scaled = (multiplier * 65536.0).round() as u32;
    scaled.saturating_sub(65536) as u16
}

/// `GetVolumeTable`: the 7-bit channel volume for an attenuation in tenths of a decibel.
///
/// See [`pitch_table`] for where the shape of this comes from.
fn volume_table(index: u32) -> u8 {
    // 724 entries, indexed by `attenuation + 723`, so index 723 is no attenuation at all.
    let index = index.min(723) as i32;
    let tenths_of_a_decibel = (index - 723) as f64;
    // What the hardware's volume divider will already have taken off, at the same three thresholds
    // the driver switches it on.
    let divider = match tenths_of_a_decibel {
        t if t < -240.0 => 16.0,
        t if t < -120.0 => 4.0,
        t if t < -60.0 => 2.0,
        _ => 1.0,
    };
    let gain = 10f64.powf(tenths_of_a_decibel / 200.0);
    (gain * divider * 127.0).round().clamp(0.0, 127.0) as u8
}

/// The same checksum over a slice, for the parts of the machine that have to produce blocks
/// software will hand to [`BiosCall::GetCrc16`] and expect to pass.
///
/// [`crate::firmware`] is the one that needs it: a fabricated settings block whose checksum was
/// computed by a second implementation of this would be one refactor away from being rejected by
/// the console it was fabricated for.
pub fn crc16(initial: u16, bytes: &[u8]) -> u16 {
    bytes
        .iter()
        .fold(initial, |crc, byte| crc16_step(crc, *byte))
}

fn crc16_step(crc: u16, byte: u8) -> u16 {
    let mut crc = crc ^ byte as u16;
    for _ in 0..8 {
        let carry = crc & 1 != 0;
        crc >>= 1;
        if carry {
            crc ^= 0xA001;
        }
    }
    crc
}

/// Which compressed format a call is unpacking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Decoder {
    Lz77,
    RunLength,
    Huffman,
    Diff8,
    Diff16,
}

/// The shared shape of every decompression call: read a header, decode into a buffer, write it
/// out.
///
/// # The callback structure is ignored, deliberately
///
/// Three of these entries are documented as `ReadByCallback`: `r3` points at a structure of
/// function pointers the BIOS calls to fetch each byte. libnds fills that structure with functions
/// that do exactly what a direct read does — `return *source` — so reading directly produces the
/// same bytes without needing to re-enter the CPU from inside a BIOS call. Software that supplies
/// a callback which *transforms* the stream would get the untransformed stream instead; nothing in
/// libnds or in any DS toolchain does that, and the alternative is a re-entrant interpreter.
///
/// # Why a buffer and not a stream
///
/// The `Vram` variants must write halfwords, because a byte write to VRAM is dropped by the ARM9's
/// bus — so a decompressor that emitted bytes would write nothing at all. Decoding into a `Vec`
/// first and writing it out at the destination's natural width makes that one decision instead of
/// five, and lets the back-reference in LZ77 read bytes it has already produced without reading
/// them back off the bus.
fn unpack<B: Bus + ?Sized>(cpu: &mut Arm7Tdmi, bus: &mut B, decoder: Decoder, vram: bool) {
    let source = cpu.reg(0);
    let destination = cpu.reg(1);
    let header = bus.read32(source);
    let size = (header >> 8) as usize;

    // A malformed or wildly out-of-range header is a game bug or a mis-set pointer, and allocating
    // from it would turn that into an out-of-memory. Main RAM is 4 MiB, so nothing decompressed on
    // a DS can legitimately be larger than that.
    const LIMIT: usize = 0x0040_0000;
    if size == 0 || size > LIMIT {
        tracing::warn!("BIOS decompression header claims {size} bytes; refusing");
        return;
    }

    let mut out = Vec::with_capacity(size);
    match decoder {
        Decoder::Lz77 => decode_lz77(bus, source + 4, size, &mut out),
        Decoder::RunLength => decode_run_length(bus, source + 4, size, &mut out),
        Decoder::Huffman => decode_huffman(bus, source, header, size, &mut out),
        Decoder::Diff8 => decode_diff8(bus, source + 4, size, &mut out),
        Decoder::Diff16 => decode_diff16(bus, source + 4, size, &mut out),
    }
    out.truncate(size);

    if vram {
        // Pad to a whole halfword rather than dropping the odd byte at the end.
        if out.len() % 2 == 1 {
            out.push(0);
        }
        for (index, pair) in out.chunks_exact(2).enumerate() {
            bus.write16(
                destination.wrapping_add(index as u32 * 2),
                u16::from_le_bytes([pair[0], pair[1]]),
            );
        }
    } else {
        for (index, byte) in out.iter().enumerate() {
            bus.write8(destination.wrapping_add(index as u32), *byte);
        }
    }
}

/// LZ77: a flag byte then eight blocks, each a literal byte or a back-reference.
fn decode_lz77<B: Bus + ?Sized>(bus: &mut B, mut source: u32, size: usize, out: &mut Vec<u8>) {
    while out.len() < size {
        let flags = bus.read8(source);
        source += 1;
        for bit in 0..8 {
            if out.len() >= size {
                return;
            }
            if flags & (0x80 >> bit) == 0 {
                out.push(bus.read8(source));
                source += 1;
                continue;
            }
            let first = bus.read8(source) as usize;
            let second = bus.read8(source + 1) as usize;
            source += 2;
            let length = (first >> 4) + 3;
            // The displacement is stored one less than it is, so a value of zero means the byte
            // immediately before the write position rather than the write position itself.
            let distance = (((first & 0x0F) << 8) | second) + 1;
            if distance > out.len() {
                // A back-reference past the start of the output is a corrupt stream. Stopping
                // leaves a short buffer, which shows up as missing graphics rather than a panic.
                tracing::warn!("LZ77 back-reference of {distance} past the start of the output");
                return;
            }
            let from = out.len() - distance;
            for offset in 0..length {
                if out.len() >= size {
                    return;
                }
                // Read one byte at a time and push as we go: an overlapping copy is legal and is
                // how the format encodes a run, so the source may include bytes this loop writes.
                out.push(out[from + offset]);
            }
        }
    }
}

/// Run-length: a flag byte whose top bit picks a literal span or a repeated byte.
fn decode_run_length<B: Bus + ?Sized>(
    bus: &mut B,
    mut source: u32,
    size: usize,
    out: &mut Vec<u8>,
) {
    while out.len() < size {
        let flag = bus.read8(source);
        source += 1;
        if flag & 0x80 == 0 {
            let count = (flag & 0x7F) as usize + 1;
            for _ in 0..count {
                if out.len() >= size {
                    return;
                }
                out.push(bus.read8(source));
                source += 1;
            }
        } else {
            // The repeated form's count is biased by three, not one — the two forms do not share a
            // bias, and using one for both produces output that is subtly the wrong length.
            let count = (flag & 0x7F) as usize + 3;
            let byte = bus.read8(source);
            source += 1;
            for _ in 0..count {
                if out.len() >= size {
                    return;
                }
                out.push(byte);
            }
        }
    }
}

/// Difference filters: each unit is stored as its difference from the one before.
fn decode_diff8<B: Bus + ?Sized>(bus: &mut B, source: u32, size: usize, out: &mut Vec<u8>) {
    let mut value = 0u8;
    for index in 0..size {
        value = value.wrapping_add(bus.read8(source.wrapping_add(index as u32)));
        out.push(value);
    }
}

fn decode_diff16<B: Bus + ?Sized>(bus: &mut B, source: u32, size: usize, out: &mut Vec<u8>) {
    let mut value = 0u16;
    for index in 0..(size / 2) {
        value = value.wrapping_add(bus.read16(source.wrapping_add(index as u32 * 2)));
        out.extend_from_slice(&value.to_le_bytes());
    }
}

/// Huffman: a tree table, then a bitstream read a word at a time, most significant bit first.
///
/// The tree's node layout is the awkward part. A node's two children sit at an offset derived from
/// the node's *own* address rounded down to even, plus its low six bits — so the tree cannot be
/// walked as a flat array, and reading it as one produces plausible output that is wrong from the
/// first symbol.
fn decode_huffman<B: Bus + ?Sized>(
    bus: &mut B,
    source: u32,
    header: u32,
    size: usize,
    out: &mut Vec<u8>,
) {
    let symbol_bits = header & 0x0F;
    if symbol_bits != 4 && symbol_bits != 8 {
        tracing::warn!("Huffman stream claims {symbol_bits}-bit symbols; only 4 and 8 exist");
        return;
    }

    // The tree begins immediately after the header word. Its first byte is its size, and the root
    // node is the byte after that.
    let tree_start = source + 4;
    let tree_bytes = (bus.read8(tree_start) as u32 + 1) * 2;
    let root = tree_start + 1;
    let mut stream = tree_start + tree_bytes;

    let mut position = root;
    let mut node = bus.read8(position);
    let mut window = bus.read32(stream);
    stream += 4;
    let mut remaining = 32u32;
    let mut pending_nibble: Option<u8> = None;

    // A corrupt tree can walk in a circle forever. Bound the work by what the output could
    // possibly need — a symbol takes at least one bit — rather than trusting the stream.
    let mut budget = size as u64 * 8 * 32 + 1024;

    while out.len() < size {
        budget -= 1;
        if budget == 0 {
            tracing::warn!("Huffman stream did not terminate; giving up part-way");
            return;
        }
        if remaining == 0 {
            window = bus.read32(stream);
            stream += 4;
            remaining = 32;
        }
        let bit = (window >> 31) & 1;
        window <<= 1;
        remaining -= 1;

        // A node's children sit at an offset from the node's own address rounded *down* to even,
        // not from the node itself.
        let child = (position & !1) + (node as u32 & 0x3F) * 2 + 2 + bit;
        let child_byte = bus.read8(child);
        // The parent carries a leaf flag for each child: bit 7 for the left, bit 6 for the right.
        // It is read from the parent before descending, which is why the flag is tested here
        // rather than after moving.
        let leaf = node & if bit == 0 { 0x80 } else { 0x40 } != 0;

        if leaf {
            if symbol_bits == 8 {
                out.push(child_byte);
            } else {
                // Four-bit symbols pack two to a byte, low nibble first.
                match pending_nibble.take() {
                    None => pending_nibble = Some(child_byte & 0x0F),
                    Some(low) => out.push(low | ((child_byte & 0x0F) << 4)),
                }
            }
            position = root;
            node = bus.read8(position);
        } else {
            position = child;
            node = child_byte;
        }
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn the_sound_tables_are_the_calls_the_arm7_actually_makes() {
        // They were at 0x20-0x22, where no DS BIOS has them. A game's sound driver calling
        // 0x1B thousands of times a second got `Unhandled` and a zero back.
        assert_eq!(
            BiosCall::from_comment(Core::Arm7, 0x1A),
            BiosCall::GetSineTable
        );
        assert_eq!(
            BiosCall::from_comment(Core::Arm7, 0x1B),
            BiosCall::GetPitchTable
        );
        assert_eq!(
            BiosCall::from_comment(Core::Arm7, 0x1C),
            BiosCall::GetVolumeTable
        );
        // And they are the ARM7's alone. The ARM9 has no sound hardware to have tables for.
        assert_eq!(
            BiosCall::from_comment(Core::Arm9, 0x1B),
            BiosCall::Unhandled(0x1B)
        );
    }

    #[test]
    fn the_pitch_table_doubles_over_an_octave() {
        // The driver adds 0x10000 to every entry, so this is `2^(index/768)` in 16.16 less one.
        // 768 steps is twelve semitones, so the far end has to be one octave up.
        assert_eq!(pitch_table(0), 0);
        // Half an octave up is the square root of two: 65536 * 1.41421 - 65536.
        assert_eq!(pitch_table(768 / 2), 27146);
        // One step short of a full octave, which would be exactly 0x10000 and wrap the u16.
        assert_eq!(pitch_table(767), 65418);
        // Indices past the table's end clamp rather than wrapping into a wildly wrong frequency.
        assert_eq!(pitch_table(10_000), pitch_table(767));
        // Monotonic, which is the property a pitch bend audibly depends on.
        for i in 1..768 {
            assert!(pitch_table(i) > pitch_table(i - 1), "at {i}");
        }
    }

    #[test]
    fn the_volume_table_hands_back_the_share_the_divider_did_not_take() {
        // Index 723 is no attenuation: full 7-bit volume, divider at one.
        assert_eq!(volume_table(723), 127);
        // Just below each of the driver's three divider thresholds the volume jumps back up,
        // because the divider has just taken a step and the table covers what is left. Without
        // that the level would drop twice at every threshold.
        for threshold in [60u32, 120, 240] {
            let above = volume_table(723 - threshold);
            let below = volume_table(723 - threshold - 1);
            assert!(
                below > above,
                "the divider steps at -{threshold} tenths of a decibel"
            );
        }
        // -6 dB is half the amplitude, which is what the top of the range must show.
        assert_eq!(volume_table(723 - 60), 64);
        assert_eq!(
            volume_table(0),
            0,
            "-72.3 dB is below one step of a 7-bit volume"
        );
        assert_eq!(volume_table(10_000), 127, "past the end clamps");
    }
    use super::*;
    use cpu_arm7tdmi::BootState;

    fn cpu() -> Arm7Tdmi {
        Arm7Tdmi::new(BootState::default())
    }

    /// Flat memory, so a test can assert on what a call moved without a memory map in the way.
    pub(super) struct FlatBus {
        bytes: Vec<u8>,
    }

    impl FlatBus {
        pub(super) fn new(size: usize) -> Self {
            Self {
                bytes: vec![0; size],
            }
        }

        fn with(at: u32, data: &[u8]) -> Self {
            let mut bus = FlatBus::new(0x4000);
            for (index, byte) in data.iter().enumerate() {
                bus.write8(at + index as u32, *byte);
            }
            bus
        }
    }

    /// A flat bus holding `data` at `at`, for the decompression tests next door.
    pub(super) fn flat_with(at: u32, data: &[u8]) -> FlatBus {
        FlatBus::with(at, data)
    }

    pub(super) fn read_bytes(bus: &mut FlatBus, at: u32, len: usize) -> Vec<u8> {
        (0..len).map(|i| bus.read8(at + i as u32)).collect()
    }

    /// Run a decompression call with its source and destination in the usual registers.
    pub(super) fn decompress(
        bus: &mut FlatBus,
        core: Core,
        comment: u8,
        source: u32,
        destination: u32,
    ) {
        let mut cpu = cpu();
        cpu.set_reg(0, source);
        cpu.set_reg(1, destination);
        run(&mut cpu, bus, core, comment);
    }

    impl core_common::Savable for FlatBus {
        fn save(&self, _w: &mut core_common::StateWriter) {}
        fn load(
            &mut self,
            _r: &mut core_common::StateReader,
        ) -> Result<(), core_common::StateError> {
            Ok(())
        }
    }

    impl Bus for FlatBus {
        fn read8(&mut self, addr: u32) -> u8 {
            self.bytes.get(addr as usize).copied().unwrap_or(0)
        }
        fn write8(&mut self, addr: u32, value: u8) {
            if let Some(slot) = self.bytes.get_mut(addr as usize) {
                *slot = value;
            }
        }
        fn open_bus8(&self, _addr: u32) -> u8 {
            0
        }
        fn read16(&mut self, addr: u32) -> u16 {
            core_common::compose_le_read16(self, addr)
        }
        fn read32(&mut self, addr: u32) -> u32 {
            core_common::compose_le_read32(self, addr)
        }
        fn write16(&mut self, addr: u32, value: u16) {
            core_common::compose_le_write16(self, addr, value)
        }
        fn write32(&mut self, addr: u32, value: u32) {
            core_common::compose_le_write32(self, addr, value)
        }
    }

    /// Run one call on one core, with a scratch flag word at `FLAGS`.
    const FLAGS: u32 = 0x0100;

    pub(super) fn run(
        cpu: &mut Arm7Tdmi,
        bus: &mut FlatBus,
        core: Core,
        comment: u8,
    ) -> BiosEffect {
        let mut waiting = false;
        dispatch(
            cpu,
            bus,
            comment,
            Context {
                core,
                flags: FLAGS,
                waiting: &mut waiting,
            },
        )
    }

    #[test]
    fn the_call_numbers_are_the_ds_table_and_not_the_gba_one() {
        // The single most expensive confusion available here. On a GBA, 0x06 is `Div` and 0x08 is
        // `Sqrt`; on a DS they are `Halt` and an ARM7-only sound call. Code carried over from
        // `system-gba` compiles perfectly and halts where a game asked to divide.
        assert_eq!(BiosCall::from_comment(Core::Arm9, 0x09), BiosCall::Div);
        assert_eq!(BiosCall::from_comment(Core::Arm9, 0x0D), BiosCall::Sqrt);
        assert_eq!(BiosCall::from_comment(Core::Arm9, 0x06), BiosCall::Halt);
        assert_eq!(
            BiosCall::from_comment(Core::Arm9, 0x08),
            BiosCall::Unhandled(0x08),
            "the GBA's Sqrt number is a sound call the ARM9 does not have"
        );
    }

    #[test]
    fn the_two_cores_have_different_tables() {
        // A call answered on the wrong core would be a plausible wrong answer, which is the whole
        // failure mode this module exists to avoid.
        assert_eq!(
            BiosCall::from_comment(Core::Arm7, 0x08),
            BiosCall::SoundBias
        );
        assert_eq!(BiosCall::from_comment(Core::Arm7, 0x07), BiosCall::Sleep);
        assert_eq!(
            BiosCall::from_comment(Core::Arm7, 0x1F),
            BiosCall::CustomHalt
        );
        assert_eq!(
            BiosCall::from_comment(Core::Arm9, 0x16),
            BiosCall::Diff8bitUnFilterWram
        );
        assert_eq!(
            BiosCall::from_comment(Core::Arm7, 0x16),
            BiosCall::Unhandled(0x16),
            "the difference filters are ARM9-only"
        );
        assert_eq!(
            BiosCall::from_comment(Core::Arm9, 0x1F),
            BiosCall::Unhandled(0x1F),
            "and the halt and sound calls are ARM7-only"
        );
        // Everything shared decodes the same on both.
        for comment in [0x00, 0x03, 0x04, 0x05, 0x06, 0x09, 0x0B, 0x0C, 0x0D, 0x0E] {
            assert_eq!(
                BiosCall::from_comment(Core::Arm9, comment),
                BiosCall::from_comment(Core::Arm7, comment),
                "SWI {comment:#04X}"
            );
        }
    }

    #[test]
    fn div_returns_quotient_remainder_and_absolute_quotient() {
        let mut cpu = cpu();
        let mut bus = FlatBus::new(0x200);
        cpu.set_reg(0, 100);
        cpu.set_reg(1, 7);
        run(&mut cpu, &mut bus, Core::Arm9, 0x09);
        assert_eq!(cpu.reg(0), 14);
        assert_eq!(cpu.reg(1), 2);
        assert_eq!(cpu.reg(3), 14);
    }

    #[test]
    fn div_truncates_toward_zero_rather_than_flooring() {
        // -7 / 2 is -3 remainder -1, not -4 remainder 1. Both conventions are common enough that
        // assuming is a real risk.
        let mut cpu = cpu();
        let mut bus = FlatBus::new(0x200);
        cpu.set_reg(0, (-7i32) as u32);
        cpu.set_reg(1, 2);
        run(&mut cpu, &mut bus, Core::Arm7, 0x09);
        assert_eq!(cpu.reg(0) as i32, -3);
        assert_eq!(cpu.reg(1) as i32, -1);
        assert_eq!(cpu.reg(3), 3, "the absolute quotient");
    }

    #[test]
    fn dividing_by_zero_leaves_the_registers_alone_rather_than_hanging() {
        let mut cpu = cpu();
        let mut bus = FlatBus::new(0x200);
        cpu.set_reg(0, 42);
        cpu.set_reg(1, 0);
        run(&mut cpu, &mut bus, Core::Arm9, 0x09);
        assert_eq!(cpu.reg(0), 42);
    }

    #[test]
    fn sqrt_is_an_integer_square_root() {
        let mut cpu = cpu();
        let mut bus = FlatBus::new(0x200);
        for (input, expected) in [
            (0u32, 0u32),
            (1, 1),
            (2, 1),
            (16, 4),
            (17, 4),
            (10_000, 100),
            (0xFFFF_FFFF, 0xFFFF),
        ] {
            cpu.set_reg(0, input);
            run(&mut cpu, &mut bus, Core::Arm9, 0x0D);
            assert_eq!(cpu.reg(0), expected, "sqrt({input})");
        }
    }

    #[test]
    fn cpu_set_counts_units_not_bytes() {
        let mut cpu = cpu();
        let mut bus = FlatBus::new(0x200);
        for index in 0..4u32 {
            bus.write16(index * 2, 0x1000 + index as u16);
        }
        cpu.set_reg(0, 0);
        cpu.set_reg(1, 0x40);
        cpu.set_reg(2, 4); // four halfwords
        run(&mut cpu, &mut bus, Core::Arm9, 0x0B);

        for index in 0..4u32 {
            assert_eq!(bus.read16(0x40 + index * 2), 0x1000 + index as u16);
        }
        assert_eq!(bus.read16(0x48), 0, "and it stopped after four");
    }

    #[test]
    fn the_word_and_fill_bits_change_what_a_unit_is_and_where_it_comes_from() {
        let mut cpu = cpu();
        let mut bus = FlatBus::new(0x200);
        bus.write32(0, 0xDEAD_BEEF);
        bus.write32(4, 0xCAFE_F00D);
        cpu.set_reg(0, 0);
        cpu.set_reg(1, 0x40);
        cpu.set_reg(2, 2 | (1 << 26));
        run(&mut cpu, &mut bus, Core::Arm9, 0x0B);
        assert_eq!(bus.read32(0x40), 0xDEAD_BEEF);
        assert_eq!(bus.read32(0x44), 0xCAFE_F00D);

        cpu.set_reg(2, 3 | (1 << 24) | (1 << 26));
        cpu.set_reg(1, 0x80);
        run(&mut cpu, &mut bus, Core::Arm9, 0x0B);
        for index in 0..3u32 {
            assert_eq!(
                bus.read32(0x80 + index * 4),
                0xDEAD_BEEF,
                "filled from one read"
            );
        }
    }

    #[test]
    fn cpu_fast_set_is_word_only_whatever_the_control_bit_says() {
        let mut cpu = cpu();
        let mut bus = FlatBus::new(0x200);
        bus.write32(0, 0xAABB_CCDD);
        cpu.set_reg(0, 0);
        cpu.set_reg(1, 0x40);
        cpu.set_reg(2, 1); // the word bit is clear, and it makes no difference
        run(&mut cpu, &mut bus, Core::Arm7, 0x0C);
        assert_eq!(bus.read32(0x40), 0xAABB_CCDD);
    }

    #[test]
    fn get_crc16_is_the_checksum_a_nds_header_carries() {
        // CRC-16/ARC's published check value: "123456789" from an initial 0x0000 is 0xBB3D. That
        // is the reason to pin this algorithm rather than any other reflected CRC — it can be
        // checked against something outside this repository.
        let mut cpu = cpu();
        let mut bus = FlatBus::with(0x200, b"123456789");
        cpu.set_reg(0, 0x0000);
        cpu.set_reg(1, 0x200);
        cpu.set_reg(2, 9);
        run(&mut cpu, &mut bus, Core::Arm9, 0x0E);
        assert_eq!(cpu.reg(0), 0xBB3D);

        // And it continues across buffers, which is what the initial value is for: hashing "1234"
        // then "56789" must equal hashing the whole thing at once.
        cpu.set_reg(0, 0x0000);
        cpu.set_reg(1, 0x200);
        cpu.set_reg(2, 4);
        run(&mut cpu, &mut bus, Core::Arm9, 0x0E);
        cpu.set_reg(1, 0x204);
        cpu.set_reg(2, 5);
        run(&mut cpu, &mut bus, Core::Arm9, 0x0E);
        assert_eq!(cpu.reg(0), 0xBB3D);
    }

    #[test]
    fn is_debugger_answers_retail() {
        let mut cpu = cpu();
        let mut bus = FlatBus::new(0x200);
        cpu.set_reg(0, 0xFFFF_FFFF);
        run(&mut cpu, &mut bus, Core::Arm9, 0x0F);
        assert_eq!(cpu.reg(0), 0);
    }

    #[test]
    fn halt_asks_the_caller_to_stop_but_not_to_repeat() {
        let mut cpu = cpu();
        let mut bus = FlatBus::new(0x200);
        let effect = run(&mut cpu, &mut bus, Core::Arm9, 0x06);
        assert!(effect.halt);
        assert!(!effect.repeat, "Halt returns once an interrupt arrives");
        assert!(
            !run(&mut cpu, &mut bus, Core::Arm9, 0x09).halt,
            "but Div does not halt"
        );
    }

    #[test]
    fn intr_wait_returns_at_once_when_the_flag_is_already_set_and_not_discarded() {
        let mut cpu = cpu();
        let mut bus = FlatBus::new(0x200);
        bus.write32(FLAGS, 1);
        cpu.set_reg(0, 0); // do not discard what is already there
        cpu.set_reg(1, 1);
        let mut waiting = false;
        let effect = dispatch(
            &mut cpu,
            &mut bus,
            0x04,
            Context {
                core: Core::Arm9,
                flags: FLAGS,
                waiting: &mut waiting,
            },
        );
        assert!(!effect.halt);
        assert!(!effect.repeat);
        assert_eq!(
            bus.read32(FLAGS),
            0,
            "and the bit it consumed is acknowledged"
        );
        assert!(!waiting);
    }

    #[test]
    fn intr_wait_discards_once_then_halts_until_the_handler_sets_the_flag() {
        // The sequence a real `swiWaitForVBlank` goes through, and the one that breaks if the
        // discard is repeated: the second entry must *not* clear the bit the handler just set.
        let mut cpu = cpu();
        let mut bus = FlatBus::new(0x200);
        bus.write32(FLAGS, 1); // a stale vertical blank from the previous frame
        let mut waiting = false;
        let call = |cpu: &mut Arm7Tdmi, bus: &mut FlatBus, waiting: &mut bool| {
            dispatch(
                cpu,
                bus,
                0x05,
                Context {
                    core: Core::Arm9,
                    flags: FLAGS,
                    waiting,
                },
            )
        };

        let first = call(&mut cpu, &mut bus, &mut waiting);
        assert!(
            first.halt && first.repeat,
            "the stale flag was discarded, so it waits"
        );
        assert_eq!(bus.read32(FLAGS), 0);
        assert!(waiting);

        // Woken by an interrupt that is not the one awaited: the handler set nothing.
        let second = call(&mut cpu, &mut bus, &mut waiting);
        assert!(second.halt && second.repeat, "and it goes back to waiting");

        // Now the handler sets the bit.
        bus.write32(FLAGS, 1);
        let third = call(&mut cpu, &mut bus, &mut waiting);
        assert!(!third.halt && !third.repeat);
        assert_eq!(bus.read32(FLAGS), 0);
        assert!(!waiting, "and the next call starts a fresh wait");
    }

    #[test]
    fn intr_wait_acknowledges_only_the_bits_it_was_asked_for() {
        let mut cpu = cpu();
        let mut bus = FlatBus::new(0x200);
        bus.write32(FLAGS, 0b1001);
        cpu.set_reg(0, 0);
        cpu.set_reg(1, 0b0001);
        let mut waiting = false;
        dispatch(
            &mut cpu,
            &mut bus,
            0x04,
            Context {
                core: Core::Arm7,
                flags: FLAGS,
                waiting: &mut waiting,
            },
        );
        assert_eq!(
            bus.read32(FLAGS),
            0b1000,
            "the source another wait is about to ask for is still there"
        );
    }

    #[test]
    fn custom_halt_writes_haltcnt_rather_than_deciding_for_itself() {
        let mut cpu = cpu();
        let mut bus = FlatBus::new(0x0400_0400);
        cpu.set_reg(2, 0x80);
        run(&mut cpu, &mut bus, Core::Arm7, 0x1F);
        assert_eq!(bus.read8(0x0400_0301), 0x80);
    }

    #[test]
    fn sound_bias_writes_the_register_it_names() {
        let mut cpu = cpu();
        let mut bus = FlatBus::new(0x0400_0600);
        cpu.set_reg(0, 1);
        run(&mut cpu, &mut bus, Core::Arm7, 0x08);
        assert_eq!(bus.read32(0x0400_0504), 0x200);
    }

    #[test]
    fn an_unimplemented_call_changes_nothing_rather_than_guessing() {
        // Unchanged registers show up in a trace; a plausible wrong answer surfaces a long way
        // from its cause. The three sound tables live in BIOS ROM this machine does not have, so
        // they are the honest example.
        let mut cpu = cpu();
        let mut bus = FlatBus::new(0x200);
        for comment in [0x10, 0x20, 0x21, 0x22, 0x99] {
            cpu.set_reg(0, 0x1234);
            cpu.set_reg(1, 0x5678);
            let effect = run(&mut cpu, &mut bus, Core::Arm7, comment);
            assert_eq!(cpu.reg(0), 0x1234, "SWI {comment:#04X}");
            assert_eq!(cpu.reg(1), 0x5678, "SWI {comment:#04X}");
            assert_eq!(effect, BiosEffect::default(), "SWI {comment:#04X}");
        }
    }
}

#[cfg(test)]
mod decompression_tests {
    use super::tests::{decompress, flat_with, read_bytes};
    use super::*;

    fn header(kind: u8, size: usize) -> [u8; 4] {
        ((kind as u32) | ((size as u32) << 8)).to_le_bytes()
    }

    #[test]
    fn lz77_copies_literals_and_back_references() {
        // Four literals, then a back-reference of length 4 at distance 4: "ABCD" then "ABCD".
        let mut stream = Vec::new();
        stream.extend_from_slice(&header(0x10, 8));
        stream.push(0b0000_1000);
        stream.extend_from_slice(b"ABCD");
        // length 4 -> (4-3) << 4 = 0x10; distance 4 -> stored as 3.
        stream.push(0x10);
        stream.push(0x03);

        let mut bus = flat_with(0x100, &stream);
        decompress(&mut bus, Core::Arm9, 0x11, 0x100, 0x800);
        assert_eq!(read_bytes(&mut bus, 0x800, 8), b"ABCDABCD".to_vec());
    }

    #[test]
    fn an_lz77_run_may_overlap_what_it_is_still_writing() {
        // Distance 1, length 5: the format's way of encoding a run, and it only works if each byte
        // is read back after being written rather than the whole span being copied at once.
        let mut stream = Vec::new();
        stream.extend_from_slice(&header(0x10, 6));
        stream.push(0b0100_0000);
        stream.push(b'Z');
        stream.push(0x20); // length 5
        stream.push(0x00); // distance 1
        let mut bus = flat_with(0x100, &stream);
        decompress(&mut bus, Core::Arm9, 0x11, 0x100, 0x800);
        assert_eq!(read_bytes(&mut bus, 0x800, 6), b"ZZZZZZ".to_vec());
    }

    #[test]
    fn a_back_reference_past_the_start_stops_rather_than_panicking() {
        let mut stream = Vec::new();
        stream.extend_from_slice(&header(0x10, 16));
        stream.push(0b1000_0000);
        stream.push(0x10);
        stream.push(0x40); // distance 65, with nothing written yet
        let mut bus = flat_with(0x100, &stream);
        decompress(&mut bus, Core::Arm9, 0x11, 0x100, 0x800);
        assert_eq!(read_bytes(&mut bus, 0x800, 4), vec![0, 0, 0, 0]);
    }

    #[test]
    fn the_vram_variant_writes_halfwords_so_nothing_is_dropped() {
        // A byte write to VRAM is dropped by the ARM9's bus, so a decompressor that emitted bytes
        // would write nothing at all. This checks the bytes come out in order and paired, which
        // they cannot if that happened.
        let mut stream = Vec::new();
        stream.extend_from_slice(&header(0x10, 4));
        stream.push(0b0000_0000);
        stream.extend_from_slice(&[0x11, 0x22, 0x33, 0x44]);
        let mut bus = flat_with(0x100, &stream);
        decompress(&mut bus, Core::Arm9, 0x12, 0x100, 0x800);
        assert_eq!(bus.read16(0x800), 0x2211);
        assert_eq!(bus.read16(0x802), 0x4433);
    }

    #[test]
    fn run_length_biases_its_two_forms_differently() {
        let mut stream = Vec::new();
        stream.extend_from_slice(&header(0x30, 7));
        stream.push(0x02); // literal, count 3
        stream.extend_from_slice(b"abc");
        stream.push(0x80 | 0x01); // repeated, count 4
        stream.push(b'!');
        let mut bus = flat_with(0x100, &stream);
        decompress(&mut bus, Core::Arm7, 0x14, 0x100, 0x800);
        assert_eq!(read_bytes(&mut bus, 0x800, 7), b"abc!!!!".to_vec());
    }

    #[test]
    fn the_difference_filters_accumulate_and_are_arm9_only() {
        let mut stream = Vec::new();
        stream.extend_from_slice(&header(0x81, 5));
        stream.extend_from_slice(&[10, 5, 250, 1, 0]);
        let mut bus = flat_with(0x100, &stream);
        decompress(&mut bus, Core::Arm9, 0x16, 0x100, 0x800);
        assert_eq!(read_bytes(&mut bus, 0x800, 5), vec![10, 15, 9, 10, 10]);

        // The same call on the ARM7 is not a call at all, and must leave the destination alone.
        let mut bus = flat_with(0x100, &stream);
        decompress(&mut bus, Core::Arm7, 0x16, 0x100, 0x900);
        assert_eq!(read_bytes(&mut bus, 0x900, 5), vec![0; 5]);
    }

    #[test]
    fn the_sixteen_bit_difference_filter_accumulates_halfwords() {
        let mut stream = Vec::new();
        stream.extend_from_slice(&header(0x82, 6));
        for value in [0x1000u16, 0x0234, 0xFFFF] {
            stream.extend_from_slice(&value.to_le_bytes());
        }
        let mut bus = flat_with(0x100, &stream);
        decompress(&mut bus, Core::Arm9, 0x18, 0x100, 0x800);
        assert_eq!(bus.read16(0x800), 0x1000);
        assert_eq!(bus.read16(0x802), 0x1234);
        assert_eq!(bus.read16(0x804), 0x1233);
    }

    #[test]
    fn a_header_claiming_an_absurd_size_is_refused_rather_than_allocated() {
        let mut stream = Vec::new();
        stream.extend_from_slice(&header(0x10, 0x00FF_FFFF));
        let mut bus = flat_with(0x100, &stream);
        decompress(&mut bus, Core::Arm9, 0x11, 0x100, 0x800);
        assert_eq!(read_bytes(&mut bus, 0x800, 4), vec![0, 0, 0, 0]);
    }

    #[test]
    fn huffman_decodes_an_eight_bit_tree() {
        // A two-symbol tree: bit 0 -> 'A', bit 1 -> 'B'. The root sits at tree_start+1, and its
        // children at (root & !1) + offset*2 + 2, which for offset 0 is tree_start + 2 and 3.
        let mut stream = Vec::new();
        stream.extend_from_slice(&header(0x28, 4));
        stream.push(0x01); // tree size: (1+1)*2 = 4 bytes
        stream.push(0xC0); // root: both children are leaves
        stream.push(b'A');
        stream.push(b'B');
        let bits: u32 = 0b0110_0000 << 24;
        stream.extend_from_slice(&bits.to_le_bytes());

        let mut bus = flat_with(0x100, &stream);
        decompress(&mut bus, Core::Arm9, 0x13, 0x100, 0x800);
        assert_eq!(read_bytes(&mut bus, 0x800, 4), b"ABBA".to_vec());
    }

    #[test]
    fn a_huffman_stream_that_does_not_terminate_gives_up_rather_than_hanging() {
        let mut stream = Vec::new();
        stream.extend_from_slice(&header(0x28, 64));
        stream.push(0x01);
        stream.push(0x00); // no leaf flags, offset 0: descends and never emits
        stream.extend_from_slice(&[0x00, 0x00, 0x00]);
        stream.extend_from_slice(&[0u8; 64]);
        let mut bus = flat_with(0x100, &stream);
        decompress(&mut bus, Core::Arm9, 0x13, 0x100, 0x800);
        // The point is simply that this returned.
    }

    #[test]
    fn every_decompression_call_number_maps_to_its_decoder() {
        // One wrong entry sends a stream to the wrong decoder, which produces garbage rather than
        // nothing — the harder failure to spot.
        for (comment, expected) in [
            (0x11, BiosCall::Lz77UnCompWram),
            (0x12, BiosCall::Lz77UnCompVram),
            (0x13, BiosCall::HuffUnComp),
            (0x14, BiosCall::RlUnCompWram),
            (0x15, BiosCall::RlUnCompVram),
            (0x16, BiosCall::Diff8bitUnFilterWram),
            (0x18, BiosCall::Diff16bitUnFilter),
        ] {
            assert_eq!(
                BiosCall::from_comment(Core::Arm9, comment),
                expected,
                "SWI {comment:#04X}"
            );
        }
    }
}
