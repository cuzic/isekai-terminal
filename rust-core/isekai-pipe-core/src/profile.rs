//! Persistent profile schema (`chatgpt.md` §13 "永続profile") -- the sole
//! on-disk store `isekai-ssh`/`isekai-pipe` read and write
//! (`ISEKAI_PIPE_DESIGN.md` §8 Epic B). The legacy `known_helpers.toml`
//! trust store (`isekai_trust::schema::{TrustStore, HelperTrust}`) is no
//! longer read by any live code path -- `migrate_legacy_helper_trust`/
//! `migrate_trust_store` below remain only as one-off conversion helpers for
//! a caller that still has an old `known_helpers.toml` lying around and
//! wants to hand-convert it.
//!
//! `known_helpers.toml` was keyed by `host:port` and stored a single cached
//! relay address/session secret per entry, plus release-trust metadata
//! (`identity_pubkey`/`trusted_helper_sha256`/`update_policy`/
//! `release_channel`/`last_via`/`last_seen_at`/`cached_stun_observed_addr`).
//! `PersistentProfile` carries all of that (see the fields below) so cutting
//! over from `HelperTrust` loses nothing a live code path used.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use isekai_trust::schema::{HelperTrust, TrustStore, UpdatePolicy};

use crate::{IntentTransport, RelayPolicy, ServerIdentity};

/// Bumped from `1` to `2` (`ISEKAI_PIPE_DESIGN.md` §8 Epic I's "codexが
///指摘したが未対応のまま残している点") as a forward-looking safety marker,
/// *not* because the on-disk shape changed here — every real `1`-tagged
/// file any live code path has ever written already has today's shape
/// (`identity_pubkey`/etc. were added as required fields under the `1` tag
/// itself, before this bump; no live path ever wrote the older,
/// `identity_pubkey`-less shape that number would otherwise imply). `2`
/// exists so a *future* shape change has an unambiguous version to gate on;
/// `load_persistent_profile` intentionally does not reject `1` here — doing
/// so would break every already-written real profile over a label that was
/// never actually wrong for its contents.
pub const PERSISTENT_PROFILE_SCHEMA_VERSION: u32 = 2;

/// `chatgpt.md` §13's persistent profile document, one per logical host.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PersistentProfile {
    pub schema_version: u32,
    pub profile: String,
    /// Not tracked by the legacy `known_helpers.toml` schema -- always
    /// `None` for a migrated profile until a real ISEKAI-link/rendezvous
    /// peer identity is assigned (`chatgpt.md` §4 `PeerId`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer_id: Option<String>,
    pub server_identity: ServerIdentity,
    pub service: String,
    #[serde(default)]
    pub link_endpoints: Vec<String>,
    #[serde(default)]
    pub rendezvous: Vec<String>,
    #[serde(default)]
    pub stun_servers: Vec<String>,
    #[serde(default)]
    pub relay_endpoints: Vec<String>,
    pub relay_policy: RelayPolicy,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_bootstrap_at: Option<String>,
    /// Not tracked by the legacy schema -- always `None` for a migrated
    /// profile (`chatgpt.md` §13's `last_path_hint`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_path_hint: Option<PathHint>,
    /// Bridges back to the single-cached-relay-address transport that
    /// `known_helpers.toml`-backed connects still use today
    /// (`isekai-ssh::wrapper::build_connection_intent`,
    /// `isekai-pipe::intent_from_profile`). Present only for profiles
    /// migrated from (or still mastered by) a `known_helpers.toml` entry;
    /// a profile created after real candidate-source exchange
    /// (`chatgpt.md` §17-20) replaces this with populated
    /// `link_endpoints`/`stun_servers`/`relay_endpoints` and leaves this
    /// `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub legacy_relay_transport: Option<LegacyRelayTransport>,
    /// The deployed `isekai-pipe serve` binary's own identity, and the
    /// digest pin used to decide whether a re-deployed binary may be
    /// accepted without re-running `init` (`isekai_trust::schema::HelperTrust`'s
    /// former job -- `UpdatePolicy::ExactDigestOnly` is not consulted for
    /// any policy decision yet, matching that type's own doc comment, but
    /// the pin is still recorded here so Epic D's signature verification
    /// has something to build on rather than starting from nothing).
    pub identity_pubkey: String,
    pub trusted_helper_sha256: String,
    pub update_policy: UpdatePolicy,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_channel: Option<String>,
    /// The jump host last used to reach this destination -- purely
    /// informational, not part of the profile's identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_via: Option<String>,
    pub last_seen_at: String,
    /// `HandshakeJson::stun_observed_addr()` from the most recent successful
    /// handshake, if the remote reported one. See `HelperTrust`'s own field
    /// of the same name for why this is a cache of server-side self
    /// observation, not a connection decision made here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_stun_observed_addr: Option<String>,
}

