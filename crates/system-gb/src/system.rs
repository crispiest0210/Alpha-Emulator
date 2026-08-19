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

use cart_common::{create_mapper, GbHeader, RTC_TRAILER_LEN};
use core_common::{
    compose_le_read16, compose_le_read32, compose_le_write16, compose_le_write32, AccessKind,
    AccessLog, AudioSample, Bus, CartridgeError, Cpu, Cycles, FrameOutput, Framebuffer, InputState,
    Savable, StateError, StateReader, StateWriter, System,
};
use cpu_sm83::Sm83;

use crate::apu::GbApu;
use crate::cgb::CgbState;
use crate::joypad::{Joypad, JOYP};
use crate::memory::{io, GbBus, GbModel};
use crate::ppu::{self, GbPpu};
use crate::timing::{interrupt, reg as timing_reg, GbTiming, TimingOutput, CLOCK_HZ};

/// Bumped on any change to what this system serializes, including in a subsystem it owns.
const STATE_VERSION: u32 = 2;

/// Everything the CPU can reach, plus the subsystems that run on their own schedule.
pub struct GbSystemBus {
    pub memory: GbBus,
    pub timing: GbTiming,
    pub ppu: GbPpu,
    pub apu: GbApu,
    pub joypad: Joypad,
    /// The CGB-only register blocks. Inert on a DMG, where nothing routes to them.
    pub cgb: CgbState,

    /// The debugger's access recorder. Records nothing until a watchpoint arms it.
    ///
    /// On the bus rather than beside the CPU because the bus is the only thing that sees every
    /// access. Kept out of save states: it is a debugging aid, not machine state, and a state file
    /// carrying one would restore somebody else's watchpoints.
    pub watch: AccessLog,

    /// Bytes the game has pushed out of the serial port.
    ///
    /// With no link cable attached this goes nowhere on hardware, but it is exactly how
    /// Blargg's test ROMs report their results, so the harness reads it. Kept out of save
    /// states: it is captured output, not machine state.
    pub serial_output: Vec<u8>,

    /// Cycles owed to the machine when a double-speed halving did not divide evenly.
    speed_remainder: u64,

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
            apu: GbApu::for_model(model),
            joypad: Joypad::new(),
            cgb: CgbState::new(),
            watch: AccessLog::new(),
            serial_output: Vec::new(),
            speed_remainder: 0,
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

    /// Run a general-purpose VRAM DMA to completion.
    ///
    /// The CPU is stopped for the duration on hardware, which is why a game only starts one of
    /// these during vertical blank. Copying it all at once is the same trade the OAM DMA above
    /// makes: the bytes that land are identical, and only code that reads VRAM mid-transfer
    /// could tell — and that code cannot run, because the CPU is stopped.
    fn vram_dma_all(&mut self) {
        while let Some(block) = self.cgb.hdma.take_block() {
            self.copy_dma_block(block);
        }
    }

    /// Move one 16-byte block, then charge the time it took.
    fn copy_dma_block(&mut self, block: crate::cgb::Block) {
        for offset in 0..block.length {
            let byte = self.read8((block.source + offset) as u32);
            self.write8((block.destination + offset) as u32, byte);
        }
        // Eight machine cycles per block at single speed. It is charged rather than ignored
        // because a game that streams a tile set during HBlank is budgeting against it: an
        // instant transfer would let it fit work in the line that hardware would not.
        self.advance(Cycles(32));
    }

    /// Called when the PPU enters horizontal blank, where a streaming transfer moves one block.
    fn vram_dma_hblank(&mut self) {
        if !self.memory.model.has_cgb_hardware() || !self.cgb.hdma.is_hblank_pending() {
            return;
        }
        if let Some(block) = self.cgb.hdma.take_block() {
            self.copy_dma_block(block);
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
            let model = self.memory.model;
            if model.uses_colour_palettes() {
                let palettes = self.cgb.palettes.clone();
                self.ppu
                    .render_scanline_with(model, line, vram, oam, &palettes);
            } else {
                self.ppu.render_scanline(line, vram, oam);
            }
            // A line finishing its drawing period *is* the entry into horizontal blank, which
            // is the moment a streaming VRAM transfer moves its next block.
            self.vram_dma_hblank();
        }
        out
    }
}

impl Bus for GbSystemBus {
    /// Every CPU read funnels through here, which is why the debugger's recorder sits here.
    ///
    /// The routing lives in `read_routed` and the recording here, rather than a
    /// `record` call inside each of the eight arms it dispatches to — one of those would eventually
    /// be forgotten, and a watchpoint that misses one region is worse than no watchpoint at all.
    fn read8(&mut self, addr: u32) -> u8 {
        let value = self.read_routed(addr);
        self.watch.record(addr, AccessKind::Read, value);
        value
    }

