//! `OpenSshBackend`: the CLI's default `BootstrapBackend`, built on spawning
//! the user's own `ssh(1)` rather than reimplementing SSH client behavior
//! (`ISEKAI_SSH_DESIGN.md` "`--via` の実装方式" — reusing `~/.ssh/config`,
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
//! 2. `launch_and_capture_handshake`: launches `isekai-helper` detached
//!    (`setsid`, stdin from `/dev/null`, wrapped in a subshell so the ssh
//!    exec channel's direct child exits immediately — see the comment in
//!    `helper_bootstrap.rs` for why that matters) and polls a handshake file
//!    until it's non-empty, then `cat`s it back over the same exec channel.
//!
//! **stdout purity is the whole point of this module.** The ssh(1)
//! subprocess's stdout is captured via `Stdio::piped()` and is *never*
//! inherited by this process — see `run_ssh_command`. Anything beyond
//! exactly one non-empty line of handshake JSON on that stdout is treated as
//! untrusted/corrupted output and rejected (`BootstrapError::UnexpectedStdout`),
//! never heuristically parsed. stderr is logged at `debug` level and never
//! mixed into stdout.
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
use crate::types::{BootstrapReport, HostSpec, JumpSpec, RelayLaunchSpec};

/// Mirrors `rust-core/src/helper_bootstrap.rs`'s constants of the same name.
/// `isekai-terminal-core` is built as a `cdylib`/`staticlib` and can't be depended on as
/// an ordinary Rust crate, so these are duplicated rather than shared;
/// unifying them is deferred to the S-0f `isekai-terminal-core` facade cleanup
/// (`ISEKAI_SSH_DESIGN.md` "共有ロジックの crate 分割").
const HELPER_INSTALL_DIR: &str = "~/.local/bin";
const HELPER_BIN_NAME: &str = "isekai-helper";
const HANDSHAKE_DIR: &str = "~/.cache/isekai-terminal";
const HANDSHAKE_FILE: &str = "~/.cache/isekai-terminal/helper.handshake";
const HANDSHAKE_LOG: &str = "~/.cache/isekai-terminal/helper.log";
const HANDSHAKE_POLL_ATTEMPTS: u32 = 50;
const HANDSHAKE_POLL_INTERVAL_MS: u32 = 100;

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
    ) -> Result<(), BootstrapError> {
        let encoded = base64::engine::general_purpose::STANDARD.encode(binary);
        let cmd = format!(
            "umask 077 && mkdir -p {HELPER_INSTALL_DIR} && \
             base64 -d > {HELPER_INSTALL_DIR}/{HELPER_BIN_NAME}.tmp && \
             chmod 0700 {HELPER_INSTALL_DIR}/{HELPER_BIN_NAME}.tmp && \
             mv {HELPER_INSTALL_DIR}/{HELPER_BIN_NAME}.tmp {HELPER_INSTALL_DIR}/{HELPER_BIN_NAME}"
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
        relay: &RelayLaunchSpec,
    ) -> Result<isekai_protocol::HandshakeJson, BootstrapError> {
        let sleep_secs = HANDSHAKE_POLL_INTERVAL_MS as f64 / 1000.0;
        let relay_addr = relay.relay_addr;
        let relay_sni = &relay.relay_sni;
        let relay_jwt = &relay.relay_jwt;
        let idle_lifetime_secs = relay.idle_lifetime_secs;
        let cmd = format!(
            "umask 077 && mkdir -p {HANDSHAKE_DIR} && \
             ( setsid {HELPER_INSTALL_DIR}/{HELPER_BIN_NAME} \
             --relay {relay_addr} --relay-sni {relay_sni} --relay-jwt {relay_jwt} \
             --max-idle-lifetime {idle_lifetime_secs} \
             </dev/null >{HANDSHAKE_FILE} 2>{HANDSHAKE_LOG} & ); \
             for i in $(seq 1 {HANDSHAKE_POLL_ATTEMPTS}); do \
               [ -s {HANDSHAKE_FILE} ] && break; \
               sleep {sleep_secs}; \
             done; \
             cat {HANDSHAKE_FILE}"
        );
        let out = self.run_ssh_command(target, via, &cmd, None).await?;

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

#[async_trait]
impl BootstrapBackend for OpenSshBackend {
    async fn install_and_start(
        &self,
        target: &HostSpec,
        via: Option<&JumpSpec>,
        helper_binary: &[u8],
        relay: &RelayLaunchSpec,
    ) -> Result<BootstrapReport, BootstrapError> {
        self.upload_binary(target, via, helper_binary).await?;
        let handshake = self.launch_and_capture_handshake(target, via, relay).await?;
        Ok(BootstrapReport { handshake })
    }
}
