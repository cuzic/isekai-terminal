//! Auto-downloads a matching `isekai-pipe` release asset from GitHub
//! Releases when no local `--helper-binary`/`--isekai-helper-binary` is
//! given (`ISEKAI_PIPE_DESIGN.md`'s "初回接続でも2回目接続でも isekai-ssh で
//! 接続できるようにしてほしい" — the wrapper's auto-bootstrap previously had
//! no way to source a binary to upload without the user supplying one by
//! hand on every invocation).
//!
//! **This project does not publish GitHub Releases yet**, so this module is
//! honestly incomplete in practice today: the download will 404 until a real
//! release matching the naming convention below exists. Callers
//! (`wrapper.rs`/`init.rs`) treat a failure here as just one more reason to
//! fall back to the pre-existing "pass `--helper-binary` explicitly, or run
//! `isekai-ssh init`" error — no behavior regresses, this only adds a chance
//! of success before that fallback.
//!
//! Integrity checking is sha256-only (`.sha256` sidecar, below) — signed
//! release manifests (`isekai-release-verify`) were tried in an earlier
//! iteration of this project and deliberately removed: GitHub's own
//! HTTPS/infrastructure already protects the download path, and ed25519
//! signing only adds protection against GitHub itself being compromised,
//! which is disproportionate for this project's threat model
//! (`ISEKAI_PIPE_DESIGN.md` §8 Epic D).
//!
//! HTTP client: `ureq` (blocking), same choice and same `tokio::task::
//! spawn_blocking`-wrapping convention as `isekai-auth::oauth`/`device_flow`
//! — see that module's docs for why blocking is the right call here (at
//! most one download in flight per bootstrap attempt).
//!
//! Asset naming convention (documented here so a future release-publishing
//! CI workflow has something concrete to match): `isekai-pipe-<arch>-unknown-linux-musl`,
//! optionally accompanied by a `.sha256` sidecar file with the same
//! plain-hex-digest format `rust-core/scripts/build-isekai-pipe-musl.sh`
//! already produces locally.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use isekai_bootstrap::{HostSpec, JumpSpec, OpenSshBackend};
use sha2::{Digest, Sha256};

/// How long a cached "latest" binary is trusted before
/// `ensure_helper_binary_cached` re-checks GitHub for a newer release
/// (`ISEKAI_SSH_HELPER_CACHE_TTL_SECS` overrides this, mainly for tests).
/// Pinned-tag caches (`ReleaseSource::tag = Some(_)`) never expire — a
/// specific release tag's assets are immutable on GitHub, so there is
/// nothing to revalidate.
const DEFAULT_FRESHNESS_TTL_SECS: u64 = 24 * 60 * 60;

fn freshness_ttl() -> Duration {
    std::env::var("ISEKAI_SSH_HELPER_CACHE_TTL_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(DEFAULT_FRESHNESS_TTL_SECS))
}

/// Sidecar path recording when `cache_file` was last checked against the
/// remote release (a plain Unix-seconds timestamp) — separate from the
/// `.sha256` sidecar, which records the remote's own integrity digest, not
/// our local check time.
fn last_checked_path(cache_file: &Path) -> PathBuf {
    let mut name = cache_file.as_os_str().to_os_string();
    name.push(".last-checked");
    PathBuf::from(name)
}

fn read_last_checked(path: &Path) -> Option<SystemTime> {
    let content = std::fs::read_to_string(path).ok()?;
    let secs: u64 = content.trim().parse().ok()?;
    Some(UNIX_EPOCH + Duration::from_secs(secs))
}

fn write_last_checked(path: &Path, now: SystemTime) -> Result<()> {
    let secs = now.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    std::fs::write(path, secs.to_string())
        .with_context(|| format!("isekai-ssh: failed to write freshness marker {}", path.display()))
}

/// Whether `cache_file` (already known to exist) is due for a freshness
/// re-check. No recorded timestamp (e.g. a cache written by a version of
/// this tool that predates freshness tracking) counts as stale, forcing one
/// revalidation.
fn is_stale(cache_file: &Path) -> bool {
    match read_last_checked(&last_checked_path(cache_file)) {
        Some(last_checked) => SystemTime::now().duration_since(last_checked).unwrap_or_default() >= freshness_ttl(),
        None => true,
    }
}

/// Production GitHub base URL. Tests override this with a local mock HTTP
/// server's address instead (`ensure_helper_binary_cached`'s `base_url`
/// parameter) — this constant is only ever passed at the real call sites.
pub const GITHUB_BASE_URL: &str = "https://github.com";

/// Duplicated from `init.rs`/`wrapper.rs`'s own `hex_sha256` per this
/// crate's established convention of small private per-module helpers
/// rather than a shared one.
fn hex_sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// Where to look for the release. `repo` is `"owner/repo"`; `tag` pins a
/// specific release, `None` means "latest" (resolved via GitHub's `/releases/
/// latest/download/<asset>` redirect — no GitHub API call/JSON parsing
/// needed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseSource {
    pub repo: String,
    pub tag: Option<String>,
}