/// `chatgpt.md` §13's `last_path_hint`: a short-lived observation used only
/// to bias the next candidate search, never a permanent connection target.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PathHint {
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
}

/// The one piece of `known_helpers.toml` (`HelperTrust`) that current
/// `connect` code paths actually dial: a single relay-reachable helper
/// address plus the session secret needed for the HELLO/proof handshake.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LegacyRelayTransport {
    pub helper_addr: String,
    pub session_secret_b64: String,
}

impl PersistentProfile {
    /// Converts a `HelperTrust` value (freshly built from a bootstrap
    /// handshake, or -- for a one-off conversion tool -- loaded from an old
    /// `known_helpers.toml`) into a `PersistentProfile`. Fields the legacy
    /// schema has no equivalent for at all (`peer_id`, `link_endpoints`,
    /// `rendezvous`, `stun_servers`, `relay_endpoints`, `last_path_hint`)
    /// are left empty/`None` rather than guessed; every other field
    /// (including the release-trust metadata `HelperTrust` carries) is
    /// preserved.
    pub fn migrate_legacy_helper_trust(profile_name: &str, trust: &HelperTrust) -> Self {
        Self {
            schema_version: PERSISTENT_PROFILE_SCHEMA_VERSION,
            profile: profile_name.to_string(),
            peer_id: None,
            server_identity: ServerIdentity {
                cert_sha256_hex: trust.cached_cert_sha256.clone(),
            },
            service: "ssh".to_string(),
            link_endpoints: Vec::new(),
            rendezvous: Vec::new(),
            stun_servers: Vec::new(),
            relay_endpoints: Vec::new(),
            relay_policy: RelayPolicy::RelayAllowed,
            remote_version: Some(trust.trusted_helper_version.clone()),
            last_bootstrap_at: Some(trust.trusted_at.clone()),
            last_path_hint: None,
            legacy_relay_transport: Some(LegacyRelayTransport {
                helper_addr: trust.cached_relay_addr.clone(),
                session_secret_b64: trust.cached_session_secret.clone(),
            }),
            identity_pubkey: trust.identity_pubkey.clone(),
            trusted_helper_sha256: trust.trusted_helper_sha256.clone(),
            update_policy: trust.update_policy,
            release_channel: trust.release_channel.clone(),
            last_via: trust.last_via.clone(),
            last_seen_at: trust.last_seen_at.clone(),
            cached_stun_observed_addr: trust.cached_stun_observed_addr.clone(),
        }
    }

    /// Reconstructs the `IntentTransport::Relay` that current `connect`
    /// code paths need, when this profile still carries a
    /// `legacy_relay_transport` bridge. Returns `None` once a profile has
    /// moved past the single-cached-relay-address model (no bridge left to
    /// reconstruct from).
    pub fn to_legacy_relay_transport(&self) -> Option<IntentTransport> {
        self.legacy_relay_transport
            .as_ref()
            .map(|legacy| IntentTransport::Relay {
                helper_addr: legacy.helper_addr.clone(),
                server_name: "isekai-helper".to_string(),
                session_secret_b64: legacy.session_secret_b64.clone(),
            })
    }
}

/// Migrates every entry in a loaded `known_helpers.toml` document. Pure
/// (no I/O): callers load the `TrustStore` with `isekai_trust::load_trust_store`
/// as they already do, and separately decide whether/where to persist the
/// result (`write_persistent_profile` below, or nothing at all for a
/// dry-run/inspection use).
pub fn migrate_trust_store(store: &TrustStore) -> Vec<PersistentProfile> {
    store
        .helpers
        .iter()
        .map(|(key, trust)| PersistentProfile::migrate_legacy_helper_trust(key, trust))
        .collect()
}

