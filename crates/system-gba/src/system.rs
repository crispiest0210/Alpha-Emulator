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
//! Each access asks [`WaitControl`] what it cost and reports that through [`Bus::tick`], the
//! same arrangement the Game Boy uses. A flat cost would make a game linked into the slow ROM
//! window run at the speed of one linked into the fast one.

use cart_common::GbaHeader;
use core_common::{
    AudioSample, Bus, CartridgeError, Cpu, Cycles, FrameOutput, Framebuffer, InputState, Savable,
    StateError, StateReader, StateWriter, System,
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
use crate::memory::{GbaBus, Region};
use crate::video::{VideoTiming, SCREEN_HEIGHT, SCREEN_WIDTH};
use crate::waitstates::{Access, WaitControl};
use crate::{background::Backgrounds, timers::Timers};

/// Bumped on any change to what this system serializes.
const STATE_VERSION: u32 = 1;

/// Cycles in one frame: 228 scanlines of 1232.
pub const FRAME_CYCLES: u64 = 280_896;
/// The CPU's clock.
pub const CLOCK_HZ: u64 = 16_777_216;

/// Where a cartridge's code begins.
const CARTRIDGE_ENTRY: u32 = 0x0800_0000;
/// Stacks the BIOS sets up before handing control to the game.
const SP_SYSTEM: u32 = 0x0300_7F00;
const SP_IRQ: u32 = 0x0300_7FA0;
const SP_SUPERVISOR: u32 = 0x0300_7FE0;

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
    pub sound: DirectSound,
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
            sound: DirectSound::new(),
            waits: WaitControl::new(),
            framebuffer: Framebuffer::new(SCREEN_WIDTH, SCREEN_HEIGHT),
            samples: Vec::with_capacity(1024),
            drained: Vec::with_capacity(1024),
            sample_accumulator: 0,
            frame_ready: false,
            pending_waits: 0,
            next_sequential: 0,
        }
    }

    /// Charge an access against the wait-state table.
    ///
    /// A ROM access that continues from the previous address is cheaper, because the cartridge
    /// bus keeps its latch — so this tracks where the last one ended rather than asking the
    /// caller, which would mean every call site knowing about cartridge timing.
    fn charge(&mut self, addr: u32, width: u32) {
        let access = if addr == self.next_sequential {
            Access::Sequential
        } else {
            Access::NonSequential
        };
        self.pending_waits += self.waits.cost(addr, width, access);
        self.next_sequential = addr.wrapping_add(width);
    }

    /// Take the wait-state cycles owed since the last call.
    pub fn take_pending_waits(&mut self) -> u32 {
        std::mem::take(&mut self.pending_waits)
    }

    /// Advance every clocked subsystem and act on what they report.
    pub fn advance(&mut self, cycles: u32) {
        let overflowed = self.timers.tick(cycles);
        if overflowed != 0 {
            let asked = self.timers.interrupts(overflowed);
            for channel in 0..4 {
                if asked & (1 << channel) != 0 {
                    self.irq.raise(irq::source::timer(channel));
                }
            }
            // A timer overflow is what advances a direct-sound channel, and draining one may
            // leave it needing a refill — which is a DMA request, not an audio event.
            self.sound.on_timer_overflow(overflowed);
            let requests: Vec<u32> = self.sound.refill_requests().collect();
            for address in requests {
                self.dma.on_fifo_empty(address);
            }
        }

        let events = self.video.tick(cycles);
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
        if events.entered_hblank {
            self.dma.on_hblank();
        }
        if events.entered_vblank {
            self.dma.on_vblank();
            self.frame_ready = true;
        }

        self.run_pending_dma();
        self.generate_samples(cycles as u64);
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
    fn run_pending_dma(&mut self) {
        while let Some(transfer) = self.dma.take_transfer() {
            let mut source = transfer.source;
            let mut destination = transfer.destination;
            for _ in 0..transfer.words {
                if transfer.unit == 4 {
                    let value = self.read32(source);
                    self.write32(destination, value);
                } else {
                    let value = self.read16(source);
                    self.write16(destination, value);
                }
                source = step(source, transfer.source_step, transfer.unit);
                destination = step(destination, transfer.destination_step, transfer.unit);
            }
            if transfer.raise_irq {
                self.irq.raise(irq::source::dma(transfer.channel));
            }
        }
    }

    fn generate_samples(&mut self, cycles: u64) {
        self.sample_accumulator += cycles * core_common::AUDIO_SAMPLE_RATE as u64;
        while self.sample_accumulator >= CLOCK_HZ {
            self.sample_accumulator -= CLOCK_HZ;
            let (left, right) = self.sound.output();
            self.samples.push(AudioSample { left, right });
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
        if self.video.write16(addr, value).is_some()
            || self.backgrounds.write16(addr, value).is_some()
            || self.timers.write16(addr, value).is_some()
            || self.dma.write16(addr, value).is_some()
            || self.effects.write16(addr, value).is_some()
            || self.irq.write16(addr, value).is_some()
            || self.keypad.write16(addr, value).is_some()
            || self.sound.write16(addr, value).is_some()
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

    /// A side-effect-free read, for the debugger's memory and disassembly views.
    ///
    /// I/O and cartridge save space answer `None`, and that is the honest answer rather than a
    /// gap. An I/O halfword read here would go through `read_io16`, which is where registers with
    /// read-side behaviour live; the save space is a flash or EEPROM state machine whose reads are
    /// commands. Showing `--` for those two regions is correct — a memory viewer that stepped a
    /// flash chip's state machine to avoid showing `--` would change the bug being investigated.
    fn peek8(&self, addr: u32) -> Option<u8> {
        match Region::of(addr) {
            Region::Io | Region::Sram => None,
            Region::Rom { .. } => Some(self.cartridge.read_rom(addr)),
            _ => self.memory.read8(addr),
        }
    }

    fn write8(&mut self, addr: u32, value: u8) {
        self.charge(addr, 1);
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

    fn read16(&mut self, addr: u32) -> u16 {
        let addr = addr & !1;
        self.charge(addr, 2);
        if Region::of(addr) == Region::Io {
            return self.read_io16(addr).unwrap_or(0);
        }
        u16::from_le_bytes([self.read8(addr), self.read8(addr + 1)])
    }

    fn write16(&mut self, addr: u32, value: u16) {
        let addr = addr & !1;
        self.charge(addr, 2);
        match Region::of(addr) {
            Region::Io => {
                self.write_io16(addr, value);
                if Self::is_dma_register(addr) {
                    self.run_pending_dma();
                }
            }
            Region::Rom { .. } => {}
            Region::Sram => self.cartridge.write_save(addr, value as u8),
            _ => {
                self.memory.write16(addr, value);
            }
        }
    }

    /// A 32-bit access is two halfword accesses, never four byte accesses.
    ///
    /// The default implementations decompose into bytes, which is catastrophic here: `write8`
    /// implements the 16-bit bus quirk where a byte written to palette RAM or VRAM is doubled
    /// across its halfword, so a word store would write each byte and then immediately overwrite
    /// it with the next. Storing 1 landed as 0. `gba-suite`'s memory test caught it on the
    /// third check; every 32-bit palette and VRAM write in every game would have been wrong.
    fn read32(&mut self, addr: u32) -> u32 {
        let addr = addr & !3;
        // The two halfword reads below charge themselves, and the second is sequential by
        // construction — which is exactly what the wait-state table says a word access costs.
        (self.read16(addr) as u32) | ((self.read16(addr.wrapping_add(2)) as u32) << 16)
    }

    fn write32(&mut self, addr: u32, value: u32) {
        let addr = addr & !3;
        self.write16(addr, value as u16);
        self.write16(addr.wrapping_add(2), (value >> 16) as u16);
    }

    fn tick(&mut self, _cycles: Cycles) {
        // Deliberately empty. The ARM core reports an instruction's cost by returning it from
        // `step`, not by calling this — unlike the SM83, which reports each access as it
        // happens. Advancing here as well would double every instruction's cost.
    }

    fn open_bus8(&self, addr: u32) -> u8 {
        (self.memory.open_bus32() >> ((addr & 3) * 8)) as u8
    }
}

/// A Game Boy Advance.
pub struct GbaSystem {
    cpu: Arm7Tdmi,
    bus: GbaSystemBus,
    save_ram_dirty: bool,
}

impl GbaSystem {
    pub fn new(rom: Vec<u8>, bios: Option<Vec<u8>>) -> Result<Self, CartridgeError> {
        let cartridge = Cartridge::new(rom)?;
        let has_bios = bios.is_some();
        let mut system = Self {
            cpu: Arm7Tdmi::new(boot_state(has_bios)),
            bus: GbaSystemBus::new(cartridge, bios),
            save_ram_dirty: false,
        };
        system.bus.memory.set_in_bios(has_bios);
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

    /// Answer a `SWI` in place of the BIOS, when there is no BIOS to answer it.
    ///
    /// Intercepted *before* the instruction executes rather than by trapping the exception
    /// afterwards, so the CPU never enters Supervisor mode and never jumps to the empty vector
    /// — which is exactly what it did before this existed, running off into unmapped memory
    /// after 84,701 correct instructions of `gba-suite`.
    ///
    /// With a real BIOS supplied this does nothing and the exception is taken normally.
    fn intercept_bios_call(&mut self) -> bool {
        if self.bus.memory.has_bios() || self.cpu.is_thumb() {
            // The Thumb form is `SWI imm8` at a different encoding; games use the ARM form for
            // these calls, and guessing at the other one is worse than not handling it.
            return false;
        }
        let pc = self.cpu.regs.pc();
        let opcode = self.bus.read32(pc);
        // `cond 1111 imm24`. Bits 24-27 identify the instruction; the 24 below them are the
        // comment, so the mask must not reach into them. Only the always-condition is handled:
        // a conditional `SWI` is vanishingly rare and would need the flag check duplicated here.
        if opcode & 0x0F00_0000 != 0x0F00_0000 || opcode >> 28 != 0xE {
            return false;
        }

        let comment = ((opcode >> 16) & 0xFF) as u8;
        let effect = bios::dispatch(&mut self.cpu, &mut self.bus, comment);
        if effect.halt {
            self.cpu.halt();
        }
        // Step over the instruction the BIOS would have returned from.
        self.cpu.regs.set_pc(pc.wrapping_add(4));
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
        // Enter the exception properly — banked registers, mode, and mask all change — then
        // redirect the program counter to the game's handler, which is the one thing the BIOS
        // would have done between the two.
        let lr = self.cpu.regs.pc().wrapping_add(4);
        self.cpu.enter_exception(Exception::Irq, lr);
        self.cpu.regs.set_pc(handler);
        self.cpu.set_irq_line(false);
    }
}

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

    fn step_instruction(&mut self) -> Cycles {
        self.service_interrupt();
        if self.intercept_bios_call() {
            // The call is answered without running the instruction, so it costs nothing beyond
            // a nominal cycle — the real BIOS is slower, and that will matter for a game timing
            // against it, but a wrong non-zero figure is no better than this one.
            self.bus.advance(1);
            self.bus.take_pending_waits();
            return Cycles(1);
        }
        let cycles = self.cpu.step(&mut self.bus).get().max(1);
        // The instruction's own cost plus whatever its memory accesses waited for. Charged
        // together so a scheduled event cannot fire between two halves of one access.
        let total = cycles as u32 + self.bus.take_pending_waits();
        self.bus.advance(total);
        Cycles(total as u64)
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

    fn step_frame(&mut self, input: InputState) -> FrameOutput {
        self.bus.keypad.set_input(input.buttons);
        if self.bus.keypad.interrupt_requested() {
            self.bus.irq.raise(irq::source::KEYPAD);
        }
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
        self.waits.load(r)?;
        self.sample_accumulator = r.read_u64()?;
        Ok(())
    }
}

impl Savable for GbaSystem {
    fn save(&self, w: &mut StateWriter) {
        self.cpu.save(w);
        self.bus.save(w);
    }

    fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        self.cpu.load(r)?;
        self.bus.load(r)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
