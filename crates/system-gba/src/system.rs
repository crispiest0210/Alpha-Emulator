//! The assembled Game Boy Advance.
//!
//! Follows `system-gb`'s proven shape: a bus that owns the memory map *and* the timed
//! subsystems, dispatching each I/O address to whichever module owns it, so the CPU sees one
//! bus and knows about none of them.
//!
//! # Booting without a BIOS
//!
//! A real BIOS is supported and none is vendored, for the reasons in the crate docs. Without
//! one the machine starts directly at the cartridge entry point in System mode with the stacks
//! the BIOS would have set up, which is the documented post-boot state.
//!
//! An interrupt then cannot go through the BIOS handler either, so the machine does what the
//! BIOS does: it reads the game's handler address from the top of IWRAM and jumps there. That
//! is not an approximation of the BIOS — it is the one thing the BIOS does that a game can
//! observe, and skipping it leaves every game's interrupt code unreachable.
//!
//! # Wait states are charged, not assumed
//!
//! Each access asks [`WaitControl`] what it cost, and the cycles it waited beyond the one the CPU
//! core already counted are accumulated and charged with the instruction. A flat cost would make a
//! game linked into the slow ROM window run at the speed of one linked into the fast one.
//!
//! Charging happens exactly once per access, in the [`Bus`] method the CPU called — see
//! `GbaSystemBus::charge` for why both halves of that sentence are load-bearing. Getting either
//! wrong does not fail a test: the emulator still produces frames at 100% speed, because a frame is
//! a fixed number of cycles however few instructions fit inside it. What a game loses is processor
//! time, and what that looks like is a title screen that never arrives.
//!
//! # There is no event scheduler here, and that is a decision rather than an omission
//!
//! `core_common::Scheduler` exists and `system-gb` uses it. This machine does not: the ARM core
//! reports an instruction's cost by *returning* it, so an instruction runs to completion and
//! [`GbaSystemBus::advance`] is called afterwards with the total. Nothing can therefore be
//! scheduled *inside* an instruction, and everything that wants to happen mid-instruction has to
//! be driven by hand from wherever the cycles are known.
//!
//! `GbaSystemBus::run_transfer` is that, done deliberately: DMA spends its cycles by calling
//! `run_clocks` between units, so the display and the timers advance through a transfer instead of
//! jumping over it. It is the shape a scheduler would give for free, written out once for the one
//! caller that needed it.
//!
//! **Moving this machine onto the scheduler remains open**, and it is the right long-term
//! direction — it would put DMA, the video edges, and the timers on one ordered queue rather than
//! on a hand-written recursion whose ordering is a property of the call graph. It was deferred
//! here because it is not a change to DMA: it means the CPU core reporting cycles as it spends
//! them rather than at the end, which is a `cpu-arm7tdmi` change that `system-nds` also depends
//! on. Doing it as part of giving DMA a duration would have made a bounded correctness fix into a
//! rewrite of how the machine is clocked.

use cart_common::GbaHeader;
use core_common::{
    AccessKind, AccessLog, AudioSample, Bus, CartridgeError, Cpu, Cycles, FrameOutput, Framebuffer,
    InputState, Savable, StateError, StateReader, StateWriter, System,
};
use cpu_arm7tdmi::{Arm7Tdmi, BootState, Exception, Mode};

use crate::affine::{self, AffineBackground};
use crate::bios;
use crate::cartridge::Cartridge;
use crate::compositor::{self, Frame};
use crate::dma::DmaController;
use crate::effects::Effects;
use crate::fifo::DirectSound;
use crate::irq::{self, InterruptController};
use crate::keypad::Keypad;
use crate::memory::{GbaBus, Region, BIOS_SIZE};
use crate::psg::Psg;
use crate::video::{VideoTiming, SCREEN_HEIGHT, SCREEN_WIDTH};
use crate::waitstates::{Access, WaitControl};
use crate::{background::Backgrounds, timers::Timers};

/// Bumped on any change to what this system serializes.
///
/// 2 added the in-progress `IntrWait` mask, which a state taken mid-wait needs to resume without
/// discarding the flags it was waiting on a second time. 3 added `GbaBus::bios_open_bus`. 4 added
/// the PSG: three channels, its register block, and the divider and frame sequencer that clock
/// them.
const STATE_VERSION: u32 = 4;

/// Cycles in one frame: 228 scanlines of 1232.
pub const FRAME_CYCLES: u64 = 280_896;
/// The CPU's clock.
pub const CLOCK_HZ: u64 = 16_777_216;

/// Where a cartridge's code begins.
///
/// `pub(crate)`: `bios::soft_reset` jumps here too, since a `SoftReset` is documented as
/// re-entering the machine exactly where a cold boot without a BIOS does.
pub(crate) const CARTRIDGE_ENTRY: u32 = 0x0800_0000;
/// Stacks the BIOS sets up before handing control to the game.
///
/// `pub(crate)` for the same reason as [`CARTRIDGE_ENTRY`]: `SoftReset` sets up the identical
/// three stacks, because a reset is documented as leaving the machine in the same state a cold
/// boot does.
pub(crate) const SP_SYSTEM: u32 = 0x0300_7F00;
pub(crate) const SP_IRQ: u32 = 0x0300_7FA0;
pub(crate) const SP_SUPERVISOR: u32 = 0x0300_7FE0;

/// The four moments GBATEK documents a real BIOS's own last-fetched opcode for, and the constant
/// value each one leaves — "the opcode at `[00DCh+8]` after startup and `SoftReset`, the opcode at
/// `[0134h+8]` during IRQ execution, and opcode at `[013Ch+8]` after IRQ execution, and opcode at
/// `[0188h+8]` after SWI execution." A no-BIOS machine never executes the real instructions that
/// produce these, so the four HLE paths that stand in for those moments stamp the constant
/// directly instead. See `memory::GbaBus::bios_open_bus`.
const BIOS_OPCODE_AFTER_STARTUP: u32 = 0xE129_F000;
const BIOS_OPCODE_AFTER_SWI: u32 = 0xE3A0_2004;
const BIOS_OPCODE_DURING_IRQ: u32 = 0xE25E_F004;
const BIOS_OPCODE_AFTER_IRQ: u32 = 0xE55E_C002;

/// Everything the CPU can reach.
pub struct GbaSystemBus {
    pub memory: GbaBus,
    pub cartridge: Cartridge,
    pub video: VideoTiming,
    pub backgrounds: Backgrounds,
    /// The two affine layers, 2 and 3.
    pub affine: [AffineBackground; 2],
    pub timers: Timers,
    pub dma: DmaController,
    pub effects: Effects,
    pub irq: InterruptController,
    pub keypad: Keypad,
    /// The debugger's access recorder. Records nothing until a watchpoint arms it, and is kept out of
    /// save states: it is a debugging aid, not machine state.
    pub watch: AccessLog,
    pub sound: DirectSound,
    /// The other half of the sound unit: the four Game Boy channels behind the register block at
    /// `0x0400_0060`. Separate from [`Self::sound`] because they share nothing but a master
    /// enable and the final mix.
    pub psg: Psg,
    pub waits: WaitControl,

    framebuffer: Framebuffer,
    samples: Vec<AudioSample>,
    drained: Vec<AudioSample>,
    /// Fixed-point accumulator for sample generation, in the same shape as the Game Boy's.
    sample_accumulator: u64,
    /// Set when the video hardware reached vertical blanking during this slice.
    frame_ready: bool,

