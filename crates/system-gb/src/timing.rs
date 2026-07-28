//! Scheduler wiring: timers, PPU mode transitions, and the APU frame sequencer.
//!
//! This module is the reference implementation of the scheduling pattern that `system-gbc`,
//! `system-gba`, and `system-nds` follow. It owns *when* things happen; what gets drawn or
//! sounded when they do belongs to the PPU and APU.
//!
//! # The frame loop
//!
//! The rule is that **the CPU never runs past the next scheduled event without the scheduler
//! getting a chance to fire it**. The shape:
//!
//! ```ignore
//! fn step_frame(&mut self, input: InputState) -> FrameOutput {
//!     self.bus.set_input(input);
//!     loop {
//!         // Run only as far as the next event, or the end of the frame.
//!         let slice = self.timing.cycles_until_next_event();
//!         let target = self.timing.now() + slice;
//!         while self.timing.now() < target {
//!             // One instruction at a time. The last one overshoots; see below.
//!             let consumed = self.cpu.step(&mut self.bus);
//!             self.timing.set_now(self.timing.now() + consumed);
//!         }
//!
//!         // Fire everything now due, including anything a handler just scheduled.
//!         let out = self.timing.advance_to(self.timing.now());
//!         self.bus.interrupt_flags |= out.interrupts;
//!         self.apu.clock_sequencer(&out);
//!         if out.frame_ready {
//!             break;
//!         }
//!     }
//!     ...
//! }
//! ```
//!
//! ## Instruction overshoot
//!
//! An interpreter executes whole instructions, and instructions do not divide evenly into a
//! slice. The last one in a slice therefore finishes a few cycles past the event boundary, so
//! the event fires slightly late.
//!
//! That is expected and is what every interpreter does; the alternative is sub-instruction
//! preemption, which costs far more than the accuracy it buys at this granularity. The policy
//! here is: **let the instruction finish, then drain every event now in the past, and
//! reschedule from the event's own timestamp rather than from the current cycle.** That last
//! part matters — rescheduling from `now` would let overshoot accumulate, and a PPU line
//! would drift longer than 456 cycles over a frame. Rescheduling from `when` keeps every
//! recurring event on its exact grid no matter how badly an individual instruction overran.
//!
//! # Never poll
//!
//! No subsystem examines another's registers every cycle. If you find yourself writing
//! `if cycles % N == 0`, schedule an event instead. The one apparent exception is `DIV`,
//! which is *derived* from the cycle counter rather than polled — see [`Timer`].
//!
//! # Determinism
//!
//! Nothing in this path may read a wall clock, depend on uninitialized memory, or iterate a
//! hash map. Two runs from the same state with the same input produce identical event traces,
//! which is what makes save states, rewind, and the accuracy harness possible at all.
//! [`GbTiming::trace`] exists to check exactly that.

use core_common::{Cycles, EventHandle, Savable, Scheduler, StateError, StateReader, StateWriter};

/// DMG clock rate, in t-cycles per second.
pub const CLOCK_HZ: u64 = 4_194_304;

/// T-cycles in one scanline, visible or not.
pub const LINE_CYCLES: u64 = 456;
/// Scanlines drawn to the screen.
pub const VISIBLE_LINES: u8 = 144;
/// Total scanlines including the vertical blanking interval.
pub const TOTAL_LINES: u8 = 154;
/// T-cycles in one full frame.
pub const FRAME_CYCLES: u64 = LINE_CYCLES * TOTAL_LINES as u64;

/// Mode 2, scanning OAM for sprites on this line.
pub const OAM_SCAN_CYCLES: u64 = 80;
/// Mode 3, the pixel transfer.
///
/// The real duration stretches from 172 to 289 cycles depending on scroll, window, and sprite
/// count. The baseline is used here because mode 3's *length* is a rendering property: it
/// depends on what the fetcher actually has to do, which prompt 08 owns. That prompt refines
/// this into a computed value; the state machine around it does not change.
pub const DRAWING_CYCLES: u64 = 172;
/// Mode 0, whatever is left of the line.
pub const HBLANK_CYCLES: u64 = LINE_CYCLES - OAM_SCAN_CYCLES - DRAWING_CYCLES;

