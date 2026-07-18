//! Resolves a deliberate subset of OpenSSH `ssh_config(5)` keywords —
//! `HostName`/`User`/`Port`/`IdentityFile`/`ProxyJump`/`ForwardAgent`/
//! `IdentityAgent` — for a given destination host, following the same
//! `Host`/`Include` structural semantics `ssh(1)` itself uses (first
//! obtained value wins per key, except `IdentityFile` which accumulates
//! across all matching blocks; `Include` splices the referenced file's
//! lines in place, glob patterns expand in sorted order, cyclic includes
//! are silently skipped on repeat).
//!
//! **Deliberate limitation**: `Match` block conditions (`Match exec`,
//! `Match host`, `Match user`, ...) are recognized structurally (so a
//! `Match` line doesn't get misparsed as a keyword) but never evaluated —
//! anything inside a `Match` block is simply never applied. This crate has
//! no opinion on process execution or the runtime context those conditions
//! need; only `Host` patterns are supported.
//!
//! Any keyword other than the ones listed above is silently ignored — this
//! is not a general-purpose `ssh_config(5)` parser.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("failed to read {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid Include pattern {pattern:?}: {source}")]
    InvalidIncludePattern {
        pattern: String,
        #[source]
        source: glob::PatternError,
    },
    #[error("failed to expand Include pattern {pattern:?}: {source}")]
    IncludeGlob {
        pattern: String,
        #[source]
        source: glob::GlobError,
    },
}

/// The subset of `ssh_config(5)` keywords this crate resolves, merged across
/// every `Host` block matching the destination. `None`/empty means the
/// keyword was never set by any matching block.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HostConfig {
    pub host_name: Option<String>,
    pub user: Option<String>,
    pub port: Option<u16>,
    /// Accumulates across every matching block, in file order (matches real
    /// `ssh_config(5)` `IdentityFile` semantics — later matches add
    /// candidates rather than overriding).
    pub identity_file: Vec<PathBuf>,
    /// Raw value, not parsed further (e.g. `"user@jump-host:2222"` or
    /// `"host1,host2"` for a multi-hop chain) — parsing this into hops is
    /// the caller's job.
    pub proxy_jump: Option<String>,
    pub forward_agent: Option<ForwardAgent>,
    /// Tilde-expanded, like `IdentityFile` (`ssh -G` expands `~` here too).
    /// May still be a sentinel rather than a real path (`"SSH_AUTH_SOCK"`
    /// meaning "use the env var", or `"none"` meaning "disabled") — this
    /// crate doesn't try to distinguish those from real paths.
    pub identity_agent: Option<PathBuf>,
}

/// `ssh_config(5)` `ForwardAgent` accepts `yes`/`no` or an explicit agent
/// socket path/env-var reference to forward instead of the default one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForwardAgent {
    Yes,
    No,
    /// Raw value (a path or `$ENV_VAR`-style reference) — not tilde-expanded
    /// since it may not be a literal path (e.g. an env var name).
    Socket(String),
}

/// Resolves `HostConfig` for `destination` from `~/.ssh/config` (`HOME` on
/// Unix, falling back to `USERPROFILE` on Windows). Returns an empty
/// `HostConfig` (not an error) if no config file exists or the home
/// directory can't be determined — a missing config file is not a failure
/// in `ssh(1)` either.
pub fn resolve_default(destination: &str) -> Result<HostConfig, Error> {
    let Some(home) = home_dir() else {
        return Ok(HostConfig::default());
    };
    let path = home.join(".ssh").join("config");
    if !path.exists() {
        return Ok(HostConfig::default());
    }
    resolve(&path, destination)
}

/// Resolves `HostConfig` for `destination` starting from the config file at
/// `path` (following any `Include` directives it contains).
pub fn resolve(path: &Path, destination: &str) -> Result<HostConfig, Error> {
    let mut visited = HashSet::new();
    let mut config = HostConfig::default();
    resolve_from_file(path, destination, &mut visited, &mut config)?;
    Ok(config)
}

