//! Normalizes the many ways a user can spell an SSH connection target
//! (`myhost`, `myhost:22`, `user@myhost`, `user@myhost:2222`, ...) into the
//! single `host:port` form used as the trust store's map key
//! (`ISEKAI_SSH_DESIGN.md` "キーの正規化"). Port defaults to 22 when
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
/// Note: this does not special-case bracketed IPv6 literals
/// (`[::1]:22`) — that is out of scope for the current MVP and not
/// exercised by isekai-ssh's target host list yet.
pub fn normalize_host_port(spec: &str) -> Result<String, TrustError> {
    let (host, port, _user) = split_user_host_port(spec)?;
    Ok(format!("{host}:{}", port.unwrap_or(22)))
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
    fn split_leaves_port_none_when_missing() {
        assert_eq!(split_user_host_port("myhost").unwrap(), ("myhost".to_string(), None, None));
    }
}