/// Interrupt bit positions in `IF`/`IE`, in hardware priority order.
pub mod interrupt {
    pub const VBLANK: u8 = 1 << 0;
    pub const LCD_STAT: u8 = 1 << 1;
    pub const TIMER: u8 = 1 << 2;
    pub const SERIAL: u8 = 1 << 3;
    pub const JOYPAD: u8 = 1 << 4;
}

/// Timing register addresses.
pub mod reg {
    pub const DIV: u16 = 0xFF04;
    pub const TIMA: u16 = 0xFF05;
    pub const TMA: u16 = 0xFF06;
    pub const TAC: u16 = 0xFF07;
    pub const LCDC: u16 = 0xFF40;
    pub const STAT: u16 = 0xFF41;
    pub const LY: u16 = 0xFF44;
    pub const LYC: u16 = 0xFF45;
}

/// Events `system-gb` puts on the scheduler.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub enum GbEvent {
    /// The divider bit selected by `TAC` fell, so `TIMA` increments.
    #[default]
    TimerIncrement,
    /// The four-cycle delay after `TIMA` overflowed has elapsed; load `TMA` and interrupt.
    TimerReload,
    /// The PPU leaves its current mode.
    PpuModeChange,
    /// The 512 Hz frame sequencer advances one step.
    ApuSequencerTick,
}

impl Savable for GbEvent {
    fn save(&self, w: &mut StateWriter) {
        w.write_u8(match self {
            GbEvent::TimerIncrement => 0,
            GbEvent::TimerReload => 1,
            GbEvent::PpuModeChange => 2,
            GbEvent::ApuSequencerTick => 3,
        });
    }
    fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        *self = match r.read_u8()? {
            0 => GbEvent::TimerIncrement,
            1 => GbEvent::TimerReload,
            2 => GbEvent::PpuModeChange,
            3 => GbEvent::ApuSequencerTick,
            other => return Err(StateError::Malformed(format!("bad GbEvent tag {other}"))),
        };
        Ok(())
    }
}

/// What the system must act on after draining events.
///
/// Returned by value with no allocation: at most one sequencer step can fall inside a single
/// slice, and interrupts are a bitmask, so there is nothing to collect into a `Vec`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TimingOutput {
    /// Bits to OR into `IF`.
    pub interrupts: u8,
    /// The PPU entered VBlank, so the frame is complete.
    pub frame_ready: bool,
    /// The PPU finished drawing this visible line, which is when it must be composited.
    ///
    /// Reported per line rather than left for the renderer to infer, because "when is a line
    /// done" is a timing question and this module is the one that knows.
    pub scanline_ready: Option<u8>,
    /// `LY` wrapped back to zero, so a new frame's rendering state begins.
    pub frame_started: bool,
    /// Frame-sequencer clocks that fired, as counts rather than flags so a caller that
    /// advances by a long jump still applies each one.
    pub apu_length_clocks: u8,
    pub apu_envelope_clocks: u8,
    pub apu_sweep_clocks: u8,
}

// ---------------------------------------------------------------------------
// The divider
// ---------------------------------------------------------------------------

/// Cycles until bit `bit` of the divider next falls, given its current value.
///
/// A divider bit falls when the counter crosses a multiple of `2^(bit+1)`, so the wait is
/// whatever remains of the current period. Every period divides 65536 evenly, so the
/// counter's own 16-bit wrap never lands mid-period and this stays correct across it.
#[inline]
fn cycles_until_bit_falls(counter: u16, bit: u32) -> u64 {
    let period = 1u64 << (bit + 1);
    period - (counter as u64 % period)
}

