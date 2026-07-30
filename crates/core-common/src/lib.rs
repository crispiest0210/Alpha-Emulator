//! Shared, system-agnostic emulation primitives.
//!
//! This crate is the contract every other crate in the workspace is written against: the
//! [`Scheduler`] that owns "what happens next and when", the [`Bus`]/[`MemoryRegion`] traits
//! that memory maps are built from, the [`Cpu`] trait the four CPU cores implement, and the
//! [`System`] trait the four platform crates implement and the frontends consume.
//!
//! # Where `Savable` lives
//!
//! Prompt 02 left this to whichever placement avoids a circular dependency. [`Savable`] is
//! defined in the `savestate` crate and `core-common` depends on it, because `savestate`
//! needs nothing from here — it is pure serialization — while [`Cpu`], [`Bus`], and
//! [`System`] all carry it as a supertrait bound so no implementer can forget save-state
//! support. The dependency therefore runs `core-common -> savestate`, and `savestate` is a
//! leaf crate.
//!
//! # What does not belong here
//!
//! No platform-specific behavior, and no `if system == Gb` anywhere. Hardware quirks — the
//! GBA's prefetch buffer, the DS's bus arbitration, the Game Boy's HALT bug — are built *on
//! top of* these primitives in the CPU, memory, and system crates. A quirk that leaks into
//! this crate is a leaky abstraction by definition, since every other system then pays for it.
//!
//! # Dependency rule
//!
//! Nothing here may depend on `winit`, `wgpu`, `egui`, or `cpal`. This is enforced by
//! `cargo deny check bans` in CI, not by review. The emulation core stays a pure library
//! consumable by the native frontend, the headless CLI, the test harness, and any future
//! frontend.

#![deny(unsafe_code)]

pub mod bus;
pub mod cpu;
pub mod debug;
pub mod event_types;
pub mod logging;
pub mod scheduler;
pub mod system;

#[cfg(test)]
mod mock_system;

pub use bus::{Addr, Bus, MapError, MemoryRegion, Ram, RegionMap};
pub use cpu::{Cpu, CpuIntrospect, DisasmInstruction, Disassemble, RegisterValue};
pub use debug::{Access, AccessKind, AccessLog, DebugRegion, DebugTarget};
pub use event_types::{
    AudioSample, Buttons, Cycles, Framebuffer, InputState, Rgba8, TouchPoint, AUDIO_SAMPLE_RATE,
    BYTES_PER_PIXEL,
};
pub use scheduler::{EventHandle, Scheduler};
pub use system::{CartridgeError, FrameOutput, System};

/// Re-exported so implementers get the save-state API from one place and never have to think
/// about which crate `Savable` lives in.
pub use savestate::{Savable, StateError, StateReader, StateWriter};
