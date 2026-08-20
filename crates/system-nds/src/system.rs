//! The machine: two CPUs, two buses over one set of memory, and the frame loop.
//!
//! # How the two CPUs are driven
//!
//! `AGENTS.md` flags this as the decision that shapes everything else, and prompt 13 settles half
//! of it: cooperative interleaving on one thread, never real parallelism, because determinism is
//! required and save states depend on it. What was left open was *the quantum*.
//!
//! The quantum here is **a video boundary**: [`crate::video::VideoTiming`] says how far the
//! machine may run before something visible happens, both cores run that many cycles, and then the
//! boundary is serviced. Two other options were considered:
//!
//! - **A fixed small quantum**, say 32 cycles. Finer IPC coupling, but it decouples the CPUs from
//!   the renderer, so a scanline can be composited from registers written part-way through it.
//!   That is the mid-frame-scroll bug prompt 08 exists to prevent.
//! - **One scheduler both cores hang off**, which prompt 07 built for one master. Extending it to
//!   two means every event carries which core it belongs to and the "next event" query becomes a
//!   merge — real work, for a machine whose two cores synchronize through polled registers and a
//!   FIFO rather than through timing.
//!
//! The video boundary is between 1536 and 594 cycles, which is 45-18 microseconds of emulated
//! time. IPC round trips take far longer than that in any real game, because both ends are
//! interrupt-driven. **This is reversible in one place**: `step_frame` is the only caller.
//!
//! # The ARM9 runs at twice the clock, and that is all
//!
//! One system cycle is one ARM7 cycle and two ARM9 cycles. The timers, the video counters, and
//! every duration in this crate are in system cycles; only the CPU stepping loop knows about the
//! doubling.
//!
//! # Two views, one bus
//!
//! [`Arm9View`] and [`Arm7View`] each borrow the whole [`NdsBus`] and implement
//! [`core_common::Bus`] differently over it. They exist because the two cores genuinely disagree
//! about what an address means — the same `0x0300_0000` is different memory depending on who is
//! asking — and because `Cpu<B>` is generic over the bus, so each core still monomorphizes down
//! to direct calls with no dynamic dispatch in the memory path.
//!
//! # One address, two meanings
//!
//! `0x0400_0400` is the 3D core's command FIFO to the ARM9 and the first sound channel's control
//! register to the ARM7. That is not a quirk to work around: it is why the bus decode takes a
//! [`Core`] rather than being one address table, and it is the clearest single example of the two
//! cores genuinely disagreeing about what an address means.

use crate::apu::NdsApu;
use crate::bios;
use crate::cartridge::{NdsCartridge, HEADER_MIRROR};
use crate::dma::{AddressStep, DmaController, Transfer};
use crate::engine2d::{Engine, Engine2d};
use crate::gpu3d::Gpu3d;
use crate::input::{Input, RAW_PER_PIXEL};
use crate::ipc::Ipc;
use crate::irq::{sources, InterruptController};
use crate::memory::{NdsMemory, WramSplit};
use crate::timers::TimerBlock;
use crate::video::{VideoEvent, VideoTiming, FRAMEBUFFER_HEIGHT, SCREEN_HEIGHT, SCREEN_WIDTH};
use crate::vram::{self, Vram};
use crate::Core;
use core_common::{
    AccessKind, AccessLog, AudioSample, Bus, CartridgeError, Cpu, Cycles, DebugTarget, FrameOutput,
    Framebuffer, InputState, Savable, StateError, StateReader, StateWriter, System,
};
use cpu_arm7tdmi::{Arm7Tdmi, BootState, Exception, Mode};
use cpu_arm946e::Arm946e;

/// Where the firmware leaves its user settings for software to read, and where direct boot
/// fabricates a block.
const USER_SETTINGS: u32 = 0x027F_FC80;

/// Stack pointers the firmware installs, at the top of the ARM7's private work RAM.
const SP_SYSTEM: u32 = 0x0380_FD80;
const SP_IRQ: u32 = 0x0380_FF80;
const SP_SUPERVISOR: u32 = 0x0380_FFC0;

/// The ARM9's, as offsets into DTCM.
///
/// The two cores' firmware stacks are not in the same memory, and giving the ARM9 the ARM7's
/// addresses is not the harmless approximation it looks like: those addresses are the ARM7's
/// private work RAM, which the ARM9 cannot see at all, and once the shared block has gone to the
/// ARM7 — see [`NdsSystem::direct_boot`] — the ARM9's `0x0380_xxxx` reads as open bus. An
/// interrupt taken before a game has installed its own stacks would then push into nothing.
///
/// DTCM is the memory the ARM9 owns outright at that point, which is why the firmware puts them
/// there. The offsets are the ones a real machine leaves behind.
const ARM9_SP_SYSTEM_OFFSET: u32 = 0x2F7C;
const ARM9_SP_IRQ_OFFSET: u32 = 0x3F80;
const ARM9_SP_SUPERVISOR_OFFSET: u32 = 0x3FC0;

/// Where each core's BIOS interrupt handler reads the address of the game's handler.
///
/// The ARM7's is at the top of its work RAM; the ARM9's is at the top of DTCM, wherever CP15 has
/// put DTCM. Both are the DS equivalent of the GBA's `HLE_HANDLER_POINTER`, and skipping them
/// leaves every game's interrupt code unreachable — which presents as a hang, not as a missing
/// BIOS.
const ARM7_HANDLER_POINTER: u32 = 0x0380_FFFC;
const ARM9_HANDLER_OFFSET: u32 = 0x3FFC;

/// Where each core's BIOS keeps the word `IntrWait` tests, one word below the handler pointer.
///
/// libnds calls it `__irq_flags`, and its interrupt handler ORs every source it acknowledges into
/// it. `IntrWait` is answered against this rather than by halting once — see [`bios::Context`].
const ARM7_IRQ_FLAGS: u32 = 0x0380_FFF8;
const ARM9_IRQ_FLAGS_OFFSET: u32 = 0x3FF8;

/// Where each core's HLE interrupt wrapper returns to.
///
/// Both sit inside a BIOS region, which is unmapped when no BIOS is supplied — so no software can
/// reach either except through the `LR` [`NdsSystem::service_interrupt`] plants. The ARM9's is in
/// its high-vector BIOS window and the ARM7's in the 16 KiB at address zero, which is the same
/// difference that puts the two interrupt vectors 4 GiB apart.
const ARM9_IRQ_RETURN: u32 = 0xFFFF_0290;
const ARM7_IRQ_RETURN: u32 = 0x0000_0290;

/// Everything both cores share.
pub struct NdsBus {
    pub memory: NdsMemory,
    /// The sound hardware. In the ARM7's I/O space only: the ARM9 cannot reach it, which is why
    /// so much DS software has an ARM7 half whose whole job is playing sounds the ARM9 asks for
    /// over IPC.
    pub apu: NdsApu,
    pub vram: Vram,
    pub ipc: Ipc,
    pub cart: NdsCartridge,
    pub input: Input,
    pub video: VideoTiming,
    pub engine_a: Engine2d,
    pub engine_b: Engine2d,
    /// The 3D core, which only the ARM9 can reach.
    pub gpu3d: Gpu3d,
    pub irq: [InterruptController; 2],
    pub timers: [TimerBlock; 2],
    pub dma: [DmaController; 2],
    /// `POSTFLG` for each core, which software uses to tell a cold boot from a warm one.
    postflg: [u8; 2],
    /// `POWCNT1`. Bit 15 swaps which engine drives which screen.
    powcnt1: u16,
    /// `EXMEMCNT`/`EXMEMSTAT`. Only bit 11, the Slot-1 owner, is acted on.
    exmemcnt: u16,
    /// Set while a core has executed a halt instruction and is waiting for an interrupt.
    halted: [bool; 2],
    /// Set while a core is part-way through an `IntrWait`, which is answered by re-running the
    /// `SWI` after each wake rather than by returning from the first one. See [`bios::Context`].
    intr_wait: [bool; 2],
    /// The debugger's access recorder. Records nothing until a watchpoint arms it.
    ///
    /// Only the ARM9's accesses reach it, because the debugger shows the ARM9: an ARM7 access
    /// appearing here would fire a watchpoint whose cause the user cannot see. The core is a
    /// literal in each bus view, so the ARM7's views compile the check away entirely.
    pub watch: AccessLog,
}

