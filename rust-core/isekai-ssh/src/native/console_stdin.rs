//! Windows console stdin adapter: replaces `tokio::io::stdin()` with a
//! `ReadConsoleW`-based reader that enables `ENABLE_VIRTUAL_TERMINAL_INPUT`
//! so the console generates VT sequences for special keys (arrows, F1–F12,
//! Home/End, etc.) and mouse events — the same strategy OpenSSH for Windows
//! uses.
//!
//! `ReadFile` on a console handle has two well-known defects:
//!
//! 1. `0x1A` (Ctrl-Z) is treated as EOF regardless of console mode.
//! 2. Mouse events and non-keyboard input are silently discarded.
//!
//! `ReadConsoleW` with `ENABLE_VIRTUAL_TERMINAL_INPUT` fixes both: the
//! console itself encodes everything as VT sequences, and `ReadConsoleW`
//! returns them as wide characters without the `0x1A` EOF trap.
//!
//! Mouse events are deliberately split into two cases:
//!
//! - **Default / ConPTY hosts** (Windows Terminal, VS Code integrated
//!   terminal, WezTerm, etc.): [`apply_interactive_input_mode`] only enables
//!   `ENABLE_VIRTUAL_TERMINAL_INPUT`, matching this crate's pre-PR #102
//!   behavior. A/B testing on Windows Terminal + MSYS2 bash showed remote
//!   `tmux` mouse click/drag/scroll already works in this mode, with no
//!   QuickEdit or `ENABLE_MOUSE_INPUT` changes at all. More importantly,
//!   Microsoft's conhost source (`src/host/getset.cpp`,
//!   `SetConsoleInputModeImpl`) special-cases pseudoconsole clients: when
//!   the input mode transitions to "mouse on and QuickEdit off", conhost
//!   unconditionally sends `ESC[?1003;1006h` to the hosting terminal. That
//!   enables xterm all-motion mouse tracking independent of the remote
//!   session's DECSET state, so the hosting terminal floods this process's
//!   stdin with raw SGR mouse-motion escapes on every pointer move.
//! - **Legacy real conhost windows** (plain `cmd.exe`/PowerShell console host
//!   without ConPTY): QuickEdit can intercept mouse clicks for its own
//!   selection UI before this process ever sees them, which looks like
//!   "mouse reporting doesn't work" downstream. Users in that environment can
//!   explicitly opt in with `ISEKAI_SSH_CONSOLE_MOUSE=1` (also accepts
//!   `true`/`yes`), which restores PR #102's behavior: set
//!   `ENABLE_MOUSE_INPUT`, set `ENABLE_EXTENDED_FLAGS`, and clear
//!   `ENABLE_QUICK_EDIT_MODE`.
//!
//! This is intentionally an opt-in rather than ConPTY auto-detection:
//! `GetConsoleWindow() == NULL` is wrong for pseudoconsole-hosted apps
//! (Windows gives them a hidden non-NULL window), and `$WT_SESSION` misses
//! other ConPTY hosts. Unlike `ENABLE_VIRTUAL_TERMINAL_INPUT` alone (which
//! this module used to set permanently, process-wide, with no restore),
//! QuickEdit is a highly visible feature outside of any SSH session
//! (right-click paste, drag-to-select in a plain `cmd.exe`/PowerShell
//! window), so any opted-in QuickEdit change is scoped through
//! [`apply_interactive_input_mode`]/[`restore_input_mode`] from
//! `native::console::RawModeGuard`, not from this module's own singleton
//! setup path.
//!
//! When stdin is redirected (pipe / file), or on non-Windows, this module
//! falls back to a plain blocking `std::io::stdin().read()` loop on a
//! background thread — the `ReadFile` defects above only apply to real
//! console handles.
//!
//! **Process-wide singleton, not one-per-call**: [`STDIN_READER`]'s
//! background reader thread — `ReadConsoleW` on a real console, or a plain
//! blocking read loop otherwise — only ever terminates on EOF or after a
//! *failed* send to its channel, i.e. after blocking for, and consuming, one
//! more chunk of input that then goes nowhere. If [`ConsoleStdin::open`]
//! spawned a fresh thread/channel on every call, a caller that opens a new
//! `ConsoleStdin` per reconnect attempt (the mux client's `OwnerLost`
//! auto-reconnect loop, and the wait between attempts — see
//! `native::mux::wait_or_abort`) would leave the previous attempt's thread
//! still blocked mid-read after its own `ConsoleStdin` was dropped — racing
//! the new thread for the very next input and silently eating some of it on
//! every reconnect. This isn't hypothetical for redirected stdin either:
//! `tokio::io::stdin()` itself has the exact same one-background-task-per-
//! instance shape internally, so calling it fresh on every `open()` (as an
//! earlier version of this module did for the non-console fallback) hits the
//! identical loss — a blocking OS read in flight when its `Stdin` instance
//! is dropped keeps running to completion with nothing left polling it, and
//! whatever it read is simply discarded. [`STDIN_READER`] is initialized at
//! most once per process, covering every flavor of stdin (console, pipe, and
//! non-Windows) uniformly; every subsequent `open()` reads through the same
//! shared receiver instead of spawning a second reader.

