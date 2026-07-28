//! The assembled Game Boy: CPU, memory, timing, PPU, APU, and joypad as one [`System`].
//!
//! This is where the abstractions from every earlier layer either hold together or do not.
//! Two of them needed a small addition once real integration pressure arrived, and both were
//! fixed at the source rather than worked around here: the timing module now reports which
//! scanline finished drawing and when `LY` wraps, because "when is a line done" is a timing
//! question and this module should not have to infer it.
//!
//! # The bus is the routing layer
//!
//! [`GbSystemBus`] owns the memory map *and* the timed subsystems, and implements [`Bus`] by
//! dispatching each MMIO address to whichever one owns it. The CPU then sees a single bus and
//! knows nothing about any of them.
//!
//! Two addresses go to more than one owner, deliberately:
//!
//! - `LCDC` reaches the PPU (for the layer-enable bits) and the timing module (for the
//!   LCD-enable bit). Neither stores the other's fields; the write is duplicated, not the
//!   state.
//! - `NR52` and the APU registers are the APU's alone, but the timing module drives its frame
//!   sequencer, so the two meet through [`TimingOutput`] rather than by sharing a field.
//!
//! # Booting without a boot ROM
//!
//! A real boot ROM is supported but not required, and none is vendored here — the licensing
//! situation makes shipping one a bad idea, and the predecessor project committed a BIOS image
//! whose status it simply assumed. Without one, [`GbSystem::new`] jumps straight to the
//! documented post-boot register state, so the emulator is usable out of the box.

use cart_common::{create_mapper, GbHeader};
use core_common::{
    AudioSample, Bus, CartridgeError, Cpu, Cycles, FrameOutput, Framebuffer, InputState, Savable,
    StateError, StateReader, StateWriter, System,
};
use cpu_sm83::Sm83;

use crate::apu::GbApu;
use crate::joypad::{Joypad, JOYP};
use crate::memory::{io, GbBus, GbModel};
use crate::ppu::{self, GbPpu};
use crate::timing::{interrupt, reg as timing_reg, GbTiming, TimingOutput, CLOCK_HZ};

/// Bumped on any change to what this system serializes, including in a subsystem it owns.
const STATE_VERSION: u32 = 1;

/// Everything the CPU can reach, plus the subsystems that run on their own schedule.
pub struct GbSystemBus {
    pub memory: GbBus,
    pub timing: GbTiming,
    pub ppu: GbPpu,
    pub apu: GbApu,
    pub joypad: Joypad,

    /// Bytes the game has pushed out of the serial port.
    ///
    /// With no link cable attached this goes nowhere on hardware, but it is exactly how
    /// Blargg's test ROMs report their results, so the harness reads it. Kept out of save
    /// states: it is captured output, not machine state.
    pub serial_output: Vec<u8>,

    /// Event results accumulated since the frame loop last collected them.
    ///
    /// Events now fire *during* an instruction, so a flag like `frame_ready` would be lost if
    /// the loop only looked between instructions. It accumulates here instead.
    pending: TimingOutput,
}

/// Serial data and control.
const SERIAL_DATA: u16 = 0xFF01;
const SERIAL_CONTROL: u16 = 0xFF02;

/// `0xFF46`: writing a page number copies 160 bytes into OAM.
const OAM_DMA: u16 = 0xFF46;
const OAM_BYTES: u16 = 0xA0;

impl GbSystemBus {
    fn new(model: GbModel, mapper: Box<dyn cart_common::Mapper>) -> Self {
        Self {
            memory: GbBus::new(model, mapper),
            timing: GbTiming::new(),
            ppu: GbPpu::new(),
            apu: GbApu::new(),
            joypad: Joypad::new(),
            serial_output: Vec::new(),
            pending: TimingOutput::default(),
        }
    }

    /// Copy 160 bytes into OAM.
    ///
    /// On hardware this takes 640 cycles, during which the CPU can only reach HRAM — which is
    /// why games jump to a routine there and spin. The copy is performed at once here: the
    /// bytes that land are identical, and the only observable difference is for code that
    /// reads OAM *during* the transfer, which is undefined behavior no game relies on.
    fn oam_dma(&mut self, page: u8) {
        let source = (page as u32) << 8;
        for offset in 0..OAM_BYTES {
            let byte = self.read8(source + offset as u32);
            self.memory.oam_mut()[offset as usize] = byte;
        }
    }

