//! The rewind buffer: a ring of periodic snapshots the frontend can step backwards through.
//!
//! # Full snapshots, not deltas
//!
//! Prompt 16 is explicit about starting here. A delta-based buffer stores less but is a second
//! serialisation path to keep correct alongside the first, and a bug in it looks like a
//! *corrupt emulator* rather than a corrupt file. Full snapshots reuse the [`Savable`] machinery
//! that every system already proves in its own round-trip tests, so a rewind that works is
//! evidence the save states work and vice versa.
//!
//! Delta compression is prompt 18's candidate — after the memory cost has been *measured* to be
//! a problem rather than assumed to be one. [`RewindBuffer::memory_used`] exists so that
//! measurement is available rather than estimated.
//!
//! # An interval, not every frame
//!
//! Snapshotting sixty times a second would spend more time serialising than emulating. The
//! buffer takes one every `interval` frames and the frontend replays forward from the nearest
//! one, which is how every emulator with this feature works — the cost of a rewind is bounded
//! by the interval, and the cost of *running* is bounded by dividing by it.
//!
//! # Rewinding does not discard what it passed
//!
//! Stepping back leaves the newer snapshots in place, so a player who overshoots can step
//! forward again. They are only dropped when the machine runs on from a rewound position and
//! records a new snapshot — at which point the future they described no longer exists. Dropping
//! them at the moment of rewind is the obvious implementation and makes overshooting
//! unrecoverable.

use std::collections::VecDeque;

/// One recorded moment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    /// The frame this was taken at, so a frontend can show how far back it is going.
    pub frame: u64,
    pub state: Vec<u8>,
}

/// A bounded history of snapshots.
#[derive(Debug, Clone)]
pub struct RewindBuffer {
    snapshots: VecDeque<Snapshot>,
    capacity: usize,
    /// Frames between snapshots.
    interval: u64,
    /// How far back the cursor currently is, in snapshots. Zero is the present.
    position: usize,
    frames_since_snapshot: u64,
}

