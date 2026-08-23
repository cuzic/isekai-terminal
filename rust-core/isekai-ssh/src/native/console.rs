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
/// (ANSI) output processing on stdout/stderr (see
/// [`enable_vt_output_processing`]) *and* stdin's VT/mouse-input mode (see
/// `super::console_stdin::apply_interactive_input_mode`) for the session's
/// lifetime, restoring each handle's original mode on drop — bundled here
/// rather than as separate guards because all three are "make this console
/// behave like a VT terminal for the duration of the interactive session"
/// setup done at the exact same call sites (`native/connect.rs` and
/// `native/mux/{mod,client}.rs`, right before/around the actual I/O loop),
/// with the same "restore exactly what was there before" lifecycle.
pub(crate) struct RawModeGuard {
    _private: (),
    /// `(handle, original_mode)` pairs to restore on drop — only the
    /// handles [`enable_vt_output_processing`] actually changed (a handle
    /// whose `GetConsoleMode`/`SetConsoleMode` failed was left alone, so
    /// there's nothing to restore for it). Empty on non-Windows.
    #[cfg(windows)]
    saved_output_modes: Vec<(windows_sys::Win32::Foundation::HANDLE, u32)>,
    /// The stdin console handle's pre-existing mode, from
    /// [`super::console_stdin::apply_interactive_input_mode`] — `None` if
    /// stdin isn't a real console handle. Restored bit-selectively on drop
    /// via [`super::console_stdin::restore_input_mode`]; see that function's
    /// doc for why a wholesale restore would be wrong. `None` on non-Windows
    /// (nothing to restore — this crate never touches input console modes
    /// outside the Windows-native path).
    #[cfg(windows)]
    saved_input_mode: Option<super::console_stdin::InputModeRestore>,
}

impl RawModeGuard {
    /// Enables raw mode for the duration of one local console-facing scope.
    /// Purely a local Win32-console-mode guard: it does **not** know or care
    /// about remote-protocol state (e.g. whether the remote negotiated
    /// mouse-tracking DECSET) — that's owned by [`reset_mouse_tracking`],
    /// called explicitly wherever a session (not just one reconnect attempt)
    /// actually ends. See that function's docs for why this guard used to
    /// bundle that reset into its own `Drop` and why that was wrong.
    pub(crate) fn enable() -> Result<Self> {
        crossterm::terminal::enable_raw_mode().context("failed to enable raw terminal mode")?;
        #[cfg(windows)]
        let saved_output_modes = enable_vt_output_processing();
        // Applies stdin's VT-input/mouse-input/QuickEdit mode for this
        // session's lifetime, scoped here (rather than being set once,
        // permanently, by `console_stdin`'s own singleton setup) precisely
        // so it can be restored below on drop — see `console_stdin`'s module
        // docs for why that matters (QuickEdit, unlike VT input, is a
        // visible feature outside any SSH session).
        #[cfg(windows)]
        let saved_input_mode = super::console_stdin::apply_interactive_input_mode();
        Ok(Self {
            _private: (),
            #[cfg(windows)]
            saved_output_modes,
            #[cfg(windows)]
            saved_input_mode,
        })
    }
}

