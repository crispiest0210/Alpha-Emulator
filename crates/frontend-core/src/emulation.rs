//! The emulation thread itself: the loop that owns the running machine.
//!
//! Nothing outside this module holds a [`System`]. That is what makes the thread boundary real
//! rather than nominal — there is no shared object to accidentally lock, and no way for the
//! drawing thread to reach in and stall a frame.
//!
//! # The loop
//!
//! Drain commands, run one frame, publish it, pace to the platform's frame duration, repeat.
//! When idle or paused the loop blocks on the command channel with a timeout instead of spinning,
//! so a paused emulator costs no CPU while still flushing dirty save RAM on schedule.
//!
//! # Pacing is the emulation thread's own business
//!
//! [`Pacer`] adjusts *this* thread's target frame duration. The drawing thread redraws at
//! whatever rate the display and compositor give it and never changes speed. That separation is
//! why fast-forward here does not drop input or stutter: the two rates are independent, and the
//! frame pipe absorbs the mismatch by dropping frames nobody would have seen.

use crate::audio::{AudioProducer, Resampler};
use crate::config::RewindConfig;
use crate::frame::FramePublisher;
use crate::input::InputReceiver;
use crate::platform;
use crate::session::{
    LoadedRom, SavedState, SessionCommand, SessionEvent, SessionOptions, SessionStats,
    SessionStatus,
};
use core_common::{AudioSample, FrameOutput, InputState, System, AUDIO_SAMPLE_RATE};
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, TryRecvError};
use debugger::{AccessKind, Breakpoints, Trigger};
use library::{AppPaths, RomId};
use savestate::RewindBuffer;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// How long dirty save RAM waits before being written.
///
/// A game writes to save RAM in bursts — several hundred bytes over a few frames — so writing on
/// every dirty frame would mean dozens of file writes for one in-game save. Two seconds is short
/// enough that a crash loses nothing a player would notice and long enough to coalesce a burst.
const SAVE_RAM_DEBOUNCE: Duration = Duration::from_secs(2);

/// How often statistics are published. Four times a second: fast enough that a HUD feels live,
/// slow enough that the number is readable rather than flickering.
const STATS_INTERVAL: Duration = Duration::from_millis(250);

/// How far behind schedule the loop may fall before it stops trying to catch up.
///
/// Beyond this, catching up would mean running flat out for a noticeable time — which sounds and
/// looks worse than accepting that some emulated time was lost. A laptop lid closing produces a
/// gap of minutes, and no player wants that replayed at maximum speed.
const RESYNC_THRESHOLD: Duration = Duration::from_millis(250);

/// How long the loop blocks waiting for a command when there is nothing to run.
const IDLE_POLL: Duration = Duration::from_millis(20);

