//! Per-tab remote→local title/clipboard control-plane
//! (`ISEKAI_PIPE_DESIGN.md` §8 Epic M, `#@isekai ctl-socket yes`) for the
//! **Unix `ssh(1)` ProxyCommand path**. Requests an SSH remote forward
//! (`-R remote-sock:local-endpoint`) for every `isekai-ssh <destination>`
//! invocation — this works whether or not the underlying connection is fresh
//! or shared via ControlMaster/ControlPersist, because OpenSSH scopes `-R`
//! forwards per client request rather than per underlying connection (unlike
//! `isekai-transport`, which cannot distinguish tabs once a connection is
//! shared, see the ADR above Epic M in `ISEKAI_PIPE_DESIGN.md`).
//!
//! **The Windows-native path does not use this module's socket bridge.** It is
//! its own `russh` SSH client, so it requests the streamlocal forward directly
//! on the `client::Handle` and consumes the forwarded channel in-process
//! (`native/mux/ctl_forward.rs`) — there is no local UNIX socket / TCP port to
//! bridge through, because the forwarded `Channel` is already an in-process
//! SSH-protocol object no other local process can connect to. Only the pure,
//! platform-independent helpers here — [`should_attempt_ctl_forward`],
//! [`new_ctl_token`], [`osc_sequence_for`], [`emit_osc`], and
//! [`REMOTE_SOCK_PREFIX`] — are shared between the two paths; the socket-bridge
//! pieces ([`CtlForward`], [`prepare_ctl_forward`], [`spawn_ctl_listener`],
//! [`forward_option_args`], [`remote_command_arg`], [`handle_ctl_connection`])
//! are `#[cfg(unix)]`-only, since `ssh(1)`'s `-R` is the only thing that needs
//! a real local listener.
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
//! - The local endpoint is a UNIX domain socket, relying on its `0700`
//!   directory for access control. Each connection still starts with a
//!   plaintext secret preamble line (the tab's `remote_path`, which
//!   [`isekai-pipe ctl`][isekai-pipe] already has in hand — it's embedded in
//!   `$ISEKAI_CTL_SOCK`'s filename), checked by [`handle_ctl_connection`]
//!   before anything else. The UNIX socket path doesn't strictly need it (its
//!   `0700` directory already provides equivalent protection), but
//!   `isekai-pipe ctl` sends it unconditionally, so the listener checks it
//!   uniformly.
//!
//!   [isekai-pipe]: ../../isekai-pipe/src/ctl.rs

#[cfg(unix)]
use std::path::Path;
#[cfg(unix)]
use std::path::PathBuf;

#[cfg(unix)]
use anyhow::{bail, Context, Result};
use isekai_protocol::CtlMessage;
#[cfg(test)]
use isekai_protocol::ClipboardMime;
#[cfg(unix)]
use isekai_protocol::CtlVarStore;

/// The `setvar`/`getvar` KV store backing this tab's ctl connections (task
/// #16). One `isekai-ssh` invocation is one process per tab (unlike the
/// Android app, which hosts many tabs in one process), so a single
/// process-wide store correctly serves `VarScope::Tab`/`VarScope::Session`
/// alike. `VarScope::Global` also resolves here for now — it does **not**
/// span multiple `isekai-ssh` invocations, since that would need
/// cross-process (likely disk-backed) sharing, deliberately left as
/// documented future work rather than built speculatively (see
/// `isekai_protocol::ctl_vars` module docs for the same trade-off spelled
/// out for both receiving implementations).
#[cfg(unix)]
static CTL_VARS: std::sync::LazyLock<CtlVarStore> = std::sync::LazyLock::new(CtlVarStore::new);

/// `/tmp/isekai-pipe-ctl-<32 hex chars>.sock` on the remote host. Shared by
/// both the Unix `ssh(1)` path and the Windows-native path (the streamlocal
/// forward target).
pub(crate) const REMOTE_SOCK_PREFIX: &str = "/tmp/isekai-pipe-ctl-";

/// The per-tab remote socket + local endpoint pair for the Unix `ssh(1)` `-R`
/// path. Unix-only: the Windows-native path forwards the streamlocal request
/// on its own `client::Handle` and never binds a local listener.
#[cfg(unix)]
pub(crate) struct CtlForward {
    pub(crate) remote_path: String,
    pub(crate) local_path: PathBuf,
}

/// 128 bits of randomness as lowercase hex, matching
/// `isekai_pipe_core`'s `new_intent_id` convention (same entropy, same
/// encoding) so a squatting attacker cannot feasibly pre-guess the path.
pub(crate) fn new_ctl_token() -> String {
    use rand::RngCore as _;
    use std::fmt::Write as _;
    let mut bytes = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut bytes);
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Pure decision of whether to attempt a ctl-socket forward for this
/// invocation, given the resolved directive and the parsed ssh args. Split
/// out from `prepare_ctl_forward` so it's testable without touching the
/// filesystem or spawning anything. Shared by both connection paths.
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

/// The replacement remote command: since there is no pre-existing remote
/// command by construction (see `should_attempt_ctl_forward`), this becomes
/// the sole arg **after** the destination, exporting `$ISEKAI_CTL_SOCK` and
/// exec'ing an interactive login shell in its place (replicating what
/// `sshd` would have run implicitly had no remote command been given at
/// all).
#[cfg(unix)]
pub(crate) fn remote_command_arg(forward: &CtlForward) -> String {
    format!(
        "export ISEKAI_CTL_SOCK={:?}; exec \"${{SHELL:-/bin/sh}}\" -i -l",
        forward.remote_path
    )
}