/// `chatgpt.md` §33's `state/profiles/` layout, rooted the same way
/// `default_runtime_dir` roots `runtime/` (an env override, else a
/// platform-conventional per-user directory).
///
/// On Windows, `LOCALAPPDATA` is checked *before* `HOME` even though `HOME`
/// isn't Windows-native at all: MSYS2/Git-Bash/WSL-style shells set `HOME`
/// on top of the very same Windows binary that a plain `cmd.exe`/PowerShell
/// session runs with `HOME` unset, so ordering by "whichever shell happened
/// to define it" makes a "trusted once" host land in a *different*
/// `PersistentProfile` directory depending only on which shell the user
/// happened to launch `isekai-ssh`/`isekai-pipe` from — invisible to the
/// user and indistinguishable from the profile having been lost. Checking
/// the OS (`cfg!(windows)`) rather than the environment makes the resolved
/// directory a property of the binary, not of the invoking shell, so both
/// shells agree. `LOCALAPPDATA` (not the roaming `APPDATA`) is deliberately
/// used: this store's cached `session_secret`/cert pin are tied to a
/// specific machine's bootstrap, and roaming them into a domain profile that
/// syncs across machines would reintroduce the same "which copy is current"
/// confusion this ordering fix removes. `HOME`-based resolution is left
/// untouched for actual Unix targets (Linux/macOS), and remains the correct
/// non-Windows-fallback branch for cross-compiled/unusual Windows-adjacent
/// targets that don't set `LOCALAPPDATA` either.
pub fn default_profiles_dir() -> io::Result<PathBuf> {
    resolve_profiles_dir(
        std::env::var_os("ISEKAI_PIPE_PROFILES_DIR"),
        std::env::var_os("XDG_STATE_HOME"),
        // Folded in here (rather than checked inside `resolve_profiles_dir`)
        // so that function stays a pure, OS-agnostic priority list —
        // `cfg!(windows)` is a compile-time constant per build target, which
        // would make a Windows-only branch untestable on a non-Windows CI
        // runner; passing `None` here for a non-Windows build has the exact
        // same effect and needs no `cfg`-gating in the tests below.
        cfg!(windows).then(|| std::env::var_os("LOCALAPPDATA")).flatten(),
        std::env::var_os("HOME"),
    )
}

/// Pure priority list behind [`default_profiles_dir`], split out so every
/// branch (including the Windows-only `LOCALAPPDATA` one) is unit-testable
/// without mutating this process's real environment variables or depending
/// on which OS the test happens to run on.
fn resolve_profiles_dir(
    explicit_override: Option<std::ffi::OsString>,
    xdg_state_home: Option<std::ffi::OsString>,
    windows_local_app_data: Option<std::ffi::OsString>,
    home: Option<std::ffi::OsString>,
) -> io::Result<PathBuf> {
    if let Some(path) = explicit_override {
        return Ok(PathBuf::from(path));
    }
    if let Some(path) = xdg_state_home {
        return Ok(PathBuf::from(path).join("isekai").join("profiles"));
    }
    if let Some(local_app_data) = windows_local_app_data {
        return Ok(PathBuf::from(local_app_data).join("isekai").join("profiles"));
    }
    if let Some(home) = home {
        return Ok(PathBuf::from(home)
            .join(".local")
            .join("state")
            .join("isekai")
            .join("profiles"));
    }
    Ok(std::env::temp_dir().join("isekai-profiles"))
}

/// Default destination for the always-on verbose diagnostic log that
/// `isekai-ssh`/`isekai-pipe connect` write to by default (no
/// `--isekai-log-file` needed) -- distinct from `default_profiles_dir`'s
/// per-host trust store, but resolved with the exact same env var / XDG /
/// `LOCALAPPDATA` / `HOME` priority so both land under the same parent
/// `isekai` directory. `ISEKAI_PIPE_LOG_FILE` doubles as both an explicit
/// user override *and* the mechanism `isekai-ssh` uses to tell a spawned
/// `isekai-pipe connect` where to write its own diagnostic log (same
/// convention as `ISEKAI_INTENT_ID`/`ISEKAI_PIPE_RUNTIME_DIR`).
pub fn default_log_file() -> io::Result<PathBuf> {
    resolve_log_file(
        std::env::var_os("ISEKAI_PIPE_LOG_FILE"),
        std::env::var_os("XDG_STATE_HOME"),
        cfg!(windows).then(|| std::env::var_os("LOCALAPPDATA")).flatten(),
        std::env::var_os("HOME"),
    )
}

/// Pure priority list behind [`default_log_file`], mirroring
/// `resolve_profiles_dir`'s structure so it stays unit-testable without
/// mutating this process's real environment variables.
fn resolve_log_file(
    explicit_override: Option<std::ffi::OsString>,
    xdg_state_home: Option<std::ffi::OsString>,
    windows_local_app_data: Option<std::ffi::OsString>,
    home: Option<std::ffi::OsString>,
) -> io::Result<PathBuf> {
    if let Some(path) = explicit_override {
        return Ok(PathBuf::from(path));
    }
    if let Some(path) = xdg_state_home {
        return Ok(PathBuf::from(path).join("isekai").join("logs").join("isekai-ssh.log"));
    }
    if let Some(local_app_data) = windows_local_app_data {
        return Ok(PathBuf::from(local_app_data).join("isekai").join("logs").join("isekai-ssh.log"));
    }
    if let Some(home) = home {
        return Ok(PathBuf::from(home)
            .join(".local")
            .join("state")
            .join("isekai")
            .join("logs")
            .join("isekai-ssh.log"));
    }
    Ok(std::env::temp_dir().join("isekai-logs").join("isekai-ssh.log"))
}

