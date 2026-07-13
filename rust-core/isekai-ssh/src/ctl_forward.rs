//! Per-tab remote→local title/clipboard control-plane
//! (`ISEKAI_PIPE_DESIGN.md` §8 Epic M, `#@isekai ctl-socket yes`). Requests
//! an SSH remote forward (`-R remote-sock:local-endpoint`) for every
//! `isekai-ssh <destination>` invocation — this works whether or not the
//! underlying connection is fresh or shared via ControlMaster/
//! ControlPersist, because OpenSSH scopes `-R` forwards per client request
//! rather than per underlying connection (unlike `isekai-transport`, which
//! cannot distinguish tabs once a connection is shared, see the ADR above
//! Epic M in `ISEKAI_PIPE_DESIGN.md`).
//!
//! Deliberately scoped to interactive sessions only (no explicit remote
//! command trailing the destination): a one-shot remote command has no
//! interactive shell for the user to run `isekai-pipe ctl` from, so silently
//! skipping matches this project's opportunistic-fallback convention rather
//! than adding complexity for a case with no user-visible benefit.
//!
//! **v1 simplifications, deliberately not hidden**:
//! - The remote socket lives under `/tmp` rather than the design doc's
//!   originally-sketched `~/.cache/isekai-pipe/ctl/`, because `-R`'s remote
//!   path is handed to `sshd` verbatim (no shell tilde-expansion), and
//!   resolving the remote `$HOME` first would need an extra network round
//!   trip this crate otherwise carefully avoids (`ISEKAI_PIPE_DESIGN.md`
//!   Epic L). Collision/squatting risk is mitigated by a 128-bit random
//!   token in the filename (matching `isekai_pipe_core`'s `new_intent_id`
//!   convention) rather than by directory placement; `bind()` on an
//!   existing path fails outright, so an attacker would have to guess the
//!   token in advance, not just win a race.
//! - `$ISEKAI_CTL_SOCK` is delivered by replacing the (absent) remote
//!   command with `export ISEKAI_CTL_SOCK=...; exec "$SHELL" -i -l` rather
//!   than `-o SetEnv=...`, because `SetEnv` requires a matching
//!   `AcceptEnv`/`SetEnv` entry in the remote `sshd_config` that most users
//!   do not control. This changes how the remote shell is invoked
//!   (explicit exec rather than sshd's own implicit login-shell exec), a
//!   deliberate, documented trade-off of an opt-in convenience feature.
//! - **The local endpoint is platform-specific: a UNIX domain socket on
//!   unix, a loopback TCP port on everything else.** `tokio::net::
//!   UnixListener` has no Windows backend, and Win32-OpenSSH's `-R`/`-L` do
//!   not support forwarding to a Windows named pipe or a native Windows
//!   `AF_UNIX` socket either (confirmed dead ends, not just an unexplored
//!   option — see `PowerShell/openssh-portable#433`, closed unmerged, and
//!   `PowerShell/Win32-OpenSSH#2321`, filed 2025-01 against OpenSSH 9.8p1,
//!   still unresolved), so a loopback TCP port is the only thing `-R` can
//!   target on a Windows client. Deliberately **not** unified onto TCP
//!   everywhere: the UNIX socket path (Linux/macOS clients — the vast
//!   majority of usage today) is unchanged, still relying on its `0700`
//!   directory for access control, exactly as before this was ever a
//!   concern for Windows.
//! - **Every platform's connection now starts with a plaintext secret
//!   preamble line** (the tab's `remote_path`, which [`isekai-pipe
//!   ctl`][isekai-pipe] already has in hand — it's embedded in
//!   `$ISEKAI_CTL_SOCK`'s filename, the same value it resolves the remote
//!   UNIX socket path from), checked by [`handle_ctl_connection`] before
//!   anything else. Not because the UNIX socket path needs it (its `0700`
//!   directory already provides equivalent protection) — but because
//!   `isekai-pipe ctl` has no way to know whether the client on the other
//!   end of the tunnel is unix or not, so it always sends the same
//!   preamble, and both platforms' listeners check it uniformly rather than
//!   the wire protocol silently differing by platform. On the loopback TCP
//!   path this preamble is the *only* access control (a bare port has no
//!   filesystem-permission equivalent to a UNIX socket's `0700` directory)
//!   — plaintext-over-loopback, not plaintext-over-network, since the real
//!   SSH hop (remote UNIX socket → sshd → this local endpoint) is already
//!   inside the SSH-encrypted tunnel; the preamble only needs to keep
//!   *other local processes on this machine* from connecting to the port
//!   directly and injecting messages, which 128 bits of randomness does
//!   adequately.
//!
//!   [isekai-pipe]: ../../../isekai-pipe/src/ctl.rs

