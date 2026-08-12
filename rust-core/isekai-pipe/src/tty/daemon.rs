//! `isekai-pipe tty daemon <name>`: the long-lived process that owns the pty
//! + shell and relays it to whichever client is currently attached. See
//! [`super`]'s module docs for the overall design.

use std::io;
use std::os::unix::process::CommandExt as _;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use tokio::net::UnixStream;

use super::attach_slot::{AttachSlot, RelayMsg};
use super::protocol::{read_frame, write_frame, Frame};
use super::pty::PtyMaster;
use super::unix_socket::verify_peer_is_self;

/// Fallback terminal size for the pty, applied at `openpty` time before any
/// client has attached to say what it actually wants — corrected via
/// `PtyMaster::resize` the moment the first real `Frame::Hello` arrives.
/// Matches `tmux new-session -d`'s own "reasonable default until someone
/// attaches" behavior.
const DEFAULT_COLS: u16 = 80;
const DEFAULT_ROWS: u16 = 24;

/// Bound on the replay-on-attach ring buffer — generous for "recent
/// terminal scrollback," not sized to hold a session's entire output.
const RING_BUFFER_CAPACITY: usize = 256 * 1024;

/// How long the daemon waits for a newly-accepted connection's `Frame::Hello`
/// before giving up on it. A connection that passed `SO_PEERCRED` but then
/// never speaks is either a bug or hostile; either way it must not tie up
/// this daemon's accept loop or the pty relay forever.
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);

/// How long [`run`] waits, after [`super::attach_slot::AttachSlot::notify_exit`]
/// queues `RelayMsg::Exit` for the current occupant, before tearing the rest
/// of this process down — gives that occupant's writer task (`handle_client`)
/// a real chance to get scheduled and flush `Frame::Exit` to the socket
/// before this process's own shutdown can race it. See the call site's doc
/// comment for the real bug this fixes.
const EXIT_NOTIFY_GRACE: Duration = Duration::from_millis(300);

/// Spawns `isekai-pipe tty daemon <name>` as a **fully detached** process —
/// not merely "backgrounded" — so it survives the SSH session `isekai-pipe
/// tty attach` (this function's caller) is itself running inside of.
///
/// This is the fix for the single most important failure mode of this whole
/// feature (found in design review before any code was written): a daemon
/// spawned as an ordinary child of `tty attach` inherits `tty attach`'s
/// session, process group, and controlling terminal — which is the SSH
/// channel's pty. When the SSH link drops, `sshd` tears down that session
/// and sends `SIGHUP` to its whole process group, killing the daemon (and
/// the shell it was created to keep alive) at the exact moment it was
/// supposed to start proving its value. A daemon that looks like it works
/// in a quick manual test (attach, detach, reattach without ever actually
/// losing the SSH link) can still fail this specific way — this must be
/// exercised against a *real* dropped connection, not just a clean detach
/// (see the standalone e2e test, task #19, before this is ever wired into
/// `isekai-ssh` itself).
///
/// The fix, applied in `pre_exec` (i.e. *before* the daemon's own `main`
/// ever starts running, not as a "first thing the daemon does itself" race):
/// `setsid()` (become a new session leader with no controlling terminal —
/// the prerequisite for `SIGHUP` immunity, since `SIGHUP` on terminal
/// disconnect is delivered to the *controlling terminal's* session) and
/// `SIGHUP` set to `SIG_IGN` (defensive — once `setsid()` succeeds there is
/// no controlling terminal left to deliver it from at all, but this costs
/// nothing and guards against any path that hasn't been thought through).
/// `stdin`/`stdout`/`stderr` are redirected to `/dev/null` via the ordinary
/// `Command` builder (applied by the standard library before `pre_exec`
/// runs, so no manual `dup2` is needed here) — holding the SSH channel's
/// own stdio fds open in the daemon would both write into a session that's
/// going away and, more importantly, can make the local `ssh(1)`/`isekai-ssh`
/// process hang waiting for the channel to fully close on exit, since
/// `sshd` commonly waits for every fd referencing a channel to close before
/// reporting it closed.
///
/// The returned `Child` handle is deliberately not `.wait()`ed on by the
/// caller (nor should it ever be) — once spawned, the daemon is meant to
/// outlive `tty attach` entirely; on `tty attach`'s own exit the daemon
/// simply gets reparented to the OS's init/subreaper, standard Unix orphan
/// handling, no explicit "detach" step needed beyond not waiting.
pub(crate) fn spawn_detached(name: &str) -> io::Result<()> {
    let exe = std::env::current_exe()?;
    let mut cmd = Command::new(exe);
    cmd.args(["tty", "daemon", name]);
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::null());
    cmd.stderr(Stdio::null());

    // SAFETY: this closure runs in the forked child before exec, so it must
    // be async-signal-safe. `setsid`/`signal` are both plain syscalls with
    // no allocation or locking, satisfying `pre_exec`'s safety contract —
    // same reasoning as `pty.rs::spawn`'s identical-shape `pre_exec` use.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: `SIG_IGN` is a valid disposition constant; installing
            // it for `SIGHUP` has no precondition beyond a live process,
            // which we are.
            if libc::signal(libc::SIGHUP, libc::SIG_IGN) == libc::SIG_ERR {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let child = cmd.spawn()?;
    // Deliberately not `.wait()`ed — see this function's doc comment. `Child`'s
    // `Drop` (the `std`, not `tokio`, version used here) does not kill the
    // process, so dropping this handle is exactly "let it run independently."
    drop(child);
    Ok(())
}

