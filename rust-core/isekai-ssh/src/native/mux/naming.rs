//! Derives the `local-ipc-mux` channel name for a resolved connection.
//!
//! Two `isekai-ssh <host>` tabs share an owner **iff** they hash to the same
//! channel name, so the name must be a function of the *entire* effective
//! connection configuration, not just the destination token. Any difference
//! in `User`/`Port`/`HostName`/`IdentityFile`/`ForwardAgent`/`IdentityAgent`/
//! `ProxyJump` — or in any connection-relevant `#@isekai` directive — must
//! land on a different name, so two genuinely-different configs never collide
//! onto one shared authenticated session.
//!
//! The name is `\\.\pipe\isekai-ssh-mux-<hex>` where `<hex>` is the SHA-256 of
//! a length-prefixed canonical serialization of those fields. Length-prefixing
//! every field is what makes the encoding injective: without it, `User=ab` +
//! `HostName=c` and `User=a` + `HostName=bc` would hash identically.
//!
//! **Fail-safe direction**: if the encoding ever changes between two binary
//! versions (e.g. a new field is added), the two versions compute *different*
//! names and simply don't share — each becomes its own owner. Over-isolation
//! is always safe here; wrong sharing is not. The protocol version handshake
//! ([`super::protocol::MUX_PROTOCOL_VERSION`]) is the backstop for the
//! remaining case where two differing versions did compute the same name.

use openssh_config::{ForwardAgent, HostConfig};
use sha2::{Digest, Sha256};

use crate::wrapper::WrapperResolution;

/// The Windows named-pipe name two matching invocations will contend for. On
/// non-Windows builds this is still computed and unit-tested (the string is
/// platform-independent); only the real pipe I/O is Windows-only.
pub fn channel_name(host_config: &HostConfig, resolution: &WrapperResolution, destination: &str) -> String {
    let mut hasher = Sha256::new();

    // Domain/scheme separator: bump the suffix if the *set of fields* hashed
    // below ever changes in a way that should force a clean re-election
    // rather than relying on incidental hash divergence.
    hash_field(&mut hasher, b"isekai-ssh-mux-v1");

    // OpenSSH-resolved transport identity, taken from the `HostConfig` itself
    // (the authoritative resolved config) with the same HostName/port fallback
    // the connect path uses — `resolution.native_host_port` derives these from
    // the very same fields, so this agrees with it in production.
    let host = host_config.host_name.clone().unwrap_or_else(|| destination.to_string());
    let port = host_config.port.unwrap_or(22);
    hash_field(&mut hasher, host.as_bytes());
    hash_field(&mut hasher, &port.to_be_bytes());
    hash_opt(&mut hasher, host_config.user.as_deref().map(str::as_bytes));

    // IdentityFile is an ordered accumulation (later config blocks add
    // candidates), so order is significant and hashed with a count prefix.
    hash_field(&mut hasher, &(host_config.identity_file.len() as u64).to_be_bytes());
    for id in &host_config.identity_file {
        hash_field(&mut hasher, id.as_os_str().as_encoded_bytes());
    }

    hash_field(&mut hasher, &[forward_agent_tag(host_config.forward_agent.as_ref())]);
    if let Some(ForwardAgent::Socket(s)) = host_config.forward_agent.as_ref() {
        hash_field(&mut hasher, s.as_bytes());
    }
    hash_opt(&mut hasher, host_config.identity_agent.as_ref().map(|p| p.as_os_str().as_encoded_bytes()));
    hash_opt(&mut hasher, host_config.proxy_jump.as_deref().map(str::as_bytes));

    // Connection-relevant `#@isekai` directives (profile name, relay/route
    // config, bootstrap policy, …). See `WrapperResolution::mux_identity_material`.
    hash_field(&mut hasher, resolution.mux_identity_material().as_bytes());

    format!(r"\\.\pipe\isekai-ssh-mux-{}", to_hex(&hasher.finalize()))
}

/// The auth-token file name for a given channel: the pipe's leaf name plus a
/// `.token` suffix, so the token file sits alongside nothing else and is
/// uniquely tied to this exact channel. The caller joins it onto the runtime
/// dir (which lives under the user profile).
pub fn token_file_name(channel_name: &str) -> String {
    // The pipe name is `\\.\pipe\isekai-ssh-mux-<hex>`; take the leaf after the
    // last backslash so the token file name carries no path separators.
    let leaf = channel_name.rsplit('\\').next().unwrap_or(channel_name);
    format!("{leaf}.token")
}

/// The spawn-lock file name for a given channel (see `mux::mod::SpawnLock`):
/// a best-effort cross-process mutex so at most one tab resolves the
/// passphrase hand-off and spawns a detached holder for this destination at a
/// time. Same leaf-name convention as [`token_file_name`], distinct suffix so
/// the two never collide.
pub fn spawn_lock_file_name(channel_name: &str) -> String {
    let leaf = channel_name.rsplit('\\').next().unwrap_or(channel_name);
    format!("{leaf}.spawning.lock")
}

/// Writes a length-prefixed field into the hasher so field boundaries are
/// unambiguous (the injectivity property the module docs rely on).
fn hash_field(hasher: &mut Sha256, bytes: &[u8]) {
    hasher.update((bytes.len() as u64).to_be_bytes());
    hasher.update(bytes);
}

/// Hashes an optional field with a one-byte present/absent discriminant, so
/// `Some("")` and `None` are distinct.
fn hash_opt(hasher: &mut Sha256, value: Option<&[u8]>) {
    match value {
        Some(bytes) => {
            hasher.update([1u8]);
            hash_field(hasher, bytes);
        }
        None => hasher.update([0u8]),
    }
}

