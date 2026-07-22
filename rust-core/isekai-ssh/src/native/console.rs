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
/// recommended usage pattern. On Windows, also best-effort enables VT
/// (ANSI) output processing on stdout/stderr for the session's lifetime,
/// restoring each handle's original mode on drop (see
/// [`enable_vt_output_processing`]) — bundled here rather than as a
/// separate guard because both are "make this console behave like a VT
/// terminal for the duration of the interactive session" setup done at the
/// exact same call site (`native/connect.rs`, right before
/// `run_shell_io_loop`), with the same "restore exactly what was there
/// before" lifecycle.
pub(crate) struct RawModeGuard {
    _private: (),
    /// `(handle, original_mode)` pairs to restore on drop — only the
    /// handles [`enable_vt_output_processing`] actually changed (a handle
    /// whose `GetConsoleMode`/`SetConsoleMode` failed was left alone, so
    /// there's nothing to restore for it). Empty on non-Windows.
    #[cfg(windows)]
    saved_output_modes: Vec<(windows_sys::Win32::Foundation::HANDLE, u32)>,
}

impl RawModeGuard {
    pub(crate) fn enable() -> Result<Self> {
        crossterm::terminal::enable_raw_mode().context("failed to enable raw terminal mode")?;
        #[cfg(windows)]
        let saved_output_modes = enable_vt_output_processing();
        Ok(Self {
            _private: (),
            #[cfg(windows)]
            saved_output_modes,
        })
    }
}

/// Enables `ENABLE_VIRTUAL_TERMINAL_PROCESSING` (plus
/// `DISABLE_NEWLINE_AUTO_RETURN`, so a bare `\n` doesn't get an implicit
/// `\r` inserted by the console host on top of whatever cursor positioning
/// the remote app already did) on stdout and stderr — the same thing
/// Win32-OpenSSH's `ssh.exe` does for its own console output (`ssh.exe`
/// itself saves the pre-existing mode and restores it on exit, which is why
/// [`RawModeGuard`] does too rather than leaving the changed mode in place
/// — an un-restored `DISABLE_NEWLINE_AUTO_RETURN` would otherwise persist
/// on the user's real console after `isekai-ssh` exits, "staircasing" the
/// output of any later program that emits a bare `\n` expecting the
/// console's normal implicit-CR behavior).
///
/// Without this, a console host that doesn't already default it on (plain
/// `cmd.exe`/legacy `conhost`, as opposed to modern Windows Terminal, which
/// usually enables it itself) renders every VT/ANSI sequence the remote
/// sends — colors, cursor movement, screen/line clears, synchronized-update
/// mode — as literal garbage bytes instead of interpreting them, rather
/// than actually moving the cursor or erasing anything. A full-screen app
/// that leans on VT sequences heavily (e.g. an Ink-based TUI) is far more
/// visibly broken by this than a plain shell prompt's occasional color
/// code, which matches the native-pty-gaps bug report this fixes.
///
/// Best-effort and silent: `GetStdHandle`/`GetConsoleMode`/`SetConsoleMode`
/// failing (piped/redirected stdout, a handle that isn't a console at all,
/// or an ancient Windows without VT support) just leaves the mode
/// unchanged — matches `console_stdin.rs::try_open_console`'s same
/// best-effort convention for the input side. Returns the `(handle,
/// original_mode)` pairs that were actually changed, for [`RawModeGuard`]'s
/// `Drop` to restore.
#[cfg(windows)]
fn enable_vt_output_processing() -> Vec<(windows_sys::Win32::Foundation::HANDLE, u32)> {
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::System::Console::{
        GetConsoleMode, GetStdHandle, SetConsoleMode, DISABLE_NEWLINE_AUTO_RETURN,
        ENABLE_VIRTUAL_TERMINAL_PROCESSING, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE,
    };

    let mut saved = Vec::new();
    for std_handle in [STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        let handle = unsafe { GetStdHandle(std_handle) };
        if handle == std::ptr::null_mut() || handle == (-1isize as HANDLE) {
            continue;
        }
        let mut mode: u32 = 0;
        if unsafe { GetConsoleMode(handle, &mut mode) } == 0 {
            continue;
        }
        let new_mode = mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING | DISABLE_NEWLINE_AUTO_RETURN;
        if unsafe { SetConsoleMode(handle, new_mode) } != 0 {
            saved.push((handle, mode));
        }
    }
    saved
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        // Best-effort: there's nothing sensible to do if disabling raw mode
        // fails on the way out (e.g. the terminal was already torn down),
        // and panicking from a `Drop` impl is its own hazard.
        let _ = crossterm::terminal::disable_raw_mode();
        #[cfg(windows)]
        {
            use windows_sys::Win32::System::Console::SetConsoleMode;
            for (handle, original_mode) in &self.saved_output_modes {
                unsafe { SetConsoleMode(*handle, *original_mode) };
            }
        }
    }
}

