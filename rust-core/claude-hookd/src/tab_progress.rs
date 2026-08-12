//! Resolves a `(state, progress)` progress-bar request to the actual OSC
//! escape sequence bytes by shelling out to a script — same shape as
//! [`super::tab_color`], for the same reason: the terminal-kind →
//! escape-sequence mapping can be customized/extended without recompiling
//! Rust, git-hooks style.
//!
//! `~/.config/claude-hookd/hooks/tab-progress`, if present and executable,
//! is used in place of the embedded default — same `hooks_dir` this crate
//! already resolves for [`super::hooks::run_hook`]/[`super::tab_color`], and
//! the same opt-in/silent-fallback shape. The embedded default
//! (`default-tab-progress.sh`, embedded via `include_str!` below) mirrors
//! `default-tab-color.sh`'s terminal-kind detection and default policy
//! byte-for-byte — see that file's own doc comment.
//!
//! Like [`super::tab_color::resolve`], any script here runs with *this
//! daemon process's* environment, not the triggering session's, and every
//! spawn is bounded by [`SCRIPT_TIMEOUT`] with `kill_on_drop(true)` — this
//! call sits inline in `daemon.rs`'s `execute_actions`, so a hanging script
//! must not stall the whole event loop.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use isekai_protocol::ProgressState;

/// Same bound as [`super::tab_color::SCRIPT_TIMEOUT`] — a script here should
/// be near-instant, and a hanging one must only ever delay one progress
/// update by this much before `daemon.rs`'s event loop moves on.
const SCRIPT_TIMEOUT: Duration = Duration::from_secs(2);

/// Source of truth for the embedded default's actual shell — see that
/// file's own doc comment for the terminal-kind resolution it reproduces.
const DEFAULT_SCRIPT: &str = include_str!("../default-tab-progress.sh");

/// Resolves `(state, progress)` to the raw (unwrapped — [`super::delivery`]
/// wraps for tmux passthrough itself, when needed) OSC escape sequence for
/// whichever terminal is on the other end.
///
/// Failure modes mirror [`super::tab_color::resolve`] exactly: a *custom*
/// override script that fails to spawn, times out, exits non-zero, or
/// produces non-UTF-8 output resolves to an empty string (a silent no-op,
/// treated as that script author's own deliberate downgrade decision — not
/// something this function second-guesses by falling back to the embedded
/// default). Only the *embedded default* failing outright (no `/bin/sh` at
/// all) falls back to [`compiled_in_fallback`].
pub(crate) async fn resolve(hooks_dir: Option<&Path>, state: ProgressState, progress: u8) -> String {
    let state_arg = (state as u8).to_string();
    let progress_arg = progress.to_string();
    if let Some(hooks_dir) = hooks_dir {
        let custom = hooks_dir.join("tab-progress");
        if super::hooks::is_executable(&custom).await {
            return run(&custom, &state_arg, &progress_arg).await.unwrap_or_default();
        }
    }
    match run_embedded_default(&state_arg, &progress_arg, None).await {
        Some(seq) => seq,
        None => compiled_in_fallback(state, progress),
    }
}

/// The OSC 9;4 sequence, compiled directly into this binary — the same
/// sequence [`DEFAULT_SCRIPT`] produces when it runs successfully (for any
/// terminal kind other than iTerm2), used only when the script itself
/// couldn't run at all. Same "always the Windows Terminal convention,
/// harmless no-op elsewhere" posture as [`super::tab_color::compiled_in_fallback`]
/// — deliberately does *not* special-case iTerm2 here (unlike the embedded
/// script, which can), since this is the last-resort path for a genuinely
/// broken environment, not a policy decision.
fn compiled_in_fallback(state: ProgressState, progress: u8) -> String {
    format!("\x1b]9;4;{};{}\x07", state as u8, progress)
}