    /// Wait-state cycles owed by accesses made during the current instruction.
    ///
    /// Accumulated rather than charged as they happen, because the ARM core reports an
    /// instruction's own cost by returning it and advancing mid-instruction would let a
    /// scheduled event fire between two halves of one access.
    pending_waits: u32,
    /// The address after the last access, for deciding whether the next one is sequential.
    next_sequential: u32,
    /// Cycles DMA has stalled the CPU for, not yet reported to the caller.
    dma_cycles: u32,
    /// Whether a transfer is running, so nothing re-enters `run_pending_dma` inside one.
    in_dma: bool,
    /// Whether the next bus access is the CPU fetching its next instruction.
    ///
    /// Set just before [`Cpu::step`](core_common::Cpu) is called and consumed by the first
    /// [`Self::charge`] that follows — an ARM or Thumb instruction is always exactly one fetch
    /// before whatever data accesses it goes on to make, so "the first access this step" and "the
    /// fetch" are the same thing. [`WaitControl::cost`] needs to tell fetches from data accesses
    /// apart to answer whether the ROM prefetch buffer applies.
    awaiting_fetch: bool,
}

impl GbaSystemBus {
    fn new(cartridge: Cartridge, bios: Option<Vec<u8>>) -> Self {
        Self {
            memory: GbaBus::new(bios),
            cartridge,
            video: VideoTiming::new(),
            backgrounds: Backgrounds::new(),
            affine: [AffineBackground::new(); 2],
            timers: Timers::new(),
            dma: DmaController::new(),
            effects: Effects::new(),
            irq: InterruptController::new(),
            keypad: Keypad::new(),
            watch: AccessLog::new(),
            sound: DirectSound::new(),
            psg: Psg::new(),
            waits: WaitControl::new(),
            framebuffer: Framebuffer::new(SCREEN_WIDTH, SCREEN_HEIGHT),
            samples: Vec::with_capacity(1024),
            drained: Vec::with_capacity(1024),
            sample_accumulator: 0,
            frame_ready: false,
            pending_waits: 0,
            next_sequential: 0,
            dma_cycles: 0,
            in_dma: false,
            awaiting_fetch: true,
        }
    }

    /// Charge an access against the wait-state table.
    ///
    /// A ROM access that continues from the previous address is cheaper, because the cartridge
    /// bus keeps its latch — so this tracks where the last one ended rather than asking the
    /// caller, which would mean every call site knowing about cartridge timing.
    ///
    /// # Why this adds one cycle less than the access costs
    ///
    /// The two halves of the machine's cycle count meet here, and they overlap by exactly one
    /// cycle per access. The ARM7TDMI core reports an instruction as a count of S, N, and I
    /// cycles summed at one each — so the *access itself* is already in the number
    /// [`Cpu::step`](core_common::Cpu) returns. [`WaitControl::cost`] then reports what the
    /// same access costs including that first cycle. Adding both charged every access twice.
    ///
    /// So the instruction's cost is `cpu_cycles + Σ(cost − 1)`: the core's own accounting, plus
    /// the cycles each access *waited* beyond the one the core already counted. An ARM
    /// data-processing instruction in internal WRAM then costs the 1 cycle hardware charges,
    /// and the same instruction fetched from the cartridge at the default wait-state setting
    /// costs 6, which is two 16-bit accesses at two wait states each.
    ///
    /// # Why this is called once per access and not once per byte
    ///
    /// A 32-bit access to a 16-bit bus is two bus cycles, and `cost` already says so. Charging
    /// again inside the halfword and byte routing this decomposes into counted the same access up
    /// to six times: a word read from internal WRAM cost 6 cycles rather than 1.
    fn charge(&mut self, addr: u32, width: u32) {
        let access = if addr == self.next_sequential {
            Access::Sequential
        } else {
            Access::NonSequential
        };
        let is_fetch = std::mem::take(&mut self.awaiting_fetch);
        self.pending_waits += self
            .waits
            .cost(addr, width, access, is_fetch)
            .saturating_sub(1);
        self.next_sequential = addr.wrapping_add(width);
    }

    /// Take the wait-state cycles owed since the last call.
    pub fn take_pending_waits(&mut self) -> u32 {
        std::mem::take(&mut self.pending_waits)
    }

    /// The address a following access would have to start at to count as sequential.
    ///
    /// Exposed so a test can say how many accesses actually happened, which is the only externally
    /// visible trace of an access that should not have been made at all.
    pub fn next_sequential_address(&self) -> u32 {
        self.next_sequential
    }

    /// Advance every clocked subsystem and act on what they report.
    ///
    /// A transfer started along the way spends cycles of its own on top of `cycles`; those are
    /// reported separately through [`Self::take_dma_cycles`], because the CPU was stalled for them
    /// and its own instruction cost cannot absorb them.
    pub fn advance(&mut self, cycles: u32) {
        self.run_clocks(cycles);
    }

    /// Cycles DMA has stalled the CPU for since the last call.
    ///
    /// Taken rather than read, and separate from the argument to [`Self::advance`], because a
    /// transfer can begin in either of two places: inside `advance`, from an HBlank or a FIFO
    /// request, or inside the CPU's own instruction, from the store that set an enable bit. Both
    /// are time the machine spent with the CPU held off the bus.
    pub fn take_dma_cycles(&mut self) -> u32 {
        std::mem::take(&mut self.dma_cycles)
    }

    /// Advance timers and video by `cycles`, acting on every edge they report.
    ///
    /// Called re-entrantly by design: [`Self::run_transfer`] drives this once per unit it moves,
    /// so a DMA burst worth several scanlines advances the display *through* the copy instead of
    /// after it. `run_pending_dma`'s guard bounds the recursion at one level.
    ///
    /// `video.tick` never advances past the next line boundary, so a step spanning several lines
    /// — a long CPU instruction or a DMA burst routinely covers more than one — is fed back in a
    /// loop here rather than asked to report every edge from one call. That used to be a single
    /// `VideoEvents` covering the whole span, which has no way to hold more than one scanline: a
    /// three-line step rendered only the last of them, advanced the affine layers once instead of
    /// three times, and armed HBlank DMA once instead of three times. Looping the small,
    /// fixed-size event instead costs nothing extra on the overwhelmingly common case of a step
    /// that does not cross a line at all — one call, one no-op check — and is exact on the rare
    /// one that does.
    fn run_clocks(&mut self, cycles: u32) {
        let mut remaining = cycles;
        while remaining > 0 {
            let (events, consumed) = self.video.tick(remaining);
            remaining -= consumed;
            // Ticked by the same chunk the display just took rather than by the whole span up
            // front: a timer overflow three scanlines into a burst has to raise its interrupt
            // three scanlines in, not before the first line was drawn.
            self.tick_timers(consumed);
            self.irq.raise(self.video.interrupt_sources(&events));

            if events.frame_started {
                // The reference point is reloaded from the registers at the top of a frame and
                // accumulates from there; see the affine module for why that matters.
                for layer in &mut self.affine {
                    layer.begin_frame();
                }
            }
            if let Some(line) = events.scanline_ready {
                self.render_line(line as u32);
                // Advance *after* drawing: the line just drawn used the position it started with.
                for layer in &mut self.affine {
                    layer.advance_line();
                }
            }
            // Hardware does not run HBlank DMA during vertical blanking, only the interrupt fires
            // there — and `entered_hblank` alone cannot tell the two apart, since it is set on
            // all 228 lines. `scanline_ready` already carries the distinction: it is `Some`
            // precisely when this hblank belongs to one of the 160 visible lines, computed by
            // `video.tick` at the exact instant hblank was entered. Reusing it here is more
            // robust than re-testing `vcount` after the fact, which by this point may already
            // have been advanced onto the next line by `advance_line`.
            if events.entered_hblank && events.scanline_ready.is_some() {
                self.dma.on_hblank();
            }
            if events.entered_vblank {
                self.dma.on_vblank();
                self.frame_ready = true;
            }

            // Run before the next line, not after the whole step: a scroll register an HBlank
            // transfer just updated has to be in place before the line after it renders, and
            // deferring every line's DMA to the end of a multi-line step would render all but the
            // first from registers none of them had actually seen updated yet.
            self.run_pending_dma();
        }
        // The PSG advances by the whole span at once rather than per line: nothing it does raises
        // an interrupt or moves another subsystem, so the only thing that can observe where inside
        // the span a duty step landed is the sample generator on the next line.
        self.psg.tick(cycles);
        self.generate_samples(cycles as u64);
    }

