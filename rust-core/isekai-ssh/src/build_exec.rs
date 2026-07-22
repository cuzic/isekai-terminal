//! Shared shell-command spawn + result-glob matching for Epic P build
//! profiles (`ISEKAI_PIPE_DESIGN.md` §8 Epic P), used both by
//! `isekai-ssh build-profile test` (`build_profile_cli.rs`, a local dry run)
//! and `ctl_forward.rs`'s real remote-triggered dispatch — kept in one place
//! so the two never drift on how a profile's `command`/`result_glob` is
//! actually interpreted.

use std::path::PathBuf;

use anyhow::{Context, Result};
use tokio::process::Command;

/// Builds (but does not spawn) the platform shell invocation for `command`
/// in `dir`, so a profile's `command` field can contain `&&`/pipes/etc.
/// rather than being limited to a single argv. Caller decides stdio wiring
/// (`build_profile_cli::test` inherits this process's own stdio;
/// `ctl_forward.rs` pipes stdout/stderr to stream them over the ctl-socket).
pub fn spawn_shell_command(command: &str, dir: &str) -> Command {
    let mut cmd = if cfg!(windows) {
        Command::new("cmd")
    } else {
        Command::new("sh")
    };
    if cfg!(windows) {
        cmd.arg("/C");
    } else {
        cmd.arg("-c");
    }
    cmd.arg(command);
    cmd.current_dir(dir);
    cmd
}

/// Matches `glob_pattern` (relative to `dir`) and returns the matched paths,
/// capped at `isekai_protocol::MAX_BUILD_RESULT_PATHS` entries — the same
/// cap `CtlMessage::BuildFinished`'s wire format enforces, so a build with
/// an overly broad glob is truncated here rather than being rejected later
/// by `validate_ctl_message` after the fact.
pub fn glob_results(dir: &str, glob_pattern: &str) -> Result<Vec<PathBuf>> {
    let pattern = std::path::Path::new(dir).join(glob_pattern);
    let pattern_str = pattern.to_string_lossy();
    let mut matches = Vec::new();
    for entry in
        glob::glob(&pattern_str).with_context(|| format!("isekai-ssh: invalid result glob {pattern_str:?}"))?
    {
        let path = entry.with_context(|| format!("isekai-ssh: failed to read a glob match under {dir:?}"))?;
        matches.push(path);
        if matches.len() >= isekai_protocol::MAX_BUILD_RESULT_PATHS {
            break;
        }
    }
    Ok(matches)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn spawn_shell_command_runs_in_the_given_directory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("marker.txt"), "hi").unwrap();
        let command = if cfg!(windows) { "dir marker.txt" } else { "ls marker.txt" };
        let status = spawn_shell_command(command, &dir.path().to_string_lossy())
            .status()
            .await
            .unwrap();
        assert!(status.success());
    }

    #[test]
    fn glob_results_matches_relative_to_dir() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("app.exe"), b"binary").unwrap();
        std::fs::write(dir.path().join("app.pdb"), b"debug").unwrap();

        let matches = glob_results(&dir.path().to_string_lossy(), "*.exe").unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].file_name().unwrap(), "app.exe");
    }

    #[test]
    fn glob_results_returns_empty_for_no_matches() {
        let dir = tempfile::tempdir().unwrap();
        let matches = glob_results(&dir.path().to_string_lossy(), "*.exe").unwrap();
        assert!(matches.is_empty());
    }
}