#[inline]
fn bit_is_set(counter: u16, bit: u32) -> bool {
    counter & (1 << bit) != 0
}

/// The divider bit that clocks the frame sequencer: 8192 cycles is `2^13`, so bit 12.
const SEQUENCER_BIT: u32 = 12;

// ---------------------------------------------------------------------------
// Timer
// ---------------------------------------------------------------------------

/// `DIV`, `TIMA`, `TMA`, and `TAC`.
///
/// # Everything hangs off one counter
///
/// There is a single 16-bit counter incrementing every t-cycle. `DIV` is simply its top
/// eight bits, and `TIMA` increments on the **falling edge** of one selected bit of it ANDed
/// with the enable bit in `TAC`.
///
/// Modelling it that way rather than as an independent countdown is what produces the
/// documented quirks for free instead of needing special cases:
///
/// - Writing `DIV` zeroes the counter. If the selected bit was set, it has just fallen, so
///   `TIMA` increments — a write to a "reset the divider" register bumps an unrelated timer.
/// - Changing `TAC` can move the multiplexer onto a bit that is currently low, or disable it
///   entirely, either of which is also a falling edge and also increments `TIMA`.
///
/// `DIV` itself is derived from the cycle counter rather than scheduled. That is not polling:
/// nothing is examined per cycle, the value is simply computed when read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Timer {
    /// The cycle at which the internal counter was last zeroed.
    div_base: Cycles,
    pub tima: u8,
    pub tma: u8,
    pub tac: u8,
    /// True for the four cycles between `TIMA` overflowing and `TMA` being loaded, during
    /// which `TIMA` reads as zero.
    reload_pending: bool,
    increment_handle: EventHandle,
    reload_handle: EventHandle,
}

impl Default for Timer {
    fn default() -> Self {
        Self {
            div_base: Cycles::ZERO,
            tima: 0,
            tma: 0,
            tac: 0,
            reload_pending: false,
            increment_handle: EventHandle::NONE,
            reload_handle: EventHandle::NONE,
        }
    }
}

impl Timer {
    /// The divider bit `TAC` currently selects.
    ///
    /// The mapping is deliberately not in increasing order — that is how the hardware
    /// multiplexer is wired.
    #[inline]
    fn selected_bit(&self) -> u32 {
        match self.tac & 0b11 {
            0b00 => 9, // 4096 Hz, every 1024 cycles
            0b01 => 3, // 262144 Hz, every 16
            0b10 => 5, // 65536 Hz, every 64
            _ => 7,    // 16384 Hz, every 256
        }
    }

    #[inline]
    fn enabled(&self) -> bool {
        self.tac & 0b100 != 0
    }

    #[inline]
    fn counter(&self, now: Cycles) -> u16 {
        (now.get().wrapping_sub(self.div_base.get())) as u16
    }

    /// The multiplexer output: the selected bit gated by the enable.
    #[inline]
    fn edge_output(&self, now: Cycles) -> bool {
        self.enabled() && bit_is_set(self.counter(now), self.selected_bit())
    }

    pub fn div(&self, now: Cycles) -> u8 {
        (self.counter(now) >> 8) as u8
    }
}

// ---------------------------------------------------------------------------
// PPU timing
// ---------------------------------------------------------------------------

/// The four PPU modes, encoded as `STAT` reads them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PpuMode {
    /// Mode 0: the rest of the scanline after drawing.
    #[default]
    HBlank = 0,
    /// Mode 1: the ten scanlines after the visible ones.
    VBlank = 1,
    /// Mode 2: scanning OAM for sprites on this line.
    OamScan = 2,
    /// Mode 3: transferring pixels. VRAM and OAM are inaccessible to the CPU.
    Drawing = 3,
}

