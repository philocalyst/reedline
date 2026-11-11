use crate::painting::W;
use std::io::{self, IsTerminal, Write};

/// Mouse reporting, on only while the completion panel is open so that the
/// terminal's own text selection works the rest of the time.
///
/// Only presses, releases and the wheel are reported: no motion and no drag,
/// which would flood the editor with events, and which a click and the wheel
/// do not need.
///
/// The modes are written to the stream the editor draws to ([`W::stream`]), not
/// to stdout: with stdout redirected, as in `nu | tee`, that is the stream that
/// is still the terminal. Dropping the guard turns reporting off, so that
/// every way out of a read, an error and an unwinding panic included, leaves
/// the terminal as it was found.
#[derive(Default)]
pub(crate) struct MouseGuard {
    active: bool,
}

impl MouseGuard {
    /// Turns reporting on or off, writing to the terminal only when that
    /// changes it. Tests never write to the terminal they run in.
    pub fn set(&mut self, on: bool) {
        let mut stream = W::stream();
        let wanted = !cfg!(test) && stream.is_terminal();
        self.set_on(on, wanted.then_some(&mut stream));
    }

    /// [`set`](Self::set) writing to `out`, if there is one to write to.
    fn set_on(&mut self, on: bool, out: Option<&mut impl Write>) {
        if on == self.active {
            return;
        }
        self.active = on;
        if let Some(out) = out {
            let _ = report(out, on);
        }
    }

    /// Whether reporting is on.
    #[cfg(test)]
    pub fn is_on(&self) -> bool {
        self.active
    }
}

impl Drop for MouseGuard {
    fn drop(&mut self) {
        self.set(false);
    }
}

/// Asks the terminal to report mouse presses and the wheel, or to stop.
#[cfg(not(windows))]
fn report(out: &mut impl Write, on: bool) -> io::Result<()> {
    // 1000 reports presses and releases, 1006 in a form that has no column
    // limit.
    out.write_all(if on {
        b"\x1b[?1000h\x1b[?1006h"
    } else {
        b"\x1b[?1006l\x1b[?1000l"
    })?;
    out.flush()
}

/// Asks the console to report mouse presses and the wheel, or to stop.
#[cfg(windows)]
fn report(out: &mut impl Write, on: bool) -> io::Result<()> {
    use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
    if on {
        crossterm::execute!(out, EnableMouseCapture)
    } else {
        crossterm::execute!(out, DisableMouseCapture)
    }
}

#[cfg(all(test, not(windows)))]
mod tests {
    use super::*;

    #[test]
    fn reporting_is_written_to_the_given_stream_only_when_it_changes() {
        let mut guard = MouseGuard::default();
        let mut out = Vec::new();

        guard.set_on(true, Some(&mut out));
        assert_eq!(out, b"\x1b[?1000h\x1b[?1006h");
        assert!(guard.is_on());
        guard.set_on(true, Some(&mut out));
        assert_eq!(out.len(), b"\x1b[?1000h\x1b[?1006h".len(), "already on");

        guard.set_on(false, Some(&mut out));
        assert!(out.ends_with(b"\x1b[?1006l\x1b[?1000l"));
        assert!(!guard.is_on());
        let written = out.len();
        guard.set_on(false, Some(&mut out));
        assert_eq!(out.len(), written, "already off");
    }

    #[test]
    fn no_stream_is_no_write_but_the_state_follows() {
        let mut guard = MouseGuard::default();
        guard.set_on(true, None::<&mut Vec<u8>>);
        assert!(guard.is_on());
        guard.set_on(false, None::<&mut Vec<u8>>);
        assert!(!guard.is_on());
    }
}
