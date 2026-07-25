//! tmux ウィンドウ/ペインを、tmux自身が持つ可変な名前/インデックスに依存せず
//! 長期的に識別するための「ロケータ」データモデルと、アプリのタブ/ペインID・
//! Epic M（`transport::ctl_streamlocal`）の ctl-socket パスを相互に対応付ける
//! マッピングテーブル(#62)。
//!
//! # なぜ名前/インデックスでは駄目か
//!
//! - tmuxウィンドウの`automatic-rename`は実行中コマンドに応じて名前を勝手に
//!   書き換える。
//! - ウィンドウの*インデックス*も`renumber-windows`や他クライアントの
//!   ウィンドウ開閉で動く。
//!
//! いずれも長期的な識別子として安全ではない。この既知の問題に対する定番の
//! 対策は、tmuxの「ユーザーオプション」(`@isekai_tab_id`のような`@`接頭辞の
//! カスタムオプション)でウィンドウ/ペインにタグを付けることで、これは
//! リネーム/リナンバリングの影響を受けず、`tmux show-options`や
//! `display-message -p '#{@isekai_tab_id}'`（ペインは対応するペイン版）で
//! 読み戻せる。[`TmuxLocator`]はこのタグ自体を識別子として持つ。
//!
//! # session group について
//!
//! 同じtmuxセッションに複数のtmuxクライアントがattachすると、既定では
//! 全クライアントがそのセッションの「現在のウィンドウ」を共有(ミラー)して
//! しまう。「ホストごとに1つのtmuxセッション、タブ=セッション内のウィンドウ」
//! という設計だけでは、複数クライアントがattachしたときにタブごとに独立した
//! 表示を持たせられない——それには追加でtmuxの「session group」
//! (`tmux new-session -t <group-name> -s <per-client-session-name>`)が要る。
//! グループのメンバーはウィンドウ/ペイン集合を共有しつつ、各メンバーは
//! 自分専用の「現在のウィンドウ」を持てる。[`TmuxSessionScope`]はこの
//! 区別（単独セッションか、グループのメンバーか）を表現できるようにしてあるが、
//! 実際にグループを作成/attachする処理自体は別タスク(#60)の範囲であり、
//! ここでは実装しない。
//!
//! # #61（execチャンネル）との関係
//!
//! リモートホスト上で実際にtmuxコマンドを実行する部分(#61で実装される、
//! プールされたSSH接続上のexecチャンネル)はこのタスクの実装時点では
//! まだ存在しない別ワークツリーでの並行作業のため、依存しない。代わりに
//! 最小限のtrait [`RemoteTmuxCommandRunner`]をシームとして定義し、この
//! モジュールのロジック/テストは全てそれ越しに書く。#61が実装され次第、
//! それをこのtraitへの薄いアダプタとして差し込むだけで配線できる想定。
//!
//! # #59（このタスクでの配線）
//!
//! #61がマージされ、上記の想定通り[`transport::ssh_handler::run_exec_on_handle`]への
//! 薄いアダプタ(`SshHandleTmuxRunner`、`transport/ssh_handler.rs`)を通して
//! [`RemoteTmuxCommandRunner`]が実装された。あわせて、Epic Mのctl-socketパスを
//! ロケータと同じtmuxウィンドウ/ペインへ書き込む[`build_set_ctl_socket_command`]/
//! [`TmuxLocatorResolver::push_ctl_socket_path`]と、[`TmuxLocatorRegistry`]を
//! プロセス全体で共有する[`TMUX_LOCATOR_REGISTRY`]を新設し、
//! `transport::ssh_handler::run_ssh_channel_loop`のctl-socket forward確立直後
//! (接続確立時・再接続時の両方)から呼ぶ形で配線した。詳細・判断根拠は
//! `run_ssh_channel_loop`のコメント、および[`TMUX_LOCATOR_REGISTRY`]のdoc参照。
//!
//! 一方、「あるタブに対応するtmuxウィンドウ/ペインをどう見つけて最初に
//! [`TmuxLocator`]/[`TmuxTag`]を割り当てるか」(tmuxセッション名の規約、
//! 複数クライアントの"current window"の曖昧さの解消等)は、モジュール冒頭の
//! 「session group について」で述べた通り#60の範囲であり、このタスクでは
//! 実装しない。このタスクが実装するのは「既に[`TmuxLocatorRegistry`]に
//! ロケータが登録されている(=#60や将来の割り当てロジックが解決済み)前提で、
//! 新しいctl-socketパスをそのロケータへ伝播する」部分に限る。ロケータが
//! 未登録のapp paneでは`push_ctl_socket_to_tmux`は何もしない
//! (opportunistic機能、他のこのcrateの慣習と同じ黙ったフォールバック)。

use std::collections::HashMap;
use std::future::Future;
use std::sync::LazyLock;

use parking_lot::Mutex;

// ── tmuxユーザーオプション名 ──────────────────────────────

/// ウィンドウに付与するタグの user-option 名。
const WINDOW_TAG_OPTION: &str = "@isekai_tab_id";
/// ペインに付与するタグの user-option 名（ウィンドウとは別の名前空間にする
/// —— 同じウィンドウ上の複数ペインがそれぞれ別のタグを持つため）。
const PANE_TAG_OPTION: &str = "@isekai_pane_id";
/// タスク#59: Epic Mのctl-socket(リモートUNIX domain socketパス)を保持する
/// user-option名。`WINDOW_TAG_OPTION`/`PANE_TAG_OPTION`とは異なり、対象は
/// ロケータの`kind`に応じてウィンドウ単位・ペイン単位のどちらにもなり得る
/// (`build_set_ctl_socket_command`参照)。あえてsession-wideの単一オプションに
/// しない理由: tmuxのオプション解決順はペイン→ウィンドウ→セッション→グローバルの
/// 継承なので、ウィンドウ/ペイン単位で設定しておけば、各タブ(=各ウィンドウ/ペイン)が
/// コマンド実行時に自分自身のスコープの値を自然に解決できる
/// (session-wideだと複数タブが同じ値を奪い合う"last write wins"になってしまう)。
const CTL_SOCK_OPTION: &str = "@isekai_ctl_sock";

// ── ロケータのコア型 ──────────────────────────────────────

/// `@isekai_tab_id`/`@isekai_pane_id` として書き込む実際のタグ値。128bitの
/// ランダムトークンとして発行する(`transport::ctl_streamlocal::new_ctl_socket_path`
/// と同じ「衝突耐性のある乱数トークン」パターン)ので、tmux側のリネーム/
/// リナンバリングは元より、他ホスト/他セッションのタグとも衝突しない。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct TmuxTag(pub(crate) String);

