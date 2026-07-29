//! Handing finished frames from the emulation thread to whatever draws them.
//!
//! # Why not a mutex around the framebuffer
//!
//! Prompt 14 asks for "no shared-mutex polling from the UI thread", and the reason is concrete.
//! With a shared framebuffer under a lock, the UI thread holds that lock for as long as the GPU
//! upload takes, and the emulation thread's next frame blocks on it. That couples the emulation
//! rate to the *presentation* rate, which is predecessor lesson §3 exactly: a slow frame in the
//! renderer becomes a slow frame in the emulator, and the audio ring runs dry.
//!
//! # What this does instead
//!
//! A bounded channel of owned buffers with a return path. The emulation thread takes a spare
//! buffer, copies the finished frame into it, and sends. The UI thread receives it, draws from
//! it, and sends it back to be reused. Neither side waits for the other:
//!
//! - if the UI is behind, the channel is full and the producer **drops** the frame rather than
//!   blocking — the emulator keeps its timing and the display simply misses one, which is the
//!   right trade because a frame nobody drew is worth nothing;
//! - if the emulator is behind, the consumer finds nothing new and redraws the frame it already
//!   has.
//!
//! The return path is what keeps this allocation-free in the steady state. A pipe that allocated
//! a framebuffer per frame would ask the allocator for 150 KiB sixty times a second forever.

use core_common::Framebuffer;
use crossbeam_channel::{Receiver, Sender, TrySendError};

/// One finished frame, owning its pixels.
#[derive(Debug, Clone)]
pub struct Frame {
    /// Frame counter since the ROM was loaded. Lets the consumer tell a genuinely new frame from
    /// the one it already drew, and gives the HUD something honest to show.
    pub number: u64,
    pub buffer: Framebuffer,
}

impl Frame {
    fn blank() -> Self {
        Self {
            number: 0,
            // Sized on first use; `Framebuffer::resize` handles the real dimensions, and a 1×1
            // placeholder is cheaper than guessing a platform here.
            buffer: Framebuffer::new(1, 1),
        }
    }
}

/// How many frames may be in flight. Two is enough to cover one late redraw without letting the
/// display fall a visible distance behind the emulator.
pub const DEFAULT_DEPTH: usize = 2;

/// Create a connected publisher and subscriber.
pub fn frame_pipe(depth: usize) -> (FramePublisher, FrameSubscriber) {
    let depth = depth.max(1);
    let (frames_tx, frames_rx) = crossbeam_channel::bounded(depth);
    // The return path is unbounded so that giving a buffer back can never fail or block. It can
    // hold at most `depth + spares` buffers by construction, so "unbounded" is not unbounded
    // memory.
    let (recycle_tx, recycle_rx) = crossbeam_channel::unbounded();
    (
        FramePublisher {
            frames: frames_tx,
            recycle: recycle_rx,
            spares: (0..=depth).map(|_| Frame::blank()).collect(),
            dropped: 0,
        },
        FrameSubscriber {
            frames: frames_rx,
            recycle: recycle_tx,
            current: None,
        },
    )
}

/// The emulation thread's end.
pub struct FramePublisher {
    frames: Sender<Frame>,
    recycle: Receiver<Frame>,
    spares: Vec<Frame>,
    dropped: u64,
}

impl FramePublisher {
    /// Copy a finished framebuffer into a spare buffer and send it.
    ///
    /// Returns `false` when the frame was dropped because the consumer is behind. Never blocks.
    pub fn publish(&mut self, number: u64, source: &Framebuffer) -> bool {
        self.collect_returned();
        let mut frame = match self.spares.pop() {
            Some(frame) => frame,
            // Only reachable if the consumer is holding every buffer at once, which the depth
            // above is chosen to prevent. Allocating is better than dropping the frame, and the
            // buffer joins the pool afterwards.
            None => Frame::blank(),
        };
        // `resize` is a no-op at the same dimensions, so the per-frame cost is the copy alone.
        // It matters on the frame after a ROM switch, where the size genuinely changes.
        frame.buffer.resize(source.width(), source.height());
        frame
            .buffer
            .as_bytes_mut()
            .copy_from_slice(source.as_bytes());
        frame.number = number;

        match self.frames.try_send(frame) {
            Ok(()) => true,
            Err(TrySendError::Full(frame)) => {
                self.spares.push(frame);
                self.dropped += 1;
                false
            }
            // The consumer is gone — the window closed. The caller is shutting down anyway.
            Err(TrySendError::Disconnected(frame)) => {
                self.spares.push(frame);
                false
            }
        }
    }

    /// Whether a `publish` right now would be accepted.
    ///
    /// Used by fast-forward to skip the copy entirely rather than perform it and throw it away:
    /// at ten times speed that is nine wasted framebuffer copies out of ten.
    pub fn has_room(&self) -> bool {
        !self.frames.is_full()
    }

    /// Frames the consumer was too slow to take.
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    fn collect_returned(&mut self) {
        while let Ok(frame) = self.recycle.try_recv() {
            self.spares.push(frame);
        }
    }
}

/// The drawing thread's end.
///
/// Holds the most recent frame, so a redraw with nothing new to show still has something to
/// draw. That is not a convenience: a window resize or an `egui` interaction triggers a redraw at
/// an arbitrary moment, and a subscriber that only exposed newly-arrived frames would flash.
pub struct FrameSubscriber {
    frames: Receiver<Frame>,
    recycle: Sender<Frame>,
    current: Option<Frame>,
}