    /// Advance everything that runs on the clock.
    pub fn advance(&mut self, cycles: Cycles) {
        let now = self.timing.now() + cycles;
        self.timing.set_now(now);
        self.apu.tick(cycles.get());
        // The cartridge may hold an RTC, which counts emulated seconds rather than host ones.
        self.memory.mapper.tick(cycles.get(), CLOCK_HZ);
    }

    /// Collect what the events have reported since the last call.
    fn take_pending(&mut self) -> TimingOutput {
        std::mem::take(&mut self.pending)
    }

    /// Fire every due event and apply its consequences.
    fn service_events(&mut self) -> TimingOutput {
        let out = self.timing.advance_to(self.timing.now());
        self.memory.interrupt_flags |= out.interrupts;
        self.apu.apply_sequencer(&out);

        if out.frame_started || self.timing.take_lcd_restarted() {
            self.ppu.begin_frame();
        }
        if let Some(line) = out.scanline_ready {
            // Composite with the registers as they are *now*, which is what makes a mid-frame
            // scroll change split the raster.
            let (vram, oam) = self.memory.vram_and_oam();
            self.ppu.render_scanline(line, vram, oam);
        }
        out
    }
}

impl Bus for GbSystemBus {
    fn read8(&mut self, addr: u32) -> u8 {
        let addr16 = addr as u16;
        match addr16 {
            JOYP => self.joypad.read(),
            timing_reg::DIV..=timing_reg::TAC => self
                .timing
                .read_register(addr16)
                .unwrap_or_else(|| self.memory.read8(addr)),
            _ if GbApu::owns(addr16) => self
                .apu
                .read_register(addr16)
                .unwrap_or_else(|| self.memory.read8(addr)),
            OAM_DMA => self.memory.read8(addr),
            timing_reg::STAT | timing_reg::LY | timing_reg::LYC => self
                .timing
                .read_register(addr16)
                .unwrap_or_else(|| self.memory.read8(addr)),
            _ => match self.ppu.read_register(addr16) {
                Some(value) => value,
                None => self.memory.read8(addr),
            },
        }
    }

    fn write8(&mut self, addr: u32, value: u8) {
        let addr16 = addr as u16;
        match addr16 {
            JOYP => self.joypad.write(value),
            SERIAL_CONTROL => {
                self.memory.write8(addr, value);
                // Bit 7 starts a transfer. Nothing is listening, so it completes at once —
                // which is what the test ROMs that use this as an output channel expect.
                if value & 0x80 != 0 {
                    let byte = self.memory.read8(SERIAL_DATA as u32);
                    self.serial_output.push(byte);
                    self.memory.write8(addr, value & 0x7F);
                    self.memory.interrupt_flags |= interrupt::SERIAL;
                }
            }
            timing_reg::DIV..=timing_reg::TAC => {
                if let Some(interrupts) = self.timing.write_register(addr16, value) {
                    self.memory.interrupt_flags |= interrupts;
                }
            }
            _ if GbApu::owns(addr16) => {
                let was_powered = self.apu.is_powered();
                self.apu
                    .write_register(addr16, value, self.timing.sequencer_step());
                if !was_powered && self.apu.is_powered() {
                    self.timing.reset_sequencer();
                }
            }
            OAM_DMA => {
                self.memory.write8(addr, value);
                self.oam_dma(value);
            }
            // The one address with two owners: the PPU takes the layer bits and the timing
            // state machine takes the LCD-enable bit.
            timing_reg::LCDC => {
                self.ppu.write_register(addr16, value);
                if let Some(interrupts) = self.timing.write_register(addr16, value) {
                    self.memory.interrupt_flags |= interrupts;
                }
            }
            timing_reg::STAT | timing_reg::LY | timing_reg::LYC => {
                if let Some(interrupts) = self.timing.write_register(addr16, value) {
                    self.memory.interrupt_flags |= interrupts;
                }
            }
            _ => {
                if self.ppu.write_register(addr16, value).is_none() {
                    self.memory.write8(addr, value);
                }
            }
        }
    }

    /// The CPU reports each machine cycle here as it happens.
    ///
    /// Advancing the clock is only half of it: the due events have to *fire* too. A timer
    /// that overflows during an instruction must have overflowed by the time that same
    /// instruction reads `TIMA` a cycle later, and moving the clock without draining the
    /// scheduler would leave the read seeing a stale value — which is worse than the lump-sum
    /// accounting this replaced, not better.
    ///
    /// Charging an instruction's cost at its end is what Blargg's `mem_timing` suite detects;
    /// draining here is what makes the fix real rather than cosmetic.
    fn tick(&mut self, cycles: Cycles) {
        self.advance(cycles);
        let out = self.service_events();
        self.pending.frame_ready |= out.frame_ready;
    }

