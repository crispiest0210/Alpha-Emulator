//! A complete toy machine built only from this crate's traits.
//!
//! This is not a system anyone would want to emulate. It exists to prove that [`Cpu`],
//! [`Bus`], [`Scheduler`], and [`System`] are actually implementable *together* — that the
//! signatures compose into a working frame loop rather than being individually plausible and
//! jointly unusable. Prompts 03–13 build real cores against exactly this shape, so if
//! something here needs a workaround, the trait is wrong and this is the cheapest possible
//! place to find that out.
//!
//! It also serves as the reference for the canonical frame loop: run the CPU in slices
//! bounded by [`Scheduler::cycles_until_next`], drain due events, repeat.

use crate::{
    bus::Addr, AudioSample, Bus, CartridgeError, Cpu, CpuIntrospect, Cycles, FrameOutput,
    Framebuffer, InputState, Ram, RegionMap, RegisterValue, Savable, Scheduler, StateError,
    StateReader, StateWriter, System,
};

const WRAM_BASE: Addr = 0x0000;
const WRAM_LEN: usize = 0x1000;
const SAVE_BASE: Addr = 0x2000;
const SAVE_LEN: usize = 0x20;
const MMIO_BASE: Addr = 0x4000;
const MMIO_LEN: Addr = 0x10;

const SCREEN_W: u32 = 8;
const SCREEN_H: u32 = 8;
const CYCLES_PER_SCANLINE: u64 = 100;
const CYCLES_PER_AUDIO_SAMPLE: u64 = 50;
const FRAME_CYCLES: Cycles = Cycles(CYCLES_PER_SCANLINE * SCREEN_H as u64);

const STATE_VERSION: u32 = 1;

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum MockEvent {
    #[default]
    FrameEnd,
    /// Draw scanline `n`, then reschedule for the next one.
    Scanline(u8),
    /// Emit one audio sample, then reschedule.
    AudioTick,
}

impl Savable for MockEvent {
    fn save(&self, w: &mut StateWriter) {
        match self {
            MockEvent::FrameEnd => w.write_u8(0),
            MockEvent::Scanline(n) => {
                w.write_u8(1);
                w.write_u8(*n);
            }
            MockEvent::AudioTick => w.write_u8(2),
        }
    }
    fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        *self = match r.read_u8()? {
            0 => MockEvent::FrameEnd,
            1 => MockEvent::Scanline(r.read_u8()?),
            2 => MockEvent::AudioTick,
            other => return Err(StateError::Malformed(format!("bad event tag {other}"))),
        };
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Bus
// ---------------------------------------------------------------------------

struct MockBus {
    map: RegionMap<Ram>,
    mmio: [u8; MMIO_LEN as usize],
    /// Stands in for a real system's open-bus source (last prefetched word, etc).
    last_value: u8,
    save_ram_dirty: bool,
    input: InputState,
}

impl MockBus {
    fn new() -> Self {
        let mut map = RegionMap::new();
        map.insert(Ram::new(WRAM_BASE, WRAM_LEN))
            .expect("WRAM must map");
        map.insert(Ram::new(SAVE_BASE, SAVE_LEN))
            .expect("save RAM must map");
        Self {
            map,
            mmio: [0; MMIO_LEN as usize],
            last_value: 0xFF,
            save_ram_dirty: false,
            input: InputState::default(),
        }
    }

    fn is_mmio(addr: Addr) -> bool {
        (MMIO_BASE..MMIO_BASE + MMIO_LEN).contains(&addr)
    }

    fn save_ram(&self) -> &[u8] {
        self.map
            .region_at(SAVE_BASE)
            .expect("save RAM is mapped")
            .as_slice()
    }
}

impl Bus for MockBus {
    fn read8(&mut self, addr: Addr) -> u8 {
        let value = if Self::is_mmio(addr) {
            match addr - MMIO_BASE {
                // Reading the input register is the one place a read has a side effect
                // here, mirroring how real MMIO behaves and keeping `peek8` honest.
                0 => (self.input.buttons.bits() & 0xFF) as u8,
                offset => self.mmio[offset as usize],
            }
        } else {
            match self.map.read8(addr) {
                Some(v) => v,
                None => self.open_bus8(addr),
            }
        };
        self.last_value = value;
        value
    }

    fn write8(&mut self, addr: Addr, value: u8) {
        self.last_value = value;
        if Self::is_mmio(addr) {
            self.mmio[(addr - MMIO_BASE) as usize] = value;
            return;
        }
        if self.map.write8(addr, value) && (SAVE_BASE..SAVE_BASE + SAVE_LEN as Addr).contains(&addr)
        {
            self.save_ram_dirty = true;
        }
    }