/// The PPU's mode/`LY`/`STAT` state machine.
///
/// Rendering is prompt 08's job. This owns only the question of which mode the PPU is in at
/// which cycle, because that is what determines when the renderer runs and when the CPU is
/// locked out of VRAM.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PpuTiming {
    pub ly: u8,
    pub lyc: u8,
    pub mode: PpuMode,
    /// `STAT` bits 3-6, the interrupt source selects. The mode and coincidence bits are
    /// derived on read rather than stored, so they cannot drift out of sync.
    stat_selects: u8,
    pub lcd_enabled: bool,
    /// The ORed interrupt condition as of the last update.
    ///
    /// The STAT interrupt fires on this line's **rising edge**, not on each contributing
    /// condition. Two sources active back to back therefore produce one interrupt, not two —
    /// the behavior usually called STAT blocking, and something games rely on.
    stat_line: bool,
    handle: EventHandle,
}

impl PpuTiming {
    #[inline]
    pub fn coincidence(&self) -> bool {
        self.ly == self.lyc
    }

    /// `STAT` as the CPU reads it. Bit 7 is unused and reads as one.
    pub fn read_stat(&self) -> u8 {
        if !self.lcd_enabled {
            // With the LCD off the mode reads as 0 and no coincidence is reported.
            return 0x80 | self.stat_selects;
        }
        0x80 | self.stat_selects | ((self.coincidence() as u8) << 2) | self.mode as u8
    }

    /// Whether any enabled `STAT` source is currently asserting.
    fn stat_condition(&self) -> bool {
        if !self.lcd_enabled {
            return false;
        }
        let by_mode = match self.mode {
            PpuMode::HBlank => self.stat_selects & 0x08 != 0,
            PpuMode::VBlank => self.stat_selects & 0x10 != 0,
            PpuMode::OamScan => self.stat_selects & 0x20 != 0,
            PpuMode::Drawing => false,
        };
        let by_coincidence = self.stat_selects & 0x40 != 0 && self.coincidence();
        by_mode || by_coincidence
    }

    /// Recompute the interrupt line, returning true only on a rising edge.
    fn update_stat_line(&mut self) -> bool {
        let now = self.stat_condition();
        let rising = now && !self.stat_line;
        self.stat_line = now;
        rising
    }
}

// ---------------------------------------------------------------------------
// APU frame sequencer
// ---------------------------------------------------------------------------

/// The 512 Hz sequencer that clocks length counters, the volume envelope, and the sweep unit.
///
/// It is clocked by a divider bit, exactly like `TIMA` — which means writing `DIV` can also
/// step the sequencer, and a game that resets the divider in a tight loop can stall note
/// lengths. Deriving it from the same counter gets that behavior for free.
///
/// Sample *generation* is a separate cadence and a separate concern (prompt 09). This owns
/// only the eight-step sequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ApuSequencer {
    /// Position in the eight-step sequence.
    pub step: u8,
    handle: EventHandle,
}

/// Which units a given sequencer step clocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SequencerStep {
    pub length: bool,
    pub envelope: bool,
    pub sweep: bool,
}

impl ApuSequencer {
    /// The fixed eight-step pattern: length on the even steps, sweep on 2 and 6, envelope on
    /// 7 alone.
    pub const fn clocks_for(step: u8) -> SequencerStep {
        SequencerStep {
            length: step.is_multiple_of(2),
            sweep: step == 2 || step == 6,
            envelope: step == 7,
        }
    }
}

// ---------------------------------------------------------------------------
// The wiring
// ---------------------------------------------------------------------------

/// Owns the scheduler and every timed subsystem, and is the only place that schedules.
///
/// The subsystems above hold state and answer questions; they do not reach for the scheduler
/// themselves. Keeping all the scheduling in one type is what makes the ordering auditable —
/// and it is the shape prompts 12 and 13 should copy, where there are far more event sources.
#[derive(Debug, Clone)]
pub struct GbTiming {
    scheduler: Scheduler<GbEvent>,
    now: Cycles,
    pub timer: Timer,
    pub ppu: PpuTiming,
    pub apu: ApuSequencer,

