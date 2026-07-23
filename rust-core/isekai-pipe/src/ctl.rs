//! `isekai-pipe ctl` — the remote-side CLI for the tabごとの title/clipboard
//! control-plane (`ISEKAI_PIPE_DESIGN.md` §8 Epic M). Connects to the
//! per-tab UNIX domain socket forwarded in via `$ISEKAI_CTL_SOCK`
//! (`-R remote-sock:local-sock`, requested by the `isekai-ssh` wrapper) and
//! sends/receives one `isekai_protocol::CtlMessage` per invocation. Never
//! touches the pane's PTY, so tmux's OSC 0/2/52 interception is irrelevant.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use anyhow::Result;
#[cfg(unix)]
use anyhow::{bail, Context};
#[cfg(unix)]
use base64::Engine as _;
use isekai_protocol::{ClipboardMime, VarScope};
#[cfg(unix)]
use isekai_protocol::{decode_ctl_message, CtlMessage};
#[cfg(unix)]
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
#[cfg(unix)]
use tokio::net::UnixStream;

use crate::connect::next_arg;
use crate::{EX_UNAVAILABLE, EX_USAGE};

const ENV_CTL_SOCK: &str = "ISEKAI_CTL_SOCK";

#[derive(Debug, PartialEq, Eq)]
enum CtlLaunch {
    Title { sock: Option<String>, value: String },
    ClipPush { sock: Option<String>, mime: ClipboardMime },
    ClipPull { sock: Option<String> },
    /// task #16: `setvar` — fire-and-forget, same wire shape as `Title`.
    SetVar { sock: Option<String>, scope: VarScope, key: String, value: String },
    /// task #16: `getvar` — request/response, same wire shape as `ClipPull`.
    GetVar { sock: Option<String>, scope: VarScope, key: String },
    /// Epic P: `build` — unlike every other variant above, this keeps the
    /// connection open for the whole build (streamed `BuildOutputChunk`s
    /// before the terminating `BuildFinished`), not a single round trip.
    Build { sock: Option<String>, profile: String },
}

fn print_ctl_help() {
    println!("USAGE:");
    println!("    isekai-pipe ctl title <text> [--sock <path>]");
    println!("    isekai-pipe ctl clip push --mime <text/plain|text/html|image/png> [--sock <path>]");
    println!("        (reads the payload from stdin)");
    println!("    isekai-pipe ctl clip pull [--sock <path>]");
    println!("        (writes the decoded payload to stdout)");
    println!("    isekai-pipe ctl setvar <key> <value> [--scope tab|session|global] [--sock <path>]");
    println!("    isekai-pipe ctl getvar <key> [--scope tab|session|global] [--sock <path>]");
    println!("        (writes the value to stdout with no trailing newline; exits non-zero and");
    println!("        prints nothing if the key was never set)");
    println!("    isekai-pipe ctl file ls|cat|info|cp|rm ...  (see `isekai-pipe ctl file --help`)");
    println!("    isekai-pipe ctl build <profile> [--sock <path>]");
    println!("        (streams the build's stdout/stderr live to this process's own stdout/stderr;");
    println!("        exits with the build's own exit code once it finishes. `<profile>` is a name");
    println!("        registered on the isekai-ssh client side (`isekai-ssh build-profile add`) —");
    println!("        the actual command run is never sent over the wire, see ISEKAI_PIPE_DESIGN.md");
    println!("        §8 Epic P)");
    println!();
    println!("`setvar`/`getvar`/`title`/`clip`/`build` need the tab's ctl-socket forward: without --sock,");
    println!("they read the target UNIX domain socket path from ${ENV_CTL_SOCK}. `file` does not");
    println!("use this socket at all (see `isekai-pipe ctl file --help`).");
    println!();
    println!("NOTE: --scope global's reach depends on which client the tab's ctl-socket forward");
    println!("terminates in. The isekai-terminal Android app is one process serving many tabs, so");
    println!("global is genuinely shared across every tab in that app. The `ssh(1)` CLI wrapper");
    println!("(`isekai-ssh`) is one process *per tab*, so global there is silently isolated to just");
    println!("that one tab/process too — a value set with --scope global from one `isekai-ssh`");
    println!("invocation is NOT visible to another. Don't rely on --scope global for cross-session");
    println!("sharing when the far end is the CLI wrapper.");
}

fn parse_var_scope(value: &str) -> Result<VarScope, String> {
    match value {
        "tab" => Ok(VarScope::Tab),
        "session" => Ok(VarScope::Session),
        "global" => Ok(VarScope::Global),
        other => Err(format!("isekai-pipe ctl: unsupported --scope {other:?} (expected tab, session, or global)")),
    }
}

fn parse_mime(value: &str) -> Result<ClipboardMime, String> {
    match value {
        "text/plain" => Ok(ClipboardMime::TextPlain),
        "text/html" => Ok(ClipboardMime::TextHtml),
        "image/png" => Ok(ClipboardMime::ImagePng),
        other => Err(format!(
            "isekai-pipe ctl clip push: unsupported --mime {other:?} (expected text/plain, text/html, or image/png)"
        )),
    }
}

fn parse_ctl(mut args: impl Iterator<Item = String>) -> Result<Option<CtlLaunch>, ExitCode> {
    match args.next().as_deref() {
        None | Some("-h") | Some("--help") => {
            print_ctl_help();
            Ok(None)
        }
        Some("title") => parse_ctl_title(args),
        Some("clip") => parse_ctl_clip(args),
        Some("setvar") => parse_ctl_setvar(args),
        Some("getvar") => parse_ctl_getvar(args),
        Some("build") => parse_ctl_build(args),
        Some(other) => {
            eprintln!("isekai-pipe ctl: unknown subcommand {other:?}");
            Err(ExitCode::from(EX_USAGE))
        }
    }
}

fn parse_ctl_title(mut args: impl Iterator<Item = String>) -> Result<Option<CtlLaunch>, ExitCode> {
    let mut sock: Option<String> = None;
    let mut value: Option<String> = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--sock" => {
                sock = Some(next_arg("ctl title", &mut args, "--sock").map_err(|e| {
                    eprintln!("{e}");
                    ExitCode::from(EX_USAGE)
                })?);
            }
            other if value.is_none() => value = Some(other.to_string()),
            other => {
                eprintln!("isekai-pipe ctl title: unexpected extra argument {other:?}");
                return Err(ExitCode::from(EX_USAGE));
            }
        }
    }
    let Some(value) = value else {
        eprintln!("isekai-pipe ctl title: a title text argument is required");
        return Err(ExitCode::from(EX_USAGE));
    };
    Ok(Some(CtlLaunch::Title { sock, value }))
}

