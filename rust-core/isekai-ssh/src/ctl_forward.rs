//! Per-tab UNIX domain socket forward for the remote→local title/clipboard
//! control-plane (`ISEKAI_PIPE_DESIGN.md` §8 Epic M, `#@isekai ctl-socket
//! yes`). Requests an SSH remote forward (`-R remote-sock:local-sock`) for
//! every `isekai-ssh <destination>` invocation — this works whether or not
//! the underlying connection is fresh or shared via ControlMaster/
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
//! - UNIX domain sockets only (`#[cfg(unix)]`): `tokio::net::UnixListener`
//!   has no Windows backend as of this writing. `ctl-socket yes` on a
//!   non-unix build logs once to stderr and otherwise no-ops, rather than
//!   failing the connection — the same "opportunistic, silent fallback"
//!   policy as physical multipath (`CLAUDE.md`).

use std::path::PathBuf;

use anyhow::{Context, Result};
#[cfg(unix)]
use isekai_protocol::CtlMessage;
#[cfg(all(unix, test))]
use isekai_protocol::ClipboardMime;

/// `/tmp/isekai-pipe-ctl-<32 hex chars>.sock` on the remote host.
const REMOTE_SOCK_PREFIX: &str = "/tmp/isekai-pipe-ctl-";

pub(crate) struct CtlForward {
    pub(crate) remote_path: String,
    pub(crate) local_path: PathBuf,
}

