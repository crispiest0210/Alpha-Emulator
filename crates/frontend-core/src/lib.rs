//! Frontend-agnostic session runtime.
//!
//! Owns everything a frontend needs that is not windowing or GPU work: the emulation thread, the
//! audio pipeline, input routing, the rewind buffer, settings, and the library integration.
//! Consumed by both `frontend-native` and `frontend-headless`, which is why nothing here may
//! touch `winit`, `wgpu`, `egui`, or `cpal`.
//!
//! # The shape of it
//!
//! ```text
//!   frontend            frontend-core                        emulation thread
//!  ┌──────────┐  command  ┌─────────┐                       ┌────────────────┐
//!  │ UI thread├──────────►│ Session ├──── commands ────────►│  Box<dyn System>│
//!  │          │◄──────────┤         │◄─── events ───────────┤  rewind buffer  │
//!  │          │  events   └─────────┘◄─── frames ───────────┤  save RAM flush │
//!  └────┬─────┘                                             └────────┬───────┘
//!       │ input (one atomic word, latest-wins) ─────────────────────►│
//!       │                                                            │
//!  audio callback ◄──────── lock-free ring ◄─────────────────────────┘
//! ```
//!
//! Three channels, three different policies, each chosen for what it carries:
//!
//! - **commands and events** are unbounded — every one matters and neither side may block;
//! - **frames** are a bounded pool with a return path, newest-wins, dropped when the drawing
//!   thread is behind ([`frame`]);
//! - **input** is a single atomic word, latest-wins, because stale input is worthless
//!   ([`input::input_channel`]);
//! - **audio** is a lock-free SPSC ring the callback never blocks on ([`audio`]).
//!
//! Getting these four policies right is most of what this crate is for. The predecessor project
//! ran emulation, rendering, and audio on one thread and had no answer for any of them.
//!
//! # Status
//!
//! Complete for prompt 14: [`session`] and [`emulation`] run any system the workspace implements,
//! [`config`] persists settings as TOML, [`catalog`] connects the SQLite library index to the
//! cartridge headers, and [`platform`] is the one place that knows which system a file needs.
//!
//! The session also serves prompt 15's debugger: snapshots on request, instruction stepping, and
//! execution breakpoints that halt — the last of those by stepping one instruction at a time while
//! the debugger is attached and checking the registry between steps, so no system crate has a hook
//! in its hot loop and a detached session runs exactly as it did.
//!
//! Not done: gamepad input (the keybind layer in [`input`] is keyboard-only by construction —
//! [`input::PhysicalKey`] would need a sibling for pads); touch input has a delivery path but no
//! producer until prompt 13 gives it a DS to point at; and while a breakpoint is set, joypad input is
//! not delivered, because `InputState` applies to a whole frame and there is no per-instruction path
//! to hand it through.
//!
//! # Dependency rule
//!
//! No `winit`, `wgpu`, `egui`, or `cpal`, enforced by `cargo deny check bans`. The audio pipeline
//! here ends at a ring buffer; `frontend-native` opens the device and does nothing in the callback
//! but drain it.

#![deny(unsafe_code)]

pub mod audio;
pub mod catalog;
pub mod config;
mod emulation;
pub mod frame;
pub mod input;
pub mod platform;
pub mod png;
pub mod session;

pub use audio::{channel, AudioConsumer, AudioProducer, Resampler, DEFAULT_CAPACITY};
pub use config::{
    AudioConfig, Config, EmulationConfig, RewindConfig, ScalingMode, ThemeChoice, VideoConfig,
};
pub use frame::{frame_pipe, Frame, FramePublisher, FrameSubscriber};
pub use input::{
    input_channel, Action, BindError, ChromeAction, InputReceiver, InputSender, InputTracker,
    KeybindMap, PhysicalInputEvent, PhysicalKey,
};
pub use platform::{frame_duration, frame_rate, is_dual_screen, screen_size, LoadError, RomInfo};
pub use png::encode_png;
pub use session::{
    LoadedRom, SavedState, Session, SessionCommand, SessionEvent, SessionOptions, SessionStats,
    SessionStatus,
};

/// Re-exported so a frontend needs one dependency for platform naming rather than reaching past
/// this crate into `library` for an enum it uses in every match.
pub use library::Platform;

/// The debugger's snapshot types, for the same reason: the panel that renders them consumes this
/// crate's session API, and making it depend on `debugger` separately would let it reach the
/// registry the emulation thread owns.
pub use debugger::{
    AccessKind, Condition, DisasmLine, MemoryRow, Request as DebugRequest,
    Snapshot as DebugSnapshot, Watchpoint,
};

#[cfg(test)]
mod tests;
