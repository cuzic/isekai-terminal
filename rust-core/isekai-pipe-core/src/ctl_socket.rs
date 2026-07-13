//! Shared conventions for the tmux-bypass control-plane's ctl socket
//! (`ISEKAI_PIPE_DESIGN.md` §8 Epic M): the remote-host
//! `/tmp/isekai-pipe-ctl-<128bit hex>.sock` naming, and the plain hex-token
//! generator it (and [`crate::ConnectionIntent::intent_id`]) both use.
//!
//! Previously reimplemented independently in three places — Android's own
//! `rust-core/src/transport.rs`, `isekai-ssh`'s `ctl_forward.rs`, and this
//! crate's own `new_intent_id` — which risked the naming convention
//! drifting apart between the client that creates a socket and the
//! `isekai-pipe serve`-side sweep (`sweep_stale_sockets`) that later
//! garbage-collects it.

use rand::RngCore as _;
use std::fmt::Write as _;

/// 128 bits of randomness as lowercase hex — the shared entropy/encoding
/// convention behind both [`crate::ConnectionIntent::intent_id`] and every
/// ctl socket token, so a squatting/guessing attacker cannot feasibly
/// pre-guess either.
pub fn new_hex_token_128() -> String {
    let mut bytes = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut bytes);
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Bare filename prefix (no directory) for ctl sockets. Kept separate from
/// [`CTL_SOCKET_DIR`] because [`crate::sweep_stale_sockets`] takes `dir`
/// and `prefix` as distinct arguments.
pub const CTL_SOCKET_FILENAME_PREFIX: &str = "isekai-pipe-ctl-";

/// The directory ctl sockets live under — `/tmp` rather than
/// `~/.cache/isekai-pipe/ctl/`, because `-R`'s remote path is handed to
/// `sshd` verbatim (no shell tilde-expansion), and resolving the remote
/// `$HOME` first would need an extra network round trip
/// (`isekai-ssh/src/ctl_forward.rs`'s module docs).
pub const CTL_SOCKET_DIR: &str = "/tmp";

/// `/tmp/isekai-pipe-ctl-<token>.sock` — the full remote-host path for a
/// ctl socket with the given (already-generated, see [`new_hex_token_128`])
/// token.
pub fn ctl_socket_remote_path(token: &str) -> String {
    format!("{CTL_SOCKET_DIR}/{CTL_SOCKET_FILENAME_PREFIX}{token}.sock")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_hex_token_128_is_32_lowercase_hex_chars() {
        let token = new_hex_token_128();
        assert_eq!(token.len(), 32);
        assert!(token.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }

    #[test]
    fn new_hex_token_128_is_not_trivially_predictable() {
        assert_ne!(new_hex_token_128(), new_hex_token_128());
    }

    #[test]
    fn ctl_socket_remote_path_matches_the_expected_shape() {
        let path = ctl_socket_remote_path("aaaa");
        assert_eq!(path, "/tmp/isekai-pipe-ctl-aaaa.sock");
    }
}
