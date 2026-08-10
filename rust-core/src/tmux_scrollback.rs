//! タスク#58: フル再接続(byte-exact resumeが諦めた後の新規ATTACH)直後に
//! tmux自身のscrollback履歴を`run_exec`(#61) + `tmux capture-pane`で取り込み、
//! ローカルscrollback(`session.rs::SessionCore::inject_scrollback_history`)へ
//! バッチ注入するための、実際のtmuxコマンド組み立て・出力パース部分。
//!
//! # なぜ`capture-pane`の範囲を`-S -<N> -E -1`にするか
//!
//! `tmux capture-pane`の行番号は、現在の可視画面の最上段を`0`とし、そこから
//! 下へ`rows - 1`まで、上(scrollback方向)へは`-1`から`-history_size`まで
//! 負数で続く。つまり:
//!
//! - `0`以上 == 現在の可視画面(このバックフィルが後で上書きする、
//!   ライブアタッチのPTY再描画と全く同じ内容)
//! - `-1`以下 == 可視画面より上のscrollback履歴のみ
//!
//! ライブアタッチ後、tmuxはアタッチのたびに可視画面を再描画する。もし
//! ここで`-E 0`(またはデフォルトの`-E`省略、これは可視画面の最終行=
//! `rows - 1`まで含む)を使うと、可視画面の内容がバックフィル注入と
//! ライブ再描画の両方で二重に表示されてしまう。`-E -1`を使うことで
//! 可視画面を完全に除外し、その手前(過去)のscrollback行だけを取得する。
//! `-S`の下限(`-N`)は実際のhistory sizeより大きい値を渡してもtmux側で
//! 自動的に実際の範囲へクランプされる(tmux自身の仕様、`man tmux`の
//! `capture-pane`の項参照)ため、`N`は「これ以上は要らない」という
//! 上限を表すだけでよい —— ここではローカルscrollbackが持つのと同じ
//! 上限(`session::SCROLLBACK_LIMIT`)をそのまま再利用し、新しい数値を
//! 発明しない(呼び出し元`orchestrator.rs::spawn_tmux_scrollback_backfill`)。
//!
//! # なぜ`-e`(ANSI装飾の保持)を使わないか
//!
//! `capture-pane -e`は各行にSGRエスケープシーケンスを埋め込んで色/太字を
//! 再現できるが、行をまたいだ色の持ち越し状態(ある行の途中で始まった
//! 色指定が次行の先頭にも及ぶケース)を正しく復元するには行単位でなく
//! ミニVTEパーサ相当の状態機械が要り、この機能の価値(切断中に流れた
//! 履歴が読めるようになること)に対して複雑さが見合わない。プレーン
//! テキスト(装飾無し、現在のテーマの既定前景/背景で塗る)で妥協する
//! ——この妥協は呼び出し元のコメントにも明記してある。
//!
//! # #61/#62との関係
//!
//! 実際にリモートでコマンドを実行する部分(#61のexecチャンネル)、および
//! アプリのタブ/ペイン⟷tmuxロケータの対応表(#62の`TmuxLocatorRegistry`)
//! はこのモジュールの外の関心事。ここは[`crate::tmux_locator`]が既に
//! 定義している[`RemoteTmuxCommandRunner`]シームと[`TmuxLocator`]型に
//! 対してのみ書く(このモジュール自身はどちらの実装にも依存しない)。

use crate::tmux_locator::{
    build_list_command, parse_list_output, shell_quote, RemoteTmuxCommandRunner, TmuxLocator, TmuxRunError,
    TmuxTag, TmuxTargetKind, TmuxSessionScope, TmuxCoordinates,
};

