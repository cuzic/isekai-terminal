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
//! [`new_ctl_token`], [`osc_sequence_for`], [`emit_osc`], [`REMOTE_SOCK_PREFIX`],
//! and [`build_login_shell_command`] — are shared between the two paths; the
//! socket-bridge pieces ([`CtlForward`], [`prepare_ctl_forward`],
//! [`spawn_ctl_listener`], [`forward_option_args`], [`remote_command_arg`],
//! [`handle_ctl_connection`]) are `#[cfg(unix)]`-only, since `ssh(1)`'s `-R`
//! is the only thing that needs a real local listener.
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
use isekai_protocol::{CtlMessage, NotifyKind};
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
/// the sole arg **after** the destination, exporting `$ISEKAI_CTL_SOCK`
/// (and, if configured, `$ISEKAI_TAB_IDLE_COLOR`/`$ISEKAI_TAB_ATTENTION_COLOR`,
/// `ISEKAI_PIPE_DESIGN.md` §8 Epic Q) and exec'ing an interactive login shell
/// in its place (replicating what `sshd` would have run implicitly had no
/// remote command been given at all).
///
/// Thin platform-specific wrapper around [`build_login_shell_command`], the
/// actual (platform-independent, pure) string builder shared with the
/// Windows-native path (`native/mux/ctl_forward.rs`) — previously each path
/// duplicated this `format!` independently, which is exactly the kind of
/// divergence that let the Windows-native copy silently miss a fix applied
/// only here.
#[cfg(unix)]
pub(crate) fn remote_command_arg(
    forward: &CtlForward,
    tab_idle_color: Option<(u8, u8, u8)>,
    tab_attention_color: Option<(u8, u8, u8)>,
) -> String {
    build_login_shell_command(&forward.remote_path, tab_idle_color, tab_attention_color)
}