impl TmuxTag {
    /// 新規のタグ値を発行する。ウィンドウ/ペインを初めて認識した時に1回だけ
    /// 呼び、以後はその値をtmux user-optionとして書き込み・読み戻す。
    pub(crate) fn new_random() -> Self {
        use rand::RngCore as _;
        use std::fmt::Write as _;
        let mut bytes = [0u8; 16];
        rand::thread_rng().fill_bytes(&mut bytes);
        let mut token = String::with_capacity(bytes.len() * 2);
        for byte in bytes {
            let _ = write!(token, "{byte:02x}");
        }
        Self(token)
    }
}

/// ロケータがウィンドウ単位かペイン単位か。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum TmuxTargetKind {
    Window,
    Pane,
}

impl TmuxTargetKind {
    fn user_option(self) -> &'static str {
        match self {
            Self::Window => WINDOW_TAG_OPTION,
            Self::Pane => PANE_TAG_OPTION,
        }
    }
}

/// このロケータが指すウィンドウ/ペインが属するtmuxセッション(またはセッション
/// グループ)の識別。「1セッション名だけをハードコードする」設計を避け、
/// session groupの存在(上記モジュールdoc参照)を後から配線できるようにする。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum TmuxSessionScope {
    /// group化されていない、ただ1つのtmuxセッション。
    Standalone { session_name: String },
    /// tmux session groupのメンバー。`group`はグループ内の全メンバーが共有する
    /// ウィンドウ/ペイン集合の識別(グループ名)、`session_name`はこの
    /// クライアント自身がattachに使った、グループ内で一意なセッション名。
    /// ウィンドウ/ペインはグループ全体で共有されるため、`-t`アドレッシングには
    /// グループ内のどのメンバーの`session_name`を使っても同じ対象に届く
    /// ——ここでは常にこのロケータを作った側のセッション名を保持する。
    GroupMember { group: String, session_name: String },
}

impl TmuxSessionScope {
    /// `tmux ... -t <this>`にそのまま使えるセッション名。tmuxの`-t`は
    /// 「グループ」という概念を直接は取らないため、group memberの場合も
    /// 具体的な`session_name`を返す(上記doc参照——グループ内のどのメンバー名を
    /// 使っても共有ウィンドウ/ペインには同じように届く)。
    pub(crate) fn addressable_session_name(&self) -> &str {
        match self {
            Self::Standalone { session_name } => session_name,
            Self::GroupMember { session_name, .. } => session_name,
        }
    }
}

/// tmuxの可変な名前/インデックスに依存しない、ウィンドウ/ペインの安定した
/// 識別子。実体は[`TmuxTag`](user-optionタグ)であり、`scope`/`kind`は
/// それをどう問い合わせる/書き込むかのコンテキストを持たせているだけ
/// (タグ自体が既にグローバルに一意なランダム値なので、識別性そのものは
/// `tag`だけで成り立つ)。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct TmuxLocator {
    pub(crate) scope: TmuxSessionScope,
    pub(crate) kind: TmuxTargetKind,
    pub(crate) tag: TmuxTag,
}

/// ある時点で[`TmuxLocator`]を解決して得られる、tmux自身の座標のスナップ
/// ショット。ウィンドウ/ペインインデックスは(モジュールdoc冒頭の通り)揮発性
/// であり、呼び出しごとに変わり得る値として扱う——長期的に保持・比較すべき
/// なのは常に[`TmuxLocator`]の方。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TmuxCoordinates {
    pub(crate) window_index: u32,
    /// ウィンドウ単位ロケータの解決結果では`None`、ペイン単位では`Some`。
    pub(crate) pane_index: Option<u32>,
}

// ── #61向けのシーム: リモートtmuxコマンド実行 ─────────────

/// リモートホスト上で1つのtmuxコマンドライン(`cmd`、例:
/// `tmux list-windows -t foo -F '...'`)を実行し、その標準出力を返す。
///
/// 本番実装は#61（プールされたSSH接続上のexecチャンネル）が提供する想定だが、
/// #61は本タスクと並行して別ワークツリーで実装中でまだ存在しないため、ここでは
/// このtrait自体をシームとして定義し、テストはフェイク実装に対して書く。
/// #61がマージされたら、実際のexecチャンネルを薄くラップするアダプタを
/// このtraitに対して実装するだけで配線できる。
///
/// 内部専用traitはnative async-fn-in-trait(RPITIT)で済ませるこのcrateの慣習
/// (`rebind_ports.rs`/`resume_client.rs`参照)に合わせ、`async-trait`マクロは
/// 使わない。
pub(crate) trait RemoteTmuxCommandRunner: Send + Sync {
    fn run(&self, cmd: &str) -> impl Future<Output = Result<String, TmuxRunError>> + Send;
}

/// [`RemoteTmuxCommandRunner::run`]の失敗(非ゼロ終了・接続断など、実際の
/// 分類は#61の実装に委ねる)。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub(crate) struct TmuxRunError(pub(crate) String);

/// [`TmuxLocatorResolver::resolve`]/[`TmuxLocatorResolver::assign_tag`]の失敗。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum TmuxLocatorError {
    #[error(transparent)]
    Command(#[from] TmuxRunError),
    /// コマンド自体は成功したが、出力の中にこのタグを持つウィンドウ/ペインが
    /// 見つからなかった(tmux側でkill-window/kill-pane済み、等)。
    #[error("tmux locator tag {0:?} was not found in the remote tmux state")]
    NotFound(TmuxTag),
    /// タスク#60: コマンド自体は成功したが、期待した形式の出力が得られなかった
    /// (例: `tmux new-window -P -F '#{window_index}'`が数値以外を返した)。
    #[error("unexpected tmux output: {0:?}")]
    UnexpectedOutput(String),
}

/// シェル引数として安全に埋め込むための最小限のシングルクォート化。
/// セッション名にシングルクォートやスペースが含まれる可能性は低いが、
/// `format!`でそのまま埋め込むよりは安全にしておく。`tmux_scrollback`
/// (#58: tmux capture-paneベースのscrollback backfill)と`tmux_session.rs`
/// (#60: session group/ウィンドウ操作コマンドの組み立て)がどちらも同じペイン
/// アドレッシング規約(`build_set_tag_command`と同じ`session:window.pane`
/// 形式)を再利用するため`pub(crate)`にしてある。
pub(crate) fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

/// `locator.scope`配下の全ウィンドウ(またはペイン、`kind`次第)を列挙し、
/// それぞれのタグ値を一緒に出力させるtmuxコマンドを組み立てる。
///
/// ペインの列挙には`-a`ではなく`-s`を使う —— tmuxの`list-panes -a`は
/// 「サーバー上の全セッションの全ペイン」を`-t`を無視して返すのに対し、
/// `-s`は「`-t`で指定したセッション内の全ペイン(全ウィンドウ横断)」を返す。
/// ここで欲しいのは後者(このセッション/グループのスコープ内)。
pub(crate) fn build_list_command(scope: &TmuxSessionScope, kind: TmuxTargetKind) -> String {
    let session = shell_quote(scope.addressable_session_name());
    match kind {
        TmuxTargetKind::Window => format!(
            "tmux list-windows -t {session} -F '#{{window_index}}\t#{{{}}}'",
            WINDOW_TAG_OPTION
        ),
        TmuxTargetKind::Pane => format!(
            "tmux list-panes -s -t {session} -F '#{{window_index}}\t#{{pane_index}}\t#{{{}}}'",
            PANE_TAG_OPTION
        ),
    }
}

