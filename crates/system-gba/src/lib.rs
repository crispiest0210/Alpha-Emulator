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
//! - **The four `apu-shared` PSG channels are not mixed** alongside the two FIFO channels, so a
//!   game whose music comes through them is silent. What blocks it is smaller than it looks and
//!   was recorded here backwards for a while: the *channels* already live in `apu-shared` and are
//!   directly usable. What lives in `system-gb::apu`, unreachable because `system-*` crates may
//!   not depend on each other, is the **address decode** — and the GBA's is genuinely different
//!   anyway. Its registers are halfwords at `0x0400_0060`, laid out with gaps rather than as the
//!   Game Boy's contiguous `NR10`-`NR52`; its wave RAM is two banks of sixteen bytes with the CPU
//!   seeing whichever is not playing; and it has a 75% volume step the Game Boy lacks. So this is
//!   a new register layer over shared channels, not a copy of an existing one, and the `GbModel`
//!   gating that was called the obstacle does not apply — the GBA follows the CGB rule throughout.
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
//! The second one has the same shape and is worth the same suspicion: `IntrWait` and
//! `VBlankIntrWait` were implemented as a plain halt, so they returned on *any* interrupt rather
//! than the ones named in `r1`. A game that also enables HBlank or a timer — which is most of them
//! — ran its main loop **618 times across three frames where hardware runs it 3**. Again no test
//! failed and the speed reading stayed at 100%; only the emulated machine's sense of a frame was
//! wrong, which makes every downstream symptom look like an unrelated bug. See `bios::intr_wait`.
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
pub use system::{GbaSystem, GbaSystemBus};
pub use timers::Timers;
pub use video::VideoTiming;
pub use waitstates::{Access, WaitControl};
