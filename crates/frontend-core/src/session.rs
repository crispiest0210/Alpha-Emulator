//! The command/event boundary between a frontend and the emulation thread.
//!
//! A frontend holds a [`Session`], sends [`SessionCommand`]s, reads [`SessionEvent`]s, and polls
//! for finished frames. It never touches a [`System`](core_common::System), never takes a lock,
//! and never blocks — which is the entire contract, and the reason a second frontend (web, TUI)
//! could be written against this crate without touching the one that exists.
//!
//! # What is deliberately *not* here
//!
//! The library index. The emulation thread writes save-state *files* and reports where it put
//! them; the frontend's thread owns the [`Library`](library::Library) and records the row. A
//! `rusqlite::Connection` shared between two threads would need a mutex, and the one thread that
//! must never wait on a lock is the one with a 16.7 ms deadline. See [`crate::catalog`] for the
//! other half of that split.
//!
//! # Why events and frames travel separately
//!
//! Events are rare, must all be seen, and are cheap; frames are frequent, only the newest
//! matters, and cost 150 KiB each. A single channel would force one policy on both — either
//! events get dropped with stale frames, or frames queue up and the display runs in slow motion.
//! See [`crate::frame`].

use crate::audio::AudioProducer;
use crate::config::{Config, RewindConfig};
use crate::frame::{frame_pipe, FrameSubscriber, DEFAULT_DEPTH};
use crate::input::{input_channel, InputSender};
use core_common::InputState;
use crossbeam_channel::{Receiver, Sender};
use debugger::{Snapshot, Watchpoint};
use library::{AppPaths, Platform, RomId};
use std::path::PathBuf;
use std::thread::JoinHandle;

/// What the emulation thread is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionStatus {
    /// No cartridge. The frontend shows its library.
    Idle,
    Running,
    Paused,
    /// The emulated machine asked to stop — a Game Boy `STOP` that nothing released, or a
    /// power-off. Distinct from `Paused`: the user did not do this and resuming is not
    /// meaningful without a reset.
    Stopped,
}

impl SessionStatus {
    pub const fn label(self) -> &'static str {
        match self {
            SessionStatus::Idle => "no cartridge",
            SessionStatus::Running => "running",
            SessionStatus::Paused => "paused",
            SessionStatus::Stopped => "stopped",
        }
    }
}

/// What is currently loaded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedRom {
    pub path: PathBuf,
    /// The library row, when the ROM came from the library rather than a drag-and-drop of a file
    /// that was never imported.
    pub rom_id: Option<RomId>,
    pub platform: Platform,
    pub title: String,
    pub width: u32,
    pub height: u32,
    /// Whether battery-backed save RAM was restored from disk at load time.
    pub save_ram_restored: bool,
}

/// A save state that has just been written to disk.
///
/// Carries everything the library needs for its row, so the frontend can index it without
/// re-opening the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedState {
    pub rom_id: Option<RomId>,
    pub path: PathBuf,
    pub label: String,
    pub slot: Option<u8>,
    pub frame: u64,
    pub size_bytes: u64,
}

/// Periodic measurements for the HUD.
///
/// All measured, none estimated. `speed_percent` in particular is emulated frames actually
/// completed against the platform's real frame rate, so a machine that cannot keep up reports
/// less than 100 rather than claiming to be fine.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct SessionStats {
    pub frame: u64,
    pub fps: f32,
    pub speed_percent: f32,
    /// Audio samples the ring could not accept. Expected to climb during fast-forward and to
    /// stay flat otherwise.
    pub audio_dropped: u64,
    /// Frames the drawing thread was too slow to collect.
    pub frames_dropped: u64,
    pub rewind_snapshots: usize,
    pub rewind_span_frames: u64,
    pub rewind_bytes: usize,
    pub fast_forward: bool,
    pub rewinding: bool,
}