use std::path::Path;
#[cfg(unix)]
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use isekai_protocol::CtlMessage;
#[cfg(test)]
use isekai_protocol::ClipboardMime;

pub(crate) struct CtlForward {
    pub(crate) remote_path: String,
    #[cfg(unix)]
    pub(crate) local_path: PathBuf,
    #[cfg(not(unix))]
    pub(crate) local_port: u16,
    /// Only populated on non-unix, where the port must be bound up front
    /// (before the `-R` argument naming it is even constructed) rather than
    /// lazily inside [`spawn_ctl_listener`] the way the unix path binds its
    /// socket — [`spawn_ctl_listener`] takes ownership of it via
    /// [`Option::take`]. Always `None` on unix.
    #[cfg(not(unix))]
    listener: Option<std::net::TcpListener>,
}

/// 128 bits of randomness as lowercase hex, matching
/// `isekai_pipe_core`'s `new_intent_id` convention (same entropy, same
/// encoding) so a squatting attacker cannot feasibly pre-guess the path.
fn new_ctl_token() -> String {
    isekai_pipe_core::new_hex_token_128()
}

/// Pure decision of whether to attempt a ctl-socket forward for this
/// invocation, given the resolved directive and the parsed ssh args. Split
/// out from `prepare_ctl_forward` so it's testable without touching the
/// filesystem or spawning anything.
pub(crate) fn should_attempt_ctl_forward(
    ctl_socket_enabled: bool,
    ssh_args_len: usize,
    destination_index: usize,
) -> bool {
    ctl_socket_enabled && ssh_args_len == destination_index + 1
}

/// The `-R` forward flag pair. Must be spliced in **before** the
/// destination in the final `ssh(1)` argv — anything after the destination
/// is the remote command, not an option, to `ssh(1)`.
#[cfg(unix)]
pub(crate) fn forward_option_args(forward: &CtlForward) -> [String; 2] {
    ["-R".to_string(), format!("{}:{}", forward.remote_path, forward.local_path.display())]
}

#[cfg(not(unix))]
pub(crate) fn forward_option_args(forward: &CtlForward) -> [String; 2] {
    ["-R".to_string(), format!("{}:127.0.0.1:{}", forward.remote_path, forward.local_port)]
}

/// The replacement remote command: since there is no pre-existing remote
/// command by construction (see `should_attempt_ctl_forward`), this becomes
/// the sole arg **after** the destination, exporting `$ISEKAI_CTL_SOCK` and
/// exec'ing an interactive login shell in its place (replicating what
/// `sshd` would have run implicitly had no remote command been given at
/// all).
pub(crate) fn remote_command_arg(forward: &CtlForward) -> String {
    format!(
        "export ISEKAI_CTL_SOCK={:?}; exec \"${{SHELL:-/bin/sh}}\" -i -l",
        forward.remote_path
    )
}

/// No abnormal-exit cleanup runs continuously (`ISEKAI_PIPE_DESIGN.md` §8
/// Epic M "stale UNIX domain socketのGC"): a crashed/`kill -9`'d tab's
/// local socket only gets removed the next time some tab prepares a new
/// one, which is what this constant bounds. Unix-only: the TCP path has no
/// socket *file* to leak — the OS reclaims the port the moment the process
/// exits.
#[cfg(unix)]
const LOCAL_SOCK_STALE_THRESHOLD: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);