/// The thread body. Runs until told to shut down or until the command channel closes.
pub(crate) fn run(
    options: SessionOptions,
    commands: Receiver<SessionCommand>,
    events: Sender<SessionEvent>,
    frames: FramePublisher,
    input: InputReceiver,
) {
    let config = options.config;
    let mut emulator = Emulator {
        paths: options.paths,
        events,
        input,
        out: Outputs {
            frames,
            audio: options.audio,
            output_rate: options.audio_output_rate.max(1),
            resampler: Resampler::new(AUDIO_SAMPLE_RATE, options.audio_output_rate.max(1)),
            resampled: Vec::new(),
            volume: config.audio.volume,
            muted: config.audio.muted,
        },
        active: None,
        paused: false,
        fast_forward: false,
        rewinding: false,
        fast_forward_speed: config.emulation.fast_forward_speed,
        rewind_config: config.rewind,
        step_budget: 0,
        instruction_budget: 0,
        breakpoints: Breakpoints::new(),
        debug_attached: false,
        last_debug_request: None,
        resume_past: None,
        pacer: Pacer::default(),
        stats: StatsWindow::new(),
    };
    emulator.emit(SessionEvent::StatusChanged(SessionStatus::Idle));

    loop {
        // Commands first, always: a pause or a ROM switch should take effect before the next
        // frame runs, not after it.
        loop {
            match commands.try_recv() {
                Ok(command) => {
                    if emulator.handle(command) {
                        emulator.close_rom();
                        return;
                    }
                }
                Err(TryRecvError::Empty) => break,
                // The `Session` was dropped without a clean shutdown. Flush and go.
                Err(TryRecvError::Disconnected) => {
                    emulator.close_rom();
                    return;
                }
            }
        }

        if emulator.should_run() {
            emulator.tick();
        } else {
            emulator.idle_maintenance();
            match commands.recv_timeout(IDLE_POLL) {
                Ok(command) => {
                    if emulator.handle(command) {
                        emulator.close_rom();
                        return;
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    emulator.close_rom();
                    return;
                }
            }
        }

        emulator.publish_stats_if_due();
    }
}

/// Everything the loop needs that is not the machine.
struct Emulator {
    paths: AppPaths,
    events: Sender<SessionEvent>,
    input: InputReceiver,
    out: Outputs,
    active: Option<Active>,

    paused: bool,
    fast_forward: bool,
    rewinding: bool,
    fast_forward_speed: f32,
    rewind_config: RewindConfig,
    /// Frames still owed to a `StepFrames` command while paused.
    step_budget: u32,
    /// Instructions still owed to a `StepInstructions` command.
    instruction_budget: u32,

    /// Execution breakpoints. Consulted only while [`debug_attached`](Self::debug_attached).
    breakpoints: Breakpoints,
    /// Whether to run instruction-at-a-time so breakpoints can be checked between them.
    debug_attached: bool,
    /// The last snapshot request, re-served whenever execution stops.
    ///
    /// Without this, "step, then refresh" is a race the caller cannot win: both commands are drained
    /// before the loop ticks, so the snapshot would be captured *before* the step ran and show the
    /// state the user just left. Remembering the request and re-serving it on every stop is what
    /// makes a step button show the instruction it landed on.
    last_debug_request: Option<debugger::Request>,
    /// The address a resume must not immediately re-break on.
    ///
    /// Without this, continuing from a breakpoint checks the same address, breaks again, and the
    /// machine never advances — the classic way a first debugger implementation appears to hang.
    resume_past: Option<u32>,

    pacer: Pacer,
    stats: StatsWindow,
}

/// The loaded machine and everything scoped to it.
struct Active {
    system: Box<dyn System>,
    rom: LoadedRom,
    /// Frames run since load. Not the machine's own counter — it is the frontend's, and it is
    /// what a save state's "frame" field and the HUD both report.
    frame: u64,
    frame_duration: Duration,
    frame_rate: f64,
    /// Emulated cycles in one frame, for the debugger's stepping loop — which has no `step_frame`
    /// to tell it when a frame ended.
    frame_cycles: u64,
    /// `None` when rewind is switched off in the settings.
    ///
    /// Not a zero-capacity buffer: [`RewindBuffer::new`] deliberately clamps capacity up to one,
    /// so "disabled" is not expressible as a capacity and trying to say it that way silently
    /// records snapshots. Making the absence structural is the only version that cannot drift.
    rewind: Option<RewindBuffer>,
    save_ram_path: PathBuf,
    /// When save RAM first went dirty without having been written since.
    dirty_since: Option<Instant>,
    stopped: bool,
}

/// The output side: frames out, audio out.
struct Outputs {
    frames: FramePublisher,
    audio: Option<AudioProducer>,
    output_rate: u32,
    resampler: Resampler,
    resampled: Vec<AudioSample>,
    volume: f32,
    muted: bool,
}

impl Outputs {
    /// Send one frame's audio to the ring, resampled and attenuated.
    ///
    /// `speed` is the emulation speed multiplier. It is folded into the resampler's target rate
    /// rather than ignored, which is what makes fast-forward *sound* sped up: four times as many
    /// samples arrive per real second, and compressing them to the device's rate raises the pitch
    /// exactly as a tape running fast does. Ignoring it instead would push four times more than
    /// the ring can take and drop three quarters of every frame's audio — a stutter, not a
    /// speed-up.
    ///
    /// `speed` of zero means uncapped, where there is no meaningful ratio because the rate is
    /// whatever the host manages moment to moment. Uncapped therefore produces silence, which is
    /// honest; a stream resampled by a guessed ratio would be noise.
    fn push_audio(&mut self, samples: &[AudioSample], speed: f32) {
        let Some(audio) = self.audio.as_mut() else {
            return;
        };
        if samples.is_empty() || speed <= 0.0 {
            return;
        }
        let gain = if self.muted { 0.0 } else { self.volume };

        // A device already running at the core's rate needs no interpolation at all, and the
        // fast path also avoids the resampler's one sample of inherent latency.
        let target = (self.output_rate as f32 / speed).round().max(1.0) as u32;
        if target == AUDIO_SAMPLE_RATE {
            self.resampled.clear();
            self.resampled.extend_from_slice(samples);
        } else {
            if self.resampler.target_rate() != target {
                self.resampler.set_target_rate(target);
            }
            self.resampled.clear();
            self.resampler.process(samples, &mut self.resampled);
        }
        if gain != 1.0 {
            for sample in &mut self.resampled {
                sample.left *= gain;
                sample.right *= gain;
            }
        }
        audio.push(&self.resampled);
    }

    fn audio_dropped(&self) -> u64 {
        self.audio.as_ref().map(|a| a.dropped()).unwrap_or(0)
    }
}

impl Emulator {
    fn emit(&self, event: SessionEvent) {
        let _ = self.events.send(event);
    }

    fn status(&self) -> SessionStatus {
        match self.active.as_ref() {
            None => SessionStatus::Idle,
            Some(active) if active.stopped => SessionStatus::Stopped,
            Some(_) if self.paused => SessionStatus::Paused,
            Some(_) => SessionStatus::Running,
        }
    }

    fn announce_status(&self) {
        self.emit(SessionEvent::StatusChanged(self.status()));
    }

    /// Whether this iteration should advance the machine.
    fn should_run(&self) -> bool {
        let Some(active) = self.active.as_ref() else {
            return false;
        };
        if active.stopped {
            return false;
        }
        // A step budget overrides a pause, which is exactly what a debugger's step button means:
        // stay paused, but advance.
        !self.paused || self.step_budget > 0 || self.instruction_budget > 0
    }

    /// The speed multiplier this iteration runs at. Zero means uncapped.
    fn speed(&self) -> f32 {
        if self.step_budget > 0 {
            // Stepping while paused runs one frame and then stops, so there is no rate to hold.
            // Reporting 1.0 keeps the audio for that single frame at correct pitch.
            return 1.0;
        }
        if self.fast_forward {
            self.fast_forward_speed
        } else {
            1.0
        }
    }

    // --- the frame ------------------------------------------------------------------------

    fn tick(&mut self) {
        if self.rewinding {
            self.rewind_tick();
        } else if self.needs_instruction_stepping() {
            self.debug_tick();
        } else {
            self.frame_tick();
        }

        let target = self.target_frame_duration();
        let sleep = self.pacer.wait_for(target, Instant::now());
        if !sleep.is_zero() {
            std::thread::sleep(sleep);
        }
    }

    /// Whether this iteration has to run one instruction at a time.
    ///
    /// Only when there is something to check between instructions. Attaching the debugger to look at
    /// registers and memory therefore costs nothing: the loop keeps calling `step_frame`, input keeps
    /// working, and snapshots are served between frames.
    fn needs_instruction_stepping(&self) -> bool {
        self.debug_attached && (!self.breakpoints.is_empty() || self.instruction_budget > 0)
    }

    /// Arm or disarm the bus recorder to match whether any watchpoint is registered.
    ///
    /// Called whenever the watchpoint set or the attachment changes, never per frame. The recorder
    /// costs a branch per bus access while armed *and* while not, so the only thing arming changes is
    /// whether entries accumulate — but leaving it armed with nothing watching would mean allocating
    /// and draining a log nobody reads.
    fn sync_access_log(&mut self) {
        let wanted = self.debug_attached && !self.breakpoints.watchpoints().is_empty();
        if let Some(log) = self.active.as_mut().and_then(|a| a.system.access_log()) {
            if log.is_armed() != wanted {
                log.set_armed(wanted);
            }
        }
    }

    fn target_frame_duration(&self) -> Duration {
        let Some(active) = self.active.as_ref() else {
            return IDLE_POLL;
        };
        let speed = self.speed();
        if speed <= 0.0 {
            return Duration::ZERO;
        }
        active.frame_duration.div_f32(speed)
    }

    /// One frame's worth of emulated time, run one instruction at a time.
    ///
    /// This is the whole mechanism behind execution breakpoints, and it needs no hook inside any
    /// system: the check happens *here*, between calls to
    /// [`step_instruction`](System::step_instruction), so a system crate never learns that
    /// breakpoints exist and a detached session runs `step_frame` exactly as it always did.
    ///
    /// The cost is real — a breakpoint check and a virtual call per instruction — and it is paid
    /// only while the debugger is attached, which is the trade prompt 15 asks for. Prompt 18 can
    /// measure it; nothing here claims a number.
    fn debug_tick(&mut self) {
        let input = self.input.latest();
        let speed = self.speed();
        let present = self.out.frames.has_room();
        let single_stepping = self.instruction_budget > 0;

        let Some(active) = self.active.as_mut() else {
            return;
        };
        // Applied once per debug tick rather than per instruction, which is the same granularity
        // `step_frame` gives it: `InputState` applies for a whole frame by the `System` contract, and
        // no frontend can produce meaningful sub-frame input from a 60 Hz event loop anyway.
        active.system.set_input(input);
        let budget = active.frame_cycles;
        let mut spent = 0u64;
        let mut hit = None;
        let mut watch_hit: Option<Trigger> = None;
        let mut watch_overflow = false;

        while spent < budget {
            let pc = active.system.debug().map(|target| target.program_counter());
            if let Some(pc) = pc {
                // The instruction being resumed *from* is exempt, once.
                if self.resume_past == Some(pc) {
                    self.resume_past = None;
                } else if self.breakpoints.check_execution(pc).is_some() {
                    hit = Some(pc);
                    break;
                }
            }
            spent += active.system.step_instruction().0;

            // Watchpoints, checked here rather than inside the bus. The bus only *records*; the
            // registry that decides lives above the systems, exactly as it does for execution
            // breakpoints.
            if let Some(log) = active.system.access_log() {
                if !log.is_empty() || log.overflowed() {
                    let overflowed = log.overflowed();
                    // Collected before consulting the registry: `check_access` needs `&mut` on the
                    // breakpoints and the drain borrows the system, and the two live on different
                    // fields of `self` only after the log's borrow has ended.
                    let accesses: Vec<_> = log.drain().collect();
                    if overflowed {
                        watch_overflow = true;
                    }
                    for access in accesses {
                        if let Some(trigger) = self.breakpoints.check_access(
                            access.addr,
                            access.kind,
                            access.value as u32,
                        ) {
                            watch_hit = Some(trigger);
                            break;
                        }
                    }
                    if watch_hit.is_some() {
                        break;
                    }
                }
            }

            if self.instruction_budget > 0 {
                self.instruction_budget -= 1;
                if self.instruction_budget == 0 {
                    break;
                }
            }
        }

        // Audio is drained whatever happened, because the system's buffer is bounded.
        let samples = active.system.take_audio_samples();
        self.out.push_audio(samples, speed);
        if present || hit.is_some() || single_stepping {
            let number = active.frame;
            self.out.frames.publish(number, active.system.framebuffer());
        }
        active.frame += 1;
        self.stats.frame_completed();

        if watch_overflow {
            // Said out loud rather than swallowed. An instruction that made more accesses than the
            // log holds may have touched a watched address without the debugger noticing, and a
            // watchpoint that silently misses a hit is worse than one that admits it might have.
            self.emit(SessionEvent::Notice(
                "an instruction made more bus accesses than the watch log holds; \
                 a watchpoint may have been missed"
                    .to_string(),
            ));
        }
        if let Some(Trigger::Watchpoint { addr, kind, value }) = watch_hit {
            // Stopped *after* the instruction, unlike an execution breakpoint: the access has
            // already happened, and the useful thing to show is the state it produced.
            self.paused = true;
            self.instruction_budget = 0;
            self.pacer.reset();
            self.emit(SessionEvent::WatchpointHit {
                addr,
                write: kind == AccessKind::Write,
                value: value as u8,
            });
            self.announce_status();
            self.reserve_debug_snapshot();
            return;
        }
        if let Some(addr) = hit {
            // Stop *before* executing the instruction, which is what a breakpoint means — the
            // register view then shows the state the instruction is about to act on.
            self.paused = true;
            self.instruction_budget = 0;
            self.resume_past = Some(addr);
            self.pacer.reset();
            self.emit(SessionEvent::BreakpointHit { addr });
            self.announce_status();
            self.reserve_debug_snapshot();
        } else if single_stepping && self.instruction_budget == 0 {
            self.paused = true;
            self.pacer.reset();
            self.announce_status();
            self.reserve_debug_snapshot();
        }
    }

    /// Re-send the last snapshot request, if there was one.
    fn reserve_debug_snapshot(&mut self) {
        if let Some(request) = self.last_debug_request {
            self.serve_debug_snapshot(request);
        }
    }

    fn frame_tick(&mut self) {
        let input = self.input.latest();
        let speed = self.speed();
        // During fast-forward most frames would be dropped by the pipe anyway, so the copy is
        // skipped rather than performed and thrown away.
        let present = self.out.frames.has_room();
        let Some(active) = self.active.as_mut() else {
            return;
        };
        let output = active.step(input, &mut self.out, speed, present);

        if self.step_budget > 0 {
            self.step_budget -= 1;
        }
        self.stats.frame_completed();

        if output.save_ram_dirty && active.dirty_since.is_none() {
            active.dirty_since = Some(Instant::now());
        }
        if output.stopped && !active.stopped {
            active.stopped = true;
            self.announce_status();
        }
        self.maybe_flush_save_ram(false);
    }

    /// One step backwards through the rewind buffer.
    ///
    /// Produces no audio. Playing a state's audio backwards is not a thing the pipeline can do,
    /// and playing it forwards while the picture goes backwards is worse than silence.
    fn rewind_tick(&mut self) {
        let Some(active) = self.active.as_mut() else {
            return;
        };
        match active.rewind_one() {
            Ok(frame) => {
                // Always presented, unlike a fast-forward frame: a rewind the player cannot see
                // is indistinguishable from a rewind that is not happening.
                self.out.frames.publish(frame, active.system.framebuffer());
                self.stats.frame_completed();
            }
            Err(RewindStep::Disabled) => {
                self.rewinding = false;
                self.emit(SessionEvent::Notice(
                    "rewind is switched off in the settings".to_string(),
                ));
            }
            Err(RewindStep::Exhausted) => {
                // Nothing older to go to. Say so once rather than every frame the key is held.
                self.rewinding = false;
                self.emit(SessionEvent::Notice(
                    "cannot rewind any further".to_string(),
                ));
            }
            Err(RewindStep::Rejected(message)) => {
                self.rewinding = false;
                self.emit(SessionEvent::Error(message));
            }
        }
    }

    /// Housekeeping that must happen even when nothing is running.
    fn idle_maintenance(&mut self) {
        self.maybe_flush_save_ram(false);
    }

    // --- commands -------------------------------------------------------------------------

    /// Returns `true` when the thread should exit.
    fn handle(&mut self, command: SessionCommand) -> bool {
        match command {
            SessionCommand::LoadRom { path, rom_id } => self.load_rom(&path, rom_id),
            SessionCommand::CloseRom => {
                if self.active.is_some() {
                    self.close_rom();
                    self.emit(SessionEvent::RomClosed);
                    self.announce_status();
                }
            }
            SessionCommand::Reset => self.reset(),
            SessionCommand::SetPaused(paused) => self.set_paused(paused),
            SessionCommand::TogglePause => self.set_paused(!self.paused),
            SessionCommand::StepFrames(n) => {
                self.step_budget = self.step_budget.saturating_add(n);
                // A step is a discrete jump, not a resumption of a rate, so the pacer must not
                // try to make up the time the machine spent paused.
                self.pacer.reset();
            }
            SessionCommand::SaveState { slot, label } => self.save_state(slot, label),
            SessionCommand::LoadState { path } => self.load_state(&path),
            SessionCommand::LoadSlot(slot) => {
                let path = self
                    .active
                    .as_ref()
                    .map(|active| self.paths.state_slot_file(&active.rom.path, slot));
                match path {
                    Some(path) => self.load_state(&path),
                    None => self.emit(SessionEvent::Error(
                        "no cartridge is loaded, so there is no slot to load".into(),
                    )),
                }
            }
            SessionCommand::SetFastForward(on) => {
                if self.fast_forward != on {
                    self.fast_forward = on;
                    // Leaving fast-forward with a stale deadline would make the loop believe it
                    // owes several seconds of frames.
                    self.pacer.reset();
                }
            }
            SessionCommand::SetRewinding(on) => {
                if self.rewinding != on {
                    self.rewinding = on;
                    self.pacer.reset();
                }
            }
            SessionCommand::SetVolume(volume) => {
                self.out.volume = volume.clamp(0.0, 1.0);
            }
            SessionCommand::SetMuted(muted) => self.out.muted = muted,
            SessionCommand::SetFastForwardSpeed(speed) => {
                self.fast_forward_speed = if speed.is_finite() {
                    speed.clamp(0.0, 64.0)
                } else {
                    1.0
                };
            }
            SessionCommand::SetRewindConfig(config) => {
                self.rewind_config = config;
                if let Some(active) = self.active.as_mut() {
                    let capacity = self.rewind_config.snapshot_capacity(active.frame_rate);
                    // Resizing means a new ring; the old contents describe the same machine, but
                    // reusing them would need a resize the buffer does not offer. Clearing loses
                    // history the player has not asked to keep, which is why the setting is not
                    // on a hotkey.
                    active.rewind = (capacity > 0).then(|| {
                        RewindBuffer::new(capacity, self.rewind_config.interval_frames as u64)
                    });
                }
            }
            SessionCommand::SetAudioOutputRate(rate) => {
                self.out.output_rate = rate.max(1);
                self.out.resampler.set_target_rate(rate.max(1));
            }
            SessionCommand::FlushSaveRam => self.maybe_flush_save_ram(true),

            SessionCommand::SetDebugAttached(attached) => {
                if self.debug_attached != attached {
                    self.debug_attached = attached;
                    self.resume_past = None;
                    self.instruction_budget = 0;
                    self.sync_access_log();
                    // Detaching leaves the breakpoints registered but unchecked. That is the honest
                    // behaviour: closing the panel should not silently discard the work of setting
                    // them, and re-opening it must not surprise the user by breaking somewhere they
                    // forgot about — so the panel shows the list either way.
                    self.pacer.reset();
                }
            }
            SessionCommand::RequestDebugSnapshot(request) => {
                self.last_debug_request = Some(request);
                self.serve_debug_snapshot(request);
            }
            SessionCommand::AddBreakpoint(addr) => {
                self.breakpoints.add_execution(addr);
                self.pacer.reset();
            }
            SessionCommand::RemoveBreakpoint(addr) => {
                self.breakpoints.remove_execution(addr);
                if self.resume_past == Some(addr) {
                    self.resume_past = None;
                }
            }
            SessionCommand::ClearBreakpoints => {
                self.breakpoints.clear();
                self.resume_past = None;
                self.sync_access_log();
            }
            SessionCommand::AddWatchpoint(watchpoint) => {
                if self
                    .active
                    .as_mut()
                    .and_then(|a| a.system.access_log())
                    .is_none()
                {
                    self.emit(SessionEvent::DebugUnavailable(
                        "this system's bus does not record accesses, so a watchpoint here could \
                         never fire"
                            .into(),
                    ));
                } else {
                    self.breakpoints.add_watchpoint(watchpoint);
                    self.sync_access_log();
                    self.pacer.reset();
                }
            }
            SessionCommand::RemoveWatchpointsAt(addr) => {
                self.breakpoints.remove_watchpoints_at(addr);
                self.sync_access_log();
            }
            SessionCommand::StepInstructions(n) => {
                if self.active.is_none() {
                    self.emit(SessionEvent::DebugUnavailable(
                        "no cartridge is loaded, so there is nothing to step".into(),
                    ));
                } else {
                    self.instruction_budget = self.instruction_budget.saturating_add(n.max(1));
                    self.pacer.reset();
                }
            }
            SessionCommand::SetProgramCounter(pc) => {
                match self
                    .active
                    .as_mut()
                    .and_then(|active| active.system.debug())
                {
                    Some(target) => {
                        target.set_program_counter(pc);
                        // The exemption belongs to the address being *left*, and the machine is no
                        // longer there.
                        self.resume_past = None;
                        self.emit(SessionEvent::Notice(format!("jumped to {pc:#X}")));
                    }
                    None => self.emit(SessionEvent::DebugUnavailable(
                        "this system offers no debug introspection".into(),
                    )),
                }
            }

            SessionCommand::Shutdown => return true,
        }
        false
    }

    /// Capture a snapshot and send it.
    ///
    /// Runs on the emulation thread between frames, which is what makes it safe to read the machine
    /// at all — and why [`debugger::Request`] is clamped before it is served. A request that asked
    /// for a million rows would show up as the emulator stuttering, not as an error.
    fn serve_debug_snapshot(&mut self, request: debugger::Request) {
        let breakpoints = &self.breakpoints;
        let snapshot = match self
            .active
            .as_mut()
            .and_then(|active| active.system.debug())
        {
            Some(target) => debugger::capture(target, breakpoints, &request),
            None => {
                let reason = if self.active.is_none() {
                    "no cartridge is loaded"
                } else {
                    "this system offers no debug introspection yet"
                };
                self.emit(SessionEvent::DebugUnavailable(reason.into()));
                return;
            }
        };
        self.emit(SessionEvent::DebugSnapshot(Box::new(snapshot)));
    }

    fn set_paused(&mut self, paused: bool) {
        if self.paused == paused {
            return;
        }
        self.paused = paused;
        self.step_budget = 0;
        self.instruction_budget = 0;
        if !paused {
            // Resuming from a breakpoint: the address the machine is sitting on is exempt for one
            // check, or continuing would break again immediately and the machine would never move.
            if let Some(target) = self
                .active
                .as_mut()
                .and_then(|active| active.system.debug())
            {
                self.resume_past = Some(target.program_counter());
            }
        }
        self.pacer.reset();
        // Resuming with a full audio ring of pre-pause samples would replay the moment before the
        // pause. Nothing is dropped here — the ring drains naturally within ~85 ms — but the
        // resampler's carried sample would splice two unrelated moments together.
        self.out.resampler.reset();
        self.announce_status();
    }

    fn load_rom(&mut self, path: &Path, rom_id: Option<RomId>) {
        self.close_rom();

        let (platform, mut system) = match platform::load(path) {
            Ok(pair) => pair,
            Err(e) => {
                self.emit(SessionEvent::Error(e.to_string()));
                self.announce_status();
                return;
            }
        };

        // Battery-backed save RAM before the first frame runs, or the game boots, sees no save,
        // and offers to create one — over the top of the file that was about to be restored.
        let save_ram_path = self.paths.save_file(path);
        let mut save_ram_restored = false;
        if system.save_ram().is_some() {
            match std::fs::read(&save_ram_path) {
                Ok(data) => match system.load_save_ram(&data) {
                    Ok(()) => save_ram_restored = true,
                    Err(e) => self.emit(SessionEvent::Error(format!(
                        "{} exists but does not fit this cartridge: {e}",
                        save_ram_path.display()
                    ))),
                },
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => self.emit(SessionEvent::Error(format!(
                    "could not read {}: {e}",
                    save_ram_path.display()
                ))),
            }
        }

        let title = std::fs::read(path)
            .ok()
            .and_then(|bytes| platform::header_title(platform, &bytes))
            .or_else(|| {
                path.file_stem()
                    .and_then(|s| s.to_str())
                    .map(|s| s.to_string())
            })
            .unwrap_or_else(|| "Untitled".to_string());

        let framebuffer = system.framebuffer();
        let rom = LoadedRom {
            path: path.to_path_buf(),
            rom_id,
            platform,
            title,
            width: framebuffer.width(),
            height: framebuffer.height(),
            save_ram_restored,
        };
        let frame_rate = platform::frame_rate(platform);
        let capacity = self.rewind_config.snapshot_capacity(frame_rate);

        self.active = Some(Active {
            system,
            rom: rom.clone(),
            frame: 0,
            frame_duration: platform::frame_duration(platform),
            frame_rate,
            frame_cycles: platform::frame_cycles(platform),
            rewind: (capacity > 0)
                .then(|| RewindBuffer::new(capacity, self.rewind_config.interval_frames as u64)),
            save_ram_path,
            dirty_since: None,
            stopped: false,
        });
        self.paused = false;
        self.rewinding = false;
        self.step_budget = 0;
        self.instruction_budget = 0;
        // Breakpoints survive a ROM change deliberately — a contributor comparing two builds of the
        // same homebrew wants the same addresses — but the resume exemption belongs to a machine that
        // no longer exists.
        self.resume_past = None;
        self.pacer.reset();
        self.out.resampler.reset();
        self.stats = StatsWindow::new();

        self.sync_access_log();
        self.emit(SessionEvent::RomLoaded(rom));
        self.announce_status();
    }

    /// Flush and unload. Safe to call with nothing loaded.
    fn close_rom(&mut self) {
        self.maybe_flush_save_ram(true);
        if self.active.take().is_some() {
            self.rewinding = false;
            self.step_budget = 0;
            self.pacer.reset();
        }
    }

    fn reset(&mut self) {
        let Some(active) = self.active.as_mut() else {
            return;
        };
        active.system.reset();
        active.frame = 0;
        active.stopped = false;
        // The buffer describes a machine that no longer exists at those frames. Carrying it over
        // would let a rewind jump into the pre-reset run, which looks like a corrupted emulator.
        if let Some(rewind) = active.rewind.as_mut() {
            rewind.clear();
        }
        self.paused = false;
        self.rewinding = false;
        self.pacer.reset();
        self.out.resampler.reset();
        self.stats = StatsWindow::new();
        self.emit(SessionEvent::Notice("reset".into()));
        self.announce_status();
    }

    // --- save states ----------------------------------------------------------------------

    fn save_state(&mut self, slot: Option<u8>, label: Option<String>) {
        let Some(active) = self.active.as_ref() else {
            self.emit(SessionEvent::Error(
                "no cartridge is loaded, so there is nothing to save".into(),
            ));
            return;
        };

        // Neither a slot nor a label means the quicksave key: slot 0.
        let (slot, label) = match (slot, label) {
            (Some(slot), label) => (Some(slot), label.unwrap_or_else(|| format!("slot{slot}"))),
            (None, Some(label)) => (None, label),
            (None, None) => (Some(0), "slot0".to_string()),
        };
        let path = match slot {
            Some(slot) => self.paths.state_slot_file(&active.rom.path, slot),
            None => self.paths.state_named_file(&active.rom.path, &label),
        };

        let frame = active.frame;
        let rom_id = active.rom.rom_id;
        let bytes = wrap_state(frame, &active.system.save_state());
        if let Err(e) = write_atomically(&path, &bytes) {
            self.emit(SessionEvent::Error(format!(
                "could not write {}: {e}",
                path.display()
            )));
            return;
        }
        self.emit(SessionEvent::StateSaved(SavedState {
            rom_id,
            path,
            label,
            slot,
            frame,
            size_bytes: bytes.len() as u64,
        }));
    }

    fn load_state(&mut self, path: &Path) {
        if self.active.is_none() {
            self.emit(SessionEvent::Error(
                "no cartridge is loaded, so there is nothing to load into".into(),
            ));
            return;
        }
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                self.emit(SessionEvent::Notice(format!(
                    "{} does not exist yet",
                    path.display()
                )));
                return;
            }
            Err(e) => {
                self.emit(SessionEvent::Error(format!(
                    "could not read {}: {e}",
                    path.display()
                )));
                return;
            }
        };

        let (saved_frame, state) = unwrap_state(&bytes);
        let active = self.active.as_mut().expect("checked above");
        match active.system.load_state(state) {
            Ok(()) => {
                // Restore the frontend's frame counter too, or the HUD would jump forward to
                // wherever the machine happened to be when the state was loaded — and the save
                // list's frame column would stop matching what loading it actually shows.
                if let Some(saved_frame) = saved_frame {
                    active.frame = saved_frame;
                }
                // The rewind history belongs to the timeline that was just abandoned.
                if let Some(rewind) = active.rewind.as_mut() {
                    rewind.clear();
                }
                let frame = active.frame;
                let number = frame;
                let _ = self.out.frames.publish(number, active.system.framebuffer());
                self.out.resampler.reset();
                self.pacer.reset();
                self.emit(SessionEvent::StateLoaded {
                    path: path.to_path_buf(),
                    frame,
                });
            }
            Err(e) => {
                // `System::load_state` documents that a corrupt state can fail partway through,
                // leaving the machine inconsistent — so resetting is mandatory, not tidiness.
                active.system.reset();
                active.frame = 0;
                if let Some(rewind) = active.rewind.as_mut() {
                    rewind.clear();
                }
                self.emit(SessionEvent::Error(format!(
                    "{} could not be loaded ({e}); the machine has been reset",
                    path.display()
                )));
                self.announce_status();
            }
        }
    }

    // --- save RAM -------------------------------------------------------------------------

    /// Write dirty save RAM if the debounce has elapsed, or immediately when `force`.
    fn maybe_flush_save_ram(&mut self, force: bool) {
        let Some(active) = self.active.as_mut() else {
            return;
        };
        let Some(dirty_since) = active.dirty_since else {
            return;
        };
        if !force && dirty_since.elapsed() < SAVE_RAM_DEBOUNCE {
            return;
        }
        let Some(data) = active.system.save_ram_for_disk() else {
            active.dirty_since = None;
            return;
        };
        let path = active.save_ram_path.clone();
        let result = write_atomically(&path, &data);
        active.dirty_since = None;
        match result {
            Ok(()) => self.emit(SessionEvent::SaveRamWritten { path }),
            Err(e) => self.emit(SessionEvent::Error(format!(
                "could not write {}: {e}",
                path.display()
            ))),
        }
    }

    // --- statistics -----------------------------------------------------------------------

    fn publish_stats_if_due(&mut self) {
        if !self.stats.due() {
            return;
        }
        let (fps, frame, rewind_snapshots, rewind_span, rewind_bytes, frame_rate) =
            match self.active.as_ref() {
                Some(active) => (
                    self.stats.fps(),
                    active.frame,
                    active.rewind.as_ref().map_or(0, |r| r.len()),
                    active.rewind.as_ref().map_or(0, |r| r.span_frames()),
                    active.rewind.as_ref().map_or(0, |r| r.memory_used()),
                    active.frame_rate,
                ),
                None => (0.0, 0, 0, 0, 0, 1.0),
            };
        self.stats.window_closed();
        self.emit(SessionEvent::Stats(SessionStats {
            frame,
            fps,
            speed_percent: (fps as f64 / frame_rate * 100.0) as f32,
            audio_dropped: self.out.audio_dropped(),
            frames_dropped: self.out.frames.dropped(),
            rewind_snapshots,
            rewind_span_frames: rewind_span,
            rewind_bytes,
            fast_forward: self.fast_forward,
            rewinding: self.rewinding,
        }));
    }
}