/// The daemon's whole life, from "do I even get to exist" to "the shell
/// exited, clean up and stop." Returns the shell's own exit code.
///
/// Ordering matters here in two ways this crate's own docs elsewhere don't
/// need to call out explicitly for most code, so it's worth being explicit:
/// [`super::unix_socket::bind_private_socket`] narrows the process `umask`
/// only for the duration of that one call, but this function must still be
/// the first thing this process does with the filesystem — see that
/// module's doc comment on why a concurrently-running task on another
/// thread could otherwise observe the narrowed umask too.
pub(crate) async fn run(name: &str, command: Vec<String>) -> anyhow::Result<u8> {
    let dir = super::unix_socket::private_runtime_dir()?;

    let _lock = match super::daemon_lock::DaemonLock::try_acquire(&dir, name)? {
        Some(lock) => lock,
        None => {
            log::info!("isekai-pipe tty daemon {name}: another daemon already holds this name, exiting");
            return Ok(0);
        }
    };

    let socket_path = super::unix_socket::socket_path(&dir, name)?;
    // Holding `_lock` proves no live daemon owns `name` right now, so any
    // socket file already at this path is unreachable/stale — safe to
    // remove before binding a fresh one.
    match std::fs::remove_file(&socket_path) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    let listener = super::unix_socket::bind_private_socket(&socket_path)?;

    // Real bug found via pre-mortem review (2026-08-12): a plain
    // `$ISEKAI_CTL_SOCK` env var, exported once by the login shell that
    // spawned this daemon (`ctl_forward.rs::build_login_shell_command`),
    // gets baked into this daemon's own env and, from there, into the pty
    // shell's env forever — but every *reconnect* to this same session
    // creates a brand-new `-R` ctl-socket forward with a fresh random path
    // (`ctl_forward.rs::new_ctl_token`), and `attach.rs::run` only calls
    // `spawn_detached` (which is what would have picked up that fresh
    // value) on the very first connection to a name; every later reconnect
    // just dials the already-running daemon directly. So the persistent
    // shell's `$ISEKAI_CTL_SOCK` silently goes stale — pointing at a `-R`
    // forward whose owning `ssh(1)`/`isekai-ssh` process already exited —
    // the moment the *first* connection to a session ends, breaking every
    // `isekai-pipe ctl` (title/clipboard/notify) invocation from inside
    // that shell for the rest of the daemon's life, with no error visible
    // anywhere near the failure. `ctl_sock_file` is a stable indirection,
    // fixed for this daemon's whole lifetime, that `tty attach` (`attach.rs`)
    // rewrites with its own `$ISEKAI_CTL_SOCK` on *every* invocation
    // (including reconnects, where no new shell is spawned) — the shell
    // this daemon owns reads through it dynamically (via `isekai-pipe ctl`,
    // `ctl.rs::resolve_ctl_socket_path_with`) instead of relying on a
    // `$ISEKAI_CTL_SOCK` baked into its own environment once at spawn time.
    let ctl_sock_file = super::unix_socket::ctl_sock_file_path(&dir, name);

    let term = std::env::var("TERM").unwrap_or_else(|_| "xterm-256color".to_string());
    let (master, mut child) = super::pty::spawn(&command, &term, DEFAULT_COLS, DEFAULT_ROWS, Some(&ctl_sock_file))?;
    let master = Arc::new(master);
    let attach_slot = Arc::new(AttachSlot::new(RING_BUFFER_CAPACITY));

    // pty -> ring buffer + current occupant, unconditionally — this loop
    // must run even with zero clients attached (see `AttachSlot::broadcast`'s
    // doc comment on the classic dtach freeze this avoids).
    let read_loop = {
        let master = master.clone();
        let attach_slot = attach_slot.clone();
        let name = name.to_string();
        tokio::spawn(async move {
            let mut buf = [0u8; 8192];
            loop {
                match master.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(n) => attach_slot.broadcast(&buf[..n]),
                    Err(e) => {
                        log::debug!("isekai-pipe tty daemon {name}: pty read ended: {e:#}");
                        break;
                    }
                }
            }
        })
    };

    let accept_loop = {
        let master = master.clone();
        let attach_slot = attach_slot.clone();
        let name = name.to_string();
        tokio::spawn(async move {
            loop {
                let stream = match listener.accept().await {
                    Ok((stream, _addr)) => stream,
                    Err(e) => {
                        log::debug!("isekai-pipe tty daemon {name}: accept failed: {e:#}");
                        continue;
                    }
                };
                let master = master.clone();
                let attach_slot = attach_slot.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_client(stream, &master, &attach_slot).await {
                        log::debug!("isekai-pipe tty daemon: client connection ended: {e:#}");
                    }
                });
            }
        })
    };

    // `std::process::Child::wait` blocks the calling thread until the child
    // exits — run it on the blocking-task pool rather than an async worker
    // thread. This is also this function's only real "wait for the shell to
    // end" signal; per this feature's "daemon lifetime = shell lifetime"
    // design, everything else shuts down once this resolves.
    let exit_status = tokio::task::spawn_blocking(move || child.wait()).await??;
    let exit_code = exit_status.code().unwrap_or(1) as u8;

    attach_slot.notify_exit(exit_code);
    // `notify_exit` only *queues* `RelayMsg::Exit` on the current occupant's
    // channel (`try_send`, non-blocking) — the attached client's own
    // per-connection writer task (spawned in `handle_client`, not tracked
    // here) still needs to actually get scheduled to pick that message up
    // and write `Frame::Exit` to the socket. Real race found live
    // (2026-08-12): returning immediately after `notify_exit` let this
    // process's own shutdown (and the client-visible fd/socket teardown
    // that comes with it) win that race often enough in practice that the
    // attached `isekai-pipe tty attach` frequently saw the connection drop
    // before ever receiving `Frame::Exit` — reported as "connection to the
    // tty daemon closed unexpectedly" instead of a clean exit, even though
    // the underlying hang this was found alongside (the attach process
    // itself failing to terminate — see `attach.rs`) was already fixed.
    // A short bounded wait is enough: this is a single small write over a
    // local Unix domain socket, not anything that could legitimately take
    // long.
    tokio::time::sleep(EXIT_NOTIFY_GRACE).await;
    read_loop.abort();
    accept_loop.abort();
    let _ = std::fs::remove_file(&socket_path);
    let _ = std::fs::remove_file(&ctl_sock_file);

    Ok(exit_code)
}