/// Escapes characters that are reserved in Windows filenames -- most
/// notably `:`, which NTFS interprets as the Alternate Data Stream
/// separator. Profile keys are `host:port` (`isekai_trust::normalize_host_port`),
/// so writing `<key>.json` unescaped turns the on-disk name into
/// `<host>:<port>.json`, i.e. a `<port>.json` *stream* on a `<host>` base
/// file; `fs::rename`'s underlying `MoveFileEx` call then fails with
/// `ERROR_INVALID_PARAMETER` (os error 87) because it can't rename across
/// streams that way. Escaping keeps the on-disk name a single real
/// filename on every platform; nothing reconstructs a key by parsing a
/// filename back (callers always pass the key explicitly), so this only
/// needs to be unambiguous, not reversible.
fn sanitize_filename_component(key: &str) -> String {
    key.chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => format!("%{:02X}", c as u32),
            c if (c as u32) < 0x20 => format!("%{:02X}", c as u32),
            c => c.to_string(),
        })
        .collect()
}

/// Writes `profile` to `<dir>/<profile.profile>.json` (filename-escaped via
/// [`sanitize_filename_component`]), atomically (write to a sibling temp
/// file, then rename) and with owner-only permissions, mirroring
/// `write_connection_intent`'s approach in `lib.rs`.
pub fn write_persistent_profile(dir: &Path, profile: &PersistentProfile) -> io::Result<PathBuf> {
    fs::create_dir_all(dir)?;
    isekai_fs_guard::set_private_dir_permissions(dir).map_err(fs_guard_err_to_io)?;
    let filename_key = sanitize_filename_component(&profile.profile);
    let path = dir.join(format!("{filename_key}.json"));
    let tmp = dir.join(format!("{filename_key}.{}.tmp", std::process::id()));
    let bytes = serde_json::to_vec_pretty(profile)?;
    fs::write(&tmp, &bytes)?;
    isekai_fs_guard::set_private_file_permissions(&tmp).map_err(fs_guard_err_to_io)?;
    fs::rename(&tmp, &path)?;
    Ok(path)
}

/// `isekai_fs_guard::FsGuardError` is deliberately path-less (see its own
/// docs); this module doesn't have a richer path-carrying error type of its
/// own the way `isekai-trust`/`isekai-auth` do, so it's flattened straight
/// into `io::Error` here instead.
fn fs_guard_err_to_io(err: isekai_fs_guard::FsGuardError) -> io::Error {
    use isekai_fs_guard::FsGuardError;
    match err {
        FsGuardError::CreateDir(e) | FsGuardError::Stat(e) | FsGuardError::SetPermissions(e) => e,
        FsGuardError::WorldWritable { mode } => {
            io::Error::other(format!("path is world-writable (mode {mode:o})"))
        }
        FsGuardError::InsecureAcl { principal, rights } => {
            io::Error::other(format!("path grants write access to {principal} (rights {rights})"))
        }
    }
}

/// Holds an exclusive advisory lock (`flock(2)`, `LOCK_EX`) on
/// `<dir>/<key>.lock` for its lifetime — scoped per profile key, so
/// concurrent updates to *different* profiles never block each other.
/// `flock` is held by the open file description, and is released
/// automatically when the underlying fd is closed (this struct's `Drop`),
/// even if the holding process crashes or is killed — no separate cleanup
/// step is needed, unlike a lockfile whose mere *existence* signals
/// ownership (`ISEKAI_PIPE_DESIGN.md` §8 Epic A: "複数`isekai-ssh`プロセス間の
/// 競合が起き得る箇所は...排他制御を入れる").
#[cfg(unix)]
struct ProfileLock {
    _file: fs::File,
}

#[cfg(unix)]
impl ProfileLock {
    fn acquire(dir: &Path, key: &str) -> io::Result<Self> {
        use std::os::unix::io::AsRawFd;

        fs::create_dir_all(dir)?;
        let lock_path = dir.join(format!("{key}.lock"));
        let file = fs::OpenOptions::new().create(true).write(true).open(&lock_path)?;
        // SAFETY: `file.as_raw_fd()` is a valid, open fd for the duration of
        // this call (the `File` outlives it), matching `flock(2)`'s contract.
        let ret = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
        if ret != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { _file: file })
    }
}