use std::io;
use std::pin::Pin;
use std::sync::{Mutex, OnceLock};
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, ReadBuf};
use tokio::sync::mpsc::UnboundedReceiver;

const LEGACY_CONSOLE_MOUSE_ENV: &str = "ISEKAI_SSH_CONSOLE_MOUSE";

#[cfg(windows)]
const ENABLE_VIRTUAL_TERMINAL_INPUT: u32 =
    windows_sys::Win32::System::Console::ENABLE_VIRTUAL_TERMINAL_INPUT;
#[cfg(windows)]
const ENABLE_MOUSE_INPUT: u32 = windows_sys::Win32::System::Console::ENABLE_MOUSE_INPUT;
#[cfg(windows)]
const ENABLE_QUICK_EDIT_MODE: u32 = windows_sys::Win32::System::Console::ENABLE_QUICK_EDIT_MODE;
#[cfg(windows)]
const ENABLE_EXTENDED_FLAGS: u32 = windows_sys::Win32::System::Console::ENABLE_EXTENDED_FLAGS;
#[cfg(windows)]
const ENABLE_INSERT_MODE: u32 = windows_sys::Win32::System::Console::ENABLE_INSERT_MODE;

#[cfg(not(windows))]
const ENABLE_VIRTUAL_TERMINAL_INPUT: u32 = 0x0200;
#[cfg(not(windows))]
const ENABLE_MOUSE_INPUT: u32 = 0x0010;
#[cfg(not(windows))]
const ENABLE_QUICK_EDIT_MODE: u32 = 0x0040;
#[cfg(not(windows))]
const ENABLE_EXTENDED_FLAGS: u32 = 0x0080;
#[cfg(not(windows))]
const ENABLE_INSERT_MODE: u32 = 0x0020;

/// A stdin reader that implements [`AsyncRead`], backed by the process-wide
/// [`STDIN_READER`] background thread (spawned at most once — see the
/// module docs). Each instance only keeps its own leftover-bytes buffer for
/// a chunk that didn't fully fit in a caller's `buf` on a previous read.
pub(crate) struct ConsoleStdin {
    buf: Vec<u8>,
    pos: usize,
}

static STDIN_READER: OnceLock<Mutex<UnboundedReceiver<Vec<u8>>>> = OnceLock::new();

impl ConsoleStdin {
    /// Opens stdin, enabling `ENABLE_VIRTUAL_TERMINAL_INPUT` if it's a
    /// Windows console handle. Safe to call more than once per process (see
    /// module docs) — later calls reuse the first call's background reader
    /// thread instead of spawning a new one.
    pub(crate) fn open() -> Self {
        ensure_stdin_reader();
        ConsoleStdin {
            buf: Vec::new(),
            pos: 0,
        }
    }
}

/// Ensures [`STDIN_READER`] is initialized, spawning the background reader
/// thread on the first call only — cached implicitly by `STDIN_READER`'s
/// presence, so repeated calls are cheap.
fn ensure_stdin_reader() {
    if STDIN_READER.get().is_some() {
        return;
    }
    #[cfg(windows)]
    let rx = try_open_console().unwrap_or_else(spawn_pipe_reader);
    #[cfg(not(windows))]
    let rx = spawn_pipe_reader();
    // If another call already won this race (shouldn't happen in practice:
    // callers open a new attempt only after the previous one's ConsoleStdin
    // was fully dropped, so this is never actually concurrent — but
    // OnceLock::set losing a race is not a bug, just a discarded `rx` whose
    // now-orphaned thread will exit on its next failed send, same as a
    // non-singleton reader's shutdown path), keep whichever receiver won.
    let _ = STDIN_READER.set(Mutex::new(rx));
}