impl RewindBuffer {
    /// `capacity` snapshots, one every `interval` frames.
    ///
    /// A zero interval is corrected to one rather than rejected: it means "every frame", which
    /// is wasteful but coherent, and a constructor that can fail would push the check into every
    /// caller for a value they usually hard-code.
    pub fn new(capacity: usize, interval: u64) -> Self {
        Self {
            snapshots: VecDeque::with_capacity(capacity.max(1)),
            capacity: capacity.max(1),
            interval: interval.max(1),
            position: 0,
            frames_since_snapshot: 0,
        }
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn interval(&self) -> u64 {
        self.interval
    }

    pub fn len(&self) -> usize {
        self.snapshots.len()
    }

    pub fn is_empty(&self) -> bool {
        self.snapshots.is_empty()
    }

    /// How far back the cursor is, in snapshots.
    pub fn position(&self) -> usize {
        self.position
    }

    /// Total bytes held, so prompt 18 can measure rather than assume.
    pub fn memory_used(&self) -> usize {
        self.snapshots.iter().map(|s| s.state.len()).sum()
    }

    /// How much emulated time the buffer covers, in frames.
    pub fn span_frames(&self) -> u64 {
        match (self.snapshots.front(), self.snapshots.back()) {
            (Some(oldest), Some(newest)) => newest.frame - oldest.frame,
            _ => 0,
        }
    }

    /// Whether a frame boundary calls for a snapshot.
    ///
    /// Asked rather than told, so the caller does not serialise a state it is about to discard:
    /// the whole point of the interval is that most frames cost nothing.
    pub fn wants_snapshot(&self) -> bool {
        self.snapshots.is_empty() || self.frames_since_snapshot >= self.interval
    }

    /// Record that a frame passed without taking a snapshot.
    pub fn frame_elapsed(&mut self) {
        self.frames_since_snapshot += 1;
    }

    /// Store a snapshot, dropping the oldest if the buffer is full.
    ///
    /// Recording while rewound discards everything ahead of the cursor: the machine has run on
    /// from that point, so the future those snapshots described no longer exists.
    pub fn push(&mut self, frame: u64, state: Vec<u8>) {
        if self.position > 0 {
            let keep = self.snapshots.len() - self.position;
            self.snapshots.truncate(keep);
            self.position = 0;
        }
        if self.snapshots.len() == self.capacity {
            self.snapshots.pop_front();
        }
        self.snapshots.push_back(Snapshot { frame, state });
        self.frames_since_snapshot = 0;
    }

    /// Step one snapshot further back, returning what to load.
    ///
    /// `None` when there is nothing older — the buffer is empty, or the cursor is already at the
    /// oldest snapshot it holds. A frontend shows that as "cannot rewind further" rather than
    /// silently doing nothing.
    pub fn rewind(&mut self) -> Option<&Snapshot> {
        // The cursor may reach the oldest snapshot but not step past it: at that point
        // `position` is `len - 1`, which `current` turns into index zero.
        if self.position + 1 >= self.snapshots.len() {
            return None;
        }
        self.position += 1;
        self.current()
    }

    /// Step one snapshot forward, for a player who rewound too far.
    pub fn advance(&mut self) -> Option<&Snapshot> {
        if self.position == 0 {
            return None;
        }
        self.position -= 1;
        self.current()
    }

    /// The snapshot the cursor is on.
    pub fn current(&self) -> Option<&Snapshot> {
        if self.snapshots.is_empty() {
            return None;
        }
        self.snapshots.get(self.snapshots.len() - 1 - self.position)
    }

    /// Forget everything, as loading a different cartridge does.
    ///
    /// A buffer carried across a cartridge change would hand a frontend states belonging to a
    /// machine that is no longer there, and the loader would reject them one at a time as the
    /// player rewound — which reads as a broken rewind rather than a stale buffer.
    pub fn clear(&mut self) {
        self.snapshots.clear();
        self.position = 0;
        self.frames_since_snapshot = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(byte: u8) -> Vec<u8> {
        vec![byte; 4]
    }

    /// Fill a buffer with `count` snapshots, numbered from zero.
    fn filled(capacity: usize, count: u64) -> RewindBuffer {
        let mut buffer = RewindBuffer::new(capacity, 1);
        for frame in 0..count {
            buffer.push(frame, state(frame as u8));
        }
        buffer
    }

    #[test]
    fn a_fresh_buffer_holds_nothing_and_wants_its_first_snapshot() {
        let buffer = RewindBuffer::new(4, 60);
        assert!(buffer.is_empty());
        assert!(
            buffer.wants_snapshot(),
            "so there is something to rewind to"
        );
        assert_eq!(buffer.current(), None);
    }

    #[test]
    fn a_snapshot_is_wanted_once_per_interval_and_not_every_frame() {
        // Snapshotting sixty times a second would spend more time serialising than emulating.
        let mut buffer = RewindBuffer::new(4, 10);
        buffer.push(0, state(0));
        for frame in 1..=10 {
            assert!(!buffer.wants_snapshot(), "frame {frame}");
            buffer.frame_elapsed();
        }
        assert!(buffer.wants_snapshot(), "ten frames have passed");
    }

    #[test]
    fn a_zero_interval_means_every_frame_rather_than_being_rejected() {
        let buffer = RewindBuffer::new(4, 0);
        assert_eq!(buffer.interval(), 1);
    }

    #[test]
    fn the_oldest_snapshot_is_dropped_when_the_buffer_is_full() {
        let buffer = filled(3, 5);
        assert_eq!(buffer.len(), 3);
        assert_eq!(buffer.current().unwrap().frame, 4, "the newest");
        assert_eq!(buffer.span_frames(), 2, "frames 2 through 4");
    }

    #[test]
    fn rewinding_walks_back_one_snapshot_at_a_time() {
        let mut buffer = filled(4, 4);
        assert_eq!(buffer.current().unwrap().frame, 3);
        assert_eq!(buffer.rewind().unwrap().frame, 2);
        assert_eq!(buffer.rewind().unwrap().frame, 1);
        assert_eq!(buffer.rewind().unwrap().frame, 0);
        assert_eq!(buffer.rewind(), None, "nothing older is held");
    }

    #[test]
    fn rewinding_returns_the_state_that_was_stored() {
        let mut buffer = filled(4, 3);
        assert_eq!(buffer.rewind().unwrap().state, state(1));
    }

    #[test]
    fn an_empty_buffer_cannot_be_rewound() {
        // A frontend shows that as "cannot rewind further" rather than silently doing nothing.
        let mut buffer = RewindBuffer::new(4, 1);
        assert_eq!(buffer.rewind(), None);
    }

    #[test]
    fn rewinding_does_not_discard_what_it_passed() {
        // A player who overshoots can step forward again. Dropping them at the moment of rewind
        // is the obvious implementation and makes overshooting unrecoverable.
        let mut buffer = filled(4, 4);
        buffer.rewind();
        buffer.rewind();
        assert_eq!(buffer.len(), 4, "all four are still held");
        assert_eq!(buffer.advance().unwrap().frame, 2);
        assert_eq!(buffer.advance().unwrap().frame, 3);
        assert_eq!(buffer.advance(), None, "already at the present");
    }

    #[test]
    fn running_on_from_a_rewound_position_discards_the_future() {
        // Those snapshots described a future that no longer exists.
        let mut buffer = filled(8, 5);
        buffer.rewind();
        buffer.rewind();
        assert_eq!(buffer.current().unwrap().frame, 2);

        buffer.push(3, state(0xFF));
        assert_eq!(buffer.len(), 4, "frames 0, 1, 2, and the new 3");
        assert_eq!(buffer.position(), 0, "and the cursor is at the present");
        assert_eq!(buffer.current().unwrap().state, state(0xFF));
        assert_eq!(buffer.rewind().unwrap().frame, 2);
    }

    #[test]
    fn pushing_resets_the_interval_counter() {
        let mut buffer = RewindBuffer::new(4, 5);
        buffer.push(0, state(0));
        for _ in 0..5 {
            buffer.frame_elapsed();
        }
        assert!(buffer.wants_snapshot());
        buffer.push(5, state(1));
        assert!(!buffer.wants_snapshot());
    }

    #[test]
    fn a_full_buffer_can_be_rewound_across_its_whole_depth() {
        // The acceptance criterion prompt 16 states: rewind works across at least a full
        // buffer-depth window.
        let mut buffer = filled(16, 16);
        for expected in (0..15).rev() {
            assert_eq!(
                buffer.rewind().unwrap().frame,
                expected,
                "stepping back to {expected}"
            );
        }
        assert_eq!(buffer.rewind(), None);
    }

    #[test]
    fn memory_used_is_measured_rather_than_estimated() {
        // Prompt 18 may replace this with deltas, but only once the cost is shown to be a real
        // problem rather than assumed to be one.
        let buffer = filled(4, 4);
        assert_eq!(buffer.memory_used(), 16, "four snapshots of four bytes");
        assert_eq!(RewindBuffer::new(4, 1).memory_used(), 0);
    }

    #[test]
    fn clearing_forgets_the_cursor_as_well_as_the_snapshots() {
        // A buffer carried across a cartridge change would hand a frontend states belonging to a
        // machine that is no longer there.
        let mut buffer = filled(4, 4);
        buffer.rewind();
        buffer.clear();
        assert!(buffer.is_empty());
        assert_eq!(buffer.position(), 0);
        assert!(buffer.wants_snapshot());
    }

    #[test]
    fn a_capacity_of_zero_still_holds_one_snapshot() {
        // Otherwise `push` would drop what it was just given, and rewind would appear broken
        // rather than disabled.
        let mut buffer = RewindBuffer::new(0, 1);
        buffer.push(0, state(0));
        assert_eq!(buffer.len(), 1);
        assert_eq!(buffer.current().unwrap().frame, 0);
    }
}
