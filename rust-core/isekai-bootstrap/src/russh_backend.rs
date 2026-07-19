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
use std::time::Duration;

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
    authenticate_session, connect_via_jump_or_direct, open_channel, verifying_handler_with_reason, ConnectionLeg,
    Credential, JumpHost, RejectionReason, Session, SessionKind, VerifyingHandler,
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

    /// Production API for `TofuConfirmation::Silent` callers
    /// (`bootstrap_and_register`'s stale-trust auto-recovery / `doctor
    /// --fix` re-deploy): any never-before-seen host key is refused
    /// immediately, without ever touching stdin. Unlike
    /// [`with_confirm_new_host`](Self::with_confirm_new_host) (test-only,
    /// arbitrary fixed answer), this is meant to be installed in real
    /// production code — `native::bootstrap_backend::default_bootstrap_backend`
    /// installs it whenever its `silent` argument is true.
    ///
    /// This exists because `TofuConfirmation::Silent` promises "no
    /// confirmation needed" (the app-level trust-registration prompt is
    /// skipped because the profile was already trusted once), but that
    /// promise doesn't reach this *separate*, SSH-protocol-layer host-key
    /// check on its own — [`prompt_new_host_confirmation`]'s doc comment
    /// explains why that function can't infer "silent" from stdin being a
    /// non-terminal (a real regression: it would also refuse the
    /// legitimate piped answers `TofuConfirmation::AlwaysPrompt` callers
    /// use). A refused-but-genuinely-unknown key here means the profile's
    /// SSH-layer trust is out of sync with its app-layer trust (e.g. the
    /// profile was copied to a different machine, or `known_ssh_hosts.toml`
    /// was deleted) — failing fast with a clear message is the correct
    /// `always-connects.md` behavior (never hang, never silently
    /// auto-accept an unverified key), pointing the user at the one
    /// genuinely-unautomatable step (`isekai-ssh init`/`doctor --fix` run
    /// interactively once).
    pub fn with_unattended_new_host_policy(mut self) -> Self {
        self.confirm_new_host = Arc::new(|fingerprint| {
            eprintln!(
                "isekai-ssh: unknown SSH host key (fingerprint {fingerprint}) in a silent/automated \
                 context — refusing without prompting. Run `isekai-ssh init`/`doctor --fix` from an \
                 interactive terminal once to confirm it."
            );
            false
        });
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
                // The jump hop authenticates with only the *first readable*
                // identity: `connect_via_jump_or_direct`/`JumpHost` take a
                // single `Credential` and authenticate it internally, so
                // trying each jump candidate in turn (the way the target hop
                // below now does) would need a `russh-stream-session` API
                // change to accept and iterate a credential list — a
                // documented follow-up, kept out of this `russh_backend.rs`-
                // local change. Unreadable candidates are skipped (not fatal);
                // only "no readable identity at all" is an error.
                let ResolvedHop { hostname, port, username, identity_paths } = jump_resolved;
                let jump_credential = first_readable_identity(&identity_paths).ok_or_else(|| {
                    BootstrapError::NoCredential {
                        host: spec.host.clone(),
                        detail: format!("no readable identity file for jump host (tried: {})", identity_paths_display(&identity_paths)),
                    }
                })?;
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
        // Shared by both legs — safe because they connect sequentially, never
        // concurrently, so only one `verify` call is ever pending at a time.
        // The reason text itself already names the specific `host_port` that
        // rejected (each leg's `FileBackedHostKeyVerifier` was constructed
        // with its own), so a single slot doesn't lose which leg failed.
        let rejection = RejectionReason::new();
        let jump_verifier_for_closure = jump.as_ref().map(|(_, v)| v.clone());
        let target_verifier_for_closure = target_verifier.clone();
        let rejection_for_closure = rejection.clone();
        let new_handler = move |leg| {
            let verifier = match leg {
                ConnectionLeg::Jump => jump_verifier_for_closure
                    .clone()
                    .expect("connect_via_jump_or_direct only builds a Jump leg when a JumpHost was passed"),
                ConnectionLeg::Target => target_verifier_for_closure.clone(),
            };
            verifying_handler_with_reason(&verifier, &rejection_for_closure)
        };

        let jump_host = jump.as_ref().map(|(jh, _)| jh);
        let mut session = connect_via_jump_or_direct(
            jump_host,
            Arc::new(client::Config::default()),
            &target_resolved.hostname,
            target_resolved.port,
            new_handler,
        )
        .await
        .map_err(|source| match rejection.take() {
            Some(reason) => BootstrapError::HostKeyRejected { reason, source },
            None => BootstrapError::Session(source),
        })?;

        // `JumpHost::credential` is only ever consulted internally by
        // `connect_via_jump_or_direct` while it's in scope above — safe to
        // zeroize immediately after that call returns, success or not (also
        // now backstopped by `Credential`'s own `Drop` impl for any path
        // that doesn't reach this line at all — Codex review finding).
        if let Some((mut jump_host, _)) = jump {
            jump_host.credential.zeroize();
        }

        // Try each target identity in turn, reading each one *lazily* right
        // before trying it (mirrors `isekai-ssh::native::connect`'s own
        // per-identity loop): a key the server rejects, one that fails to
        // parse (e.g. passphrase-protected), or one that can't be read at all
        // must not block a later configured identity the server *would*
        // accept. Reading interleaved with auth — rather than reading every
        // candidate up front — means an unreadable *later* file can't fail
        // the whole bootstrap before an accepted *earlier* one is even tried.
        let mut any_readable = false;
        let mut authed = false;
        for path in &target_resolved.identity_paths {
            let Some(mut credential) = read_identity_credential(path) else { continue };
            any_readable = true;
            let result = authenticate_session(&mut session.handle, &target_resolved.username, &credential).await;
            // Zeroize this candidate's key bytes as soon as this attempt is
            // done — don't wait for the whole loop / scope to end (also
            // backstopped by `Credential`'s own `Drop`).
            credential.zeroize();
            match result {
                Ok(true) => {
                    authed = true;
                    break;
                }
                Ok(false) => continue,
                Err(russh_stream_session::SessionError::InvalidPrivateKey(_)) => continue,
                Err(e) => return Err(BootstrapError::Session(e)),
            }
        }
        if !authed {
            // Distinguish "every readable key was rejected" (auth failure)
            // from "no identity file was readable at all" (missing key) —
            // the latter is far more actionable for a user.
            return Err(if any_readable {
                BootstrapError::Session(russh_stream_session::SessionError::Auth(russh::Error::NotAuthenticated))
            } else {
                BootstrapError::NoCredential {
                    host: target.host.clone(),
                    detail: format!("no readable identity file (tried: {})", identity_paths_display(&target_resolved.identity_paths)),
                }
            });
        }

        Ok(session)
    }
}

