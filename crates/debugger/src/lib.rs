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
//! **Execution breakpoints and watchpoints both halt**, by two different mechanisms, because they
//! are two different problems:
//!
//! - An **execution breakpoint** needs the program counter at instruction boundaries, so the session
//!   steps one instruction at a time while attached and checks
//!   [`check_execution`](Breakpoints::check_execution) between steps. No system crate learns that
//!   breakpoints exist, and a detached session pays literally nothing — not even a branch.
//! - A **watchpoint** needs every bus access, and only the bus sees those. So each system's bus owns
//!   a [`AccessLog`](core_common::AccessLog) that records when armed, and the session drains it after
//!   each instruction and asks [`check_access`](Breakpoints::check_access) about each entry. The bus
//!   records; it does not decide. It holds no addresses, knows nothing about watchpoints, and cannot
//!   stop execution — so the policy still lives above the systems.
//!
//! The asymmetry is worth naming: the second mechanism costs one branch per bus access whether or not
//! anything is watching, and the first costs nothing at all. That is why watchpoints needed a touch
//! point in each bus and breakpoints did not, and it is the cost prompt 18 should measure rather than
//! the one this crate asserts.
//!
//! Still not started: the GDB remote-serial-protocol subset and execution tracing.
//!
//! The disassemblers themselves live with their CPU cores, where `cpu-arm7tdmi` and `cpu-sm83` can
//! keep them in step with the decoders they mirror; each system's `DebugTarget` picks the right one,
//! which on the GBA means reading the T bit rather than guessing.

#![deny(unsafe_code)]

pub mod breakpoints;
pub mod view;

pub use breakpoints::{AccessKind, Breakpoints, Condition, Trigger, Watchpoint};
pub use view::{capture, DisasmLine, MemoryRow, Request, Snapshot, BYTES_PER_ROW};
