//! High-level emulation of the BIOS calls a game reaches through `SWI`.
//!
//! # Why this exists at all
//!
//! Without a BIOS image the `SWI` vector at `0x08` is unmapped, so a game calling one runs off
//! into whatever is there. That is not a hypothetical: `gba-suite` executes 84,701 instructions
//! correctly and then calls `SWI 6` to divide, and every commercial game calls these constantly.
//! A machine that cannot answer them is a machine that runs nothing.
//!
//! # The bar is behavioural accuracy, not "usually works"
//!
//! Prompt 12 is explicit about this, and it matters more here than the count of calls suggests.
//! `Div` returning the wrong remainder, or `CpuSet` copying the wrong number of units, is a
//! subtle wrong answer that surfaces a long way from its cause. Each call below implements the
//! documented contract exactly, including the parts that look like mistakes:
//!
//! - `Div` returns the quotient *and* the remainder *and* the absolute quotient, in three
//!   registers, and truncates toward zero rather than flooring.
//! - `CpuSet`'s length field counts *units*, not bytes, and the unit is chosen by a bit in a
//!   different field.
//! - `Sqrt` returns an integer square root, so callers scale their input beforehand.
//!
//! Anything not implemented here returns without doing anything rather than guessing, which
//! leaves a caller with unchanged registers — visible in a trace, unlike a plausible-looking
//! wrong answer.

use crate::irq::{reg, source};
use crate::system::{CARTRIDGE_ENTRY, SP_IRQ, SP_SUPERVISOR, SP_SYSTEM};
use core_common::Bus;
use cpu_arm7tdmi::{Arm7Tdmi, Mode, Psr};

/// The calls this module answers.
///
/// Numbered as the hardware numbers them, so a trace showing `SWI 0x06` maps to `Div` without
/// a lookup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BiosCall {
    SoftReset,
    Halt,
    Stop,
    IntrWait,
    VBlankIntrWait,
    Div,
    DivArm,
    Sqrt,
    /// Not `ArcTan2` (0x0A): the plain one-argument form, whose documented range is only
    /// `-PI/2..PI/2` and which shares `ArcTan2`'s well-known accuracy problem near the extremes.
    ArcTan,
    ArcTan2,
    CpuSet,
    CpuFastSet,
    GetBiosChecksum,
    /// Clear the areas of memory a boot would, selected by a bitmask.
    RegisterRamReset,
    BgAffineSet,
    ObjAffineSet,
    /// The five decompressors, in the two destination flavours each has.
    ///
    /// Split by destination because VRAM cannot take a byte write: the `Vram` variants must emit
    /// halfwords or every other byte is lost. That distinction is the whole reason the hardware
    /// has two of each.
    BitUnPack,
    Lz77UnCompWram,
    Lz77UnCompVram,
    HuffUnComp,
    RlUnCompWram,
    RlUnCompVram,
    Diff8bitUnFilterWram,
    Diff8bitUnFilterVram,
    Diff16bitUnFilter,
    /// Recognised but not modelled: see the crate docs' Status section for why.
    SoundBias,
    MidiKey2Freq,
    Unhandled(u8),
}

impl BiosCall {
    pub fn from_comment(comment: u8) -> Self {
        match comment {
            0x00 => BiosCall::SoftReset,
            0x02 => BiosCall::Halt,
            0x03 => BiosCall::Stop,
            0x04 => BiosCall::IntrWait,
            0x05 => BiosCall::VBlankIntrWait,
            0x06 => BiosCall::Div,
            0x07 => BiosCall::DivArm,
            0x08 => BiosCall::Sqrt,
            0x09 => BiosCall::ArcTan,
            0x0A => BiosCall::ArcTan2,
            0x0B => BiosCall::CpuSet,
            0x0C => BiosCall::CpuFastSet,
            0x0D => BiosCall::GetBiosChecksum,
            0x01 => BiosCall::RegisterRamReset,
            0x0E => BiosCall::BgAffineSet,
            0x0F => BiosCall::ObjAffineSet,
            0x10 => BiosCall::BitUnPack,
            0x11 => BiosCall::Lz77UnCompWram,
            0x12 => BiosCall::Lz77UnCompVram,
            0x13 => BiosCall::HuffUnComp,
            0x14 => BiosCall::RlUnCompWram,
            0x15 => BiosCall::RlUnCompVram,
            0x16 => BiosCall::Diff8bitUnFilterWram,
            0x17 => BiosCall::Diff8bitUnFilterVram,
            0x18 => BiosCall::Diff16bitUnFilter,
            0x19 => BiosCall::SoundBias,
            0x1F => BiosCall::MidiKey2Freq,
            other => BiosCall::Unhandled(other),
        }
    }
}

/// The BIOS's own interrupt-flag word, at the top of internal WRAM.
///
/// Separate from `IF`, and the two are acknowledged by different code at different times: `IF` is
/// cleared by the game's handler as it services a source, while this word *accumulates* what has
/// been serviced so that [`BiosCall::IntrWait`] — which runs long after — can tell whether the
/// source it is waiting for has arrived. A wait that consulted `IF` instead would see it already
/// cleared and sleep forever.
pub const INTRWAIT_FLAGS: u32 = 0x0300_7FF8;

/// What the caller must do after the call, beyond returning.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BiosEffect {
    /// Carry on at the instruction after the `SWI`.
    ///
    /// Also wakes a halted CPU, because the only way to reach a `SWI` while halted is the
    /// [`BiosEffect::Retry`] path below finishing its wait.
    #[default]
    Return,
    /// Stop the CPU until an interrupt arrives. The call itself is finished.
    Halt,
    /// The call has *not* finished: stop the CPU and run this same `SWI` again on the next step.
    ///
    /// This is how a wait that hardware implements as a loop inside the BIOS is expressed in an
    /// emulator whose CPU cannot be suspended part-way through an instruction. See `intr_wait`.
    Retry,
    /// The call already set the program counter to where execution continues; step over nothing.
    ///
    /// `SoftReset` is the only call that returns this. GBATEK is explicit that it "does not
    /// return to calling procedure" — it jumps to a fresh entry point rather than resuming after
    /// the `SWI`, so the usual "step over the instruction that called it" is wrong here: the
    /// instruction after the `SWI` is not where execution goes next, and may not even belong to
    /// the game that is still running once this call is answered.
    Jump,
}