/// No abnormal-exit cleanup runs continuously (`ISEKAI_PIPE_DESIGN.md` §8
/// Epic M "stale UNIX domain socketのGC"): a crashed/`kill -9`'d tab's
/// local socket only gets removed the next time some tab prepares a new
/// one, which is what this constant bounds.
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
        remote_path: format!("{REMOTE_SOCK_PREFIX}{token}.sock"),
        local_path: local_dir.join(format!("{token}.sock")),
    })
}

/// Binds `forward.local_path` and services incoming ctl connections until the
/// process exits (killed together with the `ssh` child it was spawned
/// alongside — see `wrapper::run`). Each connection carries a plaintext secret
/// preamble line followed by exactly one `isekai_protocol::CtlMessage` line
/// (`isekai-pipe ctl`'s wire contract, see module docs).
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

/// Reads and checks the secret preamble line, then decodes exactly one
/// `CtlMessage` line and acts on it — applying it as an OSC sequence
/// (`SetTitle`/`ClipboardPush`), reading/writing this tab's `CTL_VARS` store
/// (`SetVar`/`GetVarRequest`, task #16), or (for message kinds this CLI
/// wrapper doesn't fulfil, e.g. `ClipboardPullRequest`) simply closing
/// without a response.
#[cfg(unix)]
async fn handle_ctl_connection(
    stream: impl tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    expected_secret: &str,
) -> Result<()> {
    use isekai_protocol::decode_ctl_message;
    use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};

    let (read_half, mut write_half) = tokio::io::split(stream);
    let mut reader = BufReader::new(read_half);

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
    match msg {
        CtlMessage::SetVar { key, value, .. } => {
            CTL_VARS.set(key, value);
        }
        CtlMessage::GetVarRequest { key, .. } => {
            let response = CtlMessage::GetVarResponse { value: CTL_VARS.get(&key) };
            let mut out = serde_json::to_vec(&response).context("failed to encode getvar response")?;
            out.push(b'\n');
            write_half.write_all(&out).await.context("failed to write getvar response")?;
            write_half.shutdown().await.ok();
        }
        // `SetTitle`/`ClipboardPush` were already applied via `osc_sequence_for`
        // above. `ClipboardPullRequest`/`ClipboardPullResponse`/`GetVarResponse`
        // produce no OSC sequence and need no response from this wrapper (see
        // `osc_sequence_for`'s doc comment for why `ClipboardPullRequest`
        // specifically isn't fulfilled here).
        _ => {}
    }
    Ok(())
}

/// Maps an incoming `CtlMessage` to the OSC escape sequence to emit on the
/// local terminal, or `None` for messages this CLI wrapper doesn't act on.
/// Shared by the Unix `ssh(1)` path and the Windows-native mux client/owner
/// paths (`native/mux`).
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
/// - `SetVar`/`GetVarRequest`/`GetVarResponse` (task #16) have no OSC
///   equivalent — they're handled directly in `handle_ctl_connection`
///   against `CTL_VARS` instead of through this OSC-emitting path.
/// - `BuildRequest`/`BuildOutputChunk`/`BuildFinished` (Epic P) have no OSC
///   equivalent either — unlike `title`/`clip`, there is no terminal escape
///   sequence for "run a local command", so this variant is handled by a
///   dedicated long-lived branch in `handle_ctl_connection` instead of the
///   OSC-emitting path every other variant goes through.
pub(crate) fn osc_sequence_for(msg: &CtlMessage) -> Option<String> {
    match msg {
        CtlMessage::SetTitle { value } => Some(format!("\x1b]0;{value}\x07")),
        CtlMessage::ClipboardPush { data_b64, .. } => Some(format!("\x1b]52;c;{data_b64}\x07")),
        CtlMessage::ClipboardPullRequest {}
        | CtlMessage::ClipboardPullResponse { .. }
        | CtlMessage::SetVar { .. }
        | CtlMessage::GetVarRequest { .. }
        | CtlMessage::GetVarResponse { .. }
        | CtlMessage::BuildRequest { .. }
        | CtlMessage::BuildOutputChunk { .. }
        | CtlMessage::BuildFinished { .. } => None,
    }
}

/// Writes an OSC escape sequence to this process's own stderr in a single
/// `write_all` (rather than `eprint!`, which may split across multiple
/// internal writes) to minimize the chance of an interleaved write from a
/// concurrently-running relay garbling the escape sequence on the shared
/// terminal.
pub(crate) fn emit_osc(seq: &str) -> anyhow::Result<()> {
    use anyhow::Context as _;
    use std::io::Write as _;
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

    #[cfg(unix)]
    #[test]
    fn forward_option_args_precede_the_destination() {
        let forward = fixture_forward();
        let args = forward_option_args(&forward);
        assert_eq!(args[0], "-R");
        assert_eq!(args[1], "/tmp/isekai-pipe-ctl-aaaa.sock:/run/user/1000/isekai-ssh/ctl/aaaa.sock");
    }

    #[cfg(unix)]
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
        assert!(forward.remote_path.starts_with(REMOTE_SOCK_PREFIX));
        assert_eq!(forward.local_path.parent().unwrap(), ctl_dir);
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
    #[cfg(unix)]
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

    #[cfg(unix)]
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