fn parse_ctl_clip(mut args: impl Iterator<Item = String>) -> Result<Option<CtlLaunch>, ExitCode> {
    match args.next().as_deref() {
        Some("push") => {
            let mut sock: Option<String> = None;
            let mut mime: Option<ClipboardMime> = None;
            while let Some(arg) = args.next() {
                match arg.as_str() {
                    "--sock" => {
                        sock = Some(next_arg("ctl clip push", &mut args, "--sock").map_err(|e| {
                            eprintln!("{e}");
                            ExitCode::from(EX_USAGE)
                        })?);
                    }
                    "--mime" => {
                        let value = next_arg("ctl clip push", &mut args, "--mime").map_err(|e| {
                            eprintln!("{e}");
                            ExitCode::from(EX_USAGE)
                        })?;
                        mime = Some(parse_mime(&value).map_err(|e| {
                            eprintln!("{e}");
                            ExitCode::from(EX_USAGE)
                        })?);
                    }
                    other => {
                        eprintln!("isekai-pipe ctl clip push: unknown argument {other:?}");
                        return Err(ExitCode::from(EX_USAGE));
                    }
                }
            }
            let Some(mime) = mime else {
                eprintln!("isekai-pipe ctl clip push: --mime is required");
                return Err(ExitCode::from(EX_USAGE));
            };
            Ok(Some(CtlLaunch::ClipPush { sock, mime }))
        }
        Some("pull") => {
            let mut sock: Option<String> = None;
            while let Some(arg) = args.next() {
                match arg.as_str() {
                    "--sock" => {
                        sock = Some(next_arg("ctl clip pull", &mut args, "--sock").map_err(|e| {
                            eprintln!("{e}");
                            ExitCode::from(EX_USAGE)
                        })?);
                    }
                    other => {
                        eprintln!("isekai-pipe ctl clip pull: unknown argument {other:?}");
                        return Err(ExitCode::from(EX_USAGE));
                    }
                }
            }
            Ok(Some(CtlLaunch::ClipPull { sock }))
        }
        other => {
            eprintln!("isekai-pipe ctl clip: expected \"push\" or \"pull\", got {other:?}");
            Err(ExitCode::from(EX_USAGE))
        }
    }
}

fn parse_ctl_setvar(mut args: impl Iterator<Item = String>) -> Result<Option<CtlLaunch>, ExitCode> {
    let mut sock: Option<String> = None;
    let mut scope: Option<VarScope> = None;
    let mut key: Option<String> = None;
    let mut value: Option<String> = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--sock" => {
                sock = Some(next_arg("ctl setvar", &mut args, "--sock").map_err(|e| {
                    eprintln!("{e}");
                    ExitCode::from(EX_USAGE)
                })?);
            }
            "--scope" => {
                let raw = next_arg("ctl setvar", &mut args, "--scope").map_err(|e| {
                    eprintln!("{e}");
                    ExitCode::from(EX_USAGE)
                })?;
                scope = Some(parse_var_scope(&raw).map_err(|e| {
                    eprintln!("{e}");
                    ExitCode::from(EX_USAGE)
                })?);
            }
            other if key.is_none() => key = Some(other.to_string()),
            other if value.is_none() => value = Some(other.to_string()),
            other => {
                eprintln!("isekai-pipe ctl setvar: unexpected extra argument {other:?}");
                return Err(ExitCode::from(EX_USAGE));
            }
        }
    }
    let Some(key) = key else {
        eprintln!("isekai-pipe ctl setvar: a key argument is required");
        return Err(ExitCode::from(EX_USAGE));
    };
    let Some(value) = value else {
        eprintln!("isekai-pipe ctl setvar: a value argument is required");
        return Err(ExitCode::from(EX_USAGE));
    };
    Ok(Some(CtlLaunch::SetVar { sock, scope: scope.unwrap_or(VarScope::Tab), key, value }))
}

fn parse_ctl_getvar(mut args: impl Iterator<Item = String>) -> Result<Option<CtlLaunch>, ExitCode> {
    let mut sock: Option<String> = None;
    let mut scope: Option<VarScope> = None;
    let mut key: Option<String> = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--sock" => {
                sock = Some(next_arg("ctl getvar", &mut args, "--sock").map_err(|e| {
                    eprintln!("{e}");
                    ExitCode::from(EX_USAGE)
                })?);
            }
            "--scope" => {
                let raw = next_arg("ctl getvar", &mut args, "--scope").map_err(|e| {
                    eprintln!("{e}");
                    ExitCode::from(EX_USAGE)
                })?;
                scope = Some(parse_var_scope(&raw).map_err(|e| {
                    eprintln!("{e}");
                    ExitCode::from(EX_USAGE)
                })?);
            }
            other if key.is_none() => key = Some(other.to_string()),
            other => {
                eprintln!("isekai-pipe ctl getvar: unexpected extra argument {other:?}");
                return Err(ExitCode::from(EX_USAGE));
            }
        }
    }
    let Some(key) = key else {
        eprintln!("isekai-pipe ctl getvar: a key argument is required");
        return Err(ExitCode::from(EX_USAGE));
    };
    Ok(Some(CtlLaunch::GetVar { sock, scope: scope.unwrap_or(VarScope::Tab), key }))
}

fn parse_ctl_build(mut args: impl Iterator<Item = String>) -> Result<Option<CtlLaunch>, ExitCode> {
    let mut sock: Option<String> = None;
    let mut profile: Option<String> = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--sock" => {
                sock = Some(next_arg("ctl build", &mut args, "--sock").map_err(|e| {
                    eprintln!("{e}");
                    ExitCode::from(EX_USAGE)
                })?);
            }
            other if profile.is_none() => profile = Some(other.to_string()),
            other => {
                eprintln!("isekai-pipe ctl build: unexpected extra argument {other:?}");
                return Err(ExitCode::from(EX_USAGE));
            }
        }
    }
    let Some(profile) = profile else {
        eprintln!("isekai-pipe ctl build: a profile name argument is required");
        return Err(ExitCode::from(EX_USAGE));
    };
    Ok(Some(CtlLaunch::Build { sock, profile }))
}

