//! `OpenSshBackend`: the CLI's default `BootstrapBackend`, built on spawning
//! the user's own `ssh(1)` rather than reimplementing SSH client behavior
//! (`archive/ISEKAI_SSH_DESIGN.md` "`--via` の実装方式" — reusing `~/.ssh/config`,
//! `IdentityFile`, `IdentityAgent`, `ProxyJump`, etc. is worth far more than
//! anything a from-scratch client could offer here).
//!
//! A single ssh(1) invocation (`install_and_launch`) does the work,
//! mirroring `rust-core/src/helper_bootstrap.rs`'s `upload_binary`/
//! `launch_and_capture_handshake` in spirit, just executed as one combined
//! `ssh` subprocess script instead of over a `russh::client::Handle`.
//!
//! **What this module contributes is the transport, not the script**: the
//! remote shell script itself, its stdin framing, and the handshake parsing
//! live in `crate::install_script` — shared verbatim with
//! `crate::russh_backend`, which runs the identical script over a `russh`
//! exec channel instead of an `ssh(1)` subprocess (see that module's docs
//! for what the script does step by step, and why it is shared rather than
//! duplicated). This module owns spawning `ssh(1)`, building its argument
//! vector (`-J`/`-p`/destination), and capturing its three streams.
//!
//! **stdout purity is the whole point of this module.** The ssh(1)
//! subprocess's stdout is captured via `Stdio::piped()` and is *never*
//! inherited by this process — see `run_ssh_command`. Anything beyond
//! exactly one non-empty line of handshake JSON on that stdout is treated as
//! untrusted/corrupted output and rejected
//! (`install_script::parse_install_output`'s
//! `BootstrapError::UnexpectedStdout`), never heuristically parsed. stderr is
//! logged at `debug` level and never mixed into stdout.
//!
//! Host-key verification policy is deliberately **not** touched here:
//! `OpenSshBackend` never adds `-o StrictHostKeyChecking=no` or `-o
//! UserKnownHostsFile=...` — that would silently override the user's own
//! `~/.ssh/config` trust decisions. Tests that need to talk to a throwaway
//! mock server inject those via `with_extra_ssh_args`, which production
//! callers have no reason to use.

use std::net::SocketAddr;
use std::process::Stdio;

use async_trait::async_trait;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

use crate::backend::BootstrapBackend;
use crate::error::BootstrapError;
use crate::install_script::{build_install_script, parse_install_output, parse_uname_output, SshOutput};
use crate::types::{BootstrapReport, HostSpec, JumpSpec, LaunchSpec};