/// [`build_list_command`]の出力(`\t`区切りの行、`kind`に応じて2列/3列)を
/// パースし、末尾列が`tag`と一致する最初の行の座標を返す。一致が無ければ
/// `None`。
pub(crate) fn parse_list_output(kind: TmuxTargetKind, output: &str, tag: &TmuxTag) -> Option<TmuxCoordinates> {
    for line in output.lines() {
        let fields: Vec<&str> = line.split('\t').collect();
        let coords = match (kind, fields.as_slice()) {
            (TmuxTargetKind::Window, [window_index, line_tag]) if *line_tag == tag.0 => {
                window_index.parse().ok().map(|window_index| TmuxCoordinates { window_index, pane_index: None })
            }
            (TmuxTargetKind::Pane, [window_index, pane_index, line_tag]) if *line_tag == tag.0 => {
                match (window_index.parse(), pane_index.parse()) {
                    (Ok(window_index), Ok(pane_index)) => {
                        Some(TmuxCoordinates { window_index, pane_index: Some(pane_index) })
                    }
                    _ => None,
                }
            }
            _ => None,
        };
        if let Some(coords) = coords {
            return Some(coords);
        }
    }
    None
}

/// `coords`が指すウィンドウ/ペインに`tag`をuser-optionとして書き込む
/// (`set-option`)コマンドを組み立てる。`coords.pane_index`の有無で
/// ウィンドウ/ペインどちらを対象にするか、どちらのuser-option名を使うかが
/// 決まる(この2つを別々の引数にして矛盾した組み合わせ(例: `Pane` kindなのに
/// `pane_index: None`)を型として表現できてしまうのを避けている)。
pub(crate) fn build_set_tag_command(scope: &TmuxSessionScope, coords: &TmuxCoordinates, tag: &TmuxTag) -> String {
    let session = scope.addressable_session_name();
    // `-w`/`-p`必須: 付けないと`set-option`はセッションスコープの値を書いてしまい、
    // (コードレビューで実tmuxに対し検証済み)そのセッション内の全ウィンドウ/ペインが
    // 同じ値をinherit経由で返すようになる——ウィンドウ/ペインごとに別々のタグを
    // 持たせるというこの関数の目的そのものが壊れる。
    let (scope_flag, target, option) = match coords.pane_index {
        Some(pane_index) => ("-p", format!("{session}:{}.{pane_index}", coords.window_index), PANE_TAG_OPTION),
        None => ("-w", format!("{session}:{}", coords.window_index), WINDOW_TAG_OPTION),
    };
    format!("tmux set-option {scope_flag} -t {} {option} {}", shell_quote(&target), shell_quote(&tag.0))
}

/// タスク#59: `coords`が指すウィンドウ/ペインに、Epic Mのctl-socketパス
/// (`ctl_socket_path`)を[`CTL_SOCK_OPTION`]として書き込む(`set-option`)コマンドを
/// 組み立てる。[`build_set_tag_command`]と同じ「`coords.pane_index`の有無で対象と
/// user-option名が決まる」形だが、書き込む値がタグ(固定的な識別子)ではなく
/// 接続のたびに変わり得るctl-socketパスである点が異なる(再接続のたびにこの関数を
/// 呼び直して値を更新する)。
pub(crate) fn build_set_ctl_socket_command(
    scope: &TmuxSessionScope,
    coords: &TmuxCoordinates,
    ctl_socket_path: &str,
) -> String {
    let session = scope.addressable_session_name();
    // `-w`/`-p`必須: [`build_set_tag_command`]と同じ理由(コードレビューで実tmuxに
    // 対し検証済み)。無いとセッションスコープになり、タブごとに別のctl-socket
    // パスを持たせるという本来の目的(このモジュールdoc冒頭参照)が壊れて
    // 「最後に書いたタブの値が全タブに漏れる」状態になる。
    let (scope_flag, target) = match coords.pane_index {
        Some(pane_index) => ("-p", format!("{session}:{}.{pane_index}", coords.window_index)),
        None => ("-w", format!("{session}:{}", coords.window_index)),
    };
    format!("tmux set-option {scope_flag} -t {} {CTL_SOCK_OPTION} {}", shell_quote(&target), shell_quote(ctl_socket_path))
}

/// [`RemoteTmuxCommandRunner`]越しに実際の問い合わせ/タグ付けを行う薄いラッパー。
/// コマンド文字列の組み立て・出力のパースは全て上記の自由関数(単体テスト
/// しやすい純粋関数)に委ねており、ここでは「runnerを呼んで結果を解釈する」
/// だけを行う。
pub(crate) struct TmuxLocatorResolver<R: RemoteTmuxCommandRunner> {
    runner: R,
}

impl<R: RemoteTmuxCommandRunner> TmuxLocatorResolver<R> {
    pub(crate) fn new(runner: R) -> Self {
        Self { runner }
    }

    /// `locator`を、tmux上の現在の座標(ウィンドウ/ペインインデックス)へ解決する。
    pub(crate) async fn resolve(&self, locator: &TmuxLocator) -> Result<TmuxCoordinates, TmuxLocatorError> {
        let cmd = build_list_command(&locator.scope, locator.kind);
        let output = self.runner.run(&cmd).await?;
        parse_list_output(locator.kind, &output, &locator.tag)
            .ok_or_else(|| TmuxLocatorError::NotFound(locator.tag.clone()))
    }

    /// `scope`配下の`coords`が指すウィンドウ/ペインへ`tag`を新規に書き込む
    /// (ウィンドウ/ペインを初めて認識した時に呼ぶ想定)。
    pub(crate) async fn assign_tag(
        &self,
        scope: &TmuxSessionScope,
        coords: &TmuxCoordinates,
        tag: &TmuxTag,
    ) -> Result<(), TmuxLocatorError> {
        let cmd = build_set_tag_command(scope, coords, tag);
        self.runner.run(&cmd).await?;
        Ok(())
    }

