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

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, ReadBuf};

/// A console-aware stdin reader that implements [`AsyncRead`].
///
/// On Windows with a real console handle, spawns a background thread that
/// reads via `ReadConsoleW` and feeds bytes into an internal buffer.
/// Otherwise delegates to `tokio::io::stdin()`.
pub(crate) struct ConsoleStdin {
    #[cfg(windows)]
    inner: Inner,
    #[cfg(not(windows))]
    inner: tokio::io::Stdin,
}

#[cfg(windows)]
enum Inner {
    Console {
        rx: tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
        buf: Vec<u8>,
        pos: usize,
    },
    Pipe(tokio::io::Stdin),
}

impl ConsoleStdin {
    /// Opens stdin, enabling `ENABLE_VIRTUAL_TERMINAL_INPUT` if it's a
    /// Windows console handle.
    pub(crate) fn open() -> Self {
        #[cfg(windows)]
        {
            if let Some(rx) = try_open_console() {
                return ConsoleStdin { inner: Inner::Console { rx, buf: Vec::new(), pos: 0 } };
            }
            ConsoleStdin { inner: Inner::Pipe(tokio::io::stdin()) }
        }
        #[cfg(not(windows))]
        {
            ConsoleStdin { inner: tokio::io::stdin() }
        }
    }
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
                Inner::Console { rx, buf: inner_buf, pos } => {
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

                    // Try to get more data from the background thread.
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