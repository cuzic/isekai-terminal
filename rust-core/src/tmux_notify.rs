//! タスク#57: tmux hooks(`alert-bell`/`alert-activity`/`alert-silence`/
//! `pane-died`)を、リモートのtmuxウィンドウ/ペインへインストールするコマンド
//! 組み立て。フックが発火すると`isekai-pipe ctl notify`(`isekai-pipe/src/ctl.rs`)
//! を`run-shell`経由で起動し、`isekai_protocol::CtlMessage::Notify`をこのタブの
//! ctl-socket(`@isekai_ctl_sock`、`tmux_locator.rs`参照)越しに送り返す。受信側は
//! `session.rs`の`session_event_loop`(`TransportEvent::CtlMessage`アーム)。
//!
//! # 実tmux(3.3a)で検証済みの意味論(このモジュールの設計はすべて下記の実測に基づく。
//! 推測でコマンドを組んでいない)
//!
//! - フック名は`alert-bell`/`alert-activity`/`alert-silence`/`pane-died`
//!   (`bell`という名前のフックは存在しない — man tmux(1) "HOOKS"節で確認済み)。
//! - **`alert-bell`/`alert-activity`/`alert-silence`はセッションスコープのみ有効**。
//!   `set-hook -w -t <session>:<window>`のように`-w`を付けても、実測では黙って
//!   セッションスコープに丸められる(`show-hooks -w`では見えない、`show-hooks`
//!   (無指定=セッション)でだけ見える)。一方これらは、セッション内のどのウィンドウで
//!   発火しても`#{@isekai_tab_id}`等のフォーマット指定子が「発火したウィンドウ」の
//!   コンテキストで解決されるため、**タブごとに別々のフックをインストールする必要は
//!   ない**——1セッションにつき1回、3つのフックをインストールするだけで、
//!   そのセッション配下の全ウィンドウ(=全タブ)をカバーできる。
//! - **`pane-died`は逆にウィンドウ/ペインスコープ(`-w`/`-p`)でしか登録できない**
//!   (実測: `-t <session>`のみのbare指定では`show-hooks -t <session>`に一切
//!   現れず、発火もしない。`-w -t <session>:<window>`なら`show-hooks -w`に現れ、
//!   実際に発火する)。都合の良いことに、tmux session group(#60)で複数の
//!   グループメンバー(=同じホストへの複数デバイス接続)が同じウィンドウを共有していても、
//!   ウィンドウスコープの値は**ウィンドウオブジェクト自体に1つだけ**存在する
//!   (実測: `client-b:1`経由で上書きすると`main:1`から見た値も変わる)ため、
//!   セッションスコープの3つのフックと違って**グループメンバー数だけ重複発火する
//!   ことがない**。
//! - `remain-on-exit`を**先に**有効にしておかないと、`pane-died`は発火する前に
//!   ウィンドウ自体が閉じてしまう(実測: 即終了するコマンドで5/5回レースに負けた)。
//!   厄介なのは、**ウィンドウ作成後に`-w`でそのウィンドウへ`remain-on-exit on`を
//!   設定してもレースには勝てない**(実測: 同条件で敗け続けた)ことで、唯一確実に
//!   勝てたのは**サーバー全体のグローバルデフォルト(`set-option -g`)を、ウィンドウ
//!   作成より前に**しておく方法だけだった。これは実際上、リモートホストの
//!   tmuxサーバー全体で`remain-on-exit`のデフォルトを恒久的に変える(isekai-terminal
//!   が管理していない、そのホスト上のユーザー自身の手動tmux操作にも影響する)という
//!   副作用を伴う——`install_notify_hooks`のdoc参照、最終報告でも明示するjudgment call。
//! - `#{q:...}`フォーマット修飾子(man tmux(1) FORMATS節)が値をsh(1)向けに
//!   バックスラッシュエスケープしてくれるため、`run-shell`へ渡す1行スクリプトの
//!   中で`tag=#{q:@isekai_tab_id}`のように**追加の引用符を自前で組み立てずに**
//!   安全に埋め込める(実測: スペースやシングルクォートを含む値でも1つの
//!   シェルワードとして壊れずに渡ることを確認済み)。これにより「シェル→tmux→sh」
//!   という3層の引用のうち最後の層の手組みエスケープを丸ごと避けられる。
//! - `seq`(`isekai_protocol::CtlMessage::Notify::seq`)はこのモジュールが管理する
//!   ウィンドウスコープのuser-option(`@isekai_notify_seq`)を`run-shell`スクリプト
//!   自身が読み取り→インクリメント→書き戻すことで払い出す。複数のグループメンバー
//!   セッションから同時に同じイベントの`alert-bell`等が重複発火した場合、この
//!   read-increment-writeは`flock`等で保護していないため理論上レースし得るが
//!   (最終報告のjudgment call参照)、`isekai_protocol::CtlMessage::Notify`の
//!   `(tmux_tag, seq)`重複排除は元々このような重複配信を想定した設計
//!   (`isekai-protocol/src/ctl.rs`のdocコメント参照)なので、許容している。