    /// タスク#59: `locator`を現在の座標へ解決し(=ウィンドウ/ペインの
    /// リネーム・リナンバリングを経ていても正しい対象を突き止め)、そこへ
    /// Epic Mのctl-socketパスを[`CTL_SOCK_OPTION`]として書き込む。接続確立時・
    /// 再接続時のいずれも(ctl-socketパスは接続のたびに変わるため)このメソッドを
    /// 呼び直す想定(呼び出し側は`transport::ssh_handler::run_ssh_channel_loop`)。
    /// `locator`が指すウィンドウ/ペインが既に無くなっている場合
    /// (`TmuxLocatorError::NotFound`)は、呼び出し側がbest-effortとして
    /// ログするだけで接続自体は継続してよい(opportunistic機能)。
    pub(crate) async fn push_ctl_socket_path(
        &self,
        locator: &TmuxLocator,
        ctl_socket_path: &str,
    ) -> Result<(), TmuxLocatorError> {
        let coords = self.resolve(locator).await?;
        let cmd = build_set_ctl_socket_command(&locator.scope, &coords, ctl_socket_path);
        self.runner.run(&cmd).await?;
        Ok(())
    }

    /// タスク#60: session group ensure/attach・`new-window`・`select-window`など、
    /// ロケータの解決/タグ付け以外のtmuxコマンドをこの`resolver`が包む`runner`越しに
    /// そのまま実行する(`tmux_session.rs`参照)。コマンド文字列の組み立て・出力の
    /// パースは呼び出し側(`tmux_session.rs`)の自由関数に委ね、ここは薄いパススルーに
    /// 留める(`resolve`/`assign_tag`と同じ「runnerを呼ぶだけ」の役割分担)。
    pub(crate) async fn run_raw(&self, cmd: &str) -> Result<String, TmuxRunError> {
        self.runner.run(cmd).await
    }
}

// ── アプリのタブ/ペインID ⟷ tmuxロケータ ⟷ ctl-socketパスの対応表 ──

/// `TerminalTabsViewModel.kt`の`PaneAddress(tabId, paneId)`をそのままRust側に
/// 写した、アプリ自身のタブ内ペインの識別子。各タブは現状プライマリペイン+
/// 最大1つのsplitペインを持ち、それぞれ独立したsession/orchestratorを持つ
/// (バックグラウンド参照)ため、対応表のキーは(tabId, paneId)のペア単位にする。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct AppPaneId {
    pub(crate) tab_id: String,
    pub(crate) pane_id: String,
}

impl AppPaneId {
    /// タスク#59: Kotlin側の実`PaneAddress(tabId, paneId)`はまだUniFFI境界を越えて
    /// Rust側へ渡ってきていない(`SshConfig`等どの`connect_*`引数にも(tab_id, pane_id)
    /// 相当のフィールドが無い)。とはいえ[`TmuxLocatorRegistry`]は「再接続をまたいで
    /// 同じタブを同じキーで指せる」ことだけが要件で、Kotlin側の値と一致している必要は
    /// 無いため、`orchestrator::create_session_orchestrator`が`OrchestratorShared`
    /// 生成時に1回だけ発行する内部限定の安定トークンで代用する(同一`OrchestratorShared`
    /// ＝同一タブの生存期間中は再接続のたびに再生成されない、`orchestrator.rs`参照)。
    /// 実際の`PaneAddress`がFFI経由で渡ってくるようになったら、生成元をそちらに
    /// 差し替えるだけで済む(登録/検索ロジックは`AppPaneId`というキー型にしか
    /// 依存していない)。
    pub(crate) fn generate_process_local() -> Self {
        let token = TmuxTag::new_random().0;
        Self { tab_id: token, pane_id: String::new() }
    }
}

/// 1つのアプリペインについて、対応表が保持する情報一式。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TmuxTabEntry {
    pub(crate) app_pane: AppPaneId,
    pub(crate) locator: TmuxLocator,
    /// このタブが現在使っているEpic M ctl-socketのリモートパス
    /// (`transport::ctl_streamlocal::new_ctl_socket_path`が発行する、
    /// 接続のたびに変わるランダムパス)。まだ確立していなければ`None`。
    pub(crate) ctl_socket_path: Option<String>,
}

/// アプリのタブ/ペインID ⟷ tmuxロケータ ⟷ ctl-socketパスを相互に対応付ける
/// マッピングテーブル。
///
/// `OrchestratorState`同様、これ自身は内部で同期を取らない素のデータ構造
/// として設計してある(`rebind_manager`的な「純粋な状態」の慣習)——
/// 複数スレッドから共有する場合は呼び出し側が`Mutex`(または
/// `parking_lot::Mutex`)で包む。
#[derive(Debug, Default)]
pub(crate) struct TmuxLocatorRegistry {
    by_app_pane: HashMap<AppPaneId, TmuxTabEntry>,
    /// 逆引き用の索引。`register`/`unregister`が`by_app_pane`と同時に
    /// 整合を保つ(下記参照)。
    by_locator: HashMap<TmuxLocator, AppPaneId>,
}

impl TmuxLocatorRegistry {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// `app_pane`のタブが接続/再接続した際に、その対応表エントリを登録/更新
    /// する。既に`app_pane`のエントリが存在していれば(再接続で別のtmux
    /// ウィンドウ/ペインに繋ぎ直った場合を含め)完全に上書きし、古い
    /// `locator`の逆引きエントリは`by_locator`から確実に取り除く
    /// (取り除かないと、再接続後にその古いロケータで`app_pane_for`を
    /// 引いた際、既にこのペインのものではなくなった古い対応を返してしまう)。
    pub(crate) fn register(&mut self, app_pane: AppPaneId, locator: TmuxLocator, ctl_socket_path: Option<String>) {
        if let Some(old) = self.by_app_pane.get(&app_pane) {
            self.by_locator.remove(&old.locator);
        }
        self.by_locator.insert(locator.clone(), app_pane.clone());
        self.by_app_pane.insert(app_pane.clone(), TmuxTabEntry { app_pane, locator, ctl_socket_path });
    }

    /// `app_pane`に対応するtmuxロケータを引く。
    pub(crate) fn locator_for(&self, app_pane: &AppPaneId) -> Option<&TmuxLocator> {
        self.by_app_pane.get(app_pane).map(|e| &e.locator)
    }

    /// `locator`に対応するアプリのタブ/ペインIDを引く(逆引き)。
    pub(crate) fn app_pane_for(&self, locator: &TmuxLocator) -> Option<&AppPaneId> {
        self.by_locator.get(locator)
    }

    /// `app_pane`が現在使っているctl-socketパスを引く。
    pub(crate) fn ctl_socket_path_for(&self, app_pane: &AppPaneId) -> Option<&str> {
        self.by_app_pane.get(app_pane).and_then(|e| e.ctl_socket_path.as_deref())
    }

    /// 既に登録済みの`app_pane`について、ctl-socketパスだけを更新する
    /// (Epic Mは再接続のたびに新しいランダムパスを発行するため、
    /// ロケータ自体は変わらずctl-socketパスだけ更新したいケース向け)。
    /// `app_pane`が未登録なら何もせず`false`を返す。
    pub(crate) fn set_ctl_socket_path(&mut self, app_pane: &AppPaneId, ctl_socket_path: Option<String>) -> bool {
        match self.by_app_pane.get_mut(app_pane) {
            Some(entry) => {
                entry.ctl_socket_path = ctl_socket_path;
                true
            }
            None => false,
        }
    }