/// Instructions to the emulation thread.
#[derive(Debug, Clone, PartialEq)]
pub enum SessionCommand {
    /// Load a ROM from disk, replacing whatever is running. Flushes the outgoing cartridge's
    /// save RAM first.
    LoadRom {
        path: PathBuf,
        rom_id: Option<RomId>,
    },
    /// Unload, flushing save RAM.
    CloseRom,
    /// Power-cycle, keeping the cartridge and its save RAM.
    Reset,
    SetPaused(bool),
    TogglePause,
    /// Advance exactly `n` frames while paused. The debugger's step button, and how a screenshot
    /// of a specific frame gets taken.
    StepFrames(u32),
    /// Write a save state. `slot` picks a numbered quick-save; `label` names a keepsake state.
    /// With neither, slot 0 is used, which is what a quicksave key does.
    SaveState {
        slot: Option<u8>,
        label: Option<String>,
    },
    /// Load a specific state file.
    LoadState {
        path: PathBuf,
    },
    /// Load a numbered quick-save slot for the running ROM.
    LoadSlot(u8),
    SetFastForward(bool),
    SetRewinding(bool),
    SetVolume(f32),
    SetMuted(bool),
    SetFastForwardSpeed(f32),
    /// Change rewind depth. Takes effect on the next ROM load, and resizes immediately when it
    /// can be done without discarding history.
    SetRewindConfig(RewindConfig),
    /// The host audio device's sample rate, once it is known or when it changes.
    SetAudioOutputRate(u32),
    /// Write dirty save RAM now rather than waiting for the debounce.
    FlushSaveRam,

    // --- debugger -------------------------------------------------------------------------
    /// Attach or detach the debugger.
    ///
    /// While attached the loop runs one instruction at a time so breakpoints can be checked
    /// between them, which costs perhaps a third of the machine's speed. Detached, it runs
    /// `step_frame` exactly as before and the debugger costs *nothing* — not a branch, not a null
    /// check. That is the whole reason attachment is explicit rather than implied by having a
    /// breakpoint set.
    SetDebugAttached(bool),
    /// Ask for a fresh [`Snapshot`]. Answered with [`SessionEvent::DebugSnapshot`].
    RequestDebugSnapshot(debugger::Request),
    AddBreakpoint(u32),
    RemoveBreakpoint(u32),
    ClearBreakpoints,
    /// Watch an address or range for reads or writes.
    ///
    /// Unlike an execution breakpoint this needs the bus to be recording, so it is refused with
    /// [`SessionEvent::DebugUnavailable`] on a system whose bus does not.
    AddWatchpoint(Watchpoint),
    RemoveWatchpointsAt(u32),
    /// Advance exactly `n` instructions, then stop. Implies a pause.
    StepInstructions(u32),
    /// The debugger's "set next statement".
    SetProgramCounter(u32),
    /// Stop the thread. Sent automatically when the `Session` is dropped.
    Shutdown,
}

/// Notifications from the emulation thread.
#[derive(Debug, Clone, PartialEq)]
pub enum SessionEvent {
    RomLoaded(LoadedRom),
    RomClosed,
    StatusChanged(SessionStatus),
    Stats(SessionStats),
    StateSaved(SavedState),
    StateLoaded {
        path: PathBuf,
        frame: u64,
    },
    SaveRamWritten {
        path: PathBuf,
    },
    /// Something worth telling the user that is not a failure — "rewound to frame 1200",
    /// "cannot rewind further".
    Notice(String),
    Error(String),

    /// A debugger snapshot, in answer to [`SessionCommand::RequestDebugSnapshot`].
    ///
    /// Boxed because it carries a few kilobytes of disassembly and hex rows, and every other event
    /// in this enum is a handful of bytes — an unboxed variant would make the whole enum that large
    /// for the sixty-times-a-second ones too.
    DebugSnapshot(Box<Snapshot>),
    /// Execution stopped at a breakpoint. The session is paused when this arrives.
    BreakpointHit {
        addr: u32,
    },
    /// A watched address was accessed. The session is paused when this arrives.
    ///
    /// Reported *after* the instruction that did it, because the access has already happened — where
    /// an execution breakpoint stops before its instruction runs.
    WatchpointHit {
        addr: u32,
        write: bool,
        value: u8,
    },
    /// The debugger asked for something this machine cannot offer.
    DebugUnavailable(String),
}

