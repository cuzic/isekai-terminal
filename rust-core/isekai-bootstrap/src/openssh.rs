//! `OpenSshBackend`: the CLI's default `BootstrapBackend`, built on spawning
//! the user's own `ssh(1)` rather than reimplementing SSH client behavior
//! (`archive/ISEKAI_SSH_DESIGN.md` "`--via` の実装方式" — reusing `~/.ssh/config`,
//! `IdentityFile`, `IdentityAgent`, `ProxyJump`, etc. is worth far more than
//! anything a from-scratch client could offer here).
//!
//! Two ssh(1) invocations do the work, mirroring
//! `rust-core/src/helper_bootstrap.rs`'s `upload_binary`/
//! `launch_and_capture_handshake` almost verbatim, just executed as `ssh`
//! subprocesses instead of over a `russh::client::Handle`:
//!
//! 1. `upload_binary`: `base64 -d > ...tmp && chmod 0700 ... && mv ...` with
//!    the base64-encoded binary written to the ssh subprocess's stdin.
//! 2. `launch_and_capture_handshake`: writes `relay_jwt` to a file via this
//!    invocation's own stdin (`cat > $tmpdir/relay_jwt`, never argv — see
//!    below), then launches `isekai-helper` detached (`setsid`, stdin from
//!    `/dev/null`, wrapped in a subshell so the ssh exec channel's direct
//!    child exits immediately — see the comment in `helper_bootstrap.rs` for
//!    why that matters) and polls a handshake file until it's non-empty,
//!    then `cat`s it back over the same exec channel.
//!
//! **stdout purity is the whole point of this module.** The ssh(1)
//! subprocess's stdout is captured via `Stdio::piped()` and is *never*
//! inherited by this process — see `run_ssh_command`. Anything beyond
//! exactly one non-empty line of handshake JSON on that stdout is treated as
//! untrusted/corrupted output and rejected (`BootstrapError::UnexpectedStdout`),
//! never heuristically parsed. stderr is logged at `debug` level and never
//! mixed into stdout.
//!
//! **Hardening (security review #57/#58/#68)**: both the handshake/log
//! output files *and* the `relay_jwt` file live in a fresh `mktemp -d`
//! directory created per invocation (matching `helper_bootstrap.rs`'s
//! Android bootstrap path exactly — no more fixed
//! `~/.cache/isekai-terminal/helper.{handshake,log}` paths shared across
//! invocations). `relay_sni`/`relay_jwt` are validated against a strict
//! allow-list charset and `relay_sni` is additionally shell-quoted before
//! being interpolated into the remote command string; `relay_jwt` itself
//! never touches argv at all (delivered via `--relay-jwt-file`, exactly like
//! `session_secret` already avoided argv/env for the same reason: other
//! local users on the remote host can read another process's argv via `ps
//! aux`/`/proc/<pid>/cmdline`).
//!
//! Host-key verification policy is deliberately **not** touched here:
//! `OpenSshBackend` never adds `-o StrictHostKeyChecking=no` or `-o
//! UserKnownHostsFile=...` — that would silently override the user's own
//! `~/.ssh/config` trust decisions. Tests that need to talk to a throwaway
//! mock server inject those via `with_extra_ssh_args`, which production
//! callers have no reason to use.

use std::process::Stdio;

use async_trait::async_trait;
use base64::Engine as _;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

use crate::backend::BootstrapBackend;
use crate::error::BootstrapError;
use crate::types::{BootstrapReport, HostSpec, JumpSpec, LaunchSpec};

// `ISEKAI_PIPE_INSTALL_DIR`/`ISEKAI_PIPE_BIN_NAME`/`HANDSHAKE_POLL_ATTEMPTS`/
// `HANDSHAKE_POLL_INTERVAL_MS`/`shell_single_quote`/`validate_relay_sni`/
// `validate_relay_jwt` live in `isekai_protocol::bootstrap`, shared with
// `rust-core/src/helper_bootstrap.rs`'s identical constants/helpers (see
// that module's docs for why they must actually be the same literals, not
// just mirrored ones — security review #57/#58 applies to both call sites
// identically).
use isekai_protocol::bootstrap::{
    shell_single_quote, validate_relay_jwt, validate_relay_sni, validate_remote_path,
    HANDSHAKE_POLL_ATTEMPTS, HANDSHAKE_POLL_INTERVAL_MS, ISEKAI_PIPE_BIN_NAME,
    ISEKAI_PIPE_INSTALL_DIR,
};