    /// Advance the four timers and act on whatever overflowed.
    fn tick_timers(&mut self, cycles: u32) {
        let overflowed = self.timers.tick(cycles);
        if !overflowed.any() {
            return;
        }
        let asked = self.timers.interrupts(&overflowed);
        for channel in 0..4 {
            if asked & (1 << channel) != 0 {
                self.irq.raise(irq::source::timer(channel));
            }
        }
        // A timer overflow is what advances a direct-sound channel, and draining one may
        // leave it needing a refill — which is a DMA request, not an audio event. Iterated
        // directly rather than collected first: `refill_requests` borrows only `self.sound`,
        // a different field from the `self.dma` the loop body needs mutably, so the borrow
        // checker accepts the two side by side without a `Vec` to hold the gap open.
        self.sound.on_timer_overflow(&overflowed);
        for address in self.sound.refill_requests() {
            self.dma.on_fifo_empty(address);
        }
    }

    fn render_line(&mut self, line: u32) {
        // The framebuffer is taken out so the rest of the bus can be borrowed immutably by the
        // renderer, then put back. Cheaper than threading a lifetime through the compositor for
        // the sake of one call per scanline.
        let mut framebuffer = std::mem::replace(&mut self.framebuffer, Framebuffer::new(1, 1));
        let frame = Frame {
            video: &self.video,
            backgrounds: &self.backgrounds,
            affine: &self.affine,
            effects: &self.effects,
            vram: self.memory.vram(),
            palette: self.memory.palette(),
            oam: self.memory.oam(),
        };
        compositor::render_scanline(&frame, line, &mut framebuffer);
        self.framebuffer = framebuffer;
    }

    /// Perform every transfer that is ready, highest priority first.
    ///
    /// # Why this cannot re-enter itself
    ///
    /// Three separate paths lead here, and two of them can fire while a transfer is already
    /// running: the write hook that starts an immediate transfer the instant an enable bit is set
    /// — which a transfer whose destination lands in the DMA register block triggers on itself —
    /// and [`Self::run_clocks`], which this now calls once per unit moved and which ends every
    /// iteration by asking for pending DMA. Both used to recurse. The guard turns that into what
    /// hardware does instead: the running transfer finishes, and the loop below picks up whatever
    /// became ready in the meantime, still in channel-priority order because
    /// [`DmaController::take_transfer`] rescans from channel 0 every call.
    ///
    /// What that does *not* model is preemption. A higher-priority channel arming mid-copy waits
    /// for the block to finish rather than interrupting it; see the [`crate::dma`] module docs.
    fn run_pending_dma(&mut self) {
        // Asked after every instruction, and the answer is almost always no.
        if !self.dma.any_armed() || self.in_dma {
            return;
        }
        self.in_dma = true;

        // A transfer's accesses are charged to the transfer, not to whatever the CPU does next.
        // Every `read32`/`write32` below goes through `charge`, which accumulates wait states into
        // `pending_waits` and moves `next_sequential`; leaving either alone handed the instruction
        // *after* a copy the whole burst's wait states, and a spurious non-sequential fetch on top
        // — the CPU's own access stream is continuous across a bus cycle it never made. The cost
        // of the handover is charged where it belongs, in the transfer's startup latency.
        let waits = self.pending_waits;
        let sequential = self.next_sequential;

        let mut spent = 0u32;
        while let Some(transfer) = self.dma.take_transfer() {
            spent += self.run_transfer(&transfer);
            // A repeating HBlank channel whose block is longer than a scanline re-arms itself
            // faster than it drains, which on hardware is a machine that never gives the bus back.
            // Bounded here so the emulator makes progress and can be traced instead of hanging:
            // the channel stays armed and runs again at the next call.
            if spent >= FRAME_CYCLES as u32 {
                break;
            }
        }
        self.dma_cycles += spent;

        self.pending_waits = waits;
        self.next_sequential = sequential;
        self.in_dma = false;
    }

    /// Move one block, advancing the machine through it. Returns the cycles it took.
    ///
    /// The clock runs between units rather than after the whole block, which is the entire point:
    /// a 240-word copy is most of a scanline, and a display that stood still through it would put
    /// every HBlank the copy spans at the wrong cycle — and with it every HDMA the game hangs off
    /// one. Time is spent *after* each unit moves, so a scanline rendered inside the copy sees the
    /// bytes that had actually arrived by then.
    fn run_transfer(&mut self, transfer: &crate::dma::Transfer) -> u32 {
        let mut source = transfer.source;
        let mut destination = transfer.destination;

        let startup = crate::dma::startup_cycles(source, destination);
        self.run_clocks(startup);
        let mut spent = startup;

        // The first unit reads and writes non-sequentially; every unit after it walks on from
        // where the last one left off, on both streams independently.
        let mut access = Access::NonSequential;
        for _ in 0..transfer.words {
            let cost = crate::dma::unit_cycles(
                &mut self.waits,
                source,
                destination,
                transfer.unit,
                access,
            );
            if transfer.unit == 4 {
                let value = self.read32(source);
                self.write32(destination, value);
            } else {
                let value = self.read16(source);
                self.write16(destination, value);
            }
            self.run_clocks(cost);
            spent += cost;
            source = step(source, transfer.source_step, transfer.unit);
            destination = step(destination, transfer.destination_step, transfer.unit);
            access = Access::Sequential;
        }

        if transfer.raise_irq {
            self.irq.raise(irq::source::dma(transfer.channel));
        }
        spent
    }

    /// Mix both halves of the sound unit into output samples.
    ///
    /// # Two volume controls in series, not one
    ///
    /// The PSG has already been scaled by its own master volume, `SOUNDCNT_L`'s three bits a side.
    /// `SOUNDCNT_H` bits 0-1 then attenuate the whole PSG mix again — to a quarter, a half, or not
    /// at all — before it meets direct sound. The two cascade on hardware, so they multiply here;
    /// treating either as the volume would make a game that sets one to a quarter and the other to
    /// full play at the wrong level in one direction or the other.
    ///
    /// The fourth `SOUNDCNT_H` setting is prohibited and [`crate::fifo::DirectSoundControl::psg_volume`]
    /// reports it as silence, which is why the numerator is over four rather than a shift.
    fn generate_samples(&mut self, cycles: u64) {
        self.sample_accumulator += cycles * core_common::AUDIO_SAMPLE_RATE as u64;
        while self.sample_accumulator >= CLOCK_HZ {
            self.sample_accumulator -= CLOCK_HZ;
            let (direct_left, direct_right) = self.sound.output();
            let (psg_left, psg_right) = self.psg.output();
            let psg_scale = self.sound.control.psg_volume() as f32 / 4.0;
            // Clamped only at the end: the two halves can each be at full scale, and a game
            // mixing loud direct sound under loud PSG music really does saturate the DAC.
            self.samples.push(AudioSample {
                left: (direct_left + psg_left * psg_scale).clamp(-1.0, 1.0),
                right: (direct_right + psg_right * psg_scale).clamp(-1.0, 1.0),
            });
        }
    }

    pub fn take_samples(&mut self) -> &[AudioSample] {
        std::mem::swap(&mut self.samples, &mut self.drained);
        self.samples.clear();
        &self.drained
    }

