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

/// Priority-ordered `(env var, name prefix)` pairs `Auto` scans to derive a
/// per-"tab" session key — the first one set (and non-empty) in the
/// environment wins. Every entry here shares the one property that matters:
/// its value is stable for as long as the shell/pane that set it stays
/// alive, *independent* of `isekai-ssh` itself being killed and re-run —
/// exactly the event `--isekai-tty` exists to survive. Deliberately does
/// **not** include a raw pty device path (`tty(1)`/`$SSH_TTY`): pty device
/// numbers are recycled by the kernel once a pane closes, so an unrelated
/// *new* tab can end up allocated the very same number and silently attach
/// to a *different*, still-alive-but-abandoned tab's old daemon — every
/// candidate below is instead an app-owned counter or UUID its owner never
/// reuses within one run.
///
/// - `ISEKAI_TTY_SESSION`: not provided by any terminal — a user opts a
///   plain, unmultiplexed terminal (or any terminal not in this list) into
///   the same behavior by exporting this from `.bashrc`/`.zshrc` (see
///   `README.md`'s `--isekai-tty` section for the exact snippet). Checked
///   first: an explicit opt-in should win over an app-provided heuristic.
/// - `WT_SESSION`: Windows Terminal (a GUID).
/// - `TMUX_PANE`: tmux (`%N`, a permanent id from an ever-incrementing
///   counter — distinct from tmux's *positional* pane index, which is
///   reused when panes are closed and renumbered).
/// - `WEZTERM_PANE`: WezTerm (numeric, also an increasing counter).
/// - `KITTY_WINDOW_ID`: kitty (numeric, assigned by kitty itself, not
///   reused within one kitty instance).
/// - `ITERM_SESSION_ID`: iTerm2 on macOS (UUID-based).
/// - `STY`: GNU Screen — coarser than the others (a *session*-level name,
///   not per-window; multiple windows in one `screen` session collide onto
///   the same key), included anyway as a better-than-nothing fallback ahead
///   of the profile-only default.
const TAB_SESSION_ENV_CANDIDATES: &[(&str, &str)] = &[
    ("ISEKAI_TTY_SESSION", "env"),
    ("WT_SESSION", "wt"),
    ("TMUX_PANE", "tmux"),
    ("WEZTERM_PANE", "wezterm"),
    ("KITTY_WINDOW_ID", "kitty"),
    ("ITERM_SESSION_ID", "iterm"),
    ("STY", "screen"),
];

/// Resolves a `TtySelection` to the actual `<name>` to hand to
/// `isekai-pipe tty attach`. See [`resolve_name_from`] for `Auto`'s actual
/// derivation — this thin wrapper only supplies the real environment.
fn resolve_name(selection: &TtySelection, profile: &str) -> String {
    let candidates: Vec<(&str, Option<String>)> =
        TAB_SESSION_ENV_CANDIDATES.iter().map(|(var, prefix)| (*prefix, std::env::var(var).ok())).collect();
    resolve_name_from(selection, profile, &candidates)
}

