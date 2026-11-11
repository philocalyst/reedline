//! Answers that arrive later, from any thread.

use crate::Wake;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, Mutex, PoisonError,
};

/// The one answer reedline waits for on a request, sent from any thread.
///
/// Sending consumes it, so a request is answered at most once. A reply for a
/// line the user has since changed is no longer current: sending it does
/// nothing, and [`Reply::is_current`] tells the work it may stop. Dropping it
/// unsent answers nothing, and reedline keeps showing what it has.
pub struct Reply<T> {
    inbox: Arc<Inbox<T>>,
    seq: u64,
}

impl<T> Reply<T> {
    /// Hands `value` to the editor and wakes it to show it, if the editor
    /// still wants it.
    pub fn send(self, value: T) {
        if !self.is_current() {
            return;
        }
        *self.inbox.lock() = Some((self.seq, value));
        self.inbox.wake.wake();
    }

    /// Whether the editor still wants this answer: `false` once a newer
    /// request is made, or the answer is no longer needed at all.
    pub fn is_current(&self) -> bool {
        self.inbox.latest.load(Ordering::Acquire) == self.seq
    }
}

/// Where answers land: the latest one asked for, kept until the editor takes
/// it.
pub(crate) struct Inbox<T> {
    /// The newest request made. Older replies are stale.
    latest: AtomicU64,
    answer: Mutex<Option<(u64, T)>>,
    wake: Wake,
}

impl<T> Inbox<T> {
    pub(crate) fn new(wake: Wake) -> Arc<Self> {
        Arc::new(Self {
            latest: AtomicU64::new(0),
            answer: Mutex::new(None),
            wake,
        })
    }

    /// A reply for a new request, which makes every earlier one stale.
    pub(crate) fn ask(self: &Arc<Self>) -> Reply<T> {
        let seq = self.latest.fetch_add(1, Ordering::AcqRel) + 1;
        Reply {
            inbox: Arc::clone(self),
            seq,
        }
    }

    /// Makes every reply out there stale, wanting no answer at all.
    pub(crate) fn forget(&self) {
        self.latest.fetch_add(1, Ordering::AcqRel);
        *self.lock() = None;
    }

    /// The answer to the latest request, once it has arrived.
    pub(crate) fn take(&self) -> Option<T> {
        let latest = self.latest.load(Ordering::Acquire);
        let mut answer = self.lock();
        match answer.take() {
            Some((seq, value)) if seq == latest => Some(value),
            _ => None,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<(u64, T)>> {
        self.answer.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_answer_to_an_older_request_never_lands() {
        let inbox = Inbox::new(Wake::default());
        let old = inbox.ask();
        let new = inbox.ask();
        assert!(!old.is_current());
        old.send("old");
        assert_eq!(inbox.take(), None);
        new.send("new");
        assert_eq!(inbox.take(), Some("new"));
        assert_eq!(inbox.take(), None);
    }

    #[test]
    fn an_answer_from_another_thread_wakes_the_editor() {
        let wake = Wake::default();
        let inbox = Inbox::new(wake.clone());
        let reply = inbox.ask();
        std::thread::spawn(move || reply.send(7))
            .join()
            .expect("sender");
        assert!(wake.take_woken());
        assert_eq!(inbox.take(), Some(7));
    }

    #[test]
    fn forgetting_drops_what_was_on_its_way() {
        let inbox = Inbox::new(Wake::default());
        let reply = inbox.ask();
        inbox.forget();
        reply.send(1);
        assert_eq!(inbox.take(), None);
    }
}
