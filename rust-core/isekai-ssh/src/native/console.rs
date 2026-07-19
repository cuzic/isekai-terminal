//! Puts the local console into raw mode for the duration of an interactive
//! SSH session, so keystrokes reach the remote shell byte-for-byte instead
//! of being line-buffered/echoed locally by the console itself — the same
//! thing `ssh(1)` does for an interactive session, which real `ssh(1)` was
//! doing for the Unix ProxyCommand path (this crate never had to worry
//! about it before, since real `ssh.exe` owned the console).
//!
//! Uses [`crossterm`] rather than hand-rolling `SetConsoleMode`/`termios`:
//! unlike the ACL/file-locking code elsewhere in this codebase (which binds
//! narrow, specific Win32 APIs directly via the `windows` crate), terminal
//! raw-mode handling is exactly the well-solved cross-platform problem
//! `crossterm` exists for, not something worth reimplementing.
//!
//! **Only meaningfully exercised on a real interactive terminal** — this
//! sandboxed development/CI environment isn't attached to one, so
//! `RawModeGuard::enable`'s actual `enable_raw_mode()`/`disable_raw_mode()`
//! calls are unverified here beyond "compiles for `x86_64-pc-windows-gnu`
//! and doesn't panic when not attached to a tty" (`crossterm` itself
//! returns an `io::Error` rather than panicking in that case, which this
//! module just propagates).

use anyhow::{Context, Result};

/// Terminal size in columns/rows, matching the order
/// `russh_stream_session::SessionKind::Shell`'s `cols`/`rows` fields want.
/// Falls back to `(80, 24)` (the same default `ssh(1)`/most terminal
/// emulators use) when the size can't be determined — e.g. stdout isn't a
/// real terminal at all (piped/redirected), which shouldn't itself prevent
/// starting a session — **or when it's reported successfully as `0x0`**
/// (Codex review finding: some PTYs return a successful `TIOCGWINSZ` with
/// zero dimensions, e.g. before any resize event has ever landed; a `0x0`
/// remote PTY request is just as unusable as a missing one, so this is
/// treated the same as an error rather than propagated).
pub(crate) fn terminal_size() -> (u32, u32) {
    terminal_size_from(crossterm::terminal::size)
}

/// Pure helper split out of [`terminal_size`] purely so the `0x0` fallback
/// case can be unit-tested with an injected size lookup — `crossterm`
/// itself has no way to fake what `TIOCGWINSZ` reports (same rationale as
/// `agent_auth::resolve_agent_target_from`'s injected env lookup).
fn terminal_size_from(size_lookup: impl Fn() -> std::io::Result<(u16, u16)>) -> (u32, u32) {
    match size_lookup() {
        Ok((cols, rows)) if cols > 0 && rows > 0 => (cols as u32, rows as u32),
        _ => (80, 24),
    }
}

/// RAII guard: puts the local console into raw mode on construction,
/// restores it on drop (including on an early return via `?` or a panic
/// unwind) — mirrors `crossterm::terminal::enable_raw_mode`'s own
/// recommended usage pattern.
pub(crate) struct RawModeGuard {
    _private: (),
}

impl RawModeGuard {
    pub(crate) fn enable() -> Result<Self> {
        crossterm::terminal::enable_raw_mode().context("failed to enable raw terminal mode")?;
        Ok(Self { _private: () })
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        // Best-effort: there's nothing sensible to do if disabling raw mode
        // fails on the way out (e.g. the terminal was already torn down),
        // and panicking from a `Drop` impl is its own hazard.
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_size_never_panics_and_returns_a_nonzero_size() {
        // This sandboxed test environment isn't attached to a real
        // terminal, so this just proves the fallback path (or whatever
        // crossterm reports for a non-tty stdout) is well-formed —
        // it can't verify the real-terminal path at all.
        let (cols, rows) = terminal_size();
        assert!(cols > 0 && rows > 0);
    }

    #[test]
    fn terminal_size_from_falls_back_when_the_lookup_reports_a_successful_0x0() {
        // A PTY can report a successful `TIOCGWINSZ` with zero dimensions
        // (e.g. before any resize event has ever landed) — this must be
        // treated the same as "size unknown", not propagated verbatim.
        assert_eq!(terminal_size_from(|| Ok((0, 0))), (80, 24));
    }

    #[test]
    fn terminal_size_from_falls_back_on_error() {
        assert_eq!(
            terminal_size_from(|| Err(std::io::Error::other("no tty"))),
            (80, 24)
        );
    }

    #[test]
    fn terminal_size_from_passes_through_a_real_size() {
        assert_eq!(terminal_size_from(|| Ok((120, 40))), (120, 40));
    }
}