/// Serves exactly one client connection: reads its `Frame::Hello`, resizes
/// the pty to match, installs it as the current occupant (preempting
/// whatever was attached before), replays recent output, then relays
/// bidirectionally until the client disconnects, is itself preempted by a
/// later `attach`, or the pty signals exit (handled by [`run`] directly,
/// not here).
async fn handle_client(stream: UnixStream, master: &Arc<PtyMaster>, attach_slot: &Arc<AttachSlot>) -> anyhow::Result<()> {
    verify_peer_is_self(&stream)?;
    let (mut read_half, mut write_half) = stream.into_split();

    let hello = tokio::time::timeout(HELLO_TIMEOUT, read_frame(&mut read_half))
        .await
        .map_err(|_| anyhow::anyhow!("no Hello within {HELLO_TIMEOUT:?}"))??
        .ok_or_else(|| anyhow::anyhow!("client closed before sending Hello"))?;
    let Frame::Hello { cols, rows, .. } = hello else {
        anyhow::bail!("expected Hello as the first frame, got {hello:?}");
    };
    master.resize(cols, rows)?;

    let (tx, mut rx) = AttachSlot::new_occupant_channel();
    let (generation, replay) = attach_slot.install(tx);

    write_frame(&mut write_half, &Frame::HelloAck).await?;
    if !replay.is_empty() {
        write_frame(&mut write_half, &Frame::Stdout(replay)).await?;
    }

    // Writer half: relays pty output (and, eventually, an `Exit`) to this
    // client until preempted (the channel closes without an explicit
    // message — `AttachSlot::install` dropped our sender) or a write fails
    // (the client itself disconnected).
    let writer_task = tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Some(RelayMsg::Data(data)) => {
                    if write_frame(&mut write_half, &Frame::Stdout(data)).await.is_err() {
                        return;
                    }
                }
                Some(RelayMsg::Exit(code)) => {
                    let _ = write_frame(&mut write_half, &Frame::Exit(code)).await;
                    return;
                }
                None => {
                    // Preempted: `AttachSlot::install` dropped our sender.
                    let _ = write_frame(&mut write_half, &Frame::Preempted).await;
                    return;
                }
            }
        }
    });

    // Reader half: forwards this client's own Stdin/Resize into the pty,
    // for as long as (checked immediately before every pty write, under
    // `AttachSlot`'s lock — see `is_current`'s doc comment) this client is
    // still the current occupant.
    loop {
        let frame = match read_frame(&mut read_half).await {
            Ok(Some(frame)) => frame,
            Ok(None) | Err(_) => break,
        };
        if !attach_slot.is_current(generation) {
            break;
        }
        match frame {
            Frame::Stdin(data) => {
                if master.write_all(&data).await.is_err() {
                    break;
                }
            }
            Frame::Resize { cols, rows } => {
                let _ = master.resize(cols, rows);
            }
            other => {
                log::debug!("isekai-pipe tty daemon: unexpected frame from client: {other:?}");
            }
        }
    }

    attach_slot.vacate(generation);
    writer_task.abort();
    Ok(())
}