    /// Recorded *before* the write lands.
    ///
    /// So the log holds the value the CPU asked to store rather than whatever a register decided to
    /// keep. A watchpoint answers "what did the program try to do here", and for a register that
    /// ignores half its bits those are different answers.
    fn write8(&mut self, addr: u32, value: u8) {
        self.watch.record(addr, AccessKind::Write, value);
        self.write_routed(addr, value);
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
        // In double-speed mode the CPU runs twice as fast and *nothing else does*. So the
        // machine advances half as far per CPU cycle, rather than every scheduled interval
        // being halved — model it the other way round and the timer, the frame sequencer, and
        // the PPU all double their rates too, which is the bug this arrangement avoids.
        //
        // Every SM83 access is four t-cycles, so the halving is exact and the remainder below
        // stays at zero on real code paths. It is carried anyway, because silently dropping a
        // cycle would show up as slow clock drift that is very hard to trace back to here.
        let divisor = self.cgb.speed.cpu_multiplier();
        let total = cycles.get() + self.speed_remainder;
        self.speed_remainder = total % divisor;
        let advanced = total / divisor;

        self.advance(Cycles(advanced));
        let out = self.service_events();
        self.pending.frame_ready |= out.frame_ready;
    }

    fn open_bus8(&self, addr: u32) -> u8 {
        self.memory.open_bus8(addr)
    }

    /// Composed from [`Bus::read8`]/[`Bus::write8`] above rather than from `self.memory`
    /// directly, so a wide access still records one watchpoint entry per byte — exactly what a
    /// real byte-oriented bus does, and what the watch log needs to stay accurate.
    fn read16(&mut self, addr: u32) -> u16 {
        compose_le_read16(self, addr)
    }
    fn read32(&mut self, addr: u32) -> u32 {
        compose_le_read32(self, addr)
    }
    fn write16(&mut self, addr: u32, value: u16) {
        compose_le_write16(self, addr, value)
    }
    fn write32(&mut self, addr: u32, value: u32) {
        compose_le_write32(self, addr, value)
    }

    fn peek8(&self, addr: u32) -> Option<u8> {
        self.memory.peek8(addr)
    }
}