impl NdsBus {
    fn new(arm9_bios: Option<Vec<u8>>, arm7_bios: Option<Vec<u8>>) -> Self {
        Self {
            memory: NdsMemory::new(arm9_bios, arm7_bios),
            apu: NdsApu::new(),
            vram: Vram::new(),
            ipc: Ipc::new(),
            cart: NdsCartridge::empty(),
            input: Input::new(),
            video: VideoTiming::new(),
            engine_a: Engine2d::new(Engine::A),
            engine_b: Engine2d::new(Engine::B),
            gpu3d: Gpu3d::new(),
            irq: [
                InterruptController::new(Core::Arm9),
                InterruptController::new(Core::Arm7),
            ],
            timers: [TimerBlock::new(); 2],
            dma: [
                DmaController::new(Core::Arm9),
                DmaController::new(Core::Arm7),
            ],
            postflg: [0; 2],
            powcnt1: 0x0203,
            exmemcnt: 0,
            halted: [false; 2],
            intr_wait: [false; 2],
            watch: AccessLog::new(),
        }
    }

    /// Which engine drives the top screen.
    fn top_engine(&self) -> Engine {
        if self.powcnt1 & 0x8000 != 0 {
            Engine::A
        } else {
            Engine::B
        }
    }
}

/// A view of the shared bus from one core.
///
/// The core is a type parameter of the view rather than a field so the address decode below
/// compiles to a straight-line dispatch per core rather than a branch on every access.
macro_rules! core_view {
    ($name:ident, $core:expr, $records:expr) => {
        pub struct $name<'a>(pub &'a mut NdsBus);

        impl $name<'_> {
            /// Record an access for the debugger, if this view is the one being watched.
            ///
            /// `$records` is a literal per view, so this compiles to nothing at all in the ARM7's
            /// and to one predictable branch in the ARM9's.
            #[inline(always)]
            fn watch(&mut self, addr: u32, kind: AccessKind, value: u8) {
                if $records {
                    self.0.watch.record(addr, kind, value);
                }
            }
        }

        impl Bus for $name<'_> {
            #[inline]
            fn read8(&mut self, addr: u32) -> u8 {
                let value = self.0.read8(($core), addr);
                self.watch(addr, AccessKind::Read, value);
                value
            }

            #[inline]
            fn write8(&mut self, addr: u32, value: u8) {
                self.watch(addr, AccessKind::Write, value);
                self.0.write8(($core), addr, value);
            }

            #[inline]
            fn read16(&mut self, addr: u32) -> u16 {
                let value = self.0.read16(($core), addr & !1);
                self.watch(addr & !1, AccessKind::Read, value as u8);
                self.watch((addr & !1) + 1, AccessKind::Read, (value >> 8) as u8);
                value
            }

            #[inline]
            fn write16(&mut self, addr: u32, value: u16) {
                self.watch(addr & !1, AccessKind::Write, value as u8);
                self.watch((addr & !1) + 1, AccessKind::Write, (value >> 8) as u8);
                self.0.write16(($core), addr & !1, value);
            }

            #[inline]
            fn read32(&mut self, addr: u32) -> u32 {
                let value = self.0.read32(($core), addr & !3);
                for i in 0..4u32 {
                    self.watch((addr & !3) + i, AccessKind::Read, (value >> (i * 8)) as u8);
                }
                value
            }

            #[inline]
            fn write32(&mut self, addr: u32, value: u32) {
                for i in 0..4u32 {
                    self.watch((addr & !3) + i, AccessKind::Write, (value >> (i * 8)) as u8);
                }
                self.0.write32(($core), addr & !3, value);
            }

            #[inline]
            fn open_bus8(&self, _addr: u32) -> u8 {
                0
            }

            #[inline]
            fn peek8(&self, addr: u32) -> Option<u8> {
                self.0.peek8(($core), addr)
            }
        }

        /// A transient view; the memory it borrows is saved by its real owner.
        impl Savable for $name<'_> {
            fn save(&self, _w: &mut StateWriter) {}
            fn load(&mut self, _r: &mut StateReader) -> Result<(), StateError> {
                Ok(())
            }
        }
    };
}

core_view!(Arm9View, Core::Arm9, true);
core_view!(Arm7View, Core::Arm7, false);

impl NdsBus {
    /// The single address decode, shared by both views.
    ///
    /// Written as one function taking the core rather than two, because the two maps agree
    /// about most of themselves and two copies would drift. Where they differ, the difference is
    /// a `match` on `core` at exactly the place it applies.
    pub(crate) fn read8(&mut self, core: Core, addr: u32) -> u8 {
        if let Some(byte) = self.memory_read8(core, addr) {
            return byte;
        }
        if (0x0400_0000..0x0500_0000).contains(&addr) || addr & !3 == 0x0410_0010 {
            return self.io_read8(core, addr);
        }
        if (0x0600_0000..0x0700_0000).contains(&addr) {
            return self.vram_read8(core, addr);
        }
        // The Slot-2 cartridge is not emulated. An absent one reads as ones, which is how
        // software detects that nothing is plugged in.
        0xFF
    }

    #[inline]
    fn memory_read_wide(&self, core: Core, addr: u32, bytes: u32) -> Option<u32> {
        match core {
            Core::Arm9 => self.memory.read_wide_arm9(addr, bytes),
            Core::Arm7 => self.memory.read_wide_arm7(addr, bytes),
        }
    }

    #[inline]
    fn memory_write_wide(&mut self, core: Core, addr: u32, value: u32, bytes: u32) -> bool {
        match core {
            Core::Arm9 => self.memory.write_wide_arm9(addr, value, bytes),
            Core::Arm7 => self.memory.write_wide_arm7(addr, value, bytes),
        }
    }

    fn memory_read8(&self, core: Core, addr: u32) -> Option<u8> {
        match core {
            Core::Arm9 => self.memory.read8_arm9(addr),
            Core::Arm7 => self.memory.read8_arm7(addr),
        }
    }

    fn vram_read8(&self, core: Core, addr: u32) -> u8 {
        match core {
            Core::Arm9 => match vram::arm9_space(addr) {
                Some((space, offset)) => self.vram.read8(space, offset),
                None => 0,
            },
            Core::Arm7 => {
                let (space, offset) = vram::arm7_space(addr);
                self.vram.read8(space, offset)
            }
        }
    }

    pub(crate) fn write8(&mut self, core: Core, addr: u32, value: u8) {
        let handled = match core {
            Core::Arm9 => self.memory.write8_arm9(addr, value),
            Core::Arm7 => self.memory.write8_arm7(addr, value),
        };
        if handled {
            return;
        }
        if (0x0400_0000..0x0500_0000).contains(&addr) || addr & !3 == 0x0410_0010 {
            self.io_write8(core, addr, value);
            return;
        }
        if (0x0600_0000..0x0700_0000).contains(&addr) {
            // A byte write to VRAM is dropped on the ARM9 exactly as it is for palette RAM and
            // OAM, but the ARM7 *can* write bytes to the banks assigned to it.
            if core == Core::Arm7 {
                let (space, offset) = vram::arm7_space(addr);
                self.vram.write8(space, offset, value);
            }
        }
    }

    fn read16(&mut self, core: Core, addr: u32) -> u16 {
        // RAM first, and in one go. This is the common case by a very large margin — every
        // instruction fetch lands here — and composing it from byte reads measured as the
        // dominant cost of a DS frame, ahead of both 2D engines put together.
        if let Some(value) = self.memory_read_wide(core, addr, 2) {
            return value as u16;
        }
        if (0x0400_0000..0x0500_0000).contains(&addr) || addr & !3 == 0x0410_0010 {
            return self.io_read16(core, addr);
        }
        if (0x0600_0000..0x0700_0000).contains(&addr) {
            return match core {
                Core::Arm9 => match vram::arm9_space(addr) {
                    Some((space, offset)) => self.vram.read16(space, offset),
                    None => 0,
                },
                Core::Arm7 => {
                    let (space, offset) = vram::arm7_space(addr);
                    self.vram.read16(space, offset)
                }
            };
        }
        u16::from_le_bytes([self.read8(core, addr), self.read8(core, addr + 1)])
    }