    fn open_bus8(&self, _addr: Addr) -> u8 {
        self.last_value
    }

    fn peek8(&self, addr: Addr) -> Option<u8> {
        if Self::is_mmio(addr) {
            // Deliberately unpeekable: reading input latches state, so a debugger must not
            // be able to do it by accident.
            return None;
        }
        self.map.peek8(addr)
    }
}

impl Savable for MockBus {
    fn save(&self, w: &mut StateWriter) {
        self.map.save(w);
        self.mmio.save(w);
        w.write_u8(self.last_value);
        w.write_bool(self.save_ram_dirty);
        self.input.save(w);
    }
    fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        self.map.load(r)?;
        self.mmio.load(r)?;
        self.last_value = r.read_u8()?;
        self.save_ram_dirty = r.read_bool()?;
        self.input.load(r)?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// CPU
// ---------------------------------------------------------------------------

/// A one-accumulator toy core. Enough opcodes to produce varying instruction lengths and
/// cycle counts, which is what makes it a useful test of the scheduler's slicing.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct TinyCpu {
    a: u8,
    pc: u16,
    halted: bool,
}

impl TinyCpu {
    fn fetch8(&mut self, bus: &mut MockBus) -> u8 {
        let v = bus.read8(self.pc as Addr);
        self.pc = self.pc.wrapping_add(1);
        v
    }

    fn fetch16(&mut self, bus: &mut MockBus) -> u16 {
        let lo = self.fetch8(bus);
        let hi = self.fetch8(bus);
        u16::from_le_bytes([lo, hi])
    }
}

impl Cpu<MockBus> for TinyCpu {
    fn step(&mut self, bus: &mut MockBus) -> Cycles {
        if self.halted {
            // A halted CPU still burns time. Returning zero here would hang `step_frame`,
            // which is exactly the trap the `Cpu` docs warn about.
            return Cycles(4);
        }
        match self.fetch8(bus) {
            0x00 => Cycles(4), // NOP
            0x01 => {
                let imm = self.fetch8(bus); // LD A, imm8
                self.a = imm;
                Cycles(8)
            }
            0x02 => {
                let addr = self.fetch16(bus); // ST (addr16), A
                bus.write8(addr as Addr, self.a);
                Cycles(12)
            }
            0x03 => {
                let addr = self.fetch16(bus); // LD A, (addr16)
                self.a = bus.read8(addr as Addr);
                Cycles(12)
            }
            0x04 => {
                self.a = self.a.wrapping_add(1); // INC A
                Cycles(4)
            }
            0x05 => {
                let target = self.fetch16(bus); // JP addr16
                self.pc = target;
                Cycles(16)
            }
            0x76 => {
                self.halted = true; // HALT
                Cycles(4)
            }
            _ => Cycles(4),
        }
    }

    fn reset(&mut self) {
        *self = Self::default();
    }
}

impl CpuIntrospect for TinyCpu {
    fn registers(&self) -> Vec<RegisterValue> {
        vec![
            RegisterValue::new("A", self.a as u64, 8),
            RegisterValue::new("PC", self.pc as u64, 16),
        ]
    }
    fn program_counter(&self) -> u32 {
        self.pc as u32
    }
    fn set_program_counter(&mut self, pc: u32) {
        self.pc = pc as u16;
    }
    fn is_halted(&self) -> bool {
        self.halted
    }
}

impl Savable for TinyCpu {
    fn save(&self, w: &mut StateWriter) {
        w.write_u8(self.a);
        w.write_u16(self.pc);
        w.write_bool(self.halted);
    }
    fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        self.a = r.read_u8()?;
        self.pc = r.read_u16()?;
        self.halted = r.read_bool()?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// System
// ---------------------------------------------------------------------------

struct MockSystem {
    cpu: TinyCpu,
    bus: MockBus,
    scheduler: Scheduler<MockEvent>,
    now: Cycles,
    framebuffer: Framebuffer,
    frame_count: u64,
    /// Accumulated during the frame.
    audio: Vec<AudioSample>,
    /// Handed out by `take_audio_samples`, swapped with `audio` so nothing allocates per
    /// frame — the same trick every real system crate will use.
    audio_out: Vec<AudioSample>,
    rom_loaded: bool,
}

impl MockSystem {
    fn new() -> Self {
        let mut sys = Self {
            cpu: TinyCpu::default(),
            bus: MockBus::new(),
            scheduler: Scheduler::new(),
            now: Cycles::ZERO,
            framebuffer: Framebuffer::new(SCREEN_W, SCREEN_H),
            frame_count: 0,
            audio: Vec::new(),
            audio_out: Vec::new(),
            rom_loaded: false,
        };
        sys.reset();
        sys
    }

