//! A twenty-line executor, for the three `async` calls in `wgpu`'s setup path.
//!
//! `wgpu`'s adapter, device, and surface requests are `async` because the WebGPU backend is. On
//! native they resolve on the first poll, and this application has no other asynchronous work at
//! all — no runtime, no task spawning, nothing to drive concurrently.
//!
//! So the choice is between adding an async runtime to the dependency graph for a handful of calls
//! that finish immediately, or parking the thread until the future is ready. This is the second
//! one. It is written with [`std::task::Wake`], so there is no `unsafe` — the crate keeps
//! `#![deny(unsafe_code)]`, which a hand-rolled `RawWakerVTable` would not allow.
//!
//! A spurious `unpark` only costs one extra poll, which is why the loop re-polls rather than
//! assuming a wake means readiness.

use std::future::Future;
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};

/// Waking means unparking the thread that is blocked on the future.
struct ThreadWaker(std::thread::Thread);

impl Wake for ThreadWaker {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.unpark();
    }
}

/// Run a future to completion on the current thread.
pub fn block_on<F: Future>(future: F) -> F::Output {
    // Boxing pins without `unsafe`. It costs one allocation per call, and this is called three
    // times in the lifetime of the process.
    let mut future = Box::pin(future);
    let waker = Waker::from(Arc::new(ThreadWaker(std::thread::current())));
    let mut context = Context::from_waker(&waker);
    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(value) => return value,
            Poll::Pending => std::thread::park(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_ready_future_returns_without_parking() {
        assert_eq!(block_on(std::future::ready(7)), 7);
    }

    #[test]
    fn a_future_woken_from_another_thread_completes() {
        // The case the `park`/`unpark` pair exists for: a future that is genuinely pending on the
        // first poll and is woken later. If the waker were wrong this would hang rather than fail,
        // which is why the test is worth having at all.
        struct Once {
            done: Arc<std::sync::atomic::AtomicBool>,
            started: bool,
        }

        impl Future for Once {
            type Output = &'static str;

            fn poll(
                mut self: std::pin::Pin<&mut Self>,
                cx: &mut Context<'_>,
            ) -> Poll<Self::Output> {
                if self.done.load(std::sync::atomic::Ordering::Acquire) {
                    return Poll::Ready("finished");
                }
                if !self.started {
                    self.started = true;
                    let waker = cx.waker().clone();
                    let done = self.done.clone();
                    std::thread::spawn(move || {
                        done.store(true, std::sync::atomic::Ordering::Release);
                        waker.wake();
                    });
                }
                Poll::Pending
            }
        }

        let future = Once {
            done: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            started: false,
        };
        assert_eq!(block_on(future), "finished");
    }
}
