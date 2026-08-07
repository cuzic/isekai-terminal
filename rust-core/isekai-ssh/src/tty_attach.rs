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
/// `isekai-pipe tty attach`. `Auto` derives `isekai-<profile>` — one daemon
/// per host by default, mirroring the abandoned `--isekai-tmux` design's own
/// `Auto` default.
fn resolve_name(selection: &TtySelection, profile: &str) -> String {
    match selection {
        TtySelection::Auto => format!("isekai-{profile}"),
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
    fn auto_derives_a_name_from_the_profile() {
        assert_eq!(resolve_name(&TtySelection::Auto, "prod"), "isekai-prod");
    }

    #[test]
    fn named_uses_the_explicit_name_verbatim() {
        assert_eq!(resolve_name(&TtySelection::Named("work".to_string()), "prod"), "work");
    }

    #[test]
    fn attach_command_safely_quotes_a_hostile_name() {
        let cmd = attach_command("$(rm -rf /); evil'; exec sh");
        assert!(!cmd.contains("'; exec sh'"), "an embedded single quote must not prematurely close the quoting: {cmd:?}");
        assert!(cmd.starts_with("isekai-pipe tty attach '"));
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