    fn write16(&mut self, core: Core, addr: u32, value: u16) {
        if self.memory_write_wide(core, addr, value as u32, 2) {
            return;
        }
        if (0x0400_0000..0x0500_0000).contains(&addr) || addr & !3 == 0x0410_0010 {
            self.io_write16(core, addr, value);
            return;
        }
        if (0x0600_0000..0x0700_0000).contains(&addr) {
            match core {
                Core::Arm9 => {
                    if let Some((space, offset)) = vram::arm9_space(addr) {
                        self.vram.write16(space, offset, value);
                    }
                }
                Core::Arm7 => {
                    let (space, offset) = vram::arm7_space(addr);
                    self.vram.write16(space, offset, value);
                }
            }
            return;
        }
        // Palette RAM and OAM take halfwords through their own path, because a byte write to
        // either is dropped and composing this out of two would drop both halves.
        if core == Core::Arm9 && self.memory.write16_arm9(addr, value) {
            return;
        }
        let [low, high] = value.to_le_bytes();
        self.write8(core, addr, low);
        self.write8(core, addr + 1, high);
    }

    fn read32(&mut self, core: Core, addr: u32) -> u32 {
        if let Some(value) = self.memory_read_wide(core, addr, 4) {
            return value;
        }
        if (0x0400_0000..0x0500_0000).contains(&addr) || addr & !3 == 0x0410_0010 {
            return self.io_read32(core, addr);
        }
        (self.read16(core, addr) as u32) | ((self.read16(core, addr + 2) as u32) << 16)
    }

    fn write32(&mut self, core: Core, addr: u32, value: u32) {
        if self.memory_write_wide(core, addr, value, 4) {
            return;
        }
        if (0x0400_0000..0x0500_0000).contains(&addr) || addr & !3 == 0x0410_0010 {
            self.io_write32(core, addr, value);
            return;
        }
        self.write16(core, addr, value as u16);
        self.write16(core, addr + 2, (value >> 16) as u16);
    }

    fn peek8(&self, core: Core, addr: u32) -> Option<u8> {
        if let Some(byte) = self.memory_read8(core, addr) {
            return Some(byte);
        }
        if (0x0600_0000..0x0700_0000).contains(&addr) {
            return Some(self.vram_read8(core, addr));
        }
        // I/O is not peekable: several of these registers have read side effects, and a memory
        // viewer showing `??` is better than one that advances a FIFO by scrolling.
        None
    }
}

/// The I/O map. Split out because it is long and because it is the one part of the bus that is
/// genuinely a lookup table rather than an address calculation.
impl NdsBus {
    fn io_read32(&mut self, core: Core, addr: u32) -> u32 {
        // `IPCFIFORECV` is word-only and *pops*, so it has to be caught before the fall-through
        // that would read it as two halfwords and take two words off the queue.
        if addr == 0x0410_0000 {
            return self.ipc.receive(core);
        }
        if let Some(value) = self.irq[core as usize].read32(addr) {
            return value;
        }
        if self.dma[core as usize].owns(addr) {
            return self.dma[core as usize].read32(addr).unwrap_or(0);
        }
        if NdsCartridge::owns(addr) && self.owns_card(core) {
            return self.cart.read32(addr).unwrap_or(0);
        }
        if core == Core::Arm7 && NdsApu::owns(addr) {
            return self.apu.read32_reg(addr).unwrap_or(0);
        }
        // The 3D core is checked before the fall-through to two halfword reads, because its
        // command FIFO and its result registers are word-only.
        if core == Core::Arm9 && Gpu3d::owns(addr) {
            if let Some(value) = self.gpu3d.read32(addr) {
                return value;
            }
        }
        (self.io_read16(core, addr) as u32) | ((self.io_read16(core, addr + 2) as u32) << 16)
    }

    fn io_write32(&mut self, core: Core, addr: u32, value: u32) {
        if self.irq[core as usize].write32(addr, value) {
            return;
        }
        if self.dma[core as usize].owns(addr) {
            self.dma[core as usize].write32(addr, value);
            return;
        }
        if NdsCartridge::owns(addr) && self.owns_card(core) {
            self.cart.write32(addr, value);
            return;
        }
        if core == Core::Arm7 && NdsApu::owns(addr) {
            self.apu.write32_reg(addr, value);
            return;
        }
        // `GXFIFO` and the command ports share `0x0400_0400` with the ARM7's sound registers.
        // Which one an address means is decided by *which core is asking*, which is the whole
        // reason the bus decode takes a core rather than being a table.
        if core == Core::Arm9 && Gpu3d::owns(addr) && self.gpu3d.write32(addr, value) {
            return;
        }
        if addr == 0x0400_0180 {
            self.ipc.write_sync(core, value);
            return;
        }
        if addr == 0x0400_0188 {
            self.ipc.send(core, value);
            return;
        }
        // Everything else, the two 2D engines included, is two halfword writes. Dispatching a
        // word to the engine here would take DISPSTAT and VCOUNT with it: those sit inside
        // engine A's register block but belong to the video timing, which `io_write16` checks
        // first and this would not.
        self.io_write16(core, addr, value as u16);
        self.io_write16(core, addr + 2, (value >> 16) as u16);
    }

    fn io_read16(&mut self, core: Core, addr: u32) -> u16 {
        let addr = addr & !1;
        if addr & !3 == 0x0410_0000 {
            // A narrow read of the receive FIFO still pops one word; software does this to
            // collect a halfword message without draining twice.
            let word = self.ipc.receive(core);
            return (word >> ((addr & 2) * 8)) as u16;
        }
        if let Some(value) = self.irq[core as usize].read16(addr) {
            return value;
        }
        if let Some(value) = self.video.read16(core, addr) {
            return value;
        }
        if let Some(value) = self.input.read16(core, addr) {
            return value;
        }
        if TimerBlock::owns(addr) {
            return self.timers[core as usize].read16(addr).unwrap_or(0);
        }
        if self.dma[core as usize].owns(addr) {
            return self.dma[core as usize].read16(addr).unwrap_or(0);
        }
        if NdsCartridge::owns(addr) && self.owns_card(core) {
            return self.cart.read16(addr).unwrap_or(0);
        }
        if core == Core::Arm7 && NdsApu::owns(addr) {
            return self.apu.read16_reg(addr).unwrap_or(0);
        }
        // Before engine A, because `DISP3DCNT` sits inside engine A's register block and belongs
        // to the 3D core rather than to the 2D engine that surrounds it.
        if core == Core::Arm9 && Gpu3d::owns(addr) {
            if let Some(value) = self.gpu3d.read16(addr) {
                return value;
            }
        }
        if self.engine_a.owns(addr) {
            return self.engine_a.read16(addr).unwrap_or(0);
        }
        if core == Core::Arm9 && self.engine_b.owns(addr) {
            return self.engine_b.read16(addr).unwrap_or(0);
        }
        match addr {
            0x0400_0180 | 0x0400_0182 => (self.ipc.read_sync(core) >> ((addr & 2) * 8)) as u16,
            0x0400_0184 => self.ipc.read_control(core),
            0x0400_0204 => self.exmemcnt,
            0x0400_0300 => self.postflg[core as usize] as u16,
            0x0400_0304 => self.powcnt1,
            // VRAMCNT and WRAMCNT, which are ARM9-only and read back as written.
            0x0400_0240..=0x0400_0249 if core == Core::Arm9 => {
                (self.vramcnt_read(addr) as u16) | ((self.vramcnt_read(addr + 1) as u16) << 8)
            }
            _ => 0,
        }
    }

