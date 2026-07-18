//! `RusshBackend`: a [`BootstrapBackend`] implementation built on `russh`
//! (via `russh-stream-session`, M0) instead of shelling out to a real
//! `ssh(1)` binary — for platforms where `ssh(1)` isn't available (Windows
//! without Win32-OpenSSH, `fancy-humming-pnueli.md` M3).
//!
//! **Deliberately duplicates, rather than shares, most of `openssh.rs`'s
//! logic** (the embedded remote shell script, the `SshOutput` shape, the
//! host-key/credential plumbing): `OpenSshBackend` is already
//! security-reviewed production code (review markers #57/#58/#68 throughout
//! `openssh.rs`) backing every real deployment's SSH bootstrap today.
//! Extracting a shared abstraction out of it risks introducing a subtle
//! divergence or regression in that already-hardened path for the sake of
//! this new, Windows-only backend — not a trade worth making. This mirrors
//! the project's existing `tests/*_e2e.rs` self-containment convention, just
//! applied to production code instead of test code (a deliberate, explicit
//! decision confirmed with the user before starting this module).
//!
//! **Scope of this first cut**:
//! - 0-hop (direct) and single-hop `via` chains only —
//!   `russh_stream_session::connect_via_jump_or_direct`'s `JumpHost` is
//!   itself single-hop (matching `ssh_handler.rs`'s own `JumpConfig`, per
//!   the plan's M2 note). A `via` chain of 2+ hops returns
//!   [`BootstrapError::UnsupportedViaChain`] rather than silently connecting
//!   through only the first hop.
//! - Authentication is private-key only — `HostSpec`/`JumpSpec` carry no
//!   credential material at all (real `ssh(1)` resolves that per-hop from
//!   `~/.ssh/config`/agent internally, invisibly to this crate), so
//!   `RusshBackend` resolves `~/.ssh/config` (via the `openssh-config`
//!   crate) and a private key file for *each* hop itself, the same way
//!   `isekai-ssh`'s own native connect path does. SSH agent support is a
//!   documented follow-up — `isekai-ssh::native::agent_auth` already proves
//!   the pattern; wiring it in here is mechanical, just deferred to keep
//!   this first commit reviewable.
//! - Host-key verification uses the *same* `isekai-trust`
//!   `SshHostKeyTrustStore` (`known_ssh_hosts.toml`) `isekai-ssh`'s own
//!   native connect path already reads/writes, so a host trusted via
//!   `isekai-ssh init` (this module) is already trusted for the regular
//!   `isekai-ssh <host>` connect path afterward, and vice versa.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use base64::Engine as _;
use isekai_protocol::bootstrap::{
    remote_parent_dir, shell_single_quote, validate_log_level, validate_relay_jwt, validate_relay_sni,
    validate_remote_path, HANDSHAKE_POLL_ATTEMPTS, HANDSHAKE_POLL_INTERVAL_MS, ISEKAI_PIPE_BIN_NAME,
    ISEKAI_PIPE_INSTALL_DIR,
};
use isekai_trust::FileBackedHostKeyVerifier;
use russh::client;
use russh_stream_session::{
    authenticate_session, connect_via_jump_or_direct, open_channel, verifying_handler, ConnectionLeg, Credential,
    JumpHost, Session, SessionKind, VerifyingHandler,
};

use crate::backend::BootstrapBackend;
use crate::error::BootstrapError;
use crate::reuse::{launch_fingerprint, lock_file_path, pid_file_path, state_file_path};
use crate::types::{BootstrapReport, HostSpec, JumpSpec, LaunchSpec};

/// Emitted by the install script in place of a handshake line when the
/// upload chain (`base64 -d && chmod && mv`) itself fails — same contract
/// as `openssh.rs`'s identically-named constant (duplicated, not shared;
/// see this module's docs).
const UPLOAD_FAILED_MARKER: &str = "ISEKAI_UPLOAD_FAILED";

/// `SSH_EXTENDED_DATA_STDERR`, per RFC 4254 §5.2 — the only `ext` value
/// `ChannelMsg::ExtendedData` carries in practice for an `exec` channel.
const SSH_EXTENDED_DATA_STDERR: u32 = 1;

/// The `RusshBackend` `BootstrapBackend` implementation.
pub struct RusshBackend {
    store_path: PathBuf,
    /// Called only for a host key never seen before (see
    /// `FileBackedHostKeyVerifier`'s docs below) — defaults to a real
    /// blocking stdin prompt; tests inject a fixed answer.
    confirm_new_host: Arc<dyn Fn(&str) -> bool + Send + Sync>,
    /// Test-only: see `with_identity_file`/`with_identity_files`' docs.
    /// `None` in every production code path. When `Some`, its entries replace
    /// the per-hop `~/.ssh/config` `IdentityFile`/default-probe candidate
    /// list — so a test can pin one key, or several (to exercise the
    /// try-each-candidate auth loop).
    identity_file_override: Option<Vec<PathBuf>>,
}