    /// Set when the LCD is switched back on, so the renderer can restart its frame state.
    lcd_restarted: bool,

    /// Optional `(cycle, event)` log.
    ///
    /// Off by default and never serialized. Two runs from identical state must produce
    /// identical traces; that is the determinism property everything else depends on, and it
    /// is cheap to check directly rather than infer.
    pub trace: Option<Vec<(Cycles, GbEvent)>>,
}

impl Default for GbTiming {
    fn default() -> Self {
        Self::new()
    }
}

impl GbTiming {
    pub fn new() -> Self {
        let mut timing = Self {
            scheduler: Scheduler::with_capacity(8),
            now: Cycles::ZERO,
            timer: Timer::default(),
            ppu: PpuTiming::default(),
            apu: ApuSequencer::default(),
            lcd_restarted: false,
            trace: None,
        };
        timing.reset();
        timing
    }

    /// Return to power-on state, with the LCD on and the machine at cycle zero.
    pub fn reset(&mut self) {
        self.scheduler.clear();
        self.now = Cycles::ZERO;
        self.timer = Timer::default();
        self.ppu = PpuTiming {
            lcd_enabled: true,
            mode: PpuMode::OamScan,
            ..Default::default()
        };
        self.apu = ApuSequencer::default();
        self.lcd_restarted = false;
        if let Some(trace) = &mut self.trace {
            trace.clear();
        }

        self.schedule_ppu(self.now, OAM_SCAN_CYCLES);
        self.reschedule_sequencer();
        self.reschedule_timer();
    }

    #[inline]
    pub fn now(&self) -> Cycles {
        self.now
    }

    #[inline]
    pub fn set_now(&mut self, now: Cycles) {
        self.now = now;
    }

    /// How far the CPU may run before an event must be serviced.
    ///
    /// Always at least one cycle: returning zero would let a caller spin without making
    /// progress, and an already-due event is drained by [`Self::advance_to`] anyway.
    pub fn cycles_until_next_event(&self) -> Cycles {
        self.scheduler
            .cycles_until_next(self.now)
            .filter(|c| *c > Cycles::ZERO)
            .unwrap_or(Cycles(1))
    }

    pub fn pending_events(&self) -> usize {
        self.scheduler.len()
    }

    /// Move to `now` and fire everything due, including anything a handler schedules for the
    /// current cycle.
    pub fn advance_to(&mut self, now: Cycles) -> TimingOutput {
        self.now = now;
        let mut out = TimingOutput::default();
        while let Some((when, event)) = self.scheduler.pop_due(now) {
            if let Some(trace) = &mut self.trace {
                trace.push((when, event));
            }
            // Handlers reschedule from `when`, not from `now`, so overshoot never accumulates.
            match event {
                GbEvent::TimerIncrement => self.on_timer_increment(when, &mut out),
                GbEvent::TimerReload => self.on_timer_reload(&mut out),
                GbEvent::PpuModeChange => self.on_ppu_mode_change(when, &mut out),
                GbEvent::ApuSequencerTick => self.on_sequencer_tick(when, &mut out),
            }
        }
        out
    }

    // -- Timer ---------------------------------------------------------------

    fn reschedule_timer(&mut self) {
        self.scheduler.cancel(self.timer.increment_handle);
        self.timer.increment_handle = EventHandle::NONE;
        if !self.timer.enabled() {
            return;
        }
        let counter = self.timer.counter(self.now);
        let delay = cycles_until_bit_falls(counter, self.timer.selected_bit());
        self.timer.increment_handle = self
            .scheduler
            .schedule(self.now + Cycles(delay), GbEvent::TimerIncrement);
    }

    fn on_timer_increment(&mut self, when: Cycles, out: &mut TimingOutput) {
        self.bump_tima(when, out);

        // Reschedule from the event's own timestamp so the period stays exact.
        if self.timer.enabled() {
            let delay = 1u64 << (self.timer.selected_bit() + 1);
            self.timer.increment_handle = self
                .scheduler
                .schedule(when + Cycles(delay), GbEvent::TimerIncrement);
        }
    }