    fn io_write16(&mut self, core: Core, addr: u32, value: u16) {
        let addr = addr & !1;
        if self.irq[core as usize].write16(addr, value) {
            return;
        }
        if self.video.write16(core, addr, value) {
            return;
        }
        if self.input.write16(core, addr, value) {
            return;
        }
        if TimerBlock::owns(addr) {
            self.timers[core as usize].write16(addr, value);
            return;
        }
        if self.dma[core as usize].owns(addr) {
            self.dma[core as usize].write16(addr, value);
            return;
        }
        if NdsCartridge::owns(addr) && self.owns_card(core) {
            self.cart.write16(addr, value);
            return;
        }
        if core == Core::Arm7 && NdsApu::owns(addr) {
            self.apu.write16_reg(addr, value);
            return;
        }
        if core == Core::Arm9 && Gpu3d::owns(addr) && self.gpu3d.write16(addr, value) {
            return;
        }
        if self.engine_a.owns(addr) {
            self.engine_a.write16(addr, value);
            return;
        }
        if core == Core::Arm9 && self.engine_b.owns(addr) {
            self.engine_b.write16(addr, value);
            return;
        }
        match addr {
            0x0400_0180 => {
                // A halfword write to IPCSYNC must not clear the enable bits in the high half.
                let current = self.ipc.read_sync(core);
                self.ipc
                    .write_sync(core, (current & 0xFFFF_0000) | value as u32);
            }
            0x0400_0184 => self.ipc.write_control(core, value),
            0x0400_0204 => self.exmemcnt = value,
            0x0400_0304 => self.powcnt1 = value,
            0x0400_0240..=0x0400_0249 if core == Core::Arm9 => {
                self.vramcnt_write(addr, value as u8);
                self.vramcnt_write(addr + 1, (value >> 8) as u8);
            }
            _ => {}
        }
    }

    fn io_read8(&mut self, core: Core, addr: u32) -> u8 {
        if let Some(value) = self.irq[core as usize].read8(addr) {
            return value;
        }
        if let Some(value) = self.video.read8(core, addr) {
            return value;
        }
        if let Some(value) = self.input.read8(core, addr) {
            return value;
        }
        if TimerBlock::owns(addr) {
            return self.timers[core as usize].read8(addr).unwrap_or(0);
        }
        if NdsCartridge::owns(addr) && self.owns_card(core) {
            return self.cart.read8(addr).unwrap_or(0);
        }
        if core == Core::Arm7 && NdsApu::owns(addr) {
            return self.apu.read8_reg(addr).unwrap_or(0);
        }
        if core == Core::Arm9 && Gpu3d::owns(addr) {
            if let Some(value) = self.gpu3d.read8(addr) {
                return value;
            }
        }
        match addr {
            0x0400_0240..=0x0400_0249 if core == Core::Arm9 => self.vramcnt_read(addr),
            0x0400_0300 => self.postflg[core as usize],
            _ => (self.io_read16(core, addr & !1) >> ((addr & 1) * 8)) as u8,
        }
    }

    fn io_write8(&mut self, core: Core, addr: u32, value: u8) {
        if self.irq[core as usize].write8(addr, value) {
            return;
        }
        if self.video.write8(core, addr, value) {
            return;
        }
        if self.input.write8(core, addr, value) {
            return;
        }
        if TimerBlock::owns(addr) {
            self.timers[core as usize].write8(addr, value);
            return;
        }
        if NdsCartridge::owns(addr) && self.owns_card(core) {
            self.cart.write8(addr, value);
            return;
        }
        if core == Core::Arm7 && NdsApu::owns(addr) {
            self.apu.write8_reg(addr, value);
            return;
        }
        if core == Core::Arm9 && Gpu3d::owns(addr) && self.gpu3d.write8(addr, value) {
            return;
        }
        match addr {
            0x0400_0240..=0x0400_0249 if core == Core::Arm9 => self.vramcnt_write(addr, value),
            0x0400_0300 => {
                // POSTFLG's low bit latches: once software sets it, it cannot clear it again.
                self.postflg[core as usize] |= value & 1;
            }
            // HALTCNT: bits 6-7 select 1 = GBA mode, 2 = halt, 3 = sleep. Only halt is honoured;
            // sleep would need the lid and the wake-up sources, and GBA mode needs a whole
            // second machine.
            0x0400_0301 if core == Core::Arm7 => {
                if (value >> 6) & 3 == 2 {
                    self.halted[Core::Arm7 as usize] = true;
                }
            }
            _ => {
                let current = self.io_read16(core, addr & !1);
                let spliced = if addr & 1 == 0 {
                    (current & 0xFF00) | value as u16
                } else {
                    (current & 0x00FF) | ((value as u16) << 8)
                };
                self.io_write16(core, addr & !1, spliced);
            }
        }
    }

    /// The nine `VRAMCNT` bytes and `WRAMCNT`, which share a run of addresses but are different
    /// registers — bank H and I are at 0x248/0x249 with `WRAMCNT` between them at 0x247.
    fn vramcnt_read(&self, addr: u32) -> u8 {
        match addr {
            0x0400_0240..=0x0400_0246 => self.vram.control((addr - 0x0400_0240) as usize),
            0x0400_0247 => self.memory.split().bits(),
            0x0400_0248 => self.vram.control(7),
            0x0400_0249 => self.vram.control(8),
            _ => 0,
        }
    }

    fn vramcnt_write(&mut self, addr: u32, value: u8) {
        match addr {
            0x0400_0240..=0x0400_0246 => {
                self.vram.set_control((addr - 0x0400_0240) as usize, value)
            }
            0x0400_0247 => self.memory.set_split(WramSplit::from_bits(value)),
            0x0400_0248 => self.vram.set_control(7, value),
            0x0400_0249 => self.vram.set_control(8, value),
            _ => {}
        }
    }

    /// Whether this core currently owns the Slot-1 cartridge.
    ///
    /// `EXMEMCNT` bit 11 hands it to one core or the other, and a game moves it: the ARM7 loads
    /// the first blocks during boot and then passes the slot to the ARM9.
    fn owns_card(&self, core: Core) -> bool {
        let owner = if self.exmemcnt & (1 << 11) != 0 {
            Core::Arm7
        } else {
            Core::Arm9
        };
        core == owner
    }
}

/// What an intercepted BIOS call costs, in the calling core's own cycles.
///
/// A guess, and deliberately a small one: the real cost is whatever the BIOS routine takes, which
/// varies from a handful of cycles for `Div` to thousands for a decompression. What matters here
/// is only that it is not zero — a core sitting in an `IntrWait` that is never satisfied re-runs
/// the `SWI` on every wake, and a free call would spin the slice forever.
const HLE_CYCLES: i64 = 3;

/// The comment field of the `SWI` at `addr`.
///
/// The two encodings do not keep it in the same place. Thumb's `1101 1111 imm8` carries it in the
/// low byte, while ARM's `cond 1111 imm24` carries it in bits 16-23 — which is why ARM source that
/// wants `Div` is written `swi 0x090000`, and why assembling `swi 0x09` there calls `SoftReset`.
fn swi_comment<B: Bus + ?Sized>(bus: &mut B, addr: u32, thumb: bool) -> u8 {
    if thumb {
        bus.read16(addr) as u8
    } else {
        (bus.read32(addr) >> 16) as u8
    }
}

/// Enter a game's interrupt handler the way the BIOS does.
///
/// A real BIOS does not jump to the handler and hope. It pushes the registers the ARM procedure
/// standard lets a callee clobber, calls the handler with `LR` pointing at its own epilogue, and
/// on return restores them and leaves the exception with `subs pc, lr, #4` — which is the step
/// that puts `CPSR` back and unmasks interrupts.
///
/// Skipping the wrapper is not a small inaccuracy. libnds's `IntrMain` ends in `bx lr`, so without
/// it the handler returns into the *interrupted code* while still in IRQ mode with interrupts
/// masked: the machine takes exactly one interrupt, never another, and every `IntrWait` after that
/// waits forever. `system-gba` learned this the same way and does the same thing.
fn enter_irq_handler<B: Bus + ?Sized>(
    cpu: &mut Arm7Tdmi,
    bus: &mut B,
    lr: u32,
    handler: u32,
    ret: u32,
) {
    cpu.enter_exception(Exception::Irq, lr);
    let mut sp = cpu.regs.read(Mode::Irq, 13);
    for value in [
        cpu.reg(0),
        cpu.reg(1),
        cpu.reg(2),
        cpu.reg(3),
        cpu.reg(12),
        lr,
    ]
    .into_iter()
    .rev()
    {
        sp = sp.wrapping_sub(4);
        bus.write32(sp, value);
    }
    cpu.regs.write(Mode::Irq, 13, sp);
    // `LR` is the epilogue's address rather than the return address, which is what makes the
    // handler's `bx lr` come back to the wrapper instead of into the interrupted code.
    cpu.regs.write(Mode::Irq, 14, ret);
    cpu.regs.set_pc(handler);
}