/// The CLI-default `BootstrapBackend`. Spawns the system `ssh(1)` binary.
pub struct OpenSshBackend {
    ssh_program: String,
    /// Test-only extension point (see module docs): extra arguments spliced
    /// in right after the fixed `-T -o BatchMode=yes -o LogLevel=ERROR`
    /// prefix. Empty in every production code path.
    extra_args: Vec<String>,
}

impl Default for OpenSshBackend {
    fn default() -> Self {
        Self { ssh_program: "ssh".to_string(), extra_args: Vec::new() }
    }
}

impl OpenSshBackend {
    pub fn new() -> Self {
        Self::default()
    }

    /// Overrides the `ssh(1)` program name/path (defaults to `"ssh"`,
    /// resolved via `PATH`). Exposed mainly for tests that pin an exact
    /// binary.
    pub fn with_ssh_program(mut self, program: impl Into<String>) -> Self {
        self.ssh_program = program.into();
        self
    }

    /// Test-only: see the `extra_args` field doc. Production callers should
    /// never need this.
    pub fn with_extra_ssh_args(mut self, args: Vec<String>) -> Self {
        self.extra_args = args;
        self
    }

    fn build_args(&self, target: &HostSpec, via: Option<&JumpSpec>, remote_command: &str) -> Vec<String> {
        let mut args = vec![
            "-T".to_string(),
            "-o".to_string(),
            "BatchMode=yes".to_string(),
            "-o".to_string(),
            "LogLevel=ERROR".to_string(),
        ];
        args.extend(self.extra_args.iter().cloned());
        if let Some(via) = via {
            args.push("-J".to_string());
            args.push(via.to_arg());
        }
        if let Some(port) = target.port {
            args.push("-p".to_string());
            args.push(port.to_string());
        }
        args.push(target.ssh_destination());
        args.push(remote_command.to_string());
        args
    }

    /// Spawns one `ssh(1)` subprocess, optionally feeding `stdin_payload` to
    /// it, and collects (exit code, stdout, stderr) without ever letting the
    /// child's stdout/stderr touch this process's own stdout/stderr
    /// (`Stdio::inherit()` is never used here).
    async fn run_ssh_command(
        &self,
        target: &HostSpec,
        via: Option<&JumpSpec>,
        remote_command: &str,
        stdin_payload: Option<&[u8]>,
    ) -> Result<SshOutput, BootstrapError> {
        let args = self.build_args(target, via, remote_command);

        let mut cmd = Command::new(&self.ssh_program);
        cmd.args(&args);
        cmd.stdin(if stdin_payload.is_some() { Stdio::piped() } else { Stdio::null() });
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        let mut child = cmd.spawn()?;
        let mut child_stdin = child.stdin.take();
        let mut child_stdout = child.stdout.take().expect("stdout was piped");
        let mut child_stderr = child.stderr.take().expect("stderr was piped");

        // Write stdin, read stdout, and read stderr concurrently (not
        // sequentially) so a large payload on one pipe can never deadlock
        // against a full OS pipe buffer on another.
        let stdin_fut = async {
            if let Some(payload) = stdin_payload {
                if let Some(mut stdin) = child_stdin.take() {
                    stdin.write_all(payload).await?;
                    stdin.shutdown().await?;
                }
            }
            Ok::<(), std::io::Error>(())
        };
        let stdout_fut = async {
            let mut buf = Vec::new();
            child_stdout.read_to_end(&mut buf).await?;
            Ok::<Vec<u8>, std::io::Error>(buf)
        };
        let stderr_fut = async {
            let mut buf = Vec::new();
            child_stderr.read_to_end(&mut buf).await?;
            Ok::<Vec<u8>, std::io::Error>(buf)
        };

        let (stdin_res, stdout_res, stderr_res) = tokio::join!(stdin_fut, stdout_fut, stderr_fut);
        stdin_res?;
        let stdout = stdout_res?;
        let stderr = stderr_res?;
        let status = child.wait().await?;

        if !stderr.is_empty() {
            log::debug!("isekai-bootstrap: ssh stderr: {}", String::from_utf8_lossy(&stderr));
        }

        Ok(SshOutput { status: status.code(), stdout, stderr })
    }