/// Builds the `export ...; exec "$SHELL" -i -l` remote command string shared
/// by the Unix `ssh(1)` path ([`remote_command_arg`]) and the Windows-native
/// path (`native/mux/ctl_forward.rs`).
///
/// `tab_idle_color`/`tab_attention_color` come from `#@isekai
/// tab-idle-color`/`tab-attention-color`, already validated and parsed to
/// `(u8, u8, u8)` at config-resolution time
/// (`wrapper/config.rs::parse_tab_color`). This is deliberate defense in
/// depth, not redundant validation: by the time a color reaches this
/// function it is a *type* that cannot represent shell metacharacters (three
/// bytes, formatted as exactly two lowercase hex digits each via
/// [`isekai_pipe_core::format_hex_color`]), so this function has no way to
/// reintroduce the shell-injection bug even if a future caller forgot to
/// validate — unlike passing the raw `#@isekai` argument `String` straight
/// through, which is what the original (buggy) implementation effectively
/// did via `{:?}` Rust `Debug`-escaping (that escapes `"`/`\`/control bytes,
/// not `$`/backtick, and doesn't stop a leading `#` from turning the whole
/// `export` line into a shell comment that silently skips the `exec` and
/// kills the SSH session — `.claude/rules/always-connects.md`).
pub(crate) fn build_login_shell_command(
    remote_path: &str,
    tab_idle_color: Option<(u8, u8, u8)>,
    tab_attention_color: Option<(u8, u8, u8)>,
) -> String {
    let mut exports = format!("export ISEKAI_CTL_SOCK={remote_path:?};");
    if let Some(color) = tab_idle_color {
        exports.push_str(&format!(" export ISEKAI_TAB_IDLE_COLOR={};", isekai_pipe_core::format_hex_color(color)));
    }
    if let Some(color) = tab_attention_color {
        exports.push_str(&format!(
            " export ISEKAI_TAB_ATTENTION_COLOR={};",
            isekai_pipe_core::format_hex_color(color)
        ));
    }
    format!("{exports} exec \"${{SHELL:-/bin/sh}}\" -i -l")
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
pub(crate) async fn spawn_ctl_listener(forward: &mut CtlForward, host: String) {
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
                let host = host.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_ctl_connection(stream, &secret, &host).await {
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
/// (`SetVar`/`GetVarRequest`, task #16), running a build profile
/// (`BuildRequest`, Epic P — the one variant that keeps the connection open
/// past this first message, see `run_build`), or (for message kinds this CLI
/// wrapper doesn't fulfil, e.g. `ClipboardPullRequest`) simply closing
/// without a response.
#[cfg(unix)]
async fn handle_ctl_connection(
    stream: impl tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    expected_secret: &str,
    host: &str,
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
    if let Some(seq) = osc_sequence_for(&msg, TerminalKind::resolve()) {
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
        CtlMessage::BuildRequest { profile } => {
            run_build(&mut write_half, host, &profile).await?;
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

/// Runs the build profile `(host, profile_name)` resolves to (Epic P) and
/// streams its output back over `write_half` as `BuildOutputChunk`s,
/// finishing with `BuildFinished`. Unlike every other `CtlMessage` this
/// wrapper handles, this keeps `write_half` busy for the whole build's
/// duration rather than a single reply.
///
/// If the ctl connection breaks mid-build (the remote killed `isekai-pipe
/// ctl build`, e.g. `Ctrl-C`, or the SSH session itself dropped), the build
/// child process is killed immediately rather than left running unattended
/// — the same "every session must have a guaranteed cleanup path" principle
/// `.claude/rules/always-connects.md` documents for the fencing-slot lesson,
/// applied here to a local child process instead of a remote session slot.
#[cfg(unix)]
async fn run_build(
    write_half: &mut (impl tokio::io::AsyncWrite + Unpin),
    host: &str,
    profile_name: &str,
) -> Result<()> {
    let profile = crate::build_profile::default_build_profiles_path()
        .and_then(|path| crate::build_profile::load_build_profiles(&path))
        .ok()
        .and_then(|store| crate::build_profile::find_profile(&store, host, profile_name).cloned());

    let Some(profile) = profile else {
        send_build_output(
            write_half,
            isekai_protocol::BuildOutputStream::Stderr,
            format!("isekai-ssh: no build profile registered for {host:?}/{profile_name:?}\n").into_bytes(),
        )
        .await?;
        send_build_finished(write_half, 127, Vec::new()).await?;
        emit_build_progress_result(false);
        return Ok(());
    };

    // Epic P × 2026-08のOSC 9;4連携: ビルドの成否は事前に分からないので
    // `Indeterminate`(スピナー)で開始を知らせる。`emit_build_progress_result`と
    // 対で、doc commentはそちらを参照。
    if let Some(seq) = osc_sequence_for(&CtlMessage::SetProgress { state: isekai_protocol::ProgressState::Indeterminate, progress: 0 }, TerminalKind::resolve()) {
        let _ = emit_osc(&seq);
    }

    let mut child = crate::build_exec::spawn_shell_command(&profile.command, &profile.dir)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("isekai-ssh: failed to spawn build profile {host:?}/{profile_name:?}"))?;
    let stdout = child.stdout.take().expect("stdout was piped");
    let stderr = child.stderr.take().expect("stderr was piped");

    let (tx, mut rx) = tokio::sync::mpsc::channel::<(isekai_protocol::BuildOutputStream, Vec<u8>)>(32);
    let stdout_task = tokio::spawn(crate::build_exec::pump_bytes(stdout, isekai_protocol::BuildOutputStream::Stdout, tx.clone()));
    let stderr_task = tokio::spawn(crate::build_exec::pump_bytes(stderr, isekai_protocol::BuildOutputStream::Stderr, tx.clone()));
    drop(tx);

    let mut write_failed = false;
    while let Some((stream, bytes)) = rx.recv().await {
        if send_build_output(write_half, stream, bytes).await.is_err() {
            write_failed = true;
            break;
        }
    }
    // Dropping `rx` (implicit at scope end below doesn't happen yet — done
    // explicitly here) unblocks any pump task still waiting on a full
    // channel: their next `tx.send(...)` sees the receiver gone and returns
    // immediately instead of hanging, so awaiting them below can't deadlock.
    drop(rx);

    if write_failed {
        // The remote is gone; there is nothing left to report to. Kill the
        // child rather than let it keep running unattended.
        let _ = child.start_kill();
        let _ = child.wait().await;
        let _ = stdout_task.await;
        let _ = stderr_task.await;
        emit_build_progress_result(false);
        bail!("isekai-ssh: ctl connection closed before the build finished; killed the child process");
    }
    let _ = stdout_task.await;
    let _ = stderr_task.await;

    let status = child.wait().await.context("isekai-ssh: failed to wait for the build child process")?;
    // `status.code()` is `None` only if the process was killed by a signal
    // (not the `write_failed` kill path above, which already returned) —
    // `-1` is not a valid process exit code on any platform, so it can't be
    // confused with a real one.
    let exit_code = status.code().unwrap_or(-1);
    emit_build_progress_result(exit_code == 0);

    let result_paths: Vec<String> = match (&profile.result_glob, &profile.dest_dir) {
        (Some(glob), Some(_dest_dir)) => crate::build_exec::glob_results(&profile.dir, glob)
            .unwrap_or_default()
            .into_iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect(),
        _ => Vec::new(),
    };
    if let Some(dest_dir) = &profile.dest_dir {
        crate::build_exec::spawn_result_push(host.to_string(), dest_dir.clone(), result_paths.clone());
    }
    send_build_finished(write_half, exit_code, result_paths).await?;
    Ok(())
}

/// Clears the OSC 9;4 progress indicator `run_build` set to `Indeterminate`
/// at the start, replacing it with `Error` on failure (no profile found /
/// remote disconnected mid-build / non-zero exit) or `None` (hide) on
/// success. Same best-effort/no-op-on-failure posture as the start signal —
/// see the call site in `run_build`.
fn emit_build_progress_result(success: bool) {
    let state = if success { isekai_protocol::ProgressState::None } else { isekai_protocol::ProgressState::Error };
    if let Some(seq) = osc_sequence_for(&CtlMessage::SetProgress { state, progress: 0 }, TerminalKind::resolve()) {
        let _ = emit_osc(&seq);
    }
}

/// Thin wrappers around `build_exec`'s shared encoders that just add the
/// actual write — kept here (rather than inlining `build_exec::encode_*` at
/// each call site in `run_build`) so `run_build` itself reads the same as
/// before this encoding logic moved out to be shared with the Windows-native
/// dispatch paths (`ISEKAI_PIPE_DESIGN.md` §8 Epic P Phase 2).
#[cfg(unix)]
async fn send_build_output(
    write_half: &mut (impl tokio::io::AsyncWrite + Unpin),
    stream: isekai_protocol::BuildOutputStream,
    bytes: Vec<u8>,
) -> Result<()> {
    use tokio::io::AsyncWriteExt as _;
    let out = crate::build_exec::encode_build_output_chunk(stream, bytes)?;
    write_half.write_all(&out).await.context("isekai-ssh: failed to write build output chunk")?;
    Ok(())
}

#[cfg(unix)]
async fn send_build_finished(
    write_half: &mut (impl tokio::io::AsyncWrite + Unpin),
    exit_code: i32,
    result_paths: Vec<String>,
) -> Result<()> {
    use tokio::io::AsyncWriteExt as _;
    let out = crate::build_exec::encode_build_finished(exit_code, result_paths)?;
    write_half.write_all(&out).await.context("isekai-ssh: failed to write build finished message")?;
    write_half.shutdown().await.ok();
    Ok(())
}

/// Which real terminal emulator this `isekai-ssh` process is running inside,
/// for OSC variants that differ per terminal (currently only `SetTabColor`,
/// see `osc_sequence_for`). Deliberately NOT read from inside
/// `osc_sequence_for` itself — that function stays a pure, unit-testable
/// mapping from `(CtlMessage, TerminalKind)` to a `String`; callers resolve
/// `TerminalKind` once via [`TerminalKind::resolve`] and pass it in, the same
/// "resolve once, thread the value through" shape this project already uses
/// for `TofuConfirmation` (`.claude/rules/always-connects.md`) rather than
/// having downstream code re-derive intent from raw env/state.
///
/// Re-exported from the standalone `osc-color` crate (extracted 2026-08 when
/// `claude-hookd` was split out into its own isekai-terminal-independent
/// crate, which needs the identical terminal-detection/OSC-mapping logic
/// when emitting OSC directly instead of via this ctl-socket path) — kept as
/// a `pub(crate)` alias here so existing call sites in this crate don't need
/// to change their imports.
pub(crate) use osc_color::TerminalKind;

/// Maps an incoming `CtlMessage` to the OSC escape sequence to emit on the
/// local terminal, or `None` for messages this CLI wrapper doesn't act on.
/// Shared by the Unix `ssh(1)` path and the Windows-native mux client/owner
/// paths (`native/mux`, which always pass [`TerminalKind::WindowsTerminal`]
/// — iTerm2 doesn't exist on Windows).
///
/// - `SetTitle` → OSC 0 (icon name + window title). Passed through
///   [`strip_ascii_control_chars`] first, same reasoning as `Notify` below
///   (fixed 2026-08: this arm used to embed `value` verbatim, so a remote
///   host that could reach the ctl socket could smuggle an ESC/BEL inside
///   the title to terminate the OSC 0 sequence early and inject arbitrary
///   escape sequences — e.g. a spoofed OSC 52 clipboard write — into the
///   user's real terminal).
/// - `Notify`: 2つの独立した系統を1つのワイヤーメッセージが共有する
///   (`isekai_protocol::ctl::NotifyKind`のdocコメント参照)ため、kindで分岐する:
///   - AI/汎用の注目通知(`Waiting`/`Done`/`Info`、`AI_INTEGRATION_DESIGN.md` §6.1)
///     → OSC 9 (the iTerm2/Growl-style "post a system notification" convention
///     several terminal emulators already support, same "reuse an existing
///     escape sequence rather than inventing one" choice as `SetTitle`/OSC 0
///     and `ClipboardPush`/OSC 52). `title`/`body` are joined into OSC 9's
///     single message argument since it has no separate title/body fields.
///     Both are passed through [`strip_ascii_control_chars`] first — the
///     remote host only controls the text content (`AI_INTEGRATION_DESIGN.md`
///     §6.1/§8's "no execution authority" boundary), so an ESC/BEL byte
///     inside either field must not be able to inject an arbitrary escape
///     sequence into this process's own stderr.
///   - tmux hook由来(`Bell`/`Activity`/`Silence`/`JobDone`、#57) → this CLI
///     wrapper has no notification surface of its own (that's the Android
///     app's job); ignored (`None`) rather than implemented, matching how
///     `ClipboardPullRequest` below is also left unimplemented pending a
///     capability this wrapper doesn't have yet.
/// - `SetTabColor` → OSC 4 palette-index 264 (`TerminalKind::WindowsTerminal`,
///   Windows Terminal's private convention for the tab background color,
///   `microsoft/terminal` PR #13058, which closed the original feature
///   request #6574 — a harmless no-op on terminals that don't recognize that
///   index) or iTerm2's proprietary `OSC 6;1;bg;<channel>;brightness;<0-255>`
///   (`TerminalKind::ITerm2`, one sequence per RGB channel — see iTerm2's
///   "Proprietary Escape Codes" documentation).
/// - `SetProgress` → OSC 9;4 (`ProgressState` doc), ConEmu-originated
///   progress-bar convention also implemented by Windows Terminal (tab
///   icon + taskbar integration there). Harmless no-op elsewhere.
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
pub(crate) fn osc_sequence_for(msg: &CtlMessage, terminal: TerminalKind) -> Option<String> {
    match msg {
        CtlMessage::SetTitle { value } => Some(format!("\x1b]0;{}\x07", strip_ascii_control_chars(value))),
        CtlMessage::Notify { kind, title, body, .. } => match kind {
            NotifyKind::Waiting | NotifyKind::Done | NotifyKind::Info => {
                let title = strip_ascii_control_chars(title);
                let body = strip_ascii_control_chars(body);
                let message = if body.is_empty() { title } else { format!("{title}: {body}") };
                Some(format!("\x1b]9;{message}\x07"))
            }
            NotifyKind::Bell | NotifyKind::Activity | NotifyKind::Silence | NotifyKind::JobDone => None,
        },
        CtlMessage::SetTabColor { r, g, b } => Some(osc_color::tab_color_sequence(terminal, *r, *g, *b)),
        CtlMessage::SetProgress { state, progress } => {
            Some(format!("\x1b]9;4;{};{}\x07", *state as u8, progress))
        }
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

/// Removes ASCII C0 control characters (`< 0x20`) and DEL (`0x7F`) from
/// remote-controlled text before it is embedded in an OSC escape sequence
/// written to this process's own stderr ([`emit_osc`]). Without this, a
/// `Notify` title/body containing `ESC`/`BEL` (accidental — e.g. a hook that
/// captured ANSI-colored output — or crafted) could terminate the intended
/// `OSC 9 ... BEL` sequence early and inject an arbitrary escape sequence
/// into the user's real terminal (`AI_INTEGRATION_DESIGN.md` §6.1's "no
/// execution authority" boundary applies to more than just shell commands).
fn strip_ascii_control_chars(text: &str) -> String {
    text.chars().filter(|c| !c.is_ascii_control()).collect()
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
        let cmd = remote_command_arg(&forward, None, None);
        assert!(cmd.contains("ISEKAI_CTL_SOCK=\"/tmp/isekai-pipe-ctl-aaaa.sock\""));
        assert!(cmd.contains("exec \"${SHELL:-/bin/sh}\" -i -l"));
        assert!(!cmd.contains("ISEKAI_TAB_IDLE_COLOR"));
        assert!(!cmd.contains("ISEKAI_TAB_ATTENTION_COLOR"));
    }

    #[test]
    fn build_login_shell_command_exports_configured_tab_colors() {
        let cmd = build_login_shell_command("/tmp/isekai-pipe-ctl-aaaa.sock", Some((0x20, 0x20, 0x20)), Some((0xff, 0x88, 0x00)));
        assert!(cmd.contains("ISEKAI_CTL_SOCK=\"/tmp/isekai-pipe-ctl-aaaa.sock\""));
        assert!(cmd.contains("export ISEKAI_TAB_IDLE_COLOR=202020;"));
        assert!(cmd.contains("export ISEKAI_TAB_ATTENTION_COLOR=ff8800;"));
        assert!(cmd.ends_with("exec \"${SHELL:-/bin/sh}\" -i -l"));
    }

    #[test]
    fn build_login_shell_command_omits_unset_colors() {
        let cmd = build_login_shell_command("/tmp/isekai-pipe-ctl-aaaa.sock", None, None);
        assert!(!cmd.contains("ISEKAI_TAB_IDLE_COLOR"));
        assert!(!cmd.contains("ISEKAI_TAB_ATTENTION_COLOR"));
    }

    #[test]
    fn build_login_shell_command_never_emits_a_hash_prefix_or_shell_metacharacters() {
        // The concrete bug this (r, g, b) — not `String` — signature exists
        // to make structurally impossible: exporting a value that starts
        // with '#' (a shell comment) or contains '$'/backtick would silently
        // skip the trailing `exec` and kill the SSH session
        // (`.claude/rules/always-connects.md`). Exhaustive over all 256
        // values per channel would be excessive; spot-check the bytes whose
        // ASCII value could plausibly render as a shell metacharacter.
        for byte in [0x00, 0x23, 0x24, 0x27, 0x60, 0xff] {
            let cmd = build_login_shell_command("/tmp/x.sock", Some((byte, byte, byte)), None);
            let exported = cmd
                .split("ISEKAI_TAB_IDLE_COLOR=")
                .nth(1)
                .and_then(|rest| rest.split(';').next())
                .unwrap();
            assert_eq!(exported.len(), 6);
            assert!(exported.bytes().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        }
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
        let seq = osc_sequence_for(&CtlMessage::SetTitle { value: "hi".to_string() }, TerminalKind::WindowsTerminal).unwrap();
        assert_eq!(seq, "\x1b]0;hi\x07");
    }

    /// Regression test for the OSC injection this sanitization closes
    /// (2026-08): a title containing `ESC`/`BEL` could otherwise terminate
    /// the intended `OSC 0 ... BEL` sequence early and splice an attacker-
    /// or hook-controlled escape sequence (here, a spoofed OSC 52 clipboard
    /// write) into the user's real terminal. Mirrors
    /// `osc_sequence_for_notify_strips_esc_and_bel_from_title_and_body`.
    #[test]
    fn osc_sequence_for_set_title_strips_esc_and_bel() {
        let seq = osc_sequence_for(
            &CtlMessage::SetTitle { value: "hi\x1b]52;c;cHduZWQ=\x07pwned".to_string() },
            TerminalKind::WindowsTerminal,
        )
        .unwrap();
        assert_eq!(seq, "\x1b]0;hi]52;c;cHduZWQ=pwned\x07");
    }

    #[test]
    fn osc_sequence_for_notify_joins_title_and_body_with_osc_9() {
        let seq = osc_sequence_for(
            &CtlMessage::Notify {
                kind: NotifyKind::Info,
                tmux_tag: String::new(),
                seq: 0,
                title: "hi".to_string(),
                body: "there".to_string(),
            },
            TerminalKind::WindowsTerminal,
        )
        .unwrap();
        assert_eq!(seq, "\x1b]9;hi: there\x07");
    }

    #[test]
    fn osc_sequence_for_notify_omits_separator_when_body_is_empty() {
        let seq = osc_sequence_for(
            &CtlMessage::Notify {
                kind: NotifyKind::Info,
                tmux_tag: String::new(),
                seq: 0,
                title: "hi".to_string(),
                body: String::new(),
            },
            TerminalKind::WindowsTerminal,
        )
        .unwrap();
        assert_eq!(seq, "\x1b]9;hi\x07");
    }

    #[test]
    fn osc_sequence_for_notify_tmux_kinds_is_none() {
        for kind in [NotifyKind::Bell, NotifyKind::Activity, NotifyKind::Silence, NotifyKind::JobDone] {
            let seq = osc_sequence_for(
                &CtlMessage::Notify {
                    kind,
                    tmux_tag: "session:0".to_string(),
                    seq: 1,
                    title: String::new(),
                    body: String::new(),
                },
                TerminalKind::WindowsTerminal,
            );
            assert!(seq.is_none(), "{kind:?} should not produce an OSC sequence");
        }
    }

    /// Regression test for the OSC injection this sanitization closes: a
    /// title/body containing `ESC`/`BEL` could otherwise terminate the
    /// intended `OSC 9 ... BEL` sequence early and splice an attacker- or
    /// hook-controlled escape sequence into the user's real terminal.
    #[test]
    fn osc_sequence_for_notify_strips_esc_and_bel_from_title_and_body() {
        let seq = osc_sequence_for(
            &CtlMessage::Notify {
                kind: NotifyKind::Info,
                tmux_tag: String::new(),
                seq: 0,
                title: "hi\x1b]0;pwned\x07".to_string(),
                body: "bo\x07dy".to_string(),
            },
            TerminalKind::WindowsTerminal,
        )
        .unwrap();
        assert_eq!(seq, "\x1b]9;hi]0;pwned: body\x07");
    }

    #[test]
    fn osc_sequence_for_tab_color_on_windows_terminal_is_osc_4_264() {
        let seq =
            osc_sequence_for(&CtlMessage::SetTabColor { r: 0xff, g: 0x00, b: 0x00 }, TerminalKind::WindowsTerminal)
                .unwrap();
        assert_eq!(seq, "\x1b]4;264;rgb:ff/00/00\x1b\\");
    }

    #[test]
    fn osc_sequence_for_tab_color_on_iterm2_is_osc_6() {
        let seq = osc_sequence_for(&CtlMessage::SetTabColor { r: 0xff, g: 0x88, b: 0x00 }, TerminalKind::ITerm2).unwrap();
        assert_eq!(
            seq,
            "\x1b]6;1;bg;red;brightness;255\x07\x1b]6;1;bg;green;brightness;136\x07\x1b]6;1;bg;blue;brightness;0\x07"
        );
    }

    // `TerminalKind::resolve_from`自体の判定ロジックのテストは`osc-color`クレート
    // (再エクスポート元)に移した——ここに残すのは`osc_sequence_for`(このクレート
    // 自身のCtlMessageディスパッチ)の挙動を検証するテストのみ。

    #[test]
    fn osc_sequence_for_progress_is_osc_9_4() {
        let seq = osc_sequence_for(
            &CtlMessage::SetProgress { state: isekai_protocol::ProgressState::Normal, progress: 42 },
            TerminalKind::WindowsTerminal,
        )
        .unwrap();
        assert_eq!(seq, "\x1b]9;4;1;42\x07");
    }

    #[test]
    fn osc_sequence_for_progress_none_uses_state_zero() {
        let seq =
            osc_sequence_for(
                &CtlMessage::SetProgress { state: isekai_protocol::ProgressState::None, progress: 0 },
                TerminalKind::WindowsTerminal,
            )
            .unwrap();
        assert_eq!(seq, "\x1b]9;4;0;0\x07");
    }

    #[test]
    fn osc_sequence_for_clipboard_push_is_osc_52_and_reuses_data_b64_verbatim() {
        let seq = osc_sequence_for(
            &CtlMessage::ClipboardPush { mime: ClipboardMime::TextPlain, data_b64: "aGVsbG8=".to_string() },
            TerminalKind::WindowsTerminal,
        )
        .unwrap();
        assert_eq!(seq, "\x1b]52;c;aGVsbG8=\x07");
    }

    #[test]
    fn osc_sequence_for_pull_variants_is_none() {
        assert!(osc_sequence_for(&CtlMessage::ClipboardPullRequest {}, TerminalKind::WindowsTerminal).is_none());
        assert!(osc_sequence_for(
            &CtlMessage::ClipboardPullResponse { mime: ClipboardMime::TextPlain, data_b64: "aGVsbG8=".to_string() },
            TerminalKind::WindowsTerminal,
        )
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
        let server = tokio::spawn(async move { handle_ctl_connection(server_stream, "s3cr3t", "mybox").await });

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
        let server = tokio::spawn(async move { handle_ctl_connection(server_stream, "s3cr3t", "mybox").await });

        use tokio::io::AsyncWriteExt as _;
        client.write_all(b"wrong-secret\n{}\n").await.unwrap();
        drop(client);
        let result = server.await.unwrap();
        assert!(result.unwrap_err().to_string().contains("preamble"));
    }

    /// Points `$HOME` at a fresh tempdir and writes `profiles` to
    /// `build_profiles.toml` there — same `HOME_ENV_LOCK`-guarded pattern
    /// `init.rs`'s own `$HOME`-dependent tests use, since `cargo test` runs
    /// tests on multiple threads and `std::env::set_var` is process-global.
    /// Returns the tempdir (kept alive for the caller's whole test) and a
    /// guard that restores the previous `$HOME` on drop.
    #[cfg(unix)]
    fn with_build_profiles(profiles: Vec<crate::build_profile::BuildProfile>) -> (tempfile::TempDir, HomeRestoreGuard) {
        let home = tempfile::tempdir().unwrap();
        let old_home = std::env::var_os("HOME");
        std::env::set_var("HOME", home.path());
        let mut store = crate::build_profile::BuildProfileStore::default();
        for profile in profiles {
            crate::build_profile::upsert_profile(&mut store, profile).unwrap();
        }
        let path = crate::build_profile::default_build_profiles_path().unwrap();
        crate::build_profile::save_build_profiles(&path, &store).unwrap();
        (home, HomeRestoreGuard(old_home))
    }

    #[cfg(unix)]
    struct HomeRestoreGuard(Option<std::ffi::OsString>);

    #[cfg(unix)]
    impl Drop for HomeRestoreGuard {
        fn drop(&mut self) {
            match self.0.take() {
                Some(old) => std::env::set_var("HOME", old),
                None => std::env::remove_var("HOME"),
            }
        }
    }

    /// Reads `CtlMessage` lines off `client` until `BuildFinished`, decoding
    /// every `BuildOutputChunk` along the way and appending its bytes to the
    /// matching stream's buffer. Mirrors what `isekai-pipe ctl build` itself
    /// does in `stream_build` (`isekai-pipe/src/ctl.rs`), just without the
    /// process-exit-code plumbing a test doesn't need.
    #[cfg(unix)]
    async fn collect_build_messages(
        client: impl tokio::io::AsyncRead + Unpin,
    ) -> (Vec<u8>, Vec<u8>, i32, Vec<String>) {
        use tokio::io::{AsyncBufReadExt as _, BufReader};

        let mut reader = BufReader::new(client);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        loop {
            let mut line = String::new();
            let n = reader.read_line(&mut line).await.unwrap();
            assert_ne!(n, 0, "connection closed before BuildFinished");
            match isekai_protocol::decode_ctl_message(line.trim_end_matches('\n').as_bytes()).unwrap() {
                CtlMessage::BuildOutputChunk { stream, data_b64 } => {
                    let decoded = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, data_b64).unwrap();
                    match stream {
                        isekai_protocol::BuildOutputStream::Stdout => stdout.extend_from_slice(&decoded),
                        isekai_protocol::BuildOutputStream::Stderr => stderr.extend_from_slice(&decoded),
                    }
                }
                CtlMessage::BuildFinished { exit_code, result_paths } => {
                    return (stdout, stderr, exit_code, result_paths);
                }
                other => panic!("unexpected message: {other:?}"),
            }
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn run_build_reports_an_unknown_profile() {
        let _guard = crate::HOME_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let (_home, _restore) = with_build_profiles(vec![]);

        let (mut client, server_stream) = tokio::io::duplex(4096);
        let server = tokio::spawn(async move { handle_ctl_connection(server_stream, "s3cr3t", "mybox").await });

        use tokio::io::AsyncWriteExt as _;
        client.write_all(b"s3cr3t\n").await.unwrap();
        let request = serde_json::to_vec(&CtlMessage::BuildRequest { profile: "nope".to_string() }).unwrap();
        client.write_all(&request).await.unwrap();
        client.write_all(b"\n").await.unwrap();

        let (_stdout, stderr, exit_code, result_paths) = collect_build_messages(client).await;
        assert!(String::from_utf8_lossy(&stderr).contains("no build profile registered"));
        assert_eq!(exit_code, 127);
        assert!(result_paths.is_empty());
        server.await.unwrap().unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn run_build_streams_output_and_reports_exit_code_and_results() {
        let _guard = crate::HOME_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let workdir = tempfile::tempdir().unwrap();
        let (_home, _restore) = with_build_profiles(vec![crate::build_profile::BuildProfile {
            host: "mybox".to_string(),
            name: "t".to_string(),
            dir: workdir.path().to_string_lossy().into_owned(),
            command: "printf 'out-line\\n'; printf 'err-line\\n' 1>&2; touch out.bin; exit 5".to_string(),
            result_glob: Some("out.bin".to_string()),
            dest_dir: Some("~/dest".to_string()),
        }]);

        let (mut client, server_stream) = tokio::io::duplex(4096);
        let server = tokio::spawn(async move { handle_ctl_connection(server_stream, "s3cr3t", "mybox").await });

        use tokio::io::AsyncWriteExt as _;
        client.write_all(b"s3cr3t\n").await.unwrap();
        let request = serde_json::to_vec(&CtlMessage::BuildRequest { profile: "t".to_string() }).unwrap();
        client.write_all(&request).await.unwrap();
        client.write_all(b"\n").await.unwrap();

        let (stdout, stderr, exit_code, result_paths) = collect_build_messages(client).await;
        assert_eq!(String::from_utf8_lossy(&stdout), "out-line\n");
        assert_eq!(String::from_utf8_lossy(&stderr), "err-line\n");
        assert_eq!(exit_code, 5);
        assert_eq!(result_paths.len(), 1);
        assert!(result_paths[0].ends_with("out.bin"));
        server.await.unwrap().unwrap();
    }

    /// Guarantees the "every session must have a guaranteed cleanup path"
    /// principle (`.claude/rules/always-connects.md`'s fencing-slot lesson,
    /// applied here to a local child process): the build child keeps
    /// producing output forever (`while true`), so `run_build` can only ever
    /// return if it actually killed the child after the ctl connection broke
    /// — it would hang indefinitely on `child.wait()` otherwise. Wrapping in
    /// `tokio::time::timeout` turns "did it hang forever" into an assertion
    /// rather than a real test hang.
    #[cfg(unix)]
    #[tokio::test]
    async fn run_build_kills_the_child_when_the_connection_breaks_mid_build() {
        let _guard = crate::HOME_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let workdir = tempfile::tempdir().unwrap();
        let (_home, _restore) = with_build_profiles(vec![crate::build_profile::BuildProfile {
            host: "mybox".to_string(),
            name: "infinite".to_string(),
            dir: workdir.path().to_string_lossy().into_owned(),
            command: "while true; do printf x; sleep 0.01; done".to_string(),
            result_glob: None,
            dest_dir: None,
        }]);

        let (mut client, server_stream) = tokio::io::duplex(256);
        let server = tokio::spawn(async move { handle_ctl_connection(server_stream, "s3cr3t", "mybox").await });

        use tokio::io::AsyncWriteExt as _;
        client.write_all(b"s3cr3t\n").await.unwrap();
        let request = serde_json::to_vec(&CtlMessage::BuildRequest { profile: "infinite".to_string() }).unwrap();
        client.write_all(&request).await.unwrap();
        client.write_all(b"\n").await.unwrap();
        // Never read anything back, then disconnect entirely — the small
        // duplex buffer plus this child's continuous output guarantees a
        // write on the server side fails soon after.
        drop(client);

        let result = tokio::time::timeout(std::time::Duration::from_secs(10), server).await;
        let result = result.expect("run_build must not hang after the connection breaks").unwrap();
        let err = result.unwrap_err();
        assert!(format!("{err:#}").contains("closed") || format!("{err:#}").contains("killed"));
    }

    // `build_push_remote_command`/`spawn_result_push`(+ their tests) moved to
    // `build_exec.rs` (Epic P Phase 2) so the Windows-native dispatch paths
    // can share them too — see that module for the equivalent tests.

    /// Connects a fresh `UnixStream` to `local_path` (the listener
    /// `spawn_ctl_listener` bound) and drives one `BuildRequest` over it,
    /// mirroring exactly what a separate `isekai-pipe ctl build <profile>`
    /// process invocation would do.
    #[cfg(unix)]
    async fn connect_and_run_build(
        local_path: &Path,
        secret: &str,
        profile_name: &str,
    ) -> (Vec<u8>, Vec<u8>, i32, Vec<String>) {
        use tokio::io::AsyncWriteExt as _;
        let mut stream = tokio::net::UnixStream::connect(local_path).await.unwrap();
        stream.write_all(format!("{secret}\n").as_bytes()).await.unwrap();
        let request =
            serde_json::to_vec(&CtlMessage::BuildRequest { profile: profile_name.to_string() }).unwrap();
        stream.write_all(&request).await.unwrap();
        stream.write_all(b"\n").await.unwrap();
        collect_build_messages(stream).await
    }

    /// Guards the exact workflow just discussed: a human runs `isekai-ssh
    /// build-profile add` in a *different* terminal while an `isekai-ssh
    /// <host>` session (and its ctl-socket listener) is already running, and
    /// expects the very next `isekai-pipe ctl build <name>` — same session,
    /// no reconnect — to see it. This holds simply because `run_build` reads
    /// `build_profiles.toml` fresh off disk on every `BuildRequest` rather
    /// than caching it anywhere (unlike `CTL_VARS`, which deliberately *is*
    /// an in-memory, per-process store) — this test exists so that property
    /// stays true if this code is ever "optimized" to cache the profile
    /// store, which would silently reintroduce a restart requirement.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_running_ctl_listener_picks_up_build_profile_changes_without_restarting() {
        let _guard = crate::HOME_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let workdir = tempfile::tempdir().unwrap();
        let (_home, _restore) = with_build_profiles(vec![]);

        let mut forward = prepare_ctl_forward(workdir.path()).unwrap();
        spawn_ctl_listener(&mut forward, "mybox".to_string()).await;
        // `spawn_ctl_listener` binds the listener in a background task;
        // give it a moment to actually reach `accept()` before connecting.
        tokio::task::yield_now().await;

        let (_stdout1, stderr1, exit1, _) =
            connect_and_run_build(&forward.local_path, &forward.remote_path, "t").await;
        assert!(String::from_utf8_lossy(&stderr1).contains("no build profile registered"));
        assert_eq!(exit1, 127);

        // Register the profile — exactly what `isekai-ssh build-profile add`
        // does, run here directly rather than via the CLI subcommand, but
        // deliberately *not* touching `forward`/the listener at all, since
        // the whole point is that the already-running session doesn't need
        // to be told about this.
        let path = crate::build_profile::default_build_profiles_path().unwrap();
        let mut store = crate::build_profile::load_build_profiles(&path).unwrap();
        crate::build_profile::upsert_profile(
            &mut store,
            crate::build_profile::BuildProfile {
                host: "mybox".to_string(),
                name: "t".to_string(),
                dir: workdir.path().to_string_lossy().into_owned(),
                command: "printf added".to_string(),
                result_glob: None,
                dest_dir: None,
            },
        )
        .unwrap();
        crate::build_profile::save_build_profiles(&path, &store).unwrap();

        let (stdout2, _stderr2, exit2, _) =
            connect_and_run_build(&forward.local_path, &forward.remote_path, "t").await;
        assert_eq!(String::from_utf8_lossy(&stdout2), "added");
        assert_eq!(exit2, 0);
    }
}
