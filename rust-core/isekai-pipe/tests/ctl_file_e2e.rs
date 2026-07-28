//! Black-box process-level coverage for `isekai-pipe ctl file ls|cat|info|cp|rm`
//! (`src/ctl_file.rs`, isekai-terminal tracker task #16/#17).
//!
//! `ctl_file.rs` already has thorough unit tests for its `std::fs`-backed
//! helpers (`list_dir`/`read_chunk`/`file_info`/`copy_file`/`remove_path`)
//! and its argv parser, but nothing drives the *real compiled binary* as a
//! subprocess the way a caller (`ssh host isekai-pipe ctl file cat ...`)
//! actually would — this file closes that gap: real argv handling, real
//! stdout/stderr streams, real process exit codes.
//!
//! Unlike every other `ctl` subcommand, `file` never touches the ctl-socket
//! (`$ISEKAI_CTL_SOCK`) at all — it's a plain one-shot CLI operating on
//! whatever host it runs on (`ctl_file.rs` module docs) — so this needs no
//! mock sshd, no ctl-socket listener, just a tempdir and the real binary.

use std::path::PathBuf;
use std::process::Stdio as StdStdio;

use base64::Engine as _;
use serde_json::Value;
use tokio::process::Command as TokioCommand;

const EX_USAGE: i32 = 64;
const EX_IOERR: i32 = 74;

fn isekai_pipe_bin_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_isekai-pipe"))
}

async fn run_ctl_file(args: &[&str]) -> (Option<i32>, String, String) {
    let mut full_args = vec!["ctl", "file"];
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

fn parse_json_line(stdout: &str) -> Value {
    serde_json::from_str(stdout.trim_end()).unwrap_or_else(|e| panic!("stdout was not exactly one JSON document ({e}): {stdout:?}"))
}

#[tokio::test]
async fn ls_lists_directory_entries_sorted_by_name() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("b.txt"), b"b").unwrap();
    std::fs::write(dir.path().join("a.txt"), b"a").unwrap();

    let (code, stdout, stderr) = run_ctl_file(&["ls", dir.path().to_str().unwrap()]).await;
    assert_eq!(code, Some(0), "stderr: {stderr}");
    let json = parse_json_line(&stdout);
    let names: Vec<&str> = json["entries"].as_array().unwrap().iter().map(|e| e["name"].as_str().unwrap()).collect();
    assert_eq!(names, vec!["a.txt", "b.txt"]);
}

#[tokio::test]
async fn cat_roundtrips_file_contents_as_base64() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("f.txt");
    std::fs::write(&path, b"hello world").unwrap();

    let (code, stdout, stderr) = run_ctl_file(&["cat", path.to_str().unwrap()]).await;
    assert_eq!(code, Some(0), "stderr: {stderr}");
    let json = parse_json_line(&stdout);
    assert!(json["eof"].as_bool().unwrap());
    let decoded = base64::engine::general_purpose::STANDARD.decode(json["data_b64"].as_str().unwrap()).unwrap();
    assert_eq!(decoded, b"hello world");
}

#[tokio::test]
async fn info_reports_size_and_kind() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("f.txt");
    std::fs::write(&path, b"12345").unwrap();

    let (code, stdout, stderr) = run_ctl_file(&["info", path.to_str().unwrap()]).await;
    assert_eq!(code, Some(0), "stderr: {stderr}");
    let json = parse_json_line(&stdout);
    assert_eq!(json["size"].as_u64().unwrap(), 5);
    assert_eq!(json["is_dir"].as_bool().unwrap(), false);
}

#[tokio::test]
async fn cp_copies_the_file_and_leaves_the_source_intact() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src.txt");
    let dst = dir.path().join("dst.txt");
    std::fs::write(&src, b"copy me").unwrap();

    let (code, stdout, stderr) = run_ctl_file(&["cp", src.to_str().unwrap(), dst.to_str().unwrap()]).await;
    assert_eq!(code, Some(0), "stderr: {stderr}");
    let json = parse_json_line(&stdout);
    assert_eq!(json["ok"].as_bool().unwrap(), true);
    assert_eq!(std::fs::read(&dst).unwrap(), b"copy me");
    assert_eq!(std::fs::read(&src).unwrap(), b"copy me");
}

#[tokio::test]
async fn rm_deletes_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("f.txt");
    std::fs::write(&path, b"x").unwrap();

    let (code, stdout, stderr) = run_ctl_file(&["rm", path.to_str().unwrap()]).await;
    assert_eq!(code, Some(0), "stderr: {stderr}");
    let json = parse_json_line(&stdout);
    assert_eq!(json["ok"].as_bool().unwrap(), true);
    assert!(!path.exists());
}

#[tokio::test]
async fn rm_recursive_deletes_a_nonempty_directory() {
    let dir = tempfile::tempdir().unwrap();
    let sub = dir.path().join("sub");
    std::fs::create_dir(&sub).unwrap();
    std::fs::write(sub.join("f.txt"), b"x").unwrap();

    let (code, _stdout, stderr) = run_ctl_file(&["rm", "--recursive", sub.to_str().unwrap()]).await;
    assert_eq!(code, Some(0), "stderr: {stderr}");
    assert!(!sub.exists());
}

#[tokio::test]
async fn cat_on_a_missing_path_prints_error_json_and_exits_ex_ioerr() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("does-not-exist");

    let (code, stdout, _stderr) = run_ctl_file(&["cat", missing.to_str().unwrap()]).await;
    assert_eq!(code, Some(EX_IOERR));
    let json = parse_json_line(&stdout);
    assert_eq!(json["ok"].as_bool().unwrap(), false);
    assert!(json["error"].as_str().unwrap().len() > 0);
}

#[tokio::test]
async fn ls_without_a_path_argument_exits_ex_usage_with_no_stdout() {
    let (code, stdout, stderr) = run_ctl_file(&["ls"]).await;
    assert_eq!(code, Some(EX_USAGE));
    assert!(stdout.is_empty(), "usage errors must not print a JSON document to stdout: {stdout:?}");
    assert!(!stderr.is_empty());
}

#[tokio::test]
async fn cp_without_a_destination_argument_exits_ex_usage() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src.txt");
    std::fs::write(&src, b"x").unwrap();

    let (code, stdout, stderr) = run_ctl_file(&["cp", src.to_str().unwrap()]).await;
    assert_eq!(code, Some(EX_USAGE));
    assert!(stdout.is_empty(), "{stdout:?}");
    assert!(!stderr.is_empty());
}

#[tokio::test]
async fn cat_honors_offset_and_length_flags() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("f.txt");
    std::fs::write(&path, b"0123456789").unwrap();

    let (code, stdout, stderr) = run_ctl_file(&["cat", path.to_str().unwrap(), "--offset", "2", "--length", "3"]).await;
    assert_eq!(code, Some(0), "stderr: {stderr}");
    let json = parse_json_line(&stdout);
    assert_eq!(json["offset"].as_u64().unwrap(), 2);
    assert_eq!(json["eof"].as_bool().unwrap(), false);
    let decoded = base64::engine::general_purpose::STANDARD.decode(json["data_b64"].as_str().unwrap()).unwrap();
    assert_eq!(decoded, b"234");
}