/// Perform a BIOS call in place of the ROM.
///
/// Takes the CPU and the bus rather than a register array because several calls move memory,
/// and splitting "which registers" from "what it does" would put the contract in two places.
///
/// `wait` carries the one piece of state a BIOS call keeps between steps: the mask of an
/// `intr_wait` that has begun and not yet been satisfied. Every other call here is a pure
/// function of the registers and memory, which is why it is one parameter rather than a context
/// struct.
pub fn dispatch<B: Bus + ?Sized>(
    cpu: &mut Arm7Tdmi,
    bus: &mut B,
    wait: &mut Option<u16>,
    comment: u8,
) -> BiosEffect {
    let mut effect = BiosEffect::default();

    match BiosCall::from_comment(comment) {
        BiosCall::Div => divide(cpu, cpu.reg(0) as i32, cpu.reg(1) as i32),
        // The same operation with its operands the other way round, which exists because early
        // ARM compilers passed them that way.
        BiosCall::DivArm => divide(cpu, cpu.reg(1) as i32, cpu.reg(0) as i32),
        BiosCall::Sqrt => {
            // An *integer* square root: callers that want fractional precision scale their
            // input up first and scale the result back down themselves.
            cpu.set_reg(0, (cpu.reg(0) as f64).sqrt() as u32);
        }
        BiosCall::ArcTan2 => {
            let x = cpu.reg(0) as i16 as f64;
            let y = cpu.reg(1) as i16 as f64;
            // The result is a full circle mapped onto 16 bits, not radians or degrees.
            let angle = y.atan2(x) / (2.0 * std::f64::consts::PI);
            let wrapped = angle.rem_euclid(1.0);
            cpu.set_reg(0, (wrapped * 65536.0) as u32 & 0xFFFF);
        }
        BiosCall::ArcTan => arc_tan(cpu),
        BiosCall::CpuSet => cpu_set(cpu, bus, false),
        BiosCall::CpuFastSet => cpu_set(cpu, bus, true),
        BiosCall::GetBiosChecksum => {
            // The checksum of a genuine GBA/GBA SP BIOS image, over its 16 KiB in 32-bit units.
            // Answered as a constant rather than computed, for the same reason a no-BIOS machine
            // can answer at all: there is no image here to sum. A game checks this against the
            // one well-known value; answering with it is the faithful response regardless of
            // whether a real image happens to be loaded.
            cpu.set_reg(0, 0xBAAE_187F);
        }
        // `Halt` and `Stop` sleep until *any* interrupt arrives, and the controller decides what
        // counts as one. They are genuinely the "wake on anything" calls, which is why the two
        // below them — which are not — no longer share this arm.
        BiosCall::Halt | BiosCall::Stop => effect = BiosEffect::Halt,
        BiosCall::IntrWait => effect = intr_wait(cpu, bus, wait, false),
        BiosCall::VBlankIntrWait => effect = intr_wait(cpu, bus, wait, true),
        BiosCall::RegisterRamReset => register_ram_reset(cpu, bus),
        BiosCall::BgAffineSet => affine_set(cpu, bus, true),
        BiosCall::ObjAffineSet => affine_set(cpu, bus, false),
        BiosCall::Lz77UnCompWram => unpack(cpu, bus, Decoder::Lz77, false),
        BiosCall::Lz77UnCompVram => unpack(cpu, bus, Decoder::Lz77, true),
        BiosCall::RlUnCompWram => unpack(cpu, bus, Decoder::RunLength, false),
        BiosCall::RlUnCompVram => unpack(cpu, bus, Decoder::RunLength, true),
        BiosCall::HuffUnComp => unpack(cpu, bus, Decoder::Huffman, true),
        BiosCall::Diff8bitUnFilterWram => unpack(cpu, bus, Decoder::Diff8, false),
        BiosCall::Diff8bitUnFilterVram => unpack(cpu, bus, Decoder::Diff8, true),
        BiosCall::Diff16bitUnFilter => unpack(cpu, bus, Decoder::Diff16, true),
        BiosCall::BitUnPack => bit_unpack(cpu, bus),
        BiosCall::SoftReset => {
            soft_reset(cpu, bus);
            effect = BiosEffect::Jump;
        }
        // `fifo::DirectSound::soundbias` now stores the register a plain write to `0x0400_0088`
        // reaches, but this call is a different contract: hardware ramps the level toward the
        // target over a handful of VBlanks rather than snapping to it, which cannot be answered
        // in one step here without either blocking (like `IntrWait`) or fabricating a ramp rate
        // this crate has not modelled. Recognising the call at all is still worth doing: MP2K
        // calls it during its own init, and answering it here means that startup sequence sees a
        // call that exists rather than one that silently vanished. See the crate docs' Status
        // section.
        BiosCall::SoundBias => {
            tracing::debug!(
                "SoundBias asked to ramp SOUNDBIAS toward {:#X}; the ramp itself is not \
                 modelled, so the register is unaffected by this call",
                cpu.reg(0)
            );
        }
        BiosCall::MidiKey2Freq => midi_key_to_freq(cpu, bus),
        // Doing nothing leaves the caller's registers unchanged, which shows up in a trace.
        // Guessing would produce a plausible wrong answer that surfaces far from its cause.
        //
        // But it is *logged*. A call that silently does nothing is exactly how Pokémon Emerald
        // ran at full speed with a black screen for a whole session: every graphic it owns is
        // LZ77-compressed, the decompressor was missing, and nothing said so.
        BiosCall::Unhandled(_) => {
            tracing::warn!(
                "unimplemented BIOS call SWI {comment:#04X}; it did nothing, and whatever the \
                 game expected it to produce is missing"
            );
        }
    }
    effect
}

/// `IntrWait` and `VBlankIntrWait`: sleep until one of the interrupts named in `r1` has been
/// serviced.
///
/// # This is not `Halt`
///
/// It was implemented as one for a long time, and the difference is the whole point. `Halt` wakes
/// on *any* interrupt. `IntrWait` wakes only on the sources named in its mask, and it decides that
/// by reading [`INTRWAIT_FLAGS`] — a word that accumulates what has been serviced — rather than by
/// asking whether the CPU woke up.
///
/// A game that enables `HBlank` for a raster effect and a timer for its sound driver, which is most
/// commercial software, calls `VBlankIntrWait` once a frame and gets it back on the next `HBlank`
/// instead: up to 160 returns per frame rather than one. Nothing errors. The game simply runs its
/// main loop at 160 times the rate it expects, and every symptom downstream of frame pacing —
/// animation speed, input handling, and any rendering read mid-frame — is wrong in a way that looks
/// like a dozen unrelated bugs.
///
/// # Why the `SWI` is re-executed instead of looping here
///
/// Hardware implements this as a loop *inside* the BIOS, which an emulator can only match if it can
/// suspend a CPU part-way through an instruction. Nothing in this codebase's core can: `step` runs
/// one instruction to completion.
///
/// So the wait is spread across steps instead. An unsatisfied wait returns [`BiosEffect::Retry`],
/// the system assembly leaves the program counter *on* the `SWI`, and the same call runs again next
/// step — testing the flag word each time until it is satisfied. The observable behaviour is the
/// same and no core change is needed.
///
/// The alternative considered was polling the condition from the system's step loop and leaving the
/// program counter past the `SWI`. Rejected because it puts half of one BIOS call in `system.rs` and
/// half here, and the half in `system.rs` would run on every step of every machine including the
/// ones not waiting.
///
/// `wait` holds the mask of a wait already begun. It has to be real state rather than something
/// re-derived from the registers, because `r0` — "discard what has already arrived" — must be
/// honoured on the first pass and ignored on every later one, and after the first pass there is
/// nothing in the machine that distinguishes the two.
fn intr_wait<B: Bus + ?Sized>(
    cpu: &mut Arm7Tdmi,
    bus: &mut B,
    wait: &mut Option<u16>,
    vblank_only: bool,
) -> BiosEffect {
    // A later pass of a wait already begun: the discard is done, so only the test remains.
    if let Some(mask) = *wait {
        let flags = bus.read16(INTRWAIT_FLAGS);
        let matched = flags & mask;
        if matched == 0 {
            return BiosEffect::Retry;
        }
        // Consume only what was waited for. Another source serviced meanwhile stays set for
        // whoever asks next, which is what makes two waits on different sources compose.
        bus.write16(INTRWAIT_FLAGS, flags & !matched);
        *wait = None;
        return BiosEffect::Return;
    }

    // The first pass. `SWI 0x05` is `SWI 0x04` with the mask fixed to vertical blank and the
    // discard flag forced on, which is exactly how the BIOS implements it.
    let (mask, discard) = if vblank_only {
        (source::VBLANK, true)
    } else {
        ((cpu.reg(1) as u16) & source::ALL, cpu.reg(0) != 0)
    };

    // Hardware sets `IME` on entry, unconditionally. A game that called this with interrupts
    // masked off would otherwise never be woken by anything — and some do, relying on the BIOS
    // to turn them back on.
    bus.write16(reg::IME, 1);

    let flags = bus.read16(INTRWAIT_FLAGS);
    if discard {
        // "Wait for a *new* one": forget what has already arrived, then sleep.
        bus.write16(INTRWAIT_FLAGS, flags & !mask);
    } else {
        let matched = flags & mask;
        if matched != 0 {
            // Already arrived, so this call does not sleep at all.
            bus.write16(INTRWAIT_FLAGS, flags & !matched);
            return BiosEffect::Return;
        }
    }

    // A wait nothing can satisfy hangs on hardware, and this hangs too — deliberately, because
    // inventing a wake-up would turn a game's bug into a wrong picture somewhere far away. It is
    // said out loud, once, because the alternative is a machine that looks like it crashed.
    let enabled = bus.read16(reg::IE);
    if mask & enabled == 0 {
        tracing::warn!(
            mask = format_args!("{mask:#06X}"),
            enabled = format_args!("{enabled:#06X}"),
            "IntrWait is waiting for an interrupt that is not enabled; nothing can satisfy it, \
             and the machine will sit here exactly as hardware would"
        );
    }

    *wait = Some(mask);
    BiosEffect::Retry
}