/// `Auto`'s derivation, with the candidate env values injected rather than
/// read directly — lets tests exercise the priority order deterministically
/// instead of depending on which of these vars the test process happens to
/// have set (this crate's `HOME_ENV_LOCK` in `main.rs` exists precisely
/// because mutating real process-global env state under `cargo test`'s
/// default multi-threaded runner is a real flakiness hazard; reading a var
/// directly in a function under test has the same problem in miniature, so
/// this sidesteps it by injection instead).
///
/// `candidates` must be in the same priority order as
/// [`TAB_SESSION_ENV_CANDIDATES`] (the real caller, [`resolve_name`],
/// builds it from exactly that list) — the first entry with a non-empty
/// value wins, becoming `<prefix>-<value>`. Falls back to `isekai-<profile>`
/// (one daemon per host, no tab distinction — the original, pre-tab-aware
/// behavior) when none of them are set, which is what a plain terminal with
/// no multiplexer and no `ISEKAI_TTY_SESSION` opt-in still gets.
fn resolve_name_from(selection: &TtySelection, profile: &str, candidates: &[(&str, Option<String>)]) -> String {
    match selection {
        TtySelection::Auto => {
            let tab_session = candidates.iter().find_map(|(prefix, value)| {
                let value = value.as_deref()?;
                if value.is_empty() {
                    None
                } else {
                    Some(format!("{prefix}-{value}"))
                }
            });
            tab_session.unwrap_or_else(|| format!("isekai-{profile}"))
        }
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

    /// Builds a `candidates` slice shaped like [`TAB_SESSION_ENV_CANDIDATES`]
    /// but with only the named prefixes set, for tests that only care about
    /// a couple of entries — every other slot is `None`, exactly as an
    /// unset env var would resolve to.
    fn candidates_with(set: &[(&str, &str)]) -> Vec<(&'static str, Option<String>)> {
        TAB_SESSION_ENV_CANDIDATES
            .iter()
            .map(|(_var, prefix)| (*prefix, set.iter().find(|(p, _)| p == prefix).map(|(_, v)| v.to_string())))
            .collect()
    }

    #[test]
    fn auto_derives_a_name_from_the_profile_when_nothing_is_set() {
        assert_eq!(resolve_name_from(&TtySelection::Auto, "prod", &candidates_with(&[])), "isekai-prod");
    }

    #[test]
    fn auto_uses_wt_session_when_present() {
        assert_eq!(
            resolve_name_from(&TtySelection::Auto, "prod", &candidates_with(&[("wt", "50295e02-2ea3-4f92")])),
            "wt-50295e02-2ea3-4f92"
        );
    }

    #[test]
    fn auto_uses_tmux_pane_when_present() {
        assert_eq!(resolve_name_from(&TtySelection::Auto, "prod", &candidates_with(&[("tmux", "%37")])), "tmux-%37");
    }

    #[test]
    fn auto_falls_back_to_screen_sty_as_a_last_resort() {
        assert_eq!(
            resolve_name_from(&TtySelection::Auto, "prod", &candidates_with(&[("screen", "1234.pts-2.host")])),
            "screen-1234.pts-2.host"
        );
    }

    #[test]
    fn auto_prefers_the_explicit_opt_in_over_an_app_provided_signal() {
        // ISEKAI_TTY_SESSION is checked first in TAB_SESSION_ENV_CANDIDATES
        // — an explicit .bashrc/.zshrc opt-in should win over whatever
        // terminal-provided heuristic also happens to be set.
        assert_eq!(
            resolve_name_from(&TtySelection::Auto, "prod", &candidates_with(&[("env", "user-chosen-uuid"), ("wt", "some-guid")])),
            "env-user-chosen-uuid"
        );
    }

    #[test]
    fn auto_prefers_an_earlier_candidate_over_a_later_one_when_both_are_set() {
        // WT_SESSION precedes TMUX_PANE in the priority list.
        assert_eq!(
            resolve_name_from(&TtySelection::Auto, "prod", &candidates_with(&[("tmux", "%37"), ("wt", "some-guid")])),
            "wt-some-guid"
        );
    }

    #[test]
    fn auto_falls_back_to_the_profile_for_an_empty_value() {
        // Defensive: an env var can technically be set-but-empty (distinct
        // from unset). Treat that the same as unset rather than emitting
        // the degenerate name "wt-".
        assert_eq!(resolve_name_from(&TtySelection::Auto, "prod", &candidates_with(&[("wt", "")])), "isekai-prod");
    }

    #[test]
    fn named_uses_the_explicit_name_verbatim_regardless_of_env_candidates() {
        assert_eq!(
            resolve_name_from(&TtySelection::Named("work".to_string()), "prod", &candidates_with(&[("wt", "some-guid")])),
            "work"
        );
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