    /// `app_pane`のエントリを対応表から取り除く(タブが閉じられた際など)。
    pub(crate) fn unregister(&mut self, app_pane: &AppPaneId) -> Option<TmuxTabEntry> {
        let entry = self.by_app_pane.remove(app_pane)?;
        self.by_locator.remove(&entry.locator);
        Some(entry)
    }
}

// ── タスク#59: プロセス全体で共有するレジストリ + ctl-socketパスの伝播 ──

/// [`TmuxLocatorRegistry`]のプロセス全体シングルトン。
///
/// **配線先としてここ(`tmux_locator.rs`)を選んだ理由**(#62の作者が「どこから
/// 配線するかは判断が分かれる」と明示的に開けていた点への回答): 一見
/// `pool.rs`の`SSH_POOL`(同種の「`LazyLock<Mutex<HashMap<_,_>>>`なプロセス
/// 全体シングルトン」という前例)の隣に置くのが自然に見えるが、`pool.rs`
/// 自身のモジュールdocが「ここに置くのはtransport非依存の汎用プリミティブと
/// **プレーンSSH固有**の`SshPoolKey`/`SSH_POOL`」と明記してスコープを限定して
/// いる。tmuxロケータはSSH固有ではなく(`transport::ssh_handler::run_ssh_channel_loop`
/// はTCP・QUICネスト共通のコードであり、Epic Mのctl-socket forward自体も
/// transport非依存)、かつ`TmuxLocatorRegistry`という型自体がこのモジュールに
/// 属するため、型と状態を同じ場所に置くことを優先した。
///
/// なぜ`OrchestratorShared`(タブごとのインスタンス)ではなくプロセス全体の
/// シングルトンにするか: `TmuxLocatorRegistry`は本来「アプリ全体でどのタブが
/// どのtmuxウィンドウ/ペインを使っているか」を横断的に把握するための対応表
/// (逆引き`app_pane_for`が典型的にそう)であり、1タブに閉じた状態ではない。
/// また`SSH_POOL`と同様、キー(`AppPaneId`)自体が既にタブを一意に識別するため、
/// 複数タブが同じマップを共有しても衝突しない。
pub(crate) static TMUX_LOCATOR_REGISTRY: LazyLock<Mutex<TmuxLocatorRegistry>> =
    LazyLock::new(|| Mutex::new(TmuxLocatorRegistry::new()));

/// タスク#59: ctl-socketパスが確立された(接続確立時・再接続時のいずれも)たびに
/// 呼ぶ。`app_pane_id`に対応するtmuxロケータが既に[`TmuxLocatorRegistry`]に
/// 登録されていれば、そのロケータが指す同じtmuxウィンドウ/ペインへ
/// [`TmuxLocatorResolver::push_ctl_socket_path`]で新しいctl-socketパスを書き込む。
///
/// ロケータがまだ登録されていない場合(このapp paneのtmuxウィンドウ/ペインが
/// まだ判明していない——モジュール冒頭の「#59（このタスクでの配線）」参照)は
/// 何もしない。どちらの場合も、レジストリの`ctl_socket_path`は`register`済みの
/// エントリに対してのみ更新される(`set_ctl_socket_path`が未登録のapp paneに対して
/// no-opであることに委ねている)ため、ロケータが無いタブについては呼んでも
/// 副作用が無い。
///
/// tmux側への書き込みが失敗した場合(ウィンドウ/ペインが既に閉じられている等)は
/// `Err`を返す — 呼び出し側(`run_ssh_channel_loop`)はこれをbest-effortとして
/// ログするだけで、接続そのものは継続してよい(Epic M自体がopportunisticな機能)。
pub(crate) async fn push_ctl_socket_to_tmux<R: RemoteTmuxCommandRunner>(
    registry: &Mutex<TmuxLocatorRegistry>,
    app_pane_id: &AppPaneId,
    ctl_socket_path: &str,
    runner: R,
) -> Result<(), TmuxLocatorError> {
    let locator = registry.lock().locator_for(app_pane_id).cloned();
    let result = match &locator {
        Some(locator) => {
            let resolver = TmuxLocatorResolver::new(runner);
            resolver.push_ctl_socket_path(locator, ctl_socket_path).await
        }
        None => Ok(()),
    };
    registry.lock().set_ctl_socket_path(app_pane_id, Some(ctl_socket_path.to_string()));
    result
}