    fn schedule_startup_events(&mut self) {
        self.scheduler.clear();
        self.scheduler.schedule(
            self.now + Cycles(CYCLES_PER_SCANLINE),
            MockEvent::Scanline(0),
        );
        self.scheduler.schedule(
            self.now + Cycles(CYCLES_PER_AUDIO_SAMPLE),
            MockEvent::AudioTick,
        );
    }

    /// Handle one event. Returns true when the frame is complete.
    fn handle(&mut self, when: Cycles, event: MockEvent) -> bool {
        match event {
            MockEvent::FrameEnd => return true,
            MockEvent::Scanline(y) => {
                let shade = self.cpu.a.wrapping_add(y);
                let row = self.framebuffer.row_mut(y as u32);
                for px in row.chunks_exact_mut(4) {
                    px.copy_from_slice(&[shade, shade, shade, 0xFF]);
                }
                let next = (y + 1) % SCREEN_H as u8;
                self.scheduler.schedule(
                    when + Cycles(CYCLES_PER_SCANLINE),
                    MockEvent::Scanline(next),
                );
            }
            MockEvent::AudioTick => {
                let level = (self.cpu.a as f32 / 255.0) * 2.0 - 1.0;
                self.audio.push(AudioSample::mono(level));
                self.scheduler
                    .schedule(when + Cycles(CYCLES_PER_AUDIO_SAMPLE), MockEvent::AudioTick);
            }
        }
        false
    }
}

impl System for MockSystem {
    fn id(&self) -> &'static str {
        "mock"
    }

    fn display_name(&self) -> &'static str {
        "Mock System"
    }

    fn state_version(&self) -> u32 {
        STATE_VERSION
    }

    fn reset(&mut self) {
        self.cpu.reset();
        self.bus = MockBus::new();
        self.now = Cycles::ZERO;
        self.frame_count = 0;
        self.framebuffer.fill(crate::Rgba8::BLACK);
        self.audio.clear();
        self.audio_out.clear();
        self.schedule_startup_events();
    }

    /// The canonical frame loop. Every real system crate follows this shape.
    /// One instruction, with the scheduler drained first so events land at the right cycle.
    ///
    /// The mock exists to prove the trait is implementable without a real machine, so this is here
    /// for the same reason every other method is: a `System` that cannot single-step is not a
    /// `System`, and a mock that skipped the method would let a change to its contract go unchecked.
    fn step_instruction(&mut self) -> Cycles {
        let start = self.now;
        while let Some((when, event)) = self.scheduler.pop_due(self.now) {
            self.handle(when, event);
        }
        let cost = self.cpu.step(&mut self.bus);
        self.now += cost;
        self.now - start
    }

    fn set_input(&mut self, input: InputState) {
        self.bus.input = input;
    }

    fn step_frame(&mut self, input: InputState) -> FrameOutput {
        self.set_input(input);
        self.bus.save_ram_dirty = false;
        let start = self.now;

        self.scheduler
            .schedule(self.now + FRAME_CYCLES, MockEvent::FrameEnd);

        let mut frame_done = false;
        while !frame_done {
            // Drain everything already due, including events that handlers schedule for the
            // current cycle.
            while let Some((when, event)) = self.scheduler.pop_due(self.now) {
                frame_done |= self.handle(when, event);
            }
            if frame_done {
                break;
            }

            // Run the CPU up to the next event. Instructions do not divide evenly into the
            // slice, so the last one overshoots — events then fire a few cycles late, which
            // is the same instruction-granularity approximation every interpreter makes.
            let slice = self
                .scheduler
                .cycles_until_next(self.now)
                .unwrap_or(FRAME_CYCLES);
            let target = self.now + slice;
            while self.now < target {
                self.now += self.cpu.step(&mut self.bus);
            }
        }

        self.frame_count += 1;
        FrameOutput {
            cycles_elapsed: self.now - start,
            save_ram_dirty: self.bus.save_ram_dirty,
            stopped: false,
        }
    }

    fn load_cartridge(&mut self, rom: &[u8]) -> Result<(), CartridgeError> {
        if rom.len() < 4 {
            return Err(CartridgeError::TooSmall {
                len: rom.len(),
                min: 4,
            });
        }
        if rom.len() > WRAM_LEN {
            return Err(CartridgeError::BadSize { len: rom.len() });
        }
        self.reset();
        let wram = self
            .bus
            .map
            .region_at_mut(WRAM_BASE)
            .expect("WRAM is mapped");
        wram.as_mut_slice()[..rom.len()].copy_from_slice(rom);
        self.rom_loaded = true;
        Ok(())
    }