fn forward_agent_tag(fa: Option<&ForwardAgent>) -> u8 {
    match fa {
        None => 0,
        Some(ForwardAgent::No) => 1,
        Some(ForwardAgent::Yes) => 2,
        Some(ForwardAgent::Socket(_)) => 3,
    }
}

fn to_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Resolves a throwaway destination through the real parser so the test
    /// exercises the same `WrapperResolution` the connect path builds.
    fn resolve(destination: &str) -> (crate::wrapper::WrapperResolution, HostConfig) {
        let plan = crate::wrapper::parse_wrapper(vec![destination.to_string()]).unwrap();
        crate::wrapper::resolve_for_native(&plan).unwrap()
    }

    #[test]
    fn the_name_is_a_valid_windows_pipe_path() {
        let (resolution, host_config) = resolve("naming-test-host");
        let name = channel_name(&host_config, &resolution, "naming-test-host");
        assert!(name.starts_with(r"\\.\pipe\isekai-ssh-mux-"), "must be a \\\\.\\pipe\\ name, got {name}");
        // sha256 hex is 64 chars.
        assert_eq!(name.rsplit('-').next().unwrap().len(), 64, "hash suffix must be a full sha256 hex digest");
    }

    #[test]
    fn identical_config_hashes_to_the_same_name() {
        let (r1, h1) = resolve("stable-host");
        let (r2, h2) = resolve("stable-host");
        assert_eq!(channel_name(&h1, &r1, "stable-host"), channel_name(&h2, &r2, "stable-host"));
    }

    #[test]
    fn a_different_destination_hashes_differently() {
        let (r1, h1) = resolve("host-a");
        let (r2, h2) = resolve("host-b");
        assert_ne!(channel_name(&h1, &r1, "host-a"), channel_name(&h2, &r2, "host-b"));
    }

    // Each of the following mutates exactly one connection-relevant field of
    // an otherwise-identical HostConfig and asserts the channel name moves —
    // the "must never collide" property, field by field.

    fn base() -> (crate::wrapper::WrapperResolution, HostConfig) {
        resolve("field-test-host")
    }

    fn name_with(mutate: impl FnOnce(&mut HostConfig)) -> String {
        let (resolution, mut host_config) = base();
        mutate(&mut host_config);
        channel_name(&host_config, &resolution, "field-test-host")
    }

    #[test]
    fn user_difference_changes_the_name() {
        let a = name_with(|h| h.user = Some("alice".to_string()));
        let b = name_with(|h| h.user = Some("bob".to_string()));
        assert_ne!(a, b, "a different User must never share an owner");
    }

    #[test]
    fn port_difference_changes_the_name() {
        let a = name_with(|h| h.port = Some(22));
        let b = name_with(|h| h.port = Some(2222));
        assert_ne!(a, b, "a different Port must never share an owner");
    }

    #[test]
    fn hostname_difference_changes_the_name() {
        let a = name_with(|h| h.host_name = Some("10.0.0.1".to_string()));
        let b = name_with(|h| h.host_name = Some("10.0.0.2".to_string()));
        assert_ne!(a, b, "a different HostName must never share an owner");
    }

    #[test]
    fn identity_file_difference_changes_the_name() {
        let a = name_with(|h| h.identity_file = vec![PathBuf::from("/home/u/.ssh/id_ed25519")]);
        let b = name_with(|h| h.identity_file = vec![PathBuf::from("/home/u/.ssh/id_rsa")]);
        assert_ne!(a, b, "a different IdentityFile must never share an owner");
    }

    #[test]
    fn identity_file_order_is_significant() {
        let a = name_with(|h| h.identity_file = vec![PathBuf::from("/a"), PathBuf::from("/b")]);
        let b = name_with(|h| h.identity_file = vec![PathBuf::from("/b"), PathBuf::from("/a")]);
        assert_ne!(a, b, "IdentityFile order is significant (later entries are added candidates)");
    }

    #[test]
    fn forward_agent_difference_changes_the_name() {
        let yes = name_with(|h| h.forward_agent = Some(ForwardAgent::Yes));
        let no = name_with(|h| h.forward_agent = Some(ForwardAgent::No));
        let socket = name_with(|h| h.forward_agent = Some(ForwardAgent::Socket("/tmp/a.sock".to_string())));
        assert_ne!(yes, no, "ForwardAgent yes vs no must differ");
        assert_ne!(yes, socket, "ForwardAgent yes vs socket must differ");
        assert_ne!(no, socket, "ForwardAgent no vs socket must differ");
    }

    #[test]
    fn identity_agent_difference_changes_the_name() {
        let a = name_with(|h| h.identity_agent = Some(PathBuf::from("/tmp/agent-a.sock")));
        let b = name_with(|h| h.identity_agent = Some(PathBuf::from("/tmp/agent-b.sock")));
        assert_ne!(a, b, "a different IdentityAgent must never share an owner");
    }

    #[test]
    fn proxy_jump_difference_changes_the_name() {
        let a = name_with(|h| h.proxy_jump = Some("jump-a".to_string()));
        let b = name_with(|h| h.proxy_jump = Some("jump-b".to_string()));
        assert_ne!(a, b, "a different ProxyJump must never share an owner");
    }

    #[test]
    fn token_file_name_is_the_pipe_leaf_plus_suffix() {
        let name = r"\\.\pipe\isekai-ssh-mux-abcdef";
        assert_eq!(token_file_name(name), "isekai-ssh-mux-abcdef.token");
        assert!(!token_file_name(name).contains('\\'), "the token file name must not contain path separators");
    }

    #[test]
    fn some_empty_and_none_are_distinguished() {
        let some_empty = name_with(|h| h.user = Some(String::new()));
        let none = name_with(|h| h.user = None);
        assert_ne!(some_empty, none, "Some(\"\") and None must not collide");
    }
}