/// Windows counterpart of the `flock(2)`-based lock above: `LockFileEx`
/// with `LOCKFILE_EXCLUSIVE_LOCK` (and no `LOCKFILE_FAIL_IMMEDIATELY`, so it
/// blocks until acquired, matching `LOCK_EX`'s semantics) on the same
/// handle the file was opened with. The lock is released when the handle
/// closes (`File`'s `Drop`) — even on a crash — the same "no separate
/// cleanup step" property `flock` has.
///
/// **Not verified against a real Windows machine** — see
/// `isekai-fs-guard`'s `windows_acl.rs` module docs for what verification
/// (`cargo check --target x86_64-pc-windows-gnu`) has and hasn't been done;
/// the same caveat applies here.
#[cfg(windows)]
struct ProfileLock {
    _file: fs::File,
}

#[cfg(windows)]
impl ProfileLock {
    fn acquire(dir: &Path, key: &str) -> io::Result<Self> {
        use std::os::windows::io::AsRawHandle;

        use windows::Win32::Foundation::HANDLE;
        use windows::Win32::Storage::FileSystem::{LockFileEx, LOCKFILE_EXCLUSIVE_LOCK};
        use windows::Win32::System::IO::OVERLAPPED;

        fs::create_dir_all(dir)?;
        let lock_path = dir.join(format!("{key}.lock"));
        let file = fs::OpenOptions::new().create(true).read(true).write(true).open(&lock_path)?;

        let handle = HANDLE(file.as_raw_handle());
        let mut overlapped = OVERLAPPED::default();
        // SAFETY: `handle` is a valid, open file handle for the duration of
        // this call (`file` outlives it); locking the whole file (`u32::MAX`
        // bytes both halves) matches `flock`'s whole-file semantics above.
        unsafe {
            LockFileEx(handle, LOCKFILE_EXCLUSIVE_LOCK, 0, u32::MAX, u32::MAX, &mut overlapped)
                .map_err(|e| io::Error::from_raw_os_error(e.code().0 as i32))?;
        }
        Ok(Self { _file: file })
    }
}

/// Neither unix nor windows: locking is a no-op. Kept so this module still
/// compiles rather than gating the whole crate on `cfg(any(unix, windows))`
/// (every current build target of `isekai-ssh`/`isekai-pipe` is one or the
/// other — the Android app talks to `rust-core` through UniFFI, never this
/// CLI-facing module).
#[cfg(not(any(unix, windows)))]
struct ProfileLock;

#[cfg(not(any(unix, windows)))]
impl ProfileLock {
    fn acquire(_dir: &Path, _key: &str) -> io::Result<Self> {
        Ok(Self)
    }
}

/// Reads the current profile for `key` (`None` if it doesn't exist yet),
/// passes it to `update` to compute the new value, and writes the result —
/// all while holding [`ProfileLock`], so two concurrent processes updating
/// the *same* profile serialize instead of racing a lost update (one
/// process's change silently discarded by the other's unconditional
/// overwrite). `write_persistent_profile` itself does not lock (it's used
/// by callers that always construct a whole fresh profile from scratch —
/// `isekai-ssh init`/wrapper's auto-bootstrap — where there is no prior read
/// to race against; see this function's module docs for which future
/// callers this exists for instead).
pub fn update_persistent_profile<F>(dir: &Path, key: &str, update: F) -> io::Result<PathBuf>
where
    F: FnOnce(Option<PersistentProfile>) -> PersistentProfile,
{
    let _lock = ProfileLock::acquire(dir, key)?;
    let current = load_persistent_profile(dir, key)?;
    let next = update(current);
    write_persistent_profile(dir, &next)
}

