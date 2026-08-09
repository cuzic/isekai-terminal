//! Resolves an `(r, g, b)` tab-color request to the actual OSC escape
//! sequence bytes by shelling out to a script, rather than a compiled-in
//! terminal-kind-to-OSC mapping — replacing the former `osc-color` crate
//! dependency (removed 2026-08). Same motivation as [`super::hooks`]'s
//! command hooks: the terminal-kind → escape-sequence mapping can now be
//! customized/extended without recompiling Rust, git-hooks style.
//!
//! `~/.config/claude-hookd/hooks/tab-color`, if present and executable, is
//! used in place of the embedded default — same `hooks_dir` this crate
//! already resolves for [`super::hooks::run_hook`], and the same
//! opt-in/silent-fallback shape. The embedded default reproduces the removed
//! `osc-color` crate's `TerminalKind::resolve()` + `tab_color_sequence()`
//! behavior byte-for-byte (see `default-tab-color.sh` at this crate's root,
//! embedded via `include_str!` below), so a zero-config install sees no
//! behavior change from before this crate stopped depending on that crate.
//!
//! Like [`super::hooks::run_hook`]'s command hooks, any script here runs
//! with *this daemon process's* environment (inherited from whichever hook
//! event won the spawn race — see `main.rs::spawn_detached_daemon`), not the
//! environment of the Claude Code session whose transition triggered this
//! call. A custom `tab-color` script that wants to read `$TERM_PROGRAM`
//! itself should keep that in mind — it's reading the daemon's view, not the
//! triggering session's.
//!
//! Unlike `run_hook`'s fire-and-forget hooks, this one's output is actually
//! needed (the OSC bytes to write), so it cannot be fully detached — but
//! [`resolve`] still bounds every spawned script with [`SCRIPT_TIMEOUT`] and
//! `kill_on_drop(true)`, because unlike `run_hook` this call sits directly in
//! `daemon.rs`'s `execute_actions`, awaited inline in the main event loop: a
//! hanging script here would otherwise stall attention timeouts, idle-exit,
//! and every subsequent hook event, not just delay one color update
//! (adversarial review, 2026-08-09, caught this missing before it shipped).

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

/// Generous for a script that should be near-instant, short enough that a
/// hanging `tab-color` script can only ever delay one color update by this
/// much before this daemon's event loop moves on (see this module's docs on
/// why a bare, un-timed-out `.await` here would be far worse than
/// `hooks.rs`'s already-detached command hooks).
const SCRIPT_TIMEOUT: Duration = Duration::from_secs(2);

/// Source of truth for the embedded default's actual shell — see that file's
/// own doc comment for the terminal-kind resolution it reproduces.
const DEFAULT_SCRIPT: &str = include_str!("../default-tab-color.sh");

/// Resolves `(r, g, b)` to the raw (unwrapped — [`super::delivery`] wraps
/// for tmux passthrough itself, when needed) OSC escape sequence for
/// whichever terminal is on the other end.
///
/// A *custom* override script (`hooks_dir/tab-color`) that fails to spawn,
/// times out, exits non-zero, or produces non-UTF-8 output resolves to an
/// empty string — [`super::delivery`] already treats "nothing to write" as
/// a silent no-op, and a failing custom script is that script author's own
/// business, same trust model as `hooks.rs`'s command hooks.
///
/// The *embedded default* failing outright (no `/bin/sh`, no `cut` — a
/// genuinely broken environment, not a deliberate downgrade decision, which
/// only a real override script can express) instead falls back to
/// [`compiled_in_fallback`]: this crate's one remaining compiled-in OSC
/// sequence, so a minimal/unusual container doesn't silently lose tab
/// coloring entirely just because `sh` is missing.
pub(crate) async fn resolve(hooks_dir: Option<&Path>, r: u8, g: u8, b: u8) -> String {
    let hex = format!("{r:02x}{g:02x}{b:02x}");
    if let Some(hooks_dir) = hooks_dir {
        let custom = hooks_dir.join("tab-color");
        if super::hooks::is_executable(&custom).await {
            return run(&custom, &hex).await.unwrap_or_default();
        }
    }
    match run_embedded_default(&hex, None).await {
        Some(seq) => seq,
        None => compiled_in_fallback(r, g, b),
    }
}

