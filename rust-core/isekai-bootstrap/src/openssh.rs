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

use std::net::SocketAddr;
use std::process::Stdio;

use async_trait::async_trait;
use base64::Engine as _;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

use crate::backend::BootstrapBackend;
use crate::client_candidates::fresh_bootstrap_request_v2;
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
    remote_parent_dir, shell_single_quote, upload_binary_command, validate_relay_jwt, validate_relay_sni,
    validate_remote_path, HANDSHAKE_POLL_ATTEMPTS, HANDSHAKE_POLL_INTERVAL_MS, ISEKAI_PIPE_BIN_NAME,
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
        if out.status != Some(0) {
            return Err(BootstrapError::RemoteCommandFailed {
                command: "uname -m".to_string(),
                status: out.status,
                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            });
        }
        normalize_uname_arch(&String::from_utf8_lossy(&out.stdout))
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

    async fn upload_binary(
        &self,
        target: &HostSpec,
        via: &[JumpSpec],
        binary: &[u8],
        remote_binary_path: &str,
    ) -> Result<(), BootstrapError> {
        let encoded = base64::engine::general_purpose::STANDARD.encode(binary);
        let cmd = upload_binary_command(remote_binary_path, remote_parent_dir(remote_binary_path));
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
        via: &[JumpSpec],
        launch: &LaunchSpec,
        remote_binary_path: &str,
        stun_servers: &[SocketAddr],
    ) -> Result<isekai_protocol::HandshakeJson, BootstrapError> {
        let sleep_secs = HANDSHAKE_POLL_INTERVAL_MS as f64 / 1000.0;

        // `#20a`/`#20b`: every bootstrap operation carries a
        // `BootstrapRequestV2` over this same exec's stdin, alongside
        // whatever launch-specific secret (`relay_jwt`) already travels that
        // way. `client_candidates` is now real: one entry per `stun_servers`
        // entry that actually answered (`collect_client_stun_candidates`).
        // `session_id`/`bootstrap_attempt_id` are freshly random per call —
        // see `isekai_protocol::bootstrap_request`'s module docs for why
        // these are their own identifiers, unrelated to any later ATTACH v2
        // fencing identity the eventual QUIC connection will use.
        let bootstrap_request = fresh_bootstrap_request_v2(stun_servers).await;
        let request_bytes = serde_json::to_vec(&bootstrap_request).expect("BootstrapRequestV2 always serializes");

        // `#20b`: pass the first configured STUN server through to the
        // remote `isekai-pipe serve` too (`LaunchSpec::Direct` only —
        // `isekai-pipe serve` itself rejects `--stun-server`/`--relay`
        // together, since they're alternative transports, `#11`'s own
        // research confirmed this validation already exists), so it reports
        // its *own* `server-reflexive` candidate back in the handshake
        // (completing the other half of the exchange —
        // `client_candidates` above is the client's own address(es), this is
        // the server's). Only one is needed server-side (`isekai-pipe serve
        // --stun-server` has always been single-valued, `#11` deliberately
        // scoped multi-STUN collection to the client side only); the
        // remaining configured servers still contribute to
        // `client_candidates` regardless.
        let stun_server_arg = match stun_servers.first() {
            Some(addr) => format!(" --stun-server {addr}"),
            None => String::new(),
        };

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
                // `#qmux-leg2`: evidence-gated static choice (`ISEKAI_PIPE_DESIGN.md`
                // Epic G/H) — the deployed helper is told once, up front, which
                // transport to use to reach the relay; never a runtime fallback.
                let relay_transport_arg = match relay.relay_transport {
                    crate::types::RelayTransportKind::Udp => String::new(),
                    crate::types::RelayTransportKind::Qmux => " --relay-transport qmux".to_string(),
                };
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
                //
                // `#20a-2`: the `BootstrapRequestV2` JSON travels first on this
                // same stdin, immediately followed by `relay_jwt` — both
                // length-prefixed (the lengths themselves aren't secret, so
                // they're safe to interpolate into the command string) and
                // split with `head -c` (not `dd bs=1`, which reads one byte
                // per syscall and would be needlessly slow for a JSON payload
                // that can run to several KB once real candidates are wired
                // in by `#20b`). Byte counts are verified with `wc -c` before
                // launch so a truncated stdin (e.g. the ssh connection
                // dropping mid-write) fails closed instead of launching
                // `isekai-pipe serve` against a partially-written file.
                let request_len = request_bytes.len();
                let jwt_len = relay.relay_jwt.len();
                let cmd = format!(
                    "umask 077 && tmpdir=$(mktemp -d) && trap 'rm -rf $tmpdir' EXIT && \
                     head -c {request_len} > $tmpdir/bootstrap-request.json && \
                     head -c {jwt_len} > $tmpdir/relay_jwt && \
                     [ \"$(wc -c < $tmpdir/bootstrap-request.json)\" -eq {request_len} ] && \
                     [ \"$(wc -c < $tmpdir/relay_jwt)\" -eq {jwt_len} ] && \
                     ( setsid {remote_binary_path} serve --target 127.0.0.1:22 \
                     --relay {relay_addr} --relay-sni {quoted_sni} --relay-jwt-file $tmpdir/relay_jwt \
                     --bootstrap-request-file $tmpdir/bootstrap-request.json{relay_transport_arg} \
                     --max-idle-lifetime {idle_lifetime_secs} \
                     </dev/null >$tmpdir/handshake 2>$tmpdir/log & ); \
                     for i in $(seq 1 {HANDSHAKE_POLL_ATTEMPTS}); do \
                       [ -s $tmpdir/handshake ] && break; \
                       sleep {sleep_secs}; \
                     done; \
                     cat $tmpdir/handshake"
                );
                let stdin_payload = [request_bytes.as_slice(), relay.relay_jwt.as_bytes()].concat();
                (cmd, Some(stdin_payload))
            }
            // No relay, no STUN: the client dials this host's own SSH
            // bootstrap address at the port reported in `candidates`
            // (`direct-by-bootstrap-host`, `archive/HELPER_PROTOCOL.md` §2).
            // Only the (non-secret-carrying) `BootstrapRequestV2` travels over
            // stdin here — nothing else to deliver out of band.
            LaunchSpec::Direct { idle_lifetime_secs } => {
                let request_len = request_bytes.len();
                let cmd = format!(
                    "umask 077 && tmpdir=$(mktemp -d) && trap 'rm -rf $tmpdir' EXIT && \
                     head -c {request_len} > $tmpdir/bootstrap-request.json && \
                     [ \"$(wc -c < $tmpdir/bootstrap-request.json)\" -eq {request_len} ] && \
                     ( setsid {remote_binary_path} serve --target 127.0.0.1:22 \
                     --bind 0.0.0.0:0 --bootstrap-request-file $tmpdir/bootstrap-request.json{stun_server_arg} \
                     --max-idle-lifetime {idle_lifetime_secs} \
                     </dev/null >$tmpdir/handshake 2>$tmpdir/log & ); \
                     for i in $(seq 1 {HANDSHAKE_POLL_ATTEMPTS}); do \
                       [ -s $tmpdir/handshake ] && break; \
                       sleep {sleep_secs}; \
                     done; \
                     cat $tmpdir/handshake"
                );
                (cmd, Some(request_bytes.clone()))
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
            // `#20a-4`: every launch above sends a `BootstrapRequestV2`, so a
            // compliant `isekai-pipe serve` always echoes back a
            // `BootstrapReportV2` envelope (never a bare `HandshakeJson`) —
            // decode accordingly and unwrap the inner handshake.
            [single] => Ok(isekai_protocol::bootstrap_request::decode_bootstrap_report_v2(single)?.handshake),
            _ => Err(BootstrapError::UnexpectedStdout { extra_lines: non_empty_lines.len() - 1 }),
        }
    }
}

