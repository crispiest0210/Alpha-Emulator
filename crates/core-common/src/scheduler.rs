//! The event scheduler that drives everything time-sensitive in the core.
//!
//! # Why event-driven rather than fixed-step polling
//!
//! The predecessor project ran a plain fixed-step loop and had no single place that owned
//! "what happens next and when", which made timing bugs hard to localize. More concretely, a
//! fixed-step design forces every subsystem onto one tick granularity — effectively the LCM
//! of every period in the machine — so you either tick idle subsystems constantly or you
//! miss events that land between ticks.
//!
//! Here, subsystems schedule their next transition (PPU mode change, timer overflow, DMA
//! completion, APU frame sequencer step) at an absolute [`Cycles`] timestamp, and the CPU
//! runs in slices bounded by [`Scheduler::cycles_until_next`]. Nothing idle costs anything,
//! and events land on the exact cycle they were scheduled for regardless of instruction
//! boundaries.
//!
//! # The event type is yours
//!
//! [`Scheduler`] is generic over `E`, so each system crate defines its own event enum and
//! `core-common` never learns what a "PPU HBlank" is. Monomorphization means this costs
//! nothing at runtime versus a hardcoded global enum.

use crate::Cycles;
use savestate::{Savable, SavableInit, StateError, StateReader, StateWriter};
use std::collections::BinaryHeap;

/// Identifies one scheduled event so it can be cancelled before it fires.
///
/// Handles are never reused within a scheduler's lifetime, so a stale handle from an event
/// that already fired can never cancel an unrelated later event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EventHandle(u64);

impl EventHandle {
    /// A handle that never matches a live event. Useful as a "nothing pending" placeholder
    /// in subsystem structs, so they don't need `Option<EventHandle>` everywhere.
    pub const NONE: EventHandle = EventHandle(u64::MAX);

    #[inline]
    pub const fn raw(self) -> u64 {
        self.0
    }
}

#[derive(Debug, Clone)]
struct Entry<E> {
    when: Cycles,
    seq: u64,
    event: E,
}

impl<E> PartialEq for Entry<E> {
    fn eq(&self, other: &Self) -> bool {
        self.when == other.when && self.seq == other.seq
    }
}

impl<E> Eq for Entry<E> {}

impl<E> PartialOrd for Entry<E> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl<E> Ord for Entry<E> {
    /// Reversed on purpose: `BinaryHeap` is a max-heap, so "greater" must mean "sooner" for
    /// `pop` to yield the earliest event.
    ///
    /// Ties on `when` break by insertion order (lower `seq` first), which makes the whole
    /// scheduler deterministic — two events scheduled for the same cycle always fire in the
    /// order they were scheduled, on every run and every platform. Save states depend on
    /// this: a replayed state must resolve simultaneous events identically.
    ///
    /// Note this ignores `event` entirely, so `E` needs no `Ord` bound.
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        other
            .when
            .cmp(&self.when)
            .then_with(|| other.seq.cmp(&self.seq))
    }
}

/// A min-heap of `(when, event)` pairs, ordered by timestamp then insertion order.
#[derive(Debug, Clone)]
pub struct Scheduler<E> {
    heap: BinaryHeap<Entry<E>>,
    next_seq: u64,
}

impl<E> Default for Scheduler<E> {
    fn default() -> Self {
        Self::new()
    }
}

impl<E> Scheduler<E> {
    pub fn new() -> Self {
        Self {
            heap: BinaryHeap::new(),
            next_seq: 0,
        }
    }

    pub fn with_capacity(cap: usize) -> Self {
        Self {
            heap: BinaryHeap::with_capacity(cap),
            next_seq: 0,
        }
    }

    /// Queue `event` to fire at absolute timestamp `when`.
    ///
    /// Scheduling a timestamp already in the past is legal and fires on the next
    /// [`pop_due`](Self::pop_due) — subsystems reacting to a write sometimes need "as soon as
    /// possible", and forcing them to know the current cycle to express that would be worse.
    pub fn schedule(&mut self, when: Cycles, event: E) -> EventHandle {
        let seq = self.next_seq;
        self.next_seq += 1;
        self.heap.push(Entry { when, seq, event });
        EventHandle(seq)
    }

    /// Remove a not-yet-fired event. Returns whether it was still pending.
    ///
    /// O(n) in the number of pending events, which is the right trade here: these schedulers
    /// hold on the order of ten entries (one per active subsystem), and paying a lazy
    /// tombstone set on every pop to make cancellation O(log n) would cost more overall —
    /// and would make "was it still pending?" impossible to answer exactly.
    pub fn cancel(&mut self, handle: EventHandle) -> bool {
        let before = self.heap.len();
        self.heap.retain(|e| e.seq != handle.0);
        before != self.heap.len()
    }