    /// Read an I/O register, or `None` if nothing owns that address.
    fn read_io16(&self, addr: u32) -> Option<u16> {
        if let Some(value) = self.video.read16(addr) {
            return Some(value);
        }
        if let Some(value) = self.backgrounds.read16(addr) {
            return Some(value);
        }
        if let Some(value) = self.timers.read16(addr) {
            return Some(value);
        }
        if let Some(value) = self.dma.read16(addr) {
            return Some(value);
        }
        if let Some(value) = self.effects.read16(addr) {
            return Some(value);
        }
        if let Some(value) = self.irq.read16(addr) {
            return Some(value);
        }
        if let Some(value) = self.keypad.read16(addr) {
            return Some(value);
        }
        if let Some(value) = self.sound.read16(addr) {
            return Some(value);
        }
        if let Some(value) = self.psg.read16(addr) {
            return Some(value);
        }
        if WaitControl::owns(addr) {
            return Some(self.waits.read16());
        }
        // The affine registers are write-only, like the scroll registers beside them.
        if affine_layer_of(addr).is_some() {
            return Some(0);
        }
        None
    }

    fn write_io16(&mut self, addr: u32, value: u16) {
        // Taken out of the chain below because the answer has to be attributed: `SOUNDCNT_X`'s
        // bit 7 is one master enable over the whole sound unit, and the register belongs to the
        // direct-sound block. The PSG is told what it now reads rather than being given a second
        // claim on the same address, which would make the order of these two calls load-bearing.
        if self.sound.write16(addr, value).is_some() {
            self.psg.set_power(self.sound.control.sound_enabled());
            return;
        }
        if self.video.write16(addr, value).is_some()
            || self.backgrounds.write16(addr, value).is_some()
            || self.timers.write16(addr, value).is_some()
            || self.dma.write16(addr, value).is_some()
            || self.effects.write16(addr, value).is_some()
            || self.irq.write16(addr, value).is_some()
            || self.keypad.write16(addr, value).is_some()
            || self.psg.write16(addr, value).is_some()
        {
            return;
        }
        if WaitControl::owns(addr) {
            self.waits.write16(value);
            return;
        }
        if let Some((layer, offset)) = affine_layer_of(addr) {
            self.affine[layer].write16(offset, value);
        }
    }
}

/// Which affine layer a register belongs to, and its offset within that layer's block.
fn affine_layer_of(addr: u32) -> Option<(usize, u32)> {
    if (affine::BG2_BASE..affine::BG2_BASE + 0x10).contains(&addr) {
        return Some((0, addr - affine::BG2_BASE));
    }
    if (affine::BG3_BASE..affine::BG3_BASE + 0x10).contains(&addr) {
        return Some((1, addr - affine::BG3_BASE));
    }
    None
}

impl GbaSystemBus {
    /// Whether an address belongs to the DMA register block.
    ///
    /// Checked at the write site because an immediate transfer starts the moment its enable bit
    /// is set, not at the next scheduled tick — a game writes the register and expects the
    /// bytes to be there on the next instruction.
    fn is_dma_register(addr: u32) -> bool {
        DmaController::owns(addr)
    }
}

/// Move a DMA address by one unit.
fn step(addr: u32, step: crate::dma::AddressStep, unit: u32) -> u32 {
    use crate::dma::AddressStep::*;
    match step {
        Increment | IncrementReload => addr.wrapping_add(unit),
        Decrement => addr.wrapping_sub(unit),
        Fixed => addr,
    }
}

impl Bus for GbaSystemBus {
    fn read8(&mut self, addr: u32) -> u8 {
        self.charge(addr, 1);
        self.read8_routed(addr)
    }

    /// Recorded before the write lands, so the log holds what the CPU asked to store rather than what
    /// a register chose to keep.
    fn write8(&mut self, addr: u32, value: u8) {
        self.charge(addr, 1);
        self.write8_routed(addr, value);
    }

    fn read16(&mut self, addr: u32) -> u16 {
        let addr = addr & !1;
        self.charge(addr, 2);
        self.read16_routed(addr)
    }

    fn write16(&mut self, addr: u32, value: u16) {
        let addr = addr & !1;
        self.charge(addr, 2);
        self.write16_routed(addr, value);
    }

    /// A 32-bit access is two halfword accesses, never four byte accesses.
    ///
    /// The default implementations decompose into bytes, which is catastrophic here: `write8`
    /// implements the 16-bit bus quirk where a byte written to palette RAM or VRAM is doubled
    /// across its halfword, so a word store would write each byte and then immediately overwrite
    /// it with the next. Storing 1 landed as 0. `gba-suite`'s memory test caught it on the
    /// third check; every 32-bit palette and VRAM write in every game would have been wrong.
    ///
    /// The decomposition goes through the *routing* helpers rather than through [`Bus::read16`],
    /// because the width the wait-state table wants is the one the CPU asked for: `cost` already
    /// knows a word on a 16-bit bus is two bus cycles, and charging the halves as well counted
    /// every access twice more.
    fn read32(&mut self, addr: u32) -> u32 {
        let addr = addr & !3;
        self.charge(addr, 4);
        (self.read16_routed(addr) as u32)
            | ((self.read16_routed(addr.wrapping_add(2)) as u32) << 16)
    }

    fn write32(&mut self, addr: u32, value: u32) {
        let addr = addr & !3;
        self.charge(addr, 4);
        self.write16_routed(addr, value as u16);
        self.write16_routed(addr.wrapping_add(2), (value >> 16) as u16);
    }

    fn tick(&mut self, _cycles: Cycles) {
        // Deliberately empty. The ARM core reports an instruction's cost by returning it from
        // `step`, not by calling this — unlike the SM83, which reports each access as it
        // happens. Advancing here as well would double every instruction's cost.
    }

    fn open_bus8(&self, addr: u32) -> u8 {
        (self.memory.open_bus32() >> ((addr & 3) * 8)) as u8
    }

    /// A side-effect-free read, for the debugger's memory and disassembly views.
    ///
    /// I/O and cartridge save space answer `None`, and that is the honest answer rather than a gap.
    /// An I/O read here would go through `read_io16`, which is where registers with read-side
    /// behaviour live; the save space is a Flash or EEPROM state machine whose reads are commands.
    /// Showing `--` for those two regions is correct — a memory viewer that stepped a Flash chip's
    /// state machine to avoid showing `--` would change the bug being investigated.
    fn peek8(&self, addr: u32) -> Option<u8> {
        match Region::of(addr) {
            Region::Io | Region::Sram => None,
            Region::Rom { .. } => Some(self.cartridge.read_rom(addr)),
            _ => self.memory.read8(addr),
        }
    }
}

/// Routing without timing: where an access *goes*, separated from what it costs.
///
/// The split exists because the two answers have different shapes. A 32-bit access is one access to
/// the wait-state table and two halfword accesses to the memory behind it, and before these were
/// separated the decomposition charged the table again at every level — six times for a word.
/// Charging happens once, in the [`Bus`] method the CPU called; everything below here only moves
/// bytes and records them.
impl GbaSystemBus {
    /// An instruction word, read without charging for it or recording it.
    ///
    /// `None` where an instruction cannot be: I/O registers and the cartridge save window are the
    /// two regions [`Bus::peek8`] refuses, because reading them has side effects.
    fn peek16(&self, addr: u32) -> Option<u16> {
        Some(u16::from_le_bytes([
            self.peek8(addr)?,
            self.peek8(addr + 1)?,
        ]))
    }

    fn peek32(&self, addr: u32) -> Option<u32> {
        Some((self.peek16(addr)? as u32) | ((self.peek16(addr + 2)? as u32) << 16))
    }

    fn read16_routed(&mut self, addr: u32) -> u16 {
        if Region::of(addr) == Region::Io {
            let value = self.read_io16(addr).unwrap_or(0);
            // As two byte entries, so every entry in the log means the same thing and a watchpoint's
            // range arithmetic needs no special case for register width.
            self.watch.record(addr, AccessKind::Read, value as u8);
            self.watch
                .record(addr + 1, AccessKind::Read, (value >> 8) as u8);
            return value;
        }
        u16::from_le_bytes([self.read8_routed(addr), self.read8_routed(addr + 1)])
    }