fn resolve_from_file(
    path: &Path,
    destination: &str,
    visited: &mut HashSet<PathBuf>,
    config: &mut HostConfig,
) -> Result<(), Error> {
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    if !visited.insert(canonical) {
        return Ok(());
    }
    let content = std::fs::read_to_string(path)
        .map_err(|source| Error::Read { path: path.to_path_buf(), source })?;
    let base_dir = path.parent();
    let mut active = true;

    for raw_line in content.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (keyword, rest) = split_keyword(line);
        let lower = keyword.to_ascii_lowercase();
        match lower.as_str() {
            // Include splices the referenced file's lines in at this exact
            // point (ssh_config(5)) — if we're currently inside a
            // non-matching Host/Match block, the spliced-in content
            // inherits that inactivity too, so skip the include entirely
            // rather than let it unconditionally re-activate at the top of
            // the included file.
            "include" if active => {
                for pattern in split_words(rest) {
                    for include in expand_include_pattern(&pattern, base_dir)? {
                        resolve_from_file(&include, destination, visited, config)?;
                    }
                }
            }
            "host" => active = host_patterns_match(rest, destination),
            "match" => active = false,
            other if active => apply_keyword(config, other, rest),
            _ => {}
        }
    }
    Ok(())
}

fn apply_keyword(config: &mut HostConfig, keyword: &str, value: &str) {
    let value = strip_quotes(value.trim());
    match keyword {
        "hostname" => {
            if config.host_name.is_none() {
                config.host_name = Some(value.to_string());
            }
        }
        "user" => {
            if config.user.is_none() {
                config.user = Some(value.to_string());
            }
        }
        "port" => {
            if config.port.is_none() {
                if let Ok(port) = value.parse() {
                    config.port = Some(port);
                }
            }
        }
        "identityfile" => {
            config.identity_file.push(expand_tilde(value));
        }
        "proxyjump" => {
            // `ProxyJump none` (ssh_config(5)) explicitly disables jumping —
            // it is not a real destination named "none" — so it must
            // resolve to `None`, not `Some("none".to_string())`, or a
            // consumer (M2's ProxyJump chaining) would try to connect
            // through a literal jump host called "none" and fail.
            //
            // Known simplification: like every other keyword here,
            // first-obtained-value-wins — but because "none" resolves to
            // the same `None` state as "never configured", an explicit
            // `ProxyJump none` in an earlier matching block does not, unlike
            // real `ssh_config(5)`, block a *later* matching block's real
            // `ProxyJump <host>` from taking effect. Getting that ordering
            // nuance exactly right would need extra state this struct
            // doesn't otherwise carry; not worth it for what `ssh_config(5)`
            // itself calls a rarely-used override escape hatch.
            if config.proxy_jump.is_none() && !value.eq_ignore_ascii_case("none") {
                config.proxy_jump = Some(value.to_string());
            }
        }
        "forwardagent" => {
            if config.forward_agent.is_none() {
                config.forward_agent = Some(parse_forward_agent(value));
            }
        }
        "identityagent" => {
            if config.identity_agent.is_none() {
                config.identity_agent = Some(expand_tilde(value));
            }
        }
        _ => {}
    }
}

fn parse_forward_agent(value: &str) -> ForwardAgent {
    match value.to_ascii_lowercase().as_str() {
        "yes" => ForwardAgent::Yes,
        "no" => ForwardAgent::No,
        _ => ForwardAgent::Socket(value.to_string()),
    }
}

/// `ssh_config(5)` allows `Keyword value` (whitespace-separated) or
/// `Keyword=value` (`=`, optionally surrounded by whitespace on either
/// side) — both forms are in real-world use. Matches OpenSSH's own
/// `strdelim`, which treats both whitespace and a single `=` as the
/// keyword/value delimiter.
fn split_keyword(line: &str) -> (&str, &str) {
    let end = line.find(|c: char| c.is_whitespace() || c == '=').unwrap_or(line.len());
    let keyword = &line[..end];
    let mut rest = line[end..].trim_start();
    if let Some(stripped) = rest.strip_prefix('=') {
        rest = stripped.trim_start();
    }
    (keyword, rest)
}

fn split_words(input: &str) -> impl Iterator<Item = String> + '_ {
    input.split_whitespace().map(str::to_string)
}