    /// Increment `TIMA`, starting the reload sequence if it overflowed.
    fn bump_tima(&mut self, when: Cycles, _out: &mut TimingOutput) {
        let (value, overflowed) = self.timer.tima.overflowing_add(1);
        self.timer.tima = value;
        if !overflowed {
            return;
        }
        // On overflow TIMA reads zero for four cycles before TMA is loaded and the interrupt
        // fires. Games observe that window, and a write to TIMA inside it cancels the reload.
        self.timer.reload_pending = true;
        self.timer.reload_handle = self
            .scheduler
            .schedule(when + Cycles(4), GbEvent::TimerReload);
    }

    fn on_timer_reload(&mut self, out: &mut TimingOutput) {
        // A write to TIMA during the delay clears this, and then nothing happens here.
        if !self.timer.reload_pending {
            return;
        }
        self.timer.reload_pending = false;
        self.timer.tima = self.timer.tma;
        out.interrupts |= interrupt::TIMER;
    }

    // -- PPU -----------------------------------------------------------------

    fn schedule_ppu(&mut self, from: Cycles, delay: u64) {
        self.ppu.handle = self
            .scheduler
            .schedule(from + Cycles(delay), GbEvent::PpuModeChange);
    }

    fn on_ppu_mode_change(&mut self, when: Cycles, out: &mut TimingOutput) {
        if !self.ppu.lcd_enabled {
            return;
        }

        match self.ppu.mode {
            PpuMode::OamScan => {
                self.ppu.mode = PpuMode::Drawing;
                self.schedule_ppu(when, DRAWING_CYCLES);
            }
            PpuMode::Drawing => {
                self.ppu.mode = PpuMode::HBlank;
                // Drawing has ended, so this line's pixels are final. Anything the game
                // writes to the scroll registers from here affects the *next* line.
                out.scanline_ready = Some(self.ppu.ly);
                self.schedule_ppu(when, HBLANK_CYCLES);
            }
            PpuMode::HBlank => {
                self.ppu.ly += 1;
                if self.ppu.ly == VISIBLE_LINES {
                    self.ppu.mode = PpuMode::VBlank;
                    // The VBlank interrupt is separate from the STAT one and always fires.
                    out.interrupts |= interrupt::VBLANK;
                    out.frame_ready = true;
                    self.schedule_ppu(when, LINE_CYCLES);
                } else {
                    self.ppu.mode = PpuMode::OamScan;
                    self.schedule_ppu(when, OAM_SCAN_CYCLES);
                }
            }
            PpuMode::VBlank => {
                self.ppu.ly += 1;
                if self.ppu.ly >= TOTAL_LINES {
                    self.ppu.ly = 0;
                    self.ppu.mode = PpuMode::OamScan;
                    out.frame_started = true;
                    self.schedule_ppu(when, OAM_SCAN_CYCLES);
                } else {
                    self.schedule_ppu(when, LINE_CYCLES);
                }
            }
        }

        if self.ppu.update_stat_line() {
            out.interrupts |= interrupt::LCD_STAT;
        }
    }

    /// Whether the LCD was re-enabled since the last check, which restarts rendering.
    pub fn take_lcd_restarted(&mut self) -> bool {
        std::mem::take(&mut self.lcd_restarted)
    }

    fn set_lcd_enabled(&mut self, enabled: bool) {
        if enabled == self.ppu.lcd_enabled {
            return;
        }
        self.ppu.lcd_enabled = enabled;
        self.scheduler.cancel(self.ppu.handle);
        self.ppu.handle = EventHandle::NONE;

        if enabled {
            // Restarting always begins at the top of the frame.
            self.ppu.ly = 0;
            self.ppu.mode = PpuMode::OamScan;
            self.ppu.stat_line = false;
            self.lcd_restarted = true;
            let now = self.now;
            self.schedule_ppu(now, OAM_SCAN_CYCLES);
        } else {
            // Switching the LCD off parks the PPU at line 0 in mode 0.
            self.ppu.ly = 0;
            self.ppu.mode = PpuMode::HBlank;
            self.ppu.stat_line = false;
        }
    }

