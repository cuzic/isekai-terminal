//! Trust store for `isekai-ssh`: tracks which `isekai-helper` instances have
//! been explicitly trusted for which SSH targets
//! (`archive/ISEKAI_SSH_DESIGN.md` "trust store のファイル形式").
//!
//! Unlike `isekai-protocol`, this crate performs real filesystem I/O
//! (reading/writing `~/.config/isekai-ssh/known_helpers.toml`), but it is
//! otherwise self-contained: no dependency on tokio/quinn/russh/uniffi/
//! isekai-protocol.
//!
//! Design invariants enforced here (all required by the design doc and by
//! this crate's task acceptance criteria):
//! - Writes are atomic (write to a sibling temp file, then `rename`;
//!   see `store::save_trust_store`).
//! - The store file and its parent directory must not be world-writable;
//!   loading/saving fails closed if they are (`store::check_not_world_writable`,
//!   private but exercised via `load_trust_store`/`save_trust_store`).
//! - Malformed TOML fails closed (`TrustError::Parse`, no silent fallback to
//!   an empty/default store).
//! - Unknown `update_policy` values fail closed (rejected by
//!   `schema::UpdatePolicy`'s `Deserialize` impl, surfaced as
//!   `TrustError::Parse`).
//!
//! Trust store keys are normalized `host:port` strings
//! (`normalize::normalize_host_port`); `--via` (jumphost) is recorded only
//! as the informational `HelperTrust::last_via` field, not as part of the
//! key.

pub mod error;
pub mod normalize;
pub mod schema;
pub mod store;

pub use error::TrustError;
pub use normalize::{normalize_host_port, split_user_host_port};
pub use schema::{HelperTrust, TrustStore, UpdatePolicy};
pub use store::{default_config_dir, default_trust_store_path, load_trust_store, save_trust_store};