/// [`fetch_tmux_scrollback_history`]の失敗。呼び出し元(`orchestrator.rs`)は
/// これを見ずに一律fail-open(ログして通常の空scrollback再接続を続行)するが、
/// 診断ログ用にどこで失敗したかは区別できるようにしておく。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum TmuxScrollbackFetchError {
    /// このバックフィルは常に特定の1ペインを対象にする(#62モジュールdoc
    /// 「各タブはプライマリペイン+最大1つのsplitペインを持ち、それぞれ独立した
    /// session/orchestratorを持つ」)ため、ウィンドウ単位のロケータは対象外。
    #[error("tmux scrollback backfill requires a pane-kind TmuxLocator, got a window-kind one")]
    LocatorIsNotAPane,
    #[error(transparent)]
    Command(#[from] TmuxRunError),
    /// タグ付け済みのペインが見つからなかった(リモートでkill-pane済み等)。
    #[error("tmux pane for locator tag {0:?} was not found (it may have been closed)")]
    PaneNotFound(TmuxTag),
}

/// `locator`が指すペインの`scope`/現在座標へ向けて、可視画面より上の
/// scrollback履歴だけを`max_lines`行分(それより少なければ実際にある分だけ)
/// プレーンテキストで取得する`tmux capture-pane`コマンドを組み立てる。
/// アドレッシング形式(`session:window.pane`)は`tmux_locator::build_set_tag_command`
/// と同じ規約を踏襲する。
pub(crate) fn build_capture_pane_command(scope: &TmuxSessionScope, coords: &TmuxCoordinates, max_lines: usize) -> String {
    let session = scope.addressable_session_name();
    let window = coords.window_index;
    // このバックフィルは常にペイン単位のロケータからしか呼ばれない
    // (`fetch_tmux_scrollback_history`が`TmuxTargetKind::Pane`を要求し、
    // `parse_list_output`はPane種別なら常に`pane_index: Some(_)`を返す)。
    // それでも呼び出し側の契約が崩れた場合に備え、`unwrap_or(0)`ではなく
    // 明示的にウィンドウ全体を指す座標へフォールバックする(pane_index無しの
    // `-t session:window`は「そのウィンドウの現在アクティブなペイン」を指す、
    // tmux自身の既定動作であり、誤って別セッション/別ホストを叩くよりは
    // 安全な縮退)。
    let target = match coords.pane_index {
        Some(pane) => format!("{session}:{window}.{pane}"),
        None => format!("{session}:{window}"),
    };
    format!("tmux capture-pane -p -t {} -S -{max_lines} -E -1", shell_quote(&target))
}

/// [`build_capture_pane_command`]の出力(`tmux capture-pane -p`の標準出力)を
/// 行ごとに分割する。末尾の改行の有無どちらでも余計な空行を生まない
/// (`str::lines`の仕様通り)。
pub(crate) fn parse_capture_pane_output(output: &str) -> Vec<String> {
    output.lines().map(str::to_owned).collect()
}

/// `locator`(ペイン単位である必要がある)を`runner`越しに現在の座標へ解決し、
/// そのペインのscrollback履歴(可視画面より上、最大`max_lines`行)を
/// プレーンテキスト行のバッチとして返す。
///
/// 返す`Vec<String>`はtmuxの`capture-pane`が返した順序のまま
/// (古い→新しい、可視画面に一番近い行が最後)—— 呼び出し元
/// (`session::SessionCore::inject_scrollback_history`)がその順序をそのまま
/// 前提にしているので、ここで並べ替えてはいけない。
pub(crate) async fn fetch_tmux_scrollback_history<R: RemoteTmuxCommandRunner>(
    runner: &R,
    locator: &TmuxLocator,
    max_lines: usize,
) -> Result<Vec<String>, TmuxScrollbackFetchError> {
    if locator.kind != TmuxTargetKind::Pane {
        return Err(TmuxScrollbackFetchError::LocatorIsNotAPane);
    }
    let list_cmd = build_list_command(&locator.scope, locator.kind);
    let list_output = runner.run(&list_cmd).await?;
    let coords = parse_list_output(locator.kind, &list_output, &locator.tag)
        .ok_or_else(|| TmuxScrollbackFetchError::PaneNotFound(locator.tag.clone()))?;
    let capture_cmd = build_capture_pane_command(&locator.scope, &coords, max_lines);
    let output = runner.run(&capture_cmd).await?;
    Ok(parse_capture_pane_output(&output))
}