/// Spawns a background watcher that detects terminal resize events and
/// sends the new (cols, rows) over the returned channel whenever the
/// size changes. Returns `None` if resize detection is not available
/// (e.g. piped stdin, or the signal handler couldn't be registered).
///
/// On Unix the watcher hooks `SIGWINCH` — zero overhead, no polling.
/// On Windows the watcher polls [`terminal_size`] every 200 ms
/// (the same trade-off every Windows terminal app makes: `ReadConsoleInput`
/// and raw stdin reads conflict, so event-driven resize isn't practical).
pub(crate) fn spawn_resize_watcher() -> Option<tokio::sync::mpsc::UnboundedReceiver<(u32, u32)>> {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();

    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut sig = match signal(SignalKind::window_change()) {
            Ok(sig) => sig,
            Err(_) => return None,
        };
        tokio::spawn(async move {
            loop {
                sig.recv().await;
                let (cols, rows) = terminal_size();
                if tx.send((cols, rows)).is_err() {
                    break;
                }
            }
        });
    }

    #[cfg(not(unix))]
    {
        std::thread::spawn(move || {
            let mut last = (0u32, 0u32);
            loop {
                std::thread::sleep(std::time::Duration::from_millis(200));
                let current = terminal_size();
                if current != last {
                    last = current;
                    if tx.send(current).is_err() {
                        break;
                    }
                }
            }
        });
    }

    Some(rx)
}

