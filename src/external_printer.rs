//! Lines another thread prints above the prompt while it is being edited.

use crate::Wake;
use std::{
    fmt::Display,
    io,
    sync::{
        mpsc::{sync_channel, Receiver, SendError, SyncSender, TrySendError},
        Arc, OnceLock,
    },
};

/// How many lines can wait to be printed before a sender blocks.
pub const EXTERNAL_PRINTER_DEFAULT_CAPACITY: usize = 20;

/// Prints lines from other threads above the prompt, as output printed at a
/// quiet prompt would be, and redraws the prompt below them.
#[derive(Debug)]
pub struct ExternalPrinter<T>
where
    T: Display,
{
    sender: PrinterSender<T>,
    receiver: Receiver<T>,
}

/// Sends lines to an [`ExternalPrinter`] from any thread. Each line wakes the
/// editor, which prints it at once instead of on its next keystroke.
#[derive(Debug)]
pub struct PrinterSender<T> {
    sender: SyncSender<T>,
    wake: Arc<OnceLock<Wake>>,
}

impl<T> Clone for PrinterSender<T> {
    fn clone(&self) -> Self {
        Self {
            sender: self.sender.clone(),
            wake: Arc::clone(&self.wake),
        }
    }
}

impl<T> PrinterSender<T> {
    /// Queues `line`, blocking while the printer's queue is full.
    pub fn send(&self, line: T) -> Result<(), SendError<T>> {
        self.sender.send(line)?;
        self.wake();
        Ok(())
    }

    /// Queues `line` unless the printer's queue is full.
    pub fn try_send(&self, line: T) -> Result<(), TrySendError<T>> {
        self.sender.try_send(line)?;
        self.wake();
        Ok(())
    }

    fn wake(&self) {
        if let Some(wake) = self.wake.get() {
            wake.wake();
        }
    }
}

impl<T> ExternalPrinter<T>
where
    T: Display,
{
    /// A printer that holds up to `max_cap` lines before senders block.
    pub fn new(max_cap: usize) -> Self {
        let (sender, receiver) = sync_channel::<T>(max_cap);
        Self {
            sender: PrinterSender {
                sender,
                wake: Arc::default(),
            },
            receiver,
        }
    }

    /// A handle to send lines from another thread.
    pub fn sender(&self) -> PrinterSender<T> {
        self.sender.clone()
    }

    /// The receiving end, for a host that drains the lines itself.
    pub fn receiver(&self) -> &Receiver<T> {
        &self.receiver
    }

    /// Queues `line`, blocking while the queue is full.
    pub fn print(&self, line: T) -> Result<(), SendError<T>> {
        self.sender.send(line)
    }

    /// Takes the next queued line, if any.
    pub fn get_line(&self) -> Option<T> {
        self.receiver.try_recv().ok()
    }
}

impl<T> Default for ExternalPrinter<T>
where
    T: Display,
{
    fn default() -> Self {
        Self::new(EXTERNAL_PRINTER_DEFAULT_CAPACITY)
    }
}

/// A source of lines for the editor to print above the prompt.
pub trait ExternalOutput: Send {
    /// Every line waiting to be printed.
    fn drain(&mut self) -> io::Result<Vec<String>>;

    /// Hands over the handle that wakes the editor, for a source that can
    /// wake it when it has lines. Called once, by
    /// [`Reedline::with_external_printer`](crate::Reedline::with_external_printer).
    fn attach(&mut self, _wake: Wake) {}

    /// Whether the source wakes the editor itself. One that does not is
    /// drained on a timer instead, every
    /// [`poll_interval`](crate::Reedline::with_poll_interval).
    fn wakes(&self) -> bool {
        false
    }
}

impl<T: Display + Send> ExternalOutput for ExternalPrinter<T> {
    fn drain(&mut self) -> io::Result<Vec<String>> {
        let mut messages = Vec::new();
        while let Ok(message) = self.receiver.try_recv() {
            messages.push(message.to_string());
        }
        Ok(messages)
    }

    fn attach(&mut self, wake: Wake) {
        // A printer handed to a second editor keeps waking the first.
        let _ = self.sender.wake.set(wake);
    }

    fn wakes(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_sent_from_another_thread_is_received() {
        let printer = ExternalPrinter::<String>::new(2);
        let sender = printer.sender();
        std::thread::spawn(move || sender.send("hello".to_string()).unwrap())
            .join()
            .unwrap();
        assert_eq!(printer.get_line().as_deref(), Some("hello"));
        assert_eq!(printer.get_line(), None);
    }

    #[test]
    fn print_goes_through_the_same_channel() {
        let printer = ExternalPrinter::<String>::new(1);
        printer.print("via print".to_string()).unwrap();
        assert_eq!(printer.get_line().as_deref(), Some("via print"));
    }

    #[test]
    fn a_line_from_another_thread_wakes_the_editor() {
        let mut printer = ExternalPrinter::<String>::new(2);
        let wake = Wake::default();
        printer.attach(wake.clone());
        let sender = printer.sender();
        std::thread::spawn(move || sender.send("hello".to_string()).unwrap())
            .join()
            .unwrap();
        assert!(wake.take_woken());
    }

    #[test]
    fn drain_returns_messages_in_order() {
        let mut printer = ExternalPrinter::<String>::new(2);
        printer.print("one\ntwo".to_string()).unwrap();
        printer.print("three".to_string()).unwrap();
        assert_eq!(printer.drain().unwrap(), ["one\ntwo", "three"]);
        assert!(printer.drain().unwrap().is_empty());
    }
}
