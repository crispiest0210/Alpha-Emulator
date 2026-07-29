//! Breakpoints, watchpoints, and memory inspection.
//!
//! # Against the traits, not against the systems
//!
//! Prompt 15 requires this crate to depend on `core-common`'s introspection traits rather than
//! on any `system-*` crate, and the reason is that a debugger which knows about a particular
//! machine ends up with a branch per machine. Everything here works in terms of addresses,
//! values, and the [`CpuIntrospect`](core_common::CpuIntrospect) surface, so adding a fourth
//! system needs no change at all.
//!
//! # Inactive means free
//!
//! The acceptance bar is no measurable frame-time regression with nothing registered. That is
//! why [`Breakpoints::is_empty`] exists and why every other entry point begins by consulting it:
//! a system's hot loop tests one boolean, and a matcher that merely returns quickly would not
//! be good enough.
//!
//! # Status
//!
//! The breakpoint and watchpoint registry is done and tested. Not started: the `egui` panel
//! (which needs prompt 14's chrome to exist first), the GDB remote-serial-protocol subset, and
//! execution tracing. The disassemblers themselves already live with their CPU cores, where
//! `cpu-arm7tdmi` and `cpu-sm83` can keep them in step with the decoders they mirror.

#![deny(unsafe_code)]

pub mod breakpoints;

pub use breakpoints::{AccessKind, Breakpoints, Condition, Trigger, Watchpoint};