/// One hop's resolved connection parameters: the `HostName`-resolved
/// address to actually dial, the port, the username to authenticate as, and
/// the ordered list of candidate `IdentityFile` paths to try (probe order).
///
/// The paths are deliberately *not* read here: the caller reads each one
/// lazily, right before trying it, and skips any it can't read — so an
/// unreadable (e.g. permissions-denied) *later* candidate can never block a
/// perfectly good *earlier* one, matching `ssh(1)`'s tolerant `IdentityFile`
/// handling. (Reading them all up front and propagating the first read error
/// would reintroduce exactly that bug.)
struct ResolvedHop {
    hostname: String,
    port: u16,
    username: String,
    identity_paths: Vec<PathBuf>,
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

    let identity_paths = match identity_file_override {
        Some(paths) => paths.to_vec(),
        None => {
            let home = isekai_fs_guard::resolve_home_dir().ok_or(BootstrapError::NoHomeDir)?;
            isekai_fs_guard::identity_file_candidates(&host_config.identity_file, &home)
        }
    };

    Ok(ResolvedHop { hostname, port, username, identity_paths })
}

fn local_username() -> Option<String> {
    std::env::var("USER").ok().or_else(|| std::env::var("USERNAME").ok())
}

/// Reads one candidate identity file into a [`Credential::PublicKey`], or
/// returns `None` for **any** read error (missing, permissions-denied, ...).
/// Tolerating every read error — not just `NotFound` — matches `ssh(1)`: an
/// `IdentityFile` it can't read is skipped, never fatal, so it can't block a
/// later readable candidate. The bytes are only parsed/validated later, at
/// the authentication attempt itself (surfaced as
/// `SessionError::InvalidPrivateKey`), so a file that reads but isn't a valid
/// OpenSSH key is still returned here and fails at its own auth try.
fn read_identity_credential(path: &Path) -> Option<Credential> {
    match std::fs::read(path) {
        Ok(private_key_pem) => Some(Credential::PublicKey { private_key_pem }),
        Err(_) => None,
    }
}