/// Why a rewind step could not happen.
enum RewindStep {
    /// Rewind is switched off in the settings.
    Disabled,
    /// The buffer holds nothing older.
    Exhausted,
    /// A snapshot was there but the machine refused it.
    Rejected(String),
}

impl Active {
    /// Run one frame and hand its outputs on.
    fn step(
        &mut self,
        input: InputState,
        out: &mut Outputs,
        speed: f32,
        present: bool,
    ) -> FrameOutput {
        // Snapshot *before* the frame runs, so the recorded state is the start of frame N rather
        // than its end. Rewinding to it then replays that frame, which is what makes stepping
        // back land where the player expects.
        if let Some(rewind) = self.rewind.as_mut() {
            if rewind.wants_snapshot() {
                let state = self.system.save_state();
                rewind.push(self.frame, state);
            } else {
                rewind.frame_elapsed();
            }
        }

        let output = self.system.step_frame(input);
        self.frame += 1;

        // Audio is drained every frame whether or not anything will play it. The buffer inside
        // the system is bounded, and a run that never drains does not exercise the same path.
        let samples = self.system.take_audio_samples();
        out.push_audio(samples, speed);

        if present {
            out.frames.publish(self.frame, self.system.framebuffer());
        }
        output
    }

    /// Step one snapshot back and load it.
    fn rewind_one(&mut self) -> Result<u64, RewindStep> {
        let Some(rewind) = self.rewind.as_mut() else {
            return Err(RewindStep::Disabled);
        };
        let Some(snapshot) = rewind.rewind() else {
            return Err(RewindStep::Exhausted);
        };
        let frame = snapshot.frame;
        match self.system.load_state(&snapshot.state) {
            Ok(()) => {
                self.frame = frame;
                Ok(frame)
            }
            // A snapshot this machine cannot load means the buffer and the machine have got out
            // of step, which is a bug rather than a user error — so it is reported, not silently
            // skipped.
            Err(e) => Err(RewindStep::Rejected(format!(
                "a rewind snapshot from frame {frame} could not be restored: {e}"
            ))),
        }
    }
}

