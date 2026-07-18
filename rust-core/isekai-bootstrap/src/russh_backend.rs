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

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use base64::Engine as _;
use isekai_protocol::bootstrap::{
    remote_parent_dir, shell_single_quote, validate_log_level, validate_relay_jwt, validate_relay_sni,
    validate_remote_path, HANDSHAKE_POLL_ATTEMPTS, HANDSHAKE_POLL_INTERVAL_MS, ISEKAI_PIPE_BIN_NAME,
    ISEKAI_PIPE_INSTALL_DIR,
};
use russh::client;
use russh_stream_session::{
    authenticate_session, connect_via_jump_or_direct, open_channel, verifying_handler, Credential, HostKeyVerifier,
    JumpHost, SessionKind, VerifyingHandler,
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
    /// Test-only: see `with_identity_file`'s docs. `None` in every
    /// production code path.
    identity_file_override: Option<PathBuf>,
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
        self.identity_file_override = Some(path);
        self
    }

    /// Runs `uname -m` on `target` (through `via`, if given) and normalizes
    /// the result to `"x86_64"`/`"aarch64"` — the `RusshBackend` equivalent
    /// of `OpenSshBackend::detect_remote_arch`, same purpose (letting a
    /// caller with no explicit `--helper-binary` pick which pre-built
    /// `isekai-pipe` variant to fetch before ever calling
    /// `install_and_start`).
    pub async fn detect_remote_arch(&self, target: &HostSpec, via: &[JumpSpec]) -> Result<String, BootstrapError> {
        let handle = self.connect_and_authenticate(target, via).await?;
        let out = run_russh_command(&handle, "uname -m", None).await?;
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
    async fn connect_and_authenticate(
        &self,
        target: &HostSpec,
        via: &[JumpSpec],
    ) -> Result<client::Handle<VerifyingHandler<FileBackedHostKeyVerifier>>, BootstrapError> {
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
                ));
                Some((
                    JumpHost {
                        host: jump_resolved.hostname,
                        port: jump_resolved.port,
                        username: jump_resolved.username,
                        credential: jump_resolved.credential,
                    },
                    jump_verifier,
                ))
            }
            None => None,
        };

        // `new_handler` is called once per connection leg by
        // `connect_via_jump_or_direct`: the *first* call is always the jump
        // leg when `jump` is `Some` (never the direct/no-jump case, which
        // makes exactly one call for the target itself) — see that
        // function's own body/docs.
        let mut leg = 0u8;
        let jump_verifier_for_closure = jump.as_ref().map(|(_, v)| v.clone());
        let target_verifier_for_closure = target_verifier.clone();
        let new_handler = move || {
            leg += 1;
            let verifier = match (&jump_verifier_for_closure, leg) {
                (Some(jump_verifier), 1) => jump_verifier.clone(),
                _ => target_verifier_for_closure.clone(),
            };
            verifying_handler(&verifier)
        };

        let jump_host = jump.as_ref().map(|(jh, _)| jh);
        let session = connect_via_jump_or_direct(
            jump_host,
            Arc::new(client::Config::default()),
            &target_resolved.hostname,
            target_resolved.port,
            new_handler,
        )
        .await?;

        // `JumpHost::credential` is only ever consulted internally by
        // `connect_via_jump_or_direct` while it's in scope above — safe to
        // zeroize immediately after that call returns, success or not.
        if let Some((mut jump_host, _)) = jump {
            jump_host.credential.zeroize();
        }

        let mut handle = session.handle;
        let mut target_credential = target_resolved.credential;
        let authed = authenticate_session(&mut handle, &target_resolved.username, &target_credential).await?;
        target_credential.zeroize();
        if !authed {
            return Err(BootstrapError::Session(russh_stream_session::SessionError::Auth(
                russh::Error::NotAuthenticated,
            )));
        }

        Ok(handle)
    }
}

/// One hop's resolved connection parameters: the `HostName`-resolved
/// address to actually dial, the port, the username to authenticate as, and
/// the credential to authenticate with.
struct ResolvedHop {
    hostname: String,
    port: u16,
    username: String,
    credential: Credential,
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
    identity_file_override: Option<&Path>,
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
        Some(path) => vec![path.to_path_buf()],
        None => {
            let home = isekai_fs_guard::resolve_home_dir().ok_or(BootstrapError::NoHomeDir)?;
            identity_file_candidates(&host_config.identity_file, &home)
        }
    };
    let credential = load_first_existing(&candidates)
        .map_err(|detail| BootstrapError::NoCredential { host: host.to_string(), detail })?;

    Ok(ResolvedHop { hostname, port, username, credential })
}