fn resolve_ctl_socket_path(explicit: Option<String>) -> Result<PathBuf, ExitCode> {
    if let Some(explicit) = explicit {
        return Ok(PathBuf::from(explicit));
    }
    match std::env::var_os(ENV_CTL_SOCK) {
        Some(v) if !v.is_empty() => Ok(PathBuf::from(v)),
        _ => {
            eprintln!(
                "isekai-pipe ctl: no --sock given and ${ENV_CTL_SOCK} is unset or empty"
            );
            Err(ExitCode::from(EX_USAGE))
        }
    }
}

/// The `-R` remote path convention `isekai-ssh`'s `ctl_forward.rs` uses
/// (`/tmp/isekai-pipe-ctl-<128bit hex>.sock`, opt-in `#@isekai ctl-socket
/// yes`, `ISEKAI_PIPE_DESIGN.md` §8 Epic M). `sshd` owns cleaning up the
/// actual streamlocal forward bind on a normal disconnect; this sweep only
/// catches what's left behind by abnormal exits (crash, `kill -9`, a
/// network drop that skipped `ssh -O cancel -R`).
const CTL_SOCKET_REMOTE_PREFIX: &str = "isekai-pipe-ctl-";
const CTL_SOCKET_STALE_THRESHOLD: Duration = Duration::from_secs(24 * 60 * 60);

/// Best-effort, non-fatal: a sweep failure (e.g. `/tmp` unreadable for some
/// reason) should never block the caller (`isekai-pipe serve` startup or a
/// single `isekai-pipe ctl` invocation) from proceeding.
fn sweep_stale_ctl_sockets_in(dir: &Path) {
    match isekai_pipe_core::sweep_stale_sockets(dir, CTL_SOCKET_REMOTE_PREFIX, CTL_SOCKET_STALE_THRESHOLD) {
        Ok(removed) if !removed.is_empty() => {
            log::info!(
                "isekai-pipe: swept {} stale ctl-socket file(s) under {}",
                removed.len(),
                dir.display()
            );
        }
        Ok(_) => {}
        Err(e) => {
            log::warn!("isekai-pipe: failed to sweep stale ctl-socket files under {}: {e}", dir.display());
        }
    }
}

/// Called from both `isekai-pipe serve` startup (`main.rs`) and every
/// `isekai-pipe ctl` invocation (below). The `serve`-startup sweep alone
/// misses one topology entirely: a plain-ssh session (`isekai-ssh`本体が
/// isekai-pipe/QUICを一切経由しない直接SSH接続、`ISEKAI_PIPE_DESIGN.md` §8 Epic M
/// follow-up #3) never runs `isekai-pipe serve` on the remote host at all, so
/// nothing was ever triggering a sweep of its orphaned
/// `/tmp/isekai-pipe-ctl-*.sock` files left behind by a crashed/killed tab.
/// `isekai-pipe ctl` itself, on the other hand, DOES always run on the
/// remote host regardless of topology (it's invoked from the interactive
/// shell's `$PROMPT_COMMAND`/manual call over the ctl-socket forward, which
/// is set up the same way for both topologies) — so hooking the sweep there
/// too closes the gap without adding a new binary or a resident process.
pub(crate) fn sweep_stale_ctl_sockets_on_remote() {
    sweep_stale_ctl_sockets_in(Path::new("/tmp"));
}

/// `isekai-pipe ctl file ls|cat|info|cp|rm` (task #16) operates on the
/// filesystem of whatever host `isekai-pipe ctl` itself runs on (always the
/// remote SSH host in this project's architecture — see `crate::ctl_file`'s
/// module docs) and never touches the ctl-socket-forward channel (or its
/// orphan-socket sweep, below) at all, unlike every other `ctl` subcommand.
/// So it's peeled off before sweep/`--sock`/`$ISEKAI_CTL_SOCK` resolution,
/// none of which apply to it.
#[cfg(unix)]
pub(crate) async fn ctl_command(args: impl Iterator<Item = String>) -> ExitCode {
    ctl_command_with_sweep_dir(args, Path::new("/tmp")).await
}

/// Split out from [`ctl_command`] so tests can point the orphan sweep at a
/// tempdir instead of the real `/tmp` (this process's `#[tokio::test]`s run
/// concurrently with other agents/processes that may legitimately hold their
/// own `/tmp/isekai-pipe-ctl-*.sock` files on this machine).
#[cfg(unix)]
async fn ctl_command_with_sweep_dir(mut args: impl Iterator<Item = String>, sweep_dir: &Path) -> ExitCode {
    let first = args.next();
    if first.as_deref() == Some("file") {
        return crate::ctl_file::file_command(args).await;
    }
    let args = first.into_iter().chain(args);

    let launch = match parse_ctl(args) {
        Ok(Some(launch)) => launch,
        Ok(None) => return ExitCode::SUCCESS,
        Err(code) => return code,
    };
    // See `sweep_stale_ctl_sockets_on_remote`'s doc comment: this is the
    // only sweep trigger a plain-ssh (isekai-pipe非経由) session ever gets on
    // the remote host, so do it unconditionally before touching our own
    // socket rather than only on error/retry paths.
    sweep_stale_ctl_sockets_in(sweep_dir);
    let sock = match &launch {
        CtlLaunch::Title { sock, .. }
        | CtlLaunch::ClipPush { sock, .. }
        | CtlLaunch::ClipPull { sock }
        | CtlLaunch::SetVar { sock, .. }
        | CtlLaunch::GetVar { sock, .. }
        | CtlLaunch::Build { sock, .. } => {
            sock.clone()
        }
    };
    let sock_path = match resolve_ctl_socket_path(sock) {
        Ok(path) => path,
        Err(code) => return code,
    };
    match run_ctl(&sock_path, launch).await {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            // Fire-and-forget by design (`ISEKAI_PIPE_DESIGN.md` Epic M
            // "既知の制限"): a stale/missing socket (e.g. a tmux session
            // attached across a different SSH connection) is logged, not
            // fatal to the caller's shell.
            eprintln!("isekai-pipe ctl: {e:?}");
            ExitCode::from(EX_UNAVAILABLE)
        }
    }
}