/// Plain blocking `std::io::stdin().read()` loop on a background thread —
/// the fallback for redirected stdin (pipe/file) on Windows, and the only
/// path on non-Windows. Ends (and the channel closes) on EOF or a read
/// error.
fn spawn_pipe_reader() -> UnboundedReceiver<Vec<u8>> {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    std::thread::spawn(move || {
        use std::io::Read;
        let mut stdin = std::io::stdin();
        let mut buf = [0u8; 4096];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });
    rx
}

fn legacy_console_mouse_bits() -> u32 {
    ENABLE_MOUSE_INPUT | ENABLE_QUICK_EDIT_MODE | ENABLE_INSERT_MODE
}

fn owned_input_bits(apply_mouse_bits: bool) -> u32 {
    if apply_mouse_bits {
        ENABLE_VIRTUAL_TERMINAL_INPUT | legacy_console_mouse_bits()
    } else {
        ENABLE_VIRTUAL_TERMINAL_INPUT
    }
}

pub(crate) fn wants_legacy_console_mouse_bits(opt_in: Option<&str>) -> bool {
    matches!(opt_in, Some("1" | "true" | "yes"))
}

fn interactive_input_mode(current: u32, apply_mouse_bits: bool) -> u32 {
    if apply_mouse_bits {
        (current | ENABLE_VIRTUAL_TERMINAL_INPUT | ENABLE_MOUSE_INPUT | ENABLE_EXTENDED_FLAGS)
            & !ENABLE_QUICK_EDIT_MODE
    } else {
        current | ENABLE_VIRTUAL_TERMINAL_INPUT
    }
}

fn restore_owned_input_mode(current: u32, original: u32, owned_bits: u32) -> u32 {
    (current & !owned_bits) | (original & owned_bits)
}

#[cfg(windows)]
pub(crate) struct InputModeRestore {
    handle: windows_sys::Win32::Foundation::HANDLE,
    original_mode: u32,
    owned_bits: u32,
}

/// Applies the interactive-session console *input* modes stdin needs. By
/// default this only sets `ENABLE_VIRTUAL_TERMINAL_INPUT`; the legacy
/// `ENABLE_MOUSE_INPUT`/QuickEdit/extended-flags trio is applied only when
/// `ISEKAI_SSH_CONSOLE_MOUSE` is explicitly set to `1`, `true`, or `yes`.
/// See the module docs for why ConPTY-hosted terminals must not get that
/// trio by default even though classic conhost users may still need it.
///
/// Returns the *pre-existing* mode and the exact bit mask this call owns, for
/// [`restore_input_mode`] to restore later; returns `None` (nothing to
/// restore, and nothing changed) if this isn't a real console handle, or if
/// `SetConsoleMode` rejects the requested flag set outright. This crate's
/// `ReadConsoleW`-based reader below only ever receives mouse events via VT
/// input's own escape-sequence translation — see the module docs — so on a
/// hypothetical Windows old enough to reject `ENABLE_VIRTUAL_TERMINAL_INPUT`,
/// there is no partial mode this function could apply that would make mouse
/// reporting work anyway. Retrying with a reduced flag set would only
/// silently change the user's QuickEdit setting for no actual benefit, so
/// this deliberately does not retry — see PR #102's review for why an
/// earlier version of this function's "retry without VT input" fallback was
/// wrong.
///
/// Called from `native::console::RawModeGuard::enable`, not from this
/// module's own [`ensure_stdin_reader`] singleton setup — see the module docs
/// for why the mode change needs `RawModeGuard`'s per-session save/restore
/// lifecycle rather than being applied once, permanently, for the process.
#[cfg(windows)]
pub(crate) fn apply_interactive_input_mode() -> Option<InputModeRestore> {
    use windows_sys::Win32::System::Console::{GetConsoleMode, SetConsoleMode, STD_INPUT_HANDLE};

    let handle = super::console::console_char_handle(STD_INPUT_HANDLE)?;
    let apply_mouse_bits =
        wants_legacy_console_mouse_bits(std::env::var(LEGACY_CONSOLE_MOUSE_ENV).ok().as_deref());

    let mut mode: u32 = 0;
    if unsafe { GetConsoleMode(handle, &mut mode) } == 0 {
        return None;
    }

    let new_mode = interactive_input_mode(mode, apply_mouse_bits);
    if unsafe { SetConsoleMode(handle, new_mode) } == 0 {
        return None;
    }

    Some(InputModeRestore {
        handle,
        original_mode: mode,
        owned_bits: owned_input_bits(apply_mouse_bits),
    })
}