// ── テスト ────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // ── フェイクの RemoteTmuxCommandRunner ──────────────
    // このリポジトリの慣習(重量なモックフレームワークより実/フェイク実装を
    // 好む)に沿って、呼ばれたコマンドを記録しつつ固定の応答を返すだけの
    // 最小限のフェイクにする。

    struct FakeRunner {
        response: Result<String, TmuxRunError>,
        calls: Mutex<Vec<String>>,
    }

    impl FakeRunner {
        fn ok(output: &str) -> Self {
            Self { response: Ok(output.to_string()), calls: Mutex::new(Vec::new()) }
        }
        fn err(msg: &str) -> Self {
            Self { response: Err(TmuxRunError(msg.to_string())), calls: Mutex::new(Vec::new()) }
        }
        fn last_call(&self) -> String {
            self.calls.lock().unwrap().last().expect("no command was run").clone()
        }
    }

    impl RemoteTmuxCommandRunner for FakeRunner {
        fn run(&self, cmd: &str) -> impl Future<Output = Result<String, TmuxRunError>> + Send {
            self.calls.lock().unwrap().push(cmd.to_string());
            let response = self.response.clone();
            async move { response }
        }
    }

    fn standalone(name: &str) -> TmuxSessionScope {
        TmuxSessionScope::Standalone { session_name: name.to_string() }
    }

    fn group(group_name: &str, session_name: &str) -> TmuxSessionScope {
        TmuxSessionScope::GroupMember { group: group_name.to_string(), session_name: session_name.to_string() }
    }

    // ── build_list_command / parse_list_output ──────────

    #[test]
    fn build_list_command_window_uses_list_windows() {
        let cmd = build_list_command(&standalone("main"), TmuxTargetKind::Window);
        assert_eq!(cmd, "tmux list-windows -t 'main' -F '#{window_index}\t#{@isekai_tab_id}'");
    }

    #[test]
    fn build_list_command_pane_uses_list_panes_with_dash_s_not_dash_a() {
        let cmd = build_list_command(&standalone("main"), TmuxTargetKind::Pane);
        assert_eq!(
            cmd,
            "tmux list-panes -s -t 'main' -F '#{window_index}\t#{pane_index}\t#{@isekai_pane_id}'"
        );
    }

    #[test]
    fn build_list_command_group_member_addresses_by_its_own_session_name() {
        let cmd = build_list_command(&group("hosts-foo", "client-a"), TmuxTargetKind::Window);
        assert!(cmd.contains("-t 'client-a'"));
    }

    #[test]
    fn parse_list_output_finds_matching_window() {
        let tag = TmuxTag("abc123".to_string());
        let output = "0\tother\n1\tabc123\n2\t\n";
        let coords = parse_list_output(TmuxTargetKind::Window, output, &tag).unwrap();
        assert_eq!(coords, TmuxCoordinates { window_index: 1, pane_index: None });
    }

    #[test]
    fn parse_list_output_finds_matching_pane() {
        let tag = TmuxTag("pane-tag".to_string());
        let output = "0\t0\t\n0\t1\tpane-tag\n1\t0\t\n";
        let coords = parse_list_output(TmuxTargetKind::Pane, output, &tag).unwrap();
        assert_eq!(coords, TmuxCoordinates { window_index: 0, pane_index: Some(1) });
    }

    #[test]
    fn parse_list_output_returns_none_when_tag_absent() {
        let tag = TmuxTag("missing".to_string());
        let output = "0\tother\n1\t\n";
        assert_eq!(parse_list_output(TmuxTargetKind::Window, output, &tag), None);
    }

    #[test]
    fn parse_list_output_returns_none_on_empty_output() {
        let tag = TmuxTag("anything".to_string());
        assert_eq!(parse_list_output(TmuxTargetKind::Window, "", &tag), None);
    }

    #[test]
    fn parse_list_output_ignores_malformed_lines() {
        let tag = TmuxTag("t".to_string());
        // ウィンドウ問い合わせにペイン形式(3列)の行が紛れ込んでも無視し、
        // パニックしない。
        let output = "0\t1\tt\n1\tt\n";
        let coords = parse_list_output(TmuxTargetKind::Window, output, &tag).unwrap();
        assert_eq!(coords, TmuxCoordinates { window_index: 1, pane_index: None });
    }

    // ── build_set_tag_command ────────────────────────────

    #[test]
    fn build_set_tag_command_window_targets_session_colon_window() {
        let cmd = build_set_tag_command(
            &standalone("main"),
            &TmuxCoordinates { window_index: 2, pane_index: None },
            &TmuxTag("tagval".to_string()),
        );
        assert_eq!(cmd, "tmux set-option -w -t 'main:2' @isekai_tab_id 'tagval'");
    }

    #[test]
    fn build_set_tag_command_pane_targets_session_colon_window_dot_pane() {
        let cmd = build_set_tag_command(
            &standalone("main"),
            &TmuxCoordinates { window_index: 2, pane_index: Some(1) },
            &TmuxTag("tagval".to_string()),
        );
        assert_eq!(cmd, "tmux set-option -p -t 'main:2.1' @isekai_pane_id 'tagval'");
    }

    // ── TmuxLocatorResolver (フェイクrunner越し) ─────────

    #[tokio::test]
    async fn resolve_returns_coordinates_when_tag_found() {
        let runner = FakeRunner::ok("0\tother\n3\tmy-tag\n");
        let resolver = TmuxLocatorResolver::new(runner);
        let locator = TmuxLocator {
            scope: standalone("main"),
            kind: TmuxTargetKind::Window,
            tag: TmuxTag("my-tag".to_string()),
        };
        let coords = resolver.resolve(&locator).await.unwrap();
        assert_eq!(coords, TmuxCoordinates { window_index: 3, pane_index: None });
        assert!(resolver.runner.last_call().starts_with("tmux list-windows"));
    }

    #[tokio::test]
    async fn resolve_returns_not_found_when_tag_missing() {
        let runner = FakeRunner::ok("0\tother\n");
        let resolver = TmuxLocatorResolver::new(runner);
        let locator = TmuxLocator {
            scope: standalone("main"),
            kind: TmuxTargetKind::Window,
            tag: TmuxTag("missing-tag".to_string()),
        };
        let err = resolver.resolve(&locator).await.unwrap_err();
        assert_eq!(err, TmuxLocatorError::NotFound(TmuxTag("missing-tag".to_string())));
    }

    #[tokio::test]
    async fn resolve_propagates_command_error() {
        let runner = FakeRunner::err("ssh channel closed");
        let resolver = TmuxLocatorResolver::new(runner);
        let locator = TmuxLocator {
            scope: standalone("main"),
            kind: TmuxTargetKind::Window,
            tag: TmuxTag("t".to_string()),
        };
        let err = resolver.resolve(&locator).await.unwrap_err();
        assert_eq!(err, TmuxLocatorError::Command(TmuxRunError("ssh channel closed".to_string())));
    }

    #[tokio::test]
    async fn assign_tag_runs_the_expected_set_option_command() {
        let runner = FakeRunner::ok("");
        let resolver = TmuxLocatorResolver::new(runner);
        resolver
            .assign_tag(
                &standalone("main"),
                &TmuxCoordinates { window_index: 0, pane_index: None },
                &TmuxTag("fresh-tag".to_string()),
            )
            .await
            .unwrap();
        assert_eq!(resolver.runner.last_call(), "tmux set-option -w -t 'main:0' @isekai_tab_id 'fresh-tag'");
    }

    // ── build_set_ctl_socket_command (タスク#59) ─────────

    #[test]
    fn build_set_ctl_socket_command_window_targets_session_colon_window() {
        let cmd = build_set_ctl_socket_command(
            &standalone("main"),
            &TmuxCoordinates { window_index: 2, pane_index: None },
            "/tmp/isekai-pipe-ctl-abc.sock",
        );
        assert_eq!(cmd, "tmux set-option -w -t 'main:2' @isekai_ctl_sock '/tmp/isekai-pipe-ctl-abc.sock'");
    }

    #[test]
    fn build_set_ctl_socket_command_pane_targets_session_colon_window_dot_pane() {
        let cmd = build_set_ctl_socket_command(
            &standalone("main"),
            &TmuxCoordinates { window_index: 2, pane_index: Some(1) },
            "/tmp/isekai-pipe-ctl-abc.sock",
        );
        assert_eq!(cmd, "tmux set-option -p -t 'main:2.1' @isekai_ctl_sock '/tmp/isekai-pipe-ctl-abc.sock'");
    }

    // ── TmuxLocatorResolver::push_ctl_socket_path (タスク#59) ────

    #[tokio::test]
    async fn push_ctl_socket_path_resolves_then_dispatches_set_option() {
        // resolve()はlist-windows形式の出力をパースしてtagを探す。ここで返す固定
        // 応答はset-optionコマンド自体の実行(直後に発行される、応答は読み捨て)にも
        // 使い回されるが、その値自体は無視されるので問題ない(FakeRunnerは呼び出し
        // ごとに同じ応答を返す単純な作り)。
        let runner = FakeRunner::ok("0\tother\n3\tmy-tag\n");
        let resolver = TmuxLocatorResolver::new(runner);
        let locator = TmuxLocator {
            scope: standalone("main"),
            kind: TmuxTargetKind::Window,
            tag: TmuxTag("my-tag".to_string()),
        };
        resolver.push_ctl_socket_path(&locator, "/tmp/isekai-pipe-ctl-new.sock").await.unwrap();
        assert_eq!(
            resolver.runner.last_call(),
            "tmux set-option -w -t 'main:3' @isekai_ctl_sock '/tmp/isekai-pipe-ctl-new.sock'"
        );
    }

    #[tokio::test]
    async fn push_ctl_socket_path_propagates_not_found_when_locator_missing() {
        let runner = FakeRunner::ok("0\tother\n");
        let resolver = TmuxLocatorResolver::new(runner);
        let locator = TmuxLocator {
            scope: standalone("main"),
            kind: TmuxTargetKind::Window,
            tag: TmuxTag("missing-tag".to_string()),
        };
        let err = resolver.push_ctl_socket_path(&locator, "/tmp/x.sock").await.unwrap_err();
        assert_eq!(err, TmuxLocatorError::NotFound(TmuxTag("missing-tag".to_string())));
    }

    // ── push_ctl_socket_to_tmux + TMUX_LOCATOR_REGISTRY 配線 (タスク#59) ──
    //
    // ここが「接続確立時・再接続時どちらでもset-optionコマンドが正しく
    // 組み立て/発行されること」「再接続でレジストリのエントリが重複せず
    // 置き換わること」を検証するテスト(タスク説明のテスト要件に対応)。
    // `push_ctl_socket_to_tmux`はプロセス全体シングルトンの代わりに
    // 呼び出し側が渡した`&parking_lot::Mutex<TmuxLocatorRegistry>`をそのまま
    // 使うので、`TMUX_LOCATOR_REGISTRY`自体には触れずローカルなレジストリで
    // 完結できる(テスト間の共有可変状態を避ける)。
    //
    // `push_ctl_socket_to_tmux`はrunnerを値で消費するため、`FakeRunner`とは別に
    // 呼び出し後も外から発行済みコマンドを見られる`RecordingRunner`
    // (`Arc<std::sync::Mutex<Vec<String>>>`にログを溜める)を使う。

    struct RecordingRunner {
        response: Result<String, TmuxRunError>,
        calls: std::sync::Arc<Mutex<Vec<String>>>,
    }

    impl RecordingRunner {
        fn new(output: &str, calls: std::sync::Arc<Mutex<Vec<String>>>) -> Self {
            Self { response: Ok(output.to_string()), calls }
        }
    }

    impl RemoteTmuxCommandRunner for RecordingRunner {
        fn run(&self, cmd: &str) -> impl Future<Output = Result<String, TmuxRunError>> + Send {
            self.calls.lock().unwrap().push(cmd.to_string());
            let response = self.response.clone();
            async move { response }
        }
    }

    #[tokio::test]
    async fn push_ctl_socket_to_tmux_is_a_noop_when_locator_unknown() {
        let registry = parking_lot::Mutex::new(TmuxLocatorRegistry::new());
        let app_pane = pane("tab-1", "pane-primary");
        let calls = std::sync::Arc::new(Mutex::new(Vec::new()));
        let runner = RecordingRunner::new("0\tother\n3\tmy-tag\n", calls.clone());

        push_ctl_socket_to_tmux(&registry, &app_pane, "/tmp/a.sock", runner).await.unwrap();

        // ロケータが無いので何もtmuxコマンドは発行されない。
        assert!(calls.lock().unwrap().is_empty());
        assert!(registry.lock().locator_for(&app_pane).is_none());
    }

    #[tokio::test]
    async fn push_ctl_socket_to_tmux_dispatches_on_initial_connect_and_reconnect_without_duplicating_registry_entry() {
        let registry = parking_lot::Mutex::new(TmuxLocatorRegistry::new());
        let app_pane = pane("tab-1", "pane-primary");
        let loc = locator("my-tag");
        // #60等の上位ロジックが既にロケータを解決・登録済み、という前提
        // (このタスクのスコープは「ロケータが分かっている前提でctl-socketパスを
        // 伝播する」部分に限る、モジュール冒頭のdoc参照)。
        registry.lock().register(app_pane.clone(), loc.clone(), None);

        // ── 初回接続 ──
        let calls_a = std::sync::Arc::new(Mutex::new(Vec::new()));
        let runner_a = RecordingRunner::new("0\tother\n3\tmy-tag\n", calls_a.clone());
        push_ctl_socket_to_tmux(&registry, &app_pane, "/tmp/isekai-pipe-ctl-a.sock", runner_a)
            .await
            .unwrap();

        assert_eq!(
            calls_a.lock().unwrap().last().unwrap(),
            "tmux set-option -w -t 'main:3' @isekai_ctl_sock '/tmp/isekai-pipe-ctl-a.sock'"
        );
        {
            let r = registry.lock();
            assert_eq!(r.ctl_socket_path_for(&app_pane), Some("/tmp/isekai-pipe-ctl-a.sock"));
            // ロケータ自体・逆引きは変わらない。
            assert_eq!(r.locator_for(&app_pane), Some(&loc));
        }

        // ── 再接続(新しいctl-socketパスが発行される) ──
        let calls_b = std::sync::Arc::new(Mutex::new(Vec::new()));
        let runner_b = RecordingRunner::new("0\tother\n3\tmy-tag\n", calls_b.clone());
        push_ctl_socket_to_tmux(&registry, &app_pane, "/tmp/isekai-pipe-ctl-b.sock", runner_b)
            .await
            .unwrap();

        assert_eq!(
            calls_b.lock().unwrap().last().unwrap(),
            "tmux set-option -w -t 'main:3' @isekai_ctl_sock '/tmp/isekai-pipe-ctl-b.sock'"
        );

        // レジストリのエントリは置き換わっただけで、重複していない(app_pane→1件のみ)。
        {
            let r = registry.lock();
            assert_eq!(r.ctl_socket_path_for(&app_pane), Some("/tmp/isekai-pipe-ctl-b.sock"));
            assert_eq!(r.locator_for(&app_pane), Some(&loc));
            assert_eq!(r.app_pane_for(&loc), Some(&app_pane));
        }
    }

    // ── TmuxLocatorRegistry (マッピングテーブル) ─────────

    fn pane(tab: &str, pane: &str) -> AppPaneId {
        AppPaneId { tab_id: tab.to_string(), pane_id: pane.to_string() }
    }

    fn locator(tag: &str) -> TmuxLocator {
        TmuxLocator { scope: standalone("main"), kind: TmuxTargetKind::Window, tag: TmuxTag(tag.to_string()) }
    }

    #[test]
    fn register_then_lookup_both_directions() {
        let mut registry = TmuxLocatorRegistry::new();
        let app_pane = pane("tab-1", "pane-primary");
        let loc = locator("tag-1");
        registry.register(app_pane.clone(), loc.clone(), Some("/tmp/isekai-pipe-ctl-a.sock".to_string()));

        assert_eq!(registry.locator_for(&app_pane), Some(&loc));
        assert_eq!(registry.app_pane_for(&loc), Some(&app_pane));
        assert_eq!(registry.ctl_socket_path_for(&app_pane), Some("/tmp/isekai-pipe-ctl-a.sock"));
    }

    #[test]
    fn register_without_ctl_socket_path_yet_then_set_it_later() {
        let mut registry = TmuxLocatorRegistry::new();
        let app_pane = pane("tab-1", "pane-primary");
        registry.register(app_pane.clone(), locator("tag-1"), None);
        assert_eq!(registry.ctl_socket_path_for(&app_pane), None);

        let updated = registry.set_ctl_socket_path(&app_pane, Some("/tmp/isekai-pipe-ctl-b.sock".to_string()));
        assert!(updated);
        assert_eq!(registry.ctl_socket_path_for(&app_pane), Some("/tmp/isekai-pipe-ctl-b.sock"));
    }

    #[test]
    fn set_ctl_socket_path_on_unknown_app_pane_is_a_noop() {
        let mut registry = TmuxLocatorRegistry::new();
        let unknown = pane("tab-x", "pane-x");
        let updated = registry.set_ctl_socket_path(&unknown, Some("/tmp/whatever.sock".to_string()));
        assert!(!updated);
        assert_eq!(registry.ctl_socket_path_for(&unknown), None);
    }

    #[test]
    fn reconnect_updates_ctl_socket_path_via_register() {
        // Epic Mは再接続のたびに新しいランダムctl-socketパスを発行する
        // (`transport::ctl_streamlocal::new_ctl_socket_path`)。同じタブ/ペインが
        // 同じtmuxロケータのまま再接続しても、register()の呼び直しだけで
        // 新しいパスに更新できることを確認する。
        let mut registry = TmuxLocatorRegistry::new();
        let app_pane = pane("tab-1", "pane-primary");
        let loc = locator("tag-1");
        registry.register(app_pane.clone(), loc.clone(), Some("/tmp/old.sock".to_string()));

        registry.register(app_pane.clone(), loc.clone(), Some("/tmp/new.sock".to_string()));

        assert_eq!(registry.ctl_socket_path_for(&app_pane), Some("/tmp/new.sock"));
        // 逆引きも壊れていない。
        assert_eq!(registry.app_pane_for(&loc), Some(&app_pane));
    }

    #[test]
    fn reconnect_to_a_different_tmux_window_replaces_the_locator_and_drops_the_stale_reverse_entry() {
        // タブが再接続の際に(tmuxセッションが一度落ちて新しく張り直された等の
        // 理由で)別のtmuxウィンドウにアタッチし直したケース。古いロケータの
        // 逆引きエントリが残って誤ったapp_paneを指し続けないことを確認する。
        let mut registry = TmuxLocatorRegistry::new();
        let app_pane = pane("tab-1", "pane-primary");
        let old_locator = locator("tag-old");
        let new_locator = locator("tag-new");

        registry.register(app_pane.clone(), old_locator.clone(), Some("/tmp/old.sock".to_string()));
        registry.register(app_pane.clone(), new_locator.clone(), Some("/tmp/new.sock".to_string()));

        assert_eq!(registry.locator_for(&app_pane), Some(&new_locator));
        assert_eq!(registry.app_pane_for(&new_locator), Some(&app_pane));
        // 古いロケータはもう誰も指していない(stale状態のクリーンアップ)。
        assert_eq!(registry.app_pane_for(&old_locator), None);
    }

    #[test]
    fn lookup_of_unknown_app_pane_or_stale_locator_returns_none() {
        let registry = TmuxLocatorRegistry::new();
        let unknown_pane = pane("nope", "nope");
        let unknown_locator = locator("never-registered");

        assert_eq!(registry.locator_for(&unknown_pane), None);
        assert_eq!(registry.app_pane_for(&unknown_locator), None);
        assert_eq!(registry.ctl_socket_path_for(&unknown_pane), None);
    }

    #[test]
    fn unregister_removes_both_forward_and_reverse_entries() {
        let mut registry = TmuxLocatorRegistry::new();
        let app_pane = pane("tab-1", "pane-primary");
        let loc = locator("tag-1");
        registry.register(app_pane.clone(), loc.clone(), Some("/tmp/a.sock".to_string()));

        let removed = registry.unregister(&app_pane).unwrap();
        assert_eq!(removed.locator, loc);

        assert_eq!(registry.locator_for(&app_pane), None);
        assert_eq!(registry.app_pane_for(&loc), None);
    }

    #[test]
    fn unregister_unknown_app_pane_returns_none() {
        let mut registry = TmuxLocatorRegistry::new();
        assert_eq!(registry.unregister(&pane("nope", "nope")), None);
    }

    #[test]
    fn distinct_tabs_and_split_panes_of_the_same_tab_coexist_independently() {
        // 1タブがプライマリペイン+splitペインを最大1つ持てる(バックグラウンド
        // 参照)ため、同じtab_idで異なるpane_idの2エントリが互いを上書きしない
        // ことを確認する。
        let mut registry = TmuxLocatorRegistry::new();
        let primary = pane("tab-1", "pane-primary");
        let split = pane("tab-1", "pane-split");
        registry.register(primary.clone(), locator("tag-primary"), Some("/tmp/primary.sock".to_string()));
        registry.register(split.clone(), locator("tag-split"), Some("/tmp/split.sock".to_string()));

        assert_eq!(registry.locator_for(&primary), Some(&locator("tag-primary")));
        assert_eq!(registry.locator_for(&split), Some(&locator("tag-split")));
        assert_eq!(registry.ctl_socket_path_for(&primary), Some("/tmp/primary.sock"));
        assert_eq!(registry.ctl_socket_path_for(&split), Some("/tmp/split.sock"));
    }

    // ── TmuxTag::new_random ──────────────────────────────

    #[test]
    fn new_random_tags_are_32_hex_chars_and_unique() {
        let a = TmuxTag::new_random();
        let b = TmuxTag::new_random();
        assert_eq!(a.0.len(), 32);
        assert!(a.0.bytes().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        assert_ne!(a, b);
    }

    // ── AppPaneId::generate_process_local (タスク#59) ────────

    #[test]
    fn generate_process_local_ids_are_unique_and_stable_as_a_hashmap_key() {
        let a = AppPaneId::generate_process_local();
        let b = AppPaneId::generate_process_local();
        assert_ne!(a, b, "two independently generated ids must not collide");

        // register()のキーとして正しく機能すること(=Hash/Eqが実用上壊れていない)
        // をレジストリ越しに確認する。
        let mut registry = TmuxLocatorRegistry::new();
        registry.register(a.clone(), locator("tag-a"), None);
        assert_eq!(registry.locator_for(&a), Some(&locator("tag-a")));
        assert_eq!(registry.locator_for(&b), None);
    }
}