/// The wrapper's epilogue: restore what it pushed and leave the exception.
fn leave_irq_handler<B: Bus + ?Sized>(cpu: &mut Arm7Tdmi, bus: &mut B) {
    let mut sp = cpu.regs.read(Mode::Irq, 13);
    let mut popped = [0u32; 6];
    for slot in &mut popped {
        *slot = bus.read32(sp);
        sp = sp.wrapping_add(4);
    }
    let [r0, r1, r2, r3, r12, lr] = popped;
    cpu.regs.write(Mode::Irq, 13, sp);
    cpu.set_reg(0, r0);
    cpu.set_reg(1, r1);
    cpu.set_reg(2, r2);
    cpu.set_reg(3, r3);
    cpu.set_reg(12, r12);
    // `subs pc, lr, #4`: restore the saved status register and resume where the interrupt struck.
    // Restoring `CPSR` is the step that unmasks interrupts again, so leaving it out is what makes
    // a machine take exactly one interrupt and then no more.
    cpu.exception_return(lr.wrapping_sub(4));
}

/// The Nintendo DS.
pub struct NdsSystem {
    arm9: Arm946e,
    arm7: Arm7Tdmi,
    bus: NdsBus,
    framebuffer: Framebuffer,
    audio: Vec<AudioSample>,
    /// Cycles the ARM9 has run past its budget, carried into the next slice. Without it the
    /// faster core drifts ahead by up to one instruction per slice, thousands of times a frame.
    arm9_debt: i64,
    arm7_debt: i64,
    frame_cycles: u64,
}

impl Default for NdsSystem {
    fn default() -> Self {
        Self::new(None, None)
    }
}

impl NdsSystem {
    pub fn new(arm9_bios: Option<Vec<u8>>, arm7_bios: Option<Vec<u8>>) -> Self {
        Self {
            arm9: Arm946e::new(BootState::default()),
            arm7: Arm7Tdmi::new(BootState::default()),
            bus: NdsBus::new(arm9_bios, arm7_bios),
            framebuffer: Framebuffer::new(SCREEN_WIDTH, FRAMEBUFFER_HEIGHT),
            audio: Vec::new(),
            arm9_debt: 0,
            arm7_debt: 0,
            frame_cycles: 0,
        }
    }

    pub fn bus(&self) -> &NdsBus {
        &self.bus
    }

    pub fn bus_mut(&mut self) -> &mut NdsBus {
        &mut self.bus
    }

    pub fn arm9(&self) -> &Arm946e {
        &self.arm9
    }

    pub fn arm9_mut(&mut self) -> &mut Arm946e {
        &mut self.arm9
    }

    pub fn arm7(&self) -> &Arm7Tdmi {
        &self.arm7
    }

    /// A side-effect-free read through the ARM9's view, TCMs included.
    ///
    /// The TCMs sit between the core and the bus, so an address inside one never reaches the bus.
    /// A debugger that read the bus instead would show a game's stack — which lives in DTCM — as
    /// whatever main RAM holds there.
    pub fn peek_arm9(&self, addr: u32) -> Option<u8> {
        if self.arm9.itcm.responds_to_read(addr) {
            return Some(self.arm9.itcm.read8(addr));
        }
        if self.arm9.dtcm.responds_to_read(addr) {
            return Some(self.arm9.dtcm.read8(addr));
        }
        self.bus.peek8(Core::Arm9, addr)
    }

    /// Do what the firmware does: copy both binaries into RAM, fabricate the blocks software
    /// expects to find there, and point each core at its entry.
    fn direct_boot(&mut self) {
        let (nine, nine_bytes, seven, seven_bytes) = self.bus.cart.direct_boot();
        let nine_bytes = nine_bytes.to_vec();
        let seven_bytes = seven_bytes.to_vec();
        let header = self.bus.cart.header_bytes().to_vec();

        // System mode with interrupts unmasked, at the entry point: the state the firmware
        // leaves each core in, which is what the GBA assembly's `boot_state` does for its one.
        self.arm9 = Arm946e::new(BootState {
            pc: nine.entry,
            mode: Mode::System,
            thumb: false,
            // Replaced below, once `post_boot_nds` has put DTCM where the ARM9's stacks live.
            sp: 0,
            irq_disabled: false,
            fiq_disabled: true,
        });
        self.arm9.post_boot_nds();
        let dtcm = self.arm9.dtcm.base();
        self.arm9
            .core
            .regs
            .write(Mode::System, 13, dtcm + ARM9_SP_SYSTEM_OFFSET);
        self.arm9
            .core
            .regs
            .write(Mode::Irq, 13, dtcm + ARM9_SP_IRQ_OFFSET);
        self.arm9
            .core
            .regs
            .write(Mode::Supervisor, 13, dtcm + ARM9_SP_SUPERVISOR_OFFSET);

        self.arm7 = Arm7Tdmi::new(BootState {
            pc: seven.entry,
            mode: Mode::System,
            thumb: false,
            sp: SP_SYSTEM,
            irq_disabled: false,
            fiq_disabled: true,
        });
        self.arm7.regs.write(Mode::Irq, 13, SP_IRQ);
        self.arm7.regs.write(Mode::Supervisor, 13, SP_SUPERVISOR);

        // The ARM9 owns the card slot after boot, and the firmware has already split the shared
        // WRAM the way a booting game expects to find it.
        //
        // The split has to be set *before* the binaries are copied, because it decides where the
        // ARM7's own address for its binary lands. A libnds ARM7 links at `0x037F8000`, and that
        // address is only its work RAM when the ARM7 owns all 32 KiB of the shared block: the
        // shared WRAM then sits at `0x037F_8000`-`0x037F_FFFF` immediately below the private
        // 64 KiB at `0x0380_0000`, giving the 96 KiB contiguous window `ds_arm7.ld` describes.
        // With any other split that address is a 16 KiB window instead, and a 62 KiB ARM7 binary
        // copied into it wraps four times over itself.
        self.bus.exmemcnt = 0;
        self.bus.memory.set_split(WramSplit::Arm7All);

        // Both binaries go through the plain bus. A header's ARM9 load address is always in
        // main RAM — software that wants code in ITCM copies it there itself once running — so
        // nothing here needs the TCM-aware view the CPU crate keeps to itself.
        for (i, byte) in nine_bytes.iter().enumerate() {
            self.bus
                .write8(Core::Arm9, nine.ram_address.wrapping_add(i as u32), *byte);
        }
        for (i, byte) in seven_bytes.iter().enumerate() {
            self.bus
                .write8(Core::Arm7, seven.ram_address.wrapping_add(i as u32), *byte);
        }

        // The firmware leaves a copy of the header where software looks for it.
        for (i, byte) in header.iter().enumerate() {
            self.bus.write8(Core::Arm9, HEADER_MIRROR + i as u32, *byte);
        }
        self.write_user_settings();

        // What the firmware leaves POWCNT1 holding: both engines and both LCDs powered, with
        // engine A sent to the *upper* screen. Bit 15 is clear at power-on, so a machine that
        // never ran the firmware draws the main engine on the bottom screen — which looks like
        // the two screens being swapped rather than like a missing boot step.
        self.bus.powcnt1 = 0x820F;
    }

    /// Fabricate the firmware user-settings block direct boot cannot copy.
    ///
    /// The only field that matters here is the touchscreen calibration, and it has to be the
    /// inverse of what [`crate::input`]'s controller reports — see `RAW_PER_PIXEL`. Two points,
    /// one at the origin and one at the far corner, give the linear mapping software expects.
    fn write_user_settings(&mut self) {
        let put16 = |bus: &mut NdsBus, at: u32, v: u16| {
            bus.write8(Core::Arm9, at, v as u8);
            bus.write8(Core::Arm9, at + 1, (v >> 8) as u8);
        };
        let base = USER_SETTINGS;
        put16(&mut self.bus, base + 0x58, 0); // ADC x1
        put16(&mut self.bus, base + 0x5A, 0); // ADC y1
        self.bus.write8(Core::Arm9, base + 0x5C, 0); // screen x1
        self.bus.write8(Core::Arm9, base + 0x5D, 0); // screen y1
        put16(&mut self.bus, base + 0x5E, 255 * RAW_PER_PIXEL);
        put16(&mut self.bus, base + 0x60, 191 * RAW_PER_PIXEL);
        self.bus.write8(Core::Arm9, base + 0x62, 255);
        self.bus.write8(Core::Arm9, base + 0x63, 191);
    }