/// Non-unix builds: still parse arguments (so `--help`/usage errors behave
/// identically everywhere), but the actual transport — a UNIX domain socket
/// forwarded in via `$ISEKAI_CTL_SOCK` — has no Windows backend as of this
/// writing. Same "opportunistic, silent fallback" policy as
/// `isekai-ssh::ctl_forward` (`CLAUDE.md`): log once, fail this one
/// invocation, don't panic or refuse to build. `file` (task #16) never
/// touches this socket at all, so it's peeled off and dispatched cross-
/// platform before any of the unix-only fallback logic below applies.
#[cfg(not(unix))]
pub(crate) async fn ctl_command(mut args: impl Iterator<Item = String>) -> ExitCode {
    let first = args.next();
    if first.as_deref() == Some("file") {
        return crate::ctl_file::file_command(args).await;
    }
    let args = first.into_iter().chain(args);

    let launch = match parse_ctl(args) {
        Ok(Some(launch)) => launch,
        Ok(None) => return ExitCode::SUCCESS,
        Err(code) => return code,
    };
    // Still resolve `--sock`/`$ISEKAI_CTL_SOCK` so a misconfigured caller
    // sees the same usage error on every platform — only the final "connect
    // to it" step is unix-only.
    let sock = match &launch {
        CtlLaunch::Title { sock, .. }
        | CtlLaunch::ClipPush { sock, .. }
        | CtlLaunch::ClipPull { sock }
        | CtlLaunch::SetVar { sock, .. }
        | CtlLaunch::GetVar { sock, .. }
        | CtlLaunch::Build { sock, .. } => {
            sock.clone()
        }
    };
    if let Err(code) = resolve_ctl_socket_path(sock) {
        return code;
    }
    eprintln!("isekai-pipe ctl: not supported on this platform (requires UNIX domain sockets)");
    ExitCode::from(EX_UNAVAILABLE)
}

#[cfg(unix)]
async fn run_ctl(sock_path: &Path, launch: CtlLaunch) -> Result<u8> {
    match launch {
        CtlLaunch::Title { value, .. } => {
            send_ctl_message(sock_path, CtlMessage::SetTitle { value }).await?;
            Ok(0)
        }
        CtlLaunch::ClipPush { mime, .. } => {
            let mut raw = Vec::new();
            tokio::io::stdin()
                .read_to_end(&mut raw)
                .await
                .context("isekai-pipe ctl clip push: failed to read stdin")?;
            let data_b64 = base64::engine::general_purpose::STANDARD.encode(raw);
            send_ctl_message(sock_path, CtlMessage::ClipboardPush { mime, data_b64 }).await?;
            Ok(0)
        }
        CtlLaunch::ClipPull { .. } => {
            let response = send_ctl_message_and_read_response(sock_path, CtlMessage::ClipboardPullRequest {})
                .await?;
            match response {
                CtlMessage::ClipboardPullResponse { data_b64, .. } => {
                    let decoded = base64::engine::general_purpose::STANDARD
                        .decode(&data_b64)
                        .context("isekai-pipe ctl clip pull: response data_b64 was not valid base64")?;
                    tokio::io::stdout()
                        .write_all(&decoded)
                        .await
                        .context("isekai-pipe ctl clip pull: failed to write stdout")?;
                    Ok(0)
                }
                other => bail!("isekai-pipe ctl clip pull: unexpected response {other:?}"),
            }
        }
        CtlLaunch::SetVar { scope, key, value, .. } => {
            send_ctl_message(sock_path, CtlMessage::SetVar { scope, key, value }).await?;
            Ok(0)
        }
        CtlLaunch::GetVar { scope, key, .. } => {
            let response =
                send_ctl_message_and_read_response(sock_path, CtlMessage::GetVarRequest { scope, key }).await?;
            match response {
                CtlMessage::GetVarResponse { value: Some(value) } => {
                    tokio::io::stdout()
                        .write_all(value.as_bytes())
                        .await
                        .context("isekai-pipe ctl getvar: failed to write stdout")?;
                    Ok(0)
                }
                // Distinct from a comms failure: the peer answered, the key is
                // simply unset. Surfaced as a (non-panicking) error so the exit
                // code is non-zero and scripts can distinguish "unset" from
                // "printed an empty string" without parsing stderr.
                CtlMessage::GetVarResponse { value: None } => bail!("isekai-pipe ctl getvar: key not set"),
                other => bail!("isekai-pipe ctl getvar: unexpected response {other:?}"),
            }
        }
        CtlLaunch::Build { profile, .. } => stream_build(sock_path, profile).await,
    }
}

/// `isekai-pipe ctl build <profile>`: unlike every other launch above, this
/// keeps the connection open for the whole build rather than a single round
/// trip (`ISEKAI_PIPE_DESIGN.md` §8 Epic P deliberately relaxes Epic M's
/// "one `CtlMessage` per connection" convention for this one variant).
/// Streamed `BuildOutputChunk`s are replayed to this process's own
/// stdout/stderr as they arrive — so the remote shell sees the build's
/// output live, in real time, exactly as if it had run locally — until the
/// terminating `BuildFinished`, whose `exit_code` becomes this process's own
/// exit code (so `&&`/`;`-chaining in the remote shell behaves the same way
/// it would for any other command).
#[cfg(unix)]
async fn stream_build(sock_path: &Path, profile: String) -> Result<u8> {
    use isekai_protocol::BuildOutputStream;

    let stream = UnixStream::connect(sock_path)
        .await
        .with_context(|| format!("isekai-pipe ctl: failed to connect to {}", sock_path.display()))?;
    let (read_half, mut write_half) = stream.into_split();
    write_half
        .write_all(&secret_preamble(sock_path))
        .await
        .context("isekai-pipe ctl: failed to write ctl connection preamble")?;
    let mut line =
        serde_json::to_vec(&CtlMessage::BuildRequest { profile }).context("isekai-pipe ctl: failed to encode ctl message")?;
    line.push(b'\n');
    write_half.write_all(&line).await.context("isekai-pipe ctl: failed to write ctl message")?;
    write_half.shutdown().await.ok();

    let mut reader = BufReader::new(read_half);
    loop {
        let mut response_line = String::new();
        let n = reader
            .read_line(&mut response_line)
            .await
            .context("isekai-pipe ctl build: failed to read from the ctl connection")?;
        if n == 0 {
            bail!("isekai-pipe ctl build: connection closed before the build finished");
        }
        match decode_ctl_message(response_line.trim_end_matches('\n').as_bytes())
            .context("isekai-pipe ctl build: malformed message")?
        {
            CtlMessage::BuildOutputChunk { stream, data_b64 } => {
                let decoded = base64::engine::general_purpose::STANDARD
                    .decode(&data_b64)
                    .context("isekai-pipe ctl build: chunk data_b64 was not valid base64")?;
                match stream {
                    BuildOutputStream::Stdout => {
                        let mut stdout = tokio::io::stdout();
                        stdout.write_all(&decoded).await.context("isekai-pipe ctl build: failed to write stdout")?;
                        stdout.flush().await.ok();
                    }
                    BuildOutputStream::Stderr => {
                        let mut stderr = tokio::io::stderr();
                        stderr.write_all(&decoded).await.context("isekai-pipe ctl build: failed to write stderr")?;
                        stderr.flush().await.ok();
                    }
                }
            }
            CtlMessage::BuildFinished { exit_code, result_paths } => {
                if !result_paths.is_empty() {
                    eprintln!(
                        "isekai-pipe ctl build: {} result file(s) will be pushed to the profile's configured dest_dir",
                        result_paths.len()
                    );
                }
                return Ok(exit_code.clamp(0, i32::from(u8::MAX)) as u8);
            }
            other => bail!("isekai-pipe ctl build: unexpected message {other:?}"),
        }
    }
}