async fn run(path: &Path, state_arg: &str, progress_arg: &str) -> Option<String> {
    let mut cmd = tokio::process::Command::new(path);
    cmd.arg(state_arg).arg(progress_arg).stdin(Stdio::null()).kill_on_drop(true);
    output_of(cmd).await
}

/// Runs [`DEFAULT_SCRIPT`] via `sh -c` — see
/// [`super::tab_color::run_embedded_default`] for why (no filesystem
/// footprint, no risk of racing a concurrent daemon instance). Same `$0`
/// placeholder trick: `sh -c script name args...`'s first post-script
/// argument becomes `$0`, not `$1`, hence the literal `"tab-progress"`
/// placeholder before the real `state_arg`/`progress_arg`.
///
/// `env_override`, when `Some`, replaces `$TERM_PROGRAM`/`$ISEKAI_TERMINAL_KIND`
/// with exactly the given pairs instead of inheriting this process's real
/// values — test-only, same reasoning as `tab_color.rs`'s twin function.
async fn run_embedded_default(state_arg: &str, progress_arg: &str, env_override: Option<&[(&str, &str)]>) -> Option<String> {
    let mut cmd = tokio::process::Command::new("sh");
    cmd.arg("-c").arg(DEFAULT_SCRIPT).arg("tab-progress").arg(state_arg).arg(progress_arg).stdin(Stdio::null()).kill_on_drop(true);
    if let Some(pairs) = env_override {
        cmd.env_remove("TERM_PROGRAM").env_remove("ISEKAI_TERMINAL_KIND").envs(pairs.iter().copied());
    }
    output_of(cmd).await
}

async fn output_of(mut cmd: tokio::process::Command) -> Option<String> {
    let output = tokio::time::timeout(SCRIPT_TIMEOUT, cmd.output()).await.ok()?.ok()?;
    output.status.success().then(|| String::from_utf8(output.stdout).ok()).flatten()
}

