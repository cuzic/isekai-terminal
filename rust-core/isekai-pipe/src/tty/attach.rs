//! `isekai-pipe tty attach <name>`: the thin client `isekai-ssh` execs as
//! the remote command in place of a plain login shell. Connects to (or, on
//! first use, spawns then connects to) `isekai-pipe tty daemon <name>` and
//! pumps this process's own stdio through it.
//!
//! **No raw-mode/terminal-setup work happens here**, unlike a typical local
//! terminal client: this process's own stdin/stdout *are* the pty `sshd`
//! already allocated for this interactive SSH session (that's what `-t`/PTY
//! allocation on the SSH client side already arranged) — the local `ssh(1)`/
//! `russh` on the *other* end of the SSH connection is what puts the user's
//! real local terminal into raw mode. This process just needs to relay its
//! already-correctly-configured stdio through to the daemon.

use std::io;
use std::time::Duration;

use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::UnixStream;

use super::protocol::{read_frame, write_frame, Frame};
use super::unix_socket::verify_peer_is_self;

/// Bounded retry for "the socket isn't there (or isn't answering) yet,
/// spawn a daemon and wait for it to bind" — the same shape as
/// `claude-hookd`'s `SPAWN_RETRY_DELAYS_MS` for an identical
/// spawn-then-poll race, generous enough for a slow process start under
/// load while still bounding how long a user waits before either a shell
/// or a clear error.
const CONNECT_RETRY_DELAYS: [Duration; 6] =
    [Duration::from_millis(50), Duration::from_millis(100), Duration::from_millis(200), Duration::from_millis(400), Duration::from_millis(800), Duration::from_millis(1600)];

/// Mirrors `daemon.rs::HELLO_TIMEOUT` for the symmetric wait: this process
/// must not hang forever if the daemon accepted the connection but never
/// answers (`always-connects.md`'s "never hang forever" principle, applied
/// to this feature specifically).
const HELLO_ACK_TIMEOUT: Duration = Duration::from_secs(10);

pub(crate) async fn run(name: &str) -> anyhow::Result<u8> {
    let dir = super::unix_socket::private_runtime_dir()?;
    let socket_path = dir.join(format!("{name}.sock"));

    let mut stream = match connect(&socket_path).await {
        Ok(stream) => stream,
        Err(_) => {
            super::daemon::spawn_detached(name)?;
            connect_with_retry(&socket_path).await?
        }
    };

    let (cols, rows) = terminal_size();
    let term = std::env::var("TERM").unwrap_or_else(|_| "xterm-256color".to_string());
    write_frame(&mut stream, &Frame::Hello { term, cols, rows }).await?;

    match tokio::time::timeout(HELLO_ACK_TIMEOUT, read_frame(&mut stream)).await {
        Ok(Ok(Some(Frame::HelloAck))) => {}
        Ok(Ok(Some(other))) => anyhow::bail!("expected HelloAck from the daemon, got {other:?}"),
        Ok(Ok(None)) => anyhow::bail!("daemon closed the connection before HelloAck"),
        Ok(Err(e)) => return Err(e.into()),
        Err(_) => anyhow::bail!("no HelloAck from the daemon within {HELLO_ACK_TIMEOUT:?}"),
    }

    relay(stream).await
}

/// A single, non-retrying connect attempt, with `SO_PEERCRED` verification —
/// the socket path can be squatted by whoever creates it first, so a
/// successful `connect(2)` alone proves nothing about who's actually
/// listening (see `unix_socket.rs`'s module docs).
async fn connect(socket_path: &std::path::Path) -> io::Result<UnixStream> {
    let stream = UnixStream::connect(socket_path).await?;
    verify_peer_is_self(&stream)?;
    Ok(stream)
}

async fn connect_with_retry(socket_path: &std::path::Path) -> anyhow::Result<UnixStream> {
    let mut last_err = None;
    for delay in CONNECT_RETRY_DELAYS {
        tokio::time::sleep(delay).await;
        match connect(socket_path).await {
            Ok(stream) => return Ok(stream),
            Err(e) => last_err = Some(e),
        }
    }
    Err(anyhow::anyhow!(
        "could not connect to the tty daemon after spawning it: {}",
        last_err.map(|e| e.to_string()).unwrap_or_else(|| "no attempt succeeded".to_string())
    ))
}

