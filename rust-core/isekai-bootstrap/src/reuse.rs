//! Helper deployment reuse (`isekai-ssh`'s long-lived-helper model,
//! distinct from `rust-core/src/helper_bootstrap.rs`'s Android per-session
//! model, which intentionally launches a fresh `isekai-pipe serve` on every
//! connect and has no equivalent need for any of this).
//!
//! `OpenSshBackend::install_and_start` deploys a helper meant to be reused
//! across many separate `isekai-ssh <destination>` invocations, potentially
//! hours or days apart (`RelayLaunchSpec::idle_lifetime_secs`'s docs). Before
//! this module existed, every invocation that didn't find a *local* trust
//! store entry (a lost/stale client-side cache — the `LOCALAPPDATA`/`HOME`
//! ordering fix in `isekai-pipe-core::profile::default_profiles_dir` is one
//! concrete way that happens) unconditionally re-uploaded the binary and
//! launched a brand-new detached `isekai-pipe serve` process without ever
//! stopping whatever it had deployed earlier — which, given the 30-day
//! default idle lifetime, orphans a pileup of long-lived helper processes on
//! the remote host every time the client's own bookkeeping gets confused.
//!
//! The fix checks the remote host directly rather than trusting only local
//! state: `openssh.rs`'s combined install script records `{pid, fingerprint,
//! <raw handshake envelope>}` to [`state_file_path`] (colocated with the
//! binary, 0600, guarded by an flock on [`lock_file_path`]) every time it
//! successfully launches a helper. A later invocation first checks whether
//! that pid is still alive *and* is genuinely still running the expected
//! binary (`/proc/<pid>/exe`, guarding against PID reuse) *and* was launched
//! with a matching [`launch_fingerprint`] — only then does it trust the
//! recorded handshake and skip uploading/relaunching entirely. This doesn't
//! weaken the security review #57/#58 decision to never persist
//! `session_secret`/`relay_jwt` in a *shared* or *argv-visible* location: the
//! state file is per-deploying-user (0600) and colocated with a binary path
//! only that same user can already write to — identical to the trust
//! boundary `~/.ssh/id_rsa` already relies on, not a new one.

use sha2::{Digest, Sha256};

use crate::types::LaunchSpec;