    /// Run both cores for `cycles` system cycles.
    ///
    /// The ARM9 runs at twice the clock, and both carry a debt so an instruction that overruns
    /// the slice is paid for out of the next one rather than being free.
    fn run_cores(&mut self, cycles: u32) {
        let mut budget9 = cycles as i64 * 2 - self.arm9_debt;
        while budget9 > 0 {
            if self.bus.halted[Core::Arm9 as usize] {
                if self.bus.irq[Core::Arm9 as usize].active() != 0 {
                    self.bus.halted[Core::Arm9 as usize] = false;
                } else {
                    budget9 = 0;
                    break;
                }
            }
            self.service_interrupt(Core::Arm9);
            // Both stand in for BIOS code, so both cost time rather than being free: a wait that
            // is never satisfied must still run the slice down and hand the frame loop back.
            if self.at_bios_entry(Core::Arm9) {
                self.run_bios_hle(Core::Arm9);
                budget9 -= HLE_CYCLES;
                continue;
            }
            let mut view = Arm9View(&mut self.bus);
            budget9 -= self.arm9.step(&mut view).0 as i64;
        }
        self.arm9_debt = -budget9;

        let mut budget7 = cycles as i64 - self.arm7_debt;
        while budget7 > 0 {
            if self.bus.halted[Core::Arm7 as usize] {
                if self.bus.irq[Core::Arm7 as usize].active() != 0 {
                    self.bus.halted[Core::Arm7 as usize] = false;
                } else {
                    budget7 = 0;
                    break;
                }
            }
            self.service_interrupt(Core::Arm7);
            if self.at_bios_entry(Core::Arm7) {
                self.run_bios_hle(Core::Arm7);
                budget7 -= HLE_CYCLES;
                continue;
            }
            let mut view = Arm7View(&mut self.bus);
            budget7 -= self.arm7.step(&mut view).0 as i64;
        }
        self.arm7_debt = -budget7;

        let irq9 = self.bus.timers[Core::Arm9 as usize].step(cycles);
        let irq7 = self.bus.timers[Core::Arm7 as usize].step(cycles);
        // The sound hardware fetches its own sample data, so it needs memory — borrowed as a
        // separate field rather than through the bus, which is already borrowed mutably here.
        let NdsBus { apu, memory, .. } = &mut self.bus;
        apu.step(cycles, memory);
        for (core, mask) in [(Core::Arm9, irq9), (Core::Arm7, irq7)] {
            for channel in 0..4 {
                if mask & (1 << channel) != 0 {
                    self.bus.irq[core as usize].raise(sources::timer(channel));
                }
            }
        }
        self.drain_events();
        self.run_dma();
    }

    /// Move what the IPC hardware, the video timing, and the keypad have latched into the two
    /// interrupt controllers.
    fn drain_events(&mut self) {
        for core in [Core::Arm9, Core::Arm7] {
            let ipc = self.bus.ipc.take_pending(core);
            let mut raise = 0u32;
            if ipc.sync {
                raise |= sources::IPC_SYNC;
            }
            if ipc.send_empty {
                raise |= sources::IPC_SEND_EMPTY;
            }
            if ipc.recv_not_empty {
                raise |= sources::IPC_RECV_NOT_EMPTY;
            }
            let video = self.bus.video.take_pending(core);
            if video.vblank {
                raise |= sources::VBLANK;
            }
            if video.hblank {
                raise |= sources::HBLANK;
            }
            if video.vcount {
                raise |= sources::VCOUNT;
            }
            if self.bus.input.irq_pending() {
                raise |= sources::KEYPAD;
            }
            if raise != 0 {
                self.bus.irq[core as usize].raise(raise);
            }
        }
    }

    /// Assert or clear a core's interrupt line, standing in for its BIOS when none is supplied.
    ///
    /// With a BIOS the core enters at its vector and the BIOS does the rest. Without one — which
    /// is every run of this emulator, since it vendors no BIOS — this does what the BIOS does and
    /// no more: enter the exception properly, then redirect to the handler address the game left
    /// at the pointer its BIOS reads. Exactly the arrangement `system-gba` uses, with two
    /// pointers instead of one because the two cores keep theirs in different places.
    #[inline]
    fn service_interrupt(&mut self, core: Core) {
        let pending = self.bus.irq[core as usize].pending();
        match core {
            Core::Arm9 => self.arm9.set_irq_line(pending),
            Core::Arm7 => self.arm7.set_irq_line(pending),
        }
        if pending {
            self.enter_interrupt(core);
        }
    }

    /// The rest of [`Self::service_interrupt`], split off and never inlined.
    ///
    /// The caller runs before every instruction on both cores; this runs a few thousand times a
    /// frame. Keeping them in one function put the whole of it — two handler-pointer reads, the
    /// register frame, and the mode change — inside the frame loop's hot path.
    #[inline(never)]
    fn enter_interrupt(&mut self, core: Core) {
        let has_bios = match core {
            Core::Arm9 => self.bus.memory.has_arm9_bios(),
            Core::Arm7 => self.bus.memory.has_arm7_bios(),
        };
        if has_bios {
            return;
        }
        match core {
            Core::Arm9 => {
                if self.arm9.core.cpsr.irq_disabled() {
                    return;
                }
                // The ARM9's pointer is at the top of DTCM, wherever CP15 has put it, and DTCM
                // is inside the CPU crate rather than on the bus.
                let addr = self.arm9.dtcm.base() + ARM9_HANDLER_OFFSET;
                let handler = u32::from_le_bytes(std::array::from_fn(|i| {
                    self.arm9.dtcm.read8(addr + i as u32)
                }));
                if handler == 0 {
                    return;
                }
                let lr = self.arm9.core.regs.pc().wrapping_add(4);
                let Self { arm9, bus, .. } = self;
                let mut view = Arm9View(bus);
                // Through the TCM view: after libnds's startup code runs, the ARM9's IRQ stack is
                // in DTCM, and pushing to the bus instead would scribble on main RAM and pop back
                // whatever was there.
                arm9.with_bus(&mut view, |cpu, memory| {
                    enter_irq_handler(cpu, memory, lr, handler, ARM9_IRQ_RETURN);
                });
                self.arm9.set_irq_line(false);
            }
            Core::Arm7 => {
                if self.arm7.cpsr.irq_disabled() {
                    return;
                }
                let handler = self.bus.read32(Core::Arm7, ARM7_HANDLER_POINTER);
                if handler == 0 {
                    return;
                }
                let lr = self.arm7.regs.pc().wrapping_add(4);
                let Self { arm7, bus, .. } = self;
                let mut view = Arm7View(bus);
                enter_irq_handler(arm7, &mut view, lr, handler, ARM7_IRQ_RETURN);
                self.arm7.set_irq_line(false);
            }
        }
    }

    /// Whether this core's program counter is at an address only the BIOS HLE can put it at.
    ///
    /// This is the whole of what the frame loop pays per instruction, and it has to stay that
    /// cheap: two register comparisons that almost always fail. Both addresses are inside a BIOS
    /// region that is unmapped when no image is supplied, so software cannot reach either by
    /// accident, and the `has_*_bios` check is last because it is the one that touches memory.
    ///
    /// Written as one function taking the core rather than two, and inlined so the `match` folds
    /// away at each of its two call sites — where the core is a literal.
    #[inline(always)]
    fn at_bios_entry(&self, core: Core) -> bool {
        let swi = Exception::SoftwareInterrupt.vector();
        match core {
            Core::Arm9 => {
                let pc = self.arm9.core.regs.pc();
                (pc == ARM9_IRQ_RETURN || pc == self.arm9.core.exception_base().wrapping_add(swi))
                    && !self.bus.memory.has_arm9_bios()
            }
            Core::Arm7 => {
                let pc = self.arm7.regs.pc();
                (pc == ARM7_IRQ_RETURN || pc == self.arm7.exception_base().wrapping_add(swi))
                    && !self.bus.memory.has_arm7_bios()
            }
        }
    }

    /// Do whatever the BIOS would do at the address [`Self::at_bios_entry`] just recognised.
    ///
    /// Split from the gate and never inlined, so the frame loop's inner loop stays small: this
    /// runs a few thousand times a frame at most, against several hundred thousand instructions.
    #[inline(never)]
    fn run_bios_hle(&mut self, core: Core) {
        if !self.intercept_bios_irq_return(core) {
            self.intercept_bios_call(core);
        }
    }

