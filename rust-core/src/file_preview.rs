//! タスク#17「ファイルプレビュー機能」: `isekai-pipe ctl file ls|cat|info` を
//! リモートホスト上で1回限りの `exec` チャネル(PTY/対話シェルを一切経由しない、
//! `transport::ssh_handler::run_ssh_channel_loop` の `TransportCommand::FilePreviewExec`)
//! として実行し、その標準出力(1行のJSON、`isekai-pipe/src/ctl_file.rs` 参照)を
//! パースして構造化された [`FilePreviewOutcome`] に変換するまでの処理。
//!
//! `rust-ssot.md` に従い、ワイヤーフォーマット(JSON)のパース・base64デコード・
//! シェルクォーティングはすべてここ(Rust側)で完結させる——Kotlin側は
//! [`FilePreviewOutcome`] という既にデコード済みの構造体を受け取るだけでよく、
//! JSONパーサーもbase64デコーダーも持つ必要がない。
//!
//! `isekai-pipe ctl file` は `setvar`/`getvar`/`title`/`clip` と違い ctl-socket-forward
//! (`$ISEKAI_CTL_SOCK`)を一切使わず、`ssh host isekai-pipe ctl file ls <path>` という
//! 単発のリモートコマンド実行として動く(`ctl_file.rs` のモジュールdoc参照)。このため
//! `orchestrator.rs` からは「対話シェルのPTYチャネルとは別に、同じ`client::Handle`上へ
//! もう1本 `exec` チャネルを開いて結果を待つ」経路が必要で、それが
//! `transport::ssh_handler::run_ssh_channel_loop` に追加した `TransportCommand::FilePreviewExec`
//! の役目(実体は `transport::file_preview_exec::run_file_preview_exec`)。

use serde::Deserialize;

/// `isekai-pipe ctl file cat` の1回のチャンク上限(`ctl_file.rs::MAX_FILE_CAT_CHUNK_LEN`と
/// 同じ値、リモート側でも独立にクランプされるがKotlin側がページング時のデフォルト
/// チャンクサイズを決める参考値として使えるようUniFFI越しに公開する)。
pub const FILE_PREVIEW_MAX_CAT_CHUNK_LEN: u64 = 8 * 1024 * 1024;

/// ディレクトリエントリ1件(`isekai-pipe ctl file ls`の結果)。
#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct FilePreviewEntry {
    pub name: String,
    pub is_dir: bool,
    pub is_symlink: bool,
    pub size: u64,
    pub modified_unix: Option<i64>,
}

/// `SessionOrchestrator::file_preview_request`への要求種別。`ctl_file.rs`の
/// `ls`/`cat`/`info`サブコマンドに対応する(`cp`/`rm`はこのタスクのスコープ外
/// — プレビューは読み取り専用、削除/コピーはtrzsz転送シート等の既存導線に任せる)。
#[derive(Debug, Clone, PartialEq, uniffi::Enum)]
pub enum FilePreviewRequestKind {
    Ls { path: String },
    /// `length`が`None`なら「ファイル末尾まで(ただし8MiB上限でクランプ)」を要求する。
    /// 大きなファイルはKotlin側が`offset += 返ってきたlength`でページングし続ける
    /// (`ctl_file.rs`のドキュメント通り)。
    Cat { path: String, offset: u64, length: Option<u64> },
    Info { path: String },
}

/// `file_preview_request`の非同期結果。`OrchestratorCallback::on_file_preview_result`で
/// 届く。
#[derive(Debug, Clone, PartialEq, uniffi::Enum)]
pub enum FilePreviewOutcome {
    Ls { entries: Vec<FilePreviewEntry> },
    Cat { offset: u64, length: u64, total_size: u64, eof: bool, data: Vec<u8> },
    Info {
        name: String,
        path: String,
        is_dir: bool,
        is_symlink: bool,
        size: u64,
        modified_unix: Option<i64>,
        permissions_unix: Option<u32>,
    },
    /// I/Oエラー(`ctl_file.rs`の`{"ok":false,"error":...}`)・exec自体の失敗
    /// (未接続・チャネルオープン失敗)・JSONパース失敗のいずれか。呼び出し元は
    /// 種別を区別する必要が無いので単一のバリアントにまとめている。
    Error { message: String },
}

// ── ワイヤーフォーマット(`isekai-pipe/src/ctl_file.rs`のJSON出力と1:1) ──