/// Keeps this thread's frame rate on the platform's schedule.
#[derive(Debug, Default)]
struct Pacer {
    /// When the next frame is due. `None` after any discontinuity.
    deadline: Option<Instant>,
}

impl Pacer {
    /// Forget the schedule. Called whenever emulated time and real time have deliberately
    /// diverged — a pause, a state load, a speed change — so the loop does not try to make up
    /// time it was never supposed to spend.
    fn reset(&mut self) {
        self.deadline = None;
    }

    /// How long to sleep before running the next frame.
    ///
    /// Deadlines accumulate rather than being measured from the end of each frame. Sleeping
    /// `target` after each frame *completes* would add the frame's own execution time to every
    /// interval, so a machine taking 4 ms per frame would run at 51 fps instead of 59.7 — a 15%
    /// error, audible as a pitch shift and visible as sluggishness.
    fn wait_for(&mut self, target: Duration, now: Instant) -> Duration {
        if target.is_zero() {
            self.deadline = None;
            return Duration::ZERO;
        }
        let mut deadline = match self.deadline {
            Some(previous) => previous + target,
            None => now + target,
        };
        if now > deadline + RESYNC_THRESHOLD {
            deadline = now + target;
        }
        self.deadline = Some(deadline);
        deadline.saturating_duration_since(now)
    }
}

