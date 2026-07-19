//! タブを開いた際の tmux session group ensure/attach + ウィンドウ create-or-select
//! ロジック(#60)。tmux自体の可変な名前/インデックスに依存しない安定ロケータの
//! データモデル・タグ付けコマンド組み立ては[`crate::tmux_locator`](#62)に、実際に
//! リモートで1回だけコマンドを実行する経路(SSH exec channel)は
//! `transport::ssh_handler`の`RunExec`(#61、`SessionOrchestrator::run_exec`)に
//! それぞれ既に実装済みで、このモジュールはその2つを実際に繋ぎ合わせる
//! (`SessionOrchestrator::ensure_tmux_tab_window`、`orchestrator.rs`参照)。
//!
//! # 全体設計判断
//!
//! `.claude/rules/rust-ssot.md`に従い、「どのtmuxコマンドを、どういう順序で実行するか」
//! という判断は全てここ(Rust側)に置く。Kotlin側は
//! - タブに紐づく安定な識別子(`profile_identity`。今回は`ConnectionProfile.id`の文字列化)
//! - このアプリインストール固有の永続トークン(`client_id`。Kotlin側で1回だけ生成し
//!   `SharedPreferences`へ保存、以後使い回す)
//! - Room に永続化済みのタグがあればそれ(`existing_tag`)
//!
//! の3つをそのまま渡し、戻ってきた`TmuxTabWindowInfo`(`lib.rs`)を見て
//! (a) タブのUI状態に最小限反映(タイトルサフィックス等)しつつ
//! (b) `tag`をRoomへ書き戻す
//! だけでよい。グループ名/セッション名の具体的な文字列や、既存タグが見つからなかった
//! (リモート側でウィンドウごと閉じられた)場合にどう振る舞うか、といった判断は
//! 一切Kotlin側に持ち出さない。
//!
//! # プライマリペインのみを対象にする(split paneはtmux非対応、MVP判断)
//!
//! `TerminalTabsViewModel.kt`の1タブは、プライマリペイン+最大1つのsplitペイン
//! (それぞれ完全に独立したセッション/Rust側接続)を持てる。tmux側のウィンドウ/ペイン
//! ツリーとAndroid側のsplit pane UIを相互ミラーする設計は本タスクの対象外
//! (将来の拡張候補、tmuxのペイン分割まで含めた完全な木構造の同期は別途)——
//! ここではタブの`primaryPane`だけをtmuxウィンドウにマッピングし、split pane側は
//! 純粋にアプリ内だけのUI分割として扱う(tmux側には一切反映しない)。この関数
//! 自体はペインの種別を知らない(呼び出し側=Kotlinがprimary paneについてのみ
//! このAPIを呼ぶことでこの境界を守る)。
//!
//! # session group のネーミング
//!
//! - グループ名: `isekai-<sha256(profile_identity)の先頭16進数16文字>`
//!   ([`session_group_name`])。同じプロファイルからの接続は常に同じグループに
//!   属する(host文字列ではなくprofile identityでハッシュするため、プロファイル編集で
//!   host/usernameを変更してもグループが変わらない——「同じ論理的な接続先」という
//!   ユーザーの意図を優先する判断)。
//! - クライアント(セッション)名: `<group>-<sha256(client_id)の先頭16進数16文字>`
//!   ([`client_session_name`])。`client_id`はアプリインストールごとに一意な永続
//!   トークンなので、同じデバイスからの再接続は常に同じセッション名(≒同じ
//!   「現在のウィンドウ」ポインタ)に戻り、別デバイスは別のグループメンバーになる
//!   (`TmuxSessionScope::GroupMember`が要求する挙動そのもの)。
//!
//! いずれもハッシュ値は16進数のみで構成されるため、tmuxの`session:window.pane`
//! アドレッシング区切り文字(`:`/`.`)やシェルメタ文字と衝突しない。
//!
//! # ensure/attachが1コマンドで済む理由
//!
//! `tmux new-session -A -d -t <group> -s <session-name>`は次の性質を持つ
//! (tmux(1)マニュアル): `-t`(group)が既存セッション名と一致すればそのグループを
//! 使い、一致しなければ新しいグループをこのセッションを最初のメンバーとして作る。
//! さらに`-A`により、`<session-name>`が(グループの内外を問わず)既に存在すれば
//! 新規作成せず`attach-session`相当の動作にフォールバックする(`-d`なので
//! 実際にどこかのクライアントをdetachしたりはしない——このコマンド自体は
//! exec channel越しに実行するのでPTY自体を持たず、実際の対話的attachは別の話)。
//! つまりこの1コマンドだけで「グループが無ければ作る・あれば参加する」と
//! 「このクライアントのセッションが無ければ作る・あれば何もしない」の両方を
//! 冪等に処理できる。

