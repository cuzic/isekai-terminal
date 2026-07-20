//! `isekai-pipe ctl file ls|cat|info|cp|rm` (isekai-terminal tracker task #16,
//! "isekai-pipe ctl に setvar/getvar・file系スクリプタブルコマンドを追加").
//!
//! Unlike every other `ctl` subcommand (`title`/`clip`/`setvar`/`getvar`),
//! `file` never touches the per-tab ctl-socket-forward channel
//! (`isekai_protocol::CtlMessage`/`$ISEKAI_CTL_SOCK`) at all. It operates
//! directly, via `std::fs`, on the filesystem of whatever host `isekai-pipe
//! ctl` itself runs on — which, per this project's architecture, is always
//! the remote SSH host (`isekai-pipe` is the data-plane binary embedded on
//! and deployed to the remote host; see `CLAUDE.md`'s directory overview and
//! `ISEKAI_PIPE_DESIGN.md`). Concretely: a caller invokes
//! `isekai-pipe ctl file cat /var/log/app.log` as a plain one-shot remote SSH
//! command (`ssh host isekai-pipe ctl file cat ...`, or the isekai-terminal
//! Android app's own SSH channel running the same command) and reads
//! structured JSON off stdout — no `#@isekai ctl-socket yes` opt-in, no
//! `$ISEKAI_CTL_SOCK`, no dependency on the tab having a live ctl-socket
//! forward at all. This is deliberately the simplest thing that satisfies
//! task #16's primary motivating use case (task #17, "ファイルプレビューUI",
//! reusing this as its backend to preview a file that lives on the remote
//! host without a full trzsz download).
//!
//! **Scope note**: this only covers what the task calls "リモートパス"
//! (paths on the host `isekai-pipe ctl` runs on). Operating on the *device*
//! side's filesystem ("ローカルパス" — the `isekai-ssh` CLI wrapper's own
//! machine, or a future Android-side store) would need to go back over the
//! ctl-socket-forward channel like `setvar`/`getvar` do, similar to how
//! `ClipboardPullRequest` needs a device-side listener to fulfill it. That
//! raises its own design questions (Android's scoped storage has no general
//! "arbitrary absolute path" concept) distinct enough from this primitive
//! that it's deliberately left out of this first cut rather than bolted on;
//! see the PR description for task #16 for the full rationale.
//!
//! JSON output: `ls`/`cat`/`info`/`cp`/`rm` all print exactly one JSON
//! document to stdout, one call each. Errors (I/O failures — not found,
//! permission denied, etc.) print `{"ok":false,"error":"..."}` to stdout
//! *and* exit `EX_IOERR`, so a caller can use the exit code alone or parse
//! the JSON either way. Usage errors (bad flags) print to stderr and exit
//! `EX_USAGE`, matching every other `ctl` subcommand.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use base64::Engine as _;
use serde::Serialize;

use crate::connect::next_arg;
use crate::{EX_IOERR, EX_USAGE};

/// Cap on the number of bytes a single `file cat` call reads, regardless of
/// `--length`. A caller wanting more pages through further calls with
/// `--offset` advanced by the returned `length` (`eof: false` signals more
/// remains) — this is the "チャンク分割/オフセット指定の読み取り" task #16
/// explicitly asks for, so a single call can never accidentally try to
/// buffer an entire multi-gigabyte file into memory and stdout.
pub(crate) const MAX_FILE_CAT_CHUNK_LEN: u64 = 8 * 1024 * 1024;

#[derive(Debug, Serialize)]
struct FileLsEntry {
    name: String,
    is_dir: bool,
    is_symlink: bool,
    size: u64,
    modified_unix: Option<i64>,
}

#[derive(Debug, Serialize)]
struct FileLsResult {
    entries: Vec<FileLsEntry>,
}

#[derive(Debug, Serialize)]
struct FileCatResult {
    offset: u64,
    length: u64,
    total_size: u64,
    eof: bool,
    data_b64: String,
}

#[derive(Debug, Serialize)]
struct FileInfoResult {
    name: String,
    path: String,
    is_dir: bool,
    is_symlink: bool,
    size: u64,
    modified_unix: Option<i64>,
    /// Unix permission bits (e.g. `0o644`), `None` on platforms without the
    /// concept. Always `Some` in this project's actual deployment target
    /// (the remote SSH host is always Linux, `ISEKAI_PIPE_DESIGN.md`), kept
    /// `Option` rather than unconditionally compiled `#[cfg(unix)]` so this
    /// module — and the rest of `isekai-pipe`'s cross-platform build — isn't
    /// forced unix-only just for this one field.
    permissions_unix: Option<u32>,
}

