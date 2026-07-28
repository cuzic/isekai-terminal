//! Black-box process-level coverage for the `isekai-pipe ctl` subcommands
//! that talk to the per-tab ctl-socket (`setvar`/`getvar`/`tab-color`/
//! `notify`/`build` — everything in `src/ctl.rs` except `title`/`clip`,
//! which existing tests already touch transitively). `#[cfg(unix)]` because
//! `ctl.rs`'s actual transport is a `UnixStream`; the Windows build only
//! parses arguments and then reports the whole `ctl` subcommand family
//! unsupported (`ctl.rs`'s `#[cfg(not(unix))] ctl_command`), so a Windows
//! variant of this file would test nothing real.
//!
//! `ctl.rs` already has in-process unit tests (`#[cfg(test)] mod tests`)
//! that call `run_ctl(...)` as a library function against a listener bound
//! in the same test process — those pin down the wire protocol precisely.
//! This file adds the layer those can't reach: spawning the *real compiled*
//! `isekai-pipe` binary as a subprocess, so argv parsing, the secret-preamble
//! handshake, and the process's own exit code are all exercised the way a
//! remote shell invocation (`ssh host isekai-pipe ctl setvar ...`) actually
//! would.
//!
//! `--sock <path>` lets each invocation point at our own stub listener
//! directly, without needing `$ISEKAI_CTL_SOCK` or a real tmux pane option.
//!
//! The stub listener owns its state as a single sequential accept loop
//! (`run_stub_listener`), not one spawned task per connection — this test
//! drives client subprocesses one at a time and awaits each before starting
//! the next, so a sequential loop is both simpler and race-free for the
//! setvar→getvar roundtrip (no `Arc<Mutex<_>>` needed to sequence state
//! updates against concurrent connections that can't happen here).

#![cfg(unix)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio as StdStdio;

use base64::Engine as _;
use isekai_protocol::{decode_ctl_message, BuildOutputStream, CtlMessage, NotifyKind, VarScope};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::process::Command as TokioCommand;

const EX_USAGE: i32 = 64;
const EX_UNAVAILABLE: i32 = 69;

fn isekai_pipe_bin_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_isekai-pipe"))
}

