//! Minimal token retrieval for `isekai-ssh` (`ISEKAI_SSH_DESIGN.md`
//! "JWT発行・配布フロー", phase S-0c-1).
//!
//! This crate intentionally implements only the smallest useful slice of
//! "how does `isekai-ssh connect` get a relay JWT": a `TokenProvider` trait
//! plus two synchronous implementations (`EnvTokenProvider`,
//! `FileTokenProvider`). It does **not** implement the eventual Device
//! Authorization Flow (RFC 8628), OS keychain/Secret Service integration,
//! automatic token refresh, or the `isekai-ssh login`/`logout` commands
//! themselves — those are phase S-5 per the design doc's phase table
//! ("フェーズ分割案", row `S-0c-2 / S-5`). Keeping this crate this small is
//! deliberate so that `isekai-ssh connect`'s early end-to-end wiring isn't
//! blocked on auth UX design.
//!
//! `get_relay_jwt` is a plain synchronous `fn`: both current implementations
//! only do local env var / file I/O, and the design doc explicitly calls
//! out that `isekai-auth`'s minimal version should not block on the async
//! network calls the real Device Authorization Flow will eventually need.
//! Making the trait `async` now, before anything here actually awaits
//! anything, would just push an executor dependency onto every caller for
//! no benefit — S-5 can widen the trait (or add an async-specific one) when
//! there's a real `.await` to justify it.
//!
//! `FileTokenProvider`'s backing file (`~/.config/isekai-ssh/token.json`)
//! holds a bearer token, so it is protected the same way
//! `isekai-trust::store` protects `known_helpers.toml`: atomic write
//! (temp file + rename) and `0600`/`0700` permissions, checked and enforced
//! on every load/save (see `file_provider` for details).

pub mod env_provider;
pub mod error;
pub mod file_provider;

pub use env_provider::EnvTokenProvider;
pub use error::AuthError;
pub use file_provider::{
    default_config_dir, default_token_path, load_token, save_token, FileTokenProvider,
};

/// A source of the relay JWT used to authenticate to the relay's HTTP/3 API
/// (`ISEKAI_SSH_DESIGN.md` "シグナリングサーバー = relay の HTTP/3 API
/// （JWTベアラー認証）").
///
/// Synchronous by design — see the module-level docs for why this isn't
/// `async fn` yet.
pub trait TokenProvider {
    fn get_relay_jwt(&self) -> Result<String, AuthError>;
}