#[derive(Debug, Serialize)]
struct FileOpResult {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

impl FileOpResult {
    fn ok() -> Self {
        Self { ok: true, error: None }
    }
    fn err(e: impl std::fmt::Display) -> Self {
        Self { ok: false, error: Some(e.to_string()) }
    }
}

fn print_json<T: Serialize>(value: &T) -> Result<(), String> {
    let json = serde_json::to_string(value).map_err(|e| format!("failed to encode JSON: {e}"))?;
    println!("{json}");
    Ok(())
}

fn print_file_help() {
    println!("USAGE:");
    println!("    isekai-pipe ctl file ls <path>");
    println!("    isekai-pipe ctl file cat <path> [--offset <bytes>] [--length <bytes>]");
    println!("        (reads at most {MAX_FILE_CAT_CHUNK_LEN} bytes per call; page through a");
    println!("        larger file with further calls at offset += the returned \"length\")");
    println!("    isekai-pipe ctl file info <path>");
    println!("    isekai-pipe ctl file cp <src> <dst>");
    println!("    isekai-pipe ctl file rm <path> [--recursive]");
    println!();
    println!("Operates on the filesystem of the host `isekai-pipe ctl` itself runs on (always");
    println!("the remote SSH host in this project's architecture) — no ctl-socket forward, no");
    println!("$ISEKAI_CTL_SOCK. Each call prints exactly one JSON document to stdout.");
}

fn modified_unix(meta: &std::fs::Metadata) -> Option<i64> {
    meta.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs() as i64)
}

fn permissions_unix(meta: &std::fs::Metadata) -> Option<u32> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        Some(meta.permissions().mode() & 0o7777)
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        None
    }
}

fn list_dir(path: &Path) -> Result<FileLsResult, std::io::Error> {
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        let meta = entry.metadata()?;
        entries.push(FileLsEntry {
            name: entry.file_name().to_string_lossy().into_owned(),
            is_dir: meta.is_dir(),
            is_symlink: meta.is_symlink(),
            size: meta.len(),
            modified_unix: modified_unix(&meta),
        });
    }
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(FileLsResult { entries })
}

fn read_chunk(path: &Path, offset: u64, requested_length: Option<u64>) -> Result<FileCatResult, std::io::Error> {
    use std::io::{Read as _, Seek as _, SeekFrom};

    let mut file = std::fs::File::open(path)?;
    let total_size = file.metadata()?.len();
    let length = requested_length.unwrap_or(total_size.saturating_sub(offset)).min(MAX_FILE_CAT_CHUNK_LEN);

    let mut data = Vec::new();
    if offset < total_size && length > 0 {
        file.seek(SeekFrom::Start(offset))?;
        data = vec![0u8; length as usize];
        let read = file.read(&mut data)?;
        data.truncate(read);
    }
    let eof = offset.saturating_add(data.len() as u64) >= total_size;
    let data_b64 = base64::engine::general_purpose::STANDARD.encode(&data);
    Ok(FileCatResult { offset, length: data.len() as u64, total_size, eof, data_b64 })
}

fn file_info(path: &Path) -> Result<FileInfoResult, std::io::Error> {
    let meta = std::fs::symlink_metadata(path)?;
    Ok(FileInfoResult {
        name: path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.to_string_lossy().into_owned()),
        path: path.to_string_lossy().into_owned(),
        is_dir: meta.is_dir(),
        is_symlink: meta.is_symlink(),
        size: meta.len(),
        modified_unix: modified_unix(&meta),
        permissions_unix: permissions_unix(&meta),
    })
}

fn copy_file(src: &Path, dst: &Path) -> Result<(), std::io::Error> {
    std::fs::copy(src, dst).map(|_| ())
}

fn remove_path(path: &Path, recursive: bool) -> Result<(), std::io::Error> {
    let meta = std::fs::symlink_metadata(path)?;
    if meta.is_dir() {
        if recursive {
            std::fs::remove_dir_all(path)
        } else {
            std::fs::remove_dir(path)
        }
    } else {
        std::fs::remove_file(path)
    }
}

#[derive(Debug, PartialEq)]
enum FileLaunch {
    Ls { path: String },
    Cat { path: String, offset: u64, length: Option<u64> },
    Info { path: String },
    Cp { src: String, dst: String },
    Rm { path: String, recursive: bool },
}