/// 128 bits of randomness as lowercase hex, matching
/// `isekai_pipe_core`'s `new_intent_id` convention (same entropy, same
/// encoding) so a squatting attacker cannot feasibly pre-guess the path.
fn new_ctl_token() -> String {
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

/// Whether this build can even attempt a ctl-socket forward at all
/// (independent of whether the directive is enabled for this destination).
pub(crate) fn is_supported() -> bool {
    cfg!(unix)
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
    ctl_socket_enabled && is_supported() && ssh_args_len == destination_index + 1
}

/// The `-R` forward flag pair. Must be spliced in **before** the
/// destination in the final `ssh(1)` argv — anything after the destination
/// is the remote command, not an option, to `ssh(1)`.
pub(crate) fn forward_option_args(forward: &CtlForward) -> [String; 2] {
    [
        "-R".to_string(),
        format!("{}:{}", forward.remote_path, forward.local_path.display()),
    ]
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

#[cfg(unix)]
pub(crate) fn prepare_ctl_forward(runtime_dir: &std::path::Path) -> Result<CtlForward> {
    use std::os::unix::fs::PermissionsExt as _;

    let token = new_ctl_token();
    let local_dir = runtime_dir.join("ctl");
    std::fs::create_dir_all(&local_dir)
        .with_context(|| format!("failed to create {}", local_dir.display()))?;
    std::fs::set_permissions(&local_dir, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("failed to chmod 0700 {}", local_dir.display()))?;

    Ok(CtlForward {
        remote_path: format!("{REMOTE_SOCK_PREFIX}{token}.sock"),
        local_path: local_dir.join(format!("{token}.sock")),
    })
}

#[cfg(not(unix))]
pub(crate) fn prepare_ctl_forward(_runtime_dir: &std::path::Path) -> Result<CtlForward> {
    anyhow::bail!("isekai-ssh: ctl-socket forwarding is only supported on unix targets")
}

/// Binds `forward.local_path` and services incoming ctl connections until
/// the process exits (killed together with the `ssh` child it was spawned
/// alongside — see `wrapper::run`). Each connection carries exactly one
/// `isekai_protocol::CtlMessage` line (`isekai-pipe ctl`'s wire contract).
#[cfg(unix)]
pub(crate) async fn spawn_ctl_listener(local_path: PathBuf) {
    tokio::spawn(async move {
        if let Err(e) = run_ctl_listener(&local_path).await {
            eprintln!("isekai-ssh: ctl listener error: {e:#}");
        }
    });
}

/// Never actually reached at runtime (`prepare_ctl_forward` above always
/// returns `Err` on non-unix, so `run()`'s `Ok` match arm calling this is
/// dead code there) — exists only so that match arm still type-checks when
/// cross-compiling for a non-unix target.
#[cfg(not(unix))]
pub(crate) async fn spawn_ctl_listener(_local_path: PathBuf) {}

#[cfg(unix)]
async fn run_ctl_listener(local_path: &std::path::Path) -> Result<()> {
    use tokio::net::UnixListener;

    let listener = UnixListener::bind(local_path)
        .with_context(|| format!("failed to bind ctl listener at {}", local_path.display()))?;
    loop {
        let (stream, _) = listener.accept().await.context("ctl listener accept failed")?;
        tokio::spawn(async move {
            if let Err(e) = handle_ctl_connection(stream).await {
                eprintln!("isekai-ssh: ctl connection error: {e:#}");
            }
        });
    }
}

#[cfg(unix)]
async fn handle_ctl_connection(stream: tokio::net::UnixStream) -> Result<()> {
    use isekai_protocol::decode_ctl_message;
    use tokio::io::{AsyncBufReadExt as _, BufReader};

    let mut reader = BufReader::new(stream);
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
#[cfg(unix)]
fn osc_sequence_for(msg: &CtlMessage) -> Option<String> {
    match msg {
        CtlMessage::SetTitle { value } => Some(format!("\x1b]0;{value}\x07")),
        CtlMessage::ClipboardPush { data_b64, .. } => Some(format!("\x1b]52;c;{data_b64}\x07")),
        CtlMessage::ClipboardPullRequest {} | CtlMessage::ClipboardPullResponse { .. } => None,
    }
}

#[cfg(unix)]
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

    #[test]
    fn forward_option_args_precede_the_destination() {
        let forward = CtlForward {
            remote_path: "/tmp/isekai-pipe-ctl-aaaa.sock".to_string(),
            local_path: PathBuf::from("/run/user/1000/isekai-ssh/ctl/aaaa.sock"),
        };
        let args = forward_option_args(&forward);
        assert_eq!(args[0], "-R");
        assert_eq!(
            args[1],
            "/tmp/isekai-pipe-ctl-aaaa.sock:/run/user/1000/isekai-ssh/ctl/aaaa.sock"
        );
    }

    #[test]
    fn remote_command_exports_the_remote_path_and_execs_a_login_shell() {
        let forward = CtlForward {
            remote_path: "/tmp/isekai-pipe-ctl-aaaa.sock".to_string(),
            local_path: PathBuf::from("/run/user/1000/isekai-ssh/ctl/aaaa.sock"),
        };
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

    #[cfg(unix)]
    #[test]
    fn osc_sequence_for_set_title_is_osc_0() {
        let seq = osc_sequence_for(&CtlMessage::SetTitle { value: "hi".to_string() }).unwrap();
        assert_eq!(seq, "\x1b]0;hi\x07");
    }

    #[cfg(unix)]
    #[test]
    fn osc_sequence_for_clipboard_push_is_osc_52_and_reuses_data_b64_verbatim() {
        let seq = osc_sequence_for(&CtlMessage::ClipboardPush {
            mime: ClipboardMime::TextPlain,
            data_b64: "aGVsbG8=".to_string(),
        })
        .unwrap();
        assert_eq!(seq, "\x1b]52;c;aGVsbG8=\x07");
    }

    #[cfg(unix)]
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
    /// the pure `osc_sequence_for` tests above) without touching this test
    /// process's actual inherited stderr: a malformed line is the one case
    /// `handle_ctl_connection` surfaces as an `Err` without going through
    /// `emit_osc` at all, so it's observable without any stderr side effect.
    #[cfg(unix)]
    #[tokio::test]
    async fn handle_ctl_connection_rejects_a_malformed_message() {
        let dir = tempfile::tempdir().unwrap();
        let sock_path = dir.path().join("ctl.sock");
        let listener = tokio::net::UnixListener::bind(&sock_path).unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            handle_ctl_connection(stream).await
        });

        let mut client = tokio::net::UnixStream::connect(&sock_path).await.unwrap();
        use tokio::io::AsyncWriteExt as _;
        client.write_all(b"not json\n").await.unwrap();
        drop(client);
        let result = server.await.unwrap();
        assert!(result.is_err());
    }
}