#[cfg(unix)]
pub(crate) fn prepare_ctl_forward(runtime_dir: &Path) -> Result<CtlForward> {
    use std::os::unix::fs::PermissionsExt as _;

    let token = new_ctl_token();
    let local_dir = runtime_dir.join("ctl");
    std::fs::create_dir_all(&local_dir)
        .with_context(|| format!("failed to create {}", local_dir.display()))?;
    std::fs::set_permissions(&local_dir, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("failed to chmod 0700 {}", local_dir.display()))?;

    // Best-effort: a sweep failure should never block this tab's own
    // connection from proceeding.
    if let Ok(removed) = isekai_pipe_core::sweep_stale_sockets(&local_dir, "", LOCAL_SOCK_STALE_THRESHOLD) {
        if !removed.is_empty() {
            log::info!("isekai-ssh: swept {} stale ctl-socket file(s) under {}", removed.len(), local_dir.display());
        }
    }

    Ok(CtlForward {
        remote_path: isekai_pipe_core::ctl_socket_remote_path(&token),
        local_path: local_dir.join(format!("{token}.sock")),
    })
}

/// Binds an OS-assigned loopback TCP port up front (so its number is known
/// before `ssh(1)` is spawned with the `-R` argument that names it — there's
/// no way to reserve a port number without actually binding it). The unix
/// equivalent binds lazily inside [`spawn_ctl_listener`] instead, since a
/// UNIX socket path (unlike a TCP port number) is chosen before binding, not
/// assigned by the bind itself.
#[cfg(not(unix))]
pub(crate) fn prepare_ctl_forward(_runtime_dir: &Path) -> Result<CtlForward> {
    let token = new_ctl_token();
    let listener =
        std::net::TcpListener::bind(("127.0.0.1", 0)).context("failed to bind a local ctl listener on 127.0.0.1")?;
    let local_port = listener.local_addr().context("failed to read local ctl listener port")?.port();

    Ok(CtlForward { remote_path: isekai_pipe_core::ctl_socket_remote_path(&token), local_port, listener: Some(listener) })
}

/// Binds `forward.local_path` (unix) / takes ownership of the already-bound
/// listener (non-unix, see [`prepare_ctl_forward`]) and services incoming
/// ctl connections until the process exits (killed together with the `ssh`
/// child it was spawned alongside — see `wrapper::run`). Each connection
/// carries a plaintext secret preamble line followed by exactly one
/// `isekai_protocol::CtlMessage` line (`isekai-pipe ctl`'s wire contract,
/// see module docs for both).
#[cfg(unix)]
pub(crate) async fn spawn_ctl_listener(forward: &mut CtlForward) {
    use tokio::net::UnixListener;

    let local_path = forward.local_path.clone();
    let secret = forward.remote_path.clone();
    tokio::spawn(async move {
        let result: Result<()> = async {
            let listener = UnixListener::bind(&local_path)
                .with_context(|| format!("failed to bind ctl listener at {}", local_path.display()))?;
            loop {
                let (stream, _) = listener.accept().await.context("ctl listener accept failed")?;
                let secret = secret.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_ctl_connection(stream, &secret).await {
                        eprintln!("isekai-ssh: ctl connection error: {e:#}");
                    }
                });
            }
        }
        .await;
        if let Err(e) = result {
            eprintln!("isekai-ssh: ctl listener error: {e:#}");
        }
    });
}

#[cfg(not(unix))]
pub(crate) async fn spawn_ctl_listener(forward: &mut CtlForward) {
    let Some(listener) = forward.listener.take() else {
        eprintln!("isekai-ssh: ctl listener was already taken (spawn_ctl_listener called twice?)");
        return;
    };
    let secret = forward.remote_path.clone();
    tokio::spawn(async move {
        let result: Result<()> = async {
            listener.set_nonblocking(true).context("failed to set ctl listener non-blocking")?;
            let listener =
                tokio::net::TcpListener::from_std(listener).context("failed to hand off ctl listener to tokio")?;
            loop {
                let (stream, _) = listener.accept().await.context("ctl listener accept failed")?;
                let secret = secret.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_ctl_connection(stream, &secret).await {
                        eprintln!("isekai-ssh: ctl connection error: {e:#}");
                    }
                });
            }
        }
        .await;
        if let Err(e) = result {
            eprintln!("isekai-ssh: ctl listener error: {e:#}");
        }
    });
}