/// The byte, inside the region this call zeroes, that says where to jump: `0` for the cartridge
/// (`0x0800_0000`), any other value for RAM (`0x0200_0000`).
const SOFT_RESET_FLAG: u32 = 0x0300_7FFA;

/// `SoftReset`: clear the BIOS's own top-of-IWRAM state and restart the game exactly the way a
/// cold boot without a BIOS does.
///
/// # The documented contract
///
/// Zeroes the 0x200 bytes at `0x0300_7E00` — stacks, the interrupt handler pointer, this call's
/// own destination flag, and [`INTRWAIT_FLAGS`] all live inside that span. Zeroes `r0`-`r12`,
/// `LR_svc`/`SPSR_svc`, and `LR_irq`/`SPSR_irq`. Sets `SP_svc = 0x0300_7FE0`,
/// `SP_irq = 0x0300_7FA0`, `SP_sys = 0x0300_7F00` — the exact three stacks
/// `system::GbaSystem::apply_startup_state` also sets, because both are setting up the same
/// documented post-boot state. Switches to System mode, ARM state, and jumps to `0x0800_0000`
/// (ROM) or `0x0200_0000` (RAM) depending on the byte at [`SOFT_RESET_FLAG`] — which sits inside
/// the region just zeroed, so it has to be read before that happens.
///
/// # Why r8-r12 are written through the Supervisor bank explicitly
///
/// This runs before the exception is entered — see `system::GbaSystem::intercept_bios_call` — so
/// the CPU's *current* mode is still whatever the caller's was, not Supervisor. `cpu.set_reg`
/// resolves against the current mode, which would write the wrong bank if the caller happened to
/// be in FIQ (the one mode that banks `r8`-`r12` differently). Naming `Mode::Supervisor` directly
/// is correct regardless of the caller's mode, because every mode but FIQ shares that bank anyway.
fn soft_reset<B: Bus + ?Sized>(cpu: &mut Arm7Tdmi, bus: &mut B) {
    let to_ram = bus.read8(SOFT_RESET_FLAG) != 0;

    for offset in (0..0x200u32).step_by(4) {
        bus.write32(0x0300_7E00 + offset, 0);
    }

    for index in 0..=12 {
        cpu.regs.write(Mode::Supervisor, index, 0);
    }
    cpu.regs.write(Mode::Supervisor, 14, 0);
    cpu.regs.set_spsr(Mode::Supervisor, Psr::default());
    cpu.regs.write(Mode::Irq, 14, 0);
    cpu.regs.set_spsr(Mode::Irq, Psr::default());

    cpu.regs.write(Mode::Supervisor, 13, SP_SUPERVISOR);
    cpu.regs.write(Mode::Irq, 13, SP_IRQ);
    cpu.regs.write(Mode::System, 13, SP_SYSTEM);

    cpu.cpsr.set_mode(Mode::System);
    cpu.cpsr.set_thumb(false);
    cpu.cpsr.set_irq_disabled(false);
    cpu.cpsr.set_fiq_disabled(true);

    cpu.regs
        .set_pc(if to_ram { 0x0200_0000 } else { CARTRIDGE_ENTRY });
}

/// `RegisterRamReset`: clear the areas named by a bitmask in `r0`.
///
/// Games call this at boot, and a machine that ignores it starts with whatever the previous game
/// left behind — which on a fresh emulator is zeroes and therefore invisible, right up until a
/// reset is used and the old picture stays on screen.
fn register_ram_reset<B: Bus + ?Sized>(cpu: &mut Arm7Tdmi, bus: &mut B) {
    let flags = cpu.reg(0);
    for (start, len) in reset_regions(flags) {
        for offset in (0..len).step_by(4) {
            bus.write32(start + offset, 0);
        }
    }
    // Bits 5-7 reset the I/O registers by group. Not done: those registers have their own reset
    // semantics and zeroing them wholesale would switch the display off mid-frame.
    if flags & 0xE0 != 0 {
        tracing::debug!("RegisterRamReset asked to reset I/O registers; not done");
    }
}

/// Which spans a `RegisterRamReset` mask names.
///
/// Split out from the writing so the table can be checked directly. The one entry that is not the
/// obvious thing is internal WRAM: the top 0x200 bytes hold the BIOS's own state *and the
/// interrupt handler pointer*, so clearing the whole region takes the game's handler address with
/// it and the next interrupt jumps to zero.
fn reset_regions(flags: u32) -> Vec<(u32, u32)> {
    let mut regions = Vec::new();
    if flags & (1 << 0) != 0 {
        regions.push((0x0200_0000, 0x0004_0000));
    }
    if flags & (1 << 1) != 0 {
        regions.push((0x0300_0000, 0x7E00));
    }
    if flags & (1 << 2) != 0 {
        regions.push((0x0500_0000, 0x400));
    }
    if flags & (1 << 3) != 0 {
        regions.push((0x0600_0000, 0x0001_8000));
    }
    if flags & (1 << 4) != 0 {
        regions.push((0x0700_0000, 0x400));
    }
    regions
}

/// `BgAffineSet` and `ObjAffineSet`: build rotation/scale matrices from an angle and a scale.
///
/// The angle is a full circle over 16 bits, and only its top eight bits are significant, which is
/// why a game rotating slowly appears to step rather than glide — that is the hardware, not this.
fn affine_set<B: Bus + ?Sized>(cpu: &mut Arm7Tdmi, bus: &mut B, background: bool) {
    let mut source = cpu.reg(0);
    let mut destination = cpu.reg(1);
    let count = cpu.reg(2);
    // `ObjAffineSet` takes the gap between output halfwords in `r3`, so one call can fill the
    // scattered `PA`/`PB`/`PC`/`PD` slots of OAM without a separate call per sprite.
    let stride = if background { 2 } else { cpu.reg(3).max(2) };

    for _ in 0..count {
        // Read the whole input record before moving the pointer, so the field offsets read as
        // the documented layout rather than as arithmetic relative to an already-advanced source.
        let (scale_x, scale_y, angle, centre) = if background {
            let record = (
                bus.read32(source) as i32,            // destination x, 24.8
                bus.read32(source + 4) as i32,        // destination y, 24.8
                bus.read16(source + 8) as i16 as i32, // centre x
                bus.read16(source + 10) as i16 as i32,
                bus.read16(source + 12) as i16 as i32, // scale x, 8.8
                bus.read16(source + 14) as i16 as i32,
                bus.read16(source + 16), // angle
            );
            source += 20;
            (
                record.4,
                record.5,
                record.6 as u32,
                Some((record.0, record.1, record.2, record.3)),
            )
        } else {
            let sx = bus.read16(source) as i16 as i32;
            let sy = bus.read16(source + 2) as i16 as i32;
            let angle = bus.read16(source + 4) as u32;
            source += 6;
            (sx, sy, angle, None)
        };

        // Only bits 8-15 of the angle are significant, mapped onto a full turn — which is why a
        // slow rotation steps rather than glides. That is the hardware, not this.
        let theta = ((angle >> 8) & 0xFF) as f64 * std::f64::consts::TAU / 256.0;
        let cos = (theta.cos() * 256.0).round() as i32;
        let sin = (theta.sin() * 256.0).round() as i32;

        let pa = (scale_x * cos) >> 8;
        let pb = (-scale_x * sin) >> 8;
        let pc = (scale_y * sin) >> 8;
        let pd = (scale_y * cos) >> 8;

        for (index, value) in [pa, pb, pc, pd].into_iter().enumerate() {
            bus.write16(destination + index as u32 * stride, value as u16);
        }
        destination += 4 * stride;

        if let Some((dx, dy, cx, cy)) = centre {
            // The background form also writes the start coordinates, which is the whole reason a
            // game uses it rather than building the matrix itself.
            bus.write32(destination, (dx - cx * pa - cy * pb) as u32);
            bus.write32(destination + 4, (dy - cx * pc - cy * pd) as u32);
            destination += 8;
        }
    }
}