use isekai_protocol::bootstrap::{validate_remote_path, ISEKAI_PIPE_BIN_NAME, ISEKAI_PIPE_INSTALL_DIR};

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

    /// Runs `uname -m` on `target` (through `via`, if given) and normalizes
    /// the result to `"x86_64"`/`"aarch64"` — a *separate* `ssh(1)`
    /// round-trip from `install_and_start`'s own upload/launch steps
    /// (matching this module's existing "one ssh(1) invocation per logical
    /// step" shape). Exists so a caller with no explicit `--helper-binary`
    /// can pick which pre-built `isekai-pipe` variant to fetch/upload before
    /// ever calling `install_and_start` — mirrors `rust-core/src/
    /// helper_bootstrap.rs`'s `ensure_helper_running` (Android's own
    /// remote-bootstrap path), which runs the identical `uname -m` probe for
    /// the identical reason.
    pub async fn detect_remote_arch(&self, target: &HostSpec, via: &[JumpSpec]) -> Result<String, BootstrapError> {
        let out = self.run_ssh_command(target, via, "uname -m", None).await?;
        parse_uname_output(&out)
    }

    fn build_args(&self, target: &HostSpec, via: &[JumpSpec], remote_command: &str) -> Vec<String> {
        let mut args = vec![
            "-T".to_string(),
            "-o".to_string(),
            "BatchMode=yes".to_string(),
            "-o".to_string(),
            "LogLevel=ERROR".to_string(),
        ];
        args.extend(self.extra_args.iter().cloned());
        if let Some(via_arg) = join_via_chain(via) {
            args.push("-J".to_string());
            args.push(via_arg);
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
        via: &[JumpSpec],
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

    /// Combined upload-check + reuse-check + (conditional upload) +
    /// (conditional launch) in a single ssh(1) exec, so the whole decision
    /// is made under one held `flock` — see this module's docs and
    /// `crate::reuse`'s module docs for why splitting "decide" from "act"
    /// across separate ssh(1) subprocesses would reopen exactly the race
    /// this exists to close (two concurrent invocations both deciding to
    /// relaunch). The script itself, its stdin payload, and the parsing of
    /// what it prints back all come from `crate::install_script`, shared
    /// verbatim with `crate::russh_backend` — the only thing that is
    /// `OpenSshBackend`-specific here is the `run_ssh_command` transport
    /// call in the middle (and, because that writes one pipe rather than a
    /// sequence of channel messages, flattening the three stdin chunks with
    /// `stdin_payload()`).
    async fn install_and_launch(
        &self,
        target: &HostSpec,
        via: &[JumpSpec],
        launch: &LaunchSpec,
        remote_binary_path: &str,
        stun_servers: &[SocketAddr],
        binary: &[u8],
    ) -> Result<isekai_protocol::HandshakeJson, BootstrapError> {
        let script = build_install_script(launch, remote_binary_path, stun_servers, binary).await?;
        let stdin_payload = script.stdin_payload();
        let out = self.run_ssh_command(target, via, &script.command, Some(&stdin_payload)).await?;
        parse_install_output(&out)
    }
}

/// Builds the value for `ssh(1)`'s `-J` flag from a jump-host chain, per
/// `ISEKAI_PIPE_DESIGN.md` §8 Epic K's executor requirement: `-J` natively
/// accepts a comma-separated list of `[user@]host[:port]` hops and chains
/// through all of them in a single `ssh(1)` invocation, so a multi-hop chain
/// needs no nested `ssh`-inside-`ssh` execution (which would additionally
/// force each intermediate hop to interpret bootstrap payload/credentials it
/// has no business seeing). Returns `None` for an empty chain (0-hop direct
/// connection, no `-J` at all).
fn join_via_chain(via: &[JumpSpec]) -> Option<String> {
    if via.is_empty() {
        return None;
    }
    Some(via.iter().map(JumpSpec::to_arg).collect::<Vec<_>>().join(","))
}

#[async_trait]
impl BootstrapBackend for OpenSshBackend {
    async fn install_and_start(
        &self,
        target: &HostSpec,
        via: &[JumpSpec],
        helper_binary: &[u8],
        launch: &LaunchSpec,
        remote_binary_path: Option<&str>,
        stun_servers: &[SocketAddr],
    ) -> Result<BootstrapReport, BootstrapError> {
        let default_path = format!("{ISEKAI_PIPE_INSTALL_DIR}/{ISEKAI_PIPE_BIN_NAME}");
        let remote_binary_path = remote_binary_path.unwrap_or(&default_path);
        validate_remote_path(remote_binary_path)
            .map_err(|e| BootstrapError::InvalidRemotePath(e.to_string()))?;

        let handshake = self
            .install_and_launch(target, via, launch, remote_binary_path, stun_servers, helper_binary)
            .await?;
        Ok(BootstrapReport { handshake })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn join_via_chain_is_none_for_an_empty_chain() {
        assert_eq!(join_via_chain(&[]), None);
    }

    #[test]
    fn join_via_chain_renders_a_single_hop_unchanged() {
        assert_eq!(join_via_chain(&[JumpSpec::new("bastion")]), Some("bastion".to_string()));
    }

    #[test]
    fn join_via_chain_comma_joins_multiple_hops_in_order() {
        let chain = [
            JumpSpec::new("bastion-a").with_user("alice").with_port(2222),
            JumpSpec::new("bastion-b"),
            JumpSpec::new("bastion-c").with_port(22),
        ];
        assert_eq!(join_via_chain(&chain), Some("alice@bastion-a:2222,bastion-b,bastion-c:22".to_string()));
    }
}