    async fn upload_binary(
        &self,
        target: &HostSpec,
        via: Option<&JumpSpec>,
        binary: &[u8],
        remote_binary_path: &str,
    ) -> Result<(), BootstrapError> {
        let encoded = base64::engine::general_purpose::STANDARD.encode(binary);
        let remote_dir = remote_parent_dir(remote_binary_path);
        let cmd = format!(
            "umask 077 && mkdir -p {remote_dir} && \
             base64 -d > {remote_binary_path}.tmp && \
             chmod 0700 {remote_binary_path}.tmp && \
             mv {remote_binary_path}.tmp {remote_binary_path}"
        );
        let out = self.run_ssh_command(target, via, &cmd, Some(encoded.as_bytes())).await?;
        if out.status != Some(0) {
            return Err(BootstrapError::UploadFailed {
                status: out.status,
                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            });
        }
        Ok(())
    }

    async fn launch_and_capture_handshake(
        &self,
        target: &HostSpec,
        via: Option<&JumpSpec>,
        launch: &LaunchSpec,
        remote_binary_path: &str,
    ) -> Result<isekai_protocol::HandshakeJson, BootstrapError> {
        let sleep_secs = HANDSHAKE_POLL_INTERVAL_MS as f64 / 1000.0;

        let (cmd, stdin_payload) = match launch {
            LaunchSpec::Relay(relay) => {
                // Security review #57: validate `relay_sni`/`relay_jwt` against a
                // strict allow-list charset *before* interpolating either into a
                // remote shell command string, in addition to shell-quoting
                // `relay_sni` below (defense in depth — a compromised/misconfigured
                // relay or JWT issuer should not be able to smuggle shell
                // metacharacters into either value).
                validate_relay_sni(&relay.relay_sni)
                    .map_err(|e| BootstrapError::InvalidRelayParam(e.to_string()))?;
                validate_relay_jwt(&relay.relay_jwt)
                    .map_err(|e| BootstrapError::InvalidRelayParam(e.to_string()))?;

                let relay_addr = relay.relay_addr;
                let quoted_sni = shell_single_quote(&relay.relay_sni);
                let idle_lifetime_secs = relay.idle_lifetime_secs;
                // Security review #68: use the same per-invocation `mktemp -d` +
                // `trap ... EXIT` pattern as `rust-core/src/helper_bootstrap.rs`
                // (Android bootstrap path) instead of a fixed shared path. The fixed
                // path (`~/.cache/isekai-terminal/helper.{handshake,log}`) that used
                // to live here had the exact same class of bug that
                // `helper_bootstrap.rs`'s doc comment describes in detail: two
                // overlapping `isekai-ssh init` invocations against the same host
                // would truncate/collide on the same files. `mktemp -d` makes that
                // structurally impossible, matching `archive/HELPER_PROTOCOL.md`'s §2
                // contract.
                //
                // Security review #58: `relay_jwt` (the MASQUE relay bearer token)
                // is written to `$tmpdir/relay_jwt` via this ssh(1) subprocess's
                // stdin rather than embedded in the command line, then passed to
                // isekai-helper as `--relay-jwt-file` — argv would otherwise be
                // readable by any other local user on the remote host via `ps
                // aux`/`/proc/<pid>/cmdline`, exactly like `session_secret` already
                // avoids that path.
                let cmd = format!(
                    "umask 077 && tmpdir=$(mktemp -d) && trap 'rm -rf $tmpdir' EXIT && \
                     cat > $tmpdir/relay_jwt && \
                     ( setsid {remote_binary_path} serve --target 127.0.0.1:22 \
                     --relay {relay_addr} --relay-sni {quoted_sni} --relay-jwt-file $tmpdir/relay_jwt \
                     --max-idle-lifetime {idle_lifetime_secs} \
                     </dev/null >$tmpdir/handshake 2>$tmpdir/log & ); \
                     for i in $(seq 1 {HANDSHAKE_POLL_ATTEMPTS}); do \
                       [ -s $tmpdir/handshake ] && break; \
                       sleep {sleep_secs}; \
                     done; \
                     cat $tmpdir/handshake"
                );
                (cmd, Some(relay.relay_jwt.clone().into_bytes()))
            }
            // No relay, no STUN: the client dials this host's own SSH
            // bootstrap address at the port reported in `candidates`
            // (`direct-by-bootstrap-host`, `archive/HELPER_PROTOCOL.md` §2). No
            // stdin payload needed (nothing secret to deliver out of band).
            LaunchSpec::Direct { idle_lifetime_secs } => {
                let cmd = format!(
                    "umask 077 && tmpdir=$(mktemp -d) && trap 'rm -rf $tmpdir' EXIT && \
                     ( setsid {remote_binary_path} serve --target 127.0.0.1:22 \
                     --bind 0.0.0.0:0 --max-idle-lifetime {idle_lifetime_secs} \
                     </dev/null >$tmpdir/handshake 2>$tmpdir/log & ); \
                     for i in $(seq 1 {HANDSHAKE_POLL_ATTEMPTS}); do \
                       [ -s $tmpdir/handshake ] && break; \
                       sleep {sleep_secs}; \
                     done; \
                     cat $tmpdir/handshake"
                );
                (cmd, None)
            }
        };

        let out = self.run_ssh_command(target, via, &cmd, stdin_payload.as_deref()).await?;

        let non_empty_lines: Vec<&[u8]> =
            out.stdout.split(|&b| b == b'\n').filter(|line| !line.is_empty()).collect();

        match non_empty_lines.as_slice() {
            [] => Err(BootstrapError::HandshakeMissing {
                status: out.status,
                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            }),
            [single] => Ok(isekai_protocol::handshake::decode_handshake_json(single)?),
            _ => Err(BootstrapError::UnexpectedStdout { extra_lines: non_empty_lines.len() - 1 }),
        }
    }
}

