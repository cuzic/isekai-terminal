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

    /// Every read/write/permission/config-directory failure this crate can
    /// hit — `isekai_fs_guard::FsGuardErrorAt` is the single shared,
    /// path-attached type both this crate and `isekai-trust` build on
    /// (WU-M3; this used to be eight separate variants declared
    /// independently here).
    #[error(transparent)]
    FsGuard(#[from] isekai_fs_guard::FsGuardErrorAt),

    #[error("failed to parse token file at {path}: {source}")]
    Parse { path: PathBuf, #[source] source: serde_json::Error },

    #[error("failed to serialize token file: {0}")]
    Serialize(#[from] serde_json::Error),

    #[error("token file at {path} has an empty relay_jwt value")]
    EmptyToken { path: PathBuf },

    // --- Device Authorization Grant (RFC 8628, `device_flow.rs`) / refresh_token
    // (RFC 6749 §6, `refresh.rs`) errors, phase S-5. ---
    #[error("HTTP request to {url} failed: {reason}")]
    HttpRequest { url: String, reason: String },

    #[error("failed to parse the {context} endpoint's response: {reason}")]
    InvalidTokenResponse { context: String, reason: String },

    /// An OAuth error response (`{"error": ..., "error_description": ...}`,
    /// RFC 6749 §5.2) that isn't one of the recoverable device-flow-polling
    /// codes handled directly by `device_flow::poll_for_token`
    /// (`authorization_pending`/`slow_down`, which never surface as this
    /// error variant at all).
    #[error("OAuth {grant} request failed: {error}")]
    OAuthError { grant: &'static str, error: String, description: Option<String> },

    #[error("device authorization flow was denied")]
    DeviceFlowDenied,

    #[error("device code expired before the device authorization flow completed")]
    DeviceFlowExpired,

    #[error("cannot refresh the stored token: {reason}")]
    RefreshNotConfigured { reason: String },
}