/// `BitUnPack`: widen each source unit to a destination unit, optionally adding a bias.
///
/// Used to expand a low-colour-depth bitmap or tileset — a 1bpp font into 4bpp or 8bpp tiles, for
/// instance — without shipping a second copy of the asset at the wider depth.
///
/// `r0` is the source, `r1` the destination (word-aligned; data is written in whole words), and
/// `r2` points at a header: a `u16` length of the source in bytes, a `u8` source width and a `u8`
/// destination width in bits (source only 1/2/4/8; destination only 1/2/4/8/16/32), and a `u32`
/// whose low 31 bits are a bias added to every non-zero source unit and whose top bit — the "zero
/// data" flag — extends that bias to zero units too.
///
/// # The bit-packing order
///
/// Source units are read LSB-first out of each byte — the low `SrcBpp` bits are the first unit,
/// not the high ones — and destination units are packed LSB-first into a 32-bit accumulator that
/// flushes as a whole word once full. Getting either direction backwards produces an image that
/// is recognisably shuffled rather than obviously wrong, which is a harder bug to spot than a
/// crash.
///
/// Register-side effects are not implemented: GBATEK is explicit that this call returns nothing,
/// unlike `CpuSet`'s pointers.
fn bit_unpack<B: Bus + ?Sized>(cpu: &mut Arm7Tdmi, bus: &mut B) {
    let mut source = cpu.reg(0);
    let mut destination = cpu.reg(1);
    let info = cpu.reg(2);

    let mut len = bus.read16(info) as u32;
    let src_width = bus.read8(info + 2);
    let dest_width = bus.read8(info + 3);
    let data_offset = bus.read32(info + 4);

    if !matches!(src_width, 1 | 2 | 4 | 8) || !matches!(dest_width, 1 | 2 | 4 | 8 | 16 | 32) {
        tracing::warn!(
            "BitUnPack asked for {src_width}-bit source units and {dest_width}-bit destination \
             units; only 1/2/4/8 and 1/2/4/8/16/32 exist"
        );
        return;
    }

    let bias = data_offset & 0x7FFF_FFFF;
    let bias_on_zero = data_offset & 0x8000_0000 != 0;
    let src_mask = (1u32 << src_width) - 1;

    let mut out: u32 = 0;
    let mut out_bits = 0u32;
    let mut in_byte = 0u32;
    let mut in_bits = 0u32;

    while len > 0 || in_bits > 0 {
        if in_bits == 0 {
            in_byte = bus.read8(source) as u32;
            source += 1;
            len -= 1;
            in_bits = 8;
        }

        let mut unit = in_byte & src_mask;
        in_byte >>= src_width;
        in_bits -= src_width as u32;

        if unit != 0 || bias_on_zero {
            unit = unit.wrapping_add(bias);
        }

        out |= unit << out_bits;
        out_bits += dest_width as u32;

        if out_bits == 32 {
            bus.write32(destination, out);
            destination = destination.wrapping_add(4);
            out = 0;
            out_bits = 0;
        }
    }
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

/// The shared shape of every decompression call: read a header, decode into a buffer, write it out.
///
/// # Why a buffer and not a stream
///
/// The `Vram` variants must write halfwords, because a byte write to VRAM is doubled into both
/// halves of its containing halfword — so a decompressor that emitted bytes would corrupt every
/// other one. Decoding into a `Vec` first and writing it out at the destination's natural width
/// makes that one decision instead of five, and lets the back-reference in LZ77 read bytes it has
/// already produced without reading them back off the bus.
fn unpack<B: Bus + ?Sized>(cpu: &mut Arm7Tdmi, bus: &mut B, decoder: Decoder, vram: bool) {
    let source = cpu.reg(0);
    let destination = cpu.reg(1);
    let header = bus.read32(source);
    let size = (header >> 8) as usize;

    // A malformed or wildly out-of-range header is a game bug or a mis-set pointer, and
    // allocating from it would turn that into an out-of-memory. 256 KiB is larger than any real
    // decompressed asset — the whole of EWRAM.
    const LIMIT: usize = 0x0004_0000;
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
                // leaves a short buffer, which shows up as missing graphics rather than as a panic.
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
            // The repeated form's count is biased by three, not one — the two forms do not share
            // a bias, and using one for both produces output that is subtly the wrong length.
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

/// Difference filters: each byte is stored as its difference from the one before.
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

    // The tree begins immediately after the header word. Its first byte is its size, and the
    // root node is the byte after that.
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
        // not from the node itself. Reading the tree as a flat array produces plausible output
        // that is wrong from the first symbol.
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

/// `Div`: quotient in `r0`, remainder in `r1`, absolute quotient in `r3`.
///
/// Truncates toward zero rather than flooring, so `-7 / 2` is `-3` with remainder `-1` and not
/// `-4` with remainder `1`. The remainder takes the sign of the *dividend*, which is what
/// Rust's `%` already does — but it is worth stating, because the other convention is common
/// enough that a reader may assume it.
fn divide(cpu: &mut Arm7Tdmi, numerator: i32, denominator: i32) {
    if denominator == 0 {
        // Hardware hangs here. Returning leaves the registers alone, which is a debuggable
        // outcome rather than an emulator that stops responding.
        return;
    }
    // `i32::MIN / -1` overflows; the hardware wraps, so this does too.
    let quotient = numerator.wrapping_div(denominator);
    let remainder = numerator.wrapping_rem(denominator);
    cpu.set_reg(0, quotient as u32);
    cpu.set_reg(1, remainder as u32);
    cpu.set_reg(3, quotient.unsigned_abs());
}

/// `ArcTan`: the real BIOS's seventh-order fixed-point polynomial, not the true function.
///
/// `r0` is a `Q1.1.14` fixed-point tangent; the result is an angle in `-PI/2..PI/2` mapped onto
/// `C000h..4000h` of a full circle (the same 16-bit-per-turn convention [`affine_set`] uses).
///
/// # Why this is a polynomial and not `f64::atan`
///
/// Hardware does not evaluate a true arctangent — it runs this specific `Q14` polynomial, and the
/// polynomial has its own documented inaccuracy, worst "for THETA < -PI/4, PI/4 < THETA" per
/// GBATEK. A game reading the *exact* hardware value, rather than a merely plausible angle, would
/// see a different number from `atan` — most visibly right around zero, where `ArcTan(1)` rounds
/// to `0` but `ArcTan(-1)` rounds to `-1` rather than the `0` symmetry would suggest. Reproducing
/// `atan` here would answer a different, more accurate question than the one the game asked.
///
/// Every operation wraps like the hardware's 32-bit multiply and add do — see `divide` for the
/// same reasoning applied to a different call — because a wild out-of-range input pushes several
/// of the intermediate terms outside `i32` on the way to an answer that is still well-defined.
fn arc_tan(cpu: &mut Arm7Tdmi) {
    let i = cpu.reg(0) as i32;
    let a = (i.wrapping_mul(i) >> 14).wrapping_neg();
    let mut b = (0xA9i32.wrapping_mul(a) >> 14).wrapping_add(0x390);
    b = (b.wrapping_mul(a) >> 14).wrapping_add(0x91C);
    b = (b.wrapping_mul(a) >> 14).wrapping_add(0xFB6);
    b = (b.wrapping_mul(a) >> 14).wrapping_add(0x16AA);
    b = (b.wrapping_mul(a) >> 14).wrapping_add(0x2081);
    b = (b.wrapping_mul(a) >> 14).wrapping_add(0x3651);
    b = (b.wrapping_mul(a) >> 14).wrapping_add(0xA2F9);
    cpu.set_reg(0, (i.wrapping_mul(b) >> 16) as u32);
}

/// `CpuSet` and `CpuFastSet`: copy or fill memory.
///
/// `r2` is not a byte count. Its low 21 bits are a count of *units*, and bit 26 chooses whether
/// a unit is a halfword or a word — so the same value means different amounts depending on a bit
/// in a different field. Bit 24 switches from copying to filling, where the source is read once
/// and written repeatedly.
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

/// `MidiKey2Freq`: the playback rate a sample must be resampled to for it to sound at a given
/// MIDI key, given the rate it was originally recorded at.
///
/// The M4A/MP2K sound driver — used by every Pokémon game and much other commercial software —
/// calls this to turn a note-on event into a DMA sample rate, which is why it matters more than
/// an obscure math routine's call number suggests: without it, MP2K's own init sequence calls
/// into an unanswered `SWI` before a game gets anywhere near rendering a title screen.
///
/// `r0` points at a `WaveData` header whose word at `r0+4` is the sample's rate, pre-scaled for
/// its *original* recorded key by `sampling_rate * 2^((180 - original_key) / 12)`. `r1` is the
/// target MIDI key (0-127) and `r2` a fine-pitch adjustment in 256ths of a semitone. Dividing the
/// stored rate by the same exponential, evaluated at the target key and fine pitch instead of the
/// original one, is what turns a recording of one note into the playback rate for another.
fn midi_key_to_freq<B: Bus + ?Sized>(cpu: &mut Arm7Tdmi, bus: &mut B) {
    let wave_data = cpu.reg(0);
    let key = cpu.reg(1) as f64;
    let fine_pitch = cpu.reg(2) as f64;

    let rate = bus.read32(wave_data + 4) as f64;
    let freq = rate / 2f64.powf((180.0 - key - fine_pitch / 256.0) / 12.0);
    // Matches C's float-to-integer cast, which is what a real disassembly of this routine
    // performs: truncated toward zero, not rounded.
    cpu.set_reg(0, freq as u32);
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use cpu_arm7tdmi::BootState;

    fn cpu() -> Arm7Tdmi {
        Arm7Tdmi::new(BootState::default())
    }

    /// Flat memory, so a test can assert on what `CpuSet` moved without a memory map in the way.
    pub(super) struct FlatBus {
        bytes: Vec<u8>,
    }

    impl FlatBus {
        pub(super) fn new(size: usize) -> Self {
            Self {
                bytes: vec![0; size],
            }
        }
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
    }

    #[test]
    fn the_call_numbers_match_the_hardware_so_a_trace_reads_directly() {
        assert_eq!(BiosCall::from_comment(0x06), BiosCall::Div);
        assert_eq!(BiosCall::from_comment(0x05), BiosCall::VBlankIntrWait);
        assert_eq!(BiosCall::from_comment(0x0C), BiosCall::CpuFastSet);
        assert_eq!(BiosCall::from_comment(0x99), BiosCall::Unhandled(0x99));
    }

    #[test]
    fn div_returns_quotient_remainder_and_absolute_quotient() {
        let mut cpu = cpu();
        let mut bus = FlatBus::new(16);
        cpu.set_reg(0, 100);
        cpu.set_reg(1, 7);
        dispatch(&mut cpu, &mut bus, &mut None, 0x06);
        assert_eq!(cpu.reg(0), 14);
        assert_eq!(cpu.reg(1), 2);
        assert_eq!(cpu.reg(3), 14);
    }

    #[test]
    fn div_truncates_toward_zero_rather_than_flooring() {
        // -7 / 2 is -3 remainder -1, not -4 remainder 1. Both conventions are common enough
        // that assuming is a real risk.
        let mut cpu = cpu();
        let mut bus = FlatBus::new(16);
        cpu.set_reg(0, (-7i32) as u32);
        cpu.set_reg(1, 2);
        dispatch(&mut cpu, &mut bus, &mut None, 0x06);
        assert_eq!(cpu.reg(0) as i32, -3);
        assert_eq!(cpu.reg(1) as i32, -1);
        assert_eq!(cpu.reg(3), 3, "the absolute quotient");
    }

    #[test]
    fn div_arm_takes_its_operands_the_other_way_round() {
        let mut cpu = cpu();
        let mut bus = FlatBus::new(16);
        cpu.set_reg(0, 7);
        cpu.set_reg(1, 100);
        dispatch(&mut cpu, &mut bus, &mut None, 0x07);
        assert_eq!(cpu.reg(0), 14, "100 / 7, not 7 / 100");
    }

    #[test]
    fn dividing_by_zero_leaves_the_registers_alone_rather_than_hanging() {
        // Hardware hangs. An emulator that stops responding is worse to debug than one whose
        // registers visibly did not change.
        let mut cpu = cpu();
        let mut bus = FlatBus::new(16);
        cpu.set_reg(0, 42);
        cpu.set_reg(1, 0);
        dispatch(&mut cpu, &mut bus, &mut None, 0x06);
        assert_eq!(cpu.reg(0), 42);
    }

    #[test]
    fn sqrt_is_an_integer_square_root() {
        let mut cpu = cpu();
        let mut bus = FlatBus::new(16);
        for (input, expected) in [(0u32, 0u32), (1, 1), (2, 1), (16, 4), (17, 4), (10000, 100)] {
            cpu.set_reg(0, input);
            dispatch(&mut cpu, &mut bus, &mut None, 0x08);
            assert_eq!(cpu.reg(0), expected, "sqrt({input})");
        }
    }

    #[test]
    fn cpu_set_counts_units_not_bytes() {
        // The trap this call sets: r2's low bits are a count of halfwords or words depending on
        // a bit twenty-six places away, so the same number means different amounts.
        let mut cpu = cpu();
        let mut bus = FlatBus::new(0x100);
        for index in 0..4u32 {
            bus.write16(index * 2, 0x1000 + index as u16);
        }
        cpu.set_reg(0, 0);
        cpu.set_reg(1, 0x40);
        cpu.set_reg(2, 4); // four halfwords
        dispatch(&mut cpu, &mut bus, &mut None, 0x0B);

        for index in 0..4u32 {
            assert_eq!(bus.read16(0x40 + index * 2), 0x1000 + index as u16);
        }
        assert_eq!(bus.read16(0x48), 0, "and it stopped after four");
    }

    #[test]
    fn the_word_bit_makes_each_unit_four_bytes() {
        let mut cpu = cpu();
        let mut bus = FlatBus::new(0x100);
        bus.write32(0, 0xDEAD_BEEF);
        bus.write32(4, 0xCAFE_F00D);
        cpu.set_reg(0, 0);
        cpu.set_reg(1, 0x40);
        cpu.set_reg(2, 2 | (1 << 26));
        dispatch(&mut cpu, &mut bus, &mut None, 0x0B);
        assert_eq!(bus.read32(0x40), 0xDEAD_BEEF);
        assert_eq!(bus.read32(0x44), 0xCAFE_F00D);
    }

    #[test]
    fn the_fill_bit_reads_the_source_once_and_repeats_it() {
        let mut cpu = cpu();
        let mut bus = FlatBus::new(0x100);
        bus.write32(0, 0x1234_5678);
        cpu.set_reg(0, 0);
        cpu.set_reg(1, 0x40);
        cpu.set_reg(2, 3 | (1 << 24) | (1 << 26));
        dispatch(&mut cpu, &mut bus, &mut None, 0x0B);
        for index in 0..3u32 {
            assert_eq!(bus.read32(0x40 + index * 4), 0x1234_5678);
        }
    }

    #[test]
    fn cpu_fast_set_is_word_only_whatever_the_control_bit_says() {
        let mut cpu = cpu();
        let mut bus = FlatBus::new(0x100);
        bus.write32(0, 0xAABB_CCDD);
        cpu.set_reg(0, 0);
        cpu.set_reg(1, 0x40);
        cpu.set_reg(2, 1); // the word bit is clear, and it makes no difference
        dispatch(&mut cpu, &mut bus, &mut None, 0x0C);
        assert_eq!(bus.read32(0x40), 0xAABB_CCDD);
    }

    #[test]
    fn halt_and_stop_finish_the_call_but_the_waits_ask_to_run_again() {
        // The distinction the shared arm used to hide. `Halt` is done when it returns and the
        // program counter moves on; an `IntrWait` is not finished until its flag word says so, so
        // it asks to be re-run rather than stepped over.
        let mut cpu = cpu();
        let mut bus = FlatBus::new(16);
        for call in [0x02, 0x03] {
            assert_eq!(
                dispatch(&mut cpu, &mut bus, &mut None, call),
                BiosEffect::Halt,
                "SWI {call:#04X}"
            );
        }
        for call in [0x04, 0x05] {
            assert_eq!(
                dispatch(&mut cpu, &mut bus, &mut None, call),
                BiosEffect::Retry,
                "SWI {call:#04X}"
            );
        }
        assert_eq!(
            dispatch(&mut cpu, &mut bus, &mut None, 0x06),
            BiosEffect::Return,
            "but Div returns straight away"
        );
    }

    #[test]
    fn an_unhandled_call_changes_nothing_rather_than_guessing() {
        // Unchanged registers show up in a trace; a plausible wrong answer surfaces a long way
        // from its cause.
        let mut cpu = cpu();
        let mut bus = FlatBus::new(16);
        cpu.set_reg(0, 0x1234);
        cpu.set_reg(1, 0x5678);
        dispatch(&mut cpu, &mut bus, &mut None, 0x99);
        assert_eq!(cpu.reg(0), 0x1234);
        assert_eq!(cpu.reg(1), 0x5678);
    }
}

#[cfg(test)]
mod decompression_tests {
    use super::*;
    use crate::bios::tests::FlatBus;

    /// Put `bytes` at `at` in a bus and return it.
    fn bus_with(at: u32, bytes: &[u8]) -> FlatBus {
        let mut bus = FlatBus::new(0x4000);
        for (index, byte) in bytes.iter().enumerate() {
            bus.write8(at + index as u32, *byte);
        }
        bus
    }

    fn header(kind: u8, size: usize) -> [u8; 4] {
        let word = (kind as u32) | ((size as u32) << 8);
        word.to_le_bytes()
    }

    fn run(bus: &mut FlatBus, swi: u8, source: u32, destination: u32) {
        let mut cpu = Arm7Tdmi::default();
        cpu.set_reg(0, source);
        cpu.set_reg(1, destination);
        dispatch(&mut cpu, bus, &mut None, swi);
    }

    fn read_out(bus: &mut FlatBus, at: u32, len: usize) -> Vec<u8> {
        (0..len).map(|i| bus.read8(at + i as u32)).collect()
    }

    #[test]
    fn lz77_copies_literals_and_back_references() {
        // Four literals, then a back-reference of length 4 at distance 4: "ABCD" then "ABCD".
        let mut stream = Vec::new();
        stream.extend_from_slice(&header(0x10, 8));
        stream.push(0b0000_1000); // four literals, then one reference, then unused
        stream.extend_from_slice(b"ABCD");
        // length 4 -> (4-3) << 4 = 0x10; distance 4 -> stored as 3.
        stream.push(0x10);
        stream.push(0x03);

        let mut bus = bus_with(0x100, &stream);
        run(&mut bus, 0x11, 0x100, 0x800);
        assert_eq!(read_out(&mut bus, 0x800, 8), b"ABCDABCD".to_vec());
    }

    #[test]
    fn an_lz77_run_may_overlap_what_it_is_still_writing() {
        // Distance 1, length 5: the format's way of encoding a run, and it only works if each
        // byte is read back after being written rather than the whole span being copied at once.
        let mut stream = Vec::new();
        stream.extend_from_slice(&header(0x10, 6));
        stream.push(0b0100_0000); // one literal, then a reference
        stream.push(b'Z');
        stream.push(0x20); // length 5
        stream.push(0x00); // distance 1
        let mut bus = bus_with(0x100, &stream);
        run(&mut bus, 0x11, 0x100, 0x800);
        assert_eq!(read_out(&mut bus, 0x800, 6), b"ZZZZZZ".to_vec());
    }

    #[test]
    fn a_back_reference_past_the_start_stops_rather_than_panicking() {
        let mut stream = Vec::new();
        stream.extend_from_slice(&header(0x10, 16));
        stream.push(0b1000_0000); // a reference as the very first block
        stream.push(0x10);
        stream.push(0x40); // distance 65, with nothing written yet
        let mut bus = bus_with(0x100, &stream);
        run(&mut bus, 0x11, 0x100, 0x800);
        // Nothing was written, and the machine is still standing.
        assert_eq!(read_out(&mut bus, 0x800, 4), vec![0, 0, 0, 0]);
    }

    #[test]
    fn the_vram_variant_writes_halfwords_so_no_byte_is_lost() {
        // A byte write to VRAM is doubled into both halves of its halfword, so a decompressor
        // that emitted bytes would corrupt every other one. This checks the bytes come out in
        // order, which they cannot if that happened.
        let mut stream = Vec::new();
        stream.extend_from_slice(&header(0x10, 4));
        stream.push(0b0000_0000);
        stream.extend_from_slice(&[0x11, 0x22, 0x33, 0x44]);
        let mut bus = bus_with(0x100, &stream);
        run(&mut bus, 0x12, 0x100, 0x800);
        assert_eq!(bus.read16(0x800), 0x2211);
        assert_eq!(bus.read16(0x802), 0x4433);
    }

    #[test]
    fn run_length_biases_its_two_forms_differently() {
        // The literal form's count is biased by one and the repeated form's by three. Using one
        // bias for both produces output that is subtly the wrong length.
        let mut stream = Vec::new();
        stream.extend_from_slice(&header(0x30, 7));
        stream.push(0x02); // literal, count 3
        stream.extend_from_slice(b"abc");
        stream.push(0x80 | 0x01); // repeated, count 4
        stream.push(b'!');
        let mut bus = bus_with(0x100, &stream);
        run(&mut bus, 0x14, 0x100, 0x800);
        assert_eq!(read_out(&mut bus, 0x800, 7), b"abc!!!!".to_vec());
    }

    #[test]
    fn the_difference_filters_accumulate() {
        let mut stream = Vec::new();
        stream.extend_from_slice(&header(0x81, 5));
        stream.extend_from_slice(&[10, 5, 250, 1, 0]);
        let mut bus = bus_with(0x100, &stream);
        run(&mut bus, 0x16, 0x100, 0x800);
        // Each byte is the running sum, wrapping.
        assert_eq!(read_out(&mut bus, 0x800, 5), vec![10, 15, 9, 10, 10]);
    }

    #[test]
    fn the_sixteen_bit_difference_filter_accumulates_halfwords() {
        let mut stream = Vec::new();
        stream.extend_from_slice(&header(0x82, 6));
        for value in [0x1000u16, 0x0234, 0xFFFF] {
            stream.extend_from_slice(&value.to_le_bytes());
        }
        let mut bus = bus_with(0x100, &stream);
        run(&mut bus, 0x18, 0x100, 0x800);
        assert_eq!(bus.read16(0x800), 0x1000);
        assert_eq!(bus.read16(0x802), 0x1234);
        assert_eq!(bus.read16(0x804), 0x1233);
    }

    #[test]
    fn a_header_claiming_an_absurd_size_is_refused_rather_than_allocated() {
        // A mis-set source pointer reads a header out of whatever was there. Trusting it turns a
        // game bug into an out-of-memory.
        let mut stream = Vec::new();
        stream.extend_from_slice(&header(0x10, 0x00FF_FFFF));
        let mut bus = bus_with(0x100, &stream);
        run(&mut bus, 0x11, 0x100, 0x800);
        assert_eq!(read_out(&mut bus, 0x800, 4), vec![0, 0, 0, 0]);
    }

    #[test]
    fn huffman_decodes_an_eight_bit_tree() {
        // A two-symbol tree: bit 0 -> 'A', bit 1 -> 'B'. The root sits at tree_start+1, and its
        // children at (root & !1) + offset*2 + 2, which for offset 0 is tree_start + 2 and 3.
        let mut stream = Vec::new();
        stream.extend_from_slice(&header(0x28, 4)); // 8-bit symbols, 4 bytes out
        stream.push(0x01); // tree size: (1+1)*2 = 4 bytes
        stream.push(0xC0); // root: both children are leaves
        stream.push(b'A');
        stream.push(b'B');
        // The tree is exactly (size+1)*2 = 4 bytes: the size byte, the root, and the two leaves.
        // The bitstream begins straight after it, and a stray padding byte here would be read as
        // the first word of the stream.
        // Bitstream: A B B A, then zeroes. MSB first.
        let bits: u32 = 0b0110_0000 << 24;
        stream.extend_from_slice(&bits.to_le_bytes());

        let mut bus = bus_with(0x100, &stream);
        run(&mut bus, 0x13, 0x100, 0x800);
        assert_eq!(read_out(&mut bus, 0x800, 4), b"ABBA".to_vec());
    }

    #[test]
    fn a_huffman_stream_that_does_not_terminate_gives_up_rather_than_hanging() {
        // A tree whose root points at itself walks in a circle forever.
        let mut stream = Vec::new();
        stream.extend_from_slice(&header(0x28, 64));
        stream.push(0x01);
        stream.push(0x00); // no leaf flags, offset 0: descends and never emits
        stream.extend_from_slice(&[0x00, 0x00, 0x00]);
        stream.extend_from_slice(&[0u8; 64]);
        let mut bus = bus_with(0x100, &stream);
        run(&mut bus, 0x13, 0x100, 0x800);
        // The point is simply that this returned.
    }

    #[test]
    fn register_ram_reset_names_the_regions_its_mask_selects() {
        assert_eq!(reset_regions(0), Vec::new());
        assert_eq!(reset_regions(1 << 2), vec![(0x0500_0000, 0x400)]);
        assert_eq!(reset_regions(1 << 4), vec![(0x0700_0000, 0x400)]);
        assert_eq!(
            reset_regions(1 << 3),
            vec![(0x0600_0000, 0x0001_8000)],
            "VRAM is 96 KiB, not the 128 KiB its window spans"
        );
        // Internal WRAM stops short of its last 0x200 bytes, which hold the interrupt handler
        // pointer. Clearing those would send the next interrupt to address zero.
        assert_eq!(reset_regions(1 << 1), vec![(0x0300_0000, 0x7E00)]);
        // Several bits at once give several regions, in address order.
        assert_eq!(reset_regions(0x1F).len(), 5);
        // The I/O bits name no memory region; they are handled separately and deliberately not.
        assert_eq!(reset_regions(0xE0), Vec::new());
    }

    #[test]
    fn an_unimplemented_call_is_still_a_no_op_but_is_now_loud() {
        // The behaviour that cost a session: a call that silently does nothing looks exactly like
        // a working machine right up until the screen stays black.
        let mut bus = FlatBus::new(0x100);
        let mut cpu = Arm7Tdmi::default();
        cpu.set_reg(0, 0x1234);
        dispatch(&mut cpu, &mut bus, &mut None, 0x20);
        assert_eq!(cpu.reg(0), 0x1234, "registers are left alone");
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
            (0x17, BiosCall::Diff8bitUnFilterVram),
            (0x18, BiosCall::Diff16bitUnFilter),
            (0x01, BiosCall::RegisterRamReset),
            (0x0E, BiosCall::BgAffineSet),
            (0x0F, BiosCall::ObjAffineSet),
        ] {
            assert_eq!(
                BiosCall::from_comment(comment),
                expected,
                "SWI {comment:#04X}"
            );
        }
    }
}

#[cfg(test)]
mod bit_unpack_tests {
    use super::*;
    use crate::bios::tests::FlatBus;
    use cpu_arm7tdmi::BootState;

    /// Write a `BitUnPack` header at `at`: length, source width, destination width, data offset.
    fn header(bus: &mut FlatBus, at: u32, len: u16, src_width: u8, dest_width: u8, offset: u32) {
        bus.write16(at, len);
        bus.write8(at + 2, src_width);
        bus.write8(at + 3, dest_width);
        bus.write32(at + 4, offset);
    }

    fn run(bus: &mut FlatBus, source: u32, destination: u32, info: u32) {
        let mut cpu = Arm7Tdmi::new(BootState::default());
        cpu.set_reg(0, source);
        cpu.set_reg(1, destination);
        cpu.set_reg(2, info);
        dispatch(&mut cpu, bus, &mut None, 0x10);
    }

    #[test]
    fn four_bit_source_to_eight_bit_destination_with_no_offset() {
        // Two source bytes, LSB nibble first: byte0=0x21 -> units 1,2; byte1=0x43 -> units 3,4.
        // Each nibble widens straight into a byte, packed LSB-unit-first into the output word.
        let mut bus = FlatBus::new(0x100);
        header(&mut bus, 0x00, 2, 4, 8, 0);
        bus.write8(0x10, 0x21);
        bus.write8(0x11, 0x43);
        run(&mut bus, 0x10, 0x20, 0x00);
        assert_eq!(bus.read32(0x20), 0x0403_0201);
    }

    #[test]
    fn two_bit_source_to_four_bit_destination_with_an_offset_that_skips_zero_units() {
        // Eight 2-bit units across two bytes: [2,0,1,3, 0,0,1,2] (unit0 first, LSB of each byte).
        // Offset 5, zero-data flag clear: a raw zero stays zero, everything else gains the
        // offset. Verified by hand: transformed = [0,6,7,8, 0,0,6,7], each packed into its own
        // nibble of the output word, unit0 in the lowest nibble.
        let mut bus = FlatBus::new(0x100);
        header(&mut bus, 0x00, 2, 2, 4, 5);
        bus.write8(0x10, 0b11_10_01_00); // units, MSB pair first: 3,2,1,0
        bus.write8(0x11, 0b10_01_00_00); // units: 2,1,0,0
        run(&mut bus, 0x10, 0x20, 0x00);
        assert_eq!(bus.read32(0x20), 0x7600_8760);
    }

    #[test]
    fn one_bit_source_to_thirty_two_bit_destination_with_the_zero_data_flag_set() {
        // Every unit is a single bit, and each becomes its own destination word, so this is the
        // clearest possible demonstration of the zero-data flag: with it set, a raw 0 bit is
        // *not* left alone, it gets the offset too.
        let mut bus = FlatBus::new(0x100);
        header(&mut bus, 0x00, 1, 1, 32, 0x1000 | 0x8000_0000);
        bus.write8(0x10, 0b1010_0101); // bit0 (LSB) first: 1,0,1,0,0,1,0,1
        run(&mut bus, 0x10, 0x20, 0x00);
        let expected = [
            0x1001, 0x1000, 0x1001, 0x1000, 0x1000, 0x1001, 0x1000, 0x1001,
        ];
        for (index, value) in expected.into_iter().enumerate() {
            assert_eq!(bus.read32(0x20 + index as u32 * 4), value, "word {index}");
        }
    }

    #[test]
    fn an_unsupported_bit_width_refuses_rather_than_shifting_by_a_meaningless_amount() {
        let mut bus = FlatBus::new(0x100);
        header(&mut bus, 0x00, 1, 3, 8, 0); // 3 is not a supported source width
        bus.write8(0x10, 0xFF);
        run(&mut bus, 0x10, 0x20, 0x00);
        assert_eq!(bus.read32(0x20), 0, "nothing was written");
    }
}

#[cfg(test)]
mod soft_reset_tests {
    use super::*;
    use crate::bios::tests::FlatBus;
    use cpu_arm7tdmi::BootState;

    /// Everything a fresh boot leaves untouched: register banks with something recognisably not
    /// zero, and the region `SoftReset` is documented to clear.
    fn dirty(cpu: &mut Arm7Tdmi, bus: &mut FlatBus) {
        for index in 0..=12 {
            cpu.regs.write(Mode::Supervisor, index, 0xDEAD_BEEF);
        }
        cpu.regs.write(Mode::Supervisor, 13, 0x1111_1111);
        cpu.regs.write(Mode::Supervisor, 14, 0x2222_2222);
        cpu.regs.set_spsr(Mode::Supervisor, Psr::new(0xFFFF_FFFF));
        cpu.regs.write(Mode::Irq, 13, 0x3333_3333);
        cpu.regs.write(Mode::Irq, 14, 0x4444_4444);
        cpu.regs.set_spsr(Mode::Irq, Psr::new(0xFFFF_FFFF));
        cpu.regs.write(Mode::System, 13, 0x5555_5555);
        cpu.cpsr.set_mode(Mode::Supervisor);
        cpu.cpsr.set_thumb(true);
        cpu.cpsr.set_irq_disabled(true);
        cpu.cpsr.set_fiq_disabled(false);

        for offset in (0..0x200u32).step_by(4) {
            bus.write32(0x0300_7E00 + offset, 0xCCCC_CCCC);
        }
    }

    #[test]
    fn soft_reset_restores_the_documented_post_boot_state_and_jumps_to_rom() {
        let mut cpu = Arm7Tdmi::new(BootState::default());
        // Large enough to cover the region actually under test; SoftReset never touches
        // anything past 0x0300_8000.
        let mut bus = FlatBus::new(0x0300_8000);
        dirty(&mut cpu, &mut bus);
        bus.write8(SOFT_RESET_FLAG, 0); // 0 selects the cartridge

        dispatch(&mut cpu, &mut bus, &mut None, 0x00);

        for offset in (0..0x200u32).step_by(4) {
            assert_eq!(
                bus.read32(0x0300_7E00 + offset),
                0,
                "byte {offset:#X} of the cleared region"
            );
        }
        for index in 0..=12 {
            assert_eq!(cpu.regs.read(Mode::Supervisor, index), 0, "r{index}");
        }
        assert_eq!(cpu.regs.read(Mode::Supervisor, 13), SP_SUPERVISOR);
        assert_eq!(cpu.regs.read(Mode::Supervisor, 14), 0, "LR_svc");
        assert_eq!(cpu.regs.spsr(Mode::Supervisor), Some(Psr::default()));
        assert_eq!(cpu.regs.read(Mode::Irq, 13), SP_IRQ);
        assert_eq!(cpu.regs.read(Mode::Irq, 14), 0, "LR_irq");
        assert_eq!(cpu.regs.spsr(Mode::Irq), Some(Psr::default()));
        assert_eq!(cpu.regs.read(Mode::System, 13), SP_SYSTEM);
        assert_eq!(cpu.cpsr.mode(), Mode::System);
        assert!(!cpu.cpsr.thumb(), "the entry point runs in ARM state");
        assert!(!cpu.cpsr.irq_disabled(), "the BIOS has finished with them");
        assert!(cpu.cpsr.fiq_disabled());
        assert_eq!(cpu.regs.pc(), CARTRIDGE_ENTRY);
    }

    #[test]
    fn a_nonzero_reset_flag_jumps_to_ram_instead_of_the_cartridge() {
        let mut cpu = Arm7Tdmi::new(BootState::default());
        let mut bus = FlatBus::new(0x0300_8000);
        bus.write8(SOFT_RESET_FLAG, 1);
        dispatch(&mut cpu, &mut bus, &mut None, 0x00);
        assert_eq!(cpu.regs.pc(), 0x0200_0000);
    }

    #[test]
    fn the_reset_flag_is_read_before_the_region_containing_it_is_cleared() {
        // The flag lives inside the 0x200-byte span this call zeroes. Reading it after clearing
        // would always see 0 and always jump to the cartridge, silently losing the RAM case.
        let mut cpu = Arm7Tdmi::new(BootState::default());
        let mut bus = FlatBus::new(0x0300_8000);
        bus.write8(SOFT_RESET_FLAG, 0xFF);
        dispatch(&mut cpu, &mut bus, &mut None, 0x00);
        assert_eq!(
            cpu.regs.pc(),
            0x0200_0000,
            "the flag was honoured, not already cleared"
        );
    }
}

#[cfg(test)]
mod arc_tan_tests {
    use super::*;
    use crate::bios::tests::FlatBus;
    use cpu_arm7tdmi::BootState;

    fn run(input: i32) -> u32 {
        let mut cpu = Arm7Tdmi::new(BootState::default());
        let mut bus = FlatBus::new(16);
        cpu.set_reg(0, input as u32);
        dispatch(&mut cpu, &mut bus, &mut None, 0x09);
        cpu.reg(0)
    }

    #[test]
    fn arc_tan_matches_the_hardware_polynomial_at_the_quarter_turns() {
        // Q14 fixed point: 0x4000 is a tangent of 1.0, and the documented output range maps that
        // to +/-45 degrees -- 0x2000 of a 0x10000 full turn, the same convention `affine_set`'s
        // angle argument uses.
        assert_eq!(run(0), 0, "tan 0 is angle 0");
        assert_eq!(run(0x4000), 0x2000, "tan 1.0 is +45 degrees");
        assert_eq!(
            run(-0x4000i32),
            (-0x2000i32) as u32,
            "tan -1.0 is -45 degrees"
        );
    }

    #[test]
    fn arc_tan_is_not_symmetric_around_zero_because_it_is_the_hardware_polynomial() {
        // The polynomial's own fixed-point rounding, not a bug in this transcription: +1 and -1,
        // the smallest representable tangents, round to different-magnitude angles. A true
        // arctangent would be symmetric here; this hardware is not.
        assert_eq!(run(1), 0);
        assert_eq!(run(-1i32), (-1i32) as u32);
    }
}

#[cfg(test)]
mod get_bios_checksum_tests {
    use super::*;
    use crate::bios::tests::FlatBus;
    use cpu_arm7tdmi::BootState;

    #[test]
    fn get_bios_checksum_answers_the_one_well_known_value() {
        let mut cpu = Arm7Tdmi::new(BootState::default());
        let mut bus = FlatBus::new(16);
        dispatch(&mut cpu, &mut bus, &mut None, 0x0D);
        assert_eq!(
            cpu.reg(0),
            0xBAAE_187F,
            "the checksum of a genuine GBA/GBA SP BIOS"
        );
    }
}

#[cfg(test)]
mod midi_key_to_freq_tests {
    use super::*;
    use crate::bios::tests::FlatBus;
    use cpu_arm7tdmi::BootState;

    fn run(rate: u32, key: u32, fine_pitch: u32) -> u32 {
        let mut cpu = Arm7Tdmi::new(BootState::default());
        let mut bus = FlatBus::new(16);
        bus.write32(4, rate); // WaveData::freq sits at offset 4
        cpu.set_reg(0, 0);
        cpu.set_reg(1, key);
        cpu.set_reg(2, fine_pitch);
        dispatch(&mut cpu, &mut bus, &mut None, 0x1F);
        cpu.reg(0)
    }

    #[test]
    fn the_target_key_matching_the_stored_reference_key_leaves_the_rate_unchanged() {
        // GBATEK's WaveData.freq is pre-scaled by 2^((180-originalKey)/12); asking for that same
        // key back is an exponent of zero, the one case simple enough to check by eye rather
        // than by running the formula in a second tool.
        assert_eq!(run(4096, 180, 0), 4096);
    }

    #[test]
    fn key_168_is_the_documented_bios_read_trick_and_halves_the_rate() {
        // GBATEK's own example of this call — reading BIOS memory through it without the usual
        // range check — uses key 168 with no fine pitch, twelve semitones below the reference
        // key of 180: exactly one octave, so the exponent is 1 and the rate exactly halves.
        assert_eq!(run(1000, 168, 0), 500);
    }

    #[test]
    fn a_nonzero_fine_pitch_shifts_the_result_by_a_fraction_of_a_semitone() {
        // No clean closed form this time; verified independently by evaluating the same formula
        // in Python rather than by construction, to catch a transcription error in the Rust
        // rather than a matching one in both places.
        assert_eq!(run(8000, 172, 128), 5187);
    }
}