    // -- APU sequencer -------------------------------------------------------

    fn reschedule_sequencer(&mut self) {
        self.scheduler.cancel(self.apu.handle);
        let counter = self.timer.counter(self.now);
        let delay = cycles_until_bit_falls(counter, SEQUENCER_BIT);
        self.apu.handle = self
            .scheduler
            .schedule(self.now + Cycles(delay), GbEvent::ApuSequencerTick);
    }

    fn on_sequencer_tick(&mut self, when: Cycles, out: &mut TimingOutput) {
        self.advance_sequencer(out);
        self.apu.handle = self.scheduler.schedule(
            when + Cycles(1 << (SEQUENCER_BIT + 1)),
            GbEvent::ApuSequencerTick,
        );
    }

    fn advance_sequencer(&mut self, out: &mut TimingOutput) {
        self.apu.step = (self.apu.step + 1) % 8;
        let clocks = ApuSequencer::clocks_for(self.apu.step);
        out.apu_length_clocks += clocks.length as u8;
        out.apu_envelope_clocks += clocks.envelope as u8;
        out.apu_sweep_clocks += clocks.sweep as u8;
    }

    // -- Registers -----------------------------------------------------------

    /// Read a timing register, or `None` if this module does not own the address.
    pub fn read_register(&self, addr: u16) -> Option<u8> {
        Some(match addr {
            reg::DIV => self.timer.div(self.now),
            // TIMA reads as zero during the reload delay, which is how software detects it.
            reg::TIMA => {
                if self.timer.reload_pending {
                    0
                } else {
                    self.timer.tima
                }
            }
            reg::TMA => self.timer.tma,
            // Only three bits of TAC exist; the rest read as ones.
            reg::TAC => 0xF8 | (self.timer.tac & 0x07),
            reg::STAT => self.ppu.read_stat(),
            reg::LY => {
                if self.ppu.lcd_enabled {
                    self.ppu.ly
                } else {
                    0
                }
            }
            reg::LYC => self.ppu.lyc,
            _ => return None,
        })
    }