/// Loads a previously written persistent profile, if present.
pub fn load_persistent_profile(dir: &Path, profile_name: &str) -> io::Result<Option<PersistentProfile>> {
    let path = dir.join(format!("{}.json", sanitize_filename_component(profile_name)));
    match fs::read(&path) {
        Ok(bytes) => {
            let profile = serde_json::from_slice(&bytes)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            Ok(Some(profile))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_trust() -> HelperTrust {
        HelperTrust {
            identity_pubkey: "pk-abc".to_string(),
            trusted_helper_sha256: "a".repeat(64),
            trusted_helper_version: "0.3.1".to_string(),
            update_policy: UpdatePolicy::ExactDigestOnly,
            release_channel: Some("stable".to_string()),
            last_via: Some("bastion.example.com".to_string()),
            trusted_at: "2026-07-04T00:00:00Z".to_string(),
            last_seen_at: "2026-07-05T00:00:00Z".to_string(),
            cached_relay_addr: "203.0.113.10:45231".to_string(),
            cached_cert_sha256: "3a7f".repeat(16),
            cached_session_secret: "c2VjcmV0MTIzNDU2Nzg5MDEyMzQ1Njc4OTAxMjM=".to_string(),
            cached_stun_observed_addr: None,
        }
    }

    #[test]
    fn migrates_legacy_entry_into_new_schema() {
        let trust = sample_trust();
        let profile = PersistentProfile::migrate_legacy_helper_trust("myhost:22", &trust);

        assert_eq!(profile.schema_version, PERSISTENT_PROFILE_SCHEMA_VERSION);
        assert_eq!(profile.profile, "myhost:22");
        assert_eq!(profile.peer_id, None);
        assert_eq!(profile.server_identity.cert_sha256_hex, trust.cached_cert_sha256);
        assert_eq!(profile.service, "ssh");
        assert!(profile.link_endpoints.is_empty());
        assert!(profile.stun_servers.is_empty());
        assert_eq!(profile.relay_policy, RelayPolicy::RelayAllowed);
        assert_eq!(profile.remote_version.as_deref(), Some("0.3.1"));
        assert_eq!(profile.last_bootstrap_at.as_deref(), Some("2026-07-04T00:00:00Z"));
        assert_eq!(profile.last_path_hint, None);
        assert_eq!(
            profile.legacy_relay_transport,
            Some(LegacyRelayTransport {
                helper_addr: trust.cached_relay_addr.clone(),
                session_secret_b64: trust.cached_session_secret.clone(),
            })
        );
        assert_eq!(profile.identity_pubkey, trust.identity_pubkey);
        assert_eq!(profile.trusted_helper_sha256, trust.trusted_helper_sha256);
        assert_eq!(profile.update_policy, trust.update_policy);
        assert_eq!(profile.release_channel, trust.release_channel);
        assert_eq!(profile.last_via, trust.last_via);
        assert_eq!(profile.last_seen_at, trust.last_seen_at);
        assert_eq!(profile.cached_stun_observed_addr, trust.cached_stun_observed_addr);
    }

    #[test]
    fn rebuilds_relay_transport_from_migrated_profile() {
        let trust = sample_trust();
        let profile = PersistentProfile::migrate_legacy_helper_trust("myhost:22", &trust);

        assert_eq!(
            profile.to_legacy_relay_transport(),
            Some(IntentTransport::Relay {
                helper_addr: trust.cached_relay_addr,
                server_name: "isekai-helper".to_string(),
                session_secret_b64: trust.cached_session_secret,
            })
        );
    }

    #[test]
    fn profile_without_legacy_bridge_has_no_relay_transport() {
        let profile = PersistentProfile {
            schema_version: PERSISTENT_PROFILE_SCHEMA_VERSION,
            profile: "future-host".to_string(),
            peer_id: Some("peer_01".to_string()),
            server_identity: ServerIdentity {
                cert_sha256_hex: "ab".repeat(32),
            },
            service: "ssh".to_string(),
            link_endpoints: vec!["https://link.example.com".to_string()],
            rendezvous: Vec::new(),
            stun_servers: Vec::new(),
            relay_endpoints: Vec::new(),
            relay_policy: RelayPolicy::RelayAllowed,
            remote_version: None,
            last_bootstrap_at: None,
            last_path_hint: None,
            legacy_relay_transport: None,
            identity_pubkey: "pk-future".to_string(),
            trusted_helper_sha256: "b".repeat(64),
            update_policy: UpdatePolicy::ExactDigestOnly,
            release_channel: None,
            last_via: None,
            last_seen_at: "2026-07-08T00:00:00Z".to_string(),
            cached_stun_observed_addr: None,
        };
        assert_eq!(profile.to_legacy_relay_transport(), None);
    }

    #[test]
    fn migrates_every_entry_in_a_trust_store() {
        let mut store = TrustStore::default();
        store.insert("host-a:22".to_string(), sample_trust());
        store.insert("host-b:22".to_string(), sample_trust());

        let mut profiles = migrate_trust_store(&store);
        profiles.sort_by(|a, b| a.profile.cmp(&b.profile));

        assert_eq!(profiles.len(), 2);
        assert_eq!(profiles[0].profile, "host-a:22");
        assert_eq!(profiles[1].profile, "host-b:22");
    }

    #[test]
    fn round_trips_through_json_serialization() {
        let trust = sample_trust();
        let profile = PersistentProfile::migrate_legacy_helper_trust("myhost:22", &trust);

        let json = serde_json::to_string_pretty(&profile).unwrap();
        let parsed: PersistentProfile = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, profile);
    }

    #[test]
    fn writes_and_loads_a_persistent_profile() {
        let dir = std::env::temp_dir().join(format!(
            "isekai-pipe-profile-test-{}-{}",
            std::process::id(),
            profile_test_nonce()
        ));
        let trust = sample_trust();
        let profile = PersistentProfile::migrate_legacy_helper_trust("myhost:22", &trust);

        write_persistent_profile(&dir, &profile).unwrap();
        let loaded = load_persistent_profile(&dir, "myhost:22").unwrap();
        assert_eq!(loaded, Some(profile));

        assert_eq!(load_persistent_profile(&dir, "no-such-host").unwrap(), None);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn host_port_key_does_not_reach_the_filesystem_as_a_raw_colon() {
        // `host:port` keys written unescaped become `<host>:<port>.json` on
        // disk, which NTFS parses as an Alternate Data Stream (`<port>.json`
        // on a `<host>` base file) rather than a plain filename -- the
        // `fs::rename` in `write_persistent_profile` then fails on Windows
        // with `ERROR_INVALID_PARAMETER` (os error 87).
        let dir = std::env::temp_dir().join(format!(
            "isekai-pipe-profile-test-colon-{}-{}",
            std::process::id(),
            profile_test_nonce()
        ));
        let profile = PersistentProfile::migrate_legacy_helper_trust("myhost:22", &sample_trust());

        let path = write_persistent_profile(&dir, &profile).unwrap();
        assert!(
            !path.file_name().unwrap().to_str().unwrap().contains(':'),
            "on-disk filename must not contain a raw ':': {path:?}"
        );
        assert_eq!(load_persistent_profile(&dir, "myhost:22").unwrap(), Some(profile));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn update_persistent_profile_creates_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        let key = "myhost:22";

        let path = update_persistent_profile(dir.path(), key, |current| {
            assert_eq!(current, None, "no profile exists yet");
            PersistentProfile::migrate_legacy_helper_trust(key, &sample_trust())
        })
        .unwrap();

        assert!(path.exists());
        let loaded = load_persistent_profile(dir.path(), key).unwrap();
        assert_eq!(loaded.unwrap().profile, key);
    }

    #[test]
    fn update_persistent_profile_passes_the_existing_value() {
        let dir = tempfile::tempdir().unwrap();
        let key = "myhost:22";
        let initial = PersistentProfile::migrate_legacy_helper_trust(key, &sample_trust());
        write_persistent_profile(dir.path(), &initial).unwrap();

        update_persistent_profile(dir.path(), key, |current| {
            let mut current = current.expect("the profile written above should be visible here");
            current.remote_version = Some("updated".to_string());
            current
        })
        .unwrap();

        let loaded = load_persistent_profile(dir.path(), key).unwrap().unwrap();
        assert_eq!(loaded.remote_version.as_deref(), Some("updated"));
    }

    /// Without `ProfileLock` serializing the read-modify-write cycle, N
    /// concurrent updaters incrementing the same counter lose updates
    /// (thread A reads 5, thread B reads 5, both write 6 — one increment
    /// vanishes) — this is exactly the race `ISEKAI_PIPE_DESIGN.md` §8 Epic A
    /// calls out ("複数`isekai-ssh`プロセス間の競合...排他制御を入れる").
    /// Spawning real OS threads (not just calling the function serially) is
    /// the point: `flock(2)` must actually block a second opener of the same
    /// path, not merely look correct in single-threaded use.
    ///
    /// Unix-only: `ProfileLock` is a documented no-op on `cfg(not(unix))`
    /// (see its doc comment above), so this race is expected to actually
    /// lose updates there rather than indicating a regression.
    #[test]
    #[cfg(unix)]
    fn update_persistent_profile_serializes_concurrent_updaters_without_lost_updates() {
        let dir = tempfile::tempdir().unwrap();
        let key = "myhost:22";
        const UPDATERS: u64 = 25;

        let handles: Vec<_> = (0..UPDATERS)
            .map(|_| {
                let dir_path = dir.path().to_path_buf();
                std::thread::spawn(move || {
                    update_persistent_profile(&dir_path, key, |current| {
                        let mut profile = current.unwrap_or_else(|| PersistentProfile::migrate_legacy_helper_trust(key, &sample_trust()));
                        let n: u64 = profile.remote_version.as_deref().and_then(|v| v.parse().ok()).unwrap_or(0);
                        profile.remote_version = Some((n + 1).to_string());
                        profile
                    })
                    .unwrap();
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }

        let loaded = load_persistent_profile(dir.path(), key).unwrap().unwrap();
        assert_eq!(loaded.remote_version.as_deref(), Some(UPDATERS.to_string().as_str()), "every updater's increment should have been preserved");
    }

    #[test]
    fn resolve_profiles_dir_prefers_explicit_override_over_everything() {
        let dir = resolve_profiles_dir(
            Some("/explicit".into()),
            Some("/xdg-state".into()),
            Some(r"C:\Users\alice\AppData\Local".into()),
            Some("/home/alice".into()),
        )
        .unwrap();
        assert_eq!(dir, PathBuf::from("/explicit"));
    }

    #[test]
    fn resolve_profiles_dir_prefers_xdg_state_home_over_windows_and_home() {
        let dir = resolve_profiles_dir(
            None,
            Some("/xdg-state".into()),
            Some(r"C:\Users\alice\AppData\Local".into()),
            Some("/home/alice".into()),
        )
        .unwrap();
        assert_eq!(dir, PathBuf::from("/xdg-state/isekai/profiles"));
    }

    #[test]
    fn resolve_profiles_dir_prefers_windows_local_app_data_over_home() {
        // The whole point of this ordering (see `default_profiles_dir`'s doc
        // comment): MSYS2/Git-Bash sets `HOME` on top of the identical
        // Windows binary a plain cmd.exe/PowerShell session runs with `HOME`
        // unset, so `LOCALAPPDATA` must win regardless of whether `HOME`
        // also happens to be set, or the very same host resolves to two
        // different trust-store directories depending on which shell
        // launched it.
        let dir = resolve_profiles_dir(
            None,
            None,
            Some(r"C:\Users\alice\AppData\Local".into()),
            Some(r"C:\Users\alice".into()),
        )
        .unwrap();
        assert_eq!(dir, PathBuf::from(r"C:\Users\alice\AppData\Local").join("isekai").join("profiles"));
    }

    #[test]
    fn resolve_profiles_dir_falls_back_to_home_when_not_on_windows() {
        // A non-Windows build always passes `None` for
        // `windows_local_app_data` (`default_profiles_dir`'s `cfg!(windows)`
        // guard) — this is the branch that build actually takes.
        let dir = resolve_profiles_dir(None, None, None, Some("/home/alice".into())).unwrap();
        assert_eq!(dir, PathBuf::from("/home/alice/.local/state/isekai/profiles"));
    }

    #[test]
    fn resolve_profiles_dir_falls_back_to_temp_dir_when_nothing_is_set() {
        let dir = resolve_profiles_dir(None, None, None, None).unwrap();
        assert_eq!(dir, std::env::temp_dir().join("isekai-profiles"));
    }

    #[test]
    fn resolve_log_file_prefers_explicit_override_over_everything() {
        let path = resolve_log_file(
            Some("/explicit/isekai-ssh.log".into()),
            Some("/xdg-state".into()),
            Some(r"C:\Users\alice\AppData\Local".into()),
            Some("/home/alice".into()),
        )
        .unwrap();
        assert_eq!(path, PathBuf::from("/explicit/isekai-ssh.log"));
    }

    #[test]
    fn resolve_log_file_prefers_xdg_state_home_over_windows_and_home() {
        let path = resolve_log_file(None, Some("/xdg-state".into()), Some(r"C:\Users\alice\AppData\Local".into()), Some("/home/alice".into()))
            .unwrap();
        assert_eq!(path, PathBuf::from("/xdg-state/isekai/logs/isekai-ssh.log"));
    }

    #[test]
    fn resolve_log_file_prefers_windows_local_app_data_over_home() {
        let path = resolve_log_file(None, None, Some(r"C:\Users\alice\AppData\Local".into()), Some(r"C:\Users\alice".into())).unwrap();
        assert_eq!(path, PathBuf::from(r"C:\Users\alice\AppData\Local").join("isekai").join("logs").join("isekai-ssh.log"));
    }

    #[test]
    fn resolve_log_file_falls_back_to_home_when_not_on_windows() {
        let path = resolve_log_file(None, None, None, Some("/home/alice".into())).unwrap();
        assert_eq!(path, PathBuf::from("/home/alice/.local/state/isekai/logs/isekai-ssh.log"));
    }

    #[test]
    fn resolve_log_file_falls_back_to_temp_dir_when_nothing_is_set() {
        let path = resolve_log_file(None, None, None, None).unwrap();
        assert_eq!(path, std::env::temp_dir().join("isekai-logs").join("isekai-ssh.log"));
    }

    fn profile_test_nonce() -> u64 {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64
    }
}