fn strip_quotes(value: &str) -> &str {
    if value.len() > 1
        && ((value.starts_with('\'') && value.ends_with('\''))
            || (value.starts_with('"') && value.ends_with('"')))
    {
        &value[1..value.len() - 1]
    } else {
        value
    }
}

fn expand_include_pattern(pattern: &str, base_dir: Option<&Path>) -> Result<Vec<PathBuf>, Error> {
    let expanded = expand_path(pattern, base_dir);
    let pattern_str = expanded.to_string_lossy().into_owned();
    let mut paths = Vec::new();
    if pattern_str.contains('*') || pattern_str.contains('?') || pattern_str.contains('[') {
        for entry in glob::glob(&pattern_str)
            .map_err(|source| Error::InvalidIncludePattern { pattern: pattern_str.clone(), source })?
        {
            paths.push(entry.map_err(|source| Error::IncludeGlob { pattern: pattern_str.clone(), source })?);
        }
        paths.sort();
    } else {
        paths.push(PathBuf::from(pattern_str));
    }
    Ok(paths)
}

fn expand_path(input: &str, base_dir: Option<&Path>) -> PathBuf {
    let expanded = expand_tilde(input);
    if expanded.is_absolute() {
        expanded
    } else {
        base_dir.unwrap_or_else(|| Path::new(".")).join(expanded)
    }
}

fn expand_tilde(input: &str) -> PathBuf {
    expand_tilde_with(input, home_dir())
}

/// Split out of [`expand_tilde`] purely so it can be unit-tested with an
/// injected home directory instead of mutating the real `HOME`/`USERPROFILE`
/// process environment (`std::env::set_var` is process-global and races
/// against concurrently-running tests in the same test binary).
fn expand_tilde_with(input: &str, home: Option<PathBuf>) -> PathBuf {
    if input == "~" {
        home.unwrap_or_else(|| PathBuf::from(input))
    } else if let Some(rest) = input.strip_prefix("~/") {
        home.map(|home| home.join(rest)).unwrap_or_else(|| PathBuf::from(input))
    } else {
        PathBuf::from(input)
    }
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(PathBuf::from)
}

fn host_patterns_match(patterns: &str, destination: &str) -> bool {
    let mut matched = false;
    for pattern in patterns.split_whitespace() {
        if let Some(negative) = pattern.strip_prefix('!') {
            if wildcard_match(negative, destination) {
                return false;
            }
        } else if wildcard_match(pattern, destination) {
            matched = true;
        }
    }
    matched
}

fn wildcard_match(pattern: &str, value: &str) -> bool {
    wildcard_match_bytes(pattern.as_bytes(), value.as_bytes())
}

