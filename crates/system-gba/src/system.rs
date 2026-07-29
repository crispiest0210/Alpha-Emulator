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

use crate::cartridge::Cartridge;
use crate::compositor::{self, Frame};
use crate::dma::DmaController;
use crate::fifo::DirectSound;
use crate::irq::{self, InterruptController};
use crate::memory::{GbaBus, Region};
use crate::video::{VideoTiming, SCREEN_HEIGHT, SCREEN_WIDTH};
use crate::waitstates::WaitControl;
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
    pub timers: Timers,
    pub dma: DmaController,
    pub irq: InterruptController,
    pub sound: DirectSound,
    pub waits: WaitControl,

    framebuffer: Framebuffer,
    samples: Vec<AudioSample>,
    drained: Vec<AudioSample>,
    /// Fixed-point accumulator for sample generation, in the same shape as the Game Boy's.
    sample_accumulator: u64,
    /// Set when the video hardware reached vertical blanking during this slice.
    frame_ready: bool,
}

impl GbaSystemBus {
    fn new(cartridge: Cartridge, bios: Option<Vec<u8>>) -> Self {
        Self {
            memory: GbaBus::new(bios),
            cartridge,
            video: VideoTiming::new(),
            backgrounds: Backgrounds::new(),
            timers: Timers::new(),
            dma: DmaController::new(),
            irq: InterruptController::new(),
            sound: DirectSound::new(),
            waits: WaitControl::new(),
            framebuffer: Framebuffer::new(SCREEN_WIDTH, SCREEN_HEIGHT),
            samples: Vec::with_capacity(1024),
            drained: Vec::with_capacity(1024),
            sample_accumulator: 0,
            frame_ready: false,
        }
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

        if let Some(line) = events.scanline_ready {
            self.render_line(line as u32);
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
        if let Some(value) = self.irq.read16(addr) {
            return Some(value);
        }
        if let Some(value) = self.sound.read16(addr) {
            return Some(value);
        }
        if WaitControl::owns(addr) {
            return Some(self.waits.read16());
        }
        None
    }

    fn write_io16(&mut self, addr: u32, value: u16) {
        if self.video.write16(addr, value).is_some()
            || self.backgrounds.write16(addr, value).is_some()
            || self.timers.write16(addr, value).is_some()
            || self.dma.write16(addr, value).is_some()
            || self.irq.write16(addr, value).is_some()
            || self.sound.write16(addr, value).is_some()
        {
            return;
        }
        if WaitControl::owns(addr) {
            self.waits.write16(value);
        }
    }
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

    fn write8(&mut self, addr: u32, value: u8) {
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
        if Region::of(addr) == Region::Io {
            return self.read_io16(addr).unwrap_or(0);
        }
        u16::from_le_bytes([self.read8(addr), self.read8(addr + 1)])
    }

    fn write16(&mut self, addr: u32, value: u16) {
        let addr = addr & !1;
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
    fn id(&self) -> &'static str {
        "gba"
    }

    fn display_name(&self) -> &'static str {
        "Game Boy Advance"
    }

    fn state_version(&self) -> u32 {
        STATE_VERSION
    }

    fn step_frame(&mut self, _input: InputState) -> FrameOutput {
        self.bus.frame_ready = false;
        self.save_ram_dirty = false;
        let mut elapsed = 0u64;

        // Bounded so a frontend always gets a frame back rather than hanging, the same guard
        // the Game Boy uses.
        while !self.bus.frame_ready && elapsed < FRAME_CYCLES * 2 {
            self.service_interrupt();
            // The ARM core reports an instruction's cost by returning it rather than through
            // `Bus::tick` the way the SM83 does, so the machine is advanced here. A minimum of
            // one keeps a core that reports zero from stalling the frame loop forever.
            let cycles = self.cpu.step(&mut self.bus).get().max(1);
            self.bus.advance(cycles as u32);
            elapsed += cycles;
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
        self.timers.save(w);
        self.dma.save(w);
        self.irq.save(w);
        self.sound.save(w);
        self.waits.save(w);
        w.write_u64(self.sample_accumulator);
    }

    fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        self.memory.load(r)?;
        self.cartridge.load(r)?;
        self.video.load(r)?;
        self.backgrounds.load(r)?;
        self.timers.load(r)?;
        self.dma.load(r)?;
        self.irq.load(r)?;
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