/// Restores the bits [`apply_interactive_input_mode`] changed, without
/// touching anything else — in particular without touching
/// `ENABLE_LINE_INPUT`/`ENABLE_ECHO_INPUT`/`ENABLE_PROCESSED_INPUT`, and
/// (unlike an earlier version of this function — PR #102's review) without
/// leaving `ENABLE_EXTENDED_FLAGS` permanently set on a console that never
/// had it.
///
/// This must be a **bit-selective** read-modify-write, not
/// `SetConsoleMode(handle, original)`: `RawModeGuard::drop` calls
/// `crossterm::terminal::disable_raw_mode()` before this runs, which
/// unconditionally ORs `ENABLE_LINE_INPUT | ENABLE_ECHO_INPUT |
/// ENABLE_PROCESSED_INPUT` back into the *current* mode rather than restoring
/// any saved value (`crossterm`'s Windows backend doesn't save one). Writing
/// `original` wholesale here would silently re-clear those three bits right
/// after `disable_raw_mode` set them, leaving the console with no line input
/// and no echo for the rest of that window's life — a strictly worse bug
/// than the one this function exists to clean up after.
#[cfg(windows)]
pub(crate) fn restore_input_mode(saved: InputModeRestore) {
    use windows_sys::Win32::System::Console::{GetConsoleMode, SetConsoleMode};

    let mut current: u32 = 0;
    if unsafe { GetConsoleMode(saved.handle, &mut current) } == 0 {
        return;
    }
    if saved.owned_bits == ENABLE_VIRTUAL_TERMINAL_INPUT {
        let restored = restore_owned_input_mode(current, saved.original_mode, saved.owned_bits);
        unsafe { SetConsoleMode(saved.handle, restored) };
        return;
    }
    // Step 1: restore `ENABLE_QUICK_EDIT_MODE`/`ENABLE_INSERT_MODE` (the bits
    // in `saved.owned_bits` besides VT input/mouse input) to their
    // pre-existing values. `ENABLE_EXTENDED_FLAGS` must be part of *this*
    // call for that write to take effect at all, regardless of whether the
    // original mode had it set — see `SetConsoleMode`'s documented flag
    // semantics.
    let restored = restore_owned_input_mode(current, saved.original_mode, saved.owned_bits)
        | ENABLE_EXTENDED_FLAGS;
    if unsafe { SetConsoleMode(saved.handle, restored) } == 0 {
        return;
    }
    // Step 2: if the console never had `ENABLE_EXTENDED_FLAGS` set before
    // [`apply_interactive_input_mode`] turned it on, clear it back off now —
    // as its own, separate call, since *un*-setting `ENABLE_EXTENDED_FLAGS`
    // is not itself gated by `ENABLE_EXTENDED_FLAGS` being present in the
    // same call (only changes to `ENABLE_QUICK_EDIT_MODE`/`ENABLE_INSERT_MODE`
    // are, and step 1 already applied those). This is what makes the restore
    // byte-exact rather than leaving a mode bit permanently flipped that
    // `apply_interactive_input_mode` was the only reason it was ever on.
    if saved.original_mode & ENABLE_EXTENDED_FLAGS == 0 {
        unsafe { SetConsoleMode(saved.handle, restored & !ENABLE_EXTENDED_FLAGS) };
    }
}

