use std::path::PathBuf;

/// All failure modes of `isekai-trust` are designed to fail closed: a
/// malformed store, an unexpected file permission, or an unrecognized
/// `update_policy` value must surface as an `Err`, never a silent fallback
/// to a default/empty trust store (`archive/ISEKAI_SSH_DESIGN.md` "trust store の
/// ファイル形式").
#[derive(Debug, thiserror::Error)]
pub enum TrustError {
    /// Every read/write/permission/config-directory failure this crate can
    /// hit — `isekai_fs_guard::FsGuardErrorAt` is the single shared,
    /// path-attached type both this crate and `isekai-auth` build on (WU-M3;
    /// this used to be eight separate variants declared independently here).
    #[error(transparent)]
    FsGuard(#[from] isekai_fs_guard::FsGuardErrorAt),

    /// Covers both malformed TOML and an unrecognized `update_policy` value:
    /// the latter is rejected by `UpdatePolicy`'s `Deserialize` impl, which
    /// makes it a TOML parse error rather than a separate validation step
    /// (see `schema.rs`). Either way this must fail closed, never fall back
    /// to a default value.
    #[error("failed to parse trust store TOML at {path}: {source}")]
    Parse { path: PathBuf, #[source] source: Box<toml::de::Error> },

    #[error("failed to serialize trust store to TOML: {0}")]
    Serialize(#[from] toml::ser::Error),

    #[error("empty host spec")]
    EmptyHost,

    #[error("invalid port in host spec {spec:?}: {reason}")]
    InvalidPort { spec: String, reason: String },

    #[error("failed to acquire exclusive lock for {path}: {source}")]
    Lock { path: PathBuf, #[source] source: std::io::Error },
}
