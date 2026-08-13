//! `isekai-pipe tty attach <name>`: the thin client `isekai-ssh` execs as
//! the remote command in place of a plain login shell. Connects to (or, on
//! first use, spawns then connects to) `isekai-pipe tty daemon <name>` and
//! pumps this process's own stdio through it.
//!
//! **This process puts its own pty (fd 0) into raw mode** — a real bug found
//! 2026-08-12 via live reproduction: this module used to assume "no raw-mode
//! work needed here, the *local* `isekai-ssh`/`ssh(1)` on the other end of
//! the SSH connection already puts the user's real terminal into raw mode."
//! That's true of the *local* (client-side) terminal, but irrelevant to
//! *this* pty — the one `sshd` allocated on *this* host for this process,
//! which is a completely separate tty. For a normal login shell, bash's own
//! `readline` reconfigures that remote pty into raw/cbreak mode itself once
//! it starts (which is why `isekai-ssh`'s own initial `PTY_REQ` modes are
//! deliberately cooked-mode defaults — see
//! `isekai-ssh/src/native/console.rs::build_terminal_modes`'s doc comment).
//! But `isekai-ssh` execs *this process* in place of a shell for a
//! `--isekai-tty` session, and nothing here ever reconfigured the pty — so
//! it stayed in the kernel's default cooked/canonical mode for the entire
//! session: normal typing looked fine (the kernel's own `ECHO` renders
//! printable characters same as `readline` would), but control characters
//! were echoed by the kernel's `ECHOCTL` as literal `^X` text instead of
//! being forwarded as raw bytes, and Ctrl-D (`VEOF`) only flushed whatever
//! the kernel's line buffer already held instead of behaving like the raw
//! EOF a shell reading in raw mode would see — reported as "Ctrl-D doesn't
//! exit" and "Ctrl-A/Ctrl-K show up as `^A`/`^K`".

use std::io;
use std::os::unix::process::CommandExt as _;
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

/// `--isekai-tty` is an opt-in convenience feature layered on top of an
/// otherwise-working SSH session (`ctl_forward.rs`'s `exec_target`
/// mechanism *replaces* the login shell entirely via `exec`) — a failure
/// here must never be the reason a user gets no shell at all. Real gap
/// found via pre-mortem review (2026-08-12): every failure path before a
/// live session was ever established (daemon spawn failure, socket
/// errors, no `HelloAck`, ...) used to just propagate an `Err` straight
/// out to `tty_attach_command`, which printed it and exited — since the
/// remote command was `exec isekai-pipe tty attach '<name>'`, that exit
/// *was* the whole SSH session ending, with no shell ever having run.
///
/// Fixed by splitting the connect+handshake phase out into
/// [`run_until_attached`] and falling back to a plain login shell (via
/// [`fall_back_to_plain_shell`]) on any error from *that* phase. Once a
/// live session genuinely exists (past `HelloAck`), [`relay`] takes over
/// and this fallback deliberately does not apply — a *mid-session*
/// connection drop or a clean `exit` from inside the real attached shell
/// must not silently swap the user into a brand-new, unrelated plain
/// shell (their real persistent session may well still be alive
/// server-side and worth reconnecting to, which conjuring up a fallback
/// shell here would obscure).
pub(crate) async fn run(name: &str) -> anyhow::Result<u8> {
    // Best-effort: a non-tty fd 0 (piped/redirected, e.g. under test) just
    // means `enable()` returns `None` and the relay runs without it — same
    // opportunistic-fallback convention as `isekai-ssh`'s own
    // `RawModeGuard`, not a reason to fail the whole session.
    let raw_mode = RawModeGuard::enable();

    match run_until_attached(name).await {
        Ok(stream) => relay(stream).await,
        Err(e) => {
            eprintln!("isekai-pipe tty attach: {e:#}");
            eprintln!("isekai-pipe tty attach: falling back to a plain login shell");
            // Restore the pty to its original (cooked) mode before handing
            // off — matches what an ordinary login shell would see; its
            // own readline reconfigures the pty itself regardless (see
            // this module's own doc comment), but there is no reason to
            // leave it in a state the fallback shell never asked for.
            drop(raw_mode);
            let exec_err = fall_back_to_plain_shell();
            Err(anyhow::anyhow!(exec_err).context("tty-attach itself failed, and falling back to a plain login shell also failed"))
        }
    }
}