impl ReleaseSource {
    /// This project's own repository — the sensible default for `isekai-ssh`
    /// itself, overridable via `--helper-release-repo`/
    /// `--isekai-helper-release-repo` for forks or private mirrors.
    pub const DEFAULT_REPO: &'static str = "cuzic/isekai-terminal";

    pub fn default_repo() -> Self {
        Self { repo: Self::DEFAULT_REPO.to_string(), tag: None }
    }
}

/// The release asset name expected for `arch` (as normalized by
/// `isekai_bootstrap::openssh`'s `detect_remote_arch`, i.e. already
/// `"x86_64"`/`"aarch64"` — never a raw `uname -m` string).
pub fn asset_name_for_arch(arch: &str) -> Result<String> {
    match arch {
        "x86_64" | "aarch64" => Ok(format!("isekai-pipe-{arch}-unknown-linux-musl")),
        other => anyhow::bail!("isekai-ssh: no isekai-pipe release asset is published for architecture {other:?}"),
    }
}

/// Builds the download URL for `asset_name` under `source`, rooted at
/// `base` (`"https://github.com"` in production; tests point this at a
/// local mock HTTP server instead — see this module's tests and
/// `isekai-ssh/tests/*_helper_download_e2e.rs`).
fn download_url(base: &str, source: &ReleaseSource, asset_name: &str) -> String {
    let base = base.trim_end_matches('/');
    match &source.tag {
        Some(tag) => format!("{base}/{}/releases/download/{tag}/{asset_name}", source.repo),
        None => format!("{base}/{}/releases/latest/download/{asset_name}", source.repo),
    }
}

/// The local cache path for `asset_name` under `source` — deterministic, so
/// a second `isekai-ssh <host>` invocation against a different host (same
/// arch) reuses the same downloaded bytes without re-fetching.
fn cache_path(cache_dir: &Path, source: &ReleaseSource, asset_name: &str) -> PathBuf {
    let repo_slug = source.repo.replace('/', "_");
    let tag_slug = source.tag.as_deref().unwrap_or("latest");
    cache_dir.join(repo_slug).join(tag_slug).join(asset_name)
}

/// `$XDG_CACHE_HOME/isekai-ssh/helpers`, falling back to
/// `$HOME/.cache/isekai-ssh/helpers` — the same XDG-with-`$HOME`-fallback
/// shape `isekai_pipe_core::{default_profiles_dir, default_runtime_dir}`
/// already use for their own state/runtime directories, applied to
/// `XDG_CACHE_HOME` (the correct XDG category for a re-fetchable, safely
/// deletable download cache, as opposed to state or runtime data).
pub fn default_helper_cache_dir() -> std::io::Result<PathBuf> {
    if let Some(path) = std::env::var_os("ISEKAI_SSH_HELPER_CACHE_DIR") {
        return Ok(PathBuf::from(path));
    }
    if let Some(path) = std::env::var_os("XDG_CACHE_HOME") {
        return Ok(PathBuf::from(path).join("isekai-ssh").join("helpers"));
    }
    if let Some(home) = std::env::var_os("HOME") {
        return Ok(PathBuf::from(home).join(".cache").join("isekai-ssh").join("helpers"));
    }
    Ok(std::env::temp_dir().join("isekai-ssh-helpers"))
}

