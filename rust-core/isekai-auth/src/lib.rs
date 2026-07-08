//! Token retrieval + Device Authorization Grant for `isekai-ssh`
//! (`archive/ISEKAI_SSH_DESIGN.md` "JWT発行・配布フロー").
//!
//! Started (phase S-0c-1) as the smallest useful slice of "how does
//! `isekai-ssh connect` get a relay JWT": a `TokenProvider` trait plus two
//! synchronous implementations (`EnvTokenProvider`, `FileTokenProvider`).
//! Phase S-5 adds the pieces that were deliberately deferred back then:
//! `device_flow` (RFC 8628 Device Authorization Grant, used by `isekai-ssh
//! login`) and `refresh` (RFC 6749 §6 `refresh_token` grant, used internally
//! by `FileTokenProvider::get_relay_jwt` to auto-renew a near-expiry token).
//! OS keychain/Secret Service integration is still not implemented — see
//! `file_provider`'s module docs for why and where a future implementation
//! would slot in.
//!
//! `get_relay_jwt` stays a plain synchronous `fn`: `device_flow`/`refresh`'s
//! HTTP calls (`oauth`) use the blocking `ureq` client rather than an async
//! one for exactly this reason (see `oauth`'s module docs) — no executor
//! dependency leaks onto every caller of this trait.
//!
//! `FileTokenProvider`'s backing file (`~/.config/isekai-ssh/token.json`)
//! holds a bearer token (now possibly with a refresh token too), so it is
//! protected the same way `isekai-trust::store` protects
//! `known_helpers.toml`: atomic write (temp file + rename) and
//! `0600`/`0700` permissions, checked and enforced on every load/save (see
//! `file_provider` for details).

pub mod device_flow;
pub mod env_provider;
pub mod error;
pub mod file_provider;
mod oauth;
pub mod refresh;
mod time;

pub use env_provider::EnvTokenProvider;
pub use error::AuthError;
pub use file_provider::{
    default_config_dir, default_token_path, load_token, load_token_set, save_token, save_token_set,
    FileTokenProvider, TokenSet,
};
pub use oauth::TokenResponse;

/// A source of the relay JWT used to authenticate to the relay's HTTP/3 API
/// (`archive/ISEKAI_SSH_DESIGN.md` "シグナリングサーバー = relay の HTTP/3 API
/// （JWTベアラー認証）").
///
/// Synchronous by design — see the module-level docs for why this isn't
/// `async fn`.
pub trait TokenProvider {
    fn get_relay_jwt(&self) -> Result<String, AuthError>;
}