/// The address routing, split out so [`Bus::read8`] and [`Bus::write8`] can record every
/// access in one place each.
impl GbSystemBus {
    fn read_routed(&mut self, addr: u32) -> u8 {
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
            _ if self.memory.model.has_cgb_hardware() && CgbState::owns(addr16) => self
                .cgb
                .read_register(addr16)
                .unwrap_or_else(|| self.memory.read8(addr)),
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

    fn write_routed(&mut self, addr: u32, value: u8) {
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
            _ if self.memory.model.has_cgb_hardware() && CgbState::owns(addr16) => {
                if self.cgb.write_register(addr16, value) {
                    self.vram_dma_all();
                }
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
}

impl Savable for GbSystemBus {
    fn save(&self, w: &mut StateWriter) {
        self.memory.save(w);
        self.timing.save(w);
        self.ppu.save(w);
        self.apu.save(w);
        self.joypad.save(w);
        // Written unconditionally, including on a DMG where it is inert. Making the payload
        // depend on the model would mean a state file whose shape you cannot know until you
        // have already read part of it.
        self.cgb.save(w);
        w.write_u64(self.speed_remainder);
    }
    fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        self.memory.load(r)?;
        self.timing.load(r)?;
        self.ppu.load(r)?;
        self.apu.load(r)?;
        self.joypad.load(r)?;
        self.cgb.load(r)?;
        self.speed_remainder = r.read_u64()?;
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
        Self::with_model(rom, boot_rom, GbModel::Dmg)
    }

    /// Build the machine for a specific model.
    ///
    /// `system-gbc` calls this with [`GbModel::for_cartridge`] rather than assembling a second
    /// system: a Game Boy Color is this machine with more banks, a second palette path, and a
    /// faster clock, and every one of those is already a branch inside these components.
    pub fn with_model(
        rom: Vec<u8>,
        boot_rom: Option<Vec<u8>>,
        model: GbModel,
    ) -> Result<Self, CartridgeError> {
        let header = GbHeader::parse(&rom)?;
        let mapper = create_mapper(rom, &header)?;

        let mut system = Self {
            cpu: Sm83::new(),
            bus: GbSystemBus::new(model, mapper),
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

    /// Mutable access to the core, for the debugger's "set next statement".
    ///
    /// The counterpart to `bus_mut`, and as narrow: it exists so `DebugTarget` can move the program
    /// counter, which is the only write a debugger makes into the CPU.
    pub fn cpu_mut(&mut self) -> &mut Sm83 {
        &mut self.cpu
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

impl GbSystem {
    /// Turn a `STOP` into a speed switch when one is armed.
    ///
    /// `STOP` means two unrelated things on a CGB and the machine tells them apart by whether
    /// `KEY1` bit 0 was set beforehand: armed, it changes the CPU clock and execution resumes;
    /// unarmed, it is the DMG's low-power mode and the CPU waits for a joypad line. The CPU
    /// core cannot make that distinction — `KEY1` is not its register — so it always stops and
    /// the machine decides here what the stop meant.
    fn resolve_stop(&mut self) {
        if !self.cpu.is_stopped() || !self.bus.memory.model.has_cgb_hardware() {
            return;
        }
        if let Some(stall) = self.bus.cgb.speed.switch() {
            // The CPU is held while the clock relocks. Games enter the switch with interrupts
            // disabled and time the gap, so it has to be charged rather than skipped.
            self.bus.advance(Cycles(stall));
            self.cpu.clear_stop();
        }
    }
}

impl System for GbSystem {
    /// Run exactly one instruction and report the cycles it took.
    ///
    /// What the debugger single-steps with, and what the session checks execution breakpoints
    /// between — which is how breakpoints work without this crate knowing that breakpoints exist.
    fn step_instruction(&mut self) -> Cycles {
        let before = self.bus.timing.now();
        self.cpu.step(&mut self.bus);
        self.resolve_stop();
        self.bus.timing.now() - before
    }

    fn debug(&mut self) -> Option<&mut dyn core_common::DebugTarget> {
        Some(self)
    }

    fn access_log(&mut self) -> Option<&mut core_common::AccessLog> {
        Some(&mut self.bus.watch)
    }

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
    fn set_input(&mut self, input: InputState) {
        if self.bus.joypad.set_input(input) {
            self.bus.memory.interrupt_flags |= interrupt::JOYPAD;
            // A joypad line going low is also what releases `STOP`. That happens whether or not
            // the joypad interrupt is enabled — low-power mode ends because the hardware line
            // itself went low, not because an interrupt got serviced.
            self.cpu.clear_stop();
        }
    }

    fn step_frame(&mut self, input: InputState) -> FrameOutput {
        self.set_input(input);

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
            self.resolve_stop();
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

    /// Appends the MBC3 RTC trailer (see [`cart_common::RTC_TRAILER_LEN`]) when the cartridge
    /// has both battery-backed RAM and a clock. RTC-without-RAM is real cartridge-type space
    /// (`0x0F`) but no known game ships that way — every RTC title needs somewhere to keep its
    /// save data too — so gating on RAM being present keeps this exactly as permissive as
    /// [`Self::save_ram`] already is about which cartridges get a file at all.
    ///
    /// `SystemTime::now()` is read here, in the one place a `.sav` file is actually written,
    /// not inside `cart_common::rtc` — that module stays free of the host clock so the clock's
    /// own tests stay deterministic, per its module docs. Falls back to a zero timestamp if the
    /// host clock is unavailable or reads before the epoch; a zero is indistinguishable from
    /// "no catch-up information available" to any reader of the trailer.
    fn save_ram_for_disk(&self) -> Option<Vec<u8>> {
        let save = self.bus.memory.mapper.battery_save()?;
        let mut bytes = save.as_bytes().to_vec();
        if let Some(rtc) = self.bus.memory.mapper.rtc() {
            let unix_time = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            bytes.extend_from_slice(&rtc.to_trailer_bytes(unix_time));
        }
        Some(bytes)
    }

    /// The inverse of [`Self::save_ram_for_disk`].
    ///
    /// A file exactly `RTC_TRAILER_LEN` bytes longer than the cartridge's RAM is read as
    /// SRAM-plus-trailer; anything else is passed to the RAM chip whole, which is what makes a
    /// pre-existing trailer-less `.sav` load cleanly — it is simply the RAM-only case, already
    /// handled below — and what makes a genuinely wrong-sized file fail with the RAM chip's own
    /// size-mismatch error rather than a confusing one about the trailer.
    ///
    /// A cartridge with an RTC but no trailer in the file — the legacy case, or a save
    /// transplanted from an emulator that does not write one — gets a freshly reset clock
    /// rather than whatever the cartridge's clock happened to be running before, so "load a
    /// save" always produces the same clock state for the same file.
    fn load_save_ram(&mut self, data: &[u8]) -> Result<(), CartridgeError> {
        let Some(save) = self.bus.memory.mapper.battery_save_mut() else {
            return Err(CartridgeError::NoSaveRam);
        };
        let sram_len = save.size();
        let has_trailer =
            self.bus.memory.mapper.rtc().is_some() && data.len() == sram_len + RTC_TRAILER_LEN;
        let sram_bytes = if has_trailer { &data[..sram_len] } else { data };

        self.bus
            .memory
            .mapper
            .battery_save_mut()
            .expect("checked above")
            .load_from_bytes(sram_bytes)?;

        if let Some(rtc) = self.bus.memory.mapper.rtc_mut() {
            if has_trailer {
                let trailer: [u8; RTC_TRAILER_LEN] = data[sram_len..]
                    .try_into()
                    .expect("length checked by has_trailer above");
                rtc.from_trailer_bytes(&trailer);
            } else {
                *rtc = cart_common::Mbc3Rtc::new();
            }
        }
        Ok(())
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
