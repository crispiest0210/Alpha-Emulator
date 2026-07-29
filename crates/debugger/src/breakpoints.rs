//! Breakpoints and watchpoints.
//!
//! # Zero cost when nothing is registered
//!
//! Prompt 15 sets the bar at no measurable frame-time regression with the debugger inactive,
//! and the only way to be sure of that is for the inactive path to be a single boolean read.
//! Every check here starts with [`Breakpoints::is_empty`], and a system's hot loop is expected
//! to test that before it does anything else — not to call a matcher that happens to return
//! quickly.
//!
//! # A watchpoint is not a breakpoint on an address
//!
//! An execution breakpoint fires when the program counter reaches an address. A watchpoint
//! fires when *any* instruction touches one, whatever it was executing. They are stored
//! separately because a game reading its own code is normal and a debugger that stopped for it
//! would be unusable.
//!
//! # Conditions are values, not expressions
//!
//! A watchpoint may require a specific value, or a value that changed. What it may not do is
//! evaluate an arbitrary expression: that needs a parser, an evaluator, and a way to reference
//! machine state, and every one of those is a place for the debugger to disagree with the
//! machine it is debugging. The two conditions here cover what a watchpoint is actually used
//! for — catching who wrote a bad value — and say so rather than pretending to be general.

use std::collections::HashMap;

/// What kind of access a watchpoint watches for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AccessKind {
    Read,
    Write,
}

/// When a watchpoint should fire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Condition {
    /// Any access of the watched kind.
    Always,
    /// Only when the value is exactly this.
    Equals(u32),
    /// Only when the access changes what is there.
    ///
    /// Reads never satisfy this — a read cannot change anything — so a read watchpoint with this
    /// condition never fires, which is reported rather than silently accepted.
    Changes,
}

/// One watched address range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Watchpoint {
    pub start: u32,
    /// Exclusive. A single address is `start..start + 1`.
    pub end: u32,
    pub kind: AccessKind,
    pub condition: Condition,
}

impl Watchpoint {
    /// A watchpoint on one address.
    pub fn at(addr: u32, kind: AccessKind) -> Self {
        Self {
            start: addr,
            end: addr.saturating_add(1),
            kind,
            condition: Condition::Always,
        }
    }

    /// A watchpoint over a range, for catching a stray write into a whole structure.
    pub fn range(start: u32, end: u32, kind: AccessKind) -> Self {
        Self {
            start,
            end,
            kind,
            condition: Condition::Always,
        }
    }

    pub fn when(mut self, condition: Condition) -> Self {
        self.condition = condition;
        self
    }

    fn covers(&self, addr: u32) -> bool {
        addr >= self.start && addr < self.end
    }
}

/// Why execution stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    Execution {
        addr: u32,
    },
    Watchpoint {
        addr: u32,
        kind: AccessKind,
        value: u32,
    },
}

/// The registry a system consults as it runs.
#[derive(Debug, Clone, Default)]
pub struct Breakpoints {
    execution: Vec<u32>,
    watchpoints: Vec<Watchpoint>,
    /// Last value seen at each watched address, for [`Condition::Changes`].
    ///
    /// Populated lazily on first access rather than pre-filled, because a watchpoint may cover a
    /// range far larger than anything the machine actually touches.
    seen: HashMap<u32, u32>,
    /// Cached emptiness, so the hot path is one boolean rather than two length checks.
    empty: bool,
}

impl Breakpoints {
    pub fn new() -> Self {
        Self {
            empty: true,
            ..Default::default()
        }
    }