/// Downloads `{asset_name}.sha256` (a plain hex digest, matching
/// `build-isekai-pipe-musl.sh`'s own sidecar convention) if present, and
/// verifies `bytes` against it. A missing sidecar (404) is *not* a failure —
/// no real release exists yet to guarantee one, so this is best-effort
/// integrity checking (the only integrity checking this project does —
/// see this module's docs for why signing was deliberately not added).
fn verify_sha256_sidecar_if_present(agent: &ureq::Agent, sidecar_url: &str, bytes: &[u8]) -> Result<()> {
    let response = match agent.get(sidecar_url).call() {
        Ok(response) => response,
        Err(ureq::Error::StatusCode(404)) => {
            log::warn!("isekai-ssh: no .sha256 sidecar at {sidecar_url} — skipping integrity check");
            return Ok(());
        }
        Err(e) => anyhow::bail!("isekai-ssh: failed to fetch sha256 sidecar {sidecar_url}: {e}"),
    };
    let mut response = response;
    let body = response.body_mut().read_to_string().with_context(|| format!("isekai-ssh: failed to read sha256 sidecar body from {sidecar_url}"))?;
    let expected = body.split_whitespace().next().unwrap_or("").to_ascii_lowercase();
    let actual = hex_sha256(bytes);
    if expected != actual {
        anyhow::bail!("isekai-ssh: downloaded isekai-pipe binary failed sha256 verification (expected {expected}, got {actual})");
    }
    Ok(())
}

/// Ensures a matching `isekai-pipe` binary is present in the local cache,
/// downloading it from `source` if not already cached, and returns its
/// path. Blocking (`ureq`) work runs inside `tokio::task::spawn_blocking`,
/// matching `isekai-auth`'s convention.
///
/// A pinned tag (`source.tag = Some(_)`) is trusted indefinitely once
/// cached — a specific release's assets never change on GitHub, so there is
/// nothing to re-check. A `"latest"` cache (`source.tag = None`) is
/// re-validated once `DEFAULT_FRESHNESS_TTL_SECS` has passed since the last
/// check: this function re-downloads and compares against the cached bytes,
/// replacing the cache only if the content actually changed
/// (`download_and_cache`). Network failure during that re-check falls back
/// to the existing (stale but still valid) cached binary with a warning,
/// rather than failing the caller outright — matching this project's
/// opportunistic-fallback design (`CLAUDE.md` 設計原則): a briefly
/// unreachable GitHub shouldn't break an `isekai-ssh <host>` invocation that
/// would otherwise have worked from cache alone.
pub async fn ensure_helper_binary_cached(cache_dir: &Path, source: &ReleaseSource, arch: &str, base_url: &str) -> Result<PathBuf> {
    let asset_name = asset_name_for_arch(arch)?;
    let path = cache_path(cache_dir, source, &asset_name);
    let cache_existed = path.exists();
    if cache_existed && (source.tag.is_some() || !is_stale(&path)) {
        log::debug!("isekai-ssh: using cached isekai-pipe binary at {}", path.display());
        return Ok(path);
    }

    let base_url = base_url.to_string();
    let source = source.clone();
    let cache_dir = cache_dir.to_path_buf();
    let asset_name_for_task = asset_name.clone();
    let result = tokio::task::spawn_blocking(move || download_and_cache(&cache_dir, &source, &asset_name_for_task, &base_url))
        .await
        .context("isekai-ssh: helper binary download task panicked")?;

    match result {
        Ok(path) => Ok(path),
        Err(e) if cache_existed => {
            log::warn!(
                "isekai-ssh: failed to check for a newer isekai-pipe release ({e:#}); continuing with the cached binary at {}",
                path.display()
            );
            Ok(path)
        }
        Err(e) => Err(e),
    }
}