#[cfg(windows)]
fn try_open_console() -> Option<UnboundedReceiver<Vec<u8>>> {
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::System::Console::{ReadConsoleW, STD_INPUT_HANDLE};

    // The mode itself (VT input, mouse input, QuickEdit) is applied by
    // `native::console::RawModeGuard::enable` via
    // [`apply_interactive_input_mode`] before this function ever runs (every
    // call site opens `ConsoleStdin` right after enabling a `RawModeGuard` in
    // the same function), not here — see the module docs.
    let handle = super::console::console_char_handle(STD_INPUT_HANDLE)?;

    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();

    // Cast to isize for Send safety across the thread boundary.
    let handle_raw = handle as isize;

    std::thread::spawn(move || {
        let handle = handle_raw as HANDLE;
        // 256 wide chars is enough for a typical VT sequence plus generous
        // headroom for long paste events.
        let mut wbuf: [u16; 256] = [0; 256];
        loop {
            let mut nread: u32 = 0;
            let ret = unsafe {
                ReadConsoleW(
                    handle,
                    wbuf.as_mut_ptr() as *mut std::ffi::c_void,
                    wbuf.len() as u32,
                    &mut nread,
                    std::ptr::null_mut(),
                )
            };
            if ret == 0 || nread == 0 {
                break;
            }
            // Convert UTF-16 to UTF-8 bytes. Fine for SGR mouse sequences
            // (`ESC[<...M`/`m`, pure ASCII regardless of coordinate size) now
            // that mouse events reach here at all — but legacy X10 mouse mode
            // (`?1000` without `?1006`) encodes coordinates as `32 + value`
            // directly in the byte stream, so a coordinate past ~223 becomes
            // a non-ASCII UTF-16 unit here and would corrupt through this
            // lossy round-trip. Low practical risk (tmux negotiates SGR
            // whenever terminfo advertises it, which it does here), but worth
            // knowing if a legacy-mouse report ever looks corrupted rather
            // than simply absent.
            let utf16: Vec<u16> = wbuf[..nread as usize].to_vec();
            let utf8: Vec<u8> = String::from_utf16_lossy(&utf16).into_bytes();
            if tx.send(utf8).is_err() {
                break;
            }
        }
    });

    Some(rx)
}

impl AsyncRead for ConsoleStdin {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        // Drain buffered data first.
        if self.pos < self.buf.len() {
            let remaining = self.buf.len() - self.pos;
            let to_write = remaining.min(buf.remaining());
            buf.put_slice(&self.buf[self.pos..self.pos + to_write]);
            self.pos += to_write;
            if self.pos >= self.buf.len() {
                self.buf.clear();
                self.pos = 0;
            }
            return Poll::Ready(Ok(()));
        }