    fn write16_routed(&mut self, addr: u32, value: u16) {
        match Region::of(addr) {
            Region::Io => {
                self.watch.record(addr, AccessKind::Write, value as u8);
                self.watch
                    .record(addr + 1, AccessKind::Write, (value >> 8) as u8);
                self.write_io16(addr, value);
                if Self::is_dma_register(addr) {
                    self.run_pending_dma();
                }
            }
            Region::Rom { .. } => {}
            Region::Sram => self.cartridge.write_save(addr, value as u8),
            _ => {
                // Not the two byte writes: a byte written to palette RAM or VRAM is doubled across
                // its halfword by the 16-bit bus quirk, so writing the halves in turn would leave
                // the second overwriting the first.
                self.watch.record(addr, AccessKind::Write, value as u8);
                self.watch
                    .record(addr + 1, AccessKind::Write, (value >> 8) as u8);
                self.memory.write16(addr, value);
            }
        }
    }

    fn read8_routed(&mut self, addr: u32) -> u8 {
        let value = self.read8_inner(addr);
        self.watch.record(addr, AccessKind::Read, value);
        value
    }

    fn write8_routed(&mut self, addr: u32, value: u8) {
        self.watch.record(addr, AccessKind::Write, value);
        self.write8_inner(addr, value);
    }

    fn read8_inner(&mut self, addr: u32) -> u8 {
        match Region::of(addr) {
            Region::Io => {
                // Registers are halfwords; a byte read takes its half of one.
                let half = self.read_io16(addr & !1).unwrap_or(0);
                if addr & 1 == 0 {
                    half as u8
                } else {
                    (half >> 8) as u8
                }
            }
            Region::Rom { .. } => self.cartridge.read_rom(addr),
            Region::Sram => self.cartridge.read_save(addr),
            _ => self.memory.read8(addr).unwrap_or(0),
        }
    }

    fn write8_inner(&mut self, addr: u32, value: u8) {
        match Region::of(addr) {
            Region::Io => {
                // A byte write to a halfword register is a read-modify-write, which matters for
                // registers whose other half is live state rather than a copy of what was
                // written.
                let base = addr & !1;
                let current = self.read_io16(base).unwrap_or(0);
                let merged = if addr & 1 == 0 {
                    (current & 0xFF00) | value as u16
                } else {
                    (current & 0x00FF) | ((value as u16) << 8)
                };
                self.write_io16(base, merged);
                if Self::is_dma_register(base) {
                    self.run_pending_dma();
                }
            }
            Region::Sram => self.cartridge.write_save(addr, value),
            // ROM is not writable, and a write there is silently dropped rather than trapped:
            // games do it during cartridge probing.
            Region::Rom { .. } => {}
            _ => {
                self.memory.write8(addr, value);
            }
        }
    }
}

/// A Game Boy Advance.
pub struct GbaSystem {
    cpu: Arm7Tdmi,
    bus: GbaSystemBus,
    save_ram_dirty: bool,
    /// The mask of a BIOS `IntrWait` that has begun and not yet been satisfied.
    ///
    /// Machine state, not a cache: it is what tells a re-executed `SWI` that its discard has
    /// already happened. A save state taken while a game sits in `VBlankIntrWait` — which is where
    /// a game spends most of its time, so most quicksaves land here — restores into the wait
    /// rather than into a call that would discard the flags a second time.
    intr_wait: Option<u16>,
    /// Forces the one-cycle-at-a-time halt path even when [`GbaSystem::halt_fast_forward_cycles`]
    /// could predict the wake, so a test can run the same machine both ways and compare the
    /// result instead of only trusting the fast path's own arithmetic.
    #[cfg(test)]
    disable_halt_fast_forward: bool,
}

impl GbaSystem {
    pub fn new(rom: Vec<u8>, bios: Option<Vec<u8>>) -> Result<Self, CartridgeError> {
        let cartridge = Cartridge::new(rom)?;
        let has_bios = bios.is_some();
        let mut system = Self {
            cpu: Arm7Tdmi::new(boot_state(has_bios)),
            bus: GbaSystemBus::new(cartridge, bios),
            save_ram_dirty: false,
            intr_wait: None,
            #[cfg(test)]
            disable_halt_fast_forward: false,
        };
        // Computed rather than passed `has_bios` directly: the boot program counter is 0 (inside
        // the BIOS) exactly when a BIOS was supplied and the cartridge entry (outside it)
        // otherwise, so this agrees with `has_bios` here and stays correct once execution moves.
        system.update_in_bios();
        if !has_bios {
            system
                .bus
                .memory
                .set_bios_open_bus(BIOS_OPCODE_AFTER_STARTUP);
        }
        system.apply_startup_state();
        Ok(system)
    }

    pub fn header(&self) -> &GbaHeader {
        &self.bus.cartridge.header
    }

    pub fn bus(&self) -> &GbaSystemBus {
        &self.bus
    }

    pub fn bus_mut(&mut self) -> &mut GbaSystemBus {
        &mut self.bus
    }

    pub fn cpu(&self) -> &Arm7Tdmi {
        &self.cpu
    }

    /// Mutable access to the core, for the debugger's "set next statement".
    ///
    /// As narrow as `bus_mut`: it exists so `DebugTarget` can move the program counter, which is the
    /// only write a debugger makes into the CPU.
    pub fn cpu_mut(&mut self) -> &mut Arm7Tdmi {
        &mut self.cpu
    }

    /// Approximate the bus's floating value from the instruction about to run.
    ///
    /// There is no such thing as an unmapped read on real hardware: it returns whatever was last
    /// driven on the bus, and that is almost always the instruction fetch, because nothing else
    /// contends for the bus as often. Modelled here rather than by instrumenting every access,
    /// because the fetch this approximates is a property of *where the CPU is*, not of any one
    /// read — an unmapped data read and the fetch that preceded it see the same value on real
    /// hardware precisely because the fetch was the last thing to touch the bus.
    ///
    /// Peeked rather than read for the same reason [`Self::intercept_bios_call`] peeks: this is
    /// answering a question about the word the CPU is about to fetch itself, and reading it a
    /// second time through the bus would charge, latch, and log an access that never happened.
    fn update_open_bus(&mut self) {
        let pc = self.cpu.regs.pc();
        if self.cpu.is_thumb() {
            // A halfword bus duplicated into both halves of the word open-bus reads are
            // reconstructed from, matching the width the fetch actually was.
            if let Some(half) = self.bus.peek16(pc & !1) {
                self.bus
                    .memory
                    .set_open_bus((half as u32) | ((half as u32) << 16));
            }
        } else if let Some(word) = self.bus.peek32(pc & !3) {
            self.bus.memory.set_open_bus(word);
        }
    }

    /// Gate BIOS reads on whether code is currently executing inside it.
    ///
    /// The BIOS is readable only by code running inside it — a real cartridge probing it from
    /// outside gets open bus, which is exactly how some anti-piracy checks detect an emulator
    /// that maps the region unconditionally. Latching this once at construction only got it right
    /// for the very first instruction: a game that calls into the BIOS and returns crosses this
    /// boundary constantly, and a flag fixed at boot answers every later read as if the machine
    /// had never left its starting side of it. Recomputed every step from the program counter
    /// instead, so the two stay in step with wherever the CPU actually is.
    fn update_in_bios(&mut self) {
        let in_bios = self.cpu.regs.pc() < BIOS_SIZE as u32;
        self.bus.memory.set_in_bios(in_bios);
    }

