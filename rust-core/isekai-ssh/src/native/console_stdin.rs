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
//! Mouse events specifically need one more mode bit beyond
//! `ENABLE_VIRTUAL_TERMINAL_INPUT`: as long as QuickEdit Mode is on (the
//! default for classic conhost windows, e.g. plain `cmd.exe`), the console
//! intercepts every mouse click for its own text-selection/copy UI and never
//! delivers it to this process at all — this looks identical to "mouse
//! reporting doesn't work" for anything downstream (e.g. `tmux`'s mouse mode
//! over an SSH session tunneled through `isekai-ssh`), regardless of what the
//! remote side does (and depends on `native::console::enable_vt_output_processing`
//! having already run for the *output* handle, since conhost only starts
//! translating mouse clicks to VT sequences after its output parser has seen
//! the remote's `?1000`/`?1006` DECSET in the first place — both are enabled
//! from the same call site, see [`apply_interactive_input_mode`]'s caller).
//! [`apply_interactive_input_mode`] clears `ENABLE_QUICK_EDIT_MODE` (with
//! `ENABLE_EXTENDED_FLAGS`, required for that change to take effect) and sets
//! `ENABLE_MOUSE_INPUT` for this reason. Unlike `ENABLE_VIRTUAL_TERMINAL_INPUT`
//! alone (which this module used to set permanently, process-wide, with no
//! restore), QuickEdit is a highly visible feature outside of any SSH session
//! (right-click paste, drag-to-select in a plain `cmd.exe`/PowerShell window),
//! so leaving it clobbered after `isekai-ssh` exits would be a real
//! regression — [`apply_interactive_input_mode`]/[`restore_input_mode`] are
//! therefore *not* called from this module's own singleton setup path;
//! `native::console::RawModeGuard` calls them, scoped to (and restored at the
//! end of) each interactive session, the same lifecycle it already uses for
//! the output side's VT processing mode.
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
        ConsoleStdin { buf: Vec::new(), pos: 0 }
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

/// Bits [`apply_interactive_input_mode`] may change and [`restore_input_mode`]
/// must therefore restore selectively (see that function's doc for why a
/// wholesale mode write-back is wrong). `ENABLE_INSERT_MODE` is included even
/// though `apply_interactive_input_mode` never sets or clears it itself,
/// because — like `ENABLE_QUICK_EDIT_MODE` — its meaning is only defined when
/// `ENABLE_EXTENDED_FLAGS` is set (see `SetConsoleMode`'s documented flag
/// semantics), so it is bundled into the same "owned while extended flags are
/// on" restore group defensively.
#[cfg(windows)]
const OWNED_INPUT_BITS: u32 = {
    use windows_sys::Win32::System::Console::{
        ENABLE_INSERT_MODE, ENABLE_MOUSE_INPUT, ENABLE_QUICK_EDIT_MODE, ENABLE_VIRTUAL_TERMINAL_INPUT,
    };
    ENABLE_VIRTUAL_TERMINAL_INPUT | ENABLE_MOUSE_INPUT | ENABLE_QUICK_EDIT_MODE | ENABLE_INSERT_MODE
};