/// The single entry point `init.rs`/`wrapper.rs` call: `explicit_path`
/// (`--helper-binary`/`--isekai-helper-binary`) always wins outright — no
/// arch detection, no network, matching today's behavior exactly for
/// callers who already pass it. Only when it's `None` does this detect the
/// remote's architecture and fall through to the download+cache path.
pub async fn resolve_helper_binary(
    explicit_path: Option<&Path>,
    backend: &OpenSshBackend,
    target: &HostSpec,
    via: &[JumpSpec],
    source: &ReleaseSource,
) -> Result<Vec<u8>> {
    if let Some(path) = explicit_path {
        return std::fs::read(path).with_context(|| format!("failed to read helper binary at {}", path.display()));
    }

    // Test-only override (real callers never set this) — points the
    // download at a local mock HTTP server instead of real GitHub, the same
    // way `ISEKAI_PIPE_PROFILES_DIR`/`ISEKAI_SSH_HELPER_CACHE_DIR` already
    // let tests redirect other real paths.
    let base_url = std::env::var("ISEKAI_SSH_HELPER_RELEASE_BASE_URL").unwrap_or_else(|_| GITHUB_BASE_URL.to_string());

    let arch = backend
        .detect_remote_arch(target, via)
        .await
        .context("failed to detect the remote architecture (uname -m) needed to auto-download a helper binary")?;
    let cache_dir = default_helper_cache_dir().context("could not determine the helper binary cache directory")?;
    let path = ensure_helper_binary_cached(&cache_dir, source, &arch, &base_url)
        .await
        .with_context(|| format!("auto-downloading an isekai-pipe binary for architecture {arch:?} from {}/{} failed", source.repo, source.tag.as_deref().unwrap_or("latest")))?;
    std::fs::read(&path).with_context(|| format!("failed to read downloaded helper binary at {}", path.display()))
}