    /// Answer a `SWI` in place of the BIOS, when there is no BIOS to answer it.
    ///
    /// Intercepted *before* the instruction executes rather than by trapping the exception
    /// afterwards, so the CPU never enters Supervisor mode and never jumps to the empty vector
    /// — which is exactly what it did before this existed, running off into unmapped memory
    /// after 84,701 correct instructions of `gba-suite`.
    ///
    /// With a real BIOS supplied this does nothing and the exception is taken normally.
    ///
    /// # Why the opcode is peeked rather than read
    ///
    /// This runs before *every* instruction, and it is only asking a question — the CPU is about to
    /// fetch the same word itself. Reading it through the bus fetched it twice: charged twice
    /// against the wait-state table, latched into the cartridge's sequential-access address twice,
    /// and recorded in the watchpoint log twice. That doubling was most of why an ARM instruction in
    /// internal WRAM cost 13 cycles instead of 1, which starved every commercial game of about nine
    /// tenths of its processor.
    fn intercept_bios_call(&mut self) -> bool {
        if self.bus.memory.has_bios() {
            return false;
        }
        // A halted core runs nothing, and this runs *before* the core, so without this a `SWI`
        // sitting immediately after a `Halt` would be answered while the machine was supposed to be
        // asleep. The exception is a wait in progress: re-running its `SWI` is precisely how the
        // wait is spread across steps, and it is the only call allowed to execute while halted.
        if self.cpu.is_halted() && self.intr_wait.is_none() {
            return false;
        }
        let pc = self.cpu.regs.pc();

        // Both instruction sets, because a game may be in either and most of them are in Thumb.
        //
        // Skipping the Thumb form is not a small gap. Almost every commercial GBA game is
        // compiled to Thumb — it is the smaller encoding and the one the cartridge bus favours —
        // so a machine that only answers ARM `SWI`s answers almost none of the calls a real game
        // makes. Pokémon Emerald ran at full speed with a black screen for exactly this reason:
        // its first BIOS call fell through to an unmapped vector and the machine sat there.
        let (comment, width) = if self.cpu.is_thumb() {
            let Some(opcode) = self.bus.peek16(pc & !1) else {
                return false;
            };
            // `1101 1111 imm8`. One encoding, no condition field, and the comment is the low
            // byte — there is nothing to guess at here.
            if opcode & 0xFF00 != 0xDF00 {
                return false;
            }
            ((opcode & 0xFF) as u8, 2)
        } else {
            let Some(opcode) = self.bus.peek32(pc & !3) else {
                return false;
            };
            // `cond 1111 imm24`. Bits 24-27 identify the instruction; the 24 below them are the
            // comment, so the mask must not reach into them. Only the always-condition is
            // handled: a conditional `SWI` is vanishingly rare and would need the flag check
            // duplicated here.
            if opcode & 0x0F00_0000 != 0x0F00_0000 || opcode >> 28 != 0xE {
                return false;
            }
            (((opcode >> 16) & 0xFF) as u8, 4)
        };

        match bios::dispatch(&mut self.cpu, &mut self.bus, &mut self.intr_wait, comment) {
            // Step over the instruction the BIOS would have returned from — two bytes in Thumb,
            // four in ARM.
            //
            // Waking the core here is what finishes an `IntrWait`: the only way to be executing a
            // `SWI` while halted is a `Retry` below whose wait has just been satisfied.
            bios::BiosEffect::Return => {
                self.cpu.set_halted(false);
                self.cpu.regs.set_pc(pc.wrapping_add(width));
                self.bus.memory.set_bios_open_bus(BIOS_OPCODE_AFTER_SWI);
            }
            bios::BiosEffect::Halt => {
                self.cpu.halt();
                self.cpu.regs.set_pc(pc.wrapping_add(width));
                self.bus.memory.set_bios_open_bus(BIOS_OPCODE_AFTER_SWI);
            }
            // Leave the program counter *on* the `SWI` so it runs again next step. That is how a
            // wait hardware spends inside a BIOS loop is spread across steps here — see
            // `bios::intr_wait`, which argues the choice and the alternative.
            bios::BiosEffect::Retry => self.cpu.halt(),
            // `SoftReset` already set the program counter to its own entry point and is
            // documented as never returning to the caller, so there is nothing to step over —
            // and what it leaves on the bus is the startup trace, not the generic post-SWI one,
            // because GBATEK documents both as the same value.
            bios::BiosEffect::Jump => {
                self.cpu.set_halted(false);
                self.bus.memory.set_bios_open_bus(BIOS_OPCODE_AFTER_STARTUP);
            }
        }
        true
    }

    /// Install the stacks the BIOS would have set up.
    ///
    /// Done even when a BIOS is present, because it is harmless there — the BIOS overwrites
    /// them — and it means the no-BIOS path is not a separate startup sequence to keep in step.
    fn apply_startup_state(&mut self) {
        // R13 is the banked stack pointer; each mode gets its own.
        self.cpu.regs.write(Mode::System, 13, SP_SYSTEM);
        self.cpu.regs.write(Mode::Irq, 13, SP_IRQ);
        self.cpu.regs.write(Mode::Supervisor, 13, SP_SUPERVISOR);
    }

    /// Take an interrupt, standing in for the BIOS when none is supplied.
    ///
    /// With a BIOS the CPU enters at its vector and the BIOS does the rest. Without one, this
    /// does what the BIOS does and no more: read the handler address the game left at the top
    /// of IWRAM and enter it. Skipping this leaves every game's interrupt code unreachable,
    /// which looks like a hang rather than like a missing BIOS.
    fn service_interrupt(&mut self) {
        self.cpu.set_irq_line(self.bus.irq.pending());
        if self.bus.memory.has_bios() || !self.bus.irq.pending() {
            return;
        }
        if self.cpu.cpsr.irq_disabled() {
            return;
        }
        let handler = self.bus.read32(irq::HLE_HANDLER_POINTER);
        if handler == 0 {
            return;
        }

        // Record what is being serviced where `IntrWait` will look for it. `IF` cannot serve that
        // purpose: the game's handler clears it as it works, long before the wait is re-tested.
        //
        // On hardware this word is written by the *game's* handler — the BIOS only supplies the
        // convention, and every mainstream library's `IntrMain` maintains it. It is written here as
        // well because the two are idempotent (both set the same bits) and a game whose handler
        // does not keep the word would otherwise wait forever. Erring towards a wait that completes
        // costs a game one early return; erring the other way is a hang.
        let serviced = self.bus.irq.active();
        let already = self.bus.read16(bios::INTRWAIT_FLAGS);
        self.bus.write16(bios::INTRWAIT_FLAGS, already | serviced);

        // Enter the exception properly — banked registers, mode, and mask all change.
        let lr = self.cpu.regs.pc().wrapping_add(4);
        self.cpu.enter_exception(Exception::Irq, lr);

        // Then stand in for the BIOS's *wrapper*, which is the part that matters and the part
        // that was missing. A real BIOS does not jump to the game's handler and hope: it pushes
        // the registers the ARM procedure standard lets a callee clobber, calls the handler with
        // `LR` pointing at its own epilogue, and on return restores them and leaves the exception
        // with `subs pc, lr, #4` — which is what puts `CPSR` back and unmasks interrupts.
        //
        // Jumping straight to the handler instead leaves its `bx lr` returning into the
        // *interrupted code* while still in IRQ mode with interrupts masked. The machine then
        // runs on, never takes another interrupt, and wanders off into unmapped memory. That is
        // exactly how Pokémon Emerald reached a white screen and stayed there.
        let mut sp = self.cpu.regs.read(Mode::Irq, 13);
        for value in [
            self.cpu.reg(0),
            self.cpu.reg(1),
            self.cpu.reg(2),
            self.cpu.reg(3),
            self.cpu.reg(12),
            lr,
        ]
        .into_iter()
        .rev()
        {
            sp = sp.wrapping_sub(4);
            self.bus.write32(sp, value);
        }
        self.cpu.regs.write(Mode::Irq, 13, sp);
        // `LR` is the epilogue's address rather than the return address, which is what makes the
        // handler's `bx lr` come back here instead of into the interrupted code.
        self.cpu.regs.write(Mode::Irq, 14, HLE_IRQ_RETURN);
        self.cpu.regs.set_pc(handler);
        self.cpu.set_irq_line(false);
        self.bus.memory.set_bios_open_bus(BIOS_OPCODE_DURING_IRQ);
    }