#[cfg(test)]
mod tests {
    use super::*;
    // tmux_locator.rs/tmux_notify.rsと同一定義だった`standalone`をtest_supportへ
    // 共通化した。
    use crate::tmux_locator::test_support::standalone;
    use std::future::Future;
    use std::sync::Mutex;

    fn pane_locator(session: &str, tag: &str) -> TmuxLocator {
        TmuxLocator { scope: standalone(session), kind: TmuxTargetKind::Pane, tag: TmuxTag(tag.to_string()) }
    }

    fn window_locator(session: &str, tag: &str) -> TmuxLocator {
        TmuxLocator { scope: standalone(session), kind: TmuxTargetKind::Window, tag: TmuxTag(tag.to_string()) }
    }

    // ── build_capture_pane_command ───────────────────────

    #[test]
    fn build_capture_pane_command_targets_the_resolved_pane() {
        let cmd = build_capture_pane_command(
            &standalone("main"),
            &TmuxCoordinates { window_index: 2, pane_index: Some(1) },
            1000,
        );
        assert_eq!(cmd, "tmux capture-pane -p -t 'main:2.1' -S -1000 -E -1");
    }

    #[test]
    fn build_capture_pane_command_range_excludes_the_visible_screen() {
        // `-E -1`は可視画面の最初の行(行番号0)の1つ手前までを意味し、
        // 可視画面そのもの(0..=rows-1)を一切含まない —— ライブアタッチ後の
        // tmux再描画と内容が二重にならないための核心部分。
        let cmd = build_capture_pane_command(
            &standalone("main"),
            &TmuxCoordinates { window_index: 0, pane_index: Some(0) },
            500,
        );
        assert!(cmd.contains("-E -1"), "capture-pane must end strictly before the visible screen: {cmd}");
        assert!(!cmd.contains("-E 0"), "must not accidentally include the visible screen's first row: {cmd}");
        assert!(cmd.contains("-S -500"), "capture-pane must bound the start to the caller's max_lines: {cmd}");
    }

    #[test]
    fn build_capture_pane_command_falls_back_to_window_target_when_pane_index_missing() {
        // 契約上は起こらない想定(呼び出し元がPane種別のロケータしか渡さない)だが、
        // 万一`pane_index: None`が来ても別セッション/別ホストへ誤爆しない安全な
        // 縮退(ウィンドウの現在アクティブペイン)になっていることを確認する。
        let cmd = build_capture_pane_command(
            &standalone("main"),
            &TmuxCoordinates { window_index: 3, pane_index: None },
            10,
        );
        assert_eq!(cmd, "tmux capture-pane -p -t 'main:3' -S -10 -E -1");
    }

    // ── parse_capture_pane_output ─────────────────────────

    #[test]
    fn parse_capture_pane_output_splits_into_lines_in_order() {
        let output = "first\nsecond\nthird";
        assert_eq!(parse_capture_pane_output(output), vec!["first", "second", "third"]);
    }

    #[test]
    fn parse_capture_pane_output_trailing_newline_does_not_add_an_empty_line() {
        let output = "first\nsecond\n";
        assert_eq!(parse_capture_pane_output(output), vec!["first", "second"]);
    }

    #[test]
    fn parse_capture_pane_output_empty_output_is_empty_history() {
        assert!(parse_capture_pane_output("").is_empty());
    }

    #[test]
    fn parse_capture_pane_output_preserves_blank_lines_in_the_middle() {
        let output = "first\n\nthird";
        assert_eq!(parse_capture_pane_output(output), vec!["first", "", "third"]);
    }

    // ── fetch_tmux_scrollback_history (フェイクrunner越し) ────
    // `tmux_locator.rs`と同じ慣習(重量なモックより実/フェイク実装)に沿った
    // フェイク。固定の応答を、呼ばれた順に1つずつ返す(list-panes → capture-pane
    // の2回呼ばれる想定)。

    struct FakeRunner {
        responses: Mutex<Vec<Result<String, TmuxRunError>>>,
        calls: Mutex<Vec<String>>,
    }