impl RusshBackend {
    /// Production constructor: the real SSH host-key trust store path
    /// (`isekai_trust::default_ssh_host_key_trust_store_path`) and a real
    /// interactive stdin confirmation prompt for unknown hosts.
    pub fn new() -> Result<Self, BootstrapError> {
        let store_path = isekai_trust::default_ssh_host_key_trust_store_path()
            .map_err(|e| BootstrapError::TrustStorePath(e.to_string()))?;
        Ok(Self { store_path, confirm_new_host: Arc::new(prompt_new_host_confirmation), identity_file_override: None })
    }

    /// Test-only: pins the trust store to a throwaway path instead of the
    /// real one `default_ssh_host_key_trust_store_path` resolves.
    #[doc(hidden)]
    pub fn with_store_path(mut self, path: PathBuf) -> Self {
        self.store_path = path;
        self
    }

    /// Test-only: replaces the interactive stdin prompt with a fixed
    /// answer, so tests never block on real stdin.
    #[doc(hidden)]
    pub fn with_confirm_new_host(mut self, f: Arc<dyn Fn(&str) -> bool + Send + Sync>) -> Self {
        self.confirm_new_host = f;
        self
    }

    /// Test-only: forces every hop (target and jump, if any) to
    /// authenticate with this exact private key file instead of resolving
    /// `~/.ssh/config`'s `IdentityFile`/the default probe order — the
    /// `RusshBackend` equivalent of `OpenSshBackend::with_extra_ssh_args`'
    /// `-o IdentityFile=...`/`-o IdentitiesOnly=yes` test injection.
    /// Production callers have no reason to use this: real deployments
    /// resolve identity per-hop from that hop's own `~/.ssh/config`, same as
    /// `ssh(1)` itself would.
    #[doc(hidden)]
    pub fn with_identity_file(mut self, path: PathBuf) -> Self {
        self.identity_file_override = Some(vec![path]);
        self
    }

    /// Test-only: like [`with_identity_file`](Self::with_identity_file) but
    /// pins an ordered *list* of candidate key files, so a test can verify
    /// the target-hop auth loop tries each in turn (falls through a
    /// rejected/unparseable earlier key to a later, accepted one).
    #[doc(hidden)]
    pub fn with_identity_files(mut self, paths: Vec<PathBuf>) -> Self {
        self.identity_file_override = Some(paths);
        self
    }

    /// Runs `uname -m` on `target` (through `via`, if given) and normalizes
    /// the result to `"x86_64"`/`"aarch64"` — the `RusshBackend` equivalent
    /// of `OpenSshBackend::detect_remote_arch`, same purpose (letting a
    /// caller with no explicit `--helper-binary` pick which pre-built
    /// `isekai-pipe` variant to fetch before ever calling
    /// `install_and_start`).
    ///
    /// Known redundancy (deferred follow-up, Codex review finding 5): on the
    /// auto-download path, `helper_download::resolve_helper_binary` calls this
    /// (connection #1) and then, *after downloading the matching binary from
    /// GitHub*, the caller (`wrapper::bootstrap_and_register`/`init::run`)
    /// calls `install_and_start` (connection #2) — two full TCP+KEX+user-auth
    /// (+jump-tunnel) round-trips against the same target for one bootstrap.
    /// This was left as-is deliberately, not overlooked, for three reasons.
    /// First, it is not a `RusshBackend` defect but the shape of the whole
    /// detect-arch → download → install sequence: the already-hardened
    /// `OpenSshBackend` does exactly the same thing (two independent `ssh`
    /// subprocesses, `openssh.rs`'s `detect_remote_arch` vs.
    /// `install_and_launch`), so collapsing it in `RusshBackend` alone would
    /// make the two backends' connection models diverge. Second, the two
    /// connections straddle a potentially long GitHub asset download; caching
    /// a live authenticated session across that window (behind a `Mutex` in
    /// `self`) would turn a stateless backend into one holding a connection
    /// that can silently die mid-download, needing a liveness-check +
    /// reconnect fallback anyway — extra state and a new failure mode for
    /// modest gain. Third, the gain really is modest: no *double* host-key
    /// TOFU prompt occurs (connection #1 persists trust, so connection #2
    /// sees a known host and never prompts), so the only saving is one extra
    /// handshake.
    /// The clean fix — download first, then a single connection that does both
    /// `uname -m` and the install — belongs in `resolve_helper_binary` and its
    /// callers (`wrapper.rs`/`init.rs`), and would fix both backends at once;
    /// it is out of scope for this `russh_backend.rs`-local change set.
    pub async fn detect_remote_arch(&self, target: &HostSpec, via: &[JumpSpec]) -> Result<String, BootstrapError> {
        let session = self.connect_and_authenticate(target, via).await?;
        let out = run_russh_command(&session.handle, "uname -m", &[]).await?;
        if out.status != Some(0) {
            return Err(BootstrapError::RemoteCommandFailed {
                command: "uname -m".to_string(),
                status: out.status,
                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            });
        }
        normalize_uname_arch(&String::from_utf8_lossy(&out.stdout))
    }

