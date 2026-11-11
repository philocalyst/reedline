//! Waiting for input without polling.
//!
//! `read_line` sleeps until the terminal has an event or another thread wakes
//! it. Other threads hold a [`Wake`]: a repaint request, a line for the
//! external printer, or (with a completer that works in the background) an
//! answer that arrived. Whatever they wake the editor for, they keep
//! themselves; waking only makes the editor look.

use crossterm::event::{Event, EventStream};
use futures_core::Stream;
use std::{
    future::{poll_fn, Future},
    io,
    pin::{pin, Pin},
    sync::{Arc, Mutex, PoisonError},
    task::{Context, Poll, Wake as TaskWake, Waker},
    thread::{self, Thread},
};

/// A handle another thread uses to wake a running `read_line`.
#[derive(Clone, Debug, Default)]
pub struct Wake(Arc<Mutex<Waiting>>);

#[derive(Debug, Default)]
struct Waiting {
    /// Set by a wake nobody has seen yet.
    woken: bool,
    /// The editor's waker while it sleeps.
    waker: Option<Waker>,
}

impl Wake {
    /// Wakes the editor if it sleeps, or makes its next wait return at once.
    pub fn wake(&self) {
        let waker = {
            let mut waiting = self.0.lock().unwrap_or_else(PoisonError::into_inner);
            waiting.woken = true;
            waiting.waker.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }

    /// Whether a wake came since this was last asked, for a test that cannot
    /// sleep on it.
    #[cfg(test)]
    pub(crate) fn take_woken(&self) -> bool {
        std::mem::take(&mut self.0.lock().unwrap_or_else(PoisonError::into_inner).woken)
    }

    /// Whether a wake came since the last call. Otherwise `waker` is kept for
    /// the next one, under the same lock, so no wake slips in between.
    fn poll_woken(&self, waker: &Waker) -> bool {
        let mut waiting = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if std::mem::take(&mut waiting.woken) {
            return true;
        }
        waiting.waker = Some(waker.clone());
        false
    }
}

/// Terminal input, waited on alongside a [`Wake`].
///
/// crossterm's event stream watches the terminal from a thread of its own.
/// That thread reads the terminal while it watches, so it must not outlive a
/// wait: the host may run a program that reads the terminal itself, and a
/// cursor-position query would sit behind it. [`Input::release`] stops it,
/// and a wake does so on its way out.
#[derive(Default)]
pub(crate) struct Input {
    stream: Option<EventStream>,
}

impl Input {
    /// Sleeps until the terminal has an event, `Ok(Some(event))`, or `wake`
    /// is woken, `Ok(None)`.
    pub(crate) fn wait(&mut self, wake: &Wake) -> io::Result<Option<Event>> {
        let stream = self.stream.get_or_insert_with(EventStream::new);
        let woke = block_on(poll_fn(|cx| {
            if wake.poll_woken(cx.waker()) {
                return Poll::Ready(None);
            }
            match Pin::new(&mut *stream).poll_next(cx) {
                Poll::Ready(Some(event)) => Poll::Ready(Some(event)),
                Poll::Ready(None) => Poll::Ready(Some(Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "the terminal closed its input",
                )))),
                Poll::Pending => Poll::Pending,
            }
        }));
        match woke {
            Some(event) => event.map(Some),
            None => {
                // An event that came with the wake stays queued in crossterm
                // for the next wait.
                self.release();
                Ok(None)
            }
        }
    }

    /// Stops watching the terminal until the next wait.
    pub(crate) fn release(&mut self) {
        self.stream = None;
    }
}

/// Runs `future` to completion on this thread, parking it while the future
/// is pending.
fn block_on<F: Future>(future: F) -> F::Output {
    struct Unpark(Thread);

    impl TaskWake for Unpark {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }

        fn wake_by_ref(self: &Arc<Self>) {
            self.0.unpark();
        }
    }

    let waker = Waker::from(Arc::new(Unpark(thread::current())));
    let mut cx = Context::from_waker(&waker);
    let mut future = pin!(future);
    loop {
        if let Poll::Ready(output) = future.as_mut().poll(&mut cx) {
            return output;
        }
        // A spurious unpark only polls once more.
        thread::park();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Waits on `wake` alone, as `Input::wait` does without a terminal.
    fn sleep_until(wake: &Wake) {
        block_on(poll_fn(|cx| {
            if wake.poll_woken(cx.waker()) {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        }));
    }

    #[test]
    fn a_wake_from_another_thread_ends_the_sleep() {
        let wake = Wake::default();
        let other = wake.clone();
        let waker = thread::spawn(move || {
            thread::sleep(Duration::from_millis(20));
            other.wake();
        });
        sleep_until(&wake);
        waker.join().expect("waker thread");
    }

    #[test]
    fn a_wake_before_the_sleep_is_not_lost() {
        let wake = Wake::default();
        wake.wake();
        sleep_until(&wake);
        // Seen once: the next wait sleeps again.
        assert!(!wake.poll_woken(Waker::noop()));
    }
}