// Builds the terminal mode list for `request_pty` from the local terminal
/// settings. On Unix this reads the actual `termios` via `tcgetattr`; on
/// other platforms it returns a minimal default set matching a normal
/// cooked-mode interactive terminal (echo/canon/isig **on**, standard
/// special characters). When `tcgetattr` fails (e.g. CI sandbox without a
/// terminal), the same default set is returned.
///
/// This must **not** ask the remote pty for raw mode: `RawModeGuard`
/// separately puts the *local* console into raw mode so its own console
/// driver doesn't double-echo keystrokes, exactly mirroring what `ssh(1)`
/// does for the Unix ProxyCommand path (send the terminal's actual
/// [cooked-mode] modes to the server, then switch the local terminal to raw
/// mode afterwards). Sending `ECHO=0`/`ICANON=0`/`ISIG=0` here as well
/// configures the *remote* pty itself with echo disabled, so neither side
/// echoes typed input at all — a real bug this default set had until fixed
/// (native-pty-gaps branch review): plain shells went completely blind
/// (input never appeared, only command output did), since a normal shell
/// relies on the remote pty's own echo rather than doing its own.
/// Full-screen TUI apps (e.g. Claude Code) were largely unaffected because
/// they reconfigure the pty's mode themselves on startup regardless of what
/// this initial request set.
pub(crate) fn build_terminal_modes() -> Vec<(russh::Pty, u32)> {
    #[cfg(unix)]
    {
        let mut modes = Vec::with_capacity(32);
        unsafe {
            let mut termios: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(libc::STDIN_FILENO, &mut termios) == 0 {
                // Special characters (c_cc array)
                macro_rules! push_cc { ($pty:ident, $idx:ident) => {
                    modes.push((russh::Pty::$pty, termios.c_cc[libc::$idx] as u32));
                }}
                push_cc!(VINTR, VINTR);
                push_cc!(VQUIT, VQUIT);
                push_cc!(VERASE, VERASE);
                push_cc!(VKILL, VKILL);
                push_cc!(VEOF, VEOF);
                push_cc!(VEOL, VEOL);
                push_cc!(VEOL2, VEOL2);
                push_cc!(VSTART, VSTART);
                push_cc!(VSTOP, VSTOP);
                push_cc!(VSUSP, VSUSP);
                #[cfg(any(target_os = "macos", target_os = "freebsd", target_os = "openbsd", target_os = "netbsd"))]
                push_cc!(VDSUSP, VDSUSP);
                push_cc!(VREPRINT, VREPRINT);
                push_cc!(VWERASE, VWERASE);
                push_cc!(VLNEXT, VLNEXT);
                push_cc!(VDISCARD, VDISCARD);
                #[cfg(any(target_os = "macos", target_os = "freebsd", target_os = "openbsd", target_os = "netbsd"))]
                push_cc!(VSTATUS, VSTATUS);

                // Input flags from c_iflag
                macro_rules! push_flag { ($pty:ident, $flag:ident) => {
                    modes.push((russh::Pty::$pty, if termios.c_iflag & libc::$flag != 0 { 1 } else { 0 }));
                }}
                push_flag!(IGNPAR, IGNPAR);
                push_flag!(PARMRK, PARMRK);
                push_flag!(INPCK, INPCK);
                push_flag!(ISTRIP, ISTRIP);
                push_flag!(INLCR, INLCR);
                push_flag!(IGNCR, IGNCR);
                push_flag!(ICRNL, ICRNL);
                push_flag!(IXON, IXON);
                push_flag!(IXANY, IXANY);
                push_flag!(IXOFF, IXOFF);
                push_flag!(IMAXBEL, IMAXBEL);
                #[cfg(any(target_os = "linux", target_os = "android"))]
                push_flag!(IUTF8, IUTF8);

                // Local flags from c_lflag
                macro_rules! push_lflag { ($pty:ident, $flag:ident) => {
                    modes.push((russh::Pty::$pty, if termios.c_lflag & libc::$flag != 0 { 1 } else { 0 }));
                }}
                push_lflag!(ISIG, ISIG);
                push_lflag!(ICANON, ICANON);
                push_lflag!(ECHO, ECHO);
                push_lflag!(ECHOE, ECHOE);
                push_lflag!(ECHOK, ECHOK);
                push_lflag!(ECHONL, ECHONL);
                push_lflag!(NOFLSH, NOFLSH);
                push_lflag!(TOSTOP, TOSTOP);
                push_lflag!(IEXTEN, IEXTEN);
                push_lflag!(ECHOCTL, ECHOCTL);
                push_lflag!(ECHOKE, ECHOKE);

                // Output flags from c_oflag
                macro_rules! push_oflag { ($pty:ident, $flag:ident) => {
                    modes.push((russh::Pty::$pty, if termios.c_oflag & libc::$flag != 0 { 1 } else { 0 }));
                }}
                push_oflag!(OPOST, OPOST);
                push_oflag!(ONLCR, ONLCR);
                push_oflag!(OCRNL, OCRNL);
                push_oflag!(ONOCR, ONOCR);
                push_oflag!(ONLRET, ONLRET);

                // Speed (baud rate). `libc::speed_t` is `u32` on Linux but
                // `u64` on macOS — `as u32` is lossless in practice (real
                // baud rates never approach `u32::MAX`).
                let ispeed = libc::cfgetispeed(&termios) as u32;
                let ospeed = libc::cfgetospeed(&termios) as u32;
                modes.push((russh::Pty::TTY_OP_ISPEED, ispeed));
                modes.push((russh::Pty::TTY_OP_OSPEED, ospeed));
                return modes;
            }
        }
        // tcgetattr failed (no terminal): fall through to default set.
    }

    // Default set: cooked-mode terminal (echo/canon/isig on) with standard
    // special characters — see this function's doc comment for why this
    // must not request raw mode from the remote pty.
    vec![
        (russh::Pty::ECHO, 1),
        (russh::Pty::ICANON, 1),
        (russh::Pty::ISIG, 1),
        (russh::Pty::VINTR, 3),   // Ctrl-C
        (russh::Pty::VEOF, 4),    // Ctrl-D
        (russh::Pty::VERASE, 127), // Backspace
        (russh::Pty::VKILL, 21),  // Ctrl-U
        (russh::Pty::VQUIT, 28),  // Ctrl-\
        (russh::Pty::VSUSP, 26),  // Ctrl-Z
        (russh::Pty::VSTART, 17), // Ctrl-Q
        (russh::Pty::VSTOP, 19),  // Ctrl-S
    ]
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

    #[test]
    fn build_terminal_modes_returns_valid_list() {
        let modes = build_terminal_modes();
        // Always returns at least the default set, even without a terminal.
        assert!(!modes.is_empty(), "terminal modes list should not be empty");
        for (pty, _value) in &modes {
            let _ = format!("{pty:?}");
        }
    }

    #[test]
    fn build_terminal_modes_default_set_does_not_disable_echo() {
        // This sandboxed test environment has no real tty, so `tcgetattr`
        // fails and this always exercises the fallback "default set" —
        // the same one a real Windows session unconditionally gets. It must
        // request a normal cooked-mode remote pty (echo/canon/isig on): a
        // regression here silently makes every plain shell session blind
        // (see this function's doc comment).
        let modes = build_terminal_modes();
        let value_of = |pty: russh::Pty| {
            modes.iter().find(|(p, _)| *p == pty).map(|(_, v)| *v)
        };
        assert_eq!(value_of(russh::Pty::ECHO), Some(1), "ECHO must be enabled on the remote pty");
        assert_eq!(value_of(russh::Pty::ICANON), Some(1), "ICANON must be enabled on the remote pty");
        assert_eq!(value_of(russh::Pty::ISIG), Some(1), "ISIG must be enabled on the remote pty");
    }
}