    /// The other half of the wrapper: unwind and leave the exception.
    ///
    /// Recognised by the program counter reaching the core's [`ARM9_IRQ_RETURN`] or
    /// [`ARM7_IRQ_RETURN`], which are addresses inside a BIOS that is not there — so the only way
    /// to arrive at one is the `LR` [`Self::service_interrupt`] planted.
    fn intercept_bios_irq_return(&mut self, core: Core) -> bool {
        let (has_bios, pc, expected) = match core {
            Core::Arm9 => (
                self.bus.memory.has_arm9_bios(),
                self.arm9.core.regs.pc(),
                ARM9_IRQ_RETURN,
            ),
            Core::Arm7 => (
                self.bus.memory.has_arm7_bios(),
                self.arm7.regs.pc(),
                ARM7_IRQ_RETURN,
            ),
        };
        if has_bios || pc != expected {
            return false;
        }
        match core {
            Core::Arm9 => {
                let Self { arm9, bus, .. } = self;
                let mut view = Arm9View(bus);
                arm9.with_bus(&mut view, |cpu, memory| leave_irq_handler(cpu, memory));
            }
            Core::Arm7 => {
                let Self { arm7, bus, .. } = self;
                let mut view = Arm7View(bus);
                leave_irq_handler(arm7, &mut view);
            }
        }
        true
    }

    /// Answer a `SWI` in place of the BIOS, when there is no BIOS to answer it.
    ///
    /// Recognised by the program counter reaching the core's `SWI` vector — `0xFFFF_0008` on the
    /// ARM9 with its high vectors, `0x0000_0008` on the ARM7 — which is where the core has just
    /// taken the exception to and which holds nothing at all with no BIOS supplied. What the
    /// machine did before this existed: read open bus there, decode the zero it found as
    /// `andeq r0, r0, r0`, and execute that for the rest of the run.
    ///
    /// With a BIOS image supplied for this core, this does nothing and the exception runs on into
    /// the real vector.
    ///
    /// # Why the vector rather than the instruction
    ///
    /// `system-gba` catches its `SWI`s one step earlier, by peeking the opcode ahead of *every*
    /// instruction and stepping over the ones that turn out to be calls. That is a question asked
    /// tens of millions of times a second whose answer is almost always no, and on a DS it is
    /// asked on two cores: measured at +80% on a frame of the DS rendering benchmark, which took
    /// the machine past its own frame budget and would have made this change a regression rather
    /// than a feature.
    ///
    /// Trapping at the vector is one comparison per instruction instead, and reads the comment
    /// byte once per call rather than once per instruction. It also answers a *conditional* `SWI`
    /// correctly, which the peek could not without duplicating the flag check.
    fn intercept_bios_call(&mut self, core: Core) -> bool {
        let vector = Exception::SoftwareInterrupt.vector();
        let (has_bios, pc, expected) = match core {
            Core::Arm9 => (
                self.bus.memory.has_arm9_bios(),
                self.arm9.core.regs.pc(),
                self.arm9.core.exception_base().wrapping_add(vector),
            ),
            Core::Arm7 => (
                self.bus.memory.has_arm7_bios(),
                self.arm7.regs.pc(),
                self.arm7.exception_base().wrapping_add(vector),
            ),
        };
        if has_bios || pc != expected {
            return false;
        }

        // `R14_svc` holds the address of the instruction *after* the `SWI`, so the call itself is
        // one instruction back — and how far back depends on the state the exception saved, not on
        // the state the core is in now, because entering an exception always enters ARM.
        let (lr, thumb) = match core {
            Core::Arm9 => (
                self.arm9.core.regs.read(Mode::Supervisor, 14),
                self.arm9.core.regs.spsr(Mode::Supervisor),
            ),
            Core::Arm7 => (
                self.arm7.regs.read(Mode::Supervisor, 14),
                self.arm7.regs.spsr(Mode::Supervisor),
            ),
        };
        let thumb = thumb.is_some_and(|saved| saved.thumb());
        let call_at = lr.wrapping_sub(if thumb { 2 } else { 4 });

        // The ARM9's flag word moves with DTCM, so it is computed per call rather than being a
        // constant like the ARM7's.
        let flags = match core {
            Core::Arm9 => self.arm9.dtcm.base() + ARM9_IRQ_FLAGS_OFFSET,
            Core::Arm7 => ARM7_IRQ_FLAGS,
        };
        // Copied out and written back, because the bus view the call runs against borrows the
        // structure this flag lives in.
        let mut waiting = self.bus.intr_wait[core as usize];
        let effect = {
            let context = bios::Context {
                core,
                flags,
                waiting: &mut waiting,
            };
            match core {
                Core::Arm9 => {
                    let Self { arm9, bus, .. } = self;
                    let mut view = Arm9View(bus);
                    // Through the TCM view, because a libnds program's code, stack, and data are
                    // routinely in a TCM — including the word `IntrWait` tests.
                    arm9.with_bus(&mut view, |cpu, memory| {
                        let comment = swi_comment(memory, call_at, thumb);
                        bios::dispatch(cpu, memory, comment, context)
                    })
                }
                Core::Arm7 => {
                    let Self { arm7, bus, .. } = self;
                    let mut view = Arm7View(bus);
                    let comment = swi_comment(&mut view, call_at, thumb);
                    bios::dispatch(arm7, &mut view, comment, context)
                }
            }
        };
        self.bus.intr_wait[core as usize] = waiting;

        if effect.halt {
            self.bus.halted[core as usize] = true;
        }
        // Leaving the exception restores the caller's mode, instruction set, and interrupt mask.
        // An `IntrWait` whose condition is still unmet returns to the `SWI` itself rather than
        // past it, so waking runs the test again — and it must leave the exception to do that, or
        // the interrupt it is waiting for could never be taken.
        let resume = if effect.repeat { call_at } else { lr };
        match core {
            Core::Arm9 => self.arm9.core.exception_return(resume),
            Core::Arm7 => self.arm7.exception_return(resume),
        }
        true
    }

    /// Perform every DMA transfer that is ready, on both cores.
    fn run_dma(&mut self) {
        for core in [Core::Arm9, Core::Arm7] {
            while let Some(transfer) = self.bus.dma[core as usize].take_transfer() {
                self.perform_transfer(core, &transfer);
                if transfer.raise_irq {
                    self.bus.irq[core as usize].raise(sources::dma(transfer.channel));
                }
            }
        }
    }

    fn perform_transfer(&mut self, core: Core, transfer: &Transfer) {
        let mut source = transfer.source;
        let mut destination = transfer.destination;
        let step = |addr: u32, step: AddressStep, unit: u32| match step {
            AddressStep::Increment | AddressStep::IncrementReload => addr.wrapping_add(unit),
            AddressStep::Decrement => addr.wrapping_sub(unit),
            AddressStep::Fixed => addr,
        };
        for _ in 0..transfer.words {
            if transfer.unit == 4 {
                let value = self.bus.read32(core, source & !3);
                self.bus.write32(core, destination & !3, value);
            } else {
                let value = self.bus.read16(core, source & !1);
                self.bus.write16(core, destination & !1, value);
            }
            source = step(source, transfer.source_step, transfer.unit);
            destination = step(destination, transfer.destination_step, transfer.unit);
        }
    }

    /// Composite one line of each screen into the framebuffer.
    ///
    /// `POWCNT1` bit 15 decides which engine drives which screen, and games do swap it — so the
    /// row an engine writes to is looked up rather than fixed.
    fn render_line(&mut self, line: u32) {
        let top = self.bus.top_engine();
        let NdsBus {
            vram,
            memory,
            engine_a,
            engine_b,
            gpu3d,
            ..
        } = &mut self.bus;
        let three_d = gpu3d.enabled().then_some(&gpu3d.framebuffer);
        let row_of = |engine: Engine| {
            if engine == top {
                line
            } else {
                line + SCREEN_HEIGHT
            }
        };
        engine_a.render_line_with_3d(
            line,
            vram,
            memory.palette(),
            memory.oam(),
            three_d,
            self.framebuffer.row_mut(row_of(Engine::A)),
        );
        engine_b.render_line(
            line,
            vram,
            memory.palette(),
            memory.oam(),
            self.framebuffer.row_mut(row_of(Engine::B)),
        );
    }
}