/// Resolves `std_handle` (e.g. `STD_OUTPUT_HANDLE`/`STD_INPUT_HANDLE`) to a
/// real console character-device handle, or `None` if it's invalid, closed,
/// or redirected (piped/file) — the common "is this actually an interactive
/// console, not a redirected/piped stream" gate every Win32-console-mode
/// function in this crate needs before touching
/// `GetConsoleMode`/`SetConsoleMode`, factored out so the same
/// null/`-1`/`GetFileType` check isn't hand-copied at every call site
/// ([`enable_vt_output_processing`] below, [`RawModeGuard`]'s `Drop`, and
/// `super::console_stdin::apply_interactive_input_mode`/`try_open_console`).
#[cfg(windows)]
pub(crate) fn console_char_handle(std_handle: u32) -> Option<windows_sys::Win32::Foundation::HANDLE> {
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::Storage::FileSystem::GetFileType;
    use windows_sys::Win32::System::Console::GetStdHandle;

    const FILE_TYPE_CHAR: u32 = 0x0002;

    let handle = unsafe { GetStdHandle(std_handle) };
    if handle == std::ptr::null_mut() || handle == (-1isize as HANDLE) {
        return None;
    }
    if unsafe { GetFileType(handle) } != FILE_TYPE_CHAR {
        return None;
    }
    Some(handle)
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
/// Best-effort and silent: [`console_char_handle`]/`GetConsoleMode`/
/// `SetConsoleMode` failing (piped/redirected stdout, a handle that isn't a
/// console at all, or an ancient Windows without VT support) just leaves the
/// mode unchanged — matches `console_stdin.rs::try_open_console`'s same
/// best-effort convention for the input side. Returns the `(handle,
/// original_mode)` pairs that were actually changed, for [`RawModeGuard`]'s
/// `Drop` to restore.
#[cfg(windows)]
fn enable_vt_output_processing() -> Vec<(windows_sys::Win32::Foundation::HANDLE, u32)> {
    use windows_sys::Win32::System::Console::{
        GetConsoleMode, SetConsoleMode, DISABLE_NEWLINE_AUTO_RETURN, ENABLE_VIRTUAL_TERMINAL_PROCESSING,
        STD_ERROR_HANDLE, STD_OUTPUT_HANDLE,
    };

    let mut saved = Vec::new();
    for std_handle in [STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        let Some(handle) = console_char_handle(std_handle) else {
            continue;
        };
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

/// Best-effort resets any mouse-tracking mode (`?1000`/`?1002`/`?1003`/
/// `?1006`) the *remote* side (e.g. `tmux`) turned on via DECSET, by writing
/// the matching DECRST sequence to the local console. Must be called
/// explicitly by whoever knows a whole remote session (not just one
/// reconnect attempt within it) is actually ending — see the call sites in
/// `native/connect.rs` and `native/mux/mod.rs`'s reconnect-loop exit points.
///
/// **Not** part of [`RawModeGuard`]'s `Drop`, on purpose (an earlier version
/// of this code put it there — see PR #102's history — which was itself a
/// real bug: `native/mux/mod.rs::wait_or_abort` creates a fresh
/// `RawModeGuard`, scoped to just one reconnect-backoff wait, for *every*
/// retry attempt. That guard drops — and used to fire this reset — the
/// moment a single attempt ends, which includes an ordinary transport-level
/// reconnect blip where the remote `tmux` session never detached and has no
/// reason to re-send its own DECSET. Resetting mouse tracking from a
/// per-attempt guard therefore silently killed mouse reporting for the rest
/// of an otherwise still-live session after the very first blip.
/// `RawModeGuard` itself has no way to know "is this attempt's end also the
/// session's end" — only the reconnect loop does — so this reset must live
/// at that loop's own true exit points instead of piggybacking on every
/// guard drop.)
///
/// Without ever calling this at all, an abnormal disconnect never delivers
/// the remote's own mouse-tracking-off sequence, and conhost would otherwise
/// keep translating clicks as VT mouse reports (typed as literal
/// escape-sequence text) into whatever reuses this console window next —
/// worse and more confusing than mouse simply not working.
///
/// Two things this must get right that an earlier version of this code
/// didn't (code-review finding, PR #102):
///
/// 1. **Gated on stdout actually being a real console.** An `Exec` session
///    (`isekai-ssh host -- cmd`) shares its `RawModeGuard` scope with an
///    interactive `Shell` session (see `native/connect.rs`), and its stdout
///    is routinely redirected — `isekai-ssh host -- cmd > out.txt` or piped
///    into another program. Writing this sequence unconditionally would
///    append 20 literal bytes to the end of every such capture, silently
///    corrupting non-interactive output that has nothing to do with an
///    interactive mouse session. [`console_char_handle`] is the same "is
///    this a real console, not a redirect" check [`enable_vt_output_processing`]
///    already gates on.
/// 2. **Explicitly flushed.** `std::io::stdout()` is a global, internally
///    line-buffered writer; this payload contains no `\n`, so `write_all`
///    alone only buffers it. `main.rs`'s `fn main` always finishes via
///    `std::process::exit`, which runs no destructors *and* flushes no
///    stdio buffers — an unflushed write here would sit in that buffer and
///    simply be discarded on exit, silently defeating the entire point of
///    this reset.
/// 3. **Scope-enables VT output processing for the duration of this one
///    write, then restores whatever mode it found.** Because this function
///    is called *after* every `RawModeGuard` for the session has already
///    dropped (that's the whole point — see above), it cannot assume
///    [`enable_vt_output_processing`] is still in effect; by the time this
///    runs, the last guard's own `Drop` has already restored stdout's
///    *pre-existing* mode. On a legacy conhost window where VT output
///    processing was off to begin with — precisely the audience
///    `ISEKAI_SSH_CONSOLE_MOUSE` exists for — skipping this would print the
///    DECRST bytes below as literal garbage instead of having conhost
///    interpret them, and would fail to actually reset mouse tracking at
///    all (a real regression an earlier version of this function had: it
///    relied on running while a still-live `RawModeGuard`'s VT-processing
///    mode was in effect, which stopped being true once the reset moved out
///    of `Drop` and into this standalone, later-called function).
#[cfg(windows)]
pub(crate) fn reset_mouse_tracking() {
    use windows_sys::Win32::System::Console::{
        GetConsoleMode, SetConsoleMode, DISABLE_NEWLINE_AUTO_RETURN, ENABLE_VIRTUAL_TERMINAL_PROCESSING,
        STD_OUTPUT_HANDLE,
    };

    let Some(handle) = console_char_handle(STD_OUTPUT_HANDLE) else {
        return;
    };

    let mut mode: u32 = 0;
    let had_mode = unsafe { GetConsoleMode(handle, &mut mode) } != 0;
    if had_mode {
        unsafe { SetConsoleMode(handle, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING | DISABLE_NEWLINE_AUTO_RETURN) };
    }

    let mut stdout = std::io::stdout();
    let _ = std::io::Write::write_all(&mut stdout, b"\x1b[?1006l\x1b[?1003l\x1b[?1002l\x1b[?1000l");
    let _ = std::io::Write::flush(&mut stdout);

    if had_mode {
        unsafe { SetConsoleMode(handle, mode) };
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        // There's nothing sensible to do if disabling raw mode fails on the
        // way out (e.g. the terminal was already torn down), and panicking
        // from a `Drop` impl is its own hazard.
        let _ = crossterm::terminal::disable_raw_mode();
        #[cfg(windows)]
        {
            use windows_sys::Win32::System::Console::SetConsoleMode;
            for (handle, original_mode) in &self.saved_output_modes {
                unsafe { SetConsoleMode(*handle, *original_mode) };
            }
            // Must run *after* `disable_raw_mode()`, not before: crossterm's
            // Windows `disable_raw_mode()` unconditionally ORs
            // `ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT | ENABLE_PROCESSED_INPUT`
            // into whatever the *current* mode is rather than restoring a
            // saved value — restoring the input mode first would just have
            // those three bits re-cleared by `disable_raw_mode()` right
            // afterward, leaving the console with no line input/echo. See
            // `console_stdin::restore_input_mode`'s doc for the full
            // reasoning; that function is itself bit-selective so this
            // ordering is the only thing this call site needs to get right.
            if let Some(saved) = self.saved_input_mode.take() {
                super::console_stdin::restore_input_mode(saved);
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

/// `recv` on the optional resize channel [`spawn_resize_watcher`] returns, or
/// a future that never resolves when there is no watcher (so the `select!`
/// branch consuming this is inert) — shared by the non-mux native `ssh`
/// channel loop (`connect.rs`) and the mux client loop (`mux/client.rs`),
/// which previously each carried an identical copy of this function (plus
/// its own `recv_resize_tests` module) next to their own `select!` loop
/// rather than next to the watcher that actually produces the receiver they
/// consume.
pub(crate) async fn recv_resize(
    rx: &mut Option<tokio::sync::mpsc::UnboundedReceiver<(u32, u32)>>,
) -> Option<(u32, u32)> {
    match rx.as_mut() {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod recv_resize_tests {
    use super::*;
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn recv_resize_with_none_never_resolves() {
        // When the channel is None, recv_resize should return pending forever.
        let mut rx: Option<mpsc::UnboundedReceiver<(u32, u32)>> = None;
        let result = tokio::time::timeout(std::time::Duration::from_millis(10), recv_resize(&mut rx)).await;
        assert!(result.is_err(), "recv_resize with None should never resolve (timeout expected)");
    }

    #[tokio::test]
    async fn recv_resize_with_some_receives_value() {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut rx = Some(rx);
        tx.send((120, 40)).unwrap();
        let result = recv_resize(&mut rx).await;
        assert_eq!(result, Some((120, 40)));
    }

    #[tokio::test]
    async fn recv_resize_with_some_returns_none_when_sender_dropped() {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut rx = Some(rx);
        drop(tx);
        let result = recv_resize(&mut rx).await;
        assert_eq!(result, None);
    }
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

/// Prompts (no local echo, like `ssh(1)`'s own passphrase prompt) for the
/// passphrase to decrypt `identity_path`. `attempt` is 1-based and only
/// changes the prompt wording (a retry after a wrong passphrase says so) —
/// the caller owns the retry-count/give-up policy
/// (`native::connect::try_encrypted_identity`), not this function. Returns
/// `None` if the prompt itself fails (no real terminal attached, stdin
/// closed) or the user enters an empty line (treated as "give up on this
/// key", matching `ssh(1)`'s own behavior of moving on rather than trying to
/// authenticate with an empty passphrase). Uses `rpassword` rather than
/// hand-rolling no-echo input the way [`RawModeGuard`] does for the *whole*
/// session's raw mode — password prompting is exactly the well-solved
/// cross-platform problem `rpassword` exists for (same "don't reimplement a
/// solved problem" stance this module already takes for `crossterm`'s raw
/// mode, see the module doc comment).
///
/// **Only meaningfully exercised against a real interactive terminal** —
/// like [`RawModeGuard::enable`], `rpassword`'s no-echo read needs a real
/// console/tty this sandboxed environment isn't attached to; the retry-count
/// policy this wraps is unit-tested separately in `native::connect` via an
/// injected prompt closure instead of calling this function directly.
pub(crate) fn prompt_passphrase(identity_path: &std::path::Path, attempt: u32) -> Option<String> {
    let prompt = if attempt == 1 {
        format!("Enter passphrase for key '{}': ", identity_path.display())
    } else {
        format!("Enter passphrase for key '{}' (attempt {attempt}): ", identity_path.display())
    };
    let passphrase = rpassword::prompt_password(prompt).ok()?;
    if passphrase.is_empty() {
        None
    } else {
        Some(passphrase)
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