async fn run_ctl(sock_path: &Path, args: &[&str]) -> (Option<i32>, String, String) {
    let mut full_args = vec!["ctl"];
    full_args.extend_from_slice(args);
    full_args.push("--sock");
    let sock_str = sock_path.to_string_lossy().into_owned();
    full_args.push(&sock_str);
    let output = TokioCommand::new(isekai_pipe_bin_path())
        .args(&full_args)
        .stdin(StdStdio::null())
        .stdout(StdStdio::piped())
        .stderr(StdStdio::piped())
        .output()
        .await
        .expect("failed to spawn isekai-pipe");
    (
        output.status.code(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// Same shape as [`run_ctl`] but without appending `--sock` — for usage-error
/// cases that must fail argv parsing before ever touching a socket.
async fn run_ctl_no_sock(args: &[&str]) -> (Option<i32>, String, String) {
    let mut full_args = vec!["ctl"];
    full_args.extend_from_slice(args);
    let output = TokioCommand::new(isekai_pipe_bin_path())
        .args(&full_args)
        .stdin(StdStdio::null())
        .stdout(StdStdio::piped())
        .stderr(StdStdio::piped())
        .output()
        .await
        .expect("failed to spawn isekai-pipe");
    (
        output.status.code(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// Accepts exactly `connections` ctl-socket connections in sequence,
/// verifying the secret preamble on each, replying to request/response
/// variants (`GetVarRequest`, `BuildRequest`), and recording every decoded
/// `CtlMessage` it saw (in arrival order) as the return value. A `SetVar`
/// received along the way is remembered in a local map so a later
/// `GetVarRequest` in the same run can be answered from it — this stub
/// intentionally does NOT reach for `ctl_forward.rs`'s real `CTL_VARS`
/// (unreachable from a test, see this crate's `ctl_forward.rs` module doc),
/// it's the test's own stand-in store.
async fn run_stub_listener(listener: UnixListener, sock_path: PathBuf, connections: usize) -> Vec<CtlMessage> {
    let mut received = Vec::new();
    let mut vars: HashMap<String, String> = HashMap::new();
    for _ in 0..connections {
        let (stream, _addr) = listener.accept().await.expect("accept a ctl connection");
        let msg = handle_one_connection(stream, &sock_path, &mut vars).await;
        received.push(msg);
    }
    received
}

async fn handle_one_connection(stream: UnixStream, sock_path: &Path, vars: &mut HashMap<String, String>) -> CtlMessage {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    let mut preamble = String::new();
    reader.read_line(&mut preamble).await.expect("read the secret preamble");
    assert_eq!(preamble.trim_end_matches('\n'), sock_path.to_string_lossy(), "secret preamble must be the ctl-socket path itself");

    let mut line = String::new();
    reader.read_line(&mut line).await.expect("read the ctl message");
    let msg = decode_ctl_message(line.trim_end_matches('\n').as_bytes()).expect("a well-formed CtlMessage");

    match &msg {
        CtlMessage::SetVar { scope, key, value } => {
            vars.insert(format!("{scope:?}:{key}"), value.clone());
        }
        CtlMessage::GetVarRequest { scope, key } => {
            let value = vars.get(&format!("{scope:?}:{key}")).cloned();
            write_response(&mut write_half, &CtlMessage::GetVarResponse { value }).await;
        }
        CtlMessage::BuildRequest { .. } => {
            for (stream, text) in [(BuildOutputStream::Stdout, "compiling...\n"), (BuildOutputStream::Stderr, "a warning\n")] {
                let chunk = CtlMessage::BuildOutputChunk {
                    stream,
                    data_b64: base64::engine::general_purpose::STANDARD.encode(text),
                };
                write_response(&mut write_half, &chunk).await;
            }
            let finished = CtlMessage::BuildFinished { exit_code: 7, result_paths: vec![] };
            write_response(&mut write_half, &finished).await;
        }
        _ => {}
    }
    msg
}

async fn write_response(write_half: &mut tokio::net::unix::OwnedWriteHalf, msg: &CtlMessage) {
    let mut out = serde_json::to_vec(msg).unwrap();
    out.push(b'\n');
    write_half.write_all(&out).await.expect("write a ctl response");
}

fn bind_stub_socket() -> (tempfile::TempDir, PathBuf, UnixListener) {
    let dir = tempfile::tempdir().unwrap();
    let sock_path = dir.path().join("ctl.sock");
    let listener = UnixListener::bind(&sock_path).unwrap();
    (dir, sock_path, listener)
}

#[tokio::test]
async fn setvar_then_getvar_roundtrips_through_the_ctl_socket() {
    let (_dir, sock_path, listener) = bind_stub_socket();
    let server = tokio::spawn(run_stub_listener(listener, sock_path.clone(), 2));

    let (code, _stdout, stderr) = run_ctl(&sock_path, &["setvar", "--scope", "session", "mykey", "myvalue"]).await;
    assert_eq!(code, Some(0), "stderr: {stderr}");

    let (code, stdout, stderr) = run_ctl(&sock_path, &["getvar", "--scope", "session", "mykey"]).await;
    assert_eq!(code, Some(0), "stderr: {stderr}");
    assert_eq!(stdout, "myvalue", "getvar must print the value with no trailing newline");

    let received = server.await.unwrap();
    assert_eq!(received[0], CtlMessage::SetVar { scope: VarScope::Session, key: "mykey".to_string(), value: "myvalue".to_string() });
    assert_eq!(received[1], CtlMessage::GetVarRequest { scope: VarScope::Session, key: "mykey".to_string() });
}

#[tokio::test]
async fn getvar_scopes_are_isolated_from_each_other() {
    let (_dir, sock_path, listener) = bind_stub_socket();
    let server = tokio::spawn(run_stub_listener(listener, sock_path.clone(), 3));

    let (code, ..) = run_ctl(&sock_path, &["setvar", "--scope", "tab", "k", "tab-value"]).await;
    assert_eq!(code, Some(0));
    let (code, ..) = run_ctl(&sock_path, &["setvar", "--scope", "global", "k", "global-value"]).await;
    assert_eq!(code, Some(0));

    let (code, stdout, stderr) = run_ctl(&sock_path, &["getvar", "--scope", "global", "k"]).await;
    assert_eq!(code, Some(0), "stderr: {stderr}");
    assert_eq!(stdout, "global-value");

    server.await.unwrap();
}

#[tokio::test]
async fn getvar_on_an_unset_key_exits_nonzero_with_no_stdout() {
    let (_dir, sock_path, listener) = bind_stub_socket();
    let server = tokio::spawn(run_stub_listener(listener, sock_path.clone(), 1));

    let (code, stdout, stderr) = run_ctl(&sock_path, &["getvar", "--scope", "session", "never-set"]).await;
    assert_eq!(code, Some(EX_UNAVAILABLE));
    assert!(stdout.is_empty(), "{stdout:?}");
    assert!(!stderr.is_empty());

    server.await.unwrap();
}

#[tokio::test]
async fn tab_color_sends_the_set_tab_color_frame() {
    let (_dir, sock_path, listener) = bind_stub_socket();
    let server = tokio::spawn(run_stub_listener(listener, sock_path.clone(), 1));

    let (code, _stdout, stderr) = run_ctl(&sock_path, &["tab-color", "#ff00aa"]).await;
    assert_eq!(code, Some(0), "stderr: {stderr}");

    let received = server.await.unwrap();
    assert_eq!(received[0], CtlMessage::SetTabColor { r: 0xff, g: 0x00, b: 0xaa });
}

#[tokio::test]
async fn notify_tmux_kind_carries_tag_and_seq() {
    let (_dir, sock_path, listener) = bind_stub_socket();
    let server = tokio::spawn(run_stub_listener(listener, sock_path.clone(), 1));

    let (code, _stdout, stderr) = run_ctl(&sock_path, &["notify", "--kind", "bell", "--tag", "win1.pane2", "--seq", "5"]).await;
    assert_eq!(code, Some(0), "stderr: {stderr}");

    let received = server.await.unwrap();
    assert_eq!(
        received[0],
        CtlMessage::Notify { kind: NotifyKind::Bell, tmux_tag: "win1.pane2".to_string(), seq: 5, title: String::new(), body: String::new() }
    );
}

#[tokio::test]
async fn notify_tmux_kind_without_tag_or_seq_exits_usage() {
    let (code, stdout, stderr) = run_ctl_no_sock(&["notify", "--kind", "bell"]).await;
    assert_eq!(code, Some(EX_USAGE));
    assert!(stdout.is_empty());
    assert!(!stderr.is_empty());
}

#[tokio::test]
async fn notify_ai_kind_carries_title_and_body() {
    let (_dir, sock_path, listener) = bind_stub_socket();
    let server = tokio::spawn(run_stub_listener(listener, sock_path.clone(), 1));

    let (code, _stdout, stderr) = run_ctl(&sock_path, &["notify", "--kind", "waiting", "Permission needed", "Claude wants to run rm"]).await;
    assert_eq!(code, Some(0), "stderr: {stderr}");

    let received = server.await.unwrap();
    assert_eq!(
        received[0],
        CtlMessage::Notify {
            kind: NotifyKind::Waiting,
            tmux_tag: String::new(),
            seq: 0,
            title: "Permission needed".to_string(),
            body: "Claude wants to run rm".to_string(),
        }
    );
}

#[tokio::test]
async fn notify_ai_kind_without_title_or_body_exits_usage() {
    let (code, stdout, stderr) = run_ctl_no_sock(&["notify", "--kind", "waiting"]).await;
    assert_eq!(code, Some(EX_USAGE));
    assert!(stdout.is_empty());
    assert!(!stderr.is_empty());
}

#[tokio::test]
async fn build_streams_output_chunks_and_propagates_the_exit_code() {
    let (_dir, sock_path, listener) = bind_stub_socket();
    let server = tokio::spawn(run_stub_listener(listener, sock_path.clone(), 1));

    let (code, stdout, stderr) = run_ctl(&sock_path, &["build", "my-profile"]).await;
    assert_eq!(code, Some(7), "the process's own exit code must be the build's exit code; stderr: {stderr}");
    assert!(stdout.contains("compiling..."), "stdout: {stdout:?}");
    assert!(stderr.contains("a warning"), "stderr: {stderr:?}");

    let received = server.await.unwrap();
    assert_eq!(received[0], CtlMessage::BuildRequest { profile: "my-profile".to_string() });
}

#[tokio::test]
async fn build_without_a_profile_argument_exits_usage() {
    let (code, stdout, stderr) = run_ctl_no_sock(&["build"]).await;
    assert_eq!(code, Some(EX_USAGE));
    assert!(stdout.is_empty());
    assert!(!stderr.is_empty());
}