/// Windows Terminal's `OSC 4;264` convention, compiled directly into this
/// binary — the same sequence [`DEFAULT_SCRIPT`] produces when it runs
/// successfully, used only when the script *itself* couldn't run at all.
/// Matches this repository's "always connects" principle: a cosmetic
/// feature failing outright because its shell dependency is missing is
/// worse than falling back to a sequence that (per the removed `osc-color`
/// crate's own docs) is a harmless no-op on terminals that don't recognize
/// it anyway.
fn compiled_in_fallback(r: u8, g: u8, b: u8) -> String {
    format!("\x1b]4;264;rgb:{r:02x}/{g:02x}/{b:02x}\x1b\\")
}

async fn run(path: &Path, hex: &str) -> Option<String> {
    let mut cmd = tokio::process::Command::new(path);
    cmd.arg(hex).stdin(Stdio::null()).kill_on_drop(true);
    output_of(cmd).await
}

/// Runs [`DEFAULT_SCRIPT`] via `sh -c` rather than writing it to a temp file
/// and executing that — no filesystem footprint, and no risk of racing a
/// concurrent daemon instance over a shared temp path. `sh -c script name
/// args...`'s first post-script argument becomes `$0` inside the script
/// (POSIX), not `$1` — hence the literal `"tab-color"` placeholder before
/// the real `hex` argument, so `$1` inside `default-tab-color.sh` is the
/// hex color, matching the same `argv[1]` contract a custom override script
/// gets via [`run`].
///
/// `env_override`, when `Some`, replaces `$TERM_PROGRAM`/`$ISEKAI_TERMINAL_KIND`
/// with exactly the given pairs instead of inheriting this process's real
/// values — test-only (production always passes `None`, i.e. "inherit
/// normally"): mutating this whole *process*'s environment via
/// `std::env::set_var` to test terminal-kind detection would race every
/// other test reading those two vars concurrently (`cargo test` runs in
/// parallel within one process by default), so tests instead override just
/// the child's view of them. Only those two vars are ever touched — not a
/// full `env_clear()`, which also drops `$PATH` and breaks the script's own
/// `cut` call (found live, 2026-08-09).
async fn run_embedded_default(hex: &str, env_override: Option<&[(&str, &str)]>) -> Option<String> {
    let mut cmd = tokio::process::Command::new("sh");
    cmd.arg("-c").arg(DEFAULT_SCRIPT).arg("tab-color").arg(hex).stdin(Stdio::null()).kill_on_drop(true);
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
/// is env-isolated (see [`run_embedded_default`]'s docs) instead of
/// inheriting this test *process*'s real `$TERM_PROGRAM` — used by this
/// module's own tests and by [`super::delivery`]'s (`pub(crate)` so both can
/// reach it). An adversarial review (2026-08-09) caught that tests going
/// through bare `resolve()` only passed because *this particular sandbox*
/// happens to run under tmux (`$TERM_PROGRAM=tmux`, not `iTerm.app`); on a
/// real macOS iTerm2 machine outside tmux they would have failed every time.
#[cfg(test)]
pub(crate) async fn resolve_with_isolated_env(hooks_dir: Option<&Path>, r: u8, g: u8, b: u8, env_override: &[(&str, &str)]) -> String {
    let hex = format!("{r:02x}{g:02x}{b:02x}");
    if let Some(hooks_dir) = hooks_dir {
        let custom = hooks_dir.join("tab-color");
        if super::hooks::is_executable(&custom).await {
            return run(&custom, &hex).await.unwrap_or_default();
        }
    }
    match run_embedded_default(&hex, Some(env_override)).await {
        Some(seq) => seq,
        None => compiled_in_fallback(r, g, b),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn embedded_default_matches_windows_terminal_convention_when_unset() {
        let seq = resolve_with_isolated_env(None, 0xff, 0x00, 0x00, &[]).await;
        assert_eq!(seq, "\x1b]4;264;rgb:ff/00/00\x1b\\");
    }

    #[tokio::test]
    async fn embedded_default_matches_iterm2_convention_via_term_program() {
        let seq = resolve_with_isolated_env(None, 0xff, 0x88, 0x00, &[("TERM_PROGRAM", "iTerm.app")]).await;
        assert_eq!(seq, "\x1b]6;1;bg;red;brightness;255\x07\x1b]6;1;bg;green;brightness;136\x07\x1b]6;1;bg;blue;brightness;0\x07");
    }

    #[tokio::test]
    async fn embedded_default_explicit_override_wins_over_term_program() {
        let seq =
            resolve_with_isolated_env(None, 0x00, 0xff, 0x00, &[("ISEKAI_TERMINAL_KIND", "windows-terminal"), ("TERM_PROGRAM", "iTerm.app")])
                .await;
        assert_eq!(seq, "\x1b]4;264;rgb:00/ff/00\x1b\\");
    }

    #[tokio::test]
    async fn no_hooks_dir_uses_the_embedded_default() {
        let seq = resolve_with_isolated_env(None, 0x12, 0x34, 0x56, &[]).await;
        assert_eq!(seq, "\x1b]4;264;rgb:12/34/56\x1b\\");
    }

    #[tokio::test]
    async fn hooks_dir_without_a_tab_color_script_falls_back_to_the_embedded_default() {
        let dir = tempfile::tempdir().unwrap();
        let seq = resolve_with_isolated_env(Some(dir.path()), 0x12, 0x34, 0x56, &[]).await;
        assert_eq!(seq, "\x1b]4;264;rgb:12/34/56\x1b\\");
    }

    #[tokio::test]
    async fn a_custom_tab_color_script_overrides_the_embedded_default() {
        let dir = tempfile::tempdir().unwrap();
        let script_path = dir.path().join("tab-color");
        std::fs::write(&script_path, "#!/bin/sh\nprintf 'CUSTOM:%s' \"$1\"\n").unwrap();
        use std::os::unix::fs::PermissionsExt as _;
        let mut perms = std::fs::metadata(&script_path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script_path, perms).unwrap();

        let seq = resolve(Some(dir.path()), 0xab, 0xcd, 0xef).await;
        assert_eq!(seq, "CUSTOM:abcdef");
    }

    #[tokio::test]
    async fn a_non_executable_tab_color_script_is_ignored_falling_back_to_the_embedded_default() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("tab-color"), "#!/bin/sh\nprintf 'should-not-run'\n").unwrap();
        // Deliberately left at the default (non-executable) mode.

        let seq = resolve_with_isolated_env(Some(dir.path()), 0x11, 0x22, 0x33, &[]).await;
        assert_eq!(seq, "\x1b]4;264;rgb:11/22/33\x1b\\");
    }

    #[tokio::test]
    async fn a_custom_script_that_exits_non_zero_resolves_to_empty_not_the_default() {
        // A user's override script failing is that user's own downgrade
        // decision (e.g. "I know this terminal, and it's not safe to send
        // anything") — falling back to the embedded default here would
        // silently override that choice.
        let dir = tempfile::tempdir().unwrap();
        let script_path = dir.path().join("tab-color");
        std::fs::write(&script_path, "#!/bin/sh\nexit 1\n").unwrap();
        use std::os::unix::fs::PermissionsExt as _;
        let mut perms = std::fs::metadata(&script_path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script_path, perms).unwrap();

        let seq = resolve(Some(dir.path()), 0x11, 0x22, 0x33).await;
        assert_eq!(seq, "");
    }

    /// Pins the critical fix (adversarial review, 2026-08-09): before this,
    /// `resolve` awaited a spawned script with no timeout at all, and
    /// `daemon.rs::execute_actions` awaits `resolve` inline in the main
    /// `select!` loop — a hanging custom script would have stalled *every*
    /// subsequent event (attention timeouts, idle-exit, new hook events),
    /// not just delayed one color update. `hooks.rs`'s command hooks avoid
    /// this by fully detaching; this one can't (its output is needed), so
    /// it must bound the wait instead.
    #[tokio::test]
    async fn a_hanging_custom_script_times_out_instead_of_blocking_forever() {
        let dir = tempfile::tempdir().unwrap();
        let script_path = dir.path().join("tab-color");
        std::fs::write(&script_path, "#!/bin/sh\nsleep 30\n").unwrap();
        use std::os::unix::fs::PermissionsExt as _;
        let mut perms = std::fs::metadata(&script_path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script_path, perms).unwrap();

        let seq = tokio::time::timeout(Duration::from_secs(5), resolve(Some(dir.path()), 0x11, 0x22, 0x33))
            .await
            .expect("resolve() must itself time out well within 5s, not hang on the 30s sleep");
        assert_eq!(seq, "", "a timed-out custom script is the same silent no-op as any other failure");
    }

    #[tokio::test]
    async fn embedded_default_failing_to_run_at_all_falls_back_to_the_compiled_in_sequence() {
        // Simulates "sh isn't available" by using a `hooks_dir` override
        // that also can't run — proves the *embedded default's own*
        // execution failure (not a custom script's) reaches
        // `compiled_in_fallback` rather than collapsing to empty like a
        // custom script's failure does.
        assert_eq!(compiled_in_fallback(0xab, 0xcd, 0xef), "\x1b]4;264;rgb:ab/cd/ef\x1b\\");
    }
}