#[derive(Debug, Deserialize)]
struct WireLsEntry {
    name: String,
    is_dir: bool,
    is_symlink: bool,
    size: u64,
    modified_unix: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct WireLsResult {
    entries: Vec<WireLsEntry>,
}

#[derive(Debug, Deserialize)]
struct WireCatResult {
    offset: u64,
    length: u64,
    total_size: u64,
    eof: bool,
    data_b64: String,
}

#[derive(Debug, Deserialize)]
struct WireInfoResult {
    name: String,
    path: String,
    is_dir: bool,
    is_symlink: bool,
    size: u64,
    modified_unix: Option<i64>,
    permissions_unix: Option<u32>,
}

/// `cp`/`rm`と同じ`{"ok":false,"error":"..."}`エラー封筒。`ok:true`側
/// (`FileOpResult::ok()`)はこのタスクでは使わないサブコマンド用なので無視してよい。
#[derive(Debug, Deserialize)]
struct WireErrorEnvelope {
    ok: bool,
    #[serde(default)]
    error: Option<String>,
}

/// シェルの単語分割・展開を防ぐPOSIX単一引用符クォート。英数字と`-_./:~`のみで
/// 構成される「安全に見える」引数はそのまま(可読性のため)、それ以外は
/// `'...'`で囲みつつ埋め込まれた`'`を`'\''`にエスケープする(標準的な手法)。
///
/// `~`をあえて「安全」側に含めているのは、ディレクトリブラウザの初期パス
/// (Android側`FilePreviewSheet`の既定値`"~"`)がホームディレクトリ展開に頼っている
/// ため——単一引用符で囲んでしまうとPOSIXシェルのチルダ展開が起きず、
/// `isekai-pipe`が文字通りの`"~"`というパス名を受け取ってENOENTになってしまう
/// (チルダ展開は語頭の`~`でのみ働くので、`~`をクォートせず素通しすること自体は
/// 他の文字と違い安全側に倒れる: シェルのメタ文字ではなく単なる展開トリガーであり、
/// 語頭以外に現れても展開されない=実害が無い)。
fn shell_quote(arg: &str) -> String {
    let looks_safe = !arg.is_empty()
        && arg.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/' | ':' | '~'));
    if looks_safe {
        return arg.to_string();
    }
    format!("'{}'", arg.replace('\'', "'\\''"))
}

/// `TransportCommand::FilePreviewExec`のexecチャネルへそのまま渡すコマンド文字列を
/// 組み立てる(`isekai-pipe ctl file <subcommand> <quoted args...>`)。
pub(crate) fn build_command_line(kind: &FilePreviewRequestKind) -> String {
    let mut args: Vec<String> = vec!["isekai-pipe".to_string(), "ctl".to_string(), "file".to_string()];
    match kind {
        FilePreviewRequestKind::Ls { path } => {
            args.push("ls".to_string());
            args.push(path.clone());
        }
        FilePreviewRequestKind::Cat { path, offset, length } => {
            args.push("cat".to_string());
            args.push(path.clone());
            args.push("--offset".to_string());
            args.push(offset.to_string());
            if let Some(length) = length {
                args.push("--length".to_string());
                args.push(length.to_string());
            }
        }
        FilePreviewRequestKind::Info { path } => {
            args.push("info".to_string());
            args.push(path.clone());
        }
    }
    args.iter().map(|a| shell_quote(a)).collect::<Vec<_>>().join(" ")
}

/// execチャネルのstdout/exit statusを、要求した`kind`に応じて[`FilePreviewOutcome`]へ
/// 変換する。`{"ok":false,"error":...}`エラー封筒はexit statusに関わらず最優先で
/// 解釈する(`ctl_file.rs`が非0 exitと一緒にこれを出す契約のため、ここで先に
/// 拾えれば`exit_status`だけを見た汎用エラーより分かりやすいメッセージになる)。
pub(crate) fn parse_result(
    kind: &FilePreviewRequestKind,
    exit_status: Option<u32>,
    stdout: &[u8],
) -> FilePreviewOutcome {
    let text = String::from_utf8_lossy(stdout);
    let trimmed = text.trim();

    if let Ok(envelope) = serde_json::from_str::<WireErrorEnvelope>(trimmed) {
        if !envelope.ok {
            return FilePreviewOutcome::Error {
                message: envelope.error.unwrap_or_else(|| "isekai-pipe ctl file failed".to_string()),
            };
        }
    }

    if exit_status != Some(0) {
        return FilePreviewOutcome::Error {
            message: format!("isekai-pipe ctl file exited with status {exit_status:?}: {trimmed}"),
        };
    }

    match kind {
        FilePreviewRequestKind::Ls { .. } => match serde_json::from_str::<WireLsResult>(trimmed) {
            Ok(r) => FilePreviewOutcome::Ls {
                entries: r
                    .entries
                    .into_iter()
                    .map(|e| FilePreviewEntry {
                        name: e.name,
                        is_dir: e.is_dir,
                        is_symlink: e.is_symlink,
                        size: e.size,
                        modified_unix: e.modified_unix,
                    })
                    .collect(),
            },
            Err(e) => FilePreviewOutcome::Error { message: format!("failed to parse ls result: {e}") },
        },
        FilePreviewRequestKind::Cat { .. } => match serde_json::from_str::<WireCatResult>(trimmed) {
            Ok(r) => match base64::Engine::decode(&base64::engine::general_purpose::STANDARD, &r.data_b64) {
                Ok(data) => FilePreviewOutcome::Cat {
                    offset: r.offset,
                    length: r.length,
                    total_size: r.total_size,
                    eof: r.eof,
                    data,
                },
                Err(e) => FilePreviewOutcome::Error { message: format!("failed to decode cat data: {e}") },
            },
            Err(e) => FilePreviewOutcome::Error { message: format!("failed to parse cat result: {e}") },
        },
        FilePreviewRequestKind::Info { .. } => match serde_json::from_str::<WireInfoResult>(trimmed) {
            Ok(r) => FilePreviewOutcome::Info {
                name: r.name,
                path: r.path,
                is_dir: r.is_dir,
                is_symlink: r.is_symlink,
                size: r.size,
                modified_unix: r.modified_unix,
                permissions_unix: r.permissions_unix,
            },
            Err(e) => FilePreviewOutcome::Error { message: format!("failed to parse info result: {e}") },
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── shell_quote / build_command_line ──

    #[test]
    fn build_command_line_for_ls_quotes_simple_path_unquoted() {
        let kind = FilePreviewRequestKind::Ls { path: "/home/user/proj".to_string() };
        assert_eq!(build_command_line(&kind), "isekai-pipe ctl file ls /home/user/proj");
    }

    #[test]
    fn build_command_line_quotes_path_with_spaces() {
        let kind = FilePreviewRequestKind::Ls { path: "/home/user/my dir".to_string() };
        assert_eq!(build_command_line(&kind), "isekai-pipe ctl file ls '/home/user/my dir'");
    }

    #[test]
    fn build_command_line_escapes_embedded_single_quote() {
        let kind = FilePreviewRequestKind::Ls { path: "/tmp/it's-a-dir".to_string() };
        assert_eq!(build_command_line(&kind), "isekai-pipe ctl file ls '/tmp/it'\\''s-a-dir'");
    }

    #[test]
    fn build_command_line_for_cat_with_offset_only() {
        let kind = FilePreviewRequestKind::Cat { path: "/var/log/app.log".to_string(), offset: 0, length: None };
        assert_eq!(build_command_line(&kind), "isekai-pipe ctl file cat /var/log/app.log --offset 0");
    }

    #[test]
    fn build_command_line_for_cat_with_offset_and_length() {
        let kind = FilePreviewRequestKind::Cat {
            path: "/var/log/app.log".to_string(),
            offset: 1024,
            length: Some(4096),
        };
        assert_eq!(
            build_command_line(&kind),
            "isekai-pipe ctl file cat /var/log/app.log --offset 1024 --length 4096"
        );
    }

    #[test]
    fn build_command_line_for_info() {
        let kind = FilePreviewRequestKind::Info { path: "/etc/hostname".to_string() };
        assert_eq!(build_command_line(&kind), "isekai-pipe ctl file info /etc/hostname");
    }

    #[test]
    fn build_command_line_leaves_tilde_unquoted_so_the_remote_shell_still_expands_it() {
        // ディレクトリブラウザの既定初期パス("~")がホームディレクトリへ展開されるために
        // 必須の挙動(単一引用符で囲むとPOSIXシェルのチルダ展開が起きなくなる)。
        let kind = FilePreviewRequestKind::Ls { path: "~".to_string() };
        assert_eq!(build_command_line(&kind), "isekai-pipe ctl file ls ~");

        let kind = FilePreviewRequestKind::Ls { path: "~/projects".to_string() };
        assert_eq!(build_command_line(&kind), "isekai-pipe ctl file ls ~/projects");
    }

    // ── parse_result: ls ──

    #[test]
    fn parse_result_ls_success() {
        let kind = FilePreviewRequestKind::Ls { path: "/tmp".to_string() };
        let stdout = br#"{"entries":[{"name":"a.txt","is_dir":false,"is_symlink":false,"size":5,"modified_unix":1700000000}]}"#;
        let outcome = parse_result(&kind, Some(0), stdout);
        match outcome {
            FilePreviewOutcome::Ls { entries } => {
                assert_eq!(entries.len(), 1);
                assert_eq!(entries[0].name, "a.txt");
                assert_eq!(entries[0].size, 5);
                assert!(!entries[0].is_dir);
            }
            other => panic!("expected Ls, got {other:?}"),
        }
    }

    #[test]
    fn parse_result_ls_error_envelope() {
        let kind = FilePreviewRequestKind::Ls { path: "/no/such/dir".to_string() };
        let stdout = br#"{"ok":false,"error":"No such file or directory (os error 2)"}"#;
        let outcome = parse_result(&kind, Some(74), stdout);
        match outcome {
            FilePreviewOutcome::Error { message } => assert!(message.contains("No such file")),
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn parse_result_nonzero_exit_without_json_is_generic_error() {
        let kind = FilePreviewRequestKind::Ls { path: "/tmp".to_string() };
        let outcome = parse_result(&kind, Some(127), b"command not found");
        match outcome {
            FilePreviewOutcome::Error { message } => assert!(message.contains("127")),
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn parse_result_missing_exit_status_is_error() {
        let kind = FilePreviewRequestKind::Ls { path: "/tmp".to_string() };
        let outcome = parse_result(&kind, None, b"{\"entries\":[]}");
        assert!(matches!(outcome, FilePreviewOutcome::Error { .. }));
    }

    // ── parse_result: cat ──

    #[test]
    fn parse_result_cat_decodes_base64_data() {
        let kind = FilePreviewRequestKind::Cat { path: "/tmp/f.txt".to_string(), offset: 0, length: None };
        // "hello" base64-encoded.
        let stdout = br#"{"offset":0,"length":5,"total_size":5,"eof":true,"data_b64":"aGVsbG8="}"#;
        let outcome = parse_result(&kind, Some(0), stdout);
        match outcome {
            FilePreviewOutcome::Cat { offset, length, total_size, eof, data } => {
                assert_eq!(offset, 0);
                assert_eq!(length, 5);
                assert_eq!(total_size, 5);
                assert!(eof);
                assert_eq!(data, b"hello");
            }
            other => panic!("expected Cat, got {other:?}"),
        }
    }

    #[test]
    fn parse_result_cat_invalid_base64_is_error() {
        let kind = FilePreviewRequestKind::Cat { path: "/tmp/f.txt".to_string(), offset: 0, length: None };
        let stdout = br#"{"offset":0,"length":3,"total_size":3,"eof":true,"data_b64":"!!!not-base64!!!"}"#;
        let outcome = parse_result(&kind, Some(0), stdout);
        assert!(matches!(outcome, FilePreviewOutcome::Error { .. }));
    }

    // ── parse_result: info ──

    #[test]
    fn parse_result_info_success() {
        let kind = FilePreviewRequestKind::Info { path: "/etc/hostname".to_string() };
        let stdout = br#"{"name":"hostname","path":"/etc/hostname","is_dir":false,"is_symlink":false,"size":9,"modified_unix":1700000000,"permissions_unix":420}"#;
        let outcome = parse_result(&kind, Some(0), stdout);
        match outcome {
            FilePreviewOutcome::Info { name, path, is_dir, permissions_unix, .. } => {
                assert_eq!(name, "hostname");
                assert_eq!(path, "/etc/hostname");
                assert!(!is_dir);
                assert_eq!(permissions_unix, Some(420));
            }
            other => panic!("expected Info, got {other:?}"),
        }
    }

    #[test]
    fn parse_result_wrong_shape_json_is_error() {
        // `ls`のペイロード形状を`info`要求に対して渡した場合、フィールドが合わず
        // パース失敗として扱われるべき(サイレントに壊れたデータを返さない)。
        let kind = FilePreviewRequestKind::Info { path: "/tmp".to_string() };
        let stdout = br#"{"entries":[]}"#;
        let outcome = parse_result(&kind, Some(0), stdout);
        assert!(matches!(outcome, FilePreviewOutcome::Error { .. }));
    }
}
