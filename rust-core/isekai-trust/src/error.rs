use std::path::PathBuf;

/// All failure modes of `isekai-trust` are designed to fail closed: a
/// malformed store, an unexpected file permission, or an unrecognized
/// `update_policy` value must surface as an `Err`, never a silent fallback
/// to a default/empty trust store (`ISEKAI_SSH_DESIGN.md` "trust store の
/// ファイル形式").
#[derive(Debug, thiserror::Error)]
pub enum TrustError {
    #[error("failed to read trust store at {path}: {source}")]
    Read { path: PathBuf, #[source] source: std::io::Error },

    #[error("failed to write trust store at {path}: {source}")]
    Write { path: PathBuf, #[source] source: std::io::Error },

    #[error("failed to create config directory {path}: {source}")]
    CreateDir { path: PathBuf, #[source] source: std::io::Error },

    #[error("failed to inspect permissions of {path}: {source}")]
    Stat { path: PathBuf, #[source] source: std::io::Error },

    /// Covers both malformed TOML and an unrecognized `update_policy` value:
    /// the latter is rejected by `UpdatePolicy`'s `Deserialize` impl, which
    /// makes it a TOML parse error rather than a separate validation step
    /// (see `schema.rs`). Either way this must fail closed, never fall back
    /// to a default value.
    #[error("failed to parse trust store TOML at {path}: {source}")]
    Parse { path: PathBuf, #[source] source: Box<toml::de::Error> },

    #[error("failed to serialize trust store to TOML: {0}")]
    Serialize(#[from] toml::ser::Error),

    #[error("{path} is world-writable (mode {mode:o}); refusing to use it")]
    WorldWritable { path: PathBuf, mode: u32 },

    #[error("could not determine the home directory (HOME is not set)")]
    NoHomeDir,

    #[error("path {path} has no parent directory")]
    NoParentDir { path: PathBuf },

    #[error("empty host spec")]
    EmptyHost,

    #[error("invalid port in host spec {spec:?}: {reason}")]
    InvalidPort { spec: String, reason: String },
}
