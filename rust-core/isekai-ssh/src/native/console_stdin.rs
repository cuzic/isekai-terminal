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
//! When stdin is redirected (pipe / file), this module falls back to plain
//! `tokio::io::stdin()` — the `ReadFile` defects only apply to console
//! handles, not to pipes.
//!
//! **Process-wide singleton, not one-per-call** (windows): the background
//! `ReadConsoleW` thread only ever terminates after a *failed* send to its
//! channel — i.e. after blocking for, and consuming, one more keystroke
//! that then goes nowhere (`try_open_console`'s `tx.send(...).is_err()`
//! check below). If [`ConsoleStdin::open`] spawned a fresh thread/channel
//! on every call, a caller that opens a new `ConsoleStdin` per reconnect
//! attempt (the mux client's `OwnerLost` auto-reconnect loop) would leave
//! the previous attempt's thread still blocked reading CONIN$ after its own
//! `ConsoleStdin` was dropped — racing the new thread for the very next
//! keystroke and silently eating one on every reconnect. `CONSOLE_READER`
//! is initialized at most once per process; every subsequent `open()` reads
//! through the same shared receiver instead of spawning a second reader.

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, ReadBuf};
#[cfg(windows)]
use std::sync::{Mutex, OnceLock};
#[cfg(windows)]
use tokio::sync::mpsc::UnboundedReceiver;

/// A console-aware stdin reader that implements [`AsyncRead`].
///
/// On Windows with a real console handle, reads from the process-wide
/// [`CONSOLE_READER`] thread (spawned at most once — see the module docs)
/// that reads via `ReadConsoleW` and feeds bytes into a shared channel.
/// Otherwise delegates to `tokio::io::stdin()`.
pub(crate) struct ConsoleStdin {
    #[cfg(windows)]
    inner: Inner,
    #[cfg(not(windows))]
    inner: tokio::io::Stdin,
}

#[cfg(windows)]
enum Inner {
    /// Reads through the shared [`CONSOLE_READER`] channel rather than
    /// owning one — each instance keeps only its own leftover-bytes buffer.
    Console { buf: Vec<u8>, pos: usize },
    Pipe(tokio::io::Stdin),
}

#[cfg(windows)]
static CONSOLE_READER: OnceLock<Mutex<UnboundedReceiver<Vec<u8>>>> = OnceLock::new();

impl ConsoleStdin {
    /// Opens stdin, enabling `ENABLE_VIRTUAL_TERMINAL_INPUT` if it's a
    /// Windows console handle. Safe to call more than once per process (see
    /// module docs) — later calls reuse the first call's console-reader
    /// thread instead of spawning a new one.
    pub(crate) fn open() -> Self {
        #[cfg(windows)]
        {
            if ensure_console_reader() {
                return ConsoleStdin { inner: Inner::Console { buf: Vec::new(), pos: 0 } };
            }
            ConsoleStdin { inner: Inner::Pipe(tokio::io::stdin()) }
        }
        #[cfg(not(windows))]
        {
            ConsoleStdin { inner: tokio::io::stdin() }
        }
    }
}

/// Ensures [`CONSOLE_READER`] is initialized, spawning the `ReadConsoleW`
/// thread on the first call only. Returns whether stdin is a real console
/// (`true`) or should fall back to [`Inner::Pipe`] (`false`) — cached
/// implicitly by `CONSOLE_READER`'s presence, so repeated calls (including
/// the very first one, on `not(windows)` build configs where this function
/// doesn't exist at all) are cheap.
#[cfg(windows)]
fn ensure_console_reader() -> bool {
    if CONSOLE_READER.get().is_some() {
        return true;
    }
    let Some(rx) = try_open_console() else {
        return false;
    };
    // If another call already won this race (shouldn't happen in practice:
    // callers open a new attempt only after the previous one's ConsoleStdin
    // was fully dropped, so this is never actually concurrent — but
    // OnceLock::set losing a race is not a bug, just a discarded `rx` whose
    // now-orphaned thread will exit on its next failed send, same as
    // today's non-singleton behavior for that one thread only), keep
    // whichever receiver won.
    let _ = CONSOLE_READER.set(Mutex::new(rx));
    true
}

#[cfg(windows)]
fn try_open_console() -> Option<tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>> {
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
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        #[cfg(windows)]
        {
            match &mut self.inner {
                Inner::Console { buf: inner_buf, pos } => {
                    // Drain buffered data first.
                    if *pos < inner_buf.len() {
                        let remaining = inner_buf.len() - *pos;
                        let to_write = remaining.min(buf.remaining());
                        buf.put_slice(&inner_buf[*pos..*pos + to_write]);
                        *pos += to_write;
                        if *pos >= inner_buf.len() {
                            inner_buf.clear();
                            *pos = 0;
                        }
                        return Poll::Ready(Ok(()));
                    }

                    // Try to get more data from the shared background
                    // thread's channel. The lock is only ever held for the
                    // duration of this synchronous `poll_recv` call, never
                    // across an await point, so contention is not a concern
                    // even though callers are expected to be sequential.
                    let rx_lock = CONSOLE_READER.get().expect("Inner::Console is only constructed after ensure_console_reader() succeeded");
                    let mut rx = rx_lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                    match rx.poll_recv(cx) {
                        Poll::Ready(Some(data)) => {
                            let to_write = data.len().min(buf.remaining());
                            buf.put_slice(&data[..to_write]);
                            if to_write < data.len() {
                                *inner_buf = data;
                                *pos = to_write;
                            }
                            Poll::Ready(Ok(()))
                        }
                        Poll::Ready(None) => Poll::Ready(Ok(())), // thread ended = EOF
                        Poll::Pending => Poll::Pending,
                    }
                }
                Inner::Pipe(stdin) => Pin::new(stdin).poll_read(cx, buf),
            }
        }
        #[cfg(not(windows))]
        {
            Pin::new(&mut self.inner).poll_read(cx, buf)
        }
    }
}