struct SshOutput {
    status: Option<i32>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

/// The directory `mkdir -p` should create for `path` (a full remote binary
/// path, e.g. `~/.local/bin/isekai-pipe` -> `~/.local/bin`). Falls back to
/// `.` for a bare filename with no directory component (harmless: `mkdir -p
/// .` always succeeds) and to `/` for a path directly under the filesystem
/// root.
fn remote_parent_dir(path: &str) -> &str {
    match path.rsplit_once('/') {
        Some((dir, _)) if !dir.is_empty() => dir,
        Some(_) => "/",
        None => ".",
    }
}

#[async_trait]
impl BootstrapBackend for OpenSshBackend {
    async fn install_and_start(
        &self,
        target: &HostSpec,
        via: Option<&JumpSpec>,
        helper_binary: &[u8],
        launch: &LaunchSpec,
        remote_binary_path: Option<&str>,
    ) -> Result<BootstrapReport, BootstrapError> {
        let default_path = format!("{ISEKAI_PIPE_INSTALL_DIR}/{ISEKAI_PIPE_BIN_NAME}");
        let remote_binary_path = remote_binary_path.unwrap_or(&default_path);
        validate_remote_path(remote_binary_path)
            .map_err(|e| BootstrapError::InvalidRemotePath(e.to_string()))?;

        self.upload_binary(target, via, helper_binary, remote_binary_path).await?;
        let handshake =
            self.launch_and_capture_handshake(target, via, launch, remote_binary_path).await?;
        Ok(BootstrapReport { handshake })
    }
}