    /// How many cycles a halted CPU could fast-forward through in one step, or `None` if that
    /// cannot be predicted and the ordinary one-cycle-at-a-time path has to run instead.
    ///
    /// Two things both have to hold. First, some enabled source needs a *computable* schedule:
    /// the video edges (HBlank, VBlank, the VCOUNT match) and the four timers all do, because
    /// their state is nothing but counters advancing at a known rate. The keypad, the serial
    /// port, a cartridge GPIO line, and a completed DMA transfer do not — each depends on
    /// something external to this prediction, or on code that only runs once the CPU is already
    /// awake — so a halt that depends on only one of those still steps normally, and correctly:
    /// it just gets no benefit from this.
    ///
    /// Second, and less obvious, no DMA channel may be armed to fire again on its own during the
    /// interval — see [`DmaController::has_a_channel_that_could_fire_on_its_own`]. `run_clocks`
    /// advances the video and timers by exactly the `cycles` it is given, unconditionally, and
    /// does not stop early just because an interrupt became pending partway through — nothing
    /// needs it to, since the ordinary path re-checks after every single cycle anyway. A DMA
    /// transfer that fires during a *predicted* span is a different matter: its own cost is
    /// *additional* video/timer advancement, spent through `run_clocks` a second time from
    /// inside `run_pending_dma`, on top of whatever this predicted. A prediction blind to that
    /// would send the outer call further than the interval it computed, and the acceptance test
    /// for this exists specifically to catch that kind of drift. Modelling it exactly would mean
    /// simulating the DMA controller too — simpler and still correct is to not predict at all
    /// while any such channel is armed, and let DMA's own cost keep being accounted for the way
    /// it already is.
    fn halt_fast_forward_cycles(&self) -> Option<u32> {
        if self.bus.dma.has_a_channel_that_could_fire_on_its_own() {
            return None;
        }
        if !self.bus.irq.master_enabled() {
            return None;
        }
        let ie = self.bus.irq.enabled_sources();
        if ie == 0 {
            return None;
        }

        // Probed on scratch copies of exactly the state that decides these two kinds of edge,
        // reusing the real `VideoTiming::tick`/`interrupt_sources` and `Timers::tick`/
        // `interrupts` rather than a second, hand-derived formula that could disagree with them.
        let mut video = self.bus.video;
        let mut timers = self.bus.timers;
        let mut elapsed = 0u32;
        // The same hang guard `step_frame` uses: if nothing wakes the CPU within two frames,
        // there is nothing to predict, and stepping normally will discover that just as surely.
        let bound = (FRAME_CYCLES * 2) as u32;
        while elapsed < bound {
            // Capped to `cycles_until_next_edge`, not just `bound - elapsed`: `tick` only stops at
            // a line boundary, so an uncapped request here would sail straight past a mid-line
            // `entered_hblank` to wherever the line ends, landing up to 272 cycles later than the
            // real edge — see that method's doc comment.
            let step = (bound - elapsed).min(video.cycles_until_next_edge());
            let (events, consumed) = video.tick(step);
            let overflowed = timers.tick(consumed);
            elapsed += consumed;

            if video.interrupt_sources(&events) & ie != 0 {
                return Some(elapsed);
            }
            let asked = timers.interrupts(&overflowed);
            if (0..4).any(|ch| asked & (1 << ch) != 0 && ie & irq::source::timer(ch) != 0) {
                return Some(elapsed);
            }
        }
        None
    }

    /// The other half of the wrapper: unwind and leave the exception.
    ///
    /// Recognised by the program counter reaching [`HLE_IRQ_RETURN`], which is an address inside
    /// the BIOS that nothing else can be executing — with no BIOS supplied the region is unmapped,
    /// so the only way to arrive there is the `LR` planted above.
    fn intercept_bios_irq_return(&mut self) -> bool {
        if self.bus.memory.has_bios() || self.cpu.regs.pc() != HLE_IRQ_RETURN {
            return false;
        }
        let mut sp = self.cpu.regs.read(Mode::Irq, 13);
        let mut popped = [0u32; 6];
        for slot in &mut popped {
            *slot = self.bus.read32(sp);
            sp = sp.wrapping_add(4);
        }
        let [r0, r1, r2, r3, r12, lr] = popped;
        self.cpu.regs.write(Mode::Irq, 13, sp);
        self.cpu.set_reg(0, r0);
        self.cpu.set_reg(1, r1);
        self.cpu.set_reg(2, r2);
        self.cpu.set_reg(3, r3);
        self.cpu.set_reg(12, r12);

        // `subs pc, lr, #4`: restore the saved status register and resume where the interrupt
        // struck. Restoring `CPSR` is the step that unmasks interrupts again, so leaving it out
        // is what makes a machine take exactly one interrupt and then no more.
        self.cpu.exception_return(lr.wrapping_sub(4));
        self.bus.memory.set_bios_open_bus(BIOS_OPCODE_AFTER_IRQ);
        true
    }
}

/// Where the HLE's interrupt wrapper returns to.
///
/// Inside the BIOS region, which is unmapped when no BIOS is supplied — so no game can reach it
/// except through the `LR` the wrapper plants. The exact value is the address the real BIOS
/// returns to, which makes a trace of this machine line up with a trace of hardware.
const HLE_IRQ_RETURN: u32 = 0x0000_0138;

fn boot_state(has_bios: bool) -> BootState {
    if has_bios {
        // The reset vector, in Supervisor mode with interrupts masked, as the reset exception
        // leaves the core.
        return BootState::default();
    }
    // The documented post-boot state: the cartridge entry point, System mode, interrupts
    // unmasked because the BIOS has finished with them.
    BootState {
        pc: CARTRIDGE_ENTRY,
        mode: Mode::System,
        thumb: false,
        sp: SP_SYSTEM,
        irq_disabled: false,
        fiq_disabled: true,
    }
}

impl System for GbaSystem {
    /// Run exactly one instruction and report what it cost.
    ///
    /// The debugger single-steps with this, and the session checks execution breakpoints between
    /// calls to it. It is also how tracing a machine that has run off into unmapped memory is
    /// possible at all from outside — `step_frame` is far too coarse to see where a wrong branch
    /// was taken.
    fn debug(&mut self) -> Option<&mut dyn core_common::DebugTarget> {
        Some(self)
    }

    fn access_log(&mut self) -> Option<&mut core_common::AccessLog> {
        Some(&mut self.bus.watch)
    }