/// Counts completed frames over a fixed window, for an fps figure that is measured rather than
/// assumed.
struct StatsWindow {
    started: Instant,
    frames: u32,
    last_published: Instant,
}

impl StatsWindow {
    fn new() -> Self {
        let now = Instant::now();
        Self {
            started: now,
            frames: 0,
            last_published: now,
        }
    }

    fn frame_completed(&mut self) {
        self.frames += 1;
    }

    fn due(&self) -> bool {
        self.last_published.elapsed() >= STATS_INTERVAL
    }

    fn fps(&self) -> f32 {
        let seconds = self.started.elapsed().as_secs_f32();
        if seconds <= 0.0 {
            0.0
        } else {
            self.frames as f32 / seconds
        }
    }

    fn window_closed(&mut self) {
        let now = Instant::now();
        self.started = now;
        self.last_published = now;
        self.frames = 0;
    }
}

/// Magic and version prefixing a save-state file written by this frontend.
///
/// # Why there is a wrapper at all
///
/// The frame counter a save state reports is the *frontend's* bookkeeping — the emulated machine
/// has no idea how many times `step_frame` has been called on it, and quite rightly. But "load to
/// exact frame" is a promise the save-state list makes to the user, and a list showing frame 300
/// that loads into a HUD reading frame 1800 has broken it.
///
/// So four bytes of magic and a little-endian `u64` sit in front of the versioned container the
/// `savestate` crate produces. Deliberately *not* smuggled into the system's own `Savable` output:
/// that would put a presentation concern into every system's state format and bump four
/// `state_version`s to add a field no emulated hardware has.
const STATE_MAGIC: &[u8; 4] = b"AEF1";
const STATE_HEADER_LEN: usize = STATE_MAGIC.len() + 8;