fn parse_u64(command: &str, flag: &str, raw: &str) -> Result<u64, ExitCode> {
    raw.parse::<u64>().map_err(|_| {
        eprintln!("isekai-pipe ctl file {command}: {flag} must be a non-negative integer, got {raw:?}");
        ExitCode::from(EX_USAGE)
    })
}

fn parse_file(mut args: impl Iterator<Item = String>) -> Result<Option<FileLaunch>, ExitCode> {
    match args.next().as_deref() {
        None | Some("-h") | Some("--help") => {
            print_file_help();
            Ok(None)
        }
        Some("ls") => {
            let path = args.next().ok_or_else(|| {
                eprintln!("isekai-pipe ctl file ls: a path argument is required");
                ExitCode::from(EX_USAGE)
            })?;
            Ok(Some(FileLaunch::Ls { path }))
        }
        Some("cat") => {
            let mut path: Option<String> = None;
            let mut offset: u64 = 0;
            let mut length: Option<u64> = None;
            while let Some(arg) = args.next() {
                match arg.as_str() {
                    "--offset" => {
                        let raw = next_arg("file cat", &mut args, "--offset").map_err(|e| {
                            eprintln!("{e}");
                            ExitCode::from(EX_USAGE)
                        })?;
                        offset = parse_u64("cat", "--offset", &raw)?;
                    }
                    "--length" => {
                        let raw = next_arg("file cat", &mut args, "--length").map_err(|e| {
                            eprintln!("{e}");
                            ExitCode::from(EX_USAGE)
                        })?;
                        length = Some(parse_u64("cat", "--length", &raw)?);
                    }
                    other if path.is_none() => path = Some(other.to_string()),
                    other => {
                        eprintln!("isekai-pipe ctl file cat: unexpected extra argument {other:?}");
                        return Err(ExitCode::from(EX_USAGE));
                    }
                }
            }
            let Some(path) = path else {
                eprintln!("isekai-pipe ctl file cat: a path argument is required");
                return Err(ExitCode::from(EX_USAGE));
            };
            Ok(Some(FileLaunch::Cat { path, offset, length }))
        }
        Some("info") => {
            let path = args.next().ok_or_else(|| {
                eprintln!("isekai-pipe ctl file info: a path argument is required");
                ExitCode::from(EX_USAGE)
            })?;
            Ok(Some(FileLaunch::Info { path }))
        }
        Some("cp") => {
            let src = args.next().ok_or_else(|| {
                eprintln!("isekai-pipe ctl file cp: a source path argument is required");
                ExitCode::from(EX_USAGE)
            })?;
            let dst = args.next().ok_or_else(|| {
                eprintln!("isekai-pipe ctl file cp: a destination path argument is required");
                ExitCode::from(EX_USAGE)
            })?;
            Ok(Some(FileLaunch::Cp { src, dst }))
        }
        Some("rm") => {
            let mut path: Option<String> = None;
            let mut recursive = false;
            while let Some(arg) = args.next() {
                match arg.as_str() {
                    "--recursive" | "-r" => recursive = true,
                    other if path.is_none() => path = Some(other.to_string()),
                    other => {
                        eprintln!("isekai-pipe ctl file rm: unexpected extra argument {other:?}");
                        return Err(ExitCode::from(EX_USAGE));
                    }
                }
            }
            let Some(path) = path else {
                eprintln!("isekai-pipe ctl file rm: a path argument is required");
                return Err(ExitCode::from(EX_USAGE));
            };
            Ok(Some(FileLaunch::Rm { path, recursive }))
        }
        Some(other) => {
            eprintln!("isekai-pipe ctl file: unknown subcommand {other:?}");
            Err(ExitCode::from(EX_USAGE))
        }
    }
}

pub(crate) async fn file_command(args: impl Iterator<Item = String>) -> ExitCode {
    let launch = match parse_file(args) {
        Ok(Some(launch)) => launch,
        Ok(None) => return ExitCode::SUCCESS,
        Err(code) => return code,
    };
    match launch {
        FileLaunch::Ls { path } => match list_dir(&PathBuf::from(path)) {
            Ok(result) => emit_ok(&result),
            Err(e) => emit_io_err(e),
        },
        FileLaunch::Cat { path, offset, length } => match read_chunk(&PathBuf::from(path), offset, length) {
            Ok(result) => emit_ok(&result),
            Err(e) => emit_io_err(e),
        },
        FileLaunch::Info { path } => match file_info(&PathBuf::from(path)) {
            Ok(result) => emit_ok(&result),
            Err(e) => emit_io_err(e),
        },
        FileLaunch::Cp { src, dst } => match copy_file(&PathBuf::from(src), &PathBuf::from(dst)) {
            Ok(()) => emit_ok(&FileOpResult::ok()),
            Err(e) => emit_io_err(e),
        },
        FileLaunch::Rm { path, recursive } => match remove_path(&PathBuf::from(path), recursive) {
            Ok(()) => emit_ok(&FileOpResult::ok()),
            Err(e) => emit_io_err(e),
        },
    }
}