    /// Remove every pending event matching `pred`, returning how many were removed.
    ///
    /// For subsystems that think in terms of "cancel my pending timer overflow" without
    /// having kept the handle around.
    pub fn cancel_matching(&mut self, mut pred: impl FnMut(&E) -> bool) -> usize {
        let before = self.heap.len();
        self.heap.retain(|e| !pred(&e.event));
        before - self.heap.len()
    }

    /// Timestamp of the earliest pending event, or `None` when nothing is scheduled.
    pub fn next_event_time(&self) -> Option<Cycles> {
        self.heap.peek().map(|e| e.when)
    }

    /// How many cycles the CPU may run before the next event must be serviced.
    ///
    /// This is the primary hot-loop primitive: a system runs
    /// `cpu.step()` until it has consumed this many cycles, then drains due events. Returns
    /// `None` when nothing is pending, meaning the caller may run to any bound it likes
    /// (usually "the rest of the frame").
    ///
    /// Already-due events yield `Cycles::ZERO`, so the caller drains before running further.
    pub fn cycles_until_next(&self, now: Cycles) -> Option<Cycles> {
        self.next_event_time().map(|when| when.saturating_sub(now))
    }

    /// Pop the earliest event if it is due at or before `now`.
    ///
    /// Deliberately returns one event at a time rather than a `Vec`: handlers reschedule
    /// themselves constantly (a PPU mode transition immediately schedules the next mode), and
    /// an event scheduled by a handler for the current cycle must be picked up by the same
    /// drain loop. Batching into a `Vec` up front would defer it by a whole slice.
    ///
    /// The idiomatic drain is:
    ///
    /// ```ignore
    /// while let Some((when, event)) = scheduler.pop_due(now) {
    ///     self.handle(when, event); // may call scheduler.schedule(..)
    /// }
    /// ```
    pub fn pop_due(&mut self, now: Cycles) -> Option<(Cycles, E)> {
        match self.heap.peek() {
            Some(e) if e.when <= now => {
                let e = self.heap.pop().expect("peeked entry must pop");
                Some((e.when, e.event))
            }
            _ => None,
        }
    }

    pub fn len(&self) -> usize {
        self.heap.len()
    }

    pub fn is_empty(&self) -> bool {
        self.heap.is_empty()
    }

    /// Drop every pending event. Used on reset, never during normal running.
    pub fn clear(&mut self) {
        self.heap.clear();
    }

    /// Pending events in firing order. For the debugger's "what happens next" view and for
    /// deterministic serialization; not a hot path.
    pub fn pending_in_order(&self) -> Vec<(Cycles, &E)> {
        let mut entries: Vec<&Entry<E>> = self.heap.iter().collect();
        entries.sort_by(|a, b| a.when.cmp(&b.when).then_with(|| a.seq.cmp(&b.seq)));
        entries.iter().map(|e| (e.when, &e.event)).collect()
    }
}

impl<E: Savable + SavableInit> Savable for Scheduler<E> {
    fn save(&self, w: &mut StateWriter) {
        w.write_u64(self.next_seq);
        // Written in firing order so the same scheduler state always produces byte-identical
        // output, regardless of the heap's internal array layout. Without this, two
        // equivalent schedulers could serialize differently and break state-hash comparisons
        // in the accuracy harness.
        let mut entries: Vec<&Entry<E>> = self.heap.iter().collect();
        entries.sort_by(|a, b| a.when.cmp(&b.when).then_with(|| a.seq.cmp(&b.seq)));
        w.write_u64(entries.len() as u64);
        for e in entries {
            e.when.save(w);
            w.write_u64(e.seq);
            e.event.save(w);
        }
    }

    fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
        self.next_seq = r.read_u64()?;
        let len = r.read_u64()? as usize;
        if len > r.remaining() {
            return Err(StateError::Malformed(format!(
                "scheduler claims {len} pending events but only {} bytes remain",
                r.remaining()
            )));
        }
        self.heap.clear();
        self.heap.reserve(len);
        for _ in 0..len {
            let mut when = Cycles::ZERO;
            when.load(r)?;
            let seq = r.read_u64()?;
            let event = E::load_new(r)?;
            self.heap.push(Entry { when, seq, event });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use savestate::{decode_state, encode_state};

    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
    struct TestEvent(u32);

    impl Savable for TestEvent {
        fn save(&self, w: &mut StateWriter) {
            w.write_u32(self.0);
        }
        fn load(&mut self, r: &mut StateReader) -> Result<(), StateError> {
            self.0 = r.read_u32()?;
            Ok(())
        }
    }

    fn drain(sched: &mut Scheduler<TestEvent>, now: u64) -> Vec<(u64, u32)> {
        let mut out = Vec::new();
        while let Some((when, ev)) = sched.pop_due(Cycles(now)) {
            out.push((when.get(), ev.0));
        }
        out
    }

    #[test]
    fn fires_in_timestamp_order_regardless_of_scheduling_order() {
        let mut s = Scheduler::new();
        s.schedule(Cycles(300), TestEvent(3));
        s.schedule(Cycles(100), TestEvent(1));
        s.schedule(Cycles(200), TestEvent(2));

        assert_eq!(s.next_event_time(), Some(Cycles(100)));
        assert_eq!(drain(&mut s, 1000), vec![(100, 1), (200, 2), (300, 3)]);
        assert!(s.is_empty());
    }

    #[test]
    fn same_timestamp_ties_break_by_insertion_order() {
        let mut s = Scheduler::new();
        for i in 0..8 {
            s.schedule(Cycles(50), TestEvent(i));
        }
        let fired: Vec<u32> = drain(&mut s, 50).into_iter().map(|(_, e)| e).collect();
        assert_eq!(fired, (0..8).collect::<Vec<_>>());
    }

    #[test]
    fn ordering_is_deterministic_across_equivalent_schedulers() {
        // Same events, opposite insertion order for distinct timestamps, must drain the same.
        let mut a = Scheduler::new();
        let mut b = Scheduler::new();
        for i in 0..16u64 {
            a.schedule(Cycles(i * 10), TestEvent(i as u32));
        }
        for i in (0..16u64).rev() {
            b.schedule(Cycles(i * 10), TestEvent(i as u32));
        }
        assert_eq!(drain(&mut a, 1000), drain(&mut b, 1000));
    }

    #[test]
    fn only_due_events_are_popped() {
        let mut s = Scheduler::new();
        s.schedule(Cycles(10), TestEvent(1));
        s.schedule(Cycles(20), TestEvent(2));

        assert_eq!(drain(&mut s, 10), vec![(10, 1)]);
        assert_eq!(s.len(), 1);
        assert_eq!(s.next_event_time(), Some(Cycles(20)));
        assert_eq!(drain(&mut s, 19), vec![]);
        assert_eq!(drain(&mut s, 20), vec![(20, 2)]);
    }

    #[test]
    fn cancelling_a_pending_event_prevents_it_firing() {
        let mut s = Scheduler::new();
        s.schedule(Cycles(10), TestEvent(1));
        let h = s.schedule(Cycles(20), TestEvent(2));
        s.schedule(Cycles(30), TestEvent(3));

        assert!(s.cancel(h));
        assert_eq!(drain(&mut s, 1000), vec![(10, 1), (30, 3)]);
    }

    #[test]
    fn cancelling_a_fired_or_unknown_handle_reports_false() {
        let mut s = Scheduler::new();
        let h = s.schedule(Cycles(10), TestEvent(1));
        assert_eq!(drain(&mut s, 10), vec![(10, 1)]);
        assert!(!s.cancel(h));
        assert!(!s.cancel(EventHandle::NONE));
    }

    #[test]
    fn handles_are_never_reused_so_stale_cancels_are_harmless() {
        let mut s = Scheduler::new();
        let stale = s.schedule(Cycles(10), TestEvent(1));
        drain(&mut s, 10);
        // A later event must not be cancellable by the old handle.
        s.schedule(Cycles(20), TestEvent(2));
        assert!(!s.cancel(stale));
        assert_eq!(drain(&mut s, 20), vec![(20, 2)]);
    }

    #[test]
    fn cancel_matching_removes_every_match() {
        let mut s = Scheduler::new();
        s.schedule(Cycles(10), TestEvent(7));
        s.schedule(Cycles(20), TestEvent(9));
        s.schedule(Cycles(30), TestEvent(7));

        assert_eq!(s.cancel_matching(|e| e.0 == 7), 2);
        assert_eq!(drain(&mut s, 1000), vec![(20, 9)]);
    }

    #[test]
    fn handlers_may_schedule_from_within_the_drain_loop() {
        // This is what every real PPU does: each mode transition schedules the next one.
        let mut s = Scheduler::new();
        s.schedule(Cycles(10), TestEvent(0));

        let mut fired = Vec::new();
        let now = Cycles(100);
        while let Some((when, ev)) = s.pop_due(now) {
            fired.push((when.get(), ev.0));
            if ev.0 < 4 {
                s.schedule(when + Cycles(10), TestEvent(ev.0 + 1));
            }
        }
        assert_eq!(fired, vec![(10, 0), (20, 1), (30, 2), (40, 3), (50, 4)]);
    }

    #[test]
    fn an_event_scheduled_for_the_current_cycle_fires_in_the_same_drain() {
        let mut s = Scheduler::new();
        s.schedule(Cycles(10), TestEvent(0));

        let mut fired = Vec::new();
        while let Some((_, ev)) = s.pop_due(Cycles(10)) {
            fired.push(ev.0);
            if ev.0 == 0 {
                // Zero-delay reschedule — must not be deferred to the next slice.
                s.schedule(Cycles(10), TestEvent(1));
            }
        }
        assert_eq!(fired, vec![0, 1]);
    }

    #[test]
    fn cycles_until_next_bounds_the_cpu_slice() {
        let mut s: Scheduler<TestEvent> = Scheduler::new();
        assert_eq!(s.cycles_until_next(Cycles(0)), None);

        s.schedule(Cycles(500), TestEvent(1));
        assert_eq!(s.cycles_until_next(Cycles(120)), Some(Cycles(380)));
        // A deadline already passed yields zero, not an underflow.
        assert_eq!(s.cycles_until_next(Cycles(900)), Some(Cycles::ZERO));
    }

    #[test]
    fn events_scheduled_in_the_past_fire_immediately() {
        let mut s = Scheduler::new();
        s.schedule(Cycles(5), TestEvent(1));
        assert_eq!(drain(&mut s, 900), vec![(5, 1)]);
    }

    #[test]
    fn round_trips_through_a_save_state_preserving_order() {
        let mut s = Scheduler::new();
        s.schedule(Cycles(300), TestEvent(3));
        s.schedule(Cycles(100), TestEvent(1));
        s.schedule(Cycles(100), TestEvent(2)); // tie, must stay after event 1
        let cancelled = s.schedule(Cycles(150), TestEvent(99));
        s.cancel(cancelled);

        let blob = encode_state("test", 1, &s);
        let mut restored: Scheduler<TestEvent> = Scheduler::new();
        decode_state("test", 1, &blob, &mut restored).unwrap();

        assert_eq!(
            drain(&mut restored, 1000),
            vec![(100, 1), (100, 2), (300, 3)]
        );
        // Sequence counter survives, so handles issued after a load can't collide with
        // handles the pre-save code is still holding.
        assert_eq!(restored.next_seq, s.next_seq);
    }

    #[test]
    fn serialization_is_byte_identical_for_equivalent_schedulers() {
        let mut a = Scheduler::new();
        a.schedule(Cycles(10), TestEvent(1));
        a.schedule(Cycles(20), TestEvent(2));
        a.schedule(Cycles(30), TestEvent(3));

        let mut b = Scheduler::new();
        b.schedule(Cycles(10), TestEvent(1));
        b.schedule(Cycles(20), TestEvent(2));
        b.schedule(Cycles(30), TestEvent(3));
        // Perturb b's internal heap layout without changing its logical contents.
        let h = b.schedule(Cycles(15), TestEvent(9));
        b.cancel(h);
        b.next_seq = a.next_seq;

        assert_eq!(encode_state("t", 1, &a), encode_state("t", 1, &b));
    }

    #[test]
    fn pending_in_order_lists_events_as_they_will_fire() {
        let mut s = Scheduler::new();
        s.schedule(Cycles(30), TestEvent(3));
        s.schedule(Cycles(10), TestEvent(1));
        s.schedule(Cycles(20), TestEvent(2));

        let pending: Vec<(u64, u32)> = s
            .pending_in_order()
            .into_iter()
            .map(|(c, e)| (c.get(), e.0))
            .collect();
        assert_eq!(pending, vec![(10, 1), (20, 2), (30, 3)]);
    }

    #[test]
    fn clear_drops_everything() {
        let mut s = Scheduler::new();
        s.schedule(Cycles(10), TestEvent(1));
        s.schedule(Cycles(20), TestEvent(2));
        s.clear();
        assert!(s.is_empty());
        assert_eq!(s.next_event_time(), None);
    }
}