/// The first candidate in `paths` that reads successfully (skipping any that
/// don't), or `None` if none are readable. Used for the jump hop, which — via
/// `connect_via_jump_or_direct`/`JumpHost` — can only carry a single
/// credential (see the jump-hop note in `connect_and_authenticate`).
fn first_readable_identity(paths: &[PathBuf]) -> Option<Credential> {
    paths.iter().find_map(|p| read_identity_credential(p))
}

/// Renders a candidate-paths list for a "no readable identity file" error.
fn identity_paths_display(paths: &[PathBuf]) -> String {
    paths.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", ")
}

/// Real interactive TOFU prompt for a never-before-seen host key —
/// `ssh(1)`'s own wording, adapted (same as `isekai-ssh::native::connect`'s
/// identically-purposed function). Runs on a `spawn_blocking` thread (see
/// `FileBackedHostKeyVerifier::verify`'s docs), so a plain blocking stdin
/// read is safe here.
///
/// This is the `RusshBackend::new()` (interactive) default — deliberately
/// does **not** gate on `std::io::IsTerminal` (a real Windows CI regression:
/// an earlier version of this guard refused *any* non-tty stdin, which broke
/// the legitimate pattern of piping a real answer to this prompt — every
/// e2e test, and any real non-interactive-terminal automation, answers it
/// that way; a piped stdin is never a terminal even when something on the
/// other end genuinely is answering it). Callers that need a genuine
/// never-prompt guarantee (`bootstrap_and_register`'s
/// `TofuConfirmation::Silent` re-deploy) install
/// [`with_unattended_new_host_policy`](RusshBackend::with_unattended_new_host_policy)
/// instead of relying on this function to infer silence from stdin.
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

/// Maximum time `run_russh_command` will wait with **no forward progress at
/// all** — neither a stdin byte accepted by the remote's flow-control window
/// nor a channel message received — before giving up and returning whatever
/// it has collected so far. This is an *idle* bound (reset on every completed
/// sub-write and every received message), **not** a total-runtime bound, so a
/// legitimately slow-but-progressing multi-MB helper upload over a slow link
/// never trips it; only a genuinely wedged channel does. Bounding the wait is
/// what turns an otherwise-infinite hang into a normal recoverable failure,
/// as the `always-connects` principle requires (a bootstrap that hangs can't
/// enter the silent re-deploy/retry cycle at all).
///
/// Why this is needed: `install_and_launch` feeds the base64-encoded helper
/// binary (tens of MB) as stdin, far larger than SSH's initial flow-control
/// window. Writing past the window requires the remote to actually consume
/// the data and send `WINDOW_ADJUST` back. If the remote script dies early
/// (disk full, parse failure, ...) and sends `CHANNEL_CLOSE` before consuming
/// all of stdin, russh 0.48.2's close handling (`client/encrypted.rs`) just
/// removes the channel from its routing map — it does **not** wake a
/// `ChannelTx` (`channels/io/tx.rs`) parked waiting for window space (unlike
/// `WINDOW_ADJUST`, which does). So a plain "write all of stdin, then read"
/// loop parks forever. This function instead reads the channel *concurrently*
/// with writing, so it observes the `Close`/`Eof`/`None` and abandons the
/// doomed write; the idle timeout is a backstop for the rarer case of a
/// remote that neither consumes stdin nor closes the channel.
const NO_PROGRESS_TIMEOUT: Duration = Duration::from_secs(30);

/// Granularity (in bytes) at which stdin is handed to `write_all` between
/// progress checkpoints. Each completed sub-write pokes the idle timer, so a
/// slow-but-moving upload keeps resetting [`NO_PROGRESS_TIMEOUT`] instead of
/// racing a single giant `write_all` against it. 32 KiB matches russh's
/// default `maximum_packet_size`, so a sub-write is at most a couple of window
/// grants — small enough that even a slow remote clears one well inside the
/// idle bound, large enough to keep per-write overhead negligible.
const STDIN_WRITE_CHUNK: usize = 32 * 1024;

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
///
/// Writing stdin and reading the channel happen **concurrently** (see
/// [`NO_PROGRESS_TIMEOUT`] for the full rationale): a remote that closes the
/// channel before consuming all of stdin must not deadlock the writer. If the
/// channel closes (or idles out) mid-write, the write is abandoned and
/// whatever output arrived so far is returned — an empty/status-less result
/// then becomes a proper recoverable `BootstrapError` in the caller
/// (`HandshakeMissing`/`RemoteCommandFailed`), never an infinite hang.
async fn run_russh_command<H: client::Handler>(
    handle: &client::Handle<H>,
    remote_command: &str,
    stdin_chunks: &[&[u8]],
) -> Result<SshOutput, BootstrapError> {
    run_russh_command_inner(handle, remote_command, stdin_chunks, NO_PROGRESS_TIMEOUT).await
}