fn local_username() -> Option<String> {
    std::env::var("USER").ok().or_else(|| std::env::var("USERNAME").ok())
}

/// Default `IdentityFile` probe order tried when the config specifies none
/// explicitly — mirrors `isekai-ssh::native::private_key`'s identically
/// named constant (`id_ed25519` → `id_rsa` → `id_ecdsa`).
const DEFAULT_IDENTITY_FILE_NAMES: &[&str] = &["id_ed25519", "id_rsa", "id_ecdsa"];

fn identity_file_candidates(configured: &[PathBuf], home: &Path) -> Vec<PathBuf> {
    if !configured.is_empty() {
        return configured.to_vec();
    }
    DEFAULT_IDENTITY_FILE_NAMES.iter().map(|name| home.join(".ssh").join(name)).collect()
}

fn load_first_existing(candidates: &[PathBuf]) -> Result<Credential, String> {
    for path in candidates {
        match std::fs::read(path) {
            Ok(private_key_pem) => return Ok(Credential::PublicKey { private_key_pem }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(format!("failed to read identity file {}: {e}", path.display())),
        }
    }
    Err(format!(
        "no usable identity file found (tried: {})",
        candidates.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", "),
    ))
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

// ── Host-key TOFU (duplicated from `isekai-ssh::native::host_key_trust`) ──

/// Implements `russh_stream_session::HostKeyVerifier` backed by
/// `isekai_trust::SshHostKeyTrustStore` — TOFU semantics deliberately mirror
/// `ssh(1)`, not a simpler "always trust" shortcut. See
/// `isekai-ssh::native::host_key_trust`'s module docs for the full
/// rationale (known/matching → silently accept+refresh; known/mismatched →
/// silently reject, no prompt; unknown → `confirm_new_host` decides) — this
/// is a verbatim duplicate of that logic (see this module's own docs for
/// why it isn't shared instead).
struct FileBackedHostKeyVerifier {
    store_path: PathBuf,
    host_port: String,
    confirm_new_host: Arc<dyn Fn(&str) -> bool + Send + Sync>,
}

impl FileBackedHostKeyVerifier {
    fn new(store_path: PathBuf, host_port: String, confirm_new_host: Arc<dyn Fn(&str) -> bool + Send + Sync>) -> Self {
        Self { store_path, host_port, confirm_new_host }
    }
}

enum Resolved {
    Decided(bool),
    NeedsConfirmation,
    Failed,
}

#[async_trait]
impl HostKeyVerifier for FileBackedHostKeyVerifier {
    async fn verify(&self, fingerprint: &str) -> bool {
        match self.resolve_locked(fingerprint, false).await {
            Resolved::Decided(accepted) => return accepted,
            Resolved::NeedsConfirmation => {}
            Resolved::Failed => return false,
        }

        let confirm_new_host = self.confirm_new_host.clone();
        let fingerprint_owned = fingerprint.to_string();
        let confirmed = match tokio::task::spawn_blocking(move || confirm_new_host(&fingerprint_owned)).await {
            Ok(confirmed) => confirmed,
            Err(join_error) => {
                log::error!("isekai-bootstrap: SSH host key confirmation task panicked, rejecting connection: {join_error}");
                return false;
            }
        };
        if !confirmed {
            return false;
        }

        match self.resolve_locked(fingerprint, true).await {
            Resolved::Decided(accepted) => accepted,
            Resolved::NeedsConfirmation => unreachable!("insert_if_unknown: true never returns NeedsConfirmation"),
            Resolved::Failed => false,
        }
    }
}

impl FileBackedHostKeyVerifier {
    async fn resolve_locked(&self, fingerprint: &str, insert_if_unknown: bool) -> Resolved {
        let store_path = self.store_path.clone();
        let host_port = self.host_port.clone();
        let fingerprint = fingerprint.to_string();

        let outcome = tokio::task::spawn_blocking(move || {
            isekai_trust::with_locked_ssh_host_key_trust_store(&store_path, |store| {
                match store.get(&host_port) {
                    Some(known) if known.fingerprint == fingerprint => {
                        let mut updated = known.clone();
                        updated.last_seen_at = now_rfc3339();
                        store.insert(host_port.clone(), updated);
                        Ok(Resolved::Decided(true))
                    }
                    Some(known) => {
                        log::error!(
                            "isekai-bootstrap: host key for {host_port} changed (trusted {}, saw {fingerprint}) \
                             — refusing to connect. If this change is expected (e.g. you redeployed), \
                             remove the \"{host_port}\" entry from {} and reconnect.",
                            known.fingerprint,
                            store_path.display(),
                        );
                        Ok(Resolved::Decided(false))
                    }
                    None if insert_if_unknown => {
                        let now = now_rfc3339();
                        store.insert(
                            host_port.clone(),
                            isekai_trust::SshHostKeyTrust { fingerprint: fingerprint.clone(), trusted_at: now.clone(), last_seen_at: now },
                        );
                        Ok(Resolved::Decided(true))
                    }
                    None => Ok(Resolved::NeedsConfirmation),
                }
            })
        })
        .await;

        match outcome {
            Ok(Ok(resolved)) => resolved,
            Ok(Err(e)) => {
                log::warn!("isekai-bootstrap: SSH host key trust store operation failed, rejecting connection: {e}");
                Resolved::Failed
            }
            Err(join_error) => {
                log::error!("isekai-bootstrap: SSH host key trust check task panicked, rejecting connection: {join_error}");
                Resolved::Failed
            }
        }
    }
}

fn now_rfc3339() -> String {
    // A fourth copy of the same tiny RFC3339 formatter this codebase
    // deliberately keeps duplicated per module rather than shared — see
    // `isekai-ssh::wrapper.rs:895-897`'s doc comment for the established
    // rationale this follows.
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    let days = secs / 86_400;
    let time_of_day = secs % 86_400;
    let (h, m, s) = (time_of_day / 3600, (time_of_day % 3600) / 60, time_of_day % 60);
    let (y, mo, d) = civil_from_days(days as i64);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

// ── exec-channel command runner ────────────────────────────────────────

struct SshOutput {
    status: Option<i32>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

/// Runs `remote_command` as a single SSH `exec` request over `handle`,
/// optionally feeding `stdin_payload` to it, and collects (exit status,
/// stdout, stderr) — the `russh` equivalent of `openssh.rs`'s
/// `run_ssh_command`. Stdout/stderr are kept strictly separate
/// (`ChannelMsg::Data` vs. `ChannelMsg::ExtendedData` with `ext ==
/// SSH_EXTENDED_DATA_STDERR`) so the "stdout purity" contract
/// (`BootstrapBackend::install_and_start`'s docs) holds exactly as it does
/// for the real `ssh(1)` subprocess case.
async fn run_russh_command<H: client::Handler>(
    handle: &client::Handle<H>,
    remote_command: &str,
    stdin_payload: Option<&[u8]>,
) -> Result<SshOutput, BootstrapError> {
    let mut channel = open_channel(handle, &SessionKind::Exec { command: remote_command.to_string() })
        .await
        .map_err(BootstrapError::Session)?;

    if let Some(payload) = stdin_payload {
        channel.data(payload).await.map_err(|e| BootstrapError::Session(russh_stream_session::SessionError::Channel(e)))?;
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
            russh::ChannelMsg::Eof | russh::ChannelMsg::Close => break,
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
        let handle = self.connect_and_authenticate(target, via).await?;

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

        let stdin_payload = [request_bytes.as_slice(), jwt_bytes.as_slice(), encoded.as_bytes()].concat();
        let out = run_russh_command(&handle, &cmd, Some(&stdin_payload)).await?;

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
    fn identity_file_candidates_uses_configured_when_non_empty() {
        let configured = vec![PathBuf::from("/custom/key")];
        assert_eq!(identity_file_candidates(&configured, Path::new("/home/alice")), configured);
    }

    #[test]
    fn identity_file_candidates_falls_back_to_default_probe_order() {
        let candidates = identity_file_candidates(&[], Path::new("/home/alice"));
        assert_eq!(
            candidates,
            vec![
                PathBuf::from("/home/alice/.ssh/id_ed25519"),
                PathBuf::from("/home/alice/.ssh/id_rsa"),
                PathBuf::from("/home/alice/.ssh/id_ecdsa"),
            ]
        );
    }

    #[test]
    fn load_first_existing_skips_missing_candidates() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does-not-exist");
        let present = dir.path().join("id_ed25519");
        std::fs::write(&present, b"fake key bytes\n").unwrap();

        let credential = load_first_existing(&[missing, present.clone()]).unwrap();
        match credential {
            Credential::PublicKey { private_key_pem } => assert_eq!(private_key_pem, std::fs::read(&present).unwrap()),
            _ => panic!("expected Credential::PublicKey"),
        }
    }

    #[test]
    fn load_first_existing_errors_when_nothing_exists() {
        // `Credential` intentionally doesn't derive `Debug` (avoids
        // accidentally formatting a private key into a log line), so
        // `Result::unwrap_err()` isn't available here — match instead.
        let dir = tempfile::tempdir().unwrap();
        match load_first_existing(&[dir.path().join("a"), dir.path().join("b")]) {
            Err(detail) => assert!(detail.contains("no usable identity file")),
            Ok(_) => panic!("expected an error when no candidate exists"),
        }
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
}
