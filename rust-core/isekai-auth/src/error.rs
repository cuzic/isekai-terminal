use std::path::PathBuf;

/// All failure modes of `isekai-auth` are designed to fail closed: a missing
/// env var, a missing/malformed token file, an unexpected file permission,
/// or an empty token value must surface as an `Err`, never a silent fallback
/// to an empty/placeholder token.
#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("environment variable {var_name} is not set")]
    EnvVarMissing { var_name: String },

    #[error("environment variable {var_name} is set but empty")]
    EnvVarEmpty { var_name: String },

    #[error("token file not found at {path}")]
    TokenFileNotFound { path: PathBuf },

    #[error("failed to read token file at {path}: {source}")]
    Read { path: PathBuf, #[source] source: std::io::Error },

    #[error("failed to write token file at {path}: {source}")]
    Write { path: PathBuf, #[source] source: std::io::Error },

    #[error("failed to create config directory {path}: {source}")]
    CreateDir { path: PathBuf, #[source] source: std::io::Error },

    #[error("failed to inspect permissions of {path}: {source}")]
    Stat { path: PathBuf, #[source] source: std::io::Error },

    #[error("failed to parse token file at {path}: {source}")]
    Parse { path: PathBuf, #[source] source: serde_json::Error },

    #[error("failed to serialize token file: {0}")]
    Serialize(#[from] serde_json::Error),

    #[error("token file at {path} has an empty relay_jwt value")]
    EmptyToken { path: PathBuf },

    #[error("{path} is world-writable (mode {mode:o}); refusing to use it")]
    WorldWritable { path: PathBuf, mode: u32 },

    #[error("could not determine the home directory (HOME is not set)")]
    NoHomeDir,

    #[error("path {path} has no parent directory")]
    NoParentDir { path: PathBuf },
}
