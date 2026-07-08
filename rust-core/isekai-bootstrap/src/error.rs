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
}