    impl FakeRunner {
        fn new(responses: Vec<Result<String, TmuxRunError>>) -> Self {
            Self { responses: Mutex::new(responses), calls: Mutex::new(Vec::new()) }
        }
        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl RemoteTmuxCommandRunner for FakeRunner {
        fn run(&self, cmd: &str) -> impl Future<Output = Result<String, TmuxRunError>> + Send {
            self.calls.lock().unwrap().push(cmd.to_string());
            let mut responses = self.responses.lock().unwrap();
            let response = if responses.is_empty() {
                Err(TmuxRunError("FakeRunner: no more responses queued".to_string()))
            } else {
                responses.remove(0)
            };
            async move { response }
        }
    }

    #[tokio::test]
    async fn fetch_resolves_the_pane_then_captures_only_its_scrollback_history() {
        let runner = FakeRunner::new(vec![
            Ok("0\t0\tother\n0\t1\tmy-tag\n".to_string()),
            Ok("line one\nline two\n".to_string()),
        ]);
        let locator = pane_locator("main", "my-tag");

        let lines = fetch_tmux_scrollback_history(&runner, &locator, 1000).await.unwrap();

        assert_eq!(lines, vec!["line one", "line two"]);
        let calls = runner.calls();
        assert_eq!(calls.len(), 2);
        assert!(calls[0].starts_with("tmux list-panes"), "first call should resolve coordinates: {calls:?}");
        assert_eq!(calls[1], "tmux capture-pane -p -t 'main:0.1' -S -1000 -E -1");
    }

    #[tokio::test]
    async fn fetch_rejects_a_window_kind_locator_without_running_any_command() {
        let runner = FakeRunner::new(vec![Ok("unused".to_string())]);
        let locator = window_locator("main", "my-tag");

        let err = fetch_tmux_scrollback_history(&runner, &locator, 1000).await.unwrap_err();

        assert_eq!(err, TmuxScrollbackFetchError::LocatorIsNotAPane);
        assert!(runner.calls().is_empty(), "must fail before touching the network at all");
    }

    #[tokio::test]
    async fn fetch_fails_when_the_pane_is_not_found() {
        let runner = FakeRunner::new(vec![Ok("0\t0\tother\n".to_string())]);
        let locator = pane_locator("main", "missing-tag");

        let err = fetch_tmux_scrollback_history(&runner, &locator, 1000).await.unwrap_err();

        assert_eq!(err, TmuxScrollbackFetchError::PaneNotFound(TmuxTag("missing-tag".to_string())));
    }

    #[tokio::test]
    async fn fetch_propagates_a_command_error_from_the_resolve_step() {
        let runner = FakeRunner::new(vec![Err(TmuxRunError("ssh channel closed".to_string()))]);
        let locator = pane_locator("main", "my-tag");

        let err = fetch_tmux_scrollback_history(&runner, &locator, 1000).await.unwrap_err();

        assert_eq!(err, TmuxScrollbackFetchError::Command(TmuxRunError("ssh channel closed".to_string())));
    }

    #[tokio::test]
    async fn fetch_propagates_a_command_error_from_the_capture_step() {
        let runner = FakeRunner::new(vec![
            Ok("0\t1\tmy-tag\n".to_string()),
            Err(TmuxRunError("exec channel died mid-capture".to_string())),
        ]);
        let locator = pane_locator("main", "my-tag");

        let err = fetch_tmux_scrollback_history(&runner, &locator, 1000).await.unwrap_err();

        assert_eq!(err, TmuxScrollbackFetchError::Command(TmuxRunError("exec channel died mid-capture".to_string())));
    }

    #[tokio::test]
    async fn fetch_returns_empty_history_when_the_pane_has_no_scrollback_yet() {
        let runner = FakeRunner::new(vec![Ok("0\t1\tmy-tag\n".to_string()), Ok(String::new())]);
        let locator = pane_locator("main", "my-tag");

        let lines = fetch_tmux_scrollback_history(&runner, &locator, 1000).await.unwrap();

        assert!(lines.is_empty());
    }
}
