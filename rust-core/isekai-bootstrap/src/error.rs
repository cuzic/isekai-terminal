//! Errors a `BootstrapBackend` can fail with. Every stdout-related variant
//! here exists to enforce the fail-closed contract from
//! `archive/ISEKAI_SSH_DESIGN.md`'s "`--via` の実装方式" section: a `ssh(1)`
//! subprocess's stdout may contain *only* the one-line `isekai-helper`
//! handshake JSON, never anything else.

#[derive(Debug, thiserror::Error)]
pub enum BootstrapError {
    /// Spawning `ssh(1)`, writing to its stdin, or waiting on it failed at
    /// the OS/process level (binary not found, broken pipe, etc.) — as
    /// opposed to the remote command itself failing.
    #[error("failed to run ssh subprocess: {0}")]
    Io(#[from] std::io::Error),

    /// The binary-upload `ssh(1)` invocation exited non-zero.
    #[error("ssh binary upload command exited with status {status:?}: {stderr}")]
    UploadFailed { status: Option<i32>, stderr: String },

    /// The launch `ssh(1)` invocation produced no non-empty stdout line at
    /// all (helper never wrote the handshake file within the poll window,
    /// or the ssh connection itself failed before the remote command ran).
    #[error("ssh launch command produced no handshake line (status={status:?}): {stderr}")]
    HandshakeMissing { status: Option<i32>, stderr: String },

    /// The launch `ssh(1)` invocation's stdout contained the handshake line
    /// *plus* one or more extra non-empty lines. Per design, this is treated
    /// as untrusted/corrupted output and rejected outright rather than
    /// heuristically picking "the first line that looks like JSON" — the
    /// contract is "stdout carries exactly the handshake JSON, or nothing
    /// trustworthy at all".
    #[error(
        "ssh launch command stdout contained {extra_lines} unexpected non-empty line(s) beyond the handshake JSON"
    )]
    UnexpectedStdout { extra_lines: usize },

    /// The single stdout line we did get failed `isekai-helper`'s handshake
    /// JSON schema/validation (`isekai_protocol::handshake::decode_handshake_json`).
    #[error("failed to parse handshake JSON: {0}")]
    HandshakeParse(#[from] isekai_protocol::ProtocolError),

    /// `relay_sni`/`relay_jwt` failed the strict allow-list charset
    /// validation in `isekai_protocol::bootstrap::validate_relay_sni`/
    /// `validate_relay_jwt` (security review #57). Kept distinct from
    /// `HandshakeParse` — both wrap a `ProtocolError`, but this failure
    /// happens before ever talking to the remote host and has nothing to do
    /// with parsing the handshake response.
    #[error("invalid relay parameter: {0}")]
    InvalidRelayParam(String),

    /// A caller-supplied remote binary path (`#@isekai remote-path`) failed
    /// `isekai_protocol::bootstrap::validate_remote_path`'s strict allow-list
    /// charset check. Same defense-in-depth rationale as `InvalidRelayParam`.
    #[error("invalid remote path: {0}")]
    InvalidRemotePath(String),

    /// A caller-supplied remote log level (`#@isekai remote-log-level`)
    /// failed `isekai_protocol::bootstrap::validate_log_level`'s allow-list
    /// check. Same defense-in-depth rationale as `InvalidRelayParam`.
    #[error("invalid remote log level: {0}")]
    InvalidRemoteLogLevel(String),

    /// A supporting one-off remote command (currently only `uname -m`, for
    /// [`crate::openssh::OpenSshBackend::detect_remote_arch`]) exited
    /// non-zero — distinct from [`Self::UploadFailed`]/[`Self::HandshakeMissing`],
    /// which are specific to the upload/launch steps proper.
    #[error("remote command {command:?} exited with status {status:?}: {stderr}")]
    RemoteCommandFailed { command: String, status: Option<i32>, stderr: String },

    /// `uname -m`'s output didn't match an architecture this project ships
    /// pre-built `isekai-pipe` binaries for. Mirrors
    /// `rust-core/src/helper_bootstrap.rs`'s `IsekaiPipeBinaries::select_for`
    /// (Android's own remote-bootstrap path) — same two supported
    /// architectures, same `"aarch64"`/`"arm64"` aliasing.
    #[error("unsupported remote architecture {0:?} (uname -m)")]
    UnsupportedArch(String),

    // ── `RusshBackend`-only variants below (`fancy-humming-pnueli.md` M3) ──
    /// `RusshBackend`'s `via` chain had more than one hop. Only 0-hop
    /// (direct) and single-hop chains are supported so far —
    /// `russh_stream_session::connect_via_jump_or_direct`'s `JumpHost` is
    /// itself single-hop (`OpenSshBackend`, by contrast, hands an arbitrary
    /// chain straight to `ssh(1)`'s own multi-hop `-J host1,host2,...`
    /// support, which has no native-path equivalent yet).
    #[error("RusshBackend does not yet support a multi-hop via chain ({hops} hops given, only 0 or 1 supported)")]
    UnsupportedViaChain { hops: usize },

    /// No username was available for `host` — no explicit `HostSpec`/
    /// `JumpSpec::user`, no `ssh_config(5)` `User` for that host, and
    /// neither `$USER` nor `%USERNAME%` is set. `ssh(1)` would fall back to
    /// the local account name via the OS user database in this situation;
    /// `RusshBackend` doesn't have an equivalent OS-level lookup wired in
    /// (matches `isekai-ssh`'s own native connect path's same limitation).
    #[error("no username available for {host:?} (no ssh_config User, $USER, or %USERNAME%)")]
    NoUsername { host: String },

    /// No usable `IdentityFile` was found for `host` (checked `ssh_config(5)`
    /// `IdentityFile` entries, then the default `id_ed25519`→`id_rsa`→
    /// `id_ecdsa` probe order) — SSH agent authentication is not yet wired
    /// into `RusshBackend` (documented follow-up), so a missing private key
    /// file means there is nothing left to authenticate with.
    #[error("no usable private key found for {host:?}: {detail}")]
    NoCredential { host: String, detail: String },

    /// Resolving `~/.ssh/config` for `host` via the `openssh-config` crate
    /// failed (e.g. the file exists but isn't readable).
    #[error("failed to resolve ssh config for {host:?}: {detail}")]
    ConfigResolve { host: String, detail: String },

    /// Connecting, authenticating, or opening a channel over `russh` failed
    /// — wraps `russh_stream_session::SessionError` (handshake failure, auth
    /// failure, jump-host tunnel failure, etc.). A host-key *rejection*
    /// specifically is [`HostKeyRejected`](Self::HostKeyRejected) instead,
    /// which carries the verifier's own human-readable reason — this variant
    /// is everything else `SessionError` covers.
    #[error("russh session error: {0}")]
    Session(#[from] russh_stream_session::SessionError),

    /// The SSH host-key TOFU check (`isekai_trust::FileBackedHostKeyVerifier`,
    /// via `russh_stream_session::HostKeyVerifier`) rejected the server's
    /// host key — carries the verifier's own reason (known-mismatch,
    /// declined confirmation, non-interactive refusal, etc.) instead of just
    /// the generic `russh::Error::UnknownKey` ("Unknown server key")
    /// `SessionError::Handshake`/`JumpHandshake` alone would show (`russh`'s
    /// `check_server_key` return has no room to carry a custom message —
    /// see `russh_stream_session::RejectionReason`'s docs).
    #[error("SSH host key rejected: {reason}")]
    HostKeyRejected { reason: String, #[source] source: russh_stream_session::SessionError },

    /// Determining the current user's home directory (for the default
    /// `IdentityFile` probe order, and for the SSH host-key trust store
    /// path) failed.
    #[error("could not determine the home directory")]
    NoHomeDir,

    /// Determining or creating the SSH host-key trust store path
    /// (`isekai_trust::default_ssh_host_key_trust_store_path`) failed.
    #[error("could not determine the SSH host key trust store path: {0}")]
    TrustStorePath(String),
}