struct SshOutput {
    status: Option<i32>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
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

/// Normalizes `uname -m`'s output to `"x86_64"`/`"aarch64"`, or rejects it —
/// same mapping as `rust-core/src/helper_bootstrap.rs`'s
/// `IsekaiPipeBinaries::select_for` (Android's own remote-bootstrap path),
/// kept identical deliberately rather than reinvented here.
fn normalize_uname_arch(uname_m: &str) -> Result<String, BootstrapError> {
    match uname_m.trim() {
        "x86_64" => Ok("x86_64".to_string()),
        "aarch64" | "arm64" => Ok("aarch64".to_string()),
        other => Err(BootstrapError::UnsupportedArch(other.to_string())),
    }
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

        self.upload_binary(target, via, helper_binary, remote_binary_path).await?;
        let handshake = self
            .launch_and_capture_handshake(target, via, launch, remote_binary_path, stun_servers)
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

    #[test]
    fn normalize_uname_arch_accepts_x86_64() {
        assert_eq!(normalize_uname_arch("x86_64\n").unwrap(), "x86_64");
    }

    #[test]
    fn normalize_uname_arch_accepts_aarch64_and_arm64_aliases() {
        assert_eq!(normalize_uname_arch("aarch64\n").unwrap(), "aarch64");
        assert_eq!(normalize_uname_arch("arm64\n").unwrap(), "aarch64");
    }

    #[test]
    fn normalize_uname_arch_rejects_unknown_architectures() {
        let err = normalize_uname_arch("riscv64\n").unwrap_err();
        assert!(matches!(err, BootstrapError::UnsupportedArch(ref a) if a == "riscv64"));
    }
}