use parking_lot::Mutex;

use crate::tmux_locator::{
    shell_quote, AppPaneId, RemoteTmuxCommandRunner, TmuxCoordinates, TmuxLocatorError,
    TmuxLocatorRegistry, TmuxLocatorResolver,
};

/// セッションスコープでインストールする3つのフック: (tmuxフック名, wire上の
/// `NotifyKind`値)。`isekai_protocol::NotifyKind`のserde renameと完全に一致させる
/// (`isekai-protocol/src/ctl.rs`参照)。
const SESSION_SCOPED_HOOKS: [(&str, &str); 3] =
    [("alert-bell", "bell"), ("alert-activity", "activity"), ("alert-silence", "silence")];

/// ウィンドウスコープでしかインストールできないフック(モジュールdoc参照)。
const WINDOW_SCOPED_HOOK: (&str, &str) = ("pane-died", "job_done");

/// `alert-silence`が発火するまでの無出力秒数。既存の慣習・設計ドキュメントに
/// 具体的な値の指定が無かったため、"しばらく止まっていたら知らせる"というhookの
/// 趣旨に対する妥当な値としてこのタスクで決めたjudgment call(最終報告参照)。
const MONITOR_SILENCE_SECONDS: u32 = 30;

/// `$isekai_pipe`に、実行可能な`isekai-pipe`へのパスを解決して代入する
/// シェル片。tmuxサーバはSSH execチャンネル(非ログイン・非対話シェル)経由で
/// 起動されるため、`isekai-pipe`のインストール先
/// (`isekai_protocol::bootstrap::ISEKAI_PIPE_INSTALL_DIR` = `~/.local/bin`)が
/// PATHに入っているとは限らない(Debian系の`~/.bashrc`は非対話時に早期return
/// する)。bare `isekai-pipe`(PATH依存)だとその場合サイレントに失敗するため、
/// インストール先の絶対パス(`~`はHOMEから展開される。non-loginシェルでも
/// HOME自体はsshdがセットするため信頼できる、PATHと違いこの展開はシェル自身の
/// 組み込み機能でありPATH検索に依らない)を優先し、実行不可なら`isekai-pipe`
/// (PATH検索)へフォールバックする。
fn resolve_isekai_pipe_script() -> String {
    format!(
        "isekai_pipe={}/{}; [ -x \"$isekai_pipe\" ] || isekai_pipe={}",
        isekai_protocol::bootstrap::ISEKAI_PIPE_INSTALL_DIR,
        isekai_protocol::bootstrap::ISEKAI_PIPE_BIN_NAME,
        isekai_protocol::bootstrap::ISEKAI_PIPE_BIN_NAME,
    )
}

/// `run-shell`へ渡す1行のPOSIX shスクリプトを組み立てる。`#{q:...}`が値の
/// シェルエスケープを担うため(モジュールdoc参照)、ここでは値を追加で引用符化
/// しない——それをすると`#{q:...}`の二重エスケープになり壊れる。
fn notify_hook_script(kind_wire: &str) -> String {
    format!(
        "tag=#{{q:@isekai_tab_id}}; \
         if [ -n \"$tag\" ]; then \
         sock=#{{q:@isekai_ctl_sock}}; \
         tgt=#{{q:session_name}}:#{{window_index}}; \
         old=$(tmux show-options -wv -t \"$tgt\" @isekai_notify_seq 2>/dev/null); \
         old=${{old:-0}}; \
         new=$((old+1)); \
         tmux set-option -w -t \"$tgt\" @isekai_notify_seq \"$new\"; \
         {}; \
         \"$isekai_pipe\" ctl notify --kind {kind_wire} --tag \"$tag\" --seq \"$new\" --sock \"$sock\"; \
         fi",
        resolve_isekai_pipe_script()
    )
}