    /// Connects to `target`, through `via` if given (single hop only — see
    /// module docs), authenticating both the jump host (if any) and the
    /// target with a private key resolved per-hop from that hop's own
    /// `~/.ssh/config`.
    ///
    /// Returns the whole `Session` (not just its `handle`) — Codex review
    /// finding: `Session::_jump_handle` is what keeps a single-hop `via`
    /// connection's underlying `direct-tcpip` tunnel alive
    /// (`russh_stream_session::Session`'s own docs: "dropping `Session`
    /// tears down the tunnel too"). Returning only `.handle` would drop
    /// `_jump_handle` the moment this function returns, closing the tunnel
    /// out from under every subsequent command on a via-chain bootstrap —
    /// the caller must keep the returned `Session` alive for as long as
    /// `.handle` is in use.
    async fn connect_and_authenticate(
        &self,
        target: &HostSpec,
        via: &[JumpSpec],
    ) -> Result<Session<VerifyingHandler<FileBackedHostKeyVerifier>>, BootstrapError> {
        if via.len() > 1 {
            return Err(BootstrapError::UnsupportedViaChain { hops: via.len() });
        }

        let target_resolved =
            resolve_hop(&target.host, target.user.as_deref(), target.port, self.identity_file_override.as_deref()).await?;
        let target_host_port = format!("{}:{}", target_resolved.hostname, target_resolved.port);
        let target_verifier = Arc::new(FileBackedHostKeyVerifier::new(
            self.store_path.clone(),
            target_host_port,
            self.confirm_new_host.clone(),
            "isekai-bootstrap",
        ));

        let jump = match via.first() {
            Some(spec) => {
                let jump_resolved =
                    resolve_hop(&spec.host, spec.user.as_deref(), spec.port, self.identity_file_override.as_deref()).await?;
                let jump_host_port = format!("{}:{}", jump_resolved.hostname, jump_resolved.port);
                let jump_verifier = Arc::new(FileBackedHostKeyVerifier::new(
                    self.store_path.clone(),
                    jump_host_port,
                    self.confirm_new_host.clone(),
                    "isekai-bootstrap",
                ));
                // The jump hop still authenticates with only the *first*
                // readable identity: `connect_via_jump_or_direct`/`JumpHost`
                // take a single `Credential` and authenticate it internally,
                // so trying each jump candidate in turn (the way the target
                // hop below now does) would need a `russh-stream-session` API
                // change to accept and iterate a credential list — a
                // documented follow-up, kept out of this `russh_backend.rs`-
                // local change. `resolve_hop` guarantees a non-empty list
                // (it errors otherwise), so `next()` is always `Some`.
                let ResolvedHop { hostname, port, username, credentials } = jump_resolved;
                let jump_credential = credentials
                    .into_iter()
                    .next()
                    .expect("resolve_hop returns a non-empty credential list or errors before this point");
                Some((
                    JumpHost { host: hostname, port, username, credential: jump_credential },
                    jump_verifier,
                ))
            }
            None => None,
        };

        // `connect_via_jump_or_direct` tells us explicitly which leg it's
        // building a handler for, so we pick the matching per-host verifier
        // from the `ConnectionLeg` value rather than counting calls — a
        // future change to that function's internal connection sequence
        // (a retry, a probe) can't silently pair a host with the wrong
        // trust-store entry.
        let jump_verifier_for_closure = jump.as_ref().map(|(_, v)| v.clone());
        let target_verifier_for_closure = target_verifier.clone();
        let new_handler = move |leg| {
            let verifier = match leg {
                ConnectionLeg::Jump => jump_verifier_for_closure
                    .clone()
                    .expect("connect_via_jump_or_direct only builds a Jump leg when a JumpHost was passed"),
                ConnectionLeg::Target => target_verifier_for_closure.clone(),
            };
            verifying_handler(&verifier)
        };

        let jump_host = jump.as_ref().map(|(jh, _)| jh);
        let mut session = connect_via_jump_or_direct(
            jump_host,
            Arc::new(client::Config::default()),
            &target_resolved.hostname,
            target_resolved.port,
            new_handler,
        )
        .await?;

        // `JumpHost::credential` is only ever consulted internally by
        // `connect_via_jump_or_direct` while it's in scope above — safe to
        // zeroize immediately after that call returns, success or not (also
        // now backstopped by `Credential`'s own `Drop` impl for any path
        // that doesn't reach this line at all — Codex review finding).
        if let Some((mut jump_host, _)) = jump {
            jump_host.credential.zeroize();
        }

        // Try each readable target identity in turn (mirrors
        // `isekai-ssh::native::connect`'s own per-identity loop): a first key
        // the server rejects, or one that fails to parse (e.g.
        // passphrase-protected), must not block a later configured identity
        // the server *would* accept. The previous "first readable key only"
        // behavior silently failed the whole bootstrap in that case.
        let mut authed = false;
        for credential in &target_resolved.credentials {
            match authenticate_session(&mut session.handle, &target_resolved.username, credential).await {
                Ok(true) => {
                    authed = true;
                    break;
                }
                Ok(false) => continue,
                Err(russh_stream_session::SessionError::InvalidPrivateKey(_)) => continue,
                Err(e) => return Err(BootstrapError::Session(e)),
            }
        }
        // Every candidate credential (accepted or not) is zeroized when
        // `target_resolved` drops at this function's scope end, via
        // `Credential`'s own `Drop` impl — no explicit pass needed.
        if !authed {
            return Err(BootstrapError::Session(russh_stream_session::SessionError::Auth(
                russh::Error::NotAuthenticated,
            )));
        }

        Ok(session)
    }
}