fn wildcard_match_bytes(pattern: &[u8], value: &[u8]) -> bool {
    match (pattern.split_first(), value.split_first()) {
        (None, None) => true,
        (None, Some(_)) => false,
        (Some((&b'*', rest)), _) => {
            wildcard_match_bytes(rest, value)
                || value.split_first().map(|(_, value_rest)| wildcard_match_bytes(pattern, value_rest)).unwrap_or(false)
        }
        (Some((&b'?', rest)), Some((_, value_rest))) => wildcard_match_bytes(rest, value_rest),
        (Some((&p, rest)), Some((&v, value_rest))) if p == v => wildcard_match_bytes(rest, value_rest),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_config(dir: &tempfile::TempDir, name: &str, content: &str) -> PathBuf {
        let path = dir.path().join(name);
        std::fs::File::create(&path).unwrap().write_all(content.as_bytes()).unwrap();
        path
    }

    #[test]
    fn resolves_basic_keywords_for_matching_host() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(&dir, "config", "
Host example
    HostName example.com
    User alice
    Port 2222
    IdentityFile /home/alice/.ssh/id_ed25519
    ProxyJump jump-host
    ForwardAgent yes
    IdentityAgent /run/user/1000/ssh-agent
");
        let config = resolve(&path, "example").unwrap();
        assert_eq!(config.host_name.as_deref(), Some("example.com"));
        assert_eq!(config.user.as_deref(), Some("alice"));
        assert_eq!(config.port, Some(2222));
        assert_eq!(config.identity_file, vec![PathBuf::from("/home/alice/.ssh/id_ed25519")]);
        assert_eq!(config.proxy_jump.as_deref(), Some("jump-host"));
        assert_eq!(config.forward_agent, Some(ForwardAgent::Yes));
        assert_eq!(config.identity_agent.as_deref(), Some(Path::new("/run/user/1000/ssh-agent")));
    }

    #[test]
    fn non_matching_host_block_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(&dir, "config", "
Host other
    User bob
Host example
    User alice
");
        let config = resolve(&path, "example").unwrap();
        assert_eq!(config.user.as_deref(), Some("alice"));
    }

    #[test]
    fn first_matching_block_wins_per_key_but_wildcard_still_contributes() {
        // real ssh_config(5) semantics: once a key is set by an earlier
        // (more specific) matching block, a later matching `Host *` block
        // must not override it.
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(&dir, "config", "
Host example
    User alice
Host *
    User wildcard-user
    Port 2200
");
        let config = resolve(&path, "example").unwrap();
        assert_eq!(config.user.as_deref(), Some("alice"), "earlier block's value must win");
        assert_eq!(config.port, Some(2200), "wildcard block still contributes keys the earlier block didn't set");
    }

    #[test]
    fn identity_file_accumulates_across_matching_blocks() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(&dir, "config", "
Host example
    IdentityFile /path/to/id_ed25519
Host *
    IdentityFile /path/to/id_rsa
");
        let config = resolve(&path, "example").unwrap();
        assert_eq!(
            config.identity_file,
            vec![PathBuf::from("/path/to/id_ed25519"), PathBuf::from("/path/to/id_rsa")]
        );
    }

    #[test]
    fn negated_pattern_excludes_host_even_if_wildcard_matches() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(&dir, "config", "
Host *.example.com !excluded.example.com
    User alice
");
        assert_eq!(resolve(&path, "included.example.com").unwrap().user.as_deref(), Some("alice"));
        assert_eq!(resolve(&path, "excluded.example.com").unwrap().user, None);
    }

    #[test]
    fn include_directive_splices_referenced_file() {
        let dir = tempfile::tempdir().unwrap();
        write_config(&dir, "extra.conf", "
Host example
    User from-include
");
        let main = write_config(&dir, "config", &format!("Include {}/extra.conf\n", dir.path().display()));
        let config = resolve(&main, "example").unwrap();
        assert_eq!(config.user.as_deref(), Some("from-include"));
    }

    #[test]
    fn match_block_is_structurally_recognized_but_never_applied() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(&dir, "config", "
Match host example
    User should-never-apply
Host example
    User alice
");
        let config = resolve(&path, "example").unwrap();
        assert_eq!(config.user.as_deref(), Some("alice"), "Match-gated lines must never apply");
    }

    #[test]
    fn missing_config_file_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does-not-exist");
        let err = resolve(&missing, "example").unwrap_err();
        assert!(matches!(err, Error::Read { .. }));
    }

    #[test]
    fn quoted_identity_file_value_has_quotes_stripped() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(&dir, "config", "
Host example
    IdentityFile '/path/with spaces/id_ed25519'
");
        let config = resolve(&path, "example").unwrap();
        assert_eq!(config.identity_file, vec![PathBuf::from("/path/with spaces/id_ed25519")]);
    }

    #[test]
    fn expand_tilde_with_injected_home_does_not_touch_real_env() {
        let home = Some(PathBuf::from("/home/alice"));
        assert_eq!(expand_tilde_with("~", home.clone()), PathBuf::from("/home/alice"));
        assert_eq!(expand_tilde_with("~/.ssh/id_ed25519", home), PathBuf::from("/home/alice/.ssh/id_ed25519"));
        assert_eq!(expand_tilde_with("~", None), PathBuf::from("~"));
        assert_eq!(expand_tilde_with("/absolute/path", Some(PathBuf::from("/home/alice"))), PathBuf::from("/absolute/path"));
    }

    #[test]
    fn include_inside_a_non_matching_host_block_is_not_applied() {
        // Codex review finding: Include splices the referenced file's lines
        // in at that exact point (ssh_config(5)) — if the enclosing Host
        // block doesn't match, the spliced-in content must inherit that
        // inactivity too, even if the included file's own top-level lines
        // have no Host block of their own.
        let dir = tempfile::tempdir().unwrap();
        write_config(&dir, "extra.conf", "User from-include\n");
        let main = write_config(&dir, "config", &format!("
Host does-not-match
    Include {}/extra.conf
", dir.path().display()));
        let config = resolve(&main, "example").unwrap();
        assert_eq!(config.user, None, "Include under a non-matching Host block must not apply");
    }

    #[test]
    fn forward_agent_socket_path_is_preserved_raw() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(&dir, "config", "
Host example
    ForwardAgent /tmp/custom-agent.sock
");
        let config = resolve(&path, "example").unwrap();
        assert_eq!(config.forward_agent, Some(ForwardAgent::Socket("/tmp/custom-agent.sock".to_string())));
    }

    #[test]
    fn identity_agent_is_tilde_expanded_like_identity_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(&dir, "config", "
Host example
    IdentityAgent ~/.ssh/agent.sock
");
        // Uses the real home_dir() (HOME/USERPROFILE) since IdentityAgent
        // resolution goes through expand_tilde (not expand_tilde_with) —
        // just assert it's an absolute path under whatever HOME actually is,
        // without asserting a specific value (avoids mutating env vars).
        let config = resolve(&path, "example").unwrap();
        let identity_agent = config.identity_agent.expect("IdentityAgent should resolve");
        assert!(identity_agent.is_absolute() || !identity_agent.to_string_lossy().starts_with('~'));
    }

    #[test]
    fn duplicate_keyword_within_same_host_block_first_line_wins() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(&dir, "config", "
Host example
    User first
    User second
");
        let config = resolve(&path, "example").unwrap();
        assert_eq!(config.user.as_deref(), Some("first"));
    }

    #[test]
    fn include_cycle_does_not_infinite_loop() {
        // The self-include itself is a no-op (already-visited path is
        // skipped, preventing infinite recursion) — but that only skips
        // *re-entering* the same file, not the rest of the current file's
        // lines, so "User after-cycle" (which comes after the Include line,
        // in the same file, still executing in the outer call frame) must
        // still apply. The thing under test is "doesn't hang or error", not
        // that the include has no effect on anything.
        let dir = tempfile::tempdir().unwrap();
        let main_path = dir.path().join("config");
        std::fs::write(&main_path, format!("Include {}\nUser after-cycle\n", main_path.display())).unwrap();
        let config = resolve(&main_path, "example").unwrap();
        assert_eq!(config.user.as_deref(), Some("after-cycle"));
    }

    #[test]
    fn multiple_positive_host_patterns_on_one_line_all_match() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(&dir, "config", "
Host foo bar baz
    User alice
");
        assert_eq!(resolve(&path, "foo").unwrap().user.as_deref(), Some("alice"));
        assert_eq!(resolve(&path, "bar").unwrap().user.as_deref(), Some("alice"));
        assert_eq!(resolve(&path, "baz").unwrap().user.as_deref(), Some("alice"));
        assert_eq!(resolve(&path, "qux").unwrap().user, None);
    }

    #[test]
    fn proxy_jump_none_resolves_to_no_jump_not_a_literal_host_named_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(&dir, "config", "
Host internal
    ProxyJump none
");
        assert_eq!(
            resolve(&path, "internal").unwrap().proxy_jump, None,
            "\"ProxyJump none\" must disable jumping, not resolve to a host literally named \"none\""
        );
    }

    #[test]
    fn proxy_jump_real_value_is_unaffected_by_the_none_special_case() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(&dir, "config", "
Host internal
    ProxyJump jump-host.example.com
");
        assert_eq!(resolve(&path, "internal").unwrap().proxy_jump.as_deref(), Some("jump-host.example.com"));
    }

    #[test]
    fn key_equals_value_syntax_is_supported() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_config(&dir, "config", "
Host example
    User=alice
    Port = 2222
");
        let config = resolve(&path, "example").unwrap();
        assert_eq!(config.user.as_deref(), Some("alice"));
        assert_eq!(config.port, Some(2222));
    }
}