/// `tmux set-hook -t <session> <hook_name> 'run-shell ...'`(セッションスコープ、
/// `-w`/`-p`無し)。`SESSION_SCOPED_HOOKS`の3つに使う。
fn build_session_hook_command(session_name: &str, hook_name: &str, kind_wire: &str) -> String {
    let value = format!("run-shell {}", shell_quote(&notify_hook_script(kind_wire)));
    format!("tmux set-hook -t {} {hook_name} {}", shell_quote(session_name), shell_quote(&value))
}

/// `tmux set-hook -w -t <session>:<window> <hook_name> 'run-shell ...'`
/// (ウィンドウスコープ)。`WINDOW_SCOPED_HOOK`(`pane-died`)専用。
fn build_window_hook_command(
    session_name: &str,
    coords: &TmuxCoordinates,
    hook_name: &str,
    kind_wire: &str,
) -> String {
    let target = shell_quote(&format!("{session_name}:{}", coords.window_index));
    let value = format!("run-shell {}", shell_quote(&notify_hook_script(kind_wire)));
    format!("tmux set-hook -w -t {target} {hook_name} {}", shell_quote(&value))
}

/// `remain-on-exit`をサーバー全体のグローバルデフォルトとして有効にする。
/// `pane-died`が発火する前提条件(モジュールdoc参照——ウィンドウ作成後に
/// ウィンドウスコープで設定してもレースに勝てないことを実tmuxで確認済み)。
/// 既に`on`なら無害な冪等操作。
fn build_enable_remain_on_exit_command() -> String {
    "tmux set-option -g remain-on-exit on".to_string()
}

/// `monitor-activity`/`monitor-silence`をこのウィンドウで有効にする
/// (ウィンドウスコープ、`remain-on-exit`と違い作成後の設定で問題ない——
/// これらは継続的にチェックされる値であり、`remain-on-exit`のような
/// 「終了時の一発判定」特有のレースが無い)。両者ともデフォルトで無効
/// (`monitor-activity`は既定off、`monitor-silence`は既定0=無効)なので、
/// 明示的に有効化しないと`alert-activity`/`alert-silence`はそもそも発火しない。
fn build_enable_monitoring_command(session_name: &str, coords: &TmuxCoordinates) -> String {
    let target = shell_quote(&format!("{session_name}:{}", coords.window_index));
    format!(
        "tmux set-option -w -t {target} monitor-activity on; \
         tmux set-option -w -t {target} monitor-silence {MONITOR_SILENCE_SECONDS}"
    )
}

/// `remain-on-exit`のグローバル有効化 + 3つのセッションスコープフックのインストールを
/// 1回のexec呼び出し(`;`連結)にまとめたもの。往復回数を減らすため
/// (`install_notify_hooks`参照)。
fn build_install_session_hooks_command(session_name: &str) -> String {
    let mut cmds = vec![build_enable_remain_on_exit_command()];
    for (hook_name, kind_wire) in SESSION_SCOPED_HOOKS {
        cmds.push(build_session_hook_command(session_name, hook_name, kind_wire));
    }
    cmds.join("; ")
}

/// `pane-died`のウィンドウスコープフックインストール + monitor-activity/silence
/// 有効化を1回のexec呼び出しにまとめたもの。
fn build_install_window_hooks_command(session_name: &str, coords: &TmuxCoordinates) -> String {
    let (hook_name, kind_wire) = WINDOW_SCOPED_HOOK;
    format!(
        "{}; {}",
        build_window_hook_command(session_name, coords, hook_name, kind_wire),
        build_enable_monitoring_command(session_name, coords),
    )
}