/// One hop's resolved connection parameters: the `HostName`-resolved
/// address to actually dial, the port, the username to authenticate as, and
/// the ordered list of candidate credentials to try (every readable
/// `IdentityFile`, in probe order — non-empty by `resolve_hop`'s contract).
struct ResolvedHop {
    hostname: String,
    port: u16,
    username: String,
    credentials: Vec<Credential>,
}

/// Resolves `~/.ssh/config` for `host` (the literal `HostSpec`/`JumpSpec`
/// destination token, e.g. `"bastion"`, not an already-resolved address),
/// then a username and a private key — the `RusshBackend` equivalent of
/// what `ssh(1)` does invisibly for every hop of a `-J` chain.
/// `explicit_user`/`explicit_port` (from `HostSpec`/`JumpSpec` itself) take
/// priority over whatever the config file says, matching `ssh(1)`'s own CLI
/// argument > config file precedence.
async fn resolve_hop(
    host: &str,
    explicit_user: Option<&str>,
    explicit_port: Option<u16>,
    identity_file_override: Option<&[PathBuf]>,
) -> Result<ResolvedHop, BootstrapError> {
    let host_config = openssh_config::resolve_default(host)
        .map_err(|e| BootstrapError::ConfigResolve { host: host.to_string(), detail: e.to_string() })?;

    let hostname = host_config.host_name.clone().unwrap_or_else(|| host.to_string());
    let port = explicit_port.or(host_config.port).unwrap_or(22);
    let username = explicit_user
        .map(str::to_string)
        .or_else(|| host_config.user.clone())
        .or_else(local_username)
        .ok_or_else(|| BootstrapError::NoUsername { host: host.to_string() })?;

    let candidates = match identity_file_override {
        Some(paths) => paths.to_vec(),
        None => {
            let home = isekai_fs_guard::resolve_home_dir().ok_or(BootstrapError::NoHomeDir)?;
            isekai_fs_guard::identity_file_candidates(&host_config.identity_file, &home)
        }
    };
    let credentials = readable_credentials(&candidates)
        .map_err(|detail| BootstrapError::NoCredential { host: host.to_string(), detail })?;
    if credentials.is_empty() {
        return Err(BootstrapError::NoCredential {
            host: host.to_string(),
            detail: format!(
                "no usable identity file found (tried: {})",
                candidates.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", "),
            ),
        });
    }

    Ok(ResolvedHop { hostname, port, username, credentials })
}

fn local_username() -> Option<String> {
    std::env::var("USER").ok().or_else(|| std::env::var("USERNAME").ok())
}

/// Reads *every* candidate in `candidates` that exists on disk, in order,
/// wrapping each in a [`Credential::PublicKey`] — the ~10-line thin loader
/// half of `IdentityFile` handling. Returns all readable candidates rather
/// than just the first (mirrors `isekai-ssh::native::private_key`'s
/// `readable_credentials`): `ssh(1)` offers every configured `IdentityFile`
/// to the server in turn, so a first key that exists but is *rejected* or
/// fails to *parse* must not block the remaining configured keys — the
/// caller (`connect_and_authenticate`) tries each in order. A missing
/// candidate (`NotFound`) is skipped; any other read error (e.g. a
/// permissions problem on a file that *does* exist) is surfaced rather than
/// silently dropped. The candidate *ordering* comes from the shared
/// [`isekai_fs_guard::identity_file_candidates`]; only this `std::fs::read` +
/// `Credential` wrapping stays here (and, identically, in that isekai-ssh
/// module), since `isekai-fs-guard` deliberately doesn't depend on
/// `russh-stream-session`/`Credential`.
fn readable_credentials(candidates: &[PathBuf]) -> Result<Vec<Credential>, String> {
    let mut credentials = Vec::new();
    for path in candidates {
        match std::fs::read(path) {
            Ok(private_key_pem) => credentials.push(Credential::PublicKey { private_key_pem }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(format!("failed to read identity file {}: {e}", path.display())),
        }
    }
    Ok(credentials)
}

/// Real interactive TOFU prompt for a never-before-seen host key —
/// `ssh(1)`'s own wording, adapted (same as `isekai-ssh::native::connect`'s
/// identically-purposed function). Runs on a `spawn_blocking` thread (see
/// `FileBackedHostKeyVerifier::verify`'s docs), so a plain blocking stdin
/// read is safe here.
fn prompt_new_host_confirmation(fingerprint: &str) -> bool {
    use std::io::Write as _;
    eprint!(
        "The authenticity of the bootstrap host can't be established.\n\
         Key fingerprint is {fingerprint}.\n\
         Are you sure you want to continue connecting (yes/no)? "
    );
    let _ = std::io::stderr().flush();
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).is_err() {
        return false;
    }
    matches!(line.trim(), "yes" | "y" | "Y")
}