/// Body of [`run_russh_command`], parameterized on the idle timeout so tests
/// can exercise the "remote never responds" path in milliseconds instead of
/// [`NO_PROGRESS_TIMEOUT`]'s production value.
async fn run_russh_command_inner<H: client::Handler>(
    handle: &client::Handle<H>,
    remote_command: &str,
    stdin_chunks: &[&[u8]],
    no_progress_timeout: Duration,
) -> Result<SshOutput, BootstrapError> {
    use tokio::io::AsyncWriteExt as _;

    let mut channel = open_channel(handle, &SessionKind::Exec { command: remote_command.to_string() })
        .await
        .map_err(BootstrapError::Session)?;

    // `make_writer()` borrows the channel only for this call: the `impl
    // AsyncWrite` it returns clones the sender + window handle and owns them
    // (russh is edition 2018, so this RPIT does not capture `&self`'s
    // lifetime), leaving `channel` free for the concurrent `channel.wait()`
    // below. That independence is the whole point — it is what lets us write
    // stdin and read the channel at the same time.
    let mut writer = channel.make_writer();

    // Poked after each completed sub-write so the idle timer can distinguish
    // "slow but moving" from "wedged". A `Notify` collapses bursts to a single
    // wake, which is exactly what we want here (any progress at all resets the
    // timer).
    let progress = Arc::new(tokio::sync::Notify::new());
    let write_progress = progress.clone();

    // One future for the entire stdin stream + EOF. We only ever *abandon*
    // (drop) it — never resume it after the read side has seen the channel
    // close — so the cancel-safety hazard of re-sending a partially-written
    // buffer cannot arise: on any loop iteration where another `select!` arm
    // wins, this future is merely left un-polled (its internal write cursor is
    // preserved for the next iteration, since it is pinned across the loop);
    // it is dropped for good only when we `break`, at which point we never
    // write again.
    let write_fut = async move {
        for chunk in stdin_chunks {
            for sub in chunk.chunks(STDIN_WRITE_CHUNK) {
                writer.write_all(sub).await?;
                write_progress.notify_one();
            }
        }
        writer.shutdown().await?; // sends CHANNEL_EOF
        write_progress.notify_one();
        Ok::<(), std::io::Error>(())
    };
    tokio::pin!(write_fut);

    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut status = None;
    let mut writing_done = false;

    let idle = tokio::time::sleep(no_progress_timeout);
    tokio::pin!(idle);

    loop {
        tokio::select! {
            // Drive stdin writing to completion. A write error here almost
            // always means the remote already closed the channel; we do NOT
            // propagate it — we stop writing and keep draining so the exit
            // status / stderr explaining *why* are still collected (the caller
            // turns an empty/failed result into a proper recoverable error).
            // The `if !writing_done` guard stops the completed future from
            // being polled again (which would panic).
            res = &mut write_fut, if !writing_done => {
                writing_done = true;
                let _ = res; // benign write errors are intentionally swallowed (see above)
                idle.as_mut().reset(tokio::time::Instant::now() + no_progress_timeout);
            }
            // A completed sub-write: forward progress, so reset the idle bound
            // and keep waiting for the (legitimately slow) upload to finish.
            _ = progress.notified() => {
                idle.as_mut().reset(tokio::time::Instant::now() + no_progress_timeout);
            }
            msg = channel.wait() => {
                idle.as_mut().reset(tokio::time::Instant::now() + no_progress_timeout);
                match msg {
                    Some(russh::ChannelMsg::Data { data }) => stdout.extend_from_slice(&data),
                    Some(russh::ChannelMsg::ExtendedData { data, ext }) if ext == SSH_EXTENDED_DATA_STDERR => {
                        stderr.extend_from_slice(&data);
                    }
                    // Any other `ext` value is not stdout by definition — never
                    // let it leak into the buffer the stdout-purity contract
                    // treats as trusted (see this function's own docs).
                    Some(russh::ChannelMsg::ExtendedData { data, .. }) => stderr.extend_from_slice(&data),
                    Some(russh::ChannelMsg::ExitStatus { exit_status }) => status = Some(exit_status as i32),
                    // Only `Close` (or `wait()` returning `None`) terminates the
                    // receive loop — deliberately NOT `Eof`. A server is free to
                    // send `CHANNEL_EOF` before the `exit-status` channel
                    // request, and breaking on `Eof` would drop that later
                    // `ExitStatus`, spuriously reporting `status: None` for a
                    // command whose stdout already arrived correctly (e.g.
                    // `detect_remote_arch`'s `uname -m` probe).
                    Some(russh::ChannelMsg::Close) | None => break,
                    Some(_) => {}
                }
            }
            // No progress at all for `no_progress_timeout`: the channel is
            // wedged (remote neither consuming stdin nor closing). Give up with
            // whatever we have — a bounded failure the caller can recover from,
            // never a hang.
            _ = &mut idle => break,
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
                let resume_window_secs = relay.resume_window_secs;
                let relay_transport_arg = match relay.relay_transport {
                    crate::types::RelayTransportKind::Udp => String::new(),
                    crate::types::RelayTransportKind::Qmux => " --relay-transport qmux".to_string(),
                };
                let args = format!(
                    "--target 127.0.0.1:22 --relay {relay_addr} --relay-sni {quoted_sni} \
                     --relay-jwt-file $tmpdir/relay_jwt --bootstrap-request-file $tmpdir/bootstrap-request.json\
                     {relay_transport_arg} --max-idle-lifetime {idle_lifetime_secs} \
                     --resume-window {resume_window_secs} --log-level {remote_log_level}"
                );
                (args, relay.relay_jwt.clone().into_bytes())
            }
            LaunchSpec::Direct { idle_lifetime_secs, remote_log_level, remote_bind_port_range, resume_window_secs } => {
                let remote_log_level = validate_log_level(remote_log_level)
                    .map_err(|e| BootstrapError::InvalidRemoteLogLevel(e.to_string()))?;
                let bind_port_range_arg = match remote_bind_port_range {
                    Some((start, end)) => format!(" --bind-port-range {start}-{end}"),
                    None => String::new(),
                };
                let args = format!(
                    "--target 127.0.0.1:22 --bind 0.0.0.0:0 --bootstrap-request-file $tmpdir/bootstrap-request.json\
                     {stun_server_arg}{bind_port_range_arg} --max-idle-lifetime {idle_lifetime_secs} \
                     --resume-window {resume_window_secs} --log-level {remote_log_level}"
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
sha256_of() {{
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" 2>/dev/null | cut -d' ' -f1
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" 2>/dev/null | cut -d' ' -f1
  fi
}}
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
      # pid/exe-path/fingerprint matching only proves the same binary path
      # is still running, not that its contents are what this `isekai-ssh`
      # build expects — a still-alive helper can predate a bugfix to
      # `isekai-pipe serve` itself. Never killed here if stale (some other
      # client may be mid-session on it, same as `openssh.rs`'s
      # fingerprint-mismatch case) — just not reused, falling through to
      # the normal upload+launch path below.
      if [ -n "$existing_exe" ] && [ "$existing_exe" = "$expected_exe" ] && [ "$existing_fp" = "{fingerprint}" ]; then
        existing_sha=$(sha256_of {remote_binary_path})
        if [ "$existing_sha" = "{expected_sha256}" ]; then
          reuse_envelope=$(sed -n '2p' {state_path})
        fi
      fi
    fi
  fi
  if [ -n "$reuse_envelope" ]; then
    head -c {encoded_len} > /dev/null
    printf '%s\n' "$reuse_envelope"
  else
    need_upload=1
    if [ -x {remote_binary_path} ]; then
      current_sha=$(sha256_of {remote_binary_path})
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

    #[tokio::test]
    async fn with_unattended_new_host_policy_declines_without_blocking() {
        // Regression test for the always-connects.md gap a real Windows CI
        // run surfaced: a `TofuConfirmation::Silent` caller (doctor --fix /
        // stale-trust auto-recovery) must never block waiting on stdin. This
        // confirms the *production* silent policy — installed by
        // `native::bootstrap_backend::default_bootstrap_backend` when
        // `silent` is true — declines immediately and never touches stdin
        // (unlike the never-checked-in `IsTerminal`-gated version of this
        // fix, which incorrectly refused legitimate piped answers too; see
        // `prompt_new_host_confirmation`'s doc comment).
        let backend = RusshBackend { store_path: PathBuf::new(), confirm_new_host: Arc::new(|_| panic!("must not be reached")), identity_file_override: None }
            .with_unattended_new_host_policy();
        assert!(!(backend.confirm_new_host)("SHA256:deadbeef"));
    }

    #[test]
    fn read_identity_credential_reads_present_and_skips_missing() {
        let dir = tempfile::tempdir().unwrap();
        let present = dir.path().join("id_ed25519");
        std::fs::write(&present, b"fake key bytes\n").unwrap();

        // `Credential` impls `Drop` (it zeroizes), so it can't be moved out
        // of — bind it, then match by reference.
        let cred = read_identity_credential(&present).expect("a present file must yield Some");
        match &cred {
            Credential::PublicKey { private_key_pem } => {
                assert_eq!(*private_key_pem, std::fs::read(&present).unwrap());
            }
            _ => panic!("expected Credential::PublicKey for a present file"),
        }
        assert!(
            read_identity_credential(&dir.path().join("does-not-exist")).is_none(),
            "any read error (here, missing) must yield None, not a fatal error"
        );
    }

    #[test]
    fn first_readable_identity_skips_missing_and_returns_the_first_present() {
        // Regression for the "only the first *existing* IdentityFile is ever
        // considered" bug: a missing earlier candidate must be skipped so a
        // later present one is still found (and, in the auth loop, a rejected
        // earlier one still falls through to a later accepted one).
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does-not-exist");
        let present = dir.path().join("id_rsa");
        std::fs::write(&present, b"rsa bytes\n").unwrap();

        let cred = first_readable_identity(&[missing, present.clone()]).expect("the present candidate must be returned");
        match &cred {
            Credential::PublicKey { private_key_pem } => {
                assert_eq!(*private_key_pem, std::fs::read(&present).unwrap(), "the missing candidate is skipped, the present one returned");
            }
            _ => panic!("expected Credential::PublicKey"),
        }
    }

    #[test]
    fn first_readable_identity_returns_none_when_nothing_is_readable() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            first_readable_identity(&[dir.path().join("a"), dir.path().join("b")]).is_none(),
            "no readable candidate means no credential to offer"
        );
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
    use russh_stream_session::{verifying_handler, HostKeyVerifier, VerifyOutcome};
    use std::net::SocketAddr;
    use tokio::net::TcpListener;

    struct AcceptAllHostKeys;

    #[async_trait]
    impl HostKeyVerifier for AcceptAllHostKeys {
        async fn verify(&self, _fingerprint: &str) -> VerifyOutcome {
            VerifyOutcome::Accepted
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

    // ── run_russh_command must not hang on early close / a wedged channel ──
    //
    // The bug: `install_and_launch` feeds tens of MB of stdin, far past SSH's
    // flow-control window. The old "write ALL stdin, then read" loop parked in
    // `channel.data()` forever if the remote closed the channel before draining
    // stdin, because russh 0.48.2's CHANNEL_CLOSE path never wakes a
    // window-blocked `ChannelTx`. The fix reads concurrently with writing and
    // bounds the total idle wait, so both an early close and a silently
    // non-responsive server terminate in finite time.

    /// Connects to `addr` accepting any host key, authenticates with any key
    /// (the test servers below accept all), and returns the live session —
    /// the shared preamble for the two hang-regression tests.
    async fn connect_and_auth_accept_all(addr: SocketAddr) -> Session<VerifyingHandler<AcceptAllHostKeys>> {
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
        let key = PrivateKey::from(Ed25519Keypair::from_seed(&[8u8; 32]));
        let pem = key.to_openssh(Default::default()).unwrap().as_bytes().to_vec();
        let authed = authenticate_session(&mut session.handle, "tester", &Credential::PublicKey { private_key_pem: pem })
            .await
            .expect("authenticate_session should not error for a well-formed key");
        assert!(authed, "the server accepts any public key");
        session
    }

    /// Spawns a server advertising only a tiny (`4 KiB`) flow-control window,
    /// so a client sending far more stdin than that blocks almost immediately —
    /// the precondition for reproducing the window-blocked-writer hang.
    async fn spawn_small_window_server<S>(mut srv: S) -> SocketAddr
    where
        S: server::Server + Send + 'static,
        S::Handler: Send + 'static,
    {
        let host_key = PrivateKey::from(Ed25519Keypair::from_seed(&[7u8; 32]));
        let config = Arc::new(server::Config { keys: vec![host_key], window_size: 4096, ..Default::default() });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = srv.run_on_socket(config, &listener).await;
        });
        addr
    }

    /// Sends a line of stdout then immediately closes the channel, WITHOUT ever
    /// reading stdin. A client that writes all of stdin before reading would
    /// block forever (russh 0.48.2 doesn't wake a window-blocked writer on
    /// CHANNEL_CLOSE); reading concurrently, `run_russh_command` sees the close.
    #[derive(Clone)]
    struct CloseWithoutReadingStdinServer;

    impl server::Server for CloseWithoutReadingStdinServer {
        type Handler = CloseWithoutReadingStdinHandler;
        fn new_client(&mut self, _: Option<SocketAddr>) -> CloseWithoutReadingStdinHandler {
            CloseWithoutReadingStdinHandler
        }
    }

    #[derive(Clone)]
    struct CloseWithoutReadingStdinHandler;

    #[async_trait]
    impl server::Handler for CloseWithoutReadingStdinHandler {
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
            // Emit some stdout, then close — never touching stdin.
            session.data(channel, CryptoVec::from(b"partial-output\n".to_vec()))?;
            session.close(channel)?;
            Ok(())
        }
    }

    /// Accepts the exec but then does nothing at all: never reads stdin, never
    /// sends output, never closes. A client sending more stdin than the window
    /// parks with no forward progress — the case the idle timeout backstops.
    #[derive(Clone)]
    struct WedgedServer;

    impl server::Server for WedgedServer {
        type Handler = WedgedHandler;
        fn new_client(&mut self, _: Option<SocketAddr>) -> WedgedHandler {
            WedgedHandler
        }
    }

    #[derive(Clone)]
    struct WedgedHandler;

    #[async_trait]
    impl server::Handler for WedgedHandler {
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
            &mut self, _channel: ChannelId, _data: &[u8], _session: &mut ServerSession,
        ) -> Result<(), Self::Error> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn run_russh_command_returns_when_remote_closes_before_consuming_stdin() {
        let addr = spawn_small_window_server(CloseWithoutReadingStdinServer).await;
        let session = connect_and_auth_accept_all(addr).await;

        // 512 KiB of stdin against a 4 KiB window: the old write-all-then-read
        // loop would block in `channel.data()` forever here.
        let big = vec![b'x'; 512 * 1024];
        let stdin_chunks: [&[u8]; 1] = [big.as_slice()];

        // The outer `timeout` is the actual regression guard: if the fix
        // regresses, this fails with "did not return" instead of hanging CI.
        let out = tokio::time::timeout(Duration::from_secs(10), run_russh_command(&session.handle, "cat", &stdin_chunks))
            .await
            .expect("run_russh_command must return, not hang, when the remote closes before draining stdin")
            .expect("a partial run still yields Ok with whatever output arrived");

        assert_eq!(out.stdout, b"partial-output\n", "stdout sent before the early close must be captured");
    }

    #[tokio::test]
    async fn run_russh_command_inner_gives_up_on_a_wedged_channel() {
        let addr = spawn_small_window_server(WedgedServer).await;
        let session = connect_and_auth_accept_all(addr).await;

        let big = vec![b'x'; 512 * 1024];
        let stdin_chunks: [&[u8]; 1] = [big.as_slice()];

        // A short idle timeout so the "remote never responds" path is exercised
        // in milliseconds instead of NO_PROGRESS_TIMEOUT's 30 s. The outer
        // wall-clock timeout is far longer, so a *hang* (rather than the idle
        // bound firing) is what fails the test.
        let out = tokio::time::timeout(
            Duration::from_secs(10),
            run_russh_command_inner(&session.handle, "cat", &stdin_chunks, Duration::from_millis(200)),
        )
        .await
        .expect("the idle timeout must bound a wedged channel, not hang")
        .expect("giving up on a wedged channel is Ok(partial), which the caller turns into a recoverable error");

        assert!(out.stdout.is_empty(), "a server that never responds produces no stdout");
        assert_eq!(out.status, None, "no exit status ever arrives from a wedged channel");
    }
}