/// タスク#57本体: `app_pane_id`に対応するtmuxロケータが[`TmuxLocatorRegistry`]に
/// 既に登録されていれば(#60等が解決済みの前提、`tmux_locator.rs`の
/// `push_ctl_socket_to_tmux`と同じ前提)、そのセッション/ウィンドウへ通知用の
/// フックをインストールする。呼び出し側(`transport::ssh_handler::run_ssh_channel_loop`)
/// は接続確立時・再接続時の両方でこれを呼ぶ想定——全コマンドが冪等
/// (同じ内容を何度設定し直しても無害)なので、毎回呼び直して問題ない。
///
/// ロケータが未登録の場合は`push_ctl_socket_to_tmux`と同じく黙ってno-opになる
/// (opportunistic機能)。tmux側への書き込みが失敗した場合(ウィンドウが既に
/// 閉じられている等)は`Err`を返す——呼び出し側はbest-effortとしてログするだけで
/// 接続自体は継続してよい。
pub(crate) async fn install_notify_hooks<R: RemoteTmuxCommandRunner>(
    registry: &Mutex<TmuxLocatorRegistry>,
    app_pane_id: &AppPaneId,
    runner: R,
) -> Result<(), TmuxLocatorError> {
    let (locator, notifications_enabled) = {
        let reg = registry.lock();
        (reg.locator_for(app_pane_id).cloned(), reg.notify_hooks_enabled_for(app_pane_id))
    };
    let Some(locator) = locator else { return Ok(()) };
    // `ConnectionProfile.enableTabNotifications`が無効なタブでは、リモートの
    // tmuxサーバーへ一切書き込まない(`build_enable_remain_on_exit_command`の
    // `set-option -g`はそのサーバー全体への恒久的な副作用を持つため、opt-in
    // していないユーザーにまで強制しない、モジュールdocのjudgment call参照)。
    if !notifications_enabled {
        return Ok(());
    }
    let resolver = TmuxLocatorResolver::new(runner);
    let session_name = locator.scope.addressable_session_name().to_string();

    resolver
        .run_raw(&build_install_session_hooks_command(&session_name))
        .await
        .map_err(TmuxLocatorError::Command)?;

    let coords = resolver.resolve(&locator).await?;
    resolver
        .run_raw(&build_install_window_hooks_command(&session_name, &coords))
        .await
        .map_err(TmuxLocatorError::Command)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tmux_locator::{TmuxLocator, TmuxTag, TmuxTargetKind};
    // tmux_locator.rs/tmux_scrollback.rsと同一定義だった`standalone`/`pane`/
    // `RecordingRunner`をtest_supportへ共通化した。
    use crate::tmux_locator::test_support::{pane, standalone, RecordingRunner};
    use std::sync::{Arc, Mutex as StdMutex};

    // ── コマンド組み立て(実tmux 3.3aで検証済みの文字列を固定でpin) ──────

    #[test]
    fn session_hook_command_targets_bare_session_no_scope_flag() {
        let cmd = build_session_hook_command("main", "alert-bell", "bell");
        assert_eq!(
            cmd,
            "tmux set-hook -t 'main' alert-bell 'run-shell '\\''tag=#{q:@isekai_tab_id}; if [ -n \"$tag\" ]; then sock=#{q:@isekai_ctl_sock}; tgt=#{q:session_name}:#{window_index}; old=$(tmux show-options -wv -t \"$tgt\" @isekai_notify_seq 2>/dev/null); old=${old:-0}; new=$((old+1)); tmux set-option -w -t \"$tgt\" @isekai_notify_seq \"$new\"; isekai_pipe=~/.local/bin/isekai-pipe; [ -x \"$isekai_pipe\" ] || isekai_pipe=isekai-pipe; \"$isekai_pipe\" ctl notify --kind bell --tag \"$tag\" --seq \"$new\" --sock \"$sock\"; fi'\\'''"
        );
    }

    #[test]
    fn window_hook_command_uses_dash_w_and_session_colon_window() {
        let cmd = build_window_hook_command(
            "main",
            &TmuxCoordinates { window_index: 3, pane_index: None },
            "pane-died",
            "job_done",
        );
        assert!(cmd.starts_with("tmux set-hook -w -t 'main:3' pane-died "));
        assert!(cmd.contains("--kind job_done"));
    }

    #[test]
    fn enable_remain_on_exit_command_is_global_scope() {
        assert_eq!(build_enable_remain_on_exit_command(), "tmux set-option -g remain-on-exit on");
    }

    #[test]
    fn enable_monitoring_command_targets_specific_window() {
        let cmd = build_enable_monitoring_command("main", &TmuxCoordinates { window_index: 2, pane_index: None });
        assert_eq!(
            cmd,
            "tmux set-option -w -t 'main:2' monitor-activity on; tmux set-option -w -t 'main:2' monitor-silence 30"
        );
    }

    #[test]
    fn install_session_hooks_command_joins_remain_on_exit_and_three_hooks() {
        let cmd = build_install_session_hooks_command("main");
        // トップレベルの4コマンド(remain-on-exit + set-hook x3)の連結であること
        // (`; tmux set-hook`区切りで数える — フックの中身のスクリプト自体にも
        // `tmux `呼び出しが複数出てくるため、単純な"; tmux "分割では数え間違える)。
        assert_eq!(cmd.matches("tmux set-hook -t 'main'").count(), 3);
        assert!(cmd.starts_with("tmux set-option -g remain-on-exit on"));
        assert!(cmd.contains("alert-bell"));
        assert!(cmd.contains("alert-activity"));
        assert!(cmd.contains("alert-silence"));
        assert!(!cmd.contains("pane-died"), "pane-died must not be session-scoped");
    }

    #[test]
    fn install_window_hooks_command_joins_pane_died_and_monitoring() {
        let cmd = build_install_window_hooks_command("main", &TmuxCoordinates { window_index: 5, pane_index: None });
        assert!(cmd.contains("pane-died"));
        assert!(cmd.contains("monitor-activity on"));
        assert!(cmd.contains("monitor-silence 30"));
    }

    // ── install_notify_hooks (フェイクrunner越し、tmux_locator.rsと同じ慣習) ──

    #[tokio::test]
    async fn is_a_noop_when_locator_unknown() {
        let registry = Mutex::new(TmuxLocatorRegistry::new());
        let app_pane = pane("tab-1", "pane-primary");
        let calls = Arc::new(StdMutex::new(Vec::new()));
        let runner = RecordingRunner::new("0\tother\n3\tmy-tag\n", calls.clone());

        install_notify_hooks(&registry, &app_pane, runner).await.unwrap();

        assert!(calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn installs_session_hooks_then_resolves_then_installs_window_hooks() {
        let registry = Mutex::new(TmuxLocatorRegistry::new());
        let app_pane = pane("tab-1", "pane-primary");
        let loc = TmuxLocator { scope: standalone("main"), kind: TmuxTargetKind::Window, tag: TmuxTag("my-tag".to_string()) };
        registry.lock().register(app_pane.clone(), loc, None);
        registry.lock().set_notify_hooks_enabled(&app_pane, true);

        let calls = Arc::new(StdMutex::new(Vec::new()));
        // 1回目のrun_rawの応答は使い捨てられるが(session hooks install、出力は
        // 読み捨て)、2回目のresolve()はlist-windows形式のこの出力を実際にパースする
        // ので、3回とも同じ固定応答を返すこのフェイクの仕様に合わせてある
        // (tmux_locator.rsの`push_ctl_socket_path`テストと同じパターン)。
        let runner = RecordingRunner::new("0\tother\n3\tmy-tag\n", calls.clone());

        install_notify_hooks(&registry, &app_pane, runner).await.unwrap();

        let recorded = calls.lock().unwrap().clone();
        assert_eq!(recorded.len(), 3, "session-hooks install, resolve(list-windows), window-hooks install");
        assert!(recorded[0].contains("remain-on-exit"));
        assert!(recorded[0].contains("alert-bell"));
        assert!(recorded[1].starts_with("tmux list-windows"));
        assert!(recorded[2].contains("pane-died"));
        assert!(recorded[2].contains("main:3"), "window hooks must target the resolved window index");
    }

    #[tokio::test]
    async fn propagates_not_found_when_window_no_longer_exists() {
        let registry = Mutex::new(TmuxLocatorRegistry::new());
        let app_pane = pane("tab-1", "pane-primary");
        let loc = TmuxLocator { scope: standalone("main"), kind: TmuxTargetKind::Window, tag: TmuxTag("missing".to_string()) };
        registry.lock().register(app_pane.clone(), loc, None);
        registry.lock().set_notify_hooks_enabled(&app_pane, true);

        let calls = Arc::new(StdMutex::new(Vec::new()));
        let runner = RecordingRunner::new("0\tother\n", calls.clone());

        let err = install_notify_hooks(&registry, &app_pane, runner).await.unwrap_err();
        assert!(matches!(err, TmuxLocatorError::NotFound(_)));
    }

    #[tokio::test]
    async fn is_a_noop_when_notifications_not_opted_in() {
        // opusレビューM1: ConnectionProfile.enableTabNotificationsが無効な
        // タブでは、ロケータが解決済みでもリモートtmuxサーバーへ一切書き込まない。
        let registry = Mutex::new(TmuxLocatorRegistry::new());
        let app_pane = pane("tab-1", "pane-primary");
        let loc = TmuxLocator { scope: standalone("main"), kind: TmuxTargetKind::Window, tag: TmuxTag("my-tag".to_string()) };
        registry.lock().register(app_pane.clone(), loc, None);
        // set_notify_hooks_enabledを呼ばない(registerの既定はfalse)。

        let calls = Arc::new(StdMutex::new(Vec::new()));
        let runner = RecordingRunner::new("0\tother\n3\tmy-tag\n", calls.clone());

        install_notify_hooks(&registry, &app_pane, runner).await.unwrap();

        assert!(calls.lock().unwrap().is_empty(), "opt-inしていないタブへは何も書き込まないはず");
    }

    #[tokio::test]
    async fn retrying_after_locator_registers_installs_hooks_that_the_first_attempt_missed() {
        // 実機検証(2026-07-27)で判明した本番の実際の順序を再現する回帰テスト:
        // ctl-socket forward確立直後にspawnされる1回目の`install_notify_hooks`呼び出しは、
        // ロケータがまだ登録される前に完走してしまい黙ってno-opになる(`is_a_noop_when_locator_unknown`
        // と同じ状況)。`orchestrator.rs::ensure_tmux_tab_window`はロケータを登録した
        // 直後に`install_notify_hooks`を改めて呼び直すことで、この取りこぼしを回復する。
        let registry = Mutex::new(TmuxLocatorRegistry::new());
        let app_pane = pane("tab-1", "pane-primary");

        // ── ctl-socket forward確立直後、ロケータはまだ未登録 ──
        let early_calls = Arc::new(StdMutex::new(Vec::new()));
        let early_runner = RecordingRunner::new("0\tother\n3\tmy-tag\n", early_calls.clone());
        install_notify_hooks(&registry, &app_pane, early_runner).await.unwrap();
        assert!(early_calls.lock().unwrap().is_empty(), "ロケータ未登録の間は何も書き込まれない");

        // ── orchestrator.rs::ensure_tmux_tab_window相当: ロケータを登録し、
        //    プロファイルのenableTabNotificationsを反映する ──
        let loc = TmuxLocator { scope: standalone("main"), kind: TmuxTargetKind::Window, tag: TmuxTag("my-tag".to_string()) };
        registry.lock().register(app_pane.clone(), loc, None);
        registry.lock().set_notify_hooks_enabled(&app_pane, true);

        // ── ロケータが分かった今すぐ改めて試す ──
        let retry_calls = Arc::new(StdMutex::new(Vec::new()));
        let retry_runner = RecordingRunner::new("0\tother\n3\tmy-tag\n", retry_calls.clone());
        install_notify_hooks(&registry, &app_pane, retry_runner).await.unwrap();

        let recorded = retry_calls.lock().unwrap().clone();
        assert_eq!(recorded.len(), 3, "session-hooks install, resolve(list-windows), window-hooks install");
        assert!(recorded[0].contains("alert-bell"));
        assert!(recorded[2].contains("pane-died"));
    }
}