    fn open_bus8(&self, addr: u32) -> u8 {
        self.memory.open_bus8(addr)
    }

    fn peek8(&self, addr: u32) -> Option<u8> {
        self.memory.peek8(addr)
    }
}

impl Savable for GbSystemBus {
    fn save(&self, w: &mut StateWriter) {
        self.memory.save(w);
        self.timing.save(w);
        self.ppu.save(w);
        self.apu.save(w);
        self.joypad.save(w);
    }
    fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        self.memory.load(r)?;
        self.timing.load(r)?;
        self.ppu.load(r)?;
        self.apu.load(r)?;
        self.joypad.load(r)?;
        Ok(())
    }
}

/// A Game Boy.
pub struct GbSystem {
    cpu: Sm83,
    bus: GbSystemBus,
    /// Kept so `reset` can rebuild the machine without the caller re-supplying it.
    boot_rom: Option<Vec<u8>>,
    save_ram_dirty: bool,
}

impl GbSystem {
    /// Build a machine around a cartridge.
    ///
    /// With no boot ROM the CPU and hardware registers jump to their documented post-boot
    /// state, so a ROM runs immediately. With one, execution starts at `0x0000` and the ROM
    /// scrolls the logo and hands over exactly as hardware does.
    pub fn new(rom: Vec<u8>, boot_rom: Option<Vec<u8>>) -> Result<Self, CartridgeError> {
        let header = GbHeader::parse(&rom)?;
        let mapper = create_mapper(rom, &header)?;

        let mut system = Self {
            cpu: Sm83::new(),
            bus: GbSystemBus::new(GbModel::Dmg, mapper),
            boot_rom,
            save_ram_dirty: false,
        };
        system.apply_startup_state();
        Ok(system)
    }

    /// A machine with no cartridge, for a frontend that has not been given one yet.
    pub fn empty() -> Self {
        // A minimal valid ROM so the mapper machinery has something coherent to hold.
        let mut rom = vec![0u8; 0x8000];
        rom[0x0147] = 0x00;
        rom[0x014D] = GbHeader::header_checksum(&rom);
        let header = GbHeader::parse(&rom).expect("the synthetic ROM is valid");
        let mapper = create_mapper(rom, &header).expect("the synthetic ROM is valid");

        let mut system = Self {
            cpu: Sm83::new(),
            bus: GbSystemBus::new(GbModel::Dmg, mapper),
            boot_rom: None,
            save_ram_dirty: false,
        };
        system.apply_startup_state();
        system
    }

    fn apply_startup_state(&mut self) {
        match self.boot_rom.clone() {
            Some(rom) => {
                // The boot ROM sets everything up itself, starting from a true reset.
                self.bus.memory.install_boot_rom(rom);
            }
            None => {
                self.cpu.post_boot_dmg();
                self.apply_post_boot_registers();
            }
        }
    }

    /// The hardware register state a DMG boot ROM leaves behind.
    ///
    /// Community-documented values. Getting these wrong is rarely fatal but produces subtly
    /// different behavior in the first frames — a game that reads `LCDC` before writing it,
    /// for instance, sees a display that was already on.
    fn apply_post_boot_registers(&mut self) {
        for (addr, value) in [
            (JOYP, 0xCFu8),
            (timing_reg::TIMA, 0x00),
            (timing_reg::TMA, 0x00),
            (timing_reg::TAC, 0xF8),
            (io::IF, 0xE1),
            (ppu::reg::LCDC, 0x91),
            (ppu::reg::SCY, 0x00),
            (ppu::reg::SCX, 0x00),
            (timing_reg::LYC, 0x00),
            (ppu::reg::BGP, 0xFC),
            (ppu::reg::OBP0, 0xFF),
            (ppu::reg::OBP1, 0xFF),
            (ppu::reg::WY, 0x00),
            (ppu::reg::WX, 0x00),
            (io::IE, 0x00),
        ] {
            self.bus.write8(addr as u32, value);
        }
    }

    pub fn cpu(&self) -> &Sm83 {
        &self.cpu
    }