/// The preamble line every ctl connection starts with: the remote UNIX
/// socket path itself, which isekai-ssh's `ctl_forward` module already
/// treats as this tab's shared secret (see that module's doc comment for
/// why — in short, a loopback TCP port has no filesystem-permission
/// equivalent to this socket's own `0700` directory, so both platforms'
/// listeners check this preamble uniformly rather than the wire protocol
/// silently differing by platform). `sock_path` is exactly `$ISEKAI_CTL_SOCK`
/// (or `--sock`), i.e. the same value isekai-ssh generated it from.
#[cfg(unix)]
fn secret_preamble(sock_path: &Path) -> Vec<u8> {
    let mut line = sock_path.to_string_lossy().into_owned().into_bytes();
    line.push(b'\n');
    line
}

#[cfg(unix)]
async fn send_ctl_message(sock_path: &Path, msg: CtlMessage) -> Result<()> {
    let mut stream = UnixStream::connect(sock_path)
        .await
        .with_context(|| format!("isekai-pipe ctl: failed to connect to {}", sock_path.display()))?;
    stream
        .write_all(&secret_preamble(sock_path))
        .await
        .context("isekai-pipe ctl: failed to write ctl connection preamble")?;
    let mut line = serde_json::to_vec(&msg).context("isekai-pipe ctl: failed to encode ctl message")?;
    line.push(b'\n');
    stream
        .write_all(&line)
        .await
        .context("isekai-pipe ctl: failed to write ctl message")?;
    // Half-close the write side so a listener reading line-by-line sees a
    // clean EOF after this one message; we never keep a ctl connection open
    // across multiple messages (§8 Epic M "ワイヤーフォーマット").
    stream.shutdown().await.ok();
    Ok(())
}

