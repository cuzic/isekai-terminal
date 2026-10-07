//! Normalizes the many ways a user can spell an SSH connection target
//! (`myhost`, `myhost:22`, `user@myhost`, `user@myhost:2222`, ...) into the
//! single `host:port` form used as the trust store's map key
//! (`archive/ISEKAI_SSH_DESIGN.md` "キーの正規化"). Port defaults to 22 when
//! omitted; the username, if any, is dropped entirely — trust is scoped to
//! the (host, port) pair, not to who connects. `--via` (the jumphost) is a
//! separate, non-identity concept and is intentionally not part of this key
//! (see `schema::HelperTrust::last_via`).

use crate::error::TrustError;

/// Normalizes a raw SSH target spec into a `host:port` trust store key.
///
/// This is idempotent: normalizing an already-normalized `host:port` string
/// returns it unchanged.
///
/// IPv6 literals are accepted bare (`::1`, no port) or bracketed
/// (`[::1]` / `[::1]:2222`) and normalized to the bracketed form
/// (`[::1]:22`), so the key stays unambiguous. (A bare IPv6 literal used to
/// be split at its last `:` into a bogus host/port pair.)
///
/// Host names are deliberately **not** case-folded here even though DNS is
/// case-insensitive: this string is the key of already-persisted trust
/// entries, and changing it would silently orphan every existing entry for
/// a host spelled with capitals (forcing a fresh TOFU confirmation).
pub fn normalize_host_port(spec: &str) -> Result<String, TrustError> {
    let (host, port, _user) = split_user_host_port(spec)?;
    let port = port.unwrap_or(22);
    if host.contains(':') {
        Ok(format!("[{host}]:{port}"))
    } else {
        Ok(format!("{host}:{port}"))
    }
}

/// Tokenizes a `[user@]host[:port]` spec into its parts, without collapsing
/// a missing port to the default `22` (unlike `normalize_host_port`, which
/// is built on top of this and does that collapsing itself). Shared with
/// `isekai-ssh`'s `init` command (`init.rs`'s `parse_host_spec`/
/// `parse_jump_spec`), which needs `user`/`port` kept separate (as
/// `HostSpec`/`JumpSpec` want them) rather than collapsed into a single
/// normalized string.
pub fn split_user_host_port(spec: &str) -> Result<(String, Option<u16>, Option<String>), TrustError> {
    let spec = spec.trim();
    if spec.is_empty() {
        return Err(TrustError::EmptyHost);
    }

    // Drop a "user@" prefix. Usernames cannot contain '@', so splitting on
    // the last '@' is unambiguous.
    let (user, after_user) = match spec.rsplit_once('@') {
        Some((user, rest)) => (Some(user.to_string()), rest),
        None => (None, spec),
    };
    if after_user.is_empty() {
        return Err(TrustError::EmptyHost);
    }

    // `[v6]` / `[v6]:port`: the brackets delimit the literal; the returned
    // host is the bare address (what `ssh`/a socket address wants).
    if let Some(rest) = after_user.strip_prefix('[') {
        let Some((host, after)) = rest.split_once(']') else {
            return Err(TrustError::InvalidPort { spec: spec.to_string(), reason: "unterminated '[' in IPv6 literal".to_string() });
        };
        if host.is_empty() {
            return Err(TrustError::EmptyHost);
        }
        let port = match after {
            "" => None,
            _ => {
                let port_str = after.strip_prefix(':').ok_or_else(|| TrustError::InvalidPort {
                    spec: spec.to_string(),
                    reason: format!("unexpected {after:?} after IPv6 literal"),
                })?;
                Some(port_str.parse().map_err(|_| TrustError::InvalidPort {
                    spec: spec.to_string(),
                    reason: format!("{port_str:?} is not a valid port number"),
                })?)
            }
        };
        return Ok((host.to_string(), port, user));
    }
    // A bare IPv6 literal (two or more ':') has no port — its last ':' is
    // part of the address, not a port separator.
    if after_user.matches(':').count() >= 2 {
        return Ok((after_user.to_string(), None, user));
    }

    let (host, port) = match after_user.rsplit_once(':') {
        Some((host, port_str)) => {
            let port: u16 = port_str.parse().map_err(|_| TrustError::InvalidPort {
                spec: spec.to_string(),
                reason: format!("{port_str:?} is not a valid port number"),
            })?;
            (host, Some(port))
        }
        None => (after_user, None),
    };
    if host.is_empty() {
        return Err(TrustError::EmptyHost);
    }

    Ok((host.to_string(), port, user))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adds_default_port_when_missing() {
        assert_eq!(normalize_host_port("myhost").unwrap(), "myhost:22");
    }

    #[test]
    fn strips_username() {
        assert_eq!(normalize_host_port("user@myhost:2222").unwrap(), "myhost:2222");
    }

    #[test]
    fn strips_username_with_default_port() {
        assert_eq!(normalize_host_port("user@myhost").unwrap(), "myhost:22");
    }

    #[test]
    fn is_idempotent_on_already_normalized_input() {
        assert_eq!(normalize_host_port("myhost:22").unwrap(), "myhost:22");
        let once = normalize_host_port("user@myhost:2222").unwrap();
        let twice = normalize_host_port(&once).unwrap();
        assert_eq!(once, twice);
    }

    #[test]
    fn accepts_dotted_fqdn_and_ip() {
        assert_eq!(normalize_host_port("host.example.com").unwrap(), "host.example.com:22");
        assert_eq!(normalize_host_port("203.0.113.5:22").unwrap(), "203.0.113.5:22");
    }

    #[test]
    fn rejects_empty_spec() {
        assert!(matches!(normalize_host_port(""), Err(TrustError::EmptyHost)));
        assert!(matches!(normalize_host_port("   "), Err(TrustError::EmptyHost)));
    }

    #[test]
    fn rejects_empty_host_after_stripping_user() {
        assert!(matches!(normalize_host_port("user@"), Err(TrustError::EmptyHost)));
    }

    #[test]
    fn rejects_non_numeric_port() {
        let err = normalize_host_port("myhost:abc").unwrap_err();
        assert!(matches!(err, TrustError::InvalidPort { .. }));
    }

    #[test]
    fn split_keeps_user_and_port_separate() {
        assert_eq!(
            split_user_host_port("alice@myhost:2222").unwrap(),
            ("myhost".to_string(), Some(2222), Some("alice".to_string()))
        );
    }

    #[test]
    fn ipv6_literals_are_not_split_at_their_last_colon() {
        assert_eq!(normalize_host_port("::1").unwrap(), "[::1]:22");
        assert_eq!(normalize_host_port("2001:db8::5").unwrap(), "[2001:db8::5]:22");
        assert_eq!(normalize_host_port("[2001:db8::5]").unwrap(), "[2001:db8::5]:22");
        assert_eq!(normalize_host_port("alice@[2001:db8::5]:2222").unwrap(), "[2001:db8::5]:2222");
        assert_eq!(
            split_user_host_port("alice@[::1]:2222").unwrap(),
            ("::1".to_string(), Some(2222), Some("alice".to_string()))
        );
        // Idempotent on the normalized form.
        assert_eq!(normalize_host_port("[::1]:22").unwrap(), "[::1]:22");
        assert!(normalize_host_port("[::1").is_err());
        assert!(normalize_host_port("[::1]x").is_err());
    }

    #[test]
    fn split_leaves_port_none_when_missing() {
        assert_eq!(split_user_host_port("myhost").unwrap(), ("myhost".to_string(), None, None));
    }
}