    pub fn bus(&self) -> &GbSystemBus {
        &self.bus
    }

    pub fn bus_mut(&mut self) -> &mut GbSystemBus {
        &mut self.bus
    }

    /// The cartridge header's title, for the UI.
    pub fn title(&self) -> String {
        self.bus.memory.mapper.describe()
    }
}

impl System for GbSystem {
    fn id(&self) -> &'static str {
        "gb"
    }

    fn display_name(&self) -> &'static str {
        "Game Boy"
    }

    fn state_version(&self) -> u32 {
        STATE_VERSION
    }

    fn reset(&mut self) {
        Cpu::<GbSystemBus>::reset(&mut self.cpu);
        // In place, so the cartridge stays in the slot and its save RAM survives — resetting
        // a console does not eject the game.
        self.bus.memory.reset();
        self.bus.timing.reset();
        self.bus.ppu.reset();
        self.bus.apu.reset();
        self.bus.joypad = Joypad::new();
        self.bus.serial_output.clear();
        self.save_ram_dirty = false;
        self.apply_startup_state();
    }

    /// Run until the PPU enters vertical blanking, which is when the frame is complete.
    ///
    /// The loop is the pattern from the timing module: run the CPU only as far as the next
    /// scheduled event, then drain everything due. The CPU's last instruction in a slice
    /// overshoots the boundary, which is expected — events fire a few cycles late but stay on
    /// their own grid, because they reschedule from their own timestamps.
    fn step_frame(&mut self, input: InputState) -> FrameOutput {
        if self.bus.joypad.set_input(input) {
            self.bus.memory.interrupt_flags |= interrupt::JOYPAD;
            // A joypad line going low is also what releases `STOP`. That happens whether or not
            // the joypad interrupt is enabled — low-power mode ends because the hardware line
            // itself went low, not because an interrupt got serviced.
            self.cpu.clear_stop();
        }

        let start = self.bus.timing.now();
        self.save_ram_dirty = false;

        // Bounded in case the PPU is switched off and never reaches VBlank: a frontend must
        // always get a frame back rather than hanging.
        let deadline = start + Cycles(crate::timing::FRAME_CYCLES * 2);

        // No slicing. Events fire from inside `Bus::tick` as the CPU runs, so there is nothing
        // to bound the CPU against — running an instruction can no longer overshoot past an
        // event that should have interrupted it.
        loop {
            self.cpu.step(&mut self.bus);
            if self.bus.take_pending().frame_ready || self.bus.timing.now() >= deadline {
                break;
            }
        }

        if let Some(save) = self.bus.memory.mapper.battery_save_mut() {
            self.save_ram_dirty = save.is_dirty();
            save.clear_dirty();
        }

        FrameOutput {
            cycles_elapsed: self.bus.timing.now() - start,
            save_ram_dirty: self.save_ram_dirty,
            stopped: self.cpu.is_stopped(),
        }
    }

    fn load_cartridge(&mut self, rom: &[u8]) -> Result<(), CartridgeError> {
        let header = GbHeader::parse(rom)?;
        let mapper = create_mapper(rom.to_vec(), &header)?;
        let model = self.bus.memory.model;
        Cpu::<GbSystemBus>::reset(&mut self.cpu);
        self.bus = GbSystemBus::new(model, mapper);
        self.apply_startup_state();
        Ok(())
    }

    fn framebuffer(&self) -> &Framebuffer {
        self.bus.ppu.framebuffer()
    }

    fn take_audio_samples(&mut self) -> &[AudioSample] {
        self.bus.apu.take_samples()
    }

    fn save_ram(&self) -> Option<&[u8]> {
        self.bus.memory.mapper.battery_save().map(|s| s.as_bytes())
    }

    fn load_save_ram(&mut self, data: &[u8]) -> Result<(), CartridgeError> {
        match self.bus.memory.mapper.battery_save_mut() {
            Some(save) => save.load_from_bytes(data),
            None => Err(CartridgeError::NoSaveRam),
        }
    }
}

impl Savable for GbSystem {
    fn save(&self, w: &mut StateWriter) {
        self.cpu.save(w);
        self.bus.save(w);
        w.write_bool(self.save_ram_dirty);
        // The boot ROM is a file the user supplied, not machine state, and is not serialized.
    }

    fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        self.cpu.load(r)?;
        self.bus.load(r)?;
        self.save_ram_dirty = r.read_bool()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