use std::fmt::Write as _;

use sha2::{Digest, Sha256};

use crate::tmux_locator::{
    shell_quote, RemoteTmuxCommandRunner, TmuxCoordinates, TmuxLocator, TmuxLocatorError,
    TmuxLocatorResolver, TmuxSessionScope, TmuxTag, TmuxTargetKind,
};

// ── ネーミング ────────────────────────────────────────────

/// sha256の先頭8バイト(16進数16文字)。tmuxセッション/グループ名として安全な
/// 文字集合(0-9a-f)だけになる。
fn short_hash(input: &str) -> String {
    let digest = Sha256::digest(input.as_bytes());
    let mut out = String::with_capacity(16);
    for byte in digest.iter().take(8) {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// ホスト/プロファイル識別子(呼び出し側=Kotlinが決める安定な文字列、例:
/// `ConnectionProfile.id`の文字列化)から、tmux session groupの名前を決定する。
pub(crate) fn session_group_name(profile_identity: &str) -> String {
    format!("isekai-{}", short_hash(profile_identity))
}

/// このクライアント(アプリインストール)専用の、グループ内で一意なセッション名。
/// 同じ`client_id`からは常に同じ名前が出るため、同じデバイスからの再接続は
/// 同じセッションに戻る。異なるデバイス(別の`client_id`)は別名になるため、
/// 同じグループの別メンバーとして共存できる。
pub(crate) fn client_session_name(group: &str, client_id: &str) -> String {
    format!("{group}-{}", short_hash(client_id))
}

// ── コマンド組み立て ──────────────────────────────────────

/// グループ`group`のセッション集合を保証しつつ、このクライアント専用の
/// セッション名`session_name`でグループに参加する、冪等な1コマンド
/// (モジュールdoc「ensure/attachが1コマンドで済む理由」参照)。
pub(crate) fn build_ensure_group_attached_command(group: &str, session_name: &str) -> String {
    format!(
        "tmux new-session -A -d -t {} -s {}",
        shell_quote(group),
        shell_quote(session_name),
    )
}

/// `session_name`(のグループ)に新しいウィンドウを1つ作り、その`window_index`を
/// 標準出力へ1行で返させる。
pub(crate) fn build_new_window_command(session_name: &str) -> String {
    format!(
        "tmux new-window -t {} -P -F '#{{window_index}}'",
        shell_quote(session_name),
    )
}

/// このクライアントのセッション(`session_name`)の「現在のウィンドウ」を
/// `window_index`に切り替える(`TmuxSessionScope::GroupMember`の各メンバーが
/// 独立した現在ウィンドウを持てる、という前提を実際に使う操作)。
pub(crate) fn build_select_window_command(session_name: &str, window_index: u32) -> String {
    format!(
        "tmux select-window -t {}",
        shell_quote(&format!("{session_name}:{window_index}")),
    )
}

/// [`build_new_window_command`]の出力(`#{window_index}`1行)をパースする。
fn parse_new_window_output(output: &str) -> Option<u32> {
    output.lines().next()?.trim().parse().ok()
}

// ── オーケストレーション ──────────────────────────────────

/// [`ensure_tab_window`]の成功結果。呼び出し側(`orchestrator.rs`)がUniFFI向けの
/// `TmuxTabWindowInfo`(`lib.rs`)へ詰め替える。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TmuxWindowOutcome {
    pub(crate) locator: TmuxLocator,
    pub(crate) coords: TmuxCoordinates,
    /// 今回新規にウィンドウを作成したか(true)、既存タグを解決して再利用したか(false)。
    pub(crate) is_new_window: bool,
}

/// タスク#60本体。`runner`(実運用では`SessionOrchestrator::run_exec`への薄い
/// アダプタ、テストではフェイク)越しに:
/// 1. session group をensure/attachし、
/// 2. `existing_tag`があればそれを解決して(見つかればそのウィンドウを選択、
///    見つからなければ——リモート側で既にkill-windowされた等——新規ウィンドウを
///    作り直す)、無ければ新規ウィンドウを作ってタグを払い出す。
///
/// 戻り値はグループ名・セッション名・[`TmuxWindowOutcome`]の組。
pub(crate) async fn ensure_tab_window<R: RemoteTmuxCommandRunner>(
    runner: R,
    profile_identity: &str,
    client_id: &str,
    existing_tag: Option<String>,
) -> Result<(String, String, TmuxWindowOutcome), TmuxLocatorError> {
    let group = session_group_name(profile_identity);
    let session_name = client_session_name(&group, client_id);
    let scope = TmuxSessionScope::GroupMember { group: group.clone(), session_name: session_name.clone() };
    let resolver = TmuxLocatorResolver::new(runner);

    resolver
        .run_raw(&build_ensure_group_attached_command(&group, &session_name))
        .await
        .map_err(TmuxLocatorError::Command)?;

    let outcome = match existing_tag {
        Some(tag) => {
            let locator = TmuxLocator { scope: scope.clone(), kind: TmuxTargetKind::Window, tag: TmuxTag(tag) };
            match resolver.resolve(&locator).await {
                Ok(coords) => {
                    resolver
                        .run_raw(&build_select_window_command(&session_name, coords.window_index))
                        .await
                        .map_err(TmuxLocatorError::Command)?;
                    TmuxWindowOutcome { locator, coords, is_new_window: false }
                }
                // 前回のタグが見つからない = リモート側で既にウィンドウが閉じられた等。
                // 新規タブと同じ扱いで作り直す(#60スコープ: 「見失った」場合は
                // エラーにせず新しいウィンドウへフォールバックする opportunistic な判断)。
                Err(TmuxLocatorError::NotFound(_)) => create_new_window(&resolver, &scope, &session_name).await?,
                Err(other) => return Err(other),
            }
        }
        None => create_new_window(&resolver, &scope, &session_name).await?,
    };

    Ok((group, session_name, outcome))
}

/// 新しいウィンドウを作り、新規タグを払い出してそのウィンドウへ書き込む
/// (新規タブ、および既存タグ解決に失敗した場合の作り直し、両方から呼ばれる)。
async fn create_new_window<R: RemoteTmuxCommandRunner>(
    resolver: &TmuxLocatorResolver<R>,
    scope: &TmuxSessionScope,
    session_name: &str,
) -> Result<TmuxWindowOutcome, TmuxLocatorError> {
    let output = resolver
        .run_raw(&build_new_window_command(session_name))
        .await
        .map_err(TmuxLocatorError::Command)?;
    let window_index =
        parse_new_window_output(&output).ok_or_else(|| TmuxLocatorError::UnexpectedOutput(output.clone()))?;
    let coords = TmuxCoordinates { window_index, pane_index: None };
    let tag = TmuxTag::new_random();
    resolver.assign_tag(scope, &coords, &tag).await?;
    let locator = TmuxLocator { scope: scope.clone(), kind: TmuxTargetKind::Window, tag };
    Ok(TmuxWindowOutcome { locator, coords, is_new_window: true })
}

// ── テスト ────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tmux_locator::TmuxRunError;
    use std::future::Future;
    use std::sync::{Arc, Mutex};

    /// このリポジトリの慣習(重量なモックフレームワークより実/フェイク実装を好む、
    /// `tmux_locator.rs`のテストと同じ設計)に沿った、コマンドごとに応答を返す
    /// フェイク`RemoteTmuxCommandRunner`。呼ばれた順にコマンドを記録する。
    /// `calls`は`Arc`で共有する——`ensure_tab_window`は`runner`を値で消費する
    /// (内部の`TmuxLocatorResolver`が所有権を持つ)ため、呼び出し後にアサーションで
    /// 読みたい場合は、moveされる前に`Arc`をcloneして手元に残しておく必要がある。
    struct FakeRunner {
        /// 呼ばれた順に消費する応答キュー(空になったら最後の要素を使い回す)。
        responses: Mutex<Vec<Result<String, TmuxRunError>>>,
        calls: Arc<Mutex<Vec<String>>>,
    }

    impl FakeRunner {
        fn new(responses: Vec<Result<String, TmuxRunError>>) -> Self {
            Self { responses: Mutex::new(responses), calls: Arc::new(Mutex::new(Vec::new())) }
        }

        /// `run`が記録する呼び出し履歴を共有する`Arc`のclone。`ensure_tab_window`に
        /// `self`をmoveする前に呼んでおくこと。
        fn calls_handle(&self) -> Arc<Mutex<Vec<String>>> {
            self.calls.clone()
        }
    }

    impl RemoteTmuxCommandRunner for FakeRunner {
        fn run(&self, cmd: &str) -> impl Future<Output = Result<String, TmuxRunError>> + Send {
            self.calls.lock().unwrap().push(cmd.to_string());
            let mut queue = self.responses.lock().unwrap();
            let response = if queue.len() > 1 { queue.remove(0) } else { queue[0].clone() };
            async move { response }
        }
    }

    // ── ネーミング ────────────────────────────────────────

    #[test]
    fn session_group_name_is_deterministic_hex_and_stable_across_calls() {
        let a = session_group_name("profile:42");
        let b = session_group_name("profile:42");
        assert_eq!(a, b);
        assert!(a.starts_with("isekai-"));
        let hex_part = a.strip_prefix("isekai-").unwrap();
        assert_eq!(hex_part.len(), 16);
        assert!(hex_part.bytes().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }

    #[test]
    fn session_group_name_differs_for_different_profiles() {
        assert_ne!(session_group_name("profile:1"), session_group_name("profile:2"));
    }

    #[test]
    fn client_session_name_is_stable_per_client_and_differs_across_clients() {
        let group = session_group_name("profile:42");
        let a = client_session_name(&group, "device-a");
        let a_again = client_session_name(&group, "device-a");
        let b = client_session_name(&group, "device-b");
        assert_eq!(a, a_again, "same device should always resolve to the same session name");
        assert_ne!(a, b, "different devices must get different group members");
        assert!(a.starts_with(&format!("{group}-")));
    }

    #[test]
    fn client_session_name_differs_across_groups_even_for_the_same_client() {
        let g1 = session_group_name("profile:1");
        let g2 = session_group_name("profile:2");
        assert_ne!(client_session_name(&g1, "device-a"), client_session_name(&g2, "device-a"));
    }

    // ── コマンド組み立て ──────────────────────────────────

    #[test]
    fn build_ensure_group_attached_command_uses_new_session_dash_a_dash_d() {
        let cmd = build_ensure_group_attached_command("isekai-abc", "isekai-abc-def");
        assert_eq!(cmd, "tmux new-session -A -d -t 'isekai-abc' -s 'isekai-abc-def'");
    }

    #[test]
    fn build_new_window_command_requests_window_index_output() {
        let cmd = build_new_window_command("isekai-abc-def");
        assert_eq!(cmd, "tmux new-window -t 'isekai-abc-def' -P -F '#{window_index}'");
    }

    #[test]
    fn build_select_window_command_targets_session_colon_window() {
        let cmd = build_select_window_command("isekai-abc-def", 3);
        assert_eq!(cmd, "tmux select-window -t 'isekai-abc-def:3'");
    }

    #[test]
    fn parse_new_window_output_reads_first_line_as_index() {
        assert_eq!(parse_new_window_output("4\n"), Some(4));
        assert_eq!(parse_new_window_output("0"), Some(0));
        assert_eq!(parse_new_window_output(""), None);
        assert_eq!(parse_new_window_output("not-a-number\n"), None);
    }

    // ── ensure_tab_window: 新規タブ ─────────────────────────

    #[tokio::test]
    async fn new_tab_ensures_group_creates_a_window_and_assigns_a_fresh_tag() {
        let runner = FakeRunner::new(vec![
            Ok(String::new()),  // new-session -A -d
            Ok("5\n".to_string()), // new-window -P -F
            Ok(String::new()),  // set-option (assign_tag)
        ]);
        let calls_handle = runner.calls_handle();
        let (group, session_name, outcome) =
            ensure_tab_window(runner, "profile:1", "device-a", None).await.unwrap();

        assert_eq!(group, session_group_name("profile:1"));
        assert_eq!(session_name, client_session_name(&group, "device-a"));
        assert!(outcome.is_new_window);
        assert_eq!(outcome.coords, TmuxCoordinates { window_index: 5, pane_index: None });
        assert_eq!(outcome.locator.scope, TmuxSessionScope::GroupMember { group, session_name });

        let calls = calls_handle.lock().unwrap().clone();
        assert_eq!(calls.len(), 3);
        assert!(calls[0].starts_with("tmux new-session -A -d"));
        assert!(calls[1].starts_with("tmux new-window"));
        assert!(calls[2].starts_with("tmux set-option"));
    }

    // ── ensure_tab_window: 既存タグを持つ再接続タブ ──────────

    #[tokio::test]
    async fn reconnecting_tab_with_existing_tag_selects_the_resolved_window_without_creating_a_new_one() {
        let runner = FakeRunner::new(vec![
            Ok(String::new()), // new-session -A -d
            Ok("0\tother\n2\tmy-tag\n".to_string()), // list-windows (resolve)
            Ok(String::new()), // select-window
        ]);
        let (_, session_name, outcome) =
            ensure_tab_window(runner, "profile:1", "device-a", Some("my-tag".to_string()))
                .await
                .unwrap();

        assert!(!outcome.is_new_window);
        assert_eq!(outcome.coords, TmuxCoordinates { window_index: 2, pane_index: None });
        assert_eq!(outcome.locator.tag, TmuxTag("my-tag".to_string()));
        let _ = session_name;
    }

    #[tokio::test]
    async fn reconnecting_tab_whose_tag_is_no_longer_found_falls_back_to_a_new_window() {
        // リモート側で既にそのウィンドウがkill-windowされていた(list-windows出力に
        // 見当たらない)ケース。新規タブと同じ経路(new-window→assign_tag)へ
        // フォールバックし、エラーにはならないこと。
        let runner = FakeRunner::new(vec![
            Ok(String::new()),        // new-session -A -d
            Ok("0\tother\n".to_string()), // list-windows: タグが見つからない
            Ok("7\n".to_string()),    // new-window -P -F (フォールバック作成)
            Ok(String::new()),        // set-option (assign_tag)
        ]);
        let (_, _, outcome) =
            ensure_tab_window(runner, "profile:1", "device-a", Some("stale-tag".to_string()))
                .await
                .unwrap();

        assert!(outcome.is_new_window);
        assert_eq!(outcome.coords, TmuxCoordinates { window_index: 7, pane_index: None });
        assert_ne!(outcome.locator.tag, TmuxTag("stale-tag".to_string()));
    }

    #[tokio::test]
    async fn ensure_group_attached_command_failure_propagates_and_stops_before_window_logic() {
        let runner = FakeRunner::new(vec![Err(TmuxRunError("ssh exec: not connected".to_string()))]);
        let err = ensure_tab_window(runner, "profile:1", "device-a", None).await.unwrap_err();
        assert_eq!(err, TmuxLocatorError::Command(TmuxRunError("ssh exec: not connected".to_string())));
    }

    #[tokio::test]
    async fn new_window_command_returning_garbage_output_is_reported_as_unexpected_output() {
        let runner = FakeRunner::new(vec![
            Ok(String::new()),                  // new-session -A -d
            Ok("not-a-window-index\n".to_string()), // new-window -P -F (broken)
        ]);
        let err = ensure_tab_window(runner, "profile:1", "device-a", None).await.unwrap_err();
        assert_eq!(err, TmuxLocatorError::UnexpectedOutput("not-a-window-index\n".to_string()));
    }
}