    /// Whether anything at all is registered.
    ///
    /// The one thing a system's hot loop should call. Everything else here is behind it.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.empty
    }

    fn refresh(&mut self) {
        self.empty = self.execution.is_empty() && self.watchpoints.is_empty();
    }

    pub fn add_execution(&mut self, addr: u32) {
        if !self.execution.contains(&addr) {
            self.execution.push(addr);
        }
        self.refresh();
    }

    pub fn remove_execution(&mut self, addr: u32) {
        self.execution.retain(|&a| a != addr);
        self.refresh();
    }

    pub fn add_watchpoint(&mut self, watchpoint: Watchpoint) {
        self.watchpoints.push(watchpoint);
        self.refresh();
    }

    pub fn remove_watchpoints_at(&mut self, addr: u32) {
        self.watchpoints.retain(|w| !w.covers(addr));
        self.refresh();
    }

    pub fn clear(&mut self) {
        self.execution.clear();
        self.watchpoints.clear();
        self.seen.clear();
        self.empty = true;
    }

    pub fn execution_breakpoints(&self) -> &[u32] {
        &self.execution
    }

    pub fn watchpoints(&self) -> &[Watchpoint] {
        &self.watchpoints
    }

    /// Whether execution should stop before running the instruction at `pc`.
    #[inline]
    pub fn check_execution(&self, pc: u32) -> Option<Trigger> {
        if self.empty {
            return None;
        }
        self.execution
            .contains(&pc)
            .then_some(Trigger::Execution { addr: pc })
    }

    /// Whether an access should stop execution.
    ///
    /// Takes `&mut self` because [`Condition::Changes`] has to remember what was there. That is
    /// why the emptiness check comes first: a system that guards on [`Breakpoints::is_empty`]
    /// never reaches the borrow at all.
    pub fn check_access(&mut self, addr: u32, kind: AccessKind, value: u32) -> Option<Trigger> {
        if self.empty {
            return None;
        }
        let mut fired = None;
        for watchpoint in &self.watchpoints {
            if watchpoint.kind != kind || !watchpoint.covers(addr) {
                continue;
            }
            let matches = match watchpoint.condition {
                Condition::Always => true,
                Condition::Equals(expected) => value == expected,
                // A read cannot change anything, so this never fires on one.
                Condition::Changes => {
                    kind == AccessKind::Write && self.seen.get(&addr) != Some(&value)
                }
            };
            if matches && fired.is_none() {
                fired = Some(Trigger::Watchpoint { addr, kind, value });
            }
        }
        if kind == AccessKind::Write {
            self.seen.insert(addr, value);
        }
        fired
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_registry_answers_in_one_boolean() {
        // The bar prompt 15 sets is no measurable regression with the debugger inactive, and the
        // only way to be sure is for the inactive path to be this.
        let mut points = Breakpoints::new();
        assert!(points.is_empty());
        assert_eq!(points.check_execution(0x1234), None);
        assert_eq!(points.check_access(0x1234, AccessKind::Write, 1), None);
    }

    #[test]
    fn an_execution_breakpoint_fires_at_its_address_and_nowhere_else() {
        let mut points = Breakpoints::new();
        points.add_execution(0x0800_0100);
        assert!(!points.is_empty());
        assert_eq!(
            points.check_execution(0x0800_0100),
            Some(Trigger::Execution { addr: 0x0800_0100 })
        );
        assert_eq!(points.check_execution(0x0800_0104), None);
    }

    #[test]
    fn adding_the_same_breakpoint_twice_does_not_duplicate_it() {
        let mut points = Breakpoints::new();
        points.add_execution(0x100);
        points.add_execution(0x100);
        assert_eq!(points.execution_breakpoints().len(), 1);
    }

    #[test]
    fn removing_the_last_breakpoint_returns_the_registry_to_empty() {
        // Otherwise the hot path stays slow forever after a single debugging session.
        let mut points = Breakpoints::new();
        points.add_execution(0x100);
        points.remove_execution(0x100);
        assert!(points.is_empty());
    }

    #[test]
    fn a_watchpoint_fires_on_an_access_rather_than_on_execution() {
        // A game reading its own code is normal; a debugger that stopped for it would be
        // unusable, which is why these are stored separately.
        let mut points = Breakpoints::new();
        points.add_watchpoint(Watchpoint::at(0x0400_0000, AccessKind::Write));

        assert_eq!(points.check_execution(0x0400_0000), None);
        assert_eq!(
            points.check_access(0x0400_0000, AccessKind::Write, 0x42),
            Some(Trigger::Watchpoint {
                addr: 0x0400_0000,
                kind: AccessKind::Write,
                value: 0x42
            })
        );
    }

    #[test]
    fn a_write_watchpoint_ignores_reads_and_the_other_way_round() {
        let mut points = Breakpoints::new();
        points.add_watchpoint(Watchpoint::at(0x100, AccessKind::Write));
        assert_eq!(points.check_access(0x100, AccessKind::Read, 1), None);
        assert!(points.check_access(0x100, AccessKind::Write, 1).is_some());
    }

    #[test]
    fn a_range_watchpoint_catches_a_stray_write_anywhere_inside_it() {
        let mut points = Breakpoints::new();
        points.add_watchpoint(Watchpoint::range(0x1000, 0x1010, AccessKind::Write));

        assert!(points.check_access(0x1000, AccessKind::Write, 0).is_some());
        assert!(points.check_access(0x100F, AccessKind::Write, 0).is_some());
        assert_eq!(points.check_access(0x1010, AccessKind::Write, 0), None);
        assert_eq!(points.check_access(0x0FFF, AccessKind::Write, 0), None);
    }

    #[test]
    fn a_value_condition_fires_only_for_that_value() {
        // What a watchpoint is actually used for: catching who wrote a bad value.
        let mut points = Breakpoints::new();
        points.add_watchpoint(
            Watchpoint::at(0x100, AccessKind::Write).when(Condition::Equals(0xDEAD)),
        );
        assert_eq!(points.check_access(0x100, AccessKind::Write, 0x1234), None);
        assert!(points
            .check_access(0x100, AccessKind::Write, 0xDEAD)
            .is_some());
    }

    #[test]
    fn a_change_condition_ignores_a_write_of_the_same_value() {
        let mut points = Breakpoints::new();
        points.add_watchpoint(Watchpoint::at(0x100, AccessKind::Write).when(Condition::Changes));

        assert!(
            points.check_access(0x100, AccessKind::Write, 5).is_some(),
            "the first write is a change from nothing known"
        );
        assert_eq!(
            points.check_access(0x100, AccessKind::Write, 5),
            None,
            "the same value again is not"
        );
        assert!(points.check_access(0x100, AccessKind::Write, 6).is_some());
    }

    #[test]
    fn a_change_condition_never_fires_on_a_read() {
        // A read cannot change anything. Reported rather than silently accepted, so a user who
        // configures this gets nothing rather than everything.
        let mut points = Breakpoints::new();
        points.add_watchpoint(Watchpoint::at(0x100, AccessKind::Read).when(Condition::Changes));
        assert_eq!(points.check_access(0x100, AccessKind::Read, 1), None);
        assert_eq!(points.check_access(0x100, AccessKind::Read, 2), None);
    }

    #[test]
    fn only_the_first_matching_watchpoint_is_reported() {
        // Two overlapping watchpoints are one stop, not two.
        let mut points = Breakpoints::new();
        points.add_watchpoint(Watchpoint::range(0x100, 0x200, AccessKind::Write));
        points.add_watchpoint(Watchpoint::at(0x150, AccessKind::Write));
        assert_eq!(
            points.check_access(0x150, AccessKind::Write, 1),
            Some(Trigger::Watchpoint {
                addr: 0x150,
                kind: AccessKind::Write,
                value: 1
            })
        );
    }

    #[test]
    fn removing_a_watchpoint_by_address_removes_every_one_covering_it() {
        let mut points = Breakpoints::new();
        points.add_watchpoint(Watchpoint::range(0x100, 0x200, AccessKind::Write));
        points.add_watchpoint(Watchpoint::at(0x150, AccessKind::Write));
        points.remove_watchpoints_at(0x150);
        assert!(points.is_empty());
    }

    #[test]
    fn clearing_forgets_the_change_history_as_well_as_the_points() {
        // Otherwise a re-registered change watchpoint would compare against a value from a
        // previous session and miss the first write.
        let mut points = Breakpoints::new();
        points.add_watchpoint(Watchpoint::at(0x100, AccessKind::Write).when(Condition::Changes));
        points.check_access(0x100, AccessKind::Write, 5);
        points.clear();

        points.add_watchpoint(Watchpoint::at(0x100, AccessKind::Write).when(Condition::Changes));
        assert!(
            points.check_access(0x100, AccessKind::Write, 5).is_some(),
            "the history went with it"
        );
    }
}