    fn framebuffer(&self) -> &Framebuffer {
        &self.framebuffer
    }

    fn take_audio_samples(&mut self) -> &[AudioSample] {
        std::mem::swap(&mut self.audio, &mut self.audio_out);
        self.audio.clear();
        &self.audio_out
    }

    fn save_ram(&self) -> Option<&[u8]> {
        self.rom_loaded.then(|| self.bus.save_ram())
    }

    fn load_save_ram(&mut self, data: &[u8]) -> Result<(), CartridgeError> {
        if data.len() != SAVE_LEN {
            return Err(CartridgeError::SaveSizeMismatch {
                expected: SAVE_LEN,
                found: data.len(),
            });
        }
        let save = self
            .bus
            .map
            .region_at_mut(SAVE_BASE)
            .expect("save RAM is mapped");
        save.as_mut_slice().copy_from_slice(data);
        Ok(())
    }
}

impl Savable for MockSystem {
    fn save(&self, w: &mut StateWriter) {
        self.cpu.save(w);
        self.bus.save(w);
        self.scheduler.save(w);
        self.now.save(w);
        self.framebuffer.save(w);
        w.write_u64(self.frame_count);
        w.write_bool(self.rom_loaded);
        // `audio`/`audio_out` are deliberately not saved: they are output staging buffers
        // drained every frame by the frontend, not emulated state. Saving them would
        // duplicate samples on load.
    }

    fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        self.cpu.load(r)?;
        self.bus.load(r)?;
        self.scheduler.load(r)?;
        self.now.load(r)?;
        self.framebuffer.load(r)?;
        self.frame_count = r.read_u64()?;
        self.rom_loaded = r.read_bool()?;
        self.audio.clear();
        self.audio_out.clear();
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Buttons;

    /// A program that reads the input register, stores it to save RAM, increments the
    /// accumulator forever, and loops. Exercises MMIO reads, save-RAM writes, and a jump.
    fn program() -> Vec<u8> {
        vec![
            0x03, 0x00, 0x40, // LD A, (0x4000)   ; input register
            0x02, 0x00, 0x20, // ST (0x2000), A   ; save RAM
            0x04, // INC A
            0x05, 0x00, 0x00, // JP 0x0000
        ]
    }

    fn booted() -> MockSystem {
        let mut sys = MockSystem::new();
        sys.load_cartridge(&program()).unwrap();
        sys
    }

    #[test]
    fn traits_compose_into_a_working_frame_loop() {
        let mut sys = booted();
        let out = sys.step_frame(InputState::default());

        // A frame takes at least its nominal length; the final instruction may overshoot.
        assert!(out.cycles_elapsed >= FRAME_CYCLES);
        assert!(out.cycles_elapsed < FRAME_CYCLES + Cycles(32));
        assert!(!out.stopped);
        assert_eq!(sys.framebuffer().width(), SCREEN_W);
    }

    #[test]
    fn scheduled_events_actually_render_and_produce_audio() {
        let mut sys = booted();
        sys.step_frame(InputState::default());

        // Eight scanline events fired, so no row is still the initial black.
        let mut any_lit = false;
        for y in 0..SCREEN_H {
            if sys.framebuffer().pixel(0, y) != crate::Rgba8::BLACK {
                any_lit = true;
            }
        }
        assert!(any_lit, "scanline events should have drawn something");

        // One audio sample per CYCLES_PER_AUDIO_SAMPLE across the frame.
        let expected = (FRAME_CYCLES.get() / CYCLES_PER_AUDIO_SAMPLE) as usize;
        let produced = sys.take_audio_samples().len();
        assert!(
            produced.abs_diff(expected) <= 1,
            "expected ~{expected} samples, got {produced}"
        );
    }

    #[test]
    fn audio_is_drained_exactly_once() {
        let mut sys = booted();
        sys.step_frame(InputState::default());
        assert!(!sys.take_audio_samples().is_empty());
        // Second call with no frame in between yields nothing rather than repeating.
        assert!(sys.take_audio_samples().is_empty());
    }

    #[test]
    fn input_reaches_the_emulated_program_through_mmio() {
        let mut sys = booted();
        sys.step_frame(InputState {
            buttons: Buttons::A | Buttons::START,
            touch: None,
        });
        let expected = (Buttons::A | Buttons::START).bits() as u8;
        // The program stores the input byte to save RAM every iteration.
        assert_eq!(sys.save_ram().unwrap()[0], expected);
    }