        // Try to get more data from the shared background thread's channel.
        // The lock is only ever held for the duration of this synchronous
        // `poll_recv` call, never across an await point, so contention is
        // not a concern even though callers are expected to be sequential.
        let rx_lock = STDIN_READER
            .get()
            .expect("ConsoleStdin is only constructed after ensure_stdin_reader() runs");
        let mut rx = rx_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match rx.poll_recv(cx) {
            Poll::Ready(Some(data)) => {
                let to_write = data.len().min(buf.remaining());
                buf.put_slice(&data[..to_write]);
                if to_write < data.len() {
                    self.buf = data;
                    self.pos = to_write;
                }
                Poll::Ready(Ok(()))
            }
            Poll::Ready(None) => Poll::Ready(Ok(())), // thread ended = EOF
            Poll::Pending => Poll::Pending,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_console_mouse_bits_opt_in_accepts_only_documented_values() {
        assert!(wants_legacy_console_mouse_bits(Some("1")));
        assert!(wants_legacy_console_mouse_bits(Some("true")));
        assert!(wants_legacy_console_mouse_bits(Some("yes")));
        assert!(!wants_legacy_console_mouse_bits(None));
        assert!(!wants_legacy_console_mouse_bits(Some("")));
        assert!(!wants_legacy_console_mouse_bits(Some("0")));
        assert!(!wants_legacy_console_mouse_bits(Some("false")));
        assert!(!wants_legacy_console_mouse_bits(Some("TRUE")));
    }

    #[test]
    fn default_interactive_input_mode_only_sets_vt_input_and_leaves_quickedit_unchanged() {
        // Regression guard for ConPTY hosts: conhost's SetConsoleInputModeImpl
        // sends ESC[?1003;1006h to the hosting terminal when the input mode
        // transitions to "mouse on and QuickEdit off", causing raw all-motion
        // SGR mouse reports to land at the remote shell prompt. The default
        // path must therefore match the pre-PR #102 behavior: only VT input
        // is owned by this session, and QuickEdit is neither cleared nor set.
        let current = ENABLE_MOUSE_INPUT | ENABLE_QUICK_EDIT_MODE | ENABLE_INSERT_MODE;
        let new_mode = interactive_input_mode(current, false);
        assert_eq!(new_mode, current | ENABLE_VIRTUAL_TERMINAL_INPUT);
        assert_eq!(
            new_mode & ENABLE_QUICK_EDIT_MODE,
            current & ENABLE_QUICK_EDIT_MODE,
            "default mode must not change QuickEdit"
        );
        assert_eq!(owned_input_bits(false), ENABLE_VIRTUAL_TERMINAL_INPUT);
    }

    #[test]
    fn default_interactive_input_mode_preserves_quickedit_when_it_started_off() {
        let current = ENABLE_MOUSE_INPUT | ENABLE_INSERT_MODE;
        let new_mode = interactive_input_mode(current, false);
        assert_eq!(new_mode, current | ENABLE_VIRTUAL_TERMINAL_INPUT);
        assert_eq!(
            new_mode & ENABLE_QUICK_EDIT_MODE,
            current & ENABLE_QUICK_EDIT_MODE,
            "default mode must leave an already-off QuickEdit bit alone too"
        );
    }

    #[test]
    fn default_restore_only_restores_vt_input_and_does_not_clobber_quickedit_or_mouse_bits() {
        let original = ENABLE_QUICK_EDIT_MODE;
        let current = ENABLE_VIRTUAL_TERMINAL_INPUT | ENABLE_MOUSE_INPUT | ENABLE_INSERT_MODE;
        let restored = restore_owned_input_mode(current, original, owned_input_bits(false));
        assert_eq!(restored & ENABLE_VIRTUAL_TERMINAL_INPUT, 0);
        assert_eq!(restored & ENABLE_MOUSE_INPUT, current & ENABLE_MOUSE_INPUT);
        assert_eq!(restored & ENABLE_INSERT_MODE, current & ENABLE_INSERT_MODE);
        assert_eq!(
            restored & ENABLE_QUICK_EDIT_MODE,
            current & ENABLE_QUICK_EDIT_MODE
        );
    }

    #[test]
    fn opt_in_interactive_input_mode_preserves_pr102_legacy_conhost_behavior() {
        let current = ENABLE_QUICK_EDIT_MODE | ENABLE_INSERT_MODE;
        let new_mode = interactive_input_mode(current, true);
        assert_ne!(new_mode & ENABLE_VIRTUAL_TERMINAL_INPUT, 0);
        assert_ne!(new_mode & ENABLE_MOUSE_INPUT, 0);
        assert_ne!(new_mode & ENABLE_EXTENDED_FLAGS, 0);
        assert_eq!(new_mode & ENABLE_QUICK_EDIT_MODE, 0);
        assert_eq!(
            owned_input_bits(true),
            ENABLE_VIRTUAL_TERMINAL_INPUT
                | ENABLE_MOUSE_INPUT
                | ENABLE_QUICK_EDIT_MODE
                | ENABLE_INSERT_MODE
        );
    }

    #[test]
    fn opt_in_restore_owns_the_legacy_mouse_quickedit_insert_group() {
        let original = ENABLE_QUICK_EDIT_MODE | ENABLE_INSERT_MODE;
        let current = ENABLE_VIRTUAL_TERMINAL_INPUT | ENABLE_MOUSE_INPUT;
        let restored = restore_owned_input_mode(current, original, owned_input_bits(true));
        assert_eq!(restored & ENABLE_VIRTUAL_TERMINAL_INPUT, 0);
        assert_eq!(restored & ENABLE_MOUSE_INPUT, 0);
        assert_ne!(restored & ENABLE_QUICK_EDIT_MODE, 0);
        assert_ne!(restored & ENABLE_INSERT_MODE, 0);
    }
}