fn emit_ok<T: Serialize>(value: &T) -> ExitCode {
    match print_json(value) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("isekai-pipe ctl file: {e}");
            ExitCode::from(EX_IOERR)
        }
    }
}

fn emit_io_err(e: std::io::Error) -> ExitCode {
    // Prints the JSON envelope even on failure (see module docs): a caller
    // parsing stdout as JSON unconditionally still gets a well-formed
    // document, not an empty stream, and the non-zero exit code alone is
    // enough for a caller that just checks `$?`.
    let _ = print_json(&FileOpResult::err(e));
    ExitCode::from(EX_IOERR)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(strs: &[&str]) -> impl Iterator<Item = String> {
        strs.iter().map(|s| s.to_string()).collect::<Vec<_>>().into_iter()
    }

    // ── parsing ──

    #[test]
    fn parses_ls() {
        assert_eq!(parse_file(args(&["ls", "/tmp"])).unwrap().unwrap(), FileLaunch::Ls { path: "/tmp".to_string() });
    }

    #[test]
    fn rejects_ls_without_path() {
        let err = parse_file(args(&["ls"])).unwrap_err();
        assert_eq!(err, ExitCode::from(EX_USAGE));
    }

    #[test]
    fn parses_cat_with_defaults() {
        assert_eq!(
            parse_file(args(&["cat", "/tmp/a.txt"])).unwrap().unwrap(),
            FileLaunch::Cat { path: "/tmp/a.txt".to_string(), offset: 0, length: None }
        );
    }

    #[test]
    fn parses_cat_with_offset_and_length() {
        assert_eq!(
            parse_file(args(&["cat", "/tmp/a.txt", "--offset", "10", "--length", "20"])).unwrap().unwrap(),
            FileLaunch::Cat { path: "/tmp/a.txt".to_string(), offset: 10, length: Some(20) }
        );
    }

    #[test]
    fn rejects_cat_with_non_numeric_offset() {
        let err = parse_file(args(&["cat", "/tmp/a.txt", "--offset", "abc"])).unwrap_err();
        assert_eq!(err, ExitCode::from(EX_USAGE));
    }

    #[test]
    fn parses_info() {
        assert_eq!(
            parse_file(args(&["info", "/tmp/a.txt"])).unwrap().unwrap(),
            FileLaunch::Info { path: "/tmp/a.txt".to_string() }
        );
    }

    #[test]
    fn parses_cp() {
        assert_eq!(
            parse_file(args(&["cp", "/tmp/a.txt", "/tmp/b.txt"])).unwrap().unwrap(),
            FileLaunch::Cp { src: "/tmp/a.txt".to_string(), dst: "/tmp/b.txt".to_string() }
        );
    }

    #[test]
    fn rejects_cp_without_destination() {
        let err = parse_file(args(&["cp", "/tmp/a.txt"])).unwrap_err();
        assert_eq!(err, ExitCode::from(EX_USAGE));
    }

    #[test]
    fn parses_rm() {
        assert_eq!(
            parse_file(args(&["rm", "/tmp/a.txt"])).unwrap().unwrap(),
            FileLaunch::Rm { path: "/tmp/a.txt".to_string(), recursive: false }
        );
    }

    #[test]
    fn parses_rm_recursive() {
        assert_eq!(
            parse_file(args(&["rm", "--recursive", "/tmp/dir"])).unwrap().unwrap(),
            FileLaunch::Rm { path: "/tmp/dir".to_string(), recursive: true }
        );
    }

    #[test]
    fn rejects_unknown_subcommand() {
        let err = parse_file(args(&["frobnicate"])).unwrap_err();
        assert_eq!(err, ExitCode::from(EX_USAGE));
    }

    // ── fs operations ──

    #[test]
    fn list_dir_lists_files_sorted_by_name() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("b.txt"), b"b").unwrap();
        std::fs::write(dir.path().join("a.txt"), b"a").unwrap();
        std::fs::create_dir(dir.path().join("c_dir")).unwrap();

        let result = list_dir(dir.path()).unwrap();
        let names: Vec<&str> = result.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["a.txt", "b.txt", "c_dir"]);
        assert!(!result.entries[0].is_dir);
        assert!(result.entries[2].is_dir);
    }

    #[test]
    fn list_dir_on_missing_path_errors() {
        let dir = tempfile::tempdir().unwrap();
        assert!(list_dir(&dir.path().join("does-not-exist")).is_err());
    }

    #[test]
    fn read_chunk_reads_whole_small_file_by_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        std::fs::write(&path, b"hello world").unwrap();

        let result = read_chunk(&path, 0, None).unwrap();
        assert_eq!(result.offset, 0);
        assert_eq!(result.length, 11);
        assert_eq!(result.total_size, 11);
        assert!(result.eof);
        let decoded = base64::engine::general_purpose::STANDARD.decode(&result.data_b64).unwrap();
        assert_eq!(decoded, b"hello world");
    }

    #[test]
    fn read_chunk_honors_offset_and_length_and_reports_eof_false_when_more_remains() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        std::fs::write(&path, b"0123456789").unwrap();

        let result = read_chunk(&path, 2, Some(3)).unwrap();
        assert_eq!(result.offset, 2);
        assert_eq!(result.length, 3);
        assert_eq!(result.total_size, 10);
        assert!(!result.eof);
        let decoded = base64::engine::general_purpose::STANDARD.decode(&result.data_b64).unwrap();
        assert_eq!(decoded, b"234");
    }

    #[test]
    fn read_chunk_at_the_tail_reports_eof_true() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        std::fs::write(&path, b"0123456789").unwrap();

        let result = read_chunk(&path, 8, Some(100)).unwrap();
        assert_eq!(result.offset, 8);
        assert_eq!(result.length, 2);
        assert!(result.eof);
        let decoded = base64::engine::general_purpose::STANDARD.decode(&result.data_b64).unwrap();
        assert_eq!(decoded, b"89");
    }

    #[test]
    fn read_chunk_past_eof_returns_empty_and_eof_true() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        std::fs::write(&path, b"short").unwrap();

        let result = read_chunk(&path, 1000, None).unwrap();
        assert_eq!(result.length, 0);
        assert!(result.eof);
    }

    #[test]
    fn read_chunk_clamps_requested_length_to_the_per_call_cap() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        let big = vec![b'x'; (MAX_FILE_CAT_CHUNK_LEN + 100) as usize];
        std::fs::write(&path, &big).unwrap();

        let result = read_chunk(&path, 0, None).unwrap();
        assert_eq!(result.length, MAX_FILE_CAT_CHUNK_LEN);
        assert_eq!(result.total_size, MAX_FILE_CAT_CHUNK_LEN + 100);
        assert!(!result.eof, "a clamped read must not falsely report eof");
    }

    #[test]
    fn file_info_reports_size_and_kind() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        std::fs::write(&path, b"12345").unwrap();

        let info = file_info(&path).unwrap();
        assert_eq!(info.name, "f.txt");
        assert_eq!(info.size, 5);
        assert!(!info.is_dir);
        assert!(!info.is_symlink);
    }

    #[test]
    fn file_info_on_a_directory_reports_is_dir() {
        let dir = tempfile::tempdir().unwrap();
        let info = file_info(dir.path()).unwrap();
        assert!(info.is_dir);
    }

    #[test]
    fn copy_file_duplicates_contents() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.txt");
        let dst = dir.path().join("dst.txt");
        std::fs::write(&src, b"copy me").unwrap();

        copy_file(&src, &dst).unwrap();
        assert_eq!(std::fs::read(&dst).unwrap(), b"copy me");
        // Source is left intact.
        assert_eq!(std::fs::read(&src).unwrap(), b"copy me");
    }

    #[test]
    fn copy_file_missing_source_errors() {
        let dir = tempfile::tempdir().unwrap();
        let err = copy_file(&dir.path().join("missing"), &dir.path().join("dst")).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    }

    #[test]
    fn remove_path_deletes_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        std::fs::write(&path, b"x").unwrap();

        remove_path(&path, false).unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn remove_path_without_recursive_fails_on_a_nonempty_directory() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("f.txt"), b"x").unwrap();

        assert!(remove_path(&sub, false).is_err());
        assert!(sub.exists());
    }

    #[test]
    fn remove_path_recursive_deletes_a_nonempty_directory() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("f.txt"), b"x").unwrap();

        remove_path(&sub, true).unwrap();
        assert!(!sub.exists());
    }
}