/// Connects to (spawning if needed) the tty daemon and completes the
/// `Hello`/`HelloAck` handshake — everything [`run`] needs before handing
/// off to [`relay`]. Split out from `run` specifically so its errors can be
/// distinguished from `relay`'s: see `run`'s own doc comment for why only
/// *this* phase's failures fall back to a plain shell.
async fn run_until_attached(name: &str) -> anyhow::Result<UnixStream> {
    let dir = super::unix_socket::private_runtime_dir()?;
    let socket_path = super::unix_socket::socket_path(&dir, name)?;
    refresh_ctl_sock_file(&dir, name);

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

    Ok(stream)
}

/// Replaces this process with a plain interactive login shell — exactly
/// the command `isekai-ssh/src/tty_attach.rs`'s own `ctl_forward.rs`
/// composition would have run if `--isekai-tty`/`#@isekai tty` had never
/// been requested at all (`"${SHELL:-/bin/sh}" -i -l`,
/// `ctl_forward.rs::build_login_shell_command`'s own default). On success
/// this never returns — `Command::exec` replaces the process image in
/// place, same pid, same controlling terminal, no intervening shell layer.
/// The `io::Error` it returns is only ever observed if the exec itself
/// failed (e.g. neither `$SHELL` nor `/bin/sh` could be executed), a
/// condition severe enough that there is genuinely nothing left to fall
/// back to.
fn fall_back_to_plain_shell() -> io::Error {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
    std::process::Command::new(&shell).args(["-i", "-l"]).exec()
}