fn download_and_cache(cache_dir: &Path, source: &ReleaseSource, asset_name: &str, base_url: &str) -> Result<PathBuf> {
    let url = download_url(base_url, source, asset_name);
    let path = cache_path(cache_dir, source, asset_name);
    let previously_cached = std::fs::read(&path).ok();

    let agent: ureq::Agent = ureq::Agent::config_builder().build().into();
    let mut response = agent
        .get(&url)
        .call()
        .with_context(|| format!("isekai-ssh: failed to download isekai-pipe release asset from {url}"))?;
    let bytes = response
        .body_mut()
        .with_config()
        .limit(128 * 1024 * 1024)
        .read_to_vec()
        .with_context(|| format!("isekai-ssh: failed to read downloaded isekai-pipe binary from {url}"))?;

    verify_sha256_sidecar_if_present(&agent, &format!("{url}.sha256"), &bytes)?;

    let parent = path.parent().expect("cache_path always has a parent directory");
    std::fs::create_dir_all(parent).with_context(|| format!("isekai-ssh: failed to create helper cache directory {}", parent.display()))?;

    if previously_cached.as_deref() == Some(bytes.as_slice()) {
        log::debug!("isekai-ssh: cached isekai-pipe binary at {} is already up to date", path.display());
    } else {
        let tmp = parent.join(format!("{}.{}.tmp", asset_name, std::process::id()));
        {
            let mut file = std::fs::File::create(&tmp).with_context(|| format!("isekai-ssh: failed to create temp file {}", tmp.display()))?;
            file.write_all(&bytes).with_context(|| format!("isekai-ssh: failed to write {}", tmp.display()))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
            }
        }
        std::fs::rename(&tmp, &path).with_context(|| format!("isekai-ssh: failed to move downloaded binary into place at {}", path.display()))?;
        let verb = if previously_cached.is_some() { "updated" } else { "cached" };
        log::info!("isekai-ssh: {verb} isekai-pipe binary ({} bytes) at {}", bytes.len(), path.display());
    }

    write_last_checked(&last_checked_path(&path), SystemTime::now())?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asset_name_for_arch_covers_both_supported_architectures() {
        assert_eq!(asset_name_for_arch("x86_64").unwrap(), "isekai-pipe-x86_64-unknown-linux-musl");
        assert_eq!(asset_name_for_arch("aarch64").unwrap(), "isekai-pipe-aarch64-unknown-linux-musl");
    }

    #[test]
    fn asset_name_for_arch_rejects_unknown_architectures() {
        assert!(asset_name_for_arch("riscv64").is_err());
    }

    #[test]
    fn download_url_uses_latest_download_redirect_when_tag_is_unset() {
        let source = ReleaseSource { repo: "cuzic/isekai-terminal".to_string(), tag: None };
        assert_eq!(
            download_url("https://github.com", &source, "isekai-pipe-x86_64-unknown-linux-musl"),
            "https://github.com/cuzic/isekai-terminal/releases/latest/download/isekai-pipe-x86_64-unknown-linux-musl"
        );
    }

    #[test]
    fn download_url_targets_a_pinned_tag_when_given() {
        let source = ReleaseSource { repo: "cuzic/isekai-terminal".to_string(), tag: Some("v0.1.0".to_string()) };
        assert_eq!(
            download_url("https://github.com", &source, "isekai-pipe-aarch64-unknown-linux-musl"),
            "https://github.com/cuzic/isekai-terminal/releases/download/v0.1.0/isekai-pipe-aarch64-unknown-linux-musl"
        );
    }

    #[test]
    fn cache_path_is_deterministic_and_sanitizes_the_repo_slug() {
        let dir = Path::new("/cache");
        let source = ReleaseSource { repo: "cuzic/isekai-terminal".to_string(), tag: None };
        assert_eq!(
            cache_path(dir, &source, "isekai-pipe-x86_64-unknown-linux-musl"),
            PathBuf::from("/cache/cuzic_isekai-terminal/latest/isekai-pipe-x86_64-unknown-linux-musl")
        );

        let pinned = ReleaseSource { repo: "cuzic/isekai-terminal".to_string(), tag: Some("v1".to_string()) };
        assert_eq!(
            cache_path(dir, &pinned, "isekai-pipe-x86_64-unknown-linux-musl"),
            PathBuf::from("/cache/cuzic_isekai-terminal/v1/isekai-pipe-x86_64-unknown-linux-musl")
        );
    }

    #[test]
    fn default_repo_matches_this_project() {
        assert_eq!(ReleaseSource::default_repo(), ReleaseSource { repo: "cuzic/isekai-terminal".to_string(), tag: None });
    }

    /// Minimal single-request-at-a-time HTTP/1.1 mock server: reads the
    /// request line + headers (discarding the latter), looks up the path in
    /// `routes`, and responds 200+body or 404 — just enough to exercise
    /// `ensure_helper_binary_cached` against something other than real
    /// GitHub, matching this workspace's established "hand-roll the minimal
    /// protocol responder needed" convention (e.g. the mock STUN server in
    /// `isekai-bootstrap::openssh`'s own tests).
    fn spawn_mock_release_server(routes: std::collections::HashMap<String, Vec<u8>>) -> std::net::SocketAddr {
        use std::io::{BufRead, BufReader, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut request_line = String::new();
                if reader.read_line(&mut request_line).unwrap_or(0) == 0 {
                    continue;
                }
                loop {
                    let mut line = String::new();
                    match reader.read_line(&mut line) {
                        Ok(0) => break,
                        Ok(_) if line == "\r\n" || line == "\n" => break,
                        Ok(_) => continue,
                        Err(_) => break,
                    }
                }
                let path = request_line.split_whitespace().nth(1).unwrap_or("/").to_string();
                match routes.get(&path) {
                    Some(body) => {
                        let header = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                        let _ = stream.write_all(header.as_bytes());
                        let _ = stream.write_all(body);
                    }
                    None => {
                        let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                    }
                }
                let _ = stream.flush();
            }
        });
        addr
    }

    #[tokio::test]
    async fn ensure_helper_binary_cached_downloads_verifies_and_caches() {
        let binary_bytes = b"pretend-isekai-pipe-binary-bytes".to_vec();
        let sha256_line = format!("{}  isekai-pipe-x86_64-unknown-linux-musl\n", hex_sha256(&binary_bytes));
        let mut routes = std::collections::HashMap::new();
        routes.insert(
            "/cuzic/isekai-terminal/releases/latest/download/isekai-pipe-x86_64-unknown-linux-musl".to_string(),
            binary_bytes.clone(),
        );
        routes.insert(
            "/cuzic/isekai-terminal/releases/latest/download/isekai-pipe-x86_64-unknown-linux-musl.sha256".to_string(),
            sha256_line.into_bytes(),
        );
        let addr = spawn_mock_release_server(routes);
        let base_url = format!("http://{addr}");

        let cache_dir = tempfile::tempdir().unwrap();
        let source = ReleaseSource::default_repo();

        let path = ensure_helper_binary_cached(cache_dir.path(), &source, "x86_64", &base_url).await.unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), binary_bytes);

        // Second call must not need the network at all: shut down by
        // pointing at an address nothing listens on, and confirm it still
        // succeeds purely from the cache.
        let unreachable = "http://127.0.0.1:1";
        let cached_path = ensure_helper_binary_cached(cache_dir.path(), &source, "x86_64", unreachable).await.unwrap();
        assert_eq!(cached_path, path);
    }

    /// Seeds a cache directory as if a binary had already been downloaded
    /// `age` ago — same layout `download_and_cache` itself produces.
    fn seed_stale_cache(cache_dir: &Path, source: &ReleaseSource, bytes: &[u8], age: Duration) -> PathBuf {
        let asset_name = asset_name_for_arch("x86_64").unwrap();
        let path = cache_path(cache_dir, source, &asset_name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, bytes).unwrap();
        write_last_checked(&last_checked_path(&path), SystemTime::now() - age).unwrap();
        path
    }

    #[tokio::test]
    async fn ensure_helper_binary_cached_redownloads_a_stale_latest_cache_when_the_content_changed() {
        let old_bytes = b"old-isekai-pipe-bytes".to_vec();
        let new_bytes = b"new-isekai-pipe-bytes-longer".to_vec();
        let cache_dir = tempfile::tempdir().unwrap();
        let source = ReleaseSource::default_repo();
        let path = seed_stale_cache(cache_dir.path(), &source, &old_bytes, Duration::from_secs(25 * 60 * 60));

        let sha256_line = format!("{}  isekai-pipe-x86_64-unknown-linux-musl\n", hex_sha256(&new_bytes));
        let mut routes = std::collections::HashMap::new();
        routes.insert(
            "/cuzic/isekai-terminal/releases/latest/download/isekai-pipe-x86_64-unknown-linux-musl".to_string(),
            new_bytes.clone(),
        );
        routes.insert(
            "/cuzic/isekai-terminal/releases/latest/download/isekai-pipe-x86_64-unknown-linux-musl.sha256".to_string(),
            sha256_line.into_bytes(),
        );
        let addr = spawn_mock_release_server(routes);
        let base_url = format!("http://{addr}");

        let returned = ensure_helper_binary_cached(cache_dir.path(), &source, "x86_64", &base_url).await.unwrap();
        assert_eq!(returned, path);
        assert_eq!(std::fs::read(&path).unwrap(), new_bytes);
    }

    #[tokio::test]
    async fn ensure_helper_binary_cached_never_revalidates_a_pinned_tag() {
        let old_bytes = b"pinned-tag-bytes-never-change".to_vec();
        let cache_dir = tempfile::tempdir().unwrap();
        let source = ReleaseSource { repo: "cuzic/isekai-terminal".to_string(), tag: Some("v1.0.0".to_string()) };
        // Stale by a huge margin, and no last-checked marker at all — a
        // pinned tag must still skip revalidation entirely.
        let path = cache_path(cache_dir.path(), &source, &asset_name_for_arch("x86_64").unwrap());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &old_bytes).unwrap();

        let unreachable = "http://127.0.0.1:1";
        let returned = ensure_helper_binary_cached(cache_dir.path(), &source, "x86_64", unreachable).await.unwrap();
        assert_eq!(returned, path);
        assert_eq!(std::fs::read(&path).unwrap(), old_bytes);
    }

    #[tokio::test]
    async fn ensure_helper_binary_cached_falls_back_to_a_stale_cache_when_revalidation_is_unreachable() {
        let old_bytes = b"stale-but-still-usable-bytes".to_vec();
        let cache_dir = tempfile::tempdir().unwrap();
        let source = ReleaseSource::default_repo();
        let path = seed_stale_cache(cache_dir.path(), &source, &old_bytes, Duration::from_secs(25 * 60 * 60));

        let unreachable = "http://127.0.0.1:1";
        let returned = ensure_helper_binary_cached(cache_dir.path(), &source, "x86_64", unreachable).await.unwrap();
        assert_eq!(returned, path);
        assert_eq!(std::fs::read(&path).unwrap(), old_bytes);
    }

    #[tokio::test]
    async fn ensure_helper_binary_cached_refreshes_the_freshness_marker_without_rewriting_identical_bytes() {
        let bytes = b"unchanged-isekai-pipe-bytes".to_vec();
        let cache_dir = tempfile::tempdir().unwrap();
        let source = ReleaseSource::default_repo();
        let path = seed_stale_cache(cache_dir.path(), &source, &bytes, Duration::from_secs(25 * 60 * 60));

        let sha256_line = format!("{}  isekai-pipe-x86_64-unknown-linux-musl\n", hex_sha256(&bytes));
        let mut routes = std::collections::HashMap::new();
        routes.insert(
            "/cuzic/isekai-terminal/releases/latest/download/isekai-pipe-x86_64-unknown-linux-musl".to_string(),
            bytes.clone(),
        );
        routes.insert(
            "/cuzic/isekai-terminal/releases/latest/download/isekai-pipe-x86_64-unknown-linux-musl.sha256".to_string(),
            sha256_line.into_bytes(),
        );
        let addr = spawn_mock_release_server(routes);
        let base_url = format!("http://{addr}");

        ensure_helper_binary_cached(cache_dir.path(), &source, "x86_64", &base_url).await.unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        // The marker must have been refreshed even though the bytes were
        // identical — otherwise every single invocation would re-check.
        assert!(!is_stale(&path));
    }

    #[tokio::test]
    async fn ensure_helper_binary_cached_rejects_a_bad_sha256_sidecar() {
        let binary_bytes = b"pretend-isekai-pipe-binary-bytes".to_vec();
        let mut routes = std::collections::HashMap::new();
        routes.insert(
            "/cuzic/isekai-terminal/releases/latest/download/isekai-pipe-x86_64-unknown-linux-musl".to_string(),
            binary_bytes,
        );
        routes.insert(
            "/cuzic/isekai-terminal/releases/latest/download/isekai-pipe-x86_64-unknown-linux-musl.sha256".to_string(),
            b"deadbeef  isekai-pipe-x86_64-unknown-linux-musl\n".to_vec(),
        );
        let addr = spawn_mock_release_server(routes);
        let base_url = format!("http://{addr}");

        let cache_dir = tempfile::tempdir().unwrap();
        let source = ReleaseSource::default_repo();
        let err = ensure_helper_binary_cached(cache_dir.path(), &source, "x86_64", &base_url).await.unwrap_err();
        assert!(format!("{err:#}").contains("sha256"), "{err:#}");
    }

    #[tokio::test]
    async fn ensure_helper_binary_cached_tolerates_a_missing_sha256_sidecar() {
        let binary_bytes = b"pretend-isekai-pipe-binary-bytes".to_vec();
        let mut routes = std::collections::HashMap::new();
        routes.insert(
            "/cuzic/isekai-terminal/releases/latest/download/isekai-pipe-x86_64-unknown-linux-musl".to_string(),
            binary_bytes.clone(),
        );
        // No `.sha256` route registered — server 404s it.
        let addr = spawn_mock_release_server(routes);
        let base_url = format!("http://{addr}");

        let cache_dir = tempfile::tempdir().unwrap();
        let source = ReleaseSource::default_repo();
        let path = ensure_helper_binary_cached(cache_dir.path(), &source, "x86_64", &base_url).await.unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), binary_bytes);
    }

    #[tokio::test]
    async fn ensure_helper_binary_cached_fails_when_the_asset_itself_404s() {
        let routes = std::collections::HashMap::new();
        let addr = spawn_mock_release_server(routes);
        let base_url = format!("http://{addr}");

        let cache_dir = tempfile::tempdir().unwrap();
        let source = ReleaseSource::default_repo();
        assert!(ensure_helper_binary_cached(cache_dir.path(), &source, "x86_64", &base_url).await.is_err());
    }

    #[tokio::test]
    async fn resolve_helper_binary_prefers_the_explicit_path_and_never_touches_the_network() {
        let tmp = tempfile::tempdir().unwrap();
        let binary_path = tmp.path().join("isekai-pipe");
        std::fs::write(&binary_path, b"explicit-binary-bytes").unwrap();

        // A backend/target that would fail immediately if `detect_remote_arch`
        // were ever called (nothing listens on this port) — proving the
        // explicit-path branch really does skip SSH/network entirely.
        let backend = OpenSshBackend::new();
        let target = HostSpec::new("127.0.0.1").with_port(1);
        let source = ReleaseSource::default_repo();

        let bytes = resolve_helper_binary(Some(&binary_path), &backend, &target, &[], &source).await.unwrap();
        assert_eq!(bytes, b"explicit-binary-bytes");
    }
}