/// Reads and checks the secret preamble line, then decodes exactly one
/// `CtlMessage` line and acts on it. Generic over the stream type
/// (`UnixStream` on unix, `TcpStream` elsewhere, see module docs) since
/// nothing past that point differs by platform.
async fn handle_ctl_connection(stream: impl tokio::io::AsyncRead + Unpin, expected_secret: &str) -> Result<()> {
    use isekai_protocol::decode_ctl_message;
    use tokio::io::{AsyncBufReadExt as _, BufReader};

    let mut reader = BufReader::new(stream);

    // The preamble: whoever is on the other end of this connection must
    // already know this tab's random remote-path token (see module docs).
    let mut secret_line = String::new();
    reader.read_line(&mut secret_line).await.context("failed to read ctl connection preamble")?;
    if secret_line.trim_end_matches('\n') != expected_secret {
        bail!("isekai-ssh: ctl connection preamble did not match this tab's expected secret");
    }

    let mut line = String::new();
    reader
        .read_line(&mut line)
        .await
        .context("failed to read ctl message")?;
    if line.is_empty() {
        // Peer connected and disconnected without sending anything.
        return Ok(());
    }
    let msg = decode_ctl_message(line.trim_end_matches('\n').as_bytes())
        .context("malformed ctl message")?;
    if let Some(seq) = osc_sequence_for(&msg) {
        emit_osc(&seq)?;
    }
    // `ClipboardPullRequest`/`ClipboardPullResponse` produce no OSC sequence
    // (see `osc_sequence_for`'s doc comment) — nothing further to do.
    Ok(())
}

/// Maps an incoming `CtlMessage` to the OSC escape sequence to emit on the
/// local terminal, or `None` for messages this CLI wrapper doesn't act on.
///
/// - `SetTitle` → OSC 0 (icon name + window title).
/// - `ClipboardPush` → OSC 52 clipboard-set. `data_b64` is already the
///   base64 encoding OSC 52 itself expects — no re-encoding needed.
/// - `ClipboardPullRequest` → reading the local system clipboard back
///   (`clip pull` fulfilment) is not implemented for the CLI wrapper yet —
///   that needs a real OS clipboard API, which the Android/iOS app (task
///   #82) has direct access to but this CLI process does not. Returning
///   `None` here (closing without a response) surfaces as a clear
///   "connection closed before a response was received" error to the
///   remote's `isekai-pipe ctl clip pull` rather than hanging.
/// - `ClipboardPullResponse` → we never issue `ClipboardPullRequest`
///   ourselves, so seeing this would only be a misbehaving peer; ignored.
fn osc_sequence_for(msg: &CtlMessage) -> Option<String> {
    match msg {
        CtlMessage::SetTitle { value } => Some(format!("\x1b]0;{value}\x07")),
        CtlMessage::ClipboardPush { data_b64, .. } => Some(format!("\x1b]52;c;{data_b64}\x07")),
        CtlMessage::ClipboardPullRequest {} | CtlMessage::ClipboardPullResponse { .. } => None,
    }
}