/// Relays this process's stdin/local resizes to the daemon and the daemon's
/// output back to this process's stdout, until `Frame::Exit`,
/// `Frame::Preempted`, or the connection drops.
///
/// Stdin-reading and resize-watching each just need to *produce* frames,
/// not own the connection's write half directly — they share one bounded
/// channel into a single writer task instead, so adding a second frame
/// source (resize) never needs to fight the first (stdin) over the one
/// write half `AsyncWrite` requires exclusive access to.
async fn relay(stream: UnixStream) -> anyhow::Result<u8> {
    let (mut read_half, mut write_half) = stream.into_split();
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Frame>(64);

    let stdin_task = {
        let tx = tx.clone();
        tokio::spawn(async move {
            let mut stdin = tokio::io::stdin();
            let mut buf = [0u8; 8192];
            loop {
                let n = match stdin.read(&mut buf).await {
                    Ok(0) | Err(_) => return,
                    Ok(n) => n,
                };
                if tx.send(Frame::Stdin(buf[..n].to_vec())).await.is_err() {
                    return;
                }
            }
        })
    };

    let resize_task = spawn_resize_watcher(tx);

    let writer_task = tokio::spawn(async move {
        while let Some(frame) = rx.recv().await {
            if write_frame(&mut write_half, &frame).await.is_err() {
                return;
            }
        }
    });

    let mut stdout = tokio::io::stdout();
    let outcome = loop {
        match read_frame(&mut read_half).await {
            Ok(Some(Frame::Stdout(data))) => {
                if stdout.write_all(&data).await.is_err() || stdout.flush().await.is_err() {
                    break Ok(1);
                }
            }
            Ok(Some(Frame::Exit(code))) => break Ok(code),
            Ok(Some(Frame::Preempted)) => {
                eprintln!("isekai-pipe tty attach: preempted by a newer attach to the same session");
                break Ok(1);
            }
            Ok(Some(other)) => {
                log::debug!("isekai-pipe tty attach: unexpected frame from daemon: {other:?}");
            }
            Ok(None) => break Err(anyhow::anyhow!("connection to the tty daemon closed unexpectedly")),
            Err(e) => break Err(e.into()),
        }
    };

    stdin_task.abort();
    if let Some(task) = resize_task {
        task.abort();
    }
    writer_task.abort();
    outcome
}

/// Queries this process's own pty (see the module doc comment on why that's
/// the right thing to query here) via `TIOCGWINSZ`. Falls back to `80x24` —
/// matching `daemon.rs`'s own pre-attach default — if this process's stdin
/// isn't actually a tty (e.g. under test, or a non-interactive invocation),
/// rather than failing the connection over a cosmetic detail.
fn terminal_size() -> (u16, u16) {
    // SAFETY: `winsize` is a plain repr(C) struct with no invariants beyond
    // being zero-initializable, which `Default`/zeroed satisfies; `ioctl`
    // only writes into it on success and this process's stdin (fd 0) is a
    // valid, open descriptor for the duration of this call.
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::ioctl(0, libc::TIOCGWINSZ, &mut ws) };
    if rc == 0 && ws.ws_col > 0 && ws.ws_row > 0 {
        (ws.ws_col, ws.ws_row)
    } else {
        (80, 24)
    }
}

/// Watches for `SIGWINCH` (this process's own pty being resized — delivered
/// because it's this session's foreground process group) and forwards each
/// new size as `Frame::Resize` over `tx`. Returns `None` (nothing to
/// watch/abort) if signal handler installation itself fails, treated as
/// non-fatal — a session that never sees a live resize still works, it
/// just keeps whatever size `Frame::Hello` first reported.
fn spawn_resize_watcher(tx: tokio::sync::mpsc::Sender<Frame>) -> Option<tokio::task::JoinHandle<()>> {
    let mut signal = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change()).ok()?;
    Some(tokio::spawn(async move {
        loop {
            if signal.recv().await.is_none() {
                return;
            }
            let (cols, rows) = terminal_size();
            if tx.send(Frame::Resize { cols, rows }).await.is_err() {
                return;
            }
        }
    }))
}