fn wrap_state(frame: u64, state: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(STATE_HEADER_LEN + state.len());
    out.extend_from_slice(STATE_MAGIC);
    out.extend_from_slice(&frame.to_le_bytes());
    out.extend_from_slice(state);
    out
}

/// Split a state file into its frame counter and the machine state.
///
/// A file without the magic is passed through whole with no frame — a state written by an older
/// build, or by hand. It still loads; only the counter is unknown, and reporting it as unknown is
/// better than reporting a number read out of the middle of a serialised CPU.
fn unwrap_state(bytes: &[u8]) -> (Option<u64>, &[u8]) {
    if bytes.len() >= STATE_HEADER_LEN && bytes.starts_with(STATE_MAGIC) {
        let mut frame = [0u8; 8];
        frame.copy_from_slice(&bytes[STATE_MAGIC.len()..STATE_HEADER_LEN]);
        (Some(u64::from_le_bytes(frame)), &bytes[STATE_HEADER_LEN..])
    } else {
        (None, bytes)
    }
}

/// Write through a temporary file and rename.
///
/// Save RAM and save states are both written this way. A save state is written while the game runs
/// and a crash mid-write would otherwise leave a truncated file that looks loadable and is not;
/// battery RAM is worse, because the truncated file is the player's entire progress.
fn write_atomically(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temp = path.with_extension("tmp");
    std::fs::write(&temp, bytes)?;
    std::fs::rename(&temp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_state_wrapper_round_trips_its_frame_counter() {
        let wrapped = wrap_state(1234, b"machine state");
        let (frame, state) = unwrap_state(&wrapped);
        assert_eq!(frame, Some(1234));
        assert_eq!(state, b"machine state");
    }

    #[test]
    fn a_state_without_the_wrapper_still_loads_with_an_unknown_frame() {
        let (frame, state) = unwrap_state(b"raw savestate bytes");
        assert_eq!(frame, None);
        assert_eq!(
            state, b"raw savestate bytes",
            "the whole file is the machine state, not a truncated tail of one"
        );
    }

    #[test]
    fn a_file_too_short_to_hold_a_header_is_not_misread_as_one() {
        // Four bytes of coincidentally-matching magic and nothing else.
        let (frame, state) = unwrap_state(STATE_MAGIC);
        assert_eq!(frame, None);
        assert_eq!(state, STATE_MAGIC);
    }

    #[test]
    fn a_pacer_accumulates_deadlines_rather_than_measuring_from_frame_end() {
        let mut pacer = Pacer::default();
        let start = Instant::now();
        let target = Duration::from_millis(16);

        // First call: nothing to catch up on, so sleep a full interval.
        assert_eq!(pacer.wait_for(target, start), target);

        // The frame took 10 ms. The next deadline is 32 ms after the start, so 6 ms remain —
        // not another 16.
        let after_work = start + Duration::from_millis(26);
        assert_eq!(pacer.wait_for(target, after_work), Duration::from_millis(6));
    }

    #[test]
    fn a_pacer_reports_no_sleep_when_already_behind() {
        let mut pacer = Pacer::default();
        let start = Instant::now();
        let target = Duration::from_millis(16);
        pacer.wait_for(target, start);

        // The frame took 20 ms, longer than its budget.
        let late = start + Duration::from_millis(36);
        assert_eq!(pacer.wait_for(target, late), Duration::ZERO);
    }

    #[test]
    fn a_pacer_gives_up_catching_up_after_a_long_stall() {
        let mut pacer = Pacer::default();
        let start = Instant::now();
        let target = Duration::from_millis(16);
        pacer.wait_for(target, start);

        // The machine was suspended for ten seconds. Catching that up would mean six hundred
        // frames at maximum speed.
        let much_later = start + Duration::from_secs(10);
        assert_eq!(pacer.wait_for(target, much_later), target);
    }

    #[test]
    fn an_uncapped_pacer_never_sleeps() {
        let mut pacer = Pacer::default();
        let now = Instant::now();
        assert_eq!(pacer.wait_for(Duration::ZERO, now), Duration::ZERO);
        assert_eq!(
            pacer.wait_for(Duration::ZERO, now + Duration::from_secs(1)),
            Duration::ZERO
        );
    }

    #[test]
    fn a_reset_pacer_starts_a_fresh_interval_rather_than_owing_time() {
        let mut pacer = Pacer::default();
        let start = Instant::now();
        let target = Duration::from_millis(16);
        pacer.wait_for(target, start);
        pacer.reset();

        // An hour of being paused owes nothing.
        let resumed = start + Duration::from_secs(3600);
        assert_eq!(pacer.wait_for(target, resumed), target);
    }
}