/// Keeps `daemon.rs`'s `$ISEKAI_TTY_CTL_SOCK_FILE`-pointed indirection file
/// in sync with *this* invocation's own `$ISEKAI_CTL_SOCK` — see
/// `daemon.rs::run`'s doc comment for the staleness bug this exists to fix.
/// Called unconditionally on every `tty attach` invocation, not just the
/// one that ends up spawning a fresh daemon: a reconnect never re-execs the
/// pty shell (so it can never see a new env var directly), but it *does*
/// always run this function first, which is what lets `isekai-pipe ctl`
/// (run later, from inside that same long-lived shell) observe the fresh
/// value instead of whatever was current the first time this session's
/// daemon happened to be spawned.
///
/// Best-effort: a write failure here (e.g. a momentarily-unwritable
/// directory) must not fail the whole attach — this indirection is a
/// convenience for `isekai-pipe ctl`'s title/clipboard/notify commands, not
/// something the core "reconnect to my persistent shell" feature depends
/// on. When `$ISEKAI_CTL_SOCK` isn't set for this invocation (ctl-socket
/// forwarding disabled, or `--isekai-tty` used without it), any existing
/// file is removed rather than left behind — a stale file here would look
/// exactly as "fresh" as a real one to `ctl.rs`'s reader, silently pointing
/// `isekai-pipe ctl` at a forward that was never set up for *this*
/// reconnect.
fn refresh_ctl_sock_file(dir: &std::path::Path, name: &str) {
    let path = super::unix_socket::ctl_sock_file_path(dir, name);
    match std::env::var("ISEKAI_CTL_SOCK") {
        Ok(value) if !value.is_empty() => {
            let _ = std::fs::write(&path, value);
        }
        _ => {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// RAII guard: puts this process's own fd 0 (the pty `sshd` allocated for
/// this SSH session) into raw mode on construction, restores the original
/// termios on drop. Mirrors `isekai-ssh/src/native/console.rs::RawModeGuard`
/// (same rationale, opposite side of the connection) but hand-rolled with
/// `libc::cfmakeraw` directly rather than pulling in `crossterm` — this
/// crate has no other terminal-handling needs, and `cfmakeraw` is the
/// single POSIX-standard call for exactly this.
struct RawModeGuard {
    original: libc::termios,
}

impl RawModeGuard {
    /// Returns `None` (rather than an `Err`) when fd 0 isn't actually a tty
    /// or `tcgetattr`/`tcsetattr` otherwise fails — piped/redirected stdin
    /// (tests, a non-interactive invocation) shouldn't fail the session over
    /// a cosmetic degrade, same as `terminal_size`'s own fallback just below.
    fn enable() -> Option<Self> {
        // SAFETY: `termios` is a plain repr(C) struct with no invariants
        // beyond being zero-initializable (`tcgetattr` fully populates it on
        // success); fd 0 is always a valid, open descriptor for the
        // lifetime of this process.
        let mut original: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(0, &mut original) } != 0 {
            return None;
        }
        let mut raw = original;
        // SAFETY: `raw` is a valid, fully-initialized `termios` (copied from
        // the successful `tcgetattr` above); `cfmakeraw` only mutates the
        // flag/`c_cc` fields in place.
        unsafe { libc::cfmakeraw(&mut raw) };
        // SAFETY: `raw` is a valid, fully-initialized `termios`; fd 0 is the
        // same valid descriptor `tcgetattr` just succeeded on.
        if unsafe { libc::tcsetattr(0, libc::TCSANOW, &raw) } != 0 {
            return None;
        }
        Some(Self { original })
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        // Best-effort: nothing sensible to do if this fails on the way out
        // (e.g. the pty was already torn down), and this process is about
        // to exit regardless.
        unsafe { libc::tcsetattr(0, libc::TCSANOW, &self.original) };
    }
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

    // Deliberately fire-and-forget, not bound to a `JoinHandle` this
    // function later `.abort()`s — see the doc comment down by
    // `std::process::exit` on why an abort here would be a no-op anyway
    // (this task's real work is a blocking `read(2)` on a background
    // thread, which `abort` cannot reach anyway) and why this function
    // force-exits the whole process instead of waiting for it.
    {
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

    if let Some(task) = resize_task {
        task.abort();
    }
    writer_task.abort();

    // `stdin_task.abort()` (removed above — see why) cannot actually
    // interrupt this: `tokio::io::Stdin` on Unix reads via its own
    // internal blocking-thread pool (there is no portable way to poll
    // stdin's readiness directly), and `JoinHandle::abort` only takes
    // effect at an `.await` point inside the *async* task — it does not,
    // and cannot, kill the underlying OS thread blocked in the real
    // `read(2)` syscall. That thread stays blocked forever once the user
    // stops typing (nothing more will ever arrive on this session's pty
    // once the daemon/shell has already exited), and it still holds this
    // process's fd 0 open.
    //
    // Real bug this caused (found live, 2026-08-12): letting this
    // function return normally so `main`'s `#[tokio::main]`-equivalent
    // multi-thread `Runtime` drops and performs its default *graceful*
    // shutdown — which waits for exactly this kind of outstanding
    // blocking work to finish — deadlocked the whole process forever
    // after a clean `exit`/logout: the shell had already exited, the
    // daemon had already relayed `Frame::Exit` here, but this process
    // itself never actually terminated, so `sshd` never saw every fd
    // referencing the SSH channel close and never reported the channel
    // closed — leaving the *client* (`isekai-ssh`) hanging too, congruent
    // with (but distinct from — this is this process failing to exit at
    // all, not a channel-close-detection gap) the mux owner-side hang
    // fixed in PR #85.
    //
    // `std::process::exit` skips the stuck thread (and every other
    // destructor, including `RawModeGuard`'s termios restore above — moot
    // anyway, since this pty is being torn down by `sshd` regardless) and
    // ends the process immediately, which is exactly what's needed here.
    let code = match outcome {
        Ok(code) => code,
        Err(e) => {
            eprintln!("isekai-pipe tty attach: {e:#}");
            1
        }
    };
    std::process::exit(code as i32);
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