// ── exec-channel command runner ────────────────────────────────────────

struct SshOutput {
    status: Option<i32>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

/// Runs `remote_command` as a single SSH `exec` request over `handle`,
/// optionally feeding `stdin_chunks` to it in order (sent as separate
/// `channel.data()` writes rather than pre-concatenated into one buffer —
/// Codex review finding: `install_and_launch`'s `stdin_chunks` includes the
/// base64-encoded helper binary, potentially tens of MB, and the remote
/// script's `dd`/`head -c` reads treat stdin as one continuous byte stream
/// regardless of how many SSH_MSG_CHANNEL_DATA packets it arrives in, so
/// there's no need to materialize the concatenation on this side first),
/// and collects (exit status, stdout, stderr) — the `russh` equivalent of
/// `openssh.rs`'s `run_ssh_command`. Stdout/stderr are kept strictly
/// separate (`ChannelMsg::Data` vs. `ChannelMsg::ExtendedData` with `ext ==
/// SSH_EXTENDED_DATA_STDERR`) so the "stdout purity" contract
/// (`BootstrapBackend::install_and_start`'s docs) holds exactly as it does
/// for the real `ssh(1)` subprocess case.
async fn run_russh_command<H: client::Handler>(
    handle: &client::Handle<H>,
    remote_command: &str,
    stdin_chunks: &[&[u8]],
) -> Result<SshOutput, BootstrapError> {
    let mut channel = open_channel(handle, &SessionKind::Exec { command: remote_command.to_string() })
        .await
        .map_err(BootstrapError::Session)?;

    for chunk in stdin_chunks {
        channel.data(*chunk).await.map_err(|e| BootstrapError::Session(russh_stream_session::SessionError::Channel(e)))?;
    }
    channel.eof().await.map_err(|e| BootstrapError::Session(russh_stream_session::SessionError::Channel(e)))?;

    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut status = None;

    while let Some(msg) = channel.wait().await {
        match msg {
            russh::ChannelMsg::Data { data } => stdout.extend_from_slice(&data),
            russh::ChannelMsg::ExtendedData { data, ext } if ext == SSH_EXTENDED_DATA_STDERR => {
                stderr.extend_from_slice(&data);
            }
            // Any other `ext` value is not stdout by definition — never let
            // it leak into the buffer the stdout-purity contract treats as
            // trusted (see this function's own docs).
            russh::ChannelMsg::ExtendedData { data, .. } => stderr.extend_from_slice(&data),
            russh::ChannelMsg::ExitStatus { exit_status } => status = Some(exit_status as i32),
            // Only `Close` (or the loop naturally ending when `wait()`
            // returns `None`) terminates the receive loop — deliberately
            // NOT `Eof`. A server is free to send `CHANNEL_EOF` before the
            // `exit-status` channel request, and breaking on `Eof` would
            // drop that later `ExitStatus`, spuriously reporting `status:
            // None` for a command whose stdout already arrived correctly
            // (e.g. `detect_remote_arch`'s `uname -m` probe).
            russh::ChannelMsg::Close => break,
            _ => {}
        }
    }

    Ok(SshOutput { status, stdout, stderr })
}

fn hex_sha256(binary: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(binary);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

fn normalize_uname_arch(uname_m: &str) -> Result<String, BootstrapError> {
    match uname_m.trim() {
        "x86_64" => Ok("x86_64".to_string()),
        "aarch64" | "arm64" => Ok("aarch64".to_string()),
        other => Err(BootstrapError::UnsupportedArch(other.to_string())),
    }
}

// ── install/launch script (duplicated verbatim from `openssh.rs`, see ──
// ── this module's own docs for why) ──────────────────────────────────

impl RusshBackend {
    async fn install_and_launch(
        &self,
        target: &HostSpec,
        via: &[JumpSpec],
        launch: &LaunchSpec,
        remote_binary_path: &str,
        stun_servers: &[std::net::SocketAddr],
        binary: &[u8],
    ) -> Result<isekai_protocol::HandshakeJson, BootstrapError> {
        let session = self.connect_and_authenticate(target, via).await?;

        let sleep_secs = HANDSHAKE_POLL_INTERVAL_MS as f64 / 1000.0;

        let bootstrap_request = crate::client_candidates::fresh_bootstrap_request_v2(stun_servers).await;
        let request_bytes = serde_json::to_vec(&bootstrap_request).expect("BootstrapRequestV2 always serializes");
        let request_len = request_bytes.len();

        let stun_server_arg = match stun_servers.first() {
            Some(addr) => format!(" --stun-server {addr}"),
            None => String::new(),
        };

        let (launch_args, jwt_bytes): (String, Vec<u8>) = match launch {
            LaunchSpec::Relay(relay) => {
                validate_relay_sni(&relay.relay_sni)
                    .map_err(|e| BootstrapError::InvalidRelayParam(e.to_string()))?;
                validate_relay_jwt(&relay.relay_jwt)
                    .map_err(|e| BootstrapError::InvalidRelayParam(e.to_string()))?;
                let remote_log_level = validate_log_level(&relay.remote_log_level)
                    .map_err(|e| BootstrapError::InvalidRemoteLogLevel(e.to_string()))?;

                let relay_addr = relay.relay_addr;
                let quoted_sni = shell_single_quote(&relay.relay_sni);
                let idle_lifetime_secs = relay.idle_lifetime_secs;
                let relay_transport_arg = match relay.relay_transport {
                    crate::types::RelayTransportKind::Udp => String::new(),
                    crate::types::RelayTransportKind::Qmux => " --relay-transport qmux".to_string(),
                };
                let args = format!(
                    "--target 127.0.0.1:22 --relay {relay_addr} --relay-sni {quoted_sni} \
                     --relay-jwt-file $tmpdir/relay_jwt --bootstrap-request-file $tmpdir/bootstrap-request.json\
                     {relay_transport_arg} --max-idle-lifetime {idle_lifetime_secs} --log-level {remote_log_level}"
                );
                (args, relay.relay_jwt.clone().into_bytes())
            }
            LaunchSpec::Direct { idle_lifetime_secs, remote_log_level, remote_bind_port_range } => {
                let remote_log_level = validate_log_level(remote_log_level)
                    .map_err(|e| BootstrapError::InvalidRemoteLogLevel(e.to_string()))?;
                let bind_port_range_arg = match remote_bind_port_range {
                    Some((start, end)) => format!(" --bind-port-range {start}-{end}"),
                    None => String::new(),
                };
                let args = format!(
                    "--target 127.0.0.1:22 --bind 0.0.0.0:0 --bootstrap-request-file $tmpdir/bootstrap-request.json\
                     {stun_server_arg}{bind_port_range_arg} --max-idle-lifetime {idle_lifetime_secs} --log-level {remote_log_level}"
                );
                (args, Vec::new())
            }
        };

        let jwt_len = jwt_bytes.len();
        let read_jwt_step = if jwt_len > 0 {
            format!(
                "dd bs=1 count={jwt_len} > $tmpdir/relay_jwt 2>/dev/null && [ \"$(wc -c < $tmpdir/relay_jwt | tr -d '[:space:]')\" -eq {jwt_len} ] && "
            )
        } else {
            String::new()
        };

        let remote_dir = remote_parent_dir(remote_binary_path);
        let fingerprint = launch_fingerprint(launch);
        let lock_path = lock_file_path(remote_binary_path);
        let state_path = state_file_path(remote_binary_path, &fingerprint);
        let pid_path = pid_file_path(remote_binary_path, &fingerprint);
        let expected_sha256 = hex_sha256(binary);
        let encoded = base64::engine::general_purpose::STANDARD.encode(binary);
        let encoded_len = encoded.len();
        let upload_failed_marker = UPLOAD_FAILED_MARKER;

        let cmd = format!(
            r#"umask 077
mkdir -p {remote_dir} 2>/dev/null
exec 9>>{lock_path} 2>/dev/null
if command -v flock >/dev/null 2>&1; then flock -w 30 9 2>/dev/null || true; fi
tmpdir=$(mktemp -d) && trap 'rm -rf $tmpdir' EXIT
if dd bs=1 count={request_len} > $tmpdir/bootstrap-request.json 2>/dev/null && [ "$(wc -c < $tmpdir/bootstrap-request.json | tr -d '[:space:]')" -eq {request_len} ] && {read_jwt_step}true; then
  reuse_envelope=""
  if [ -f {state_path} ]; then
    existing_pid=$(sed -n '1p' {state_path} | cut -d' ' -f1)
    existing_fp=$(sed -n '1p' {state_path} | cut -d' ' -f2)
    if [ -n "$existing_pid" ] && kill -0 "$existing_pid" 2>/dev/null; then
      if [ -d /proc ]; then
        existing_exe=$(readlink -f /proc/$existing_pid/exe 2>/dev/null)
        expected_exe=$(readlink -f {remote_binary_path} 2>/dev/null)
      else
        existing_exe=ok
        expected_exe=ok
      fi
      if [ -n "$existing_exe" ] && [ "$existing_exe" = "$expected_exe" ] && [ "$existing_fp" = "{fingerprint}" ]; then
        reuse_envelope=$(sed -n '2p' {state_path})
      fi
    fi
  fi
  if [ -n "$reuse_envelope" ]; then
    head -c {encoded_len} > /dev/null
    printf '%s\n' "$reuse_envelope"
  else
    need_upload=1
    if [ -x {remote_binary_path} ]; then
      if command -v sha256sum >/dev/null 2>&1; then
        current_sha=$(sha256sum {remote_binary_path} 2>/dev/null | cut -d' ' -f1)
      elif command -v shasum >/dev/null 2>&1; then
        current_sha=$(shasum -a 256 {remote_binary_path} 2>/dev/null | cut -d' ' -f1)
      fi
      [ -n "$current_sha" ] && [ "$current_sha" = "{expected_sha256}" ] && need_upload=0
    fi
    upload_ok=1
    if [ "$need_upload" -eq 1 ]; then
      head -c {encoded_len} | base64 -d > {remote_binary_path}.tmp && chmod 0700 {remote_binary_path}.tmp && mv {remote_binary_path}.tmp {remote_binary_path} || upload_ok=0
    else
      head -c {encoded_len} > /dev/null
    fi
    if [ "$upload_ok" -eq 0 ]; then
      echo {upload_failed_marker}
    else
      if command -v setsid >/dev/null 2>&1; then
        ( setsid {remote_binary_path} serve {launch_args} </dev/null >$tmpdir/handshake 2>$tmpdir/log 9>&- & echo $! > {pid_path} )
      else
        ( ( trap '' HUP; exec {remote_binary_path} serve {launch_args} </dev/null >$tmpdir/handshake 2>$tmpdir/log 9>&- ) & echo $! > {pid_path} )
      fi
      for i in $(seq 1 {HANDSHAKE_POLL_ATTEMPTS}); do
        [ -s $tmpdir/handshake ] && break
        sleep {sleep_secs}
      done
      if [ -s $tmpdir/handshake ]; then
        envelope=$(cat $tmpdir/handshake)
        new_pid=$(cat {pid_path} 2>/dev/null)
        ( printf '%s %s\n' "$new_pid" "{fingerprint}"; printf '%s\n' "$envelope" ) > {state_path}.tmp.$$ && mv {state_path}.tmp.$$ {state_path}
        printf '%s\n' "$envelope"
      fi
    fi
  fi
  for gc_state in {remote_binary_path}.*.state; do
    [ -e "$gc_state" ] || continue
    [ "$gc_state" = {state_path} ] && continue
    gc_pid=$(sed -n '1p' "$gc_state" | cut -d' ' -f1)
    if [ -z "$gc_pid" ] || ! kill -0 "$gc_pid" 2>/dev/null; then
      rm -f "$gc_state" "${{gc_state%.state}}.pid"
    fi
  done
fi
"#
        );

        let stdin_chunks: [&[u8]; 3] = [request_bytes.as_slice(), jwt_bytes.as_slice(), encoded.as_bytes()];
        let out = run_russh_command(&session.handle, &cmd, &stdin_chunks).await?;

        let non_empty_lines: Vec<&[u8]> =
            out.stdout.split(|&b| b == b'\n').filter(|line| !line.is_empty()).collect();

        match non_empty_lines.as_slice() {
            [] => Err(BootstrapError::HandshakeMissing {
                status: out.status,
                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            }),
            [marker] if *marker == UPLOAD_FAILED_MARKER.as_bytes() => Err(BootstrapError::UploadFailed {
                status: out.status,
                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            }),
            [single] => Ok(isekai_protocol::bootstrap_request::decode_bootstrap_report_v2(single)?.handshake),
            _ => Err(BootstrapError::UnexpectedStdout { extra_lines: non_empty_lines.len() - 1 }),
        }
    }
}

#[async_trait]
impl BootstrapBackend for RusshBackend {
    async fn install_and_start(
        &self,
        target: &HostSpec,
        via: &[JumpSpec],
        helper_binary: &[u8],
        launch: &LaunchSpec,
        remote_binary_path: Option<&str>,
        stun_servers: &[std::net::SocketAddr],
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
    fn readable_credentials_skips_missing_candidates() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does-not-exist");
        let present = dir.path().join("id_ed25519");
        std::fs::write(&present, b"fake key bytes\n").unwrap();

        let credentials = readable_credentials(&[missing, present.clone()]).unwrap();
        assert_eq!(credentials.len(), 1, "the missing candidate is skipped, the present one kept");
        match &credentials[0] {
            Credential::PublicKey { private_key_pem } => assert_eq!(*private_key_pem, std::fs::read(&present).unwrap()),
            _ => panic!("expected Credential::PublicKey"),
        }
    }

    #[test]
    fn readable_credentials_returns_all_present_candidates_in_order() {
        // Regression for the "only the first existing IdentityFile is ever
        // loaded" bug: every present candidate must be returned, in order,
        // so `connect_and_authenticate` can offer each in turn.
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("id_ed25519");
        let second = dir.path().join("id_rsa");
        std::fs::write(&first, b"ed25519 bytes\n").unwrap();
        std::fs::write(&second, b"rsa bytes\n").unwrap();

        let credentials = readable_credentials(&[first.clone(), second.clone()]).unwrap();
        assert_eq!(credentials.len(), 2, "both present candidates must be returned, not just the first");
        let pems: Vec<&Vec<u8>> = credentials
            .iter()
            .map(|c| match c {
                Credential::PublicKey { private_key_pem } => private_key_pem,
                _ => panic!("expected Credential::PublicKey"),
            })
            .collect();
        assert_eq!(*pems[0], std::fs::read(&first).unwrap(), "first candidate must come first");
        assert_eq!(*pems[1], std::fs::read(&second).unwrap(), "second candidate must come second");
    }

    #[test]
    fn readable_credentials_returns_empty_when_nothing_exists() {
        let dir = tempfile::tempdir().unwrap();
        let credentials = readable_credentials(&[dir.path().join("a"), dir.path().join("b")]).unwrap();
        assert!(credentials.is_empty(), "no existing candidate means no credentials to offer");
    }

    #[test]
    fn normalize_uname_arch_accepts_known_architectures() {
        assert_eq!(normalize_uname_arch("x86_64\n").unwrap(), "x86_64");
        assert_eq!(normalize_uname_arch("aarch64\n").unwrap(), "aarch64");
        assert_eq!(normalize_uname_arch("arm64\n").unwrap(), "aarch64");
    }

    #[test]
    fn normalize_uname_arch_rejects_unknown_architectures() {
        let err = normalize_uname_arch("riscv64\n").unwrap_err();
        assert!(matches!(err, BootstrapError::UnsupportedArch(ref a) if a == "riscv64"));
    }

    // ── Finding 1 regression: `Eof` before `exit-status` ────────────────

    use russh::server::{self, Auth, Msg as ServerMsg, Server as _, Session as ServerSession};
    use russh::{Channel as RusshChannel, ChannelId, CryptoVec};
    use russh_keys::ssh_key::private::Ed25519Keypair;
    use russh_keys::PrivateKey;
    use russh_stream_session::HostKeyVerifier;
    use std::net::SocketAddr;
    use tokio::net::TcpListener;

    struct AcceptAllHostKeys;

    #[async_trait]
    impl HostKeyVerifier for AcceptAllHostKeys {
        async fn verify(&self, _fingerprint: &str) -> bool {
            true
        }
    }

    /// Sends its exec output, then `CHANNEL_EOF`, and only *after that* the
    /// `exit-status` request (then closes) — the exact ordering a real
    /// server is free to use but which `run_russh_command` previously
    /// mishandled by breaking its receive loop on `Eof` and dropping the
    /// later `ExitStatus`.
    #[derive(Clone)]
    struct EofBeforeExitStatusServer;

    impl server::Server for EofBeforeExitStatusServer {
        type Handler = EofBeforeExitStatusHandler;
        fn new_client(&mut self, _: Option<SocketAddr>) -> EofBeforeExitStatusHandler {
            EofBeforeExitStatusHandler
        }
    }

    #[derive(Clone)]
    struct EofBeforeExitStatusHandler;

    #[async_trait]
    impl server::Handler for EofBeforeExitStatusHandler {
        type Error = russh::Error;

        async fn auth_publickey(
            &mut self, _user: &str, _public_key: &russh_keys::ssh_key::PublicKey,
        ) -> Result<Auth, Self::Error> {
            Ok(Auth::Accept)
        }

        async fn channel_open_session(
            &mut self, _channel: RusshChannel<ServerMsg>, _session: &mut ServerSession,
        ) -> Result<bool, Self::Error> {
            Ok(true)
        }

        async fn exec_request(
            &mut self, channel: ChannelId, _data: &[u8], session: &mut ServerSession,
        ) -> Result<(), Self::Error> {
            // Deliberate order: stdout, THEN eof, THEN exit-status, THEN
            // close. A loop that breaks on `Eof` would never observe the
            // exit status sent afterward.
            session.data(channel, CryptoVec::from(b"x86_64\n".to_vec()))?;
            session.eof(channel)?;
            session.exit_status_request(channel, 0)?;
            session.close(channel)?;
            Ok(())
        }
    }

    async fn spawn_eof_before_exit_server() -> SocketAddr {
        let host_key = PrivateKey::from(Ed25519Keypair::from_seed(&[7u8; 32]));
        let config = Arc::new(server::Config { keys: vec![host_key], ..Default::default() });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let mut srv = EofBeforeExitStatusServer;
        tokio::spawn(async move {
            let _ = srv.run_on_socket(config, &listener).await;
        });
        addr
    }

    #[tokio::test]
    async fn run_russh_command_captures_exit_status_sent_after_eof() {
        let addr = spawn_eof_before_exit_server().await;
        let verifier = Arc::new(AcceptAllHostKeys);
        let mut session = connect_via_jump_or_direct(
            None,
            Arc::new(client::Config::default()),
            &addr.ip().to_string(),
            addr.port(),
            |_leg| verifying_handler(&verifier),
        )
        .await
        .expect("direct connect should succeed");

        // Any public key is accepted by the server.
        let key = PrivateKey::from(Ed25519Keypair::from_seed(&[8u8; 32]));
        let pem = key.to_openssh(Default::default()).unwrap().as_bytes().to_vec();
        let authed = authenticate_session(&mut session.handle, "tester", &Credential::PublicKey { private_key_pem: pem })
            .await
            .expect("authenticate_session should not error for a well-formed key");
        assert!(authed, "the server accepts any public key");

        let out = run_russh_command(&session.handle, "uname -m", &[]).await.expect("run_russh_command should succeed");

        assert_eq!(
            out.status,
            Some(0),
            "an exit status sent AFTER eof must still be captured, not dropped by breaking on eof"
        );
        assert_eq!(out.stdout, b"x86_64\n", "stdout sent before eof must be preserved");
    }
}