fn emit_osc(seq: &str) -> Result<()> {
    use std::io::Write as _;
    // A single `write_all` call (rather than `eprint!`, which may split
    // across multiple internal writes) to minimize the chance of an
    // interleaved write from the concurrently-running `ssh` child garbling
    // the escape sequence on the shared inherited terminal.
    std::io::stderr()
        .write_all(seq.as_bytes())
        .context("write to stderr failed")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_32_lowercase_hex_chars_and_unique() {
        let a = new_ctl_token();
        let b = new_ctl_token();
        assert_eq!(a.len(), 32);
        assert!(a.bytes().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        assert_ne!(a, b);
    }

    #[test]
    fn attempts_forward_only_when_enabled_supported_and_interactive() {
        assert!(should_attempt_ctl_forward(true, 1, 0)); // `isekai-ssh host`
        assert!(!should_attempt_ctl_forward(false, 1, 0)); // directive not set
        assert!(!should_attempt_ctl_forward(true, 2, 0)); // `isekai-ssh host 'cmd'`
    }

    #[cfg(unix)]
    fn fixture_forward() -> CtlForward {
        CtlForward {
            remote_path: "/tmp/isekai-pipe-ctl-aaaa.sock".to_string(),
            local_path: PathBuf::from("/run/user/1000/isekai-ssh/ctl/aaaa.sock"),
        }
    }

    #[cfg(not(unix))]
    fn fixture_forward() -> CtlForward {
        CtlForward { remote_path: "/tmp/isekai-pipe-ctl-aaaa.sock".to_string(), local_port: 54321, listener: None }
    }

    #[test]
    fn forward_option_args_precede_the_destination() {
        let forward = fixture_forward();
        let args = forward_option_args(&forward);
        assert_eq!(args[0], "-R");
        #[cfg(unix)]
        assert_eq!(args[1], "/tmp/isekai-pipe-ctl-aaaa.sock:/run/user/1000/isekai-ssh/ctl/aaaa.sock");
        #[cfg(not(unix))]
        assert_eq!(args[1], "/tmp/isekai-pipe-ctl-aaaa.sock:127.0.0.1:54321");
    }

    #[test]
    fn remote_command_exports_the_remote_path_and_execs_a_login_shell() {
        let forward = fixture_forward();
        let cmd = remote_command_arg(&forward);
        assert!(cmd.contains("ISEKAI_CTL_SOCK=\"/tmp/isekai-pipe-ctl-aaaa.sock\""));
        assert!(cmd.contains("exec \"${SHELL:-/bin/sh}\" -i -l"));
    }

    #[cfg(unix)]
    #[test]
    fn prepare_ctl_forward_creates_a_private_local_directory() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().unwrap();
        let forward = prepare_ctl_forward(dir.path()).unwrap();
        let ctl_dir = dir.path().join("ctl");
        let mode = std::fs::metadata(&ctl_dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
        assert!(forward.remote_path.starts_with(isekai_pipe_core::CTL_SOCKET_DIR));
        assert_eq!(forward.local_path.parent().unwrap(), ctl_dir);
    }

    #[cfg(not(unix))]
    #[test]
    fn prepare_ctl_forward_binds_a_real_loopback_port() {
        let forward = prepare_ctl_forward(Path::new(".")).unwrap();
        assert!(forward.remote_path.starts_with(isekai_pipe_core::CTL_SOCKET_DIR));
        assert_ne!(forward.local_port, 0);
        assert!(forward.listener.is_some());
    }

    #[test]
    fn osc_sequence_for_set_title_is_osc_0() {
        let seq = osc_sequence_for(&CtlMessage::SetTitle { value: "hi".to_string() }).unwrap();
        assert_eq!(seq, "\x1b]0;hi\x07");
    }

    #[test]
    fn osc_sequence_for_clipboard_push_is_osc_52_and_reuses_data_b64_verbatim() {
        let seq = osc_sequence_for(&CtlMessage::ClipboardPush {
            mime: ClipboardMime::TextPlain,
            data_b64: "aGVsbG8=".to_string(),
        })
        .unwrap();
        assert_eq!(seq, "\x1b]52;c;aGVsbG8=\x07");
    }

    #[test]
    fn osc_sequence_for_pull_variants_is_none() {
        assert!(osc_sequence_for(&CtlMessage::ClipboardPullRequest {}).is_none());
        assert!(osc_sequence_for(&CtlMessage::ClipboardPullResponse {
            mime: ClipboardMime::TextPlain,
            data_b64: "aGVsbG8=".to_string(),
        })
        .is_none());
    }

    /// Exercises the real socket read/decode path end-to-end (distinct from
    /// the pure `osc_sequence_for` tests above): a correct preamble followed
    /// by a malformed message line is the one case `handle_ctl_connection`
    /// surfaces as an `Err` without going through `emit_osc` at all, so it's
    /// observable without any stderr side effect on this test process's own
    /// inherited stderr.
    #[tokio::test]
    async fn handle_ctl_connection_rejects_a_malformed_message_after_a_correct_preamble() {
        let (mut client, server_stream) = tokio::io::duplex(256);
        let server = tokio::spawn(async move { handle_ctl_connection(server_stream, "s3cr3t").await });

        use tokio::io::AsyncWriteExt as _;
        client.write_all(b"s3cr3t\nnot json\n").await.unwrap();
        drop(client);
        let result = server.await.unwrap();
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn handle_ctl_connection_rejects_a_mismatched_preamble() {
        let (mut client, server_stream) = tokio::io::duplex(256);
        let server = tokio::spawn(async move { handle_ctl_connection(server_stream, "s3cr3t").await });

        use tokio::io::AsyncWriteExt as _;
        client.write_all(b"wrong-secret\n{}\n").await.unwrap();
        drop(client);
        let result = server.await.unwrap();
        assert!(result.unwrap_err().to_string().contains("preamble"));
    }
}
