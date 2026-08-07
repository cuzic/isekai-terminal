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

#[cfg(windows)]
fn try_open_console() -> Option<UnboundedReceiver<Vec<u8>>> {
    use windows_sys::Win32::Storage::FileSystem::GetFileType;
    use windows_sys::Win32::System::Console::{
        GetConsoleMode, GetStdHandle, ReadConsoleW, SetConsoleMode,
        ENABLE_VIRTUAL_TERMINAL_INPUT, STD_INPUT_HANDLE,
    };
    use windows_sys::Win32::Foundation::HANDLE;

    const FILE_TYPE_CHAR: u32 = 0x0002;

    let handle = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
    if handle == std::ptr::null_mut() || handle == (-1isize as HANDLE) {
        return None;
    }

    // Only apply VT input mode to character devices (real consoles).
    if unsafe { GetFileType(handle) } != FILE_TYPE_CHAR {
        return None;
    }

    let mut mode: u32 = 0;
    if unsafe { GetConsoleMode(handle, &mut mode) } == 0 {
        return None;
    }

    // Enable VT input so the console generates escape sequences for special
    // keys and mouse events. Best-effort: older Windows (pre-Anniversary
    // Update) may not support this flag — in that case we still use
    // `ReadConsoleW` but without VT sequences (better than ReadFile).
    let new_mode = mode | ENABLE_VIRTUAL_TERMINAL_INPUT;
    unsafe { SetConsoleMode(handle, new_mode) };

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
            // Convert UTF-16 to UTF-8 bytes.
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