    fn step_instruction(&mut self) -> Cycles {
        self.service_interrupt();
        if self.intercept_bios_irq_return() {
            return Cycles(3);
        }
        // After any interrupt entry above, so both reflect the instruction genuinely about to
        // run this step rather than the one interrupted.
        self.update_in_bios();
        self.update_open_bus();
        // A CPU that is still halted at this point — `service_interrupt` above did not just wake
        // it — is doing nothing until some event ends the halt. That covers two shapes on this
        // machine, and both are pure overhead one cycle at a time: a plain `Halt`, and an
        // `IntrWait`/`VBlankIntrWait` retry loop, which re-enters `intercept_bios_call` below on
        // *every* step to re-read a flag word that nothing changes until some source fires. Real
        // software spends most of a frame in the second shape — `VBlankIntrWait` once per frame is
        // the standard idiom — so this has to run before `intercept_bios_call`, not only for a
        // plain `Halt`, or the common case would never reach it at all. Either way `Cpu::step`
        // returns `Cycles(1)` without touching the bus, so stepping through it was running up to a
        // whole frame's worth of iterations, ~280,000 of them, that changed nothing. Skip straight
        // to whichever wakes it first, when that is something this can predict, and return with
        // exactly that many cycles reported — *without* also running `cpu.step` or
        // `intercept_bios_call` on the same call.
        //
        // The predicted edge is the earliest source `IE` enables, not only the source a wait
        // named: on real hardware a `VBlankIntrWait` sitting under an `HBlank`-enabled raster
        // effect still takes the full detour through the game's handler on every `HBlank`, and
        // only the wait's own mask actually ends it — see `bios::intr_wait`'s doc comment. Landing
        // on any enabled edge and handing off to the ordinary `service_interrupt` /
        // `intercept_bios_call` sequence reproduces exactly that: a wait not yet satisfied re-halts
        // and this runs again for the next edge, just as the slow path would have found on its own.
        //
        // That hand-off deliberately does *not* try to also finish the wake-up here by poking
        // `irq_line`, re-running `service_interrupt`, or calling `intercept_bios_call` after the
        // jump: the HLE handshake is a multi-call sequence (one call masks `CPSR` and points `pc`
        // at the game's handler; only the *next* call's `service_interrupt`, seeing the mask now
        // set, lets `Cpu::step`'s own halt check clear `halted` and fall into running the handler,
        // which itself takes several more calls before acknowledging `IF` and returning) and
        // short-circuiting any of it here duplicates logic that already exists once, correctly, in
        // `service_interrupt` and `intercept_bios_call`. Landing exactly on the wake edge and
        // leaving the rest to those functions, one or more `step_instruction` calls away, costs a
        // little more than the theoretical minimum but reuses their sequencing instead of
        // re-deriving it.
        //
        // Re-triggering forever on that next call cannot happen: the jump above raised the very
        // interrupt this predicted, so `self.bus.irq.pending()` is true by the time this next runs,
        // and the guard below only fires while nothing is pending yet.
        #[cfg(test)]
        let fast_forward_allowed = !self.disable_halt_fast_forward;
        #[cfg(not(test))]
        let fast_forward_allowed = true;
        if fast_forward_allowed && self.cpu.is_halted() && !self.bus.irq.pending() {
            if let Some(distance) = self.halt_fast_forward_cycles() {
                self.bus.advance(distance);
                self.bus.take_pending_waits();
                return Cycles((distance + self.bus.take_dma_cycles()) as u64);
            }
        }
        if self.intercept_bios_call() {
            // The call is answered without running the instruction, so it costs nothing beyond
            // a nominal cycle — the real BIOS is slower, and that will matter for a game timing
            // against it, but a wrong non-zero figure is no better than this one.
            self.bus.advance(1);
            self.bus.take_pending_waits();
            return Cycles(1 + self.bus.take_dma_cycles() as u64);
        }
        // Armed immediately before the step that will consume it: an ARM or Thumb instruction
        // makes exactly one fetch, always its first bus access, so this is reliably true for that
        // access and false for every data access the instruction goes on to make. See
        // `GbaSystemBus::awaiting_fetch`.
        self.bus.awaiting_fetch = true;
        let cycles = self.cpu.step(&mut self.bus).get().max(1);
        // The instruction's own cost plus whatever its memory accesses waited for. Charged
        // together so a scheduled event cannot fire between two halves of one access.
        let total = cycles as u32 + self.bus.take_pending_waits();
        self.bus.advance(total);
        // Plus however long DMA held the bus. The transfer has already advanced the machine
        // itself, so this only reports the time — but reporting it is what makes a game that
        // copies a megabyte a frame get through proportionally less code, which is the whole
        // observable effect of DMA having a duration.
        Cycles((total + self.bus.take_dma_cycles()) as u64)
    }

    fn id(&self) -> &'static str {
        "gba"
    }

    fn display_name(&self) -> &'static str {
        "Game Boy Advance"
    }

    fn state_version(&self) -> u32 {
        STATE_VERSION
    }

    fn set_input(&mut self, input: InputState) {
        self.bus.keypad.set_input(input.buttons);
        if self.bus.keypad.interrupt_requested() {
            self.bus.irq.raise(irq::source::KEYPAD);
        }
    }

    fn step_frame(&mut self, input: InputState) -> FrameOutput {
        self.set_input(input);
        self.bus.frame_ready = false;
        self.save_ram_dirty = false;
        let mut elapsed = 0u64;

        // Bounded so a frontend always gets a frame back rather than hanging, the same guard
        // the Game Boy uses.
        while !self.bus.frame_ready && elapsed < FRAME_CYCLES * 2 {
            self.service_interrupt();
            // The ARM core reports an instruction's cost by returning it rather than through
            // `Bus::tick` the way the SM83 does, so `step_instruction` advances the machine.
            elapsed += self.step_instruction().get();
        }

        if let Some(save) = self.bus.cartridge.battery_save_mut() {
            self.save_ram_dirty = save.is_dirty();
            save.clear_dirty();
        }

        FrameOutput {
            cycles_elapsed: Cycles(elapsed),
            save_ram_dirty: self.save_ram_dirty,
            stopped: false,
        }
    }

    fn reset(&mut self) {
        let has_bios = self.bus.memory.has_bios();
        self.cpu = Arm7Tdmi::new(boot_state(has_bios));
        self.apply_startup_state();
    }

    fn framebuffer(&self) -> &Framebuffer {
        &self.bus.framebuffer
    }

    fn take_audio_samples(&mut self) -> &[AudioSample] {
        self.bus.take_samples()
    }

    fn load_cartridge(&mut self, rom: &[u8]) -> Result<(), CartridgeError> {
        self.bus.cartridge = Cartridge::new(rom.to_vec())?;
        <Self as System>::reset(self);
        Ok(())
    }

    fn save_ram(&self) -> Option<&[u8]> {
        self.bus.cartridge.battery_save().map(|s| s.as_bytes())
    }

    fn load_save_ram(&mut self, data: &[u8]) -> Result<(), CartridgeError> {
        match self.bus.cartridge.battery_save_mut() {
            Some(save) => save.load_from_bytes(data),
            None => Err(CartridgeError::NoSaveRam),
        }
    }
}

impl Savable for GbaSystemBus {
    fn save(&self, w: &mut StateWriter) {
        self.memory.save(w);
        self.cartridge.save(w);
        self.video.save(w);
        self.backgrounds.save(w);
        for layer in &self.affine {
            layer.save(w);
        }
        self.timers.save(w);
        self.dma.save(w);
        self.effects.save(w);
        self.irq.save(w);
        self.keypad.save(w);
        self.sound.save(w);
        self.psg.save(w);
        self.waits.save(w);
        w.write_u64(self.sample_accumulator);
    }

    fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        self.memory.load(r)?;
        self.cartridge.load(r)?;
        self.video.load(r)?;
        self.backgrounds.load(r)?;
        for layer in &mut self.affine {
            layer.load(r)?;
        }
        self.timers.load(r)?;
        self.dma.load(r)?;
        self.effects.load(r)?;
        self.irq.load(r)?;
        self.keypad.load(r)?;
        self.sound.load(r)?;
        self.psg.load(r)?;
        self.waits.load(r)?;
        self.sample_accumulator = r.read_u64()?;
        Ok(())
    }
}

impl Savable for GbaSystem {
    fn save(&self, w: &mut StateWriter) {
        self.cpu.save(w);
        self.bus.save(w);
        // Written as a present flag and a mask rather than a sentinel mask, because zero is a
        // legal mask — a game can ask to wait for nothing, and hardware then never returns.
        w.write_bool(self.intr_wait.is_some());
        w.write_u16(self.intr_wait.unwrap_or(0));
    }

    fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        self.cpu.load(r)?;
        self.bus.load(r)?;
        let waiting = r.read_bool()?;
        let mask = r.read_u16()?;
        self.intr_wait = waiting.then_some(mask);
        Ok(())
    }
}

#[cfg(test)]
mod tests;