impl System for NdsSystem {
    fn id(&self) -> &'static str {
        "nds"
    }

    fn display_name(&self) -> &'static str {
        "Nintendo DS"
    }

    /// Raised to 2 when the BIOS HLE added the per-core `IntrWait` flag: a state written before
    /// it does not carry that word, and loading one into this build would leave a core waiting
    /// with no record of whether it had already discarded its flags.
    fn state_version(&self) -> u32 {
        2
    }

    fn set_input(&mut self, input: InputState) {
        self.bus.input.set_input(input);
    }

    fn step_frame(&mut self, input: InputState) -> FrameOutput {
        self.set_input(input);
        let start = self.frame_cycles;

        loop {
            let budget = self.bus.video.cycles_until_next_event();
            self.run_cores(budget);
            self.frame_cycles += budget as u64;

            match self.bus.video.advance(budget) {
                Some(VideoEvent::HBlankStart) => {
                    let line = self.bus.video.line();
                    if line < SCREEN_HEIGHT as u16 {
                        self.render_line(line as u32);
                    }
                    for core in [Core::Arm9, Core::Arm7] {
                        self.bus.dma[core as usize].on_hblank();
                    }
                }
                Some(VideoEvent::LineEnd) => {
                    let line = self.bus.video.line();
                    if line <= SCREEN_HEIGHT as u16 {
                        self.bus.engine_a.on_line_end();
                        self.bus.engine_b.on_line_end();
                    }
                    if line == SCREEN_HEIGHT as u16 {
                        // The 3D swap happens at vertical blank, not where `SWAP_BUFFERS`
                        // appeared in the display list — which is what lets a game build the
                        // next frame's geometry while this one is still being scanned out.
                        let NdsBus { gpu3d, vram, .. } = &mut self.bus;
                        gpu3d.on_vblank(vram);
                        for core in [Core::Arm9, Core::Arm7] {
                            self.bus.dma[core as usize].on_vblank();
                        }
                    }
                    if line == 0 {
                        self.bus.engine_a.on_frame_start();
                        self.bus.engine_b.on_frame_start();
                        break;
                    }
                }
                None => {}
            }
        }

        let save_ram_dirty = self.bus.cart.save.is_dirty();
        self.bus.cart.save.clear_dirty();
        FrameOutput {
            cycles_elapsed: Cycles(self.frame_cycles - start),
            save_ram_dirty,
            stopped: false,
        }
    }

    fn step_instruction(&mut self) -> Cycles {
        self.service_interrupt(Core::Arm9);
        // Single-stepping has to see the same machine the frame loop does, or a `SWI` stepped over
        // in the debugger would take the exception the frame loop never lets it take.
        if self.at_bios_entry(Core::Arm9) {
            self.run_bios_hle(Core::Arm9);
            self.run_cores(0);
            let system_cycles = (HLE_CYCLES as u64).div_ceil(2);
            self.frame_cycles += system_cycles;
            return Cycles(system_cycles);
        }
        let mut view = Arm9View(&mut self.bus);
        let cycles = self.arm9.step(&mut view);
        // One ARM9 instruction is half as many system cycles, rounded up so a single-step always
        // advances the rest of the machine.
        let system_cycles = cycles.0.div_ceil(2).max(1);
        self.run_cores(0);
        self.frame_cycles += system_cycles;
        Cycles(system_cycles)
    }

    fn reset(&mut self) {
        let has_cart = self.bus.cart.is_present();
        self.arm9 = Arm946e::new(BootState::default());
        self.arm7 = Arm7Tdmi::new(BootState::default());
        self.bus.memory.reset();
        self.bus.apu.reset();
        self.bus.vram = Vram::new();
        self.bus.ipc = Ipc::new();
        self.bus.input.reset();
        self.bus.video.reset();
        self.bus.engine_a = Engine2d::new(Engine::A);
        self.bus.engine_b = Engine2d::new(Engine::B);
        self.bus.gpu3d.reset();
        self.bus.irq = [
            InterruptController::new(Core::Arm9),
            InterruptController::new(Core::Arm7),
        ];
        self.bus.timers = [TimerBlock::new(); 2];
        self.bus.dma = [
            DmaController::new(Core::Arm9),
            DmaController::new(Core::Arm7),
        ];
        self.bus.cart.reset();
        self.bus.halted = [false; 2];
        self.bus.intr_wait = [false; 2];
        self.bus.postflg = [0; 2];
        self.bus.powcnt1 = 0x0203;
        self.arm9_debt = 0;
        self.arm7_debt = 0;
        self.frame_cycles = 0;
        self.audio.clear();
        if has_cart {
            self.direct_boot();
        }
    }

    fn load_cartridge(&mut self, rom: &[u8]) -> Result<(), CartridgeError> {
        self.bus.cart = NdsCartridge::new(rom.to_vec())?;
        self.reset();
        Ok(())
    }

    fn framebuffer(&self) -> &Framebuffer {
        &self.framebuffer
    }

    fn take_audio_samples(&mut self) -> &[AudioSample] {
        self.bus.apu.take_samples()
    }

    /// The debugger sees the ARM9. See [`crate::debug`] for why, and for what that leaves out.
    fn debug(&mut self) -> Option<&mut dyn DebugTarget> {
        Some(self)
    }

    fn access_log(&mut self) -> Option<&mut AccessLog> {
        Some(&mut self.bus.watch)
    }

    fn save_ram(&self) -> Option<&[u8]> {
        self.bus.cart.save_ram()
    }

    fn load_save_ram(&mut self, data: &[u8]) -> Result<(), CartridgeError> {
        self.bus.cart.load_save_ram(data)
    }
}

impl Savable for NdsSystem {
    fn save(&self, w: &mut StateWriter) {
        self.arm9.save(w);
        self.arm7.save(w);
        self.bus.memory.save(w);
        self.bus.vram.save(w);
        self.bus.ipc.save(w);
        self.bus.cart.save(w);
        self.bus.input.save(w);
        self.bus.video.save(w);
        self.bus.engine_a.save(w);
        self.bus.engine_b.save(w);
        self.bus.gpu3d.save(w);
        self.bus.apu.save(w);
        for controller in &self.bus.irq {
            controller.save(w);
        }
        for block in &self.bus.timers {
            block.save(w);
        }
        for controller in &self.bus.dma {
            controller.save(w);
        }
        w.write_u8(self.bus.postflg[0]);
        w.write_u8(self.bus.postflg[1]);
        w.write_u16(self.bus.powcnt1);
        w.write_u16(self.bus.exmemcnt);
        w.write_bool(self.bus.halted[0]);
        w.write_bool(self.bus.halted[1]);
        w.write_bool(self.bus.intr_wait[0]);
        w.write_bool(self.bus.intr_wait[1]);
        w.write_i64(self.arm9_debt);
        w.write_i64(self.arm7_debt);
        w.write_u64(self.frame_cycles);
        // The access log is not saved: it holds what one instruction did, and a save state is
        // taken between instructions. Carrying it would be saving the debugger, not the machine.
    }

    fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        self.arm9.load(r)?;
        self.arm7.load(r)?;
        self.bus.memory.load(r)?;
        self.bus.vram.load(r)?;
        self.bus.ipc.load(r)?;
        self.bus.cart.load(r)?;
        self.bus.input.load(r)?;
        self.bus.video.load(r)?;
        self.bus.engine_a.load(r)?;
        self.bus.engine_b.load(r)?;
        self.bus.gpu3d.load(r)?;
        self.bus.apu.load(r)?;
        for controller in &mut self.bus.irq {
            controller.load(r)?;
        }
        for block in &mut self.bus.timers {
            block.load(r)?;
        }
        for controller in &mut self.bus.dma {
            controller.load(r)?;
        }
        self.bus.postflg[0] = r.read_u8()?;
        self.bus.postflg[1] = r.read_u8()?;
        self.bus.powcnt1 = r.read_u16()?;
        self.bus.exmemcnt = r.read_u16()?;
        self.bus.halted[0] = r.read_bool()?;
        self.bus.halted[1] = r.read_bool()?;
        self.bus.intr_wait[0] = r.read_bool()?;
        self.bus.intr_wait[1] = r.read_bool()?;
        self.arm9_debt = r.read_i64()?;
        self.arm7_debt = r.read_i64()?;
        self.frame_cycles = r.read_u64()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