#[cfg(unix)]
async fn send_ctl_message_and_read_response(sock_path: &Path, msg: CtlMessage) -> Result<CtlMessage> {
    let stream = UnixStream::connect(sock_path)
        .await
        .with_context(|| format!("isekai-pipe ctl: failed to connect to {}", sock_path.display()))?;
    let (read_half, mut write_half) = stream.into_split();
    write_half
        .write_all(&secret_preamble(sock_path))
        .await
        .context("isekai-pipe ctl: failed to write ctl connection preamble")?;
    let mut line = serde_json::to_vec(&msg).context("isekai-pipe ctl: failed to encode ctl message")?;
    line.push(b'\n');
    write_half
        .write_all(&line)
        .await
        .context("isekai-pipe ctl: failed to write ctl message")?;
    write_half.shutdown().await.ok();

    let mut reader = BufReader::new(read_half);
    let mut response_line = String::new();
    reader
        .read_line(&mut response_line)
        .await
        .context("isekai-pipe ctl: failed to read response")?;
    if response_line.is_empty() {
        bail!("isekai-pipe ctl: connection closed before a response was received");
    }
    decode_ctl_message(response_line.trim_end_matches('\n').as_bytes())
        .map_err(anyhow::Error::from)
        .context("isekai-pipe ctl: malformed response")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use tokio::net::UnixListener;

    fn args(strs: &[&str]) -> impl Iterator<Item = String> {
        strs.iter().map(|s| s.to_string()).collect::<Vec<_>>().into_iter()
    }

    #[test]
    fn parses_title() {
        let launch = parse_ctl(args(&["title", "my tab"])).unwrap().unwrap();
        assert_eq!(
            launch,
            CtlLaunch::Title { sock: None, value: "my tab".to_string() }
        );
    }

    #[test]
    fn parses_title_with_explicit_sock() {
        let launch = parse_ctl(args(&["title", "--sock", "/tmp/a.sock", "hi"])).unwrap().unwrap();
        assert_eq!(
            launch,
            CtlLaunch::Title { sock: Some("/tmp/a.sock".to_string()), value: "hi".to_string() }
        );
    }

    #[test]
    fn rejects_title_without_text() {
        let err = parse_ctl(args(&["title"])).unwrap_err();
        assert_eq!(err, ExitCode::from(EX_USAGE));
    }

    #[test]
    fn parses_clip_push() {
        let launch = parse_ctl(args(&["clip", "push", "--mime", "text/plain"])).unwrap().unwrap();
        assert_eq!(launch, CtlLaunch::ClipPush { sock: None, mime: ClipboardMime::TextPlain });
    }

    #[test]
    fn parses_clip_push_image() {
        let launch = parse_ctl(args(&["clip", "push", "--mime", "image/png"])).unwrap().unwrap();
        assert_eq!(launch, CtlLaunch::ClipPush { sock: None, mime: ClipboardMime::ImagePng });
    }

    #[test]
    fn rejects_clip_push_without_mime() {
        let err = parse_ctl(args(&["clip", "push"])).unwrap_err();
        assert_eq!(err, ExitCode::from(EX_USAGE));
    }

    #[test]
    fn rejects_clip_push_with_unknown_mime() {
        let err = parse_ctl(args(&["clip", "push", "--mime", "application/octet-stream"])).unwrap_err();
        assert_eq!(err, ExitCode::from(EX_USAGE));
    }

    #[test]
    fn parses_clip_pull() {
        let launch = parse_ctl(args(&["clip", "pull"])).unwrap().unwrap();
        assert_eq!(launch, CtlLaunch::ClipPull { sock: None });
    }

    #[test]
    fn rejects_unknown_subcommand() {
        let err = parse_ctl(args(&["frobnicate"])).unwrap_err();
        assert_eq!(err, ExitCode::from(EX_USAGE));
    }

    #[test]
    fn parses_setvar_with_default_scope() {
        let launch = parse_ctl(args(&["setvar", "last_build_status", "ok"])).unwrap().unwrap();
        assert_eq!(
            launch,
            CtlLaunch::SetVar {
                sock: None,
                scope: VarScope::Tab,
                key: "last_build_status".to_string(),
                value: "ok".to_string(),
            }
        );
    }

    #[test]
    fn parses_setvar_with_explicit_scope_and_sock() {
        let launch = parse_ctl(args(&[
            "setvar", "--scope", "global", "--sock", "/tmp/a.sock", "k", "v",
        ]))
        .unwrap()
        .unwrap();
        assert_eq!(
            launch,
            CtlLaunch::SetVar {
                sock: Some("/tmp/a.sock".to_string()),
                scope: VarScope::Global,
                key: "k".to_string(),
                value: "v".to_string(),
            }
        );
    }

    #[test]
    fn rejects_setvar_without_value() {
        let err = parse_ctl(args(&["setvar", "key-only"])).unwrap_err();
        assert_eq!(err, ExitCode::from(EX_USAGE));
    }

    #[test]
    fn rejects_setvar_with_unknown_scope() {
        let err = parse_ctl(args(&["setvar", "--scope", "bogus", "k", "v"])).unwrap_err();
        assert_eq!(err, ExitCode::from(EX_USAGE));
    }

    #[test]
    fn parses_getvar_with_default_scope() {
        let launch = parse_ctl(args(&["getvar", "last_build_status"])).unwrap().unwrap();
        assert_eq!(
            launch,
            CtlLaunch::GetVar { sock: None, scope: VarScope::Tab, key: "last_build_status".to_string() }
        );
    }

    #[test]
    fn parses_getvar_with_session_scope() {
        let launch = parse_ctl(args(&["getvar", "--scope", "session", "k"])).unwrap().unwrap();
        assert_eq!(launch, CtlLaunch::GetVar { sock: None, scope: VarScope::Session, key: "k".to_string() });
    }

    #[test]
    fn rejects_getvar_without_key() {
        let err = parse_ctl(args(&["getvar"])).unwrap_err();
        assert_eq!(err, ExitCode::from(EX_USAGE));
    }

    #[test]
    fn parses_build() {
        let launch = parse_ctl(args(&["build", "win"])).unwrap().unwrap();
        assert_eq!(launch, CtlLaunch::Build { sock: None, profile: "win".to_string() });
    }

    #[test]
    fn parses_build_with_explicit_sock() {
        let launch = parse_ctl(args(&["build", "--sock", "/tmp/a.sock", "win"])).unwrap().unwrap();
        assert_eq!(launch, CtlLaunch::Build { sock: Some("/tmp/a.sock".to_string()), profile: "win".to_string() });
    }

    #[test]
    fn rejects_build_without_profile() {
        let err = parse_ctl(args(&["build"])).unwrap_err();
        assert_eq!(err, ExitCode::from(EX_USAGE));
    }

    #[test]
    fn resolves_sock_from_explicit_flag_over_env() {
        // `crate::ENV_LOCK`: `std::env::set_var`/`remove_var` are process-
        // global with no thread-local scoping, and `cargo test` runs
        // `#[test]`s on multiple threads by default — without this, this
        // test's mutation of `$ISEKAI_CTL_SOCK` can race with the other two
        // env-mutating tests below (matches `isekai-ssh`'s `HOME_ENV_LOCK`).
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var(ENV_CTL_SOCK, "/from/env.sock");
        let resolved = resolve_ctl_socket_path(Some("/from/flag.sock".to_string())).unwrap();
        assert_eq!(resolved, PathBuf::from("/from/flag.sock"));
        std::env::remove_var(ENV_CTL_SOCK);
    }

    #[test]
    fn resolves_sock_from_env_when_no_flag() {
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var(ENV_CTL_SOCK, "/from/env.sock");
        let resolved = resolve_ctl_socket_path(None).unwrap();
        assert_eq!(resolved, PathBuf::from("/from/env.sock"));
        std::env::remove_var(ENV_CTL_SOCK);
    }

    #[test]
    fn rejects_missing_sock() {
        let _guard = crate::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var(ENV_CTL_SOCK);
        let err = resolve_ctl_socket_path(None).unwrap_err();
        assert_eq!(err, ExitCode::from(EX_USAGE));
    }

    /// e2e: a minimal hand-written listener (matches this crate's convention
    /// of small hand-rolled mock servers per test file rather than a shared
    /// `tests/common`) that reads one line, asserts it decodes to the
    /// expected `CtlMessage`, and closes.
    #[cfg(unix)]
    #[tokio::test]
    async fn send_ctl_message_delivers_set_title() {
        let dir = tempfile::tempdir().unwrap();
        let sock_path = dir.path().join("ctl.sock");
        let listener = UnixListener::bind(&sock_path).unwrap();

        let server = tokio::spawn({
            let sock_path = sock_path.clone();
            async move {
                let (stream, _) = listener.accept().await.unwrap();
                let mut reader = BufReader::new(stream);
                let mut preamble = String::new();
                reader.read_line(&mut preamble).await.unwrap();
                assert_eq!(preamble.trim_end(), sock_path.to_string_lossy());
                let mut line = String::new();
                reader.read_line(&mut line).await.unwrap();
                let msg = decode_ctl_message(line.trim_end().as_bytes()).unwrap();
                assert_eq!(msg, CtlMessage::SetTitle { value: "hello".to_string() });
            }
        });

        send_ctl_message(&sock_path, CtlMessage::SetTitle { value: "hello".to_string() })
            .await
            .unwrap();
        server.await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn send_ctl_message_and_read_response_round_trips_clip_pull() {
        let dir = tempfile::tempdir().unwrap();
        let sock_path = dir.path().join("ctl.sock");
        let listener = UnixListener::bind(&sock_path).unwrap();

        let server = tokio::spawn({
            let sock_path = sock_path.clone();
            async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read_half, mut write_half) = stream.into_split();
            let mut reader = BufReader::new(read_half);
            let mut preamble = String::new();
            reader.read_line(&mut preamble).await.unwrap();
            assert_eq!(preamble.trim_end(), sock_path.to_string_lossy());
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            let msg = decode_ctl_message(line.trim_end().as_bytes()).unwrap();
            assert_eq!(msg, CtlMessage::ClipboardPullRequest {});

            let response = CtlMessage::ClipboardPullResponse {
                mime: ClipboardMime::TextPlain,
                data_b64: base64::engine::general_purpose::STANDARD.encode("clipboard contents"),
            };
            let mut out = serde_json::to_vec(&response).unwrap();
            out.push(b'\n');
            write_half.write_all(&out).await.unwrap();
            }
        });

        let response = send_ctl_message_and_read_response(&sock_path, CtlMessage::ClipboardPullRequest {})
            .await
            .unwrap();
        assert_eq!(
            response,
            CtlMessage::ClipboardPullResponse {
                mime: ClipboardMime::TextPlain,
                data_b64: base64::engine::general_purpose::STANDARD.encode("clipboard contents"),
            }
        );
        server.await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn send_ctl_message_fails_cleanly_when_socket_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let sock_path = dir.path().join("does-not-exist.sock");
        let err = send_ctl_message(&sock_path, CtlMessage::SetTitle { value: "x".to_string() })
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("failed to connect"));
    }

    #[test]
    fn sweep_stale_ctl_sockets_uses_the_isekai_ssh_naming_convention() {
        // Regression guard: this prefix/threshold must stay in lockstep with
        // isekai-ssh's `ctl_forward.rs` and the Android tmux-detour channel's
        // `ctl_streamlocal::new_ctl_socket_path` (`/tmp/isekai-pipe-ctl-<hex>.sock`),
        // since all three write into the same directory.
        assert_eq!(CTL_SOCKET_REMOTE_PREFIX, "isekai-pipe-ctl-");
        assert_eq!(CTL_SOCKET_STALE_THRESHOLD, Duration::from_secs(24 * 60 * 60));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn sweep_stale_ctl_sockets_in_removes_an_abandoned_matching_socket() {
        let dir = tempfile::tempdir().unwrap();
        let abandoned = dir.path().join("isekai-pipe-ctl-abandoned.sock");
        {
            // Bind and immediately drop: nobody `listen()`s anymore, exactly
            // like a crashed/killed tab that skipped `ssh -O cancel -R`.
            let _listener = UnixListener::bind(&abandoned).unwrap();
        }
        assert!(abandoned.exists());

        sweep_stale_ctl_sockets_in(dir.path());

        assert!(!abandoned.exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn sweep_stale_ctl_sockets_in_leaves_an_unrelated_prefix_alone() {
        let dir = tempfile::tempdir().unwrap();
        let unrelated = dir.path().join("some-other-app-abandoned.sock");
        {
            let _listener = UnixListener::bind(&unrelated).unwrap();
        }
        std::fs::remove_file(&unrelated).ok(); // still "abandoned" without a listener
        std::fs::write(&unrelated, b"not a socket, just a leftover file").unwrap();

        sweep_stale_ctl_sockets_in(dir.path());

        assert!(unrelated.exists(), "must not touch files outside the isekai-pipe-ctl- prefix");
    }

    /// Wiring test for the actual gap this change closes: a plain-ssh
    /// (isekai-pipe非経由) session never runs `isekai-pipe serve` on the
    /// remote host, so `isekai-pipe ctl`'s own invocation is the only place
    /// left that can ever sweep its orphaned ctl sockets. This exercises
    /// `ctl_command_with_sweep_dir` end to end (not just the sweep helper in
    /// isolation) to prove the sweep actually runs before `run_ctl` attempts
    /// to connect, using a tempdir rather than real `/tmp` so it can't
    /// collide with another agent/process's live ctl sockets on this
    /// sandbox.
    #[cfg(unix)]
    #[tokio::test]
    async fn ctl_command_sweeps_stale_ctl_sockets_before_connecting() {
        let dir = tempfile::tempdir().unwrap();
        let abandoned = dir.path().join("isekai-pipe-ctl-abandoned.sock");
        {
            let _listener = UnixListener::bind(&abandoned).unwrap();
        }
        assert!(abandoned.exists());

        // Point `--sock` at a socket that doesn't exist so the connect step
        // itself fails (EX_UNAVAILABLE) — the sweep must still have run
        // first, independent of whether this particular invocation succeeds.
        let missing_sock = dir.path().join("isekai-pipe-ctl-not-here.sock");
        let code = ctl_command_with_sweep_dir(
            args(&["title", "--sock", missing_sock.to_str().unwrap(), "hi"]),
            dir.path(),
        )
        .await;

        assert_eq!(code, ExitCode::from(EX_UNAVAILABLE));
        assert!(!abandoned.exists(), "ctl_command should have swept the abandoned socket before connecting");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn send_ctl_message_delivers_setvar() {
        let dir = tempfile::tempdir().unwrap();
        let sock_path = dir.path().join("ctl.sock");
        let listener = UnixListener::bind(&sock_path).unwrap();

        let server = tokio::spawn({
            let sock_path = sock_path.clone();
            async move {
                let (stream, _) = listener.accept().await.unwrap();
                let mut reader = BufReader::new(stream);
                let mut preamble = String::new();
                reader.read_line(&mut preamble).await.unwrap();
                assert_eq!(preamble.trim_end(), sock_path.to_string_lossy());
                let mut line = String::new();
                reader.read_line(&mut line).await.unwrap();
                let msg = decode_ctl_message(line.trim_end().as_bytes()).unwrap();
                assert_eq!(
                    msg,
                    CtlMessage::SetVar {
                        scope: VarScope::Global,
                        key: "last_build_status".to_string(),
                        value: "ok".to_string(),
                    }
                );
            }
        });

        send_ctl_message(
            &sock_path,
            CtlMessage::SetVar {
                scope: VarScope::Global,
                key: "last_build_status".to_string(),
                value: "ok".to_string(),
            },
        )
        .await
        .unwrap();
        server.await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn send_ctl_message_and_read_response_round_trips_getvar() {
        let dir = tempfile::tempdir().unwrap();
        let sock_path = dir.path().join("ctl.sock");
        let listener = UnixListener::bind(&sock_path).unwrap();

        let server = tokio::spawn({
            let sock_path = sock_path.clone();
            async move {
                let (stream, _) = listener.accept().await.unwrap();
                let (read_half, mut write_half) = stream.into_split();
                let mut reader = BufReader::new(read_half);
                let mut preamble = String::new();
                reader.read_line(&mut preamble).await.unwrap();
                assert_eq!(preamble.trim_end(), sock_path.to_string_lossy());
                let mut line = String::new();
                reader.read_line(&mut line).await.unwrap();
                let msg = decode_ctl_message(line.trim_end().as_bytes()).unwrap();
                assert_eq!(msg, CtlMessage::GetVarRequest { scope: VarScope::Tab, key: "k".to_string() });

                let response = CtlMessage::GetVarResponse { value: Some("v".to_string()) };
                let mut out = serde_json::to_vec(&response).unwrap();
                out.push(b'\n');
                write_half.write_all(&out).await.unwrap();
            }
        });

        let response = send_ctl_message_and_read_response(
            &sock_path,
            CtlMessage::GetVarRequest { scope: VarScope::Tab, key: "k".to_string() },
        )
        .await
        .unwrap();
        assert_eq!(response, CtlMessage::GetVarResponse { value: Some("v".to_string()) });
        server.await.unwrap();
    }

    /// End-to-end through `run_ctl` itself (not just the lower-level
    /// `send_ctl_message*` helpers above): exercises `CtlLaunch::GetVar` →
    /// stdout for both the "value present" and "key unset" cases, matching
    /// `isekai-pipe ctl getvar`'s actual documented contract (value on
    /// stdout with no trailing newline; non-zero exit and nothing printed
    /// when unset).
    #[cfg(unix)]
    #[tokio::test]
    async fn run_ctl_getvar_writes_the_value_to_stdout_with_no_trailing_newline() {
        let dir = tempfile::tempdir().unwrap();
        let sock_path = dir.path().join("ctl.sock");
        let listener = UnixListener::bind(&sock_path).unwrap();

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read_half, mut write_half) = stream.into_split();
            let mut reader = BufReader::new(read_half);
            let mut preamble = String::new();
            reader.read_line(&mut preamble).await.unwrap();
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            let response = CtlMessage::GetVarResponse { value: Some("build-42".to_string()) };
            let mut out = serde_json::to_vec(&response).unwrap();
            out.push(b'\n');
            write_half.write_all(&out).await.unwrap();
        });

        let launch = CtlLaunch::GetVar { sock: None, scope: VarScope::Tab, key: "last_build_status".to_string() };
        run_ctl(&sock_path, launch).await.unwrap();
        server.await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn run_ctl_getvar_errors_when_the_key_is_unset() {
        let dir = tempfile::tempdir().unwrap();
        let sock_path = dir.path().join("ctl.sock");
        let listener = UnixListener::bind(&sock_path).unwrap();

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read_half, mut write_half) = stream.into_split();
            let mut reader = BufReader::new(read_half);
            let mut preamble = String::new();
            reader.read_line(&mut preamble).await.unwrap();
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            let response = CtlMessage::GetVarResponse { value: None };
            let mut out = serde_json::to_vec(&response).unwrap();
            out.push(b'\n');
            write_half.write_all(&out).await.unwrap();
        });

        let launch = CtlLaunch::GetVar { sock: None, scope: VarScope::Tab, key: "unset-key".to_string() };
        let err = run_ctl(&sock_path, launch).await.unwrap_err();
        assert!(format!("{err:#}").contains("key not set"));
        server.await.unwrap();
    }

    /// e2e for `stream_build`'s one deliberate departure from every other
    /// launch in this file: the server sends *two* `BuildOutputChunk`s
    /// before the terminating `BuildFinished`, and `run_ctl` must keep
    /// reading (rather than returning after the first line, the way every
    /// other `send_ctl_message_and_read_response` caller does) until it
    /// sees that terminator, then surface its `exit_code` as its own return
    /// value.
    #[cfg(unix)]
    #[tokio::test]
    async fn run_ctl_build_streams_output_chunks_then_returns_the_exit_code() {
        let dir = tempfile::tempdir().unwrap();
        let sock_path = dir.path().join("ctl.sock");
        let listener = UnixListener::bind(&sock_path).unwrap();

        let server = tokio::spawn({
            let sock_path = sock_path.clone();
            async move {
                let (stream, _) = listener.accept().await.unwrap();
                let (read_half, mut write_half) = stream.into_split();
                let mut reader = BufReader::new(read_half);
                let mut preamble = String::new();
                reader.read_line(&mut preamble).await.unwrap();
                assert_eq!(preamble.trim_end(), sock_path.to_string_lossy());
                let mut line = String::new();
                reader.read_line(&mut line).await.unwrap();
                let msg = decode_ctl_message(line.trim_end().as_bytes()).unwrap();
                assert_eq!(msg, CtlMessage::BuildRequest { profile: "win".to_string() });

                for (stream, text) in [
                    (isekai_protocol::BuildOutputStream::Stdout, "compiling...\n"),
                    (isekai_protocol::BuildOutputStream::Stderr, "warning: unused import\n"),
                ] {
                    let chunk = CtlMessage::BuildOutputChunk {
                        stream,
                        data_b64: base64::engine::general_purpose::STANDARD.encode(text),
                    };
                    let mut out = serde_json::to_vec(&chunk).unwrap();
                    out.push(b'\n');
                    write_half.write_all(&out).await.unwrap();
                }

                let finished = CtlMessage::BuildFinished {
                    exit_code: 3,
                    result_paths: vec!["target/release/app.exe".to_string()],
                };
                let mut out = serde_json::to_vec(&finished).unwrap();
                out.push(b'\n');
                write_half.write_all(&out).await.unwrap();
            }
        });

        let launch = CtlLaunch::Build { sock: None, profile: "win".to_string() };
        let exit_code = run_ctl(&sock_path, launch).await.unwrap();
        assert_eq!(exit_code, 3);
        server.await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn run_ctl_build_fails_when_the_connection_closes_before_build_finished() {
        let dir = tempfile::tempdir().unwrap();
        let sock_path = dir.path().join("ctl.sock");
        let listener = UnixListener::bind(&sock_path).unwrap();

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read_half, write_half) = stream.into_split();
            let mut reader = BufReader::new(read_half);
            let mut preamble = String::new();
            reader.read_line(&mut preamble).await.unwrap();
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            // Drop without ever sending `BuildFinished` — simulates the
            // remote killing `isekai-pipe ctl build` (e.g. Ctrl-C) mid-build.
            drop(write_half);
        });

        let launch = CtlLaunch::Build { sock: None, profile: "win".to_string() };
        let err = run_ctl(&sock_path, launch).await.unwrap_err();
        assert!(format!("{err:#}").contains("connection closed"));
        server.await.unwrap();
    }
}