impl FrameSubscriber {
    /// Take the newest available frame, returning whether the held frame changed.
    ///
    /// Every frame newer than the one being replaced is drained and the intermediate ones are
    /// returned unread. Skipping to the newest is correct for a display: an already-stale frame
    /// has no value, and drawing a queue of them in sequence would be slow motion, not catch-up.
    pub fn poll(&mut self) -> bool {
        let mut updated = false;
        while let Ok(frame) = self.frames.try_recv() {
            if let Some(old) = self.current.replace(frame) {
                let _ = self.recycle.send(old);
            }
            updated = true;
        }
        updated
    }

    /// The frame currently held, if any has ever arrived.
    pub fn current(&self) -> Option<&Frame> {
        self.current.as_ref()
    }

    /// Drop the held frame — used when a ROM closes, so the window does not keep showing the
    /// last frame of a game that is no longer running.
    pub fn clear(&mut self) {
        if let Some(frame) = self.current.take() {
            let _ = self.recycle.send(frame);
        }
        while let Ok(frame) = self.frames.try_recv() {
            let _ = self.recycle.send(frame);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_common::Rgba8;

    fn framebuffer(width: u32, height: u32, fill: u8) -> Framebuffer {
        let mut fb = Framebuffer::new(width, height);
        fb.fill(Rgba8 {
            r: fill,
            g: fill,
            b: fill,
            a: 255,
        });
        fb
    }

    #[test]
    fn a_published_frame_arrives_with_its_pixels() {
        let (mut publisher, mut subscriber) = frame_pipe(DEFAULT_DEPTH);
        let source = framebuffer(4, 2, 0x30);

        assert!(publisher.publish(7, &source));
        assert!(subscriber.poll());

        let frame = subscriber.current().unwrap();
        assert_eq!(frame.number, 7);
        assert_eq!(frame.buffer.width(), 4);
        assert_eq!(frame.buffer.height(), 2);
        assert_eq!(frame.buffer.as_bytes(), source.as_bytes());
    }

    #[test]
    fn polling_with_nothing_new_keeps_the_last_frame() {
        let (mut publisher, mut subscriber) = frame_pipe(DEFAULT_DEPTH);
        publisher.publish(1, &framebuffer(2, 2, 9));
        subscriber.poll();

        assert!(!subscriber.poll(), "nothing new arrived");
        assert_eq!(
            subscriber.current().unwrap().number,
            1,
            "a redraw with no new frame must still have a frame"
        );
    }

    #[test]
    fn a_slow_consumer_skips_to_the_newest_frame_rather_than_replaying() {
        let (mut publisher, mut subscriber) = frame_pipe(4);
        let source = framebuffer(2, 2, 1);
        for number in 1..=4 {
            assert!(publisher.publish(number, &source));
        }

        assert!(subscriber.poll());
        assert_eq!(
            subscriber.current().unwrap().number,
            4,
            "stale frames are worthless to a display"
        );
    }

    #[test]
    fn a_full_pipe_drops_rather_than_blocking() {
        let (mut publisher, _subscriber) = frame_pipe(2);
        let source = framebuffer(2, 2, 1);
        assert!(publisher.publish(1, &source));
        assert!(publisher.publish(2, &source));
        assert!(!publisher.has_room());

        // The third has nowhere to go. It must return promptly and report the drop, because the
        // emulation thread is on a deadline.
        assert!(!publisher.publish(3, &source));
        assert_eq!(publisher.dropped(), 1);
    }

    #[test]
    fn returned_buffers_are_reused_so_the_steady_state_does_not_allocate() {
        let (mut publisher, mut subscriber) = frame_pipe(2);
        let source = framebuffer(8, 8, 3);
        // Far more frames than there are buffers: if recycling were broken, the pipe would
        // either allocate forever or start dropping every frame.
        for number in 1..=200 {
            assert!(
                publisher.publish(number, &source),
                "frame {number} was dropped, so buffers are not coming back"
            );
            subscriber.poll();
        }
        assert_eq!(publisher.dropped(), 0);
    }

    #[test]
    fn a_size_change_is_carried_through() {
        let (mut publisher, mut subscriber) = frame_pipe(2);
        publisher.publish(1, &framebuffer(160, 144, 1));
        subscriber.poll();
        // Switching from a Game Boy to a Game Boy Advance mid-session.
        publisher.publish(2, &framebuffer(240, 160, 2));
        subscriber.poll();

        let frame = subscriber.current().unwrap();
        assert_eq!((frame.buffer.width(), frame.buffer.height()), (240, 160));
        assert_eq!(frame.buffer.pixel(0, 0).r, 2);
    }

    #[test]
    fn clearing_releases_the_held_frame_and_the_queue() {
        let (mut publisher, mut subscriber) = frame_pipe(2);
        publisher.publish(1, &framebuffer(2, 2, 1));
        subscriber.poll();
        publisher.publish(2, &framebuffer(2, 2, 2));

        subscriber.clear();
        assert!(subscriber.current().is_none());
        assert!(!subscriber.poll(), "the queued frame was released too");
    }

    #[test]
    fn a_dead_consumer_does_not_panic_the_producer() {
        let (mut publisher, subscriber) = frame_pipe(1);
        drop(subscriber);
        // The window closed while a frame was in flight. Emulation must unwind cleanly.
        assert!(!publisher.publish(1, &framebuffer(2, 2, 1)));
    }
}