/// Mirrors [`resolve`]'s own logic exactly, except the embedded-default leg
/// is env-isolated instead of inheriting this test *process*'s real
/// `$TERM_PROGRAM` — used by this module's own tests and by
/// [`super::delivery`]'s (`pub(crate)` so both can reach it), same reason as
/// `tab_color.rs::resolve_with_isolated_env`.
#[cfg(test)]
pub(crate) async fn resolve_with_isolated_env(hooks_dir: Option<&Path>, state: ProgressState, progress: u8, env_override: &[(&str, &str)]) -> String {
    let state_arg = (state as u8).to_string();
    let progress_arg = progress.to_string();
    if let Some(hooks_dir) = hooks_dir {
        let custom = hooks_dir.join("tab-progress");
        if super::hooks::is_executable(&custom).await {
            return run(&custom, &state_arg, &progress_arg).await.unwrap_or_default();
        }
    }
    match run_embedded_default(&state_arg, &progress_arg, Some(env_override)).await {
        Some(seq) => seq,
        None => compiled_in_fallback(state, progress),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn embedded_default_matches_windows_terminal_convention_when_unset() {
        let seq = resolve_with_isolated_env(None, ProgressState::Indeterminate, 0, &[]).await;
        assert_eq!(seq, "\x1b]9;4;3;0\x07");
    }

    #[tokio::test]
    async fn embedded_default_is_empty_for_iterm2_via_term_program() {
        let seq = resolve_with_isolated_env(None, ProgressState::Indeterminate, 0, &[("TERM_PROGRAM", "iTerm.app")]).await;
        assert_eq!(seq, "", "iTerm2 has no OSC 9;4 progress convention; sending one anyway pops a spurious notification");
    }

    #[tokio::test]
    async fn embedded_default_explicit_override_wins_over_term_program() {
        let seq = resolve_with_isolated_env(
            None,
            ProgressState::Normal,
            42,
            &[("ISEKAI_TERMINAL_KIND", "windows-terminal"), ("TERM_PROGRAM", "iTerm.app")],
        )
        .await;
        assert_eq!(seq, "\x1b]9;4;1;42\x07");
    }

    #[tokio::test]
    async fn clearing_progress_sends_state_zero() {
        let seq = resolve_with_isolated_env(None, ProgressState::None, 0, &[]).await;
        assert_eq!(seq, "\x1b]9;4;0;0\x07");
    }

    #[tokio::test]
    async fn no_hooks_dir_uses_the_embedded_default() {
        let seq = resolve_with_isolated_env(None, ProgressState::Error, 0, &[]).await;
        assert_eq!(seq, "\x1b]9;4;2;0\x07");
    }

    #[tokio::test]
    async fn hooks_dir_without_a_tab_progress_script_falls_back_to_the_embedded_default() {
        let dir = tempfile::tempdir().unwrap();
        let seq = resolve_with_isolated_env(Some(dir.path()), ProgressState::Indeterminate, 0, &[]).await;
        assert_eq!(seq, "\x1b]9;4;3;0\x07");
    }

    #[tokio::test]
    async fn a_custom_tab_progress_script_overrides_the_embedded_default() {
        let dir = tempfile::tempdir().unwrap();
        let script_path = dir.path().join("tab-progress");
        std::fs::write(&script_path, "#!/bin/sh\nprintf 'CUSTOM:%s:%s' \"$1\" \"$2\"\n").unwrap();
        use std::os::unix::fs::PermissionsExt as _;
        let mut perms = std::fs::metadata(&script_path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script_path, perms).unwrap();

        let seq = resolve(Some(dir.path()), ProgressState::Normal, 77).await;
        assert_eq!(seq, "CUSTOM:1:77");
    }

    #[tokio::test]
    async fn a_non_executable_tab_progress_script_is_ignored_falling_back_to_the_embedded_default() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("tab-progress"), "#!/bin/sh\nprintf 'should-not-run'\n").unwrap();
        // Deliberately left at the default (non-executable) mode.

        let seq = resolve_with_isolated_env(Some(dir.path()), ProgressState::Indeterminate, 0, &[]).await;
        assert_eq!(seq, "\x1b]9;4;3;0\x07");
    }

    #[tokio::test]
    async fn a_custom_script_that_exits_non_zero_resolves_to_empty_not_the_default() {
        let dir = tempfile::tempdir().unwrap();
        let script_path = dir.path().join("tab-progress");
        std::fs::write(&script_path, "#!/bin/sh\nexit 1\n").unwrap();
        use std::os::unix::fs::PermissionsExt as _;
        let mut perms = std::fs::metadata(&script_path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script_path, perms).unwrap();

        let seq = resolve(Some(dir.path()), ProgressState::Indeterminate, 0).await;
        assert_eq!(seq, "");
    }

    #[tokio::test]
    async fn a_hanging_custom_script_times_out_instead_of_blocking_forever() {
        let dir = tempfile::tempdir().unwrap();
        let script_path = dir.path().join("tab-progress");
        std::fs::write(&script_path, "#!/bin/sh\nsleep 30\n").unwrap();
        use std::os::unix::fs::PermissionsExt as _;
        let mut perms = std::fs::metadata(&script_path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script_path, perms).unwrap();

        let seq = tokio::time::timeout(Duration::from_secs(5), resolve(Some(dir.path()), ProgressState::Indeterminate, 0))
            .await
            .expect("resolve() must itself time out well within 5s, not hang on the 30s sleep");
        assert_eq!(seq, "", "a timed-out custom script is the same silent no-op as any other failure");
    }

    #[tokio::test]
    async fn embedded_default_failing_to_run_at_all_falls_back_to_the_compiled_in_sequence() {
        let seq =
            resolve_with_isolated_env(None, ProgressState::Indeterminate, 0, &[("PATH", "/nonexistent-dir-for-this-test-xyz")]).await;
        assert_eq!(seq, compiled_in_fallback(ProgressState::Indeterminate, 0));
        assert_eq!(seq, "\x1b]9;4;3;0\x07");
    }
}