/// A stable digest of the *topology-affecting* parts of a [`LaunchSpec`]:
/// enough to tell "would relaunching with these arguments produce a helper
/// reachable the same way the currently-running one is" apart from "this is
/// a materially different deployment (different relay, different launch
/// mode) that must supersede the old one". Deliberately excludes
/// `remote_log_level`/`idle_lifetime_secs`/`remote_bind_port_range` — none of
/// them change whether an already-running helper can serve this connection,
/// only how verbosely/long it runs or which port a *fresh* launch would
/// pick, so a bare settings tweak must not force an unnecessary relaunch
/// (and thereby drop whatever peer is using the still-good existing
/// connection).
pub(crate) fn launch_fingerprint(launch: &LaunchSpec) -> String {
    let discriminator = match launch {
        LaunchSpec::Direct { .. } => "direct".to_string(),
        LaunchSpec::Relay(relay) => {
            format!("relay:{}:{}:{:?}", relay.relay_addr, relay.relay_sni, relay.relay_transport)
        }
    };
    let digest = Sha256::digest(discriminator.as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// Where the combined install script records `{pid, fingerprint, <raw
/// handshake envelope>}` after a successful launch — colocated with the
/// binary itself (same directory, so it inherits that directory's
/// permissions/ownership) rather than under a separate shared state tree,
/// deliberately scoping reuse to "the currently-deployed binary at this
/// exact path" (a caller-supplied `#@isekai remote-path`/`--isekai-helper-binary`
/// pointing elsewhere is, correctly, a different deployment with no
/// relationship to this one).
pub(crate) fn state_file_path(remote_binary_path: &str) -> String {
    format!("{remote_binary_path}.state")
}

/// Advisory-lock path guarding read-modify-write access to
/// [`state_file_path`] (best-effort `flock(1)` — see `openssh.rs`'s install
/// script for what happens when `flock(1)` itself isn't available).
pub(crate) fn lock_file_path(remote_binary_path: &str) -> String {
    format!("{remote_binary_path}.lock")
}

/// Where the install script writes the freshly-launched helper's own pid —
/// a separate small file (rather than smuggling it into the strictly
/// one-line-of-JSON handshake stdout) purely so it survives outside the
/// per-invocation `mktemp -d` scratch directory that gets `rm -rf`'d when
/// the launching shell exits.
pub(crate) fn pid_file_path(remote_binary_path: &str) -> String {
    format!("{remote_binary_path}.pid")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::RelayTransportKind;

    fn direct(remote_bind_port_range: Option<(u16, u16)>) -> LaunchSpec {
        LaunchSpec::Direct {
            idle_lifetime_secs: 2_592_000,
            remote_log_level: "info".to_string(),
            remote_bind_port_range,
        }
    }

    fn relay(relay_addr: &str, relay_sni: &str, relay_transport: RelayTransportKind) -> LaunchSpec {
        LaunchSpec::Relay(crate::types::RelayLaunchSpec {
            relay_addr: relay_addr.parse().unwrap(),
            relay_sni: relay_sni.to_string(),
            relay_jwt: "some-jwt".to_string(),
            relay_transport,
            idle_lifetime_secs: 2_592_000,
            remote_log_level: "info".to_string(),
        })
    }

    #[test]
    fn same_launch_spec_yields_the_same_fingerprint() {
        assert_eq!(launch_fingerprint(&direct(None)), launch_fingerprint(&direct(None)));
        assert_eq!(
            launch_fingerprint(&relay("203.0.113.10:443", "relay.example.com", RelayTransportKind::Udp)),
            launch_fingerprint(&relay("203.0.113.10:443", "relay.example.com", RelayTransportKind::Udp)),
        );
    }

    #[test]
    fn direct_and_relay_always_differ() {
        assert_ne!(
            launch_fingerprint(&direct(None)),
            launch_fingerprint(&relay("203.0.113.10:443", "relay.example.com", RelayTransportKind::Udp)),
        );
    }

    #[test]
    fn different_relay_addr_changes_the_fingerprint() {
        assert_ne!(
            launch_fingerprint(&relay("203.0.113.10:443", "relay.example.com", RelayTransportKind::Udp)),
            launch_fingerprint(&relay("203.0.113.11:443", "relay.example.com", RelayTransportKind::Udp)),
        );
    }

    #[test]
    fn different_relay_sni_changes_the_fingerprint() {
        assert_ne!(
            launch_fingerprint(&relay("203.0.113.10:443", "relay.example.com", RelayTransportKind::Udp)),
            launch_fingerprint(&relay("203.0.113.10:443", "other-relay.example.com", RelayTransportKind::Udp)),
        );
    }

    #[test]
    fn different_relay_transport_changes_the_fingerprint() {
        assert_ne!(
            launch_fingerprint(&relay("203.0.113.10:443", "relay.example.com", RelayTransportKind::Udp)),
            launch_fingerprint(&relay("203.0.113.10:443", "relay.example.com", RelayTransportKind::Qmux)),
        );
    }

    #[test]
    fn remote_log_level_does_not_affect_the_fingerprint() {
        let mut a = direct(None);
        let LaunchSpec::Direct { remote_log_level, .. } = &mut a else { unreachable!() };
        *remote_log_level = "debug".to_string();
        assert_eq!(launch_fingerprint(&a), launch_fingerprint(&direct(None)));
    }

    #[test]
    fn idle_lifetime_does_not_affect_the_fingerprint() {
        let mut a = direct(None);
        let LaunchSpec::Direct { idle_lifetime_secs, .. } = &mut a else { unreachable!() };
        *idle_lifetime_secs = 42;
        assert_eq!(launch_fingerprint(&a), launch_fingerprint(&direct(None)));
    }

    #[test]
    fn remote_bind_port_range_does_not_affect_the_fingerprint() {
        assert_eq!(launch_fingerprint(&direct(Some((40000, 40100)))), launch_fingerprint(&direct(None)));
    }

    #[test]
    fn state_lock_and_pid_paths_are_colocated_siblings_of_the_binary_path() {
        assert_eq!(state_file_path("~/.local/bin/isekai-pipe"), "~/.local/bin/isekai-pipe.state");
        assert_eq!(lock_file_path("~/.local/bin/isekai-pipe"), "~/.local/bin/isekai-pipe.lock");
        assert_eq!(pid_file_path("~/.local/bin/isekai-pipe"), "~/.local/bin/isekai-pipe.pid");
    }
}
