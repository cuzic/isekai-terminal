//! Resolves `--isekai-tty[=<name>]` (`wrapper::TtySelection`) into the
//! `isekai-pipe tty attach <name>` remote command string, composed with
//! ctl-socket's login-shell builder via `ctl_forward::build_login_shell_command`'s
//! `exec_target` parameter — see that function's doc comment for how the two
//! features share one remote command line instead of one silently disabling
//! the other.
//!
//! This module mirrors the `tmux_session.rs` module from the abandoned
//! `--isekai-tmux` design (`feat/isekai-ssh-tmux-autobind`, superseded by the
//! self-owned `isekai-pipe tty daemon`/`tty attach` pty-persistence daemon
//! this crate now wires up instead of tmux) — same shape, same reuse of
//! `wrapper::shell_quote` for the untrusted `<name>` value.

use crate::wrapper::TtySelection;

/// Resolves a `TtySelection` to the actual `<name>` to hand to
/// `isekai-pipe tty attach`. See [`resolve_name_from`] for `Auto`'s actual
/// derivation — this thin wrapper only supplies the real `$WT_SESSION`.
fn resolve_name(selection: &TtySelection, profile: &str) -> String {
    resolve_name_from(selection, profile, std::env::var("WT_SESSION").ok().as_deref())
}

/// `Auto`'s derivation, with `$WT_SESSION` injected rather than read
/// directly — lets tests exercise both branches deterministically instead of
/// depending on whether the test process happens to have it set (this
/// crate's `HOME_ENV_LOCK` in `main.rs` exists precisely because mutating
/// real process-global env state under `cargo test`'s default multi-threaded
/// runner is a real flakiness hazard; reading a var directly in a function
/// under test has the same problem in miniature, so this sidesteps it by
/// injection instead).
///
/// `Windows Terminal` sets `$WT_SESSION` to a GUID that stays the same for
/// the lifetime of one pane/tab, independent of whatever child process is
/// currently running in it — unlike anything `isekai-ssh` itself controls,
/// this survives the exact "local process gets killed and re-run" event
/// `--isekai-tty` exists to survive, which is what makes it a good default
/// session key: re-running `isekai-ssh --isekai-tty host` from the *same*
/// Windows Terminal tab reliably lands back on the *same* remote daemon, and
/// a *different* tab connected to the *same* host gets a distinct one
/// (`wt-<session>`) — no user-chosen name, and no ambiguity between tabs to
/// resolve with a picker (considered and set aside: WT_SESSION already
/// disambiguates this deterministically when it's available).
///
/// Falls back to `isekai-<profile>` (one daemon per host, no tab
/// distinction — the pre-WT_SESSION behavior, and what every non-Windows-Terminal
/// environment still gets, e.g. tmux, a plain Linux/macOS terminal, or an
/// older Windows Terminal release without the variable) when `$WT_SESSION`
/// isn't set. Scoped to Windows Terminal only, deliberately: no other
/// terminal this project has looked at exposes an equivalently stable
/// per-pane identity a *child* process can read after being killed and
/// restarted, and building a cross-terminal picker for the ambiguous case
/// was explicitly set aside as unnecessary scope for now.
fn resolve_name_from(selection: &TtySelection, profile: &str, wt_session: Option<&str>) -> String {
    match selection {
        TtySelection::Auto => match wt_session {
            Some(session) if !session.is_empty() => format!("wt-{session}"),
            _ => format!("isekai-{profile}"),
        },
        TtySelection::Named(name) => name.clone(),
    }
}

/// The `isekai-pipe tty attach <name>` command string, safely quoted —
/// `<name>` can come straight from a `--isekai-tty=<name>` CLI argument, so
/// it must be treated as untrusted shell input the same way any other
/// `#@isekai` directive value already is (`wrapper::shell_quote`).
/// `isekai-pipe`'s own `tty::validate_name` is the authoritative rejection
/// of a structurally invalid name (path traversal, embedded NUL, etc.) —
/// this function's job is narrower: whatever name reaches the remote shell
/// must not be able to break out of the single-quoted argument it's placed
/// in.
pub(crate) fn attach_command(name: &str) -> String {
    format!("isekai-pipe tty attach {}", crate::wrapper::shell_quote(name))
}

/// `apply_ctl_socket_forward`/`native::connect::run_authenticated_session`/
/// `native::mux::mod::run_as_client_over`'s shared entry point: `None` when
/// `--isekai-tty` wasn't given, `Some(command)` otherwise. Callers are
/// responsible for only calling this when there is no explicit trailing
/// remote command already (`WrapperPlan::remote_command().is_none()`) — see
/// `WrapperPlan::tty_selection`'s doc comment for why that check lives at
/// each call site rather than here.
pub(crate) fn resolve_exec_command(selection: Option<&TtySelection>, profile: &str) -> Option<String> {
    selection.map(|selection| attach_command(&resolve_name(selection, profile)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_derives_a_name_from_the_profile_without_wt_session() {
        assert_eq!(resolve_name_from(&TtySelection::Auto, "prod", None), "isekai-prod");
    }

    #[test]
    fn auto_prefers_wt_session_over_the_profile_when_present() {
        assert_eq!(resolve_name_from(&TtySelection::Auto, "prod", Some("50295e02-2ea3-4f92")), "wt-50295e02-2ea3-4f92");
    }

    #[test]
    fn auto_falls_back_to_the_profile_for_an_empty_wt_session() {
        // Defensive: an env var can technically be set-but-empty (distinct
        // from unset). Treat that the same as unset rather than emitting the
        // degenerate name "wt-".
        assert_eq!(resolve_name_from(&TtySelection::Auto, "prod", Some("")), "isekai-prod");
    }

    #[test]
    fn named_uses_the_explicit_name_verbatim_regardless_of_wt_session() {
        assert_eq!(resolve_name_from(&TtySelection::Named("work".to_string()), "prod", Some("some-guid")), "work");
    }

    #[test]
    fn attach_command_safely_quotes_a_hostile_name() {
        let cmd = attach_command("$(rm -rf /); evil'; exec sh");
        // The single embedded `'` must become POSIX single-quoting's standard
        // close-quote/escaped-quote/reopen-quote sequence (`'\''`), not be
        // left bare (which really would prematurely close the argument) —
        // computed by hand from `wrapper::shell_quote`'s documented
        // algorithm, not just pattern-matched against a substring that (as
        // an earlier version of this test wrongly did) can coincidentally
        // appear as part of a *correctly* escaped string too.
        assert_eq!(cmd, "isekai-pipe tty attach '$(rm -rf /); evil'\\''; exec sh'");
    }

    #[test]
    fn resolve_exec_command_is_none_without_a_selection() {
        assert_eq!(resolve_exec_command(None, "prod"), None);
    }

    #[test]
    fn resolve_exec_command_composes_auto_and_the_profile() {
        assert_eq!(
            resolve_exec_command(Some(&TtySelection::Auto), "prod"),
            Some("isekai-pipe tty attach 'isekai-prod'".to_string())
        );
    }
}
