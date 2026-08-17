//! Game Boy Advance system assembly.
//!
//! Follows prompt 11's proven pattern, adapted for a much larger machine, and built up in the
//! same order: the memory map first, then the register blocks that sit on it, then the
//! assembly that drives them.
//!
//! # Status
//!
//! **Commercial games run.** A cartridge boots, the display renders through the compositor, DMA
//! and timers drive the sound FIFOs, and interrupts reach the game's handler with or without a
//! BIOS. A save state round-trips frame-exactly and two runs of the same ROM are identical. All
//! four `gba-suite` ROMs pass — the instruction set in both states, memory, and the BIOS's
//! documented call and open-bus behaviour — and a real game plays in the window at a measured
//! 100% speed with no dropped frames or audio samples.
//!
//! What is not done, in rough order of how much it matters:
//!
//! - **Three of the four `apu-shared` PSG channels are now mixed in**, alongside the two FIFO
//!   channels: [`psg`] is a new register layer — halfwords from `0x0400_0060` with gaps between
//!   them, unlike the Game Boy's contiguous `NR10`-`NR52` — over the same `SquareChannel` and
//!   `NoiseChannel` types `system-gb::apu` already uses, at CGB semantics throughout (no
//!   `GbModel`-style gating; the obstacle that gating was once thought to be never applied here).
//!   `SOUNDCNT_H` bits 0-1 attenuate the PSG's own `SOUNDCNT_L` master volume again, multiplicatively
//!   rather than one overriding the other, before it meets direct sound in `GbaSystemBus::generate_samples`.
//!   **Channel 3 (wave) is still missing**, deliberately: its wave RAM is two sixteen-byte banks
//!   with the CPU seeing whichever is not playing, a real difference from `apu_shared::WaveChannel`'s
//!   single-bank model that needs its own decision rather than a rushed reuse, so `SOUND3CNT_L/H/X`
//!   and the wave RAM window are left unclaimed — reading as unmapped, not as a channel that
//!   happens to be quiet. `SOUNDBIAS` is likewise still unclaimed, so `bios::dispatch`'s
//!   `SoundBias` call still recognises but does not act on it.
//! - **EEPROM saves are reported as absent** rather than emulated; SRAM and Flash work, and a real
//!   cartridge's chip and size are detected correctly. No game has yet been played far enough to
//!   write a save file, so the path from a game's write to a file on disk is unverified.
//! - **Mosaic is implemented for text backgrounds and ordinary sprites**, both axes, as the
//!   sample-and-hold hardware defines it — BG mosaic quantizes the *screen* position before
//!   sampling; OBJ mosaic quantizes the sprite's own *local* position, so a mosaiced sprite looks
//!   the same wherever it is on screen. **Not covered: affine backgrounds, the bitmap layer in
//!   modes 3-5, and affine (rotated or scaled) sprites** — all three sample through per-scanline
//!   state that is accumulated once and not kept for any line but the latest, so a vertical
//!   mosaic block would need snapshotting that state at every block boundary, which nothing here
//!   does yet.
//! - The object window is: a sprite whose graphics mode is `ObjectWindow` draws nothing and its
//!   shape is a region instead, with `WINOUT`'s high byte saying what is visible inside it.
//! - **No cartridge GPIO**, so a game with a real-time clock finds none. That is a supported
//!   hardware state rather than a failure — a cartridge with a dead battery behaves the same way
//!   and games handle it — but the clock never advances.
//! - The HLE BIOS in [`bios`] answers every call with a small, well-documented contract:
//!   `Div`/`DivArm`/`Sqrt`/`ArcTan`/`ArcTan2`, `CpuSet`/`CpuFastSet`, `GetBiosChecksum`, all five
//!   decompressors plus `BitUnPack`, `BgAffineSet`/`ObjAffineSet`, `RegisterRamReset`,
//!   `SoftReset`, `Halt`/`Stop`/`IntrWait`/`VBlankIntrWait`, and `MidiKey2Freq`. `SoundBias` is
//!   recognised — MP2K's own init calls it — but does not ramp `SOUNDBIAS`, because nothing in
//!   this crate owns that register yet; see `bios::dispatch`'s `SoundBias` arm. **Not
//!   implemented, deliberately:** the M4A/MP2K sound-engine entry points at `SWI` 0x1A-0x1E and
//!   0x20-0x2A (`SoundDriverInit`/`Main`/`Mode`/`VSync`, the `MusicPlayer*` family,
//!   `SoundChannelClear`) — these are not small pure functions but the BIOS-resident sound
//!   engine itself, and reproducing them means building that engine, not transcribing a
//!   contract — plus `MultiBoot`, `HardReset`, and `CustomHalt`. Every other call changes
//!   nothing rather than guessing, which shows up in a trace instead of surfacing far from its
//!   cause. `bios::intr_wait` is the one call that keeps state between steps, and the one worth
//!   reading before trusting anything about frame pacing.
//!
//! # The failure mode this system keeps producing
//!
//! Every rendering bug found here so far produced a **complete and plausible wrong picture** rather
//! than a missing one, which is far harder to notice than a gap and is why the reference
//! comparisons and a save-state repro matter more than they look. The list, each now covered by a
//! test:
//!
//! - Colour index 0 in a text background drawn as a colour rather than transparent, so the
//!   frontmost layer went opaque and hid everything behind it.
//! - Sprite-versus-background priority decided by the Game Boy's single "behind background" bit
//!   instead of comparing the two priorities, so every sprite won and characters walked over the
//!   text boxes in front of them.
//! - Bit 5 of `WININ`/`WINOUT` treated as a sixth layer when it is the colour-effect enable, so
//!   effects applied inside regions a game had switched them off in and menu panels came out grey.
//! - A text background wrapping at 32x32 whatever its real size, so a larger one never reached its
//!   second screen block and half its content was simply absent.
//! - The object window answered as never covering, so content revealed *through* one vanished.
//! - Windows were applied *after* the line resolved: the winning layer was masked away and
//!   overpainted with the backdrop, instead of being kept out of priority resolution so the next
//!   layer down could win. Every window used to *filter* rather than to hide — text-box interiors,
//!   battle HUDs, a cave's light radius — came out as hard-edged rectangles of flat backdrop. The
//!   contract now lives in `ppu_tile2d::ScanlineBuffer::set`, the one point every renderer commits
//!   a pixel through.
//! - Affine sprites were composited by a second pass that could not see the first, so they ignored
//!   background priority entirely *and* were overwritten by every ordinary sprite regardless of
//!   which was in front — a rotating object punched through the text box before it, and a farther
//!   plain sprite erased a nearer rotated one. Both now share one ordered pass and one
//!   `ppu_tile2d::SpritePass`.
//! - `GraphicsMode::SemiTransparent` was decoded and never read, so shadows, water, reflections
//!   and battle-move flashes rendered as solid blocks. Such a sprite is a blend first target
//!   whatever `BLDCNT` selects, and forces an alpha blend even where `BLDCNT` asks for a
//!   brightness effect.
//! - The alpha-blend second pass excluded every layer `BLDCNT` declared a first target, to find
//!   what lies beneath the winner. That is a different, narrower question from hardware's: where a
//!   layer was declared both a first *and* a second target — a common `BLDCNT` shape — it excluded
//!   itself from being the answer under its own winning pixel, so a translucent sprite over
//!   artwork mixed with the backdrop instead of the artwork it was actually sitting on. The pass
//!   now excludes, per pixel, exactly the layer *that pixel's own winner* came from.
//! - Bitmap modes 3, 4, and 5 were written straight to the framebuffer and the whole scanline
//!   returned before anything else ran. A bitmap mode is background 2 wearing a direct-colour
//!   pixel format, not a separate world: a rotated `BG2PA`-`BG2PD` never rotated the picture
//!   because the matrix was never consulted, a window over it did nothing, the blend unit never
//!   saw it, and every sprite pixel overwrote it with no priority comparison at all — a farther
//!   sprite always won. It now draws into the same buffer as everything else, subject to the same
//!   enable bit, windows, blend unit, and sprite-priority rule.
//!
//! # The bug worth knowing about before touching timing
//!
//! Every memory access was charged three to six times over, so an ARM instruction in internal
//! WRAM cost 13 cycles against hardware's 1. **No test failed and the emulator reported 100%
//! speed throughout**, because a frame is a fixed number of cycles however few instructions fit
//! inside it. What a commercial game lost was nine tenths of its processor, and what that looked
//! like was a frozen picture with the CPU visibly running. See `system::GbaSystemBus::charge`:
//! charge once, at the width the CPU asked for, and charge only the waiting.
//!
//! Fixing that made the wait-state table matter for the first time, which is what exposed the gap
//! next to it: `WAITCNT`'s prefetch bit was read and otherwise ignored, so a game that linked its
//! hot code into a slow ROM window specifically to turn this on and get sequential fetches for one
//! cycle instead of the window's configured cost paid the full price anyway. `waitstates::cost` now
//! tracks whether the previous access was a sequential code fetch with the bit set — see the
//! module's own doc comment for why that single bit, not a queue with a depth, is enough.
//!
//! The second one has the same shape and is worth the same suspicion: `IntrWait` and
//! `VBlankIntrWait` were implemented as a plain halt, so they returned on *any* interrupt rather
//! than the ones named in `r1`. A game that also enables HBlank or a timer — which is most of them
//! — ran its main loop **618 times across three frames where hardware runs it 3**. Again no test
//! failed and the speed reading stayed at 100%; only the emulated machine's sense of a frame was
//! wrong, which makes every downstream symptom look like an unrelated bug. See `bios::intr_wait`.
//!
//! A third has the same shape from the opposite direction: `video::VideoTiming::tick` reported at
//! most one of each edge no matter how many cycles it was given, so a single call spanning more
//! than one scanline — a DMA burst or a long instruction, both routine — rendered only the *last*
//! line crossed and silently dropped the others, advanced the affine layers once instead of once
//! per line, and armed HBlank DMA once instead of once per line. There is a test whose name states
//! the correct behaviour and whose body asserted the bug: `assert_eq!(events.scanline_ready,
//! Some(3), "the most recent one")`. `tick` now advances at most one line per call and reports how
//! many cycles that used; the caller loops, feeding the remainder back in until none is left. See
//! `system::GbaSystemBus::advance`.
//!
//! A fourth is the largest of the four and the same shape again: **DMA was instantaneous and
//! free.** A transfer copied its whole block inside one `while` loop in zero emulated cycles —
//! no 2-cycle startup latency, no per-unit read and write cost, no CPU stall, and neither the
//! display nor the timers moved while it ran. Three further things followed from that, and every
//! one of them was invisible for the same reason the first three were: the machine still produced
//! 228 scanlines of 1232 cycles each, so nothing about a frame looked wrong. The wait states a
//! transfer's own accesses incurred landed in `pending_waits` and were charged to *the next
//! instruction*, which also had its fetch counted as non-sequential because the copy had moved the
//! bus's latch; and `Timers::tick` returned a bitmask, so N overflows inside one call collapsed to
//! one — one FIFO sample popped where hardware popped N. That last one was safe only because
//! `advance` was never called with more than an instruction's handful of cycles, which is exactly
//! what stopped being true.
//!
//! What a game loses is the opposite of the wait-state bug: not processor time but the *absence*
//! of a stall it was counting on. A game that fires a 240-word HDMA on every scanline gets those
//! copies for nothing, so its CPU runs further into each line than hardware allows and every raster
//! effect lands a few cycles early; a game pacing direct sound off a short timer gets one sample
//! per burst instead of twenty, and the queue never drains far enough to ask for a refill.
//! `dma::unit_cycles` and `system::GbaSystemBus::run_transfer` are the fix, and
//! `system::tests::dma_timing` pins all five behaviours.
//!
//! A fifth is the reverse of the fourth: not a stall going missing, but a fast path landing on the
//! wrong cycle while trying to skip one. A halted CPU — a plain `Halt`, or the far more common
//! `IntrWait`/`VBlankIntrWait` retry loop real software spends most of a frame in — used to run
//! `step_instruction` once per cycle until something woke it, which is correct and also up to
//! 280,896 calls a frame that touch nothing. `GbaSystem::halt_fast_forward_cycles` predicts the
//! next enabled edge instead and jumps the bus straight there. The bug was in what "the edge"
//! meant: `video::VideoTiming::tick` only stops at a line boundary, so asking it for the rest of a
//! whole frame in one call sailed straight past a mid-line `HBlank` edge to wherever the line
//! ended, up to 272 cycles late — a game pacing a raster effect off `HBlank` would have woken it a
//! partial scanline after hardware does. `video::VideoTiming::cycles_until_next_edge` caps each
//! request to the edge instead. Found by an equivalence test that runs the fast and slow paths
//! side by side and asserts they land on the identical cycle count and register state, not merely
//! that both eventually complete — the weaker check would have passed with the overshoot still in
//! it, since both paths still terminate.
//!
//! The GBA is the system the *predecessor* project targeted, so prompt 12 sets the bar at "at
//! least as correct and complete as the vendored core it replaces, with the test coverage that
//! core never had". The second half of that is met; the first is unmeasured until the accuracy
//! ROMs land.

#![deny(unsafe_code)]

pub mod affine;
pub mod background;
pub mod bios;
pub mod bitmap;
pub mod cartridge;
pub mod compositor;
pub mod debug;
pub mod dma;
pub mod effects;
pub mod fifo;
pub mod irq;
pub mod keypad;
pub mod memory;
pub mod objects;
pub mod psg;
pub mod system;
pub mod timers;
pub mod video;
pub mod waitstates;

pub use affine::AffineBackground;
pub use background::{Backgrounds, GbaTilemap};
pub use bitmap::bgr555_to_rgba8;
pub use cartridge::Cartridge;
pub use compositor::{Frame, GbaPalette};
pub use dma::DmaController;
pub use effects::{BlendMode, Effects, Layer};
pub use fifo::{DirectSound, SoundFifo};
pub use irq::InterruptController;
pub use keypad::Keypad;
pub use memory::{GbaBus, Region};
pub use objects::{Object, ObjectAttributeMemory};
pub use psg::Psg;
pub use system::{GbaSystem, GbaSystemBus};
pub use timers::Timers;
pub use video::VideoTiming;
pub use waitstates::{Access, WaitControl};
