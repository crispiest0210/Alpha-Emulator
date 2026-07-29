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
//! Done: the breakpoint and watchpoint registry, and [`view`] — the snapshot a debugger panel
//! renders, built from [`DebugTarget`](core_common::DebugTarget) and so with no branch per system.
//! `frontend-native` has the panel; `frontend-core`'s session serves the snapshots and single-steps.
//!
//! **Execution breakpoints halt; watchpoints do not yet.** The session steps instruction by
//! instruction while a debugger is attached and checks
//! [`check_execution`](Breakpoints::check_execution) between steps, which is how breakpoints work
//! without a hook inside any system's hot loop — and why ordinary play pays literally nothing rather
//! than paying a branch. Watchpoints need bus-access interception, which that trick cannot provide:
//! [`check_access`](Breakpoints::check_access) is implemented and tested, and nothing calls it from
//! a running machine yet. That is prompt 15's remaining work along with the GDB
//! remote-serial-protocol subset and execution tracing.
//!
//! The disassemblers themselves live with their CPU cores, where `cpu-arm7tdmi` and `cpu-sm83` can
//! keep them in step with the decoders they mirror; each system's `DebugTarget` picks the right one,
//! which on the GBA means reading the T bit rather than guessing.

#![deny(unsafe_code)]

pub mod breakpoints;
pub mod view;

pub use breakpoints::{AccessKind, Breakpoints, Condition, Trigger, Watchpoint};
pub use view::{capture, DisasmLine, MemoryRow, Request, Snapshot, BYTES_PER_ROW};