/// How to start a session.
pub struct SessionOptions {
    pub paths: AppPaths,
    pub config: Config,
    /// The producing end of the audio ring. `None` runs with no audio at all, which is what
    /// `frontend-headless` and the tests do — audio samples are then drained and discarded, so
    /// the emulation path is identical either way.
    pub audio: Option<AudioProducer>,
    /// The host device's rate. Ignored when `audio` is `None`.
    pub audio_output_rate: u32,
    /// Frames that may be in flight to the drawing thread.
    pub frame_depth: usize,
}

impl SessionOptions {
    pub fn new(paths: AppPaths, config: Config) -> Self {
        Self {
            paths,
            config,
            audio: None,
            audio_output_rate: core_common::AUDIO_SAMPLE_RATE,
            frame_depth: DEFAULT_DEPTH,
        }
    }

    pub fn with_audio(mut self, producer: AudioProducer, output_rate: u32) -> Self {
        self.audio = Some(producer);
        self.audio_output_rate = output_rate;
        self
    }
}

/// A running emulation thread and the channels to it.
pub struct Session {
    commands: Sender<SessionCommand>,
    events: Receiver<SessionEvent>,
    frames: FrameSubscriber,
    input: InputSender,
    thread: Option<JoinHandle<()>>,
}

impl Session {
    /// Start the emulation thread.
    pub fn spawn(options: SessionOptions) -> Self {
        let (command_tx, command_rx) = crossbeam_channel::unbounded();
        // Events are unbounded on purpose. A bounded event channel would make the emulation
        // thread block when the UI is busy, which is the one thing this design exists to
        // prevent; the volume is a handful per second, so unbounded is not a memory risk.
        let (event_tx, event_rx) = crossbeam_channel::unbounded();
        let (publisher, subscriber) = frame_pipe(options.frame_depth);
        let (input_tx, input_rx) = input_channel();

        let thread = std::thread::Builder::new()
            .name("emulation".into())
            .spawn(move || {
                crate::emulation::run(options, command_rx, event_tx, publisher, input_rx);
            })
            .expect("the OS refused to start the emulation thread");

        Self {
            commands: command_tx,
            events: event_rx,
            frames: subscriber,
            input: input_tx,
            thread: Some(thread),
        }
    }

    /// Send a command. Silently ignored if the thread has already exited, which only happens
    /// during shutdown.
    pub fn send(&self, command: SessionCommand) {
        let _ = self.commands.send(command);
    }

    /// Publish the current input state. Latest-wins; see [`crate::input::input_channel`].
    pub fn set_input(&self, state: InputState) {
        self.input.send(state);
    }

    pub fn input(&self) -> &InputSender {
        &self.input
    }

    /// Take one pending event, or `None`. Never blocks.
    pub fn poll_event(&self) -> Option<SessionEvent> {
        self.events.try_recv().ok()
    }

    /// Drain every pending event.
    pub fn drain_events(&self) -> Vec<SessionEvent> {
        self.events.try_iter().collect()
    }

    /// Frames from the emulation thread. Call [`poll`](FrameSubscriber::poll) once per redraw.
    pub fn frames(&mut self) -> &mut FrameSubscriber {
        &mut self.frames
    }

    /// Whether the emulation thread is still alive.
    pub fn is_alive(&self) -> bool {
        self.thread.as_ref().is_some_and(|t| !t.is_finished())
    }

    /// Stop the thread and wait for it, so save RAM is on disk before the process exits.
    pub fn shutdown(mut self) {
        self.stop();
    }

    fn stop(&mut self) {
        let _ = self.commands.send(SessionCommand::Shutdown);
        if let Some(thread) = self.thread.take() {
            // A panicked emulation thread must not take the frontend down with it during a
            // window close; the panic has already been reported by the default hook.
            if thread.join().is_err() {
                tracing::error!("the emulation thread panicked");
            }
        }
    }
}

/// Shutting down on drop is what makes save RAM durable without the frontend having to remember.
///
/// Emulators that only flush on a clean exit path lose saves whenever that path is not taken, and
/// "the window closed" is not a clean exit path in any windowing library.
impl Drop for Session {
    fn drop(&mut self) {
        self.stop();
    }
}