    /// Write a timing register. Returns any interrupt the write itself raises, or `None` if
    /// this module does not own the address.
    pub fn write_register(&mut self, addr: u16, value: u8) -> Option<u8> {
        let mut interrupts = 0u8;
        match addr {
            reg::DIV => {
                // Any write zeroes the counter regardless of the value. If the timer's
                // selected bit was high it has just fallen, so TIMA increments — the quirk
                // that falls out of modelling the shared counter honestly.
                let was_high = self.timer.edge_output(self.now);
                let sequencer_bit_was_high =
                    bit_is_set(self.timer.counter(self.now), SEQUENCER_BIT);

                self.timer.div_base = self.now;

                if was_high {
                    let now = self.now;
                    let mut out = TimingOutput::default();
                    self.bump_tima(now, &mut out);
                    interrupts |= out.interrupts;
                }
                if sequencer_bit_was_high {
                    // The same edge clocks the frame sequencer, which is why resetting DIV
                    // can shorten a note.
                    let mut out = TimingOutput::default();
                    self.advance_sequencer(&mut out);
                }
                self.reschedule_timer();
                self.reschedule_sequencer();
            }
            reg::TIMA => {
                // Writing during the reload delay cancels the pending TMA load and its
                // interrupt; the written value stands.
                if self.timer.reload_pending {
                    self.timer.reload_pending = false;
                    self.scheduler.cancel(self.timer.reload_handle);
                    self.timer.reload_handle = EventHandle::NONE;
                }
                self.timer.tima = value;
            }
            reg::TMA => {
                self.timer.tma = value;
                // A TMA write lands in time to be the value reloaded, so the game sees the
                // new modulo immediately rather than one period late.
                if self.timer.reload_pending {
                    self.timer.tima = value;
                }
            }
            reg::TAC => {
                let was_high = self.timer.edge_output(self.now);
                self.timer.tac = value & 0x07;
                let now_high = self.timer.edge_output(self.now);
                // Moving the multiplexer onto a low bit, or disabling the timer while its bit
                // is high, is also a falling edge.
                if was_high && !now_high {
                    let now = self.now;
                    let mut out = TimingOutput::default();
                    self.bump_tima(now, &mut out);
                    interrupts |= out.interrupts;
                }
                self.reschedule_timer();
            }
            reg::LCDC => self.set_lcd_enabled(value & 0x80 != 0),
            reg::STAT => {
                // Bits 0-2 are read-only status; only the source selects are writable.
                self.ppu.stat_selects = value & 0x78;
                if self.ppu.update_stat_line() {
                    interrupts |= interrupt::LCD_STAT;
                }
            }
            // Writing LY resets the line counter rather than setting it.
            reg::LY => {
                self.ppu.ly = 0;
                if self.ppu.update_stat_line() {
                    interrupts |= interrupt::LCD_STAT;
                }
            }
            reg::LYC => {
                self.ppu.lyc = value;
                if self.ppu.update_stat_line() {
                    interrupts |= interrupt::LCD_STAT;
                }
            }
            _ => return None,
        }
        Some(interrupts)
    }
}

impl Savable for GbTiming {
    fn save(&self, w: &mut StateWriter) {
        self.scheduler.save(w);
        self.now.save(w);

        self.timer.div_base.save(w);
        w.write_u8(self.timer.tima);
        w.write_u8(self.timer.tma);
        w.write_u8(self.timer.tac);
        w.write_bool(self.timer.reload_pending);
        w.write_u64(self.timer.increment_handle.raw());
        w.write_u64(self.timer.reload_handle.raw());

        w.write_u8(self.ppu.ly);
        w.write_u8(self.ppu.lyc);
        w.write_u8(self.ppu.mode as u8);
        w.write_u8(self.ppu.stat_selects);
        w.write_bool(self.ppu.lcd_enabled);
        w.write_bool(self.ppu.stat_line);
        w.write_u64(self.ppu.handle.raw());

        w.write_u8(self.apu.step);
        w.write_u64(self.apu.handle.raw());
        // `trace` is a diagnostic buffer, not machine state, so it is deliberately omitted.
    }

    fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        self.scheduler.load(r)?;
        self.now.load(r)?;

        self.timer.div_base.load(r)?;
        self.timer.tima = r.read_u8()?;
        self.timer.tma = r.read_u8()?;
        self.timer.tac = r.read_u8()?;
        self.timer.reload_pending = r.read_bool()?;
        self.timer.increment_handle = EventHandle::from_raw(r.read_u64()?);
        self.timer.reload_handle = EventHandle::from_raw(r.read_u64()?);

        self.ppu.ly = r.read_u8()?;
        self.ppu.lyc = r.read_u8()?;
        self.ppu.mode = match r.read_u8()? {
            0 => PpuMode::HBlank,
            1 => PpuMode::VBlank,
            2 => PpuMode::OamScan,
            3 => PpuMode::Drawing,
            other => return Err(StateError::Malformed(format!("bad PPU mode {other}"))),
        };
        self.ppu.stat_selects = r.read_u8()?;
        self.ppu.lcd_enabled = r.read_bool()?;
        self.ppu.stat_line = r.read_bool()?;
        self.ppu.handle = EventHandle::from_raw(r.read_u64()?);

        self.apu.step = r.read_u8()?;
        self.apu.handle = EventHandle::from_raw(r.read_u64()?);
        Ok(())
    }
}

#[cfg(test)]
mod tests;