/// Applies the interactive-session console *input* modes stdin needs: VT
/// input (special keys, mouse events) plus the two bits mouse reporting
/// specifically needs beyond that (see the module docs) — `ENABLE_MOUSE_INPUT`
/// and clearing `ENABLE_QUICK_EDIT_MODE` (which requires `ENABLE_EXTENDED_FLAGS`
/// to take effect). Returns the *pre-existing* mode alongside the handle, for
/// [`restore_input_mode`] to restore later; returns `None` (nothing to
/// restore) if this isn't a real console handle, matching [`try_open_console`]'s
/// own `FILE_TYPE_CHAR` gate.
///
/// Called from `native::console::RawModeGuard::enable`, not from this
/// module's own [`ensure_stdin_reader`] singleton setup — see the module docs
/// for why the mode change needs `RawModeGuard`'s per-session save/restore
/// lifecycle rather than being applied once, permanently, for the process.
#[cfg(windows)]
pub(crate) fn apply_interactive_input_mode() -> Option<(windows_sys::Win32::Foundation::HANDLE, u32)> {
    use windows_sys::Win32::Storage::FileSystem::GetFileType;
    use windows_sys::Win32::System::Console::{
        GetConsoleMode, GetStdHandle, SetConsoleMode, ENABLE_EXTENDED_FLAGS, ENABLE_MOUSE_INPUT,
        ENABLE_QUICK_EDIT_MODE, ENABLE_VIRTUAL_TERMINAL_INPUT, STD_INPUT_HANDLE,
    };
    use windows_sys::Win32::Foundation::HANDLE;

    const FILE_TYPE_CHAR: u32 = 0x0002;

    let handle = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
    if handle == std::ptr::null_mut() || handle == (-1isize as HANDLE) {
        return None;
    }
    if unsafe { GetFileType(handle) } != FILE_TYPE_CHAR {
        return None;
    }

    let mut mode: u32 = 0;
    if unsafe { GetConsoleMode(handle, &mut mode) } == 0 {
        return None;
    }

    let new_mode = (mode | ENABLE_VIRTUAL_TERMINAL_INPUT | ENABLE_MOUSE_INPUT | ENABLE_EXTENDED_FLAGS)
        & !ENABLE_QUICK_EDIT_MODE;
    if unsafe { SetConsoleMode(handle, new_mode) } == 0 {
        // `SetConsoleMode` is all-or-nothing: a single flag the running
        // Windows build rejects (e.g. `ENABLE_VIRTUAL_TERMINAL_INPUT` on
        // pre-Anniversary-Update Windows) fails the *whole* call, which would
        // otherwise silently drop the mouse-input/QuickEdit fix too. Retry
        // without VT input so mouse reporting still works even where VT
        // input's own escape-sequence translation for special keys doesn't.
        let fallback = (mode | ENABLE_MOUSE_INPUT | ENABLE_EXTENDED_FLAGS) & !ENABLE_QUICK_EDIT_MODE;
        if unsafe { SetConsoleMode(handle, fallback) } == 0 {
            return None;
        }
    }

    Some((handle, mode))
}

/// Restores the bits [`apply_interactive_input_mode`] changed, without
/// touching anything else — in particular without touching
/// `ENABLE_LINE_INPUT`/`ENABLE_ECHO_INPUT`/`ENABLE_PROCESSED_INPUT`.
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
pub(crate) fn restore_input_mode(handle: windows_sys::Win32::Foundation::HANDLE, original: u32) {
    use windows_sys::Win32::System::Console::{ENABLE_EXTENDED_FLAGS, GetConsoleMode, SetConsoleMode};

    let mut current: u32 = 0;
    if unsafe { GetConsoleMode(handle, &mut current) } == 0 {
        return;
    }
    // Keep every bit `disable_raw_mode` (or anything else) has touched since
    // `apply_interactive_input_mode` ran, except the ones this module owns —
    // restore exactly those to their pre-existing values. `ENABLE_EXTENDED_FLAGS`
    // itself is left set: it has no visible effect on its own (it only gates
    // whether `ENABLE_QUICK_EDIT_MODE`/`ENABLE_INSERT_MODE` writes take
    // effect), and leaving it on is what makes this restore fully take effect
    // in the same call.
    let restored = (current & !OWNED_INPUT_BITS) | (original & OWNED_INPUT_BITS) | ENABLE_EXTENDED_FLAGS;
    unsafe { SetConsoleMode(handle, restored) };
}

#[cfg(windows)]
fn try_open_console() -> Option<UnboundedReceiver<Vec<u8>>> {
    use windows_sys::Win32::Storage::FileSystem::GetFileType;
    use windows_sys::Win32::System::Console::{GetStdHandle, ReadConsoleW, STD_INPUT_HANDLE};
    use windows_sys::Win32::Foundation::HANDLE;

    const FILE_TYPE_CHAR: u32 = 0x0002;

    let handle = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
    if handle == std::ptr::null_mut() || handle == (-1isize as HANDLE) {
        return None;
    }

    // Only read via `ReadConsoleW` for character devices (real consoles) —
    // the mode itself (VT input, mouse input, QuickEdit) is applied by
    // `native::console::RawModeGuard::enable` via
    // [`apply_interactive_input_mode`] before this function ever runs (every
    // call site opens `ConsoleStdin` right after enabling a `RawModeGuard` in
    // the same function), not here — see the module docs.
    if unsafe { GetFileType(handle) } != FILE_TYPE_CHAR {
        return None;
    }

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
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
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
        let rx_lock = STDIN_READER.get().expect("ConsoleStdin is only constructed after ensure_stdin_reader() runs");
        let mut rx = rx_lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
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