    #[test]
    fn writing_save_ram_is_reported_to_the_frontend() {
        let mut sys = booted();
        let out = sys.step_frame(InputState::default());
        assert!(out.save_ram_dirty);

        // A program that never touches save RAM must not report a dirty save, or the
        // frontend would rewrite the save file every frame forever.
        let mut idle = MockSystem::new();
        idle.load_cartridge(&[0x00, 0x00, 0x05, 0x00]).unwrap();
        assert!(!idle.step_frame(InputState::default()).save_ram_dirty);
    }

    #[test]
    fn save_state_round_trip_is_frame_exact() {
        let mut sys = booted();
        for _ in 0..3 {
            sys.step_frame(InputState::default());
        }
        let state = sys.save_state();

        // Reference: keep running from here.
        for _ in 0..2 {
            sys.step_frame(InputState::default());
        }
        let reference_fb = sys.framebuffer().clone();
        let reference_cpu = sys.cpu.clone();
        let reference_now = sys.now;

        // Divergent path, then load and replay the same two frames.
        for _ in 0..5 {
            sys.step_frame(InputState {
                buttons: Buttons::B,
                touch: None,
            });
        }
        sys.load_state(&state).unwrap();
        for _ in 0..2 {
            sys.step_frame(InputState::default());
        }

        assert_eq!(sys.cpu, reference_cpu);
        assert_eq!(sys.now, reference_now);
        assert_eq!(sys.framebuffer(), &reference_fb);
    }

    #[test]
    fn save_state_rejects_a_state_from_another_system() {
        let mut sys = booted();
        let mut state = sys.save_state();
        // Rewrite the embedded system id from "mock" to "mocX".
        let idx = state
            .windows(4)
            .position(|w| w == b"mock")
            .expect("system id is in the header");
        state[idx + 3] = b'X';

        assert!(matches!(
            sys.load_state(&state),
            Err(StateError::WrongSystem { .. })
        ));
    }

    #[test]
    fn save_state_rejects_a_truncated_state() {
        let mut sys = booted();
        let state = sys.save_state();
        assert!(sys.load_state(&state[..state.len() / 2]).is_err());
    }

    #[test]
    fn cartridge_errors_are_reported_rather_than_panicking() {
        let mut sys = MockSystem::new();
        assert!(matches!(
            sys.load_cartridge(&[0x00]),
            Err(CartridgeError::TooSmall { .. })
        ));
        assert!(matches!(
            sys.load_cartridge(&vec![0u8; WRAM_LEN + 1]),
            Err(CartridgeError::BadSize { .. })
        ));
        assert!(matches!(
            sys.load_save_ram(&[0u8; 3]),
            Err(CartridgeError::SaveSizeMismatch { .. })
        ));
    }

    #[test]
    fn stepping_a_system_with_no_cartridge_returns_a_blank_frame() {
        let mut sys = MockSystem::new();
        let out = sys.step_frame(InputState::default());
        assert!(out.cycles_elapsed >= FRAME_CYCLES);
        assert!(sys.save_ram().is_none());
    }

    #[test]
    fn reset_returns_the_machine_to_power_on() {
        let mut sys = booted();
        for _ in 0..4 {
            sys.step_frame(InputState::default());
        }
        assert!(sys.now > Cycles::ZERO);

        sys.reset();
        assert_eq!(sys.now, Cycles::ZERO);
        assert_eq!(sys.frame_count, 0);
        assert_eq!(sys.cpu, TinyCpu::default());
        assert_eq!(sys.framebuffer().pixel(0, 0), crate::Rgba8::BLACK);
    }

    #[test]
    fn a_halted_cpu_does_not_hang_the_frame_loop() {
        let mut sys = MockSystem::new();
        sys.load_cartridge(&[0x76, 0x00, 0x00, 0x00]).unwrap();
        let out = sys.step_frame(InputState::default());
        assert!(sys.cpu.is_halted());
        assert!(out.cycles_elapsed >= FRAME_CYCLES);
    }

    #[test]
    fn debugger_can_inspect_the_cpu_without_running_it() {
        let mut sys = booted();
        sys.step_frame(InputState::default());

        let regs = sys.cpu.registers();
        assert_eq!(regs[0].name, "A");
        assert_eq!(regs[1].name, "PC");
        assert_eq!(sys.cpu.program_counter(), sys.cpu.pc as u32);

        // Peeking RAM is safe; peeking MMIO is refused rather than triggering the read's
        // side effect.
        assert!(sys.bus.peek8(WRAM_BASE).is_some());
        assert_eq!(sys.bus.peek8(MMIO_BASE), None);
    }
}
