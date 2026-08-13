//! Regression coverage for a real gap found via Opus pre-mortem review
//! (2026-08-12, `attach.rs::run`'s own doc comment has the full mechanism):
//! `isekai-ssh`'s `--isekai-tty`/`#@isekai tty` remote command is
//! `exec isekai-pipe tty attach '<name>'`, which *replaces* the login shell
//! entirely. Before this fix, any failure before a live session was ever
//! established (daemon spawn failure, a broken `$HOME`, no `HelloAck`, ...)
//! just propagated an error straight out — since the remote command was an
//! `exec`, that error *was* the whole SSH session ending, with no shell
//! ever having run at all. `--isekai-tty` is meant to be an opt-in
//! convenience layered on top of an otherwise-working SSH session; it must
//! never be the reason a user gets no shell.
//!
//! Fixed by falling back to a plain interactive login shell (via
//! `std::os::unix::process::CommandExt::exec`, replacing the process image
//! in place, no intervening shell layer) whenever the connect+handshake
//! phase fails, before ever reaching a live relay.

#![cfg(unix)]

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

fn isekai_pipe_bin_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_isekai-pipe"))
}

fn short_tmp_home() -> tempfile::TempDir {
    tempfile::Builder::new().prefix("e").tempdir_in("/tmp").expect("failed to create a short-path temp dir under /tmp")
}

/// Forces `unix_socket.rs::private_runtime_dir`'s very first filesystem
/// operation to fail deterministically: that function does
/// `home.join(".cache").join("isekai-pipe").join("tty")` and
/// `DirBuilder::new().recursive(true).create(&dir)` — if `.cache/isekai-pipe`
/// already exists as a *plain file* (not a directory), creating `tty`
/// underneath it can never succeed (`ENOTDIR`), regardless of anything
/// else about the environment. This is deliberately a filesystem-level
/// failure, not a contrived error injection hook, so this test exercises
/// the exact same code path a real broken `$HOME` would.
fn make_private_runtime_dir_impossible(home: &std::path::Path) {
    let cache_dir = home.join(".cache");
    std::fs::create_dir_all(&cache_dir).expect("failed to create .cache");
    std::fs::write(cache_dir.join("isekai-pipe"), b"not a directory").expect("failed to create the blocking file");
}

#[tokio::test]
async fn tty_attach_falls_back_to_a_working_login_shell_when_it_cannot_even_start() {
    let home = short_tmp_home();
    make_private_runtime_dir_impossible(home.path());

    let mut attach = tokio::process::Command::new(isekai_pipe_bin_path())
        .args(["tty", "attach", "e2e-fallback-shell"])
        .env("HOME", home.path())
        .env("SHELL", "/bin/sh")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn `isekai-pipe tty attach`");

    let mut stdin = attach.stdin.take().expect("stdin must be piped");
    let mut stdout = attach.stdout.take().expect("stdout must be piped");

    // If the fallback works, this lands in a real, live `/bin/sh -i -l` —
    // proving not just that *some* process is running, but that it's a
    // genuinely functional interactive shell able to read a command from
    // stdin and write its output back.
    stdin.write_all(b"echo FALLBACK_SHELL_IS_ALIVE_$((1 + 1))\n").await.expect("failed to write the probe command");

    let mut collected = Vec::new();
    let mut buf = [0u8; 4096];
    let saw_marker = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let n = stdout.read(&mut buf).await.expect("read from tty attach's stdout failed");
            if n == 0 {
                return false;
            }
            collected.extend_from_slice(&buf[..n]);
            if String::from_utf8_lossy(&collected).contains("FALLBACK_SHELL_IS_ALIVE_2") {
                return true;
            }
        }
    })
    .await
    .unwrap_or(false);
    assert!(
        saw_marker,
        "isekai-pipe tty attach must fall back to a genuinely working login shell when it cannot even start \
         (here: private_runtime_dir() forced to fail) — regression: this used to just end the whole SSH session \
         with no shell at all; got so far: {:?}",
        String::from_utf8_lossy(&collected)
    );

    let _ = stdin.write_all(b"exit 0\n").await;
    let exited = tokio::time::timeout(Duration::from_secs(5), attach.wait()).await;
    assert!(exited.is_ok(), "the fallback shell must exit cleanly on `exit 0` like any ordinary login shell");
}
