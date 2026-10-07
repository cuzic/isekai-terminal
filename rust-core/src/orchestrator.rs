use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use parking_lot::Mutex;

use crate::{
    CellData, ClipboardPayload, ConnectionIssueHint, ConnectionPublicState, ForwardState, OrchestratorCallback,
    ScreenUpdate, ScrollbackSearchMatch, SessionCallback, SshConfig, SshError, TrzszPublicState, RUNTIME,
};
use crate::file_preview::{self, FilePreviewOutcome, FilePreviewRequestKind};
use crate::net_health_policy;
use crate::quic_transport::{QuicConfig, QuicSession};
use crate::isekai_pipe_quic_transport::{IsekaiPipeQuicConfig, IsekaiPipeQuicSession};
use crate::multipath_transport::{MultipathIsekaiPipeQuicConfig, MultipathIsekaiPipeQuicSession};
use crate::isekai_stun_p2p_transport::{IsekaiStunP2pConfig, IsekaiStunP2pSession};
use crate::isekai_link_relay_transport::{IsekaiLinkRelayConfig, IsekaiLinkRelaySession};
use crate::transport::{ExecError, ExecOutput};
use crate::reconnect_fsm::{
    AttemptRef, AttemptSource, BackgroundState, ConnPhase, DisconnectKind, ReconnectEffect, ReconnectEvent,
    ReconnectState, NETWORK_LOST_REASON,
};
use std::collections::VecDeque;

// ── Active session ────────────────────────────────────────

/// `Arc<T>`のバリアントのみを持つため`Clone`は安価(参照カウントの複製のみ)。
/// `run_exec`(タスク#61)がasyncメソッドで、同期版`dispatch_all!`マクロの
/// match式内で`.await`できない(アームごとに型の異なるFutureを生むため)ため、
/// 呼び出し側で`session`ロック(`parking_lot::Mutex`、非async)を先に解放してから
/// awaitできるよう、cloneして手元に持ってから使う。
#[derive(Clone)]
enum ActiveSession {
    Ssh(Arc<crate::SshSession>),
    Quic(Arc<QuicSession>),
    IsekaiPipeQuic(Arc<IsekaiPipeQuicSession>),
    MultipathIsekaiPipeQuic(Arc<MultipathIsekaiPipeQuicSession>),
    IsekaiStunP2p(Arc<IsekaiStunP2pSession>),
    IsekaiLinkRelay(Arc<IsekaiLinkRelaySession>),
}

/// `ActiveSession`の全バリアントに同じメソッド呼び出しを委譲するだけのmatchを
/// 展開する。6トランスポートすべてが同じ`SessionCore`委譲メソッドを持つため
/// （各transportモジュール参照）、ここは常に「アームごとの分岐ロジックが無い」
/// 純粋な委譲にのみ使う。トランスポートごとに挙動が違うメソッドは対象外とし、
/// 手書きのmatchのままにする。
macro_rules! dispatch_all {
    ($self:expr, $method:ident $(, $arg:expr)*) => {
        match $self {
            Self::Ssh(s) => s.$method($($arg),*),
            Self::Quic(s) => s.$method($($arg),*),
            Self::IsekaiPipeQuic(s) => s.$method($($arg),*),
            Self::MultipathIsekaiPipeQuic(s) => s.$method($($arg),*),
            Self::IsekaiStunP2p(s) => s.$method($($arg),*),
            Self::IsekaiLinkRelay(s) => s.$method($($arg),*),
        }
    };
}

impl ActiveSession {
    fn send(&self, data: Vec<u8>) {
        dispatch_all!(self, send, data)
    }
    fn resize(&self, cols: u32, rows: u32) {
        dispatch_all!(self, resize, cols, rows)
    }
    /// タスク#60: OSのフォーカス変化を全トランスポート共通で`SessionCore`まで委譲する
    /// (`Terminal`/`SessionCore`はトランスポート非依存のため対象外の分岐は無い)。
    fn notify_focus_change(&self, focused: bool) {
        dispatch_all!(self, notify_focus_change, focused)
    }
    fn disconnect(&self) {
        dispatch_all!(self, disconnect)
    }
    /// #11: ユーザーが「今すぐWiFiに戻す」を要求した。マルチパス以外のセッションでは
    /// 意味を持たないため何もしない（呼び出し側は「そのとき使っているtransportが
    /// マルチパスかどうか」を意識せず日和見的に呼べばよい）。
    fn force_return_to_wifi(&self) {
        if let Self::MultipathIsekaiPipeQuic(s) = self {
            s.force_return_to_wifi();
        }
    }
    /// `UpstreamHealthMonitor`(Android ConnectivityManager由来、force_return_to_wifiと
    /// 同じくマルチパス以外のtransportでは何もしない)からの生イベントを
    /// `RebindManager`へ転送する。
    fn notify_upstream_health_degraded(&self) {
        if let Self::MultipathIsekaiPipeQuic(s) = self {
            s.notify_upstream_health_degraded();
        }
    }
    /// trzsz転送中(WaitingUser含む)かどうかをRebindManager(#22のDriver)の
    /// 静けさ判定の補助シグナルとして伝える。マルチパス以外では意味を持たないため
    /// `force_return_to_wifi`と同じくno-op委譲。
    fn set_interactive_busy(&self, busy: bool) {
        if let Self::MultipathIsekaiPipeQuic(s) = self {
            s.set_interactive_busy(busy);
        }
    }
    fn scrollback_len(&self) -> u32 {
        dispatch_all!(self, scrollback_len)
    }
    fn scrollback_cells(&self, offset: u32, rows: u32) -> Vec<CellData> {
        dispatch_all!(self, scrollback_cells, offset, rows)
    }
    fn search_scrollback(&self, query: String, case_sensitive: bool) -> Vec<ScrollbackSearchMatch> {
        dispatch_all!(self, search_scrollback, query, case_sensitive)
    }
    fn trzsz_accept_upload(&self, transfer_id: String, file_name: String, file_size: u64, mode: u32) {
        dispatch_all!(self, trzsz_accept_upload, transfer_id, file_name, file_size, mode)
    }
    fn trzsz_send_chunk(&self, transfer_id: String, data: Vec<u8>, is_last: bool) {
        dispatch_all!(self, trzsz_send_chunk, transfer_id, data, is_last)
    }
    fn trzsz_accept_download(&self, transfer_id: String) {
        dispatch_all!(self, trzsz_accept_download, transfer_id)
    }
    fn trzsz_cancel(&self, transfer_id: String) {
        dispatch_all!(self, trzsz_cancel, transfer_id)
    }
    /// タスク#13(OSC 133): 全トランスポート共通で`SessionCore`まで委譲する
    /// (`notify_focus_change`と同じくトランスポート非依存のため対象外の分岐は無い)。
    fn jump_to_previous_prompt(&self, from_scroll_offset: u32, from_showing_scrollback: bool) {
        dispatch_all!(self, jump_to_previous_prompt, from_scroll_offset, from_showing_scrollback)
    }
    fn jump_to_next_prompt(&self, from_scroll_offset: u32, from_showing_scrollback: bool) {
        dispatch_all!(self, jump_to_next_prompt, from_scroll_offset, from_showing_scrollback)
    }
    fn click_to_prompt_cursor(&self, row: u32, col: u32) {
        dispatch_all!(self, click_to_prompt_cursor, row, col)
    }
    fn copy_last_command_output(&self) {
        dispatch_all!(self, copy_last_command_output)
    }
    /// タスク#17: `run_ssh_channel_loop`は6トランスポート共通の実体なので
    /// (`transport/ssh_handler.rs`のモジュールdoc参照)、トランスポート別の
    /// 対応可否分岐は無い——全バリアントで同じ委譲でよい。
    fn file_preview_exec(&self, request_id: String, command_line: String) -> bool {
        dispatch_all!(self, file_preview_exec, request_id, command_line)
    }
    /// Phase 12: per-session theme。全トランスポート共通(`Terminal`/`SessionCore`は
    /// トランスポート非依存)なので対象外の分岐は無い。
    fn set_theme(&self, theme: crate::theme::Theme) {
        dispatch_all!(self, set_theme, theme)
    }
    /// `AI_INTEGRATION_DESIGN.md` §3のAIパネル機能opt-inゲート。`set_theme`と同じく
    /// 全トランスポート共通。
    fn set_panel_enabled(&self, enabled: bool) {
        dispatch_all!(self, set_panel_enabled, enabled)
    }
    /// タスク#61: 既存のインタラクティブチャネル/PTYに触れず、この(プール済み)
    /// 接続上で短命なexecコマンドを実行する。全トランスポート共通
    /// (`SessionCore::run_exec`)なので対象外の分岐は無いが、
    /// asyncメソッドは`dispatch_all!`(各アームを`.await`しないため型が揃わない)
    /// では書けないので手書きのmatchにする。
    async fn run_exec(&self, command: String) -> Result<ExecOutput, ExecError> {
        match self {
            Self::Ssh(s) => s.run_exec(command).await,
            Self::Quic(s) => s.run_exec(command).await,
            Self::IsekaiPipeQuic(s) => s.run_exec(command).await,
            Self::MultipathIsekaiPipeQuic(s) => s.run_exec(command).await,
            Self::IsekaiStunP2p(s) => s.run_exec(command).await,
            Self::IsekaiLinkRelay(s) => s.run_exec(command).await,
        }
    }
}

// ── Shared internal state ─────────────────────────────────

// `ConnPhase`/`BackgroundState`/`DisconnectKind`/`NETWORK_LOST_REASON`は
// `crate::reconnect_fsm`へ移した(ADR_FUNCTIONAL_CORE_EFFECTS.md §6 Step 3a)。

/// 直前に成功した(あるいは試みた)`connect_*`の種類とConfigを保持し、予期しない
/// 切断時に同じ接続を自動的に張り直せるようにする(tssh のUDPモード reconnect相当)。
/// 全Configは既に`Clone`実装済みなので、そのまま複製して再利用できる。
/// `IsekaiPipeQuic`と`IsekaiPipeQuicAuto`は同じ`IsekaiPipeQuicConfig`/セッション型を
/// 使うが呼ぶメソッド(`connect` vs `connect_auto`)が違うため、別バリアントとして区別する
/// (`connect_auto`はQUICブートストラップ失敗時に自動でTCP SSHへフォールバックする挙動を持つ)。
#[derive(Clone)]
enum LastConnectAttempt {
    Ssh(SshConfig),
    Quic(QuicConfig),
    IsekaiPipeQuic(IsekaiPipeQuicConfig),
    IsekaiPipeQuicAuto(IsekaiPipeQuicConfig),
    MultipathIsekaiPipeQuic(MultipathIsekaiPipeQuicConfig),
    IsekaiStunP2p(IsekaiStunP2pConfig),
    IsekaiLinkRelay(IsekaiLinkRelayConfig),
}

impl LastConnectAttempt {
    fn host_port_is_quic(&self) -> (String, u16, bool) {
        match self {
            Self::Ssh(c) => (c.host.clone(), c.port, false),
            Self::Quic(c) => (c.ssh_host.clone(), c.ssh_port, true),
            Self::IsekaiPipeQuic(c) | Self::IsekaiPipeQuicAuto(c) => (c.ssh_host.clone(), c.ssh_port, true),
            Self::MultipathIsekaiPipeQuic(c) => (c.ssh_host.clone(), c.ssh_port, true),
            Self::IsekaiStunP2p(c) => (c.ssh_host.clone(), c.ssh_port, true),
            Self::IsekaiLinkRelay(c) => (c.ssh_host.clone(), c.ssh_port, true),
        }
    }

    /// #19: Local Network Privacyヒント判定の材料として使う、この接続試行が
    /// 使ったプライベートLANアドレス候補。`MultipathIsekaiPipeQuic`は
    /// `direct_host`(Tailscaleを介さない直接到達アドレス)こそが本来の狙い
    /// なのでそちらを優先し、無ければ主ホストにフォールバックする。
    fn local_network_candidate_host(&self) -> String {
        if let Self::MultipathIsekaiPipeQuic(c) = self {
            if let Some(direct) = &c.direct_host {
                return direct.clone();
            }
        }
        self.host_port_is_quic().0
    }

    /// #175: この接続設定で確立した世代について、プラットフォーム側のupstream health監視
    /// (`ConnectionEdge::Established::upstream_failover`)を登録すべきか。upstream failover
    /// (`RebindManager`)はマルチパスのトランスポートにしか無いので、それ以外は常に`false`。
    fn wants_upstream_failover_monitor(&self) -> bool {
        match self {
            Self::MultipathIsekaiPipeQuic(c) => c.enable_upstream_failover,
            Self::Ssh(_)
            | Self::Quic(_)
            | Self::IsekaiPipeQuic(_)
            | Self::IsekaiPipeQuicAuto(_)
            | Self::IsekaiStunP2p(_)
            | Self::IsekaiLinkRelay(_) => false,
        }
    }

    /// #175: 自動再接続・フォアグラウンド復帰の再接続([`connect_via`])に渡す形にする。
    ///
    /// `MultipathIsekaiPipeQuicConfig`の`wifi_fd`/`cellular_fd`は、Kotlin側が`detachFd()`で
    /// 所有権を手放した生fdで、最初のセッションが`udp_socket_from_raw_fd`で引き取り、そのセッションの
    /// 破棄と同時にcloseされる(1回きりの資源)。再接続ループ・フォアグラウンド復帰は、一度`Connected`に
    /// なった(=そのfdを既に引き取った)後にしか走らないので、同じfd番号をもう一度引き取ると、既にclose
    /// 済みのfd、あるいはその番号を再利用した**無関係な**fdを奪って閉じてしまう。再接続では物理pathを
    /// 外し、path0/path1だけのマルチパスで張り直す(物理Wi-Fi/セルラーpathは実験的・既定OFFで、
    /// 使えなければ黙ってフォールバックする日和見的ポリシー、`PLAN.md` Phase 9-4)。Kotlin側も
    /// 物理マルチパスのhandleを`Lost`で解放し、再接続では取り直さない(取り直してもこのConfigへ
    /// 渡す経路が無い)。
    fn for_reconnect(self) -> Self {
        match self {
            Self::MultipathIsekaiPipeQuic(mut c) => {
                c.wifi_fd = None;
                c.wifi_local_ip = None;
                c.cellular_fd = None;
                c.cellular_local_ip = None;
                Self::MultipathIsekaiPipeQuic(c)
            }
            other @ (Self::Ssh(_)
            | Self::Quic(_)
            | Self::IsekaiPipeQuic(_)
            | Self::IsekaiPipeQuicAuto(_)
            | Self::IsekaiStunP2p(_)
            | Self::IsekaiLinkRelay(_)) => other,
        }
    }
}

/// #19: 接続失敗の原因がiOSのLocal Network Privacy拒否である可能性を示す
/// ヒントを判定する。`attempt`が指すアドレスがプライベート/リンクローカル
/// (またはBonjourの`.local`名)であればヒントを付ける。
fn classify_disconnect_issue_hint(attempt: Option<&LastConnectAttempt>) -> Option<ConnectionIssueHint> {
    let host = attempt?.local_network_candidate_host();
    looks_like_local_network_target(&host).then_some(ConnectionIssueHint::LocalNetworkPermissionPossiblyDenied)
}

fn looks_like_local_network_target(host: &str) -> bool {
    match host.parse::<std::net::IpAddr>() {
        Ok(ip) => is_private_or_link_local(ip),
        // 大文字小文字を区別しないDNS名の性質上"MacBook.LOCAL"、末尾ドット付きの
        // "host.local."(FQDN表記)も同じmDNS名として扱う(codexレビュー指摘)。
        Err(_) => host.trim_end_matches('.').to_ascii_lowercase().ends_with(".local"),
    }
}

fn is_private_or_link_local(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => v4.is_private() || v4.is_link_local(),
        std::net::IpAddr::V6(v6) => {
            let seg = v6.segments();
            (seg[0] & 0xfe00) == 0xfc00 // fc00::/7 unique local
                || (seg[0] & 0xffc0) == 0xfe80 // fe80::/10 link local
        }
    }
}

/// 自動再接続ループのタイミング定数(`net_health_policy::NetPathPolicy`と同じ理由で
/// テストで短い値に差し替えられるよう構造体化。既定値はMVPとしてハードコード、tssh の
/// `aliveTimeout` 相当が60秒)。定義はtick会計を持つreducer側(ADR §6 Step 3b/3c)。
pub(crate) use crate::reconnect_fsm::ReconnectPolicy;

/// trzsz ダウンロードの累積バッファに設ける上限(#60)。trzsz プロトコルの
/// `SIZE`(申告値)はサーバー側の自己申告に過ぎず強制されないため、悪意ある/
/// 壊れたサーバーが巨大な SIZE を申告して DATA を送り続けると `download_buf` が
/// 無制限に肥大化し端末が OOM でクラッシュし得る。実際に受信したバイト数の実測値
/// (`download_buf.len() + 今回のchunk長`)がこの上限を超えたら転送を中断する。
const MAX_DOWNLOAD_BUF_BYTES: usize = 2 * 1024 * 1024 * 1024; // 2 GiB

struct OrchestratorState {
    /// 再接続に関する集約(`phase`・`reconnect_epoch`・`reconnect_loop_active`・
    /// `retry_attempt_in_flight`・`pending_wake`・`user_initiated_disconnect`・
    /// `background_state`・`last_attempt`)。判断は[`ReconnectState::apply`]が行う
    /// (ADR_FUNCTIONAL_CORE_EFFECTS.md §6 Step 3a)。別ストアではなくこの構造体の
    /// フィールドなので、所有者は`OrchestratorState`1つのまま(ADR §4.3)。
    /// Step 3aで`apply`経由に移行していない書き手は従来どおりフィールドを直接書く。
    reconnect: ReconnectState,
    /// `reconnect`のapplyが出した状態公開・接続エッジの、配信待ちの列(PR #167レビューL-1)。
    /// applyと**同じ臨界区間で**積むので、列の順序はreducerが遷移を適用した順序そのもの。
    /// 配信は[`flush_publications`]が1スレッドずつ行う([`PublicationQueue`]参照)。
    publications: PublicationQueue,
    /// Active transfer ID set by on_trzsz_request; used to route trzsz commands without exposing ID to Kotlin
    current_transfer_id: Option<String>,
    /// "upload" / "download" set on on_trzsz_request; used to detect download accumulation
    trzsz_mode: Option<String>,
    /// Accumulates bytes from on_trzsz_download_chunk; drained on on_trzsz_finished
    download_buf: Vec<u8>,
    /// #60: `MAX_DOWNLOAD_BUF_BYTES` を超えてローカルに中断した転送のID。
    /// `trzsz_cancel` は非同期(セッションイベントループへのコマンド送信)なので、
    /// 実際の `on_trzsz_finished`(success=false, message="Cancelled" 等の汎用文言)が
    /// 届くのは少し後になる。その届いた際にこのIDが一致すれば、汎用文言ではなく
    /// ユーザーに分かりやすい「大きすぎる」メッセージへ差し替える。
    size_limit_exceeded_for: Option<String>,

    /// タスク#17: `file_preview_request`が発行した`request_id`→要求種別のマップ。
    /// `TransportEvent::FilePreviewExecResult`が届いた時点でここから取り出して
    /// `crate::file_preview::parse_result`に渡す(execチャネル自体は「どのサブコマンドを
    /// 要求したか」を知らずstdoutを運ぶだけなので、パースにはこの対応が要る)。
    /// 複数の要求が同時に in-flight でも(例: ディレクトリ一覧中に別ファイルをcat)
    /// `request_id`ごとに独立して解決できるようMapにしている(trzszの
    /// `current_transfer_id`のような単一スロットでは足りない)。
    pending_file_previews: HashMap<String, FilePreviewRequestKind>,

    // ── 自動再接続(tssh風reconnect) ──────────────────────
    /// セッションオブジェクト1つ生成するごとにインクリメントする世代カウンタ。
    /// `OrchestratorAdapter`は生成時にこの値をキャプチャし、`SessionCallback`の
    /// 各メソッド呼び出し時にこの値と現在値が一致するかを確認する。不一致なら
    /// 「既に見捨てられた古いセッションからの遅延コールバック」なので無視する
    /// (新しい手動接続/再接続試行が、古いセッションの遅延イベントに状態を
    /// 巻き戻されないようにするための独立した仕組み。`reconnect_epoch`とは別物)。
    session_generation: u64,
    /// 直前に成功した(あるいは試みた)`connect_*`。予期しない切断時にこれを
    /// 使って自動的に再接続を試みる。
    ///
    /// 「今どのホスト/ポートへ、QUIC系トランスポートで繋いでいるか」もこの1つの
    /// フィールドが唯一の出所(SSOT)であり、専用のミラーフィールドは持たない
    /// ([`OrchestratorState::current_target`]が[`LastConnectAttempt::host_port_is_quic`]
    /// 経由で導出する)。以前は`current_host`/`current_port`/`is_quic`という3つの
    /// ミラーフィールドがあったが、書き込みがこのフィールドと別のロック取得に
    /// 分かれていたため、その隙間で両者が食い違って観測され得た(`rust-ssot.md`が
    /// Kotlin側について警告している「もう1つの状態のコピー」と同じ問題がRust内部で
    /// 起きていた)。
    ///
    /// **書き手は[`SessionOrchestrator::begin_connect`]だけ**: reducer側の不透明な参照
    /// `reconnect.last_attempt`(`ManualConnectStarted`のapplyが書く)と常に同じ臨界区間で
    /// 書く(秘密を含むConfigはreducerに載せない、ADR §3-3)。
    last_connect_attempt: Option<LastConnectAttempt>,
    /// 再接続ループのタイミング。テストでは短い値に差し替える。
    reconnect_policy: ReconnectPolicy,
    /// タスク#57: このタブが現在フォーカスされている(=Compose側の
    /// `isActive && hasFocus`)かどうか。`notify_focus_change`が
    /// `Terminal`のCSIフォーカスレポーティングと同じ生イベントから複製する
    /// (`rust-ssot.md`: 新しい判断ロジックのために新しいUniFFIメソッドを増やすのでは
    /// なく、既存の生イベント転送経路を再利用する)。`OrchestratorAdapter::on_notify`が
    /// `background_state`と合わせて「今この瞬間ユーザーがこのタブを見ているか」の
    /// 抑制判断に使う。
    tab_focused: bool,
    /// タスク#57フォローアップ(実機検証、2026-07-28): アプリ自体が今フォアグラウンド
    /// かどうかの生の事実。`notify_did_enter_background`/`notify_will_enter_foreground`
    /// が`phase`に関係なく無条件で複製する(`tab_focused`と同じ「生イベントをそのまま
    /// 転送するだけ」の扱い、`rust-ssot.md`)。`background_state`(#20)は
    /// 再接続バジェット管理のためのFSMであり「ユーザーが今画面を見ているか」の事実とは
    /// 別物——`Idle`中の切断待機や再接続成功時に`Foreground`へ戻る等、アプリの実際の
    /// 前景/背景とは無関係な理由で値が変わる。`OrchestratorAdapter::on_notify`の
    /// 抑制判断はこの`app_foreground`を見る(以前は`background_state`を誤用しており、
    /// バックグラウンド化してもtmux通知が永久に抑制され続けるバグがあった)。
    app_foreground: bool,
    /// タスク#57: 直近配信した`(tmux_tag, seq)`の小さなリングバッファ。
    /// `isekai_protocol::CtlMessage::Notify`のdocコメントが想定する重複配信
    /// (tmux hookの再発火・session group内の複数メンバーからの重複起動、
    /// `tmux_notify.rs`のモジュールdoc参照)を、同じペアが来たら黙って無視する
    /// ことで検出する。1件だけ(`Option`)だと、session group内の別ウィンドウの
    /// タグが交互に届いた場合に重複排除が破れる(opusレビュー指摘)ため、
    /// [`RECENT_NOTIFY_SEQ_CAPACITY`]件までは覚えておく。
    recent_notify_seqs: std::collections::VecDeque<(String, u64)>,
}

impl OrchestratorState {
    /// テスト専用: 直前の接続試行を、`begin_connect`を経由せずに記録する。shell側の実体
    /// (`last_connect_attempt`)とreducer側の不透明な参照(`reconnect.last_attempt`)を同時に進める
    /// (本番の書き手は[`SessionOrchestrator::begin_connect`]だけで、そちらは
    /// [`ReconnectEvent::ManualConnectStarted`]のapplyと同じ臨界区間で両者を書く)。
    #[cfg(test)]
    fn set_last_connect_attempt(&mut self, attempt: LastConnectAttempt) {
        self.reconnect.last_attempt = Some(AttemptRef::next_after(self.reconnect.last_attempt));
        self.last_connect_attempt = Some(attempt);
    }

    /// 現在(直近に)接続を試みている相手と、その経路がQUIC系かどうか。
    /// [`OrchestratorState::last_connect_attempt`]からその場で導出するため、
    /// 「接続先」と「その接続に使ったConfig」が食い違うことは原理的に起こり得ない。
    /// まだ一度も`connect_*`が呼ばれていなければ`None`。
    fn current_target(&self) -> Option<(String, u16, bool)> {
        self.last_connect_attempt.as_ref().map(LastConnectAttempt::host_port_is_quic)
    }

    /// [`OrchestratorState::current_target`]のQUIC判定だけを取り出したもの。
    /// 接続試行が無い間は「QUICではない」(=以前の`is_quic`フィールドの初期値と同じ)。
    fn is_quic(&self) -> bool {
        self.current_target().is_some_and(|(_, _, is_quic)| is_quic)
    }
}

/// [`OrchestratorState::recent_notify_seqs`]が覚えておく直近件数。tmux hookの
/// 重複配信は同じイベントについてほぼ同時に(せいぜい数件)届く想定のため、
/// 無界に育てる必要はない——小さな固定upper boundにしてメモリを有界に保つ。
const RECENT_NOTIFY_SEQ_CAPACITY: usize = 8;

/// 1回の再接続試行を実行する処理の型。既定は`connect_via`(実際にセッションを
/// 生成して接続する)。テストでは実ネットワークに触れないフェイクへ差し替え、
/// 呼び出し回数・cadenceだけを検証する — `connect()`自体が非同期fire-and-forget
/// なので、実際に接続できたかどうかまではこの粒度の単体テストでは検証しない
/// (Codexレビュー指摘、実ネットワーク越しの成功パスは実機確認でカバーする)。
type ReconnectAttemptFn = dyn Fn(&Arc<OrchestratorShared>, LastConnectAttempt) -> Result<(), SshError> + Send + Sync;

pub(crate) struct OrchestratorShared {
    state: Mutex<OrchestratorState>,
    callback: Arc<dyn OrchestratorCallback>,
    session: Mutex<Option<ActiveSession>>,
    /// `notify_network_path_changed`のdebounce/epoch状態。`Connected && !is_quic`の
    /// ケースだけがこれを実際に使う([`crate::net_health_policy`]参照)。
    path_observer: Mutex<crate::net_health_policy::PathObserver>,
    /// タスク#59: [`crate::tmux_locator::TmuxLocatorRegistry`]のキーとして使う、
    /// このタブの安定した識別子。`create_session_orchestrator`で1回だけ発行され、
    /// 再接続(`connect_via`が新しい`ActiveSession`を作り直す場合を含む)をまたいでも
    /// 不変(`OrchestratorShared`自体はタブの生存期間中ずっと同じインスタンス)。
    /// Kotlin側の実`PaneAddress(tabId, paneId)`が現時点でUniFFI境界を越えて
    /// 渡ってきていないための暫定値である点は
    /// [`crate::tmux_locator::AppPaneId::generate_process_local`]のdoc参照。
    pub(crate) app_pane_id: crate::tmux_locator::AppPaneId,
    reconnect_attempt: Box<ReconnectAttemptFn>,
    /// `spawn_reconnect_loop`の固定間隔ポーリング待機を、ネットワーク復帰通知で
    /// 早期に打ち切るためのシグナル(isekai-pipe側`resume_loop::wait_backoff_or_network_change`
    /// と同じ発想 — 詳細は`notify_network_path_changed`の`ConnPhase::Idle`分岐と
    /// `spawn_reconnect_loop`のコメント参照)。`notify_one`は「まだ誰も待っていない
    /// 状態で複数回呼ぶ」場合でも1許可分にしかならないため、フラッピングする
    /// ネットワークで無限にウェイクし続ける心配は無い。
    reconnect_wake: tokio::sync::Notify,
    /// このオーケストレータが自分のバックグラウンドtask(自動再接続ループ
    /// [`spawn_reconnect_loop`]と、TCP網断debounceの遅延発火)をspawnする先の
    /// tokioランタイム(ADR_FUNCTIONAL_CORE_EFFECTS.md §6 Step 2.5)。
    ///
    /// 本番(`create_session_orchestrator`)は常にグローバル[`RUNTIME`]のHandleで、
    /// 以前の`RUNTIME.spawn`と挙動は同一。テストは`#[tokio::test(start_paused = true)]`の
    /// current_threadランタイムのHandleを**明示的に**渡し、ループのtick/debounceを
    /// 仮想時間で決定論的に進める。`Handle::try_current()`による暗黙のフォールバックは
    /// 採らない(ADR_CONNECTION_RESILIENCE_SIMULATION.md §5): 呼び出し元のランタイムに
    /// 黙って乗り換えると、本番でUniFFIスレッドからの呼び出しとtokio task内からの
    /// 呼び出しでspawn先が変わってしまうため。
    rt: tokio::runtime::Handle,
}

// ── OrchestratorAdapter ───────────────────────────────────
// Translates old SessionCallback events → structured OrchestratorCallback

pub(crate) struct OrchestratorAdapter {
    pub(crate) shared: Arc<OrchestratorShared>,
    /// 生成時にキャプチャした`session_generation`。`is_current()`参照。
    generation: u64,
}

impl OrchestratorAdapter {
    /// 新しいセッションを1つ作るたびに呼ぶ。`session_generation`をインクリメントし、
    /// その値をこのアダプタ自身にキャプチャする(このアダプタ経由のコールバックが
    /// 「今まさに有効なセッションからのものか」を後から判定できるようにする)。
    ///
    /// Step 8a′: `session_generation += 1`と同じ臨界区間で[`ReconnectEvent::SessionCreated`]を
    /// applyする(ADR round 3 m-R3-2)。別世代のedgeが開いていれば`Lost(old)`が返るので、
    /// **ロック解放後に**ここで公開する(ADR m-R4-4・§2.4-2。呼び出し元はいずれもロックを
    /// 保持せずにこれを呼ぶ)。現状の2経路(`begin_connect`/`connect_via`)では直前のphase遷移で
    /// 既に`Lost(old)`が出ているので、ここでは何も出ない(phase遷移を伴わない将来の経路への保険)。
    fn new(shared: Arc<OrchestratorShared>) -> Self {
        let (generation, effects) = {
            let mut s = shared.state.lock();
            s.session_generation += 1;
            let generation = s.session_generation;
            let effects =
                apply_reconnect(&mut s, ReconnectEvent::SessionCreated { new_generation: generation }, &EffectContext::default());
            (generation, effects)
        };
        execute_reconnect_effects(&shared, effects, EffectContext::default());
        Self { shared, generation }
    }

    /// このアダプタが今も「現行の」セッションのものかどうか。古い(既に見捨てられた)
    /// セッションからの遅延コールバックはこれがfalseになり、呼び出し元は無視する。
    fn is_current(&self) -> bool {
        self.shared.state.lock().session_generation == self.generation
    }
}

/// `OrchestratorAdapter`の`SessionCallback`実装のうち、「古い世代からの遅延
/// コールバックなら何もしない、そうでなければ同名の`OrchestratorCallback`へ
/// そのまま委譲する」だけの単純委譲メソッドをまとめて生成する。
///
/// `ActiveSession`の`dispatch_all!`と同じ方針で、**staleガード以外の分岐
/// ロジックを持たない委譲にのみ**使う —— `on_host_key`/`on_connected`/
/// `on_disconnected`/trzsz系/`on_notify`/`on_file_preview_exec_result`のように
/// 固有の処理を持つものは手書きのまま下に残す(そちらこそが読む価値のある
/// 部分なので、マクロの一覧に埋もれさせない)。
///
/// staleだった場合の戻り値は`Default::default()`——このマクロが対象にする
/// コールバックの戻り値は`()`/`bool`/`Option<T>`だけで、いずれも既定値
/// (何もしない・拒否・値なし)が手書き実装当時と同じ意味になる。
macro_rules! forward_if_current {
    ($( fn $name:ident ( $( $arg:ident : $ty:ty ),* $(,)? ) $( -> $ret:ty )? ; )*) => {
        $(
            fn $name(&self, $( $arg : $ty ),*) $( -> $ret )? {
                if !self.is_current() { return Default::default(); }
                self.shared.callback.$name($( $arg ),*)
            }
        )*
    };
}

impl SessionCallback for OrchestratorAdapter {
    forward_if_current! {
        fn on_data(data: Vec<u8>);
        fn on_screen_update(update: ScreenUpdate);
        fn on_no_viable_path();
        fn on_forward_state_changed(id: String, state: ForwardState);
        fn on_agent_sign_request(key_fingerprint: String) -> bool;
        fn on_clipboard_write(payload: ClipboardPayload);
        fn on_clipboard_pull_request() -> Option<ClipboardPayload>;
        fn on_request_wifi_fd() -> Option<crate::PlatformFd>;
        fn on_request_cellular_fd() -> Option<crate::PlatformFd>;
        fn on_rebind_state_changed(state: crate::rebind_manager::RebindPublicState);
        fn on_prompt_jump(target: Option<crate::PromptJumpTarget>);
        fn on_prompt_output_copy_ready(text: Option<String>);
    }

    fn on_host_key(&self, fingerprint: String) -> bool {
        if !self.is_current() { return false; }
        let (host, port, _is_quic) = self.shared.state.lock().current_target()
            // 接続試行が記録されていない状態でホスト鍵確認が来ることは実際には
            // 無い(セッションは必ず`begin_connect`/`connect_via`を通ってから
            // 生成される)が、万一の場合も以前のミラーフィールドの既定値と
            // 同じ("", 22)でUIへ渡す。
            .unwrap_or_else(|| (String::new(), 22, false));
        self.shared.callback.on_host_key(host, port, fingerprint)
    }

    fn on_connected(&self) {
        if !self.is_current() { return; }
        let (effects, retry_log) = {
            let mut s = self.shared.state.lock();
            let retry_log = s.reconnect.reconnect_loop_active;
            // `Connected{host}`/`Established{host}`のhostは、`apply_reconnect`がapplyと同じ臨界区間で
            // `current_target()`から解決する。
            let effects = apply_reconnect(
                &mut s,
                ReconnectEvent::AttemptConnected { generation: self.generation },
                &EffectContext::default(),
            );
            (effects, retry_log)
        };
        if retry_log {
            crate::debug_reconnect::record("retry_attempt_in_flight off result=connected");
        }
        execute_reconnect_effects(&self.shared, effects, EffectContext::default());
    }

    fn on_disconnected(&self, reason: Option<String>) {
        if !self.is_current() { return; }
        handle_unexpected_disconnect(&self.shared, reason, Some(self.generation));
    }

    fn on_trzsz_request(
        &self, transfer_id: String, mode: String,
        suggested_name: Option<String>, expected_size: Option<u64>,
    ) {
        if !self.is_current() { return; }
        {
            let mut s = self.shared.state.lock();
            s.current_transfer_id = Some(transfer_id.clone());
            s.trzsz_mode = Some(mode.clone());
            s.download_buf.clear();
            s.size_limit_exceeded_for = None;
        }
        if let Some(session) = self.shared.session.lock().as_ref() {
            session.set_interactive_busy(true);
        }
        self.shared.callback.on_trzsz_state_changed(
            TrzszPublicState::WaitingUser { transfer_id, mode, suggested_name, expected_size }
        );
    }

    /// #60: trzsz の `SIZE` 申告値はサーバーの自己申告に過ぎず強制されないため、
    /// 実際に受信したバイト数(累積 `download_buf` 長)を都度 `MAX_DOWNLOAD_BUF_BYTES`
    /// と比較する。超過したら OOM する前に `download_buf` を捨て、転送そのものも
    /// `trzsz_cancel` で中断させる(FSM側は非同期に `on_trzsz_finished` を返してくる
    /// ので、そちらで success=false・分かりやすいメッセージに揃える)。
    fn on_trzsz_download_chunk(&self, transfer_id: String, data: Vec<u8>, _is_last: bool) {
        if !self.is_current() { return; }
        let exceeded = {
            let mut s = self.shared.state.lock();
            let would_be_len = s.download_buf.len().saturating_add(data.len());
            if would_be_len > MAX_DOWNLOAD_BUF_BYTES {
                log::warn!(
                    "trzsz: download {} exceeds {} byte cap (would reach {}), aborting to avoid OOM",
                    transfer_id, MAX_DOWNLOAD_BUF_BYTES, would_be_len
                );
                s.download_buf.clear();
                s.size_limit_exceeded_for = Some(transfer_id.clone());
                true
            } else {
                s.download_buf.extend_from_slice(&data);
                false
            }
        };
        if exceeded {
            if let Some(session) = self.shared.session.lock().as_ref() {
                session.trzsz_cancel(transfer_id);
            }
        }
    }

    fn on_trzsz_progress(&self, transfer_id: String, transferred: u64, total: Option<u64>) {
        if !self.is_current() { return; }
        let mode = self.shared.state.lock()
            .trzsz_mode.clone()
            .unwrap_or_else(|| "download".to_string());
        self.shared.callback.on_trzsz_state_changed(
            TrzszPublicState::InProgress {
                transfer_id, mode, file_name: None, transferred, total
            }
        );
    }

    fn on_trzsz_finished(&self, transfer_id: String, success: bool, message: Option<String>) {
        if !self.is_current() { return; }
        let (data, is_download, success, message) = {
            let mut s = self.shared.state.lock();
            s.current_transfer_id = None;
            let size_limit_hit = s.size_limit_exceeded_for.take().as_deref() == Some(transfer_id.as_str());
            let data = std::mem::take(&mut s.download_buf);
            let is_download = s.trzsz_mode.as_deref() == Some("download");
            if size_limit_hit {
                // #60: on_trzsz_download_chunk側で既に中断済み。trzsz_cancel経由の
                // 汎用的な message(例: "Cancelled")を、ユーザーに分かりやすい文言へ
                // 差し替える。success も常にfalseにする(万一cancel競合でtrueが
                // 届いても、上限超過を成功扱いにしてはいけない)。
                (data, is_download, false, Some("ファイルが大きすぎるため転送を中断しました".to_string()))
            } else {
                (data, is_download, success, message)
            }
        };
        if let Some(session) = self.shared.session.lock().as_ref() {
            session.set_interactive_busy(false);
        }
        if success && is_download && !data.is_empty() {
            self.shared.callback.on_download_complete(None, data);
        }
        self.shared.callback.on_trzsz_state_changed(
            TrzszPublicState::Done { transfer_id, success, message }
        );
    }

    /// タスク#57: tmux hookの発火を、(a)`(tmux_tag, seq)`重複排除、(b)フォアグラウンド
    /// +このタブ表示中の抑制、の2段階を経てから`OrchestratorCallback::on_notify`へ
    /// 渡す。
    ///
    /// (a): `isekai_protocol::CtlMessage::Notify`のdocコメントが想定する重複配信
    /// (`tmux_notify.rs`のモジュールdoc: session group内の複数グループメンバーが
    /// それぞれセッションスコープのフックを持ち得るため、同じ実イベントに対し
    /// 複数回発火し得る)を、直前に配信した`(tmux_tag, seq)`と完全一致したら
    /// 黙って無視することで検出する。
    ///
    /// (b): 「アプリがフォアグラウンドかつこのタブが今まさに表示されている」なら
    /// ユーザーは既にその出来事を画面上で見ているはずなので、Android通知としては
    /// 冗長 — 抑制する。この判断はアプリの前景/背景の生の事実(`app_foreground`)と
    /// UOSの生フォーカスイベントの複製(`tab_focused`)に基づくため`rust-ssot.md`の
    /// 対象(Kotlin側にミラー状態を作って分岐させない)。`background_state`(#20)は
    /// 再接続バジェット管理のためのFSMであり「今ユーザーが画面を見ているか」の
    /// 事実とは別物なので、ここでは意図的に見ない(実機検証、2026-07-28: これを
    /// 誤って見ていたため、バックグラウンド化してもtmux通知が一切配信されない
    /// バグがあった——`background_state`は`Idle`中の切断待機や再接続成功時など
    /// アプリの実際の前景/背景と無関係な理由でも`Foreground`に戻り得る)。
    /// per-tab通知ON/OFF設定自体はUI設定でありKotlin側
    /// (`OrchestratorCallback::on_notify`実装)の責務。
    fn on_notify(&self, kind: crate::NotifyKind, tmux_tag: String, seq: u64) {
        if !self.is_current() { return; }
        let should_deliver = {
            let mut s = self.shared.state.lock();
            let key = (tmux_tag, seq);
            if s.recent_notify_seqs.contains(&key) {
                false
            } else {
                if s.recent_notify_seqs.len() >= RECENT_NOTIFY_SEQ_CAPACITY {
                    s.recent_notify_seqs.pop_front();
                }
                s.recent_notify_seqs.push_back(key);
                !(s.tab_focused && s.app_foreground)
            }
        };
        if should_deliver {
            self.shared.callback.on_notify(kind);
        }
    }

    /// タスク#17: `pending_file_previews`から`request_id`に対応する要求種別を取り出し、
    /// `crate::file_preview::parse_result`でJSON/base64をデコード済みの
    /// `FilePreviewOutcome`へ変換してから`OrchestratorCallback`へ渡す。対応する要求が
    /// 見つからない(二重配送・古い世代からの遅延イベント等)場合はエラーとして扱う
    /// (呼び出し元がKotlin側で待っているリクエストを永遠に待たせたままにしない)。
    fn on_file_preview_exec_result(&self, request_id: String, stdout: Vec<u8>, exit_status: Option<u32>) {
        if !self.is_current() { return; }
        let kind = self.shared.state.lock().pending_file_previews.remove(&request_id);
        let outcome = match kind {
            Some(kind) => file_preview::parse_result(&kind, exit_status, &stdout),
            None => FilePreviewOutcome::Error {
                message: format!("file_preview: unknown or already-resolved request_id {request_id}"),
            },
        };
        self.shared.callback.on_file_preview_result(request_id, outcome);
    }
}

/// `notify_network_path_changed`の実際の切断処理。`&Arc<OrchestratorShared>`だけを
/// 取る自由関数にしてあるのは、debounce後の発火が`SessionOrchestrator`自身ではなく
/// `shared.rt`へspawnされたtokio task(`Arc<OrchestratorShared>`のcloneしか持たない)から
/// 呼ばれるため — `SessionOrchestrator::disconnect`(セッションを切るだけの2行)と
/// 中身は同じだが、`&self`経由ではなく`shared`に対して直接操作する。
///
/// [[always-connects.md]]の実インシデント(網断debounce発火の経路だけが自動復旧の
/// 対象外になっていた)と同じ見落としを繰り返さないよう、`OrchestratorAdapter::
/// on_disconnected`と同じ`handle_unexpected_disconnect`を経由させる —
/// 個別に「phase=Idle + Disconnected通知」を書かない。
fn apply_network_lost(shared: &Arc<OrchestratorShared>) {
    if let Some(s) = shared.session.lock().as_ref() {
        s.disconnect();
    }
    // アダプタを経由しないので、現行の`session_generation`を付けて同じ遷移を通す
    // (ADR round 3 R3-1、`generation: None`=現行)。
    handle_unexpected_disconnect(shared, Some(NETWORK_LOST_REASON.to_string()), None);
}

/// 予期しない切断(`OrchestratorAdapter::on_disconnected`・`apply_network_lost`の
/// 両方から呼ばれる)の共通処理。一度`Connected`になっていて・ユーザーが明示的に
/// 切断したのでなく・リモートプロセスの正常終了でもなく・直前の接続設定が分かって
/// いれば自動再接続ループを起動する。既にループが動作中の切断(＝1回のリトライ
/// 試行自体の失敗)は、二重にループを起動せず・連続で`Disconnected`を通知もせず、
/// ループ自身のtickに任せる。
///
/// 判断は[`ReconnectState::apply`]`(`[`ReconnectEvent::AttemptDisconnected`]`)`が行い
/// (ADR_FUNCTIONAL_CORE_EFFECTS.md §6 Step 3a)、この関数はshellとして
/// 「ロック下でapply+in-lock解決 → ロック解放後にEffectを解釈」だけを行う(§2.4-2,3)。
/// `generation`はアダプタ経由ならそのアダプタの世代、`None`なら現行の`session_generation`
/// (`apply_network_lost`経路)。
fn handle_unexpected_disconnect(shared: &Arc<OrchestratorShared>, reason: Option<String>, generation: Option<u64>) {
    let kind = DisconnectKind::classify(&reason);
    let mut ctx = EffectContext { reason, ..EffectContext::default() };
    let (effects, retry_log) = {
        let mut s = shared.state.lock();
        let generation = generation.unwrap_or(s.session_generation);
        // #19のLocal Networkヒント判定材料。秘密を含む`last_connect_attempt`はreducerへ渡さず、
        // shellが事前計算した真偽値だけを載せる(ADR §3-3)。
        let targets_local_network = classify_disconnect_issue_hint(s.last_connect_attempt.as_ref()).is_some();
        let effects = s.reconnect.apply(ReconnectEvent::AttemptDisconnected { generation, kind, targets_local_network });
        // in-lock解決(§2.4-2): ループ起動の`AttemptRef`が指す実体を、applyと同じ臨界区間で
        // 取り出す(ロック解放後に引き直すと、間に入った手動接続の新しいConfigを掴みうる)。
        ctx.resolved_attempt = resolve_start_loop_attempt(&s, &effects);
        // 旧`Action::Suppress`/`Action::StartLoop`(=Disconnectedを公開しない分岐)のときだけ記録する。
        // NOTE: この`matches!`と`resolve_start_loop_attempt`の`matches!`は登録interpreter
        // (`stage_publications`/`execute_reconnect_effects`)の外で、ログ判定とin-lock解決のために
        // Effectを覗くだけで、消費(捨てる)はしない。Effectの解釈はすべてinterpreterの明示armで行う(ADR §3-8)。
        let retry_log = !effects.iter().any(|e| matches!(e, ReconnectEffect::PublishDisconnected { .. }));
        stage_publications(&mut s, &effects, &ctx);
        (effects, retry_log)
    };
    if retry_log {
        crate::debug_reconnect::record(format!(
            "retry_attempt_in_flight off result=disconnected reason={}",
            ctx.reason.as_deref().unwrap_or("none")
        ));
    }

    // `InvalidatePathObserver`(reducerが常に先頭に返す)について:
    // `Connected`から離脱するあらゆる経路の唯一の合流点であるこの遷移の中で
    // 無効化する — `begin_connect`(新しい手動接続開始時)しか
    // `path_observer.invalidate()`を呼んでいなかったため、`disconnect()`
    // (ユーザー操作)経由やトランスポート層由来の切断がここへ到達した直後、
    // 直前のセッションに対して保留中だったnetwork-loss debounceタイマーが
    // 期限通り発火すると`is_current(epoch)`がまだtrueのまま`apply_network_lost`
    // を呼び、既に`Idle`になっている(=`was_connected=false`)このセッションに
    // 対して2回目の`Disconnected{reason: "network lost"}`通知を誤って上書き
    // 送出してしまう(pre-mortemレビューで発見、実際に踏める競合)。
    // `apply_network_lost`自身がこの関数を呼ぶ経路では、debounceクロージャの
    // `is_current(epoch)`判定は既にこの呼び出しより前に完了しているため、
    // ここでの無効化は次回以降の保留debounceにのみ作用し自己無効化にはならない。
    execute_reconnect_effects(shared, effects, ctx);
}

/// [`ReconnectEffect::StartReconnectLoop`]が指す[`AttemptRef`]の実体を、applyと同じ
/// 臨界区間内で解決する(§2.4-2のin-lock解決)。`set_last_connect_attempt`が両者を常に
/// 同時に書くので、参照が一致しないことは起こらない想定。
fn resolve_start_loop_attempt(
    s: &OrchestratorState,
    effects: &[ReconnectEffect],
) -> Option<(AttemptRef, LastConnectAttempt)> {
    let wants_loop = effects.iter().any(|e| matches!(e, ReconnectEffect::StartReconnectLoop { .. }));
    if !wants_loop {
        return None;
    }
    Some((s.reconnect.last_attempt?, s.last_connect_attempt.clone()?))
}

/// [`stage_publications`]/[`execute_reconnect_effects`]がEffectを解釈するのに要る、shell側の材料。
/// どれもreducerに載せない(秘密を含むConfig・公開文字列等)。
#[derive(Default)]
struct EffectContext<'a> {
    /// 公開する`Disconnected`/`Reconnecting`の理由文字列(切断理由・ループが起動時に受け取った理由・
    /// 中止/同期失敗の理由)。ループ起動にも使う。
    reason: Option<String>,
    /// `StartReconnectLoop`の`AttemptRef`をin-lockで解決したもの。
    resolved_attempt: Option<(AttemptRef, LastConnectAttempt)>,
    /// 再接続ループの接続設定(ループが起動時に受け取ったclone)。`StartAttempt`で試行し、
    /// `PublishReconnectTimedOut`のLocal Networkヒントの判定に使う。ループの外では`None`。
    loop_attempt: Option<&'a LastConnectAttempt>,
}

/// コールバックで公開する1件(状態公開か接続エッジ)。
enum Publication {
    State(ConnectionPublicState),
    Edge(crate::ConnectionEdge, u64),
}

/// 状態公開・接続エッジの配信待ちの列(PR #167レビューL-1、ADR §2.4-4)。
///
/// 以前は、各スレッドがapplyの後ロックを外してから自分でコールバックを呼んでいたので、別スレッドの
/// applyがその隙間に割り込むと配信順がapply順と逆転しえた(例: セッションtaskの`on_connected`が
/// `Connected`/`Established(g)`を公開する直前に、別スレッドのnetwork-lost debounceが`Lost(g)`を先に
/// 公開する。エッジはレベルでなくエッジなので、逆転したまま次のエッジまで自己修正されない)。
///
/// いまは**applyと同じ臨界区間で**この列に積み([`apply_reconnect`]/[`stage_publications`]、§2.4-2の
/// in-lock effect)、配信は[`flush_publications`]が**同時に1スレッドだけ**、ロックを持たずに先頭から順に行う
/// (`draining`)。よって`on_connection_state_changed`/`on_connection_edge`の配信順は、それらを生んだ
/// reducerの遷移の適用順と常に一致する(スレッドをまたいでも)。配信中のコールバックが同期的に別の公開を
/// 起こしても、それは列に積まれて同じ配信者が後で配る(非再入の`parking_lot::Mutex`でデッドロックしない)。
#[derive(Default)]
struct PublicationQueue {
    queue: VecDeque<Publication>,
    /// あるスレッドが[`flush_publications`]で配信中か。
    draining: bool,
}

/// `s.reconnect`にEventを1つ適用し、その出力のうち状態公開・接続エッジを**同じ臨界区間で**
/// 配信待ちの列に積む(§2.4-2のin-lock effect)。返したEffect列はロック解放後に
/// [`execute_reconnect_effects`]で解釈する。`ReconnectState::apply`を本番コードで呼ぶのは
/// これと`handle_unexpected_disconnect`/`begin_connect`(applyと積む間にin-lockの処理を挟む)だけ。
fn apply_reconnect(s: &mut OrchestratorState, ev: ReconnectEvent, ctx: &EffectContext<'_>) -> Vec<ReconnectEffect> {
    let effects = s.reconnect.apply(ev);
    stage_publications(s, &effects, ctx);
    effects
}

/// [`ReconnectEffect`]のin-lock interpreter(ADR §2.4-2 / §3-8): 状態公開・接続エッジを、applyと
/// 同じ臨界区間で配信待ちの列に積む。Effect列は消費しない(公開系のEffectは
/// [`execute_reconnect_effects`]で「ここまでに積んだ分を配信する」点として残る)。
/// `Connected{host}`/`Established{host}`のhostはここで`current_target()`から解決する。
#[deny(clippy::wildcard_enum_match_arm)]
fn stage_publications(s: &mut OrchestratorState, effects: &[ReconnectEffect], ctx: &EffectContext<'_>) {
    for effect in effects {
        let publication = match effect {
            ReconnectEffect::PublishDisconnected { issue_hint } => {
                Publication::State(ConnectionPublicState::Disconnected { reason: ctx.reason.clone(), issue_hint: *issue_hint })
            }
            ReconnectEffect::PublishConnected => Publication::State(ConnectionPublicState::Connected {
                host: s.current_target().map(|(host, _, _)| host).unwrap_or_default(),
            }),
            ReconnectEffect::PublishConnecting => Publication::State(ConnectionPublicState::Connecting),
            ReconnectEffect::PublishReconnecting { elapsed_secs, timeout_secs } => {
                Publication::State(ConnectionPublicState::Reconnecting {
                    elapsed_secs: *elapsed_secs,
                    timeout_secs: *timeout_secs,
                    reason: ctx.reason.clone(),
                })
            }
            ReconnectEffect::PublishReconnectTimedOut { timeout_secs } => {
                log::warn!("orchestrator: reconnect loop gave up after {timeout_secs}s");
                Publication::State(ConnectionPublicState::Disconnected {
                    reason: Some(format!(
                        "reconnect timed out after {timeout_secs}s (last: {})",
                        ctx.reason.clone().unwrap_or_else(|| "unknown".to_string())
                    )),
                    issue_hint: classify_disconnect_issue_hint(ctx.loop_attempt),
                })
            }
            // #175: `upstream_failover`もhostと同じく、applyと同じ臨界区間で`last_connect_attempt`から
            // 解決する(自動再接続・フォアグラウンド復帰の世代も同じ設定で張り直すので、どの世代の
            // `Established`にも同じ判断が載る。秘密を含むConfigはreducerに載せない、ADR §3-3)。
            ReconnectEffect::EdgeEstablished { generation } => Publication::Edge(
                crate::ConnectionEdge::Established {
                    host: s.current_target().map(|(host, _, _)| host).unwrap_or_default(),
                    upstream_failover: s
                        .last_connect_attempt
                        .as_ref()
                        .is_some_and(LastConnectAttempt::wants_upstream_failover_monitor),
                },
                *generation,
            ),
            ReconnectEffect::EdgeLost { generation } => Publication::Edge(crate::ConnectionEdge::Lost, *generation),
            ReconnectEffect::InvalidatePathObserver
            | ReconnectEffect::WakeReconnectLoop
            | ReconnectEffect::StartReconnectLoop { .. }
            | ReconnectEffect::ManualConnectRejected
            | ReconnectEffect::StartAttempt { .. }
            | ReconnectEffect::PendingWakeRecorded { .. }
            | ReconnectEffect::ArmLoopTimer { .. }
            | ReconnectEffect::LoopWokeEarly { .. }
            | ReconnectEffect::LoopTicked { .. } => continue,
        };
        s.publications.queue.push_back(publication);
    }
}

/// 配信待ちの列([`PublicationQueue`])を、ロックを持たずに先頭から順に配信する。既に別のスレッドが
/// 配信中なら何もしない(そのスレッドが今積まれた分まで配る)。コールバックがpanicしても`draining`は
/// 戻すので、以後の公開が止まったままにはならない。
fn flush_publications(shared: &OrchestratorShared) {
    {
        let mut s = shared.state.lock();
        if s.publications.draining || s.publications.queue.is_empty() {
            return;
        }
        s.publications.draining = true;
    }
    struct Draining<'a> {
        shared: &'a OrchestratorShared,
        armed: bool,
    }
    impl Drop for Draining<'_> {
        fn drop(&mut self) {
            if self.armed {
                self.shared.state.lock().publications.draining = false;
            }
        }
    }
    let mut guard = Draining { shared, armed: true };
    loop {
        let next = {
            let mut s = shared.state.lock();
            match s.publications.queue.pop_front() {
                Some(publication) => publication,
                None => {
                    // 「列が空」の確認と`draining`を下ろすのを同じ臨界区間で行う(間に積まれた分を取り残さない)。
                    s.publications.draining = false;
                    guard.armed = false;
                    return;
                }
            }
        };
        match next {
            Publication::State(state) => shared.callback.on_connection_state_changed(state),
            Publication::Edge(edge, generation) => {
                let label = match edge {
                    crate::ConnectionEdge::Established { .. } => "established",
                    crate::ConnectionEdge::Lost => "lost",
                };
                crate::debug_reconnect::record(format!("connection_edge {label} generation={generation}"));
                shared.callback.on_connection_edge(edge, generation);
            }
        }
    }
}

/// [`ReconnectEffect`]のout-of-lock interpreter(ADR §2.4 / §3-8)。**ロック解放後に**、`apply`が返した
/// 順に解釈する。Effectは`match`の明示armでのみ消費し、黙って捨てない(Step 7a)。状態公開・接続エッジは
/// [`stage_publications`]が既に列に積んでいるので、その位置で[`flush_publications`]する。
/// 戻り値は再接続ループの次のタイマー(`ArmLoopTimer`の`after`)。`None`ならループtaskは終了する。
#[deny(clippy::wildcard_enum_match_arm)]
fn execute_reconnect_effects(
    shared: &Arc<OrchestratorShared>,
    effects: Vec<ReconnectEffect>,
    mut ctx: EffectContext<'_>,
) -> Option<Duration> {
    let mut loop_timer = None;
    for effect in effects {
        match effect {
            ReconnectEffect::InvalidatePathObserver => {
                shared.path_observer.lock().invalidate();
            }
            ReconnectEffect::WakeReconnectLoop => {
                crate::debug_reconnect::record("pending_wake notify_after_in_flight_result");
                shared.reconnect_wake.notify_one();
            }
            ReconnectEffect::StartReconnectLoop { attempt, epoch } => {
                let resolved = ctx.resolved_attempt.take().filter(|(resolved_ref, _)| *resolved_ref == attempt);
                if let Some((_, resolved)) = resolved {
                    spawn_reconnect_loop(shared.clone(), resolved, ctx.reason.clone(), epoch);
                } else {
                    // `set_last_connect_attempt`の不変条件が破れない限り到達しない。到達した
                    // 場合もループフラグを立てたまま放置せず(always-connects.md)、ループ無しの
                    // 切断として扱う(判断は`ReconnectEvent::LoopStartAborted`のapply、Step 3aレビューm2)。
                    debug_assert!(false, "StartReconnectLoop: AttemptRef {attempt:?} could not be resolved");
                    log::error!("orchestrator: reconnect attempt {attempt:?} could not be resolved; not starting the loop");
                    let abort_ctx = EffectContext { reason: ctx.reason.clone(), ..EffectContext::default() };
                    let effects = apply_reconnect(&mut shared.state.lock(), ReconnectEvent::LoopStartAborted { epoch }, &abort_ctx);
                    execute_reconnect_effects(shared, effects, abort_ctx);
                }
            }
            ReconnectEffect::PublishDisconnected { .. }
            | ReconnectEffect::PublishConnected
            | ReconnectEffect::PublishConnecting
            | ReconnectEffect::PublishReconnecting { .. }
            | ReconnectEffect::PublishReconnectTimedOut { .. }
            | ReconnectEffect::EdgeEstablished { .. }
            | ReconnectEffect::EdgeLost { .. } => {
                flush_publications(shared);
            }
            ReconnectEffect::ManualConnectRejected => {
                // `begin_connect`はこのEffectを見たら解釈せずに`Err`を返すので、ここへは来ない。
                debug_assert!(false, "ManualConnectRejected must be handled by begin_connect");
                log::error!("orchestrator: ManualConnectRejected reached the interpreter; ignoring");
            }
            ReconnectEffect::StartAttempt { epoch, source } => {
                if let Some(attempt) = ctx.loop_attempt {
                    run_reconnect_attempt(shared, attempt, epoch, source);
                } else {
                    // `StartAttempt`は再接続ループのwake/tickからしか出ない(ループは常に
                    // `loop_attempt`を渡す)。
                    debug_assert!(false, "StartAttempt without a loop attempt");
                    log::error!("orchestrator: StartAttempt outside the reconnect loop; releasing the in-flight guard");
                    let effects = apply_reconnect(
                        &mut shared.state.lock(),
                        ReconnectEvent::AttemptFailedSync { epoch },
                        &EffectContext::default(),
                    );
                    execute_reconnect_effects(shared, effects, EffectContext::default());
                }
            }
            ReconnectEffect::PendingWakeRecorded { epoch } => {
                crate::debug_reconnect::record(format!("pending_wake set epoch={epoch}"));
            }
            ReconnectEffect::ArmLoopTimer { epoch, after, elapsed_secs } => {
                // `ArmLoopTimer`は再接続ループのEvent(`LoopStarted`/wake/tick)からしか出ない。
                debug_assert!(ctx.loop_attempt.is_some(), "ArmLoopTimer outside the reconnect loop");
                if crate::debug_reconnect::is_enabled() {
                    crate::debug_reconnect::record(format!(
                        "spawn_reconnect_loop tick_wait epoch={epoch} elapsed_secs={elapsed_secs}"
                    ));
                }
                loop_timer = Some(after);
            }
            ReconnectEffect::LoopWokeEarly { epoch } => {
                if crate::debug_reconnect::is_enabled() {
                    crate::debug_reconnect::record(format!("loop_woke_early epoch={epoch}"));
                }
            }
            ReconnectEffect::LoopTicked { epoch, tick_count, elapsed_secs } => {
                if crate::debug_reconnect::is_enabled() {
                    crate::debug_reconnect::record(format!(
                        "spawn_reconnect_loop tick epoch={epoch} tick_count={tick_count} elapsed_secs={elapsed_secs}"
                    ));
                }
            }
        }
    }
    // 公開系のEffectを伴わないapply(例: 別スレッドが積んだ分の配信者が終わった直後)でも取り残さない。
    flush_publications(shared);
    loop_timer
}

/// 再接続ループの1回の試行(`reconnect_attempt`、既定は`connect_via`)を実行する。
/// `retry_attempt_in_flight`は`StartAttempt`を返したapplyで既に立っている。同期的に
/// 失敗した場合は[`ReconnectEvent::AttemptFailedSync`]を新しいapplyとして戻す(§2.4-3)。
fn run_reconnect_attempt(
    shared: &Arc<OrchestratorShared>,
    attempt: &LastConnectAttempt,
    epoch: u64,
    source: AttemptSource,
) {
    let (source_label, error_source_label) = match source {
        AttemptSource::Tick => ("tick", "tick"),
        AttemptSource::PendingWake => ("pending_wake", "tick"),
        AttemptSource::NetworkWake => ("network_wake", "network_wake"),
    };
    crate::debug_reconnect::record(format!(
        "retry_attempt_in_flight on epoch={} source={}",
        epoch,
        source_label
    ));
    if source == AttemptSource::NetworkWake {
        log::info!(
            "orchestrator: network path restored while reconnecting; retrying immediately instead of waiting out the rest of this tick"
        );
    }
    match (shared.reconnect_attempt)(shared, attempt.clone()) {
        Ok(()) => {}
        Err(e) => {
            log::warn!("orchestrator: reconnect attempt failed synchronously: {e:?}");
            let (matched, effects) = {
                let mut s = shared.state.lock();
                let matched = s.reconnect.reconnect_epoch == epoch;
                (matched, apply_reconnect(&mut s, ReconnectEvent::AttemptFailedSync { epoch }, &EffectContext::default()))
            };
            if matched {
                crate::debug_reconnect::record(format!(
                    "retry_attempt_in_flight off epoch={} result=sync_error source={} error={:?}",
                    epoch,
                    error_source_label,
                    e
                ));
            }
            execute_reconnect_effects(shared, effects, EffectContext::default());
        }
    }
}

/// リトライ専用のセッション生成。`begin_connect()`(手動接続の開始、`Connecting`通知・
/// `reconnect_epoch`無効化を伴う)とは別関数にしてある — リトライのたびに`begin_connect()`
/// を呼ぶと、リトライループ自身の`reconnect_epoch`を無効化してしまい自己終了してしまう。
///
/// `last_connect_attempt`はここでは書き換えない —— この関数は常にその
/// `last_connect_attempt`自身のcloneを渡されて呼ばれる(`spawn_reconnect_loop`/
/// `notify_will_enter_foreground`)ので書き戻しても同じ値になるはずだが、
/// ループがattemptをcloneしてから実際に発火するまでの間に手動接続が
/// `last_connect_attempt`を更新していた場合、古い値で上書きし返してしまう。
/// 「接続先のSSOTは`last_connect_attempt`」という原則の下では、その唯一の
/// 書き手は手動接続(`begin_connect`)だけにしておくのが安全。
///
/// 物理マルチパスの生fdは最初のセッションが引き取り済みなので外してから張り直す
/// ([`LastConnectAttempt::for_reconnect`]、#175)。
fn connect_via(shared: &Arc<OrchestratorShared>, attempt: LastConnectAttempt) -> Result<(), SshError> {
    let adapter = begin_reconnect_session(shared);
    build_and_store_session(shared, attempt.for_reconnect(), adapter)
}

/// [`connect_via`]のうち、セッション生成より前の「phaseを`Connecting`へ動かし、新しい世代の
/// アダプタを作る」部分(ADR §6 Step 8a′)。phase遷移は[`ReconnectEvent::ReconnectSessionStarting`]
/// のapplyで行い、Connectedのまま入った場合(フォアグラウンド復帰)はそのapplyが`Lost(old)`を返すので、
/// ロック解放後・新しい世代へ進む前に公開する。この経路は状態公開(`Connecting`)を伴わない
/// (既存の挙動、ADR m-R4-4)。テストのフェイク`reconnect_attempt`も、実接続をせずに本番と同じ
/// 遷移を踏むためにこれを呼ぶ。
fn begin_reconnect_session(shared: &Arc<OrchestratorShared>) -> OrchestratorAdapter {
    let effects =
        apply_reconnect(&mut shared.state.lock(), ReconnectEvent::ReconnectSessionStarting, &EffectContext::default());
    execute_reconnect_effects(shared, effects, EffectContext::default());
    OrchestratorAdapter::new(shared.clone())
}

/// `attempt`が指すトランスポートのセッションを1つ生成・接続し、成功したら
/// `shared.session`へ格納する。手動接続([`SessionOrchestrator::start_manual_connect`])と
/// 自動再接続([`connect_via`])の両方がここを通るため、**トランスポート分岐の
/// matchはこの1箇所だけ**になる —— 以前は`connect_via`と7つの`connect_*`が
/// 同じ「セッション生成→connect→`ActiveSession`格納」を二重に持っており、
/// 新しいトランスポートを足すたびに2つのリストを同期させる必要があった。
///
/// `adapter`を呼び出し側から受け取るのは、[`OrchestratorAdapter::new`]が
/// `session_generation`をインクリメントする(=古いセッションからの遅延
/// コールバックをそこで一斉に無効化する)副作用を持つため —— そのタイミングは
/// 呼び出し側の手順(手動接続なら`begin_connect`のstate更新+`Connecting`通知の
/// 直後)に合わせる必要があり、この関数の中へ動かしてよいものではない。
///
/// `connect`が`Err`を返した場合は`shared.session`を書き換えないまま伝播する
/// (直前のセッションを壊さない)。
fn build_and_store_session(
    shared: &Arc<OrchestratorShared>,
    attempt: LastConnectAttempt,
    adapter: OrchestratorAdapter,
) -> Result<(), SshError> {
    let session = match attempt {
        LastConnectAttempt::Ssh(config) => {
            let session = crate::create_ssh_session(config);
            // タスク#59: このタブの安定識別子を渡す(`OrchestratorShared::app_pane_id`
            // 参照)。プレーンSSH経路(TCP直結・踏み台・QUICネスト共通の
            // `run_ssh_channel_loop`)のctl-socket forwardが、確立/再接続の
            // たびにこのIDでtmuxロケータレジストリを引く。
            session.connect(Box::new(adapter), shared.app_pane_id.clone())?;
            ActiveSession::Ssh(session)
        }
        LastConnectAttempt::Quic(config) => {
            let session = crate::quic_transport::create_quic_session(config);
            session.connect(Box::new(adapter))?;
            ActiveSession::Quic(session)
        }
        LastConnectAttempt::IsekaiPipeQuic(config) => {
            let session = crate::isekai_pipe_quic_transport::create_isekai_pipe_quic_session(config);
            session.connect(Box::new(adapter), shared.app_pane_id.clone())?;
            ActiveSession::IsekaiPipeQuic(session)
        }
        LastConnectAttempt::IsekaiPipeQuicAuto(config) => {
            let session = crate::isekai_pipe_quic_transport::create_isekai_pipe_quic_session(config);
            session.connect_auto(Box::new(adapter), shared.app_pane_id.clone())?;
            ActiveSession::IsekaiPipeQuic(session)
        }
        LastConnectAttempt::MultipathIsekaiPipeQuic(config) => {
            let session = crate::multipath_transport::create_multipath_isekai_pipe_quic_session(config);
            session.connect(Box::new(adapter))?;
            ActiveSession::MultipathIsekaiPipeQuic(session)
        }
        LastConnectAttempt::IsekaiStunP2p(config) => {
            let session = crate::isekai_stun_p2p_transport::create_isekai_stun_p2p_session(config);
            session.connect(Box::new(adapter))?;
            ActiveSession::IsekaiStunP2p(session)
        }
        LastConnectAttempt::IsekaiLinkRelay(config) => {
            let session = crate::isekai_link_relay_transport::create_isekai_link_relay_session(config);
            session.connect(Box::new(adapter))?;
            ActiveSession::IsekaiLinkRelay(session)
        }
    };
    *shared.session.lock() = Some(session);
    Ok(())
}

/// `spawn_reconnect_loop`の1回分のタイマー待ち。`after`(reducerが`ArmLoopTimer`で指定した長さ)を
/// 素通しで待つのと、`wake`(`OrchestratorShared::reconnect_wake`)がネットワーク復帰通知で
/// 起こされるのをレースさせる — 戻り値は「`wake`側で早期に起きたか」。
/// 早期に起きた場合、reducer(`ReconnectEvent::ReconnectWake`)は`elapsed`/`tick_count`の通常の会計には
/// 一切触れずに「今すぐ1回試す」ボーナス試行だけ行い、次のタイマーを満額で設定し直す
/// (isekai-pipe側`resume_loop::wait_backoff_or_network_change`と同じ「バックオフ待機とOS通知を
/// レースさせる」発想を、elapsed/timeoutの会計を一切歪めない形で移植したもの)。
async fn sleep_tick_or_network_restored(after: Duration, wake: &tokio::sync::Notify) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(after) => false,
        _ = wake.notified() => true,
    }
}

/// 自動再接続ループ本体。`shared.rt`(本番はグローバル`RUNTIME`)へspawnされたtokio task。tsshのUDPモード
/// reconnectと同じく、1秒ごとに`Reconnecting`をライブ通知しつつ、
/// `retry_interval`ごとに実際の再接続(`connect_via`)を試みる。
/// `retry_attempt_in_flight`により、1回の試行の結果(成功/失敗)が判明するまで
/// 次の試行を重ねて発火しない(ホスト鍵確認プロンプトの多重発生を防ぐ)。
///
/// 通常のtickに加え、`shared.reconnect_wake`(`notify_network_path_changed`の
/// `ConnPhase::Idle`分岐がネットワーク復帰時に鳴らす)で早期に起こされた場合は
/// `retry_interval`のcadenceを待たず、その場で1回だけボーナス試行する
/// (`sleep_tick_or_network_restored`参照)。
///
/// ADR §6 Step 3b/3c: このtaskは§2.2-1のタイマーの**shellだけ**を担う。tick会計(`elapsed`/`tick_count`)・
/// `due`の計算・`woke_early`分岐・タイムアウトによるギブアップ・epochによる自己終了の判断はすべて
/// `ReconnectState::apply`([`ReconnectEvent::LoopStarted`]/[`ReconnectEvent::ReconnectTick`]/
/// [`ReconnectEvent::ReconnectWake`])が行い、taskは返ってきた`ArmLoopTimer`の`after`だけ待って次の
/// Eventを戻す。`ArmLoopTimer`が返らなければ(別の何かに主導権が移った・ギブアップした)静かに終了する。
fn spawn_reconnect_loop(
    shared: Arc<OrchestratorShared>,
    attempt: LastConnectAttempt,
    reason: Option<String>,
    epoch: u64,
) {
    let rt = shared.rt.clone();
    rt.spawn(async move {
        // spawnされてから走り始めるまでの間に、既に別の何か(即座の手動再接続・cancel_reconnect等)に
        // 主導権が移っていた場合、reducerは初回のReconnecting通知すら返さず、taskはここで終了する。
        let mut next = run_reconnect_loop_step(&shared, &attempt, &reason, |policy| ReconnectEvent::LoopStarted {
            epoch,
            policy,
        });
        while let Some(after) = next {
            let woke_early = sleep_tick_or_network_restored(after, &shared.reconnect_wake).await;
            next = run_reconnect_loop_step(&shared, &attempt, &reason, |policy| {
                if woke_early {
                    ReconnectEvent::ReconnectWake { epoch, policy }
                } else {
                    ReconnectEvent::ReconnectTick { epoch, policy }
                }
            });
        }
    });
}

/// 再接続ループの1ステップ: ロックを取り、その臨界区間で読んだ現在のポリシー(`debug_set_reconnect_policy`の
/// 即時反映のため毎回読み直す)を載せたEventをapplyし(ADR §2.2の`apply_with`形。クロージャはEventを
/// 組み立てるだけ)、ロック解放後に解釈する。戻り値は次のタイマー(`None`ならループ終了)。
fn run_reconnect_loop_step(
    shared: &Arc<OrchestratorShared>,
    attempt: &LastConnectAttempt,
    reason: &Option<String>,
    make_event: impl FnOnce(ReconnectPolicy) -> ReconnectEvent,
) -> Option<Duration> {
    let ctx = EffectContext { reason: reason.clone(), loop_attempt: Some(attempt), ..EffectContext::default() };
    let effects = {
        let mut s = shared.state.lock();
        let policy = s.reconnect_policy;
        apply_reconnect(&mut s, make_event(policy), &ctx)
    };
    execute_reconnect_effects(shared, effects, ctx)
}

// ── SessionOrchestrator ───────────────────────────────────

#[derive(uniffi::Object)]
pub struct SessionOrchestrator {
    shared: Arc<OrchestratorShared>,
}

#[uniffi::export]
pub fn create_session_orchestrator(callback: Box<dyn OrchestratorCallback>) -> Arc<SessionOrchestrator> {
    crate::init_logger();
    let reconnect_policy = crate::debug_reconnect::reconnect_policy_override().unwrap_or_default();
    let shared = Arc::new(OrchestratorShared {
        state: Mutex::new(OrchestratorState {
            reconnect: ReconnectState::default(),
            publications: PublicationQueue::default(),
            current_transfer_id: None,
            trzsz_mode: None,
            download_buf: Vec::new(),
            size_limit_exceeded_for: None,
            pending_file_previews: HashMap::new(),
            session_generation: 0,
            last_connect_attempt: None,
            reconnect_policy,
            tab_focused: false,
            app_foreground: true,
            recent_notify_seqs: std::collections::VecDeque::new(),
        }),
        callback: Arc::from(callback),
        session: Mutex::new(None),
        path_observer: Mutex::new(crate::net_health_policy::PathObserver::default()),
        app_pane_id: crate::tmux_locator::AppPaneId::generate_process_local(),
        reconnect_attempt: Box::new(connect_via),
        reconnect_wake: tokio::sync::Notify::new(),
        rt: RUNTIME.handle().clone(),
    });
    let orchestrator = Arc::new(SessionOrchestrator { shared });
    crate::debug_reconnect::register_orchestrator(&orchestrator);
    orchestrator
}

impl SessionOrchestrator {
    /// 各`connect_*`が共通で行う「state更新→Connecting通知→adapter生成」を
    /// 一箇所にまとめる。実際のsession生成・接続・`ActiveSession`格納は、
    /// 自動再接続経路と共有する[`build_and_store_session`]が引き受ける
    /// (呼び出し元は[`SessionOrchestrator::start_manual_connect`])。
    ///
    /// phaseが既に`Connecting`(=前の`connect_*`呼び出しがまだ実行中)の間の新規呼び出しは
    /// 拒否する(真の二重start防止、Task #9)。`Connected`中の呼び出しは意図的に許可する
    /// ——「保留中のnetwork-path debounceをキャンセルしつつ別セッションへ手動で切り替える」
    /// 正当な経路であり(下記invalidate呼び出し、および
    /// `notify_network_path_changed_pending_debounce_is_cancelled_by_a_new_connect_attempt`
    /// テスト参照)、`Idle`と同様に受理してよい。
    ///
    /// 判断(拒否するか・どのフィールドを書くか・Connectedからの切り替えなら旧世代の`Lost`)は
    /// [`ReconnectEvent::ManualConnectStarted`]のapplyが行う(ADR §6 Step 8a′)。返ったEffect
    /// (`Lost(old)`→path_observerの無効化→`Connecting`公開)はロック解放後に、新しい世代の
    /// アダプタを作る前に解釈する(旧世代の`Lost`が新しい世代より先に届く)。
    fn begin_connect(&self, attempt: LastConnectAttempt) -> Result<OrchestratorAdapter, SshError> {
        let effects = {
            let mut s = self.shared.state.lock();
            let attempt_ref = AttemptRef::next_after(s.reconnect.last_attempt);
            let effects = s.reconnect.apply(ReconnectEvent::ManualConnectStarted { attempt: attempt_ref });
            // NOTE: 拒否された場合のEffect列は`ManualConnectRejected`だけ(公開するものは無い)なので、
            // 覗いて`Err`を返しても何も捨てない(登録interpreterの外でのpeek、ADR §3-8)。
            if effects.contains(&ReconnectEffect::ManualConnectRejected) {
                return Err(SshError::ConnectionFailed);
            }
            // 接続先(host/port/QUIC種別)と再接続用のConfigは同じ1つの
            // `last_connect_attempt`が担う。reducer側の参照(`last_attempt`)と同じ臨界区間で
            // 書く(ロックを一度解放した後に書くと、その間だけ両者が食い違って見え得る)。
            s.last_connect_attempt = Some(attempt);
            stage_publications(&mut s, &effects, &EffectContext::default());
            effects
        };
        execute_reconnect_effects(&self.shared, effects, EffectContext::default());
        Ok(OrchestratorAdapter::new(self.shared.clone()))
    }

    /// すべての手動接続(`connect_*`)の共通実装。`begin_connect`で状態遷移と
    /// `Connecting`通知を済ませてから、[`build_and_store_session`]で実際の
    /// セッションを生成する。トランスポートごとに違うのは`LastConnectAttempt`の
    /// バリアントだけなので、各`connect_*`はこれを1回呼ぶだけの薄いUniFFI入口に
    /// なる(生成手順そのものは自動再接続経路[`connect_via`]と共有する)。
    fn start_manual_connect(&self, attempt: LastConnectAttempt) -> Result<(), SshError> {
        let adapter = self.begin_connect(attempt.clone())?;
        build_and_store_session(&self.shared, attempt, adapter)
    }

    /// Android実機スパイク用: `debug_set_reconnect_policy`/`debug_clear_reconnect_policy`
    /// (`debug_reconnect.rs`)が、生きている全orchestratorへ即座に反映するために呼ぶ。
    /// UniFFI経由では公開しない(呼び出し口はRust側のレジストリのみ、
    /// `SessionOrchestratorInterface`を肥大化させてKotlin側のFakeOrchestrator実装
    /// (テスト専用)を壊さないため)。
    pub(crate) fn apply_reconnect_policy_override(&self) {
        self.shared.state.lock().reconnect_policy =
            crate::debug_reconnect::reconnect_policy_override().unwrap_or_default();
        // `spawn_reconnect_loop`が古いtick長のままsleep中の場合、これを起こさないと
        // 新しいポリシーは現在のsleepが自然に終わるまで反映されない
        // (code-reviewで発見、このメソッドのドキュメント上の「次のtickを待たず
        // 即座に反映する」という約束を守るために必須)。
        self.shared.reconnect_wake.notify_one();
    }
}

#[uniffi::export]
impl SessionOrchestrator {
    pub fn connect(&self, config: SshConfig) -> Result<(), SshError> {
        self.start_manual_connect(LastConnectAttempt::Ssh(config))
    }

    pub fn connect_quic(&self, config: QuicConfig) -> Result<(), SshError> {
        self.start_manual_connect(LastConnectAttempt::Quic(config))
    }

    /// Phase 7: 自作ヘルパー（isekai-helper）経由の QUIC 接続。フォールバック無し
    /// （`TransportPreference::IsekaiPipeQuic` 相当、明示選択時に使う）。
    pub fn connect_isekai_pipe_quic(&self, config: IsekaiPipeQuicConfig) -> Result<(), SshError> {
        self.start_manual_connect(LastConnectAttempt::IsekaiPipeQuic(config))
    }

    /// Phase 7: `TransportPreference::Auto` 相当。自作ヘルパー経由 QUIC のブートストラップ/
    /// 接続に失敗した場合、内部で自動的に通常の TCP SSH にフォールバックする。
    pub fn connect_isekai_pipe_quic_auto(&self, config: IsekaiPipeQuicConfig) -> Result<(), SshError> {
        self.start_manual_connect(LastConnectAttempt::IsekaiPipeQuicAuto(config))
    }

    /// Phase 9: `TransportPreference::IsekaiPipeQuicMultipath` 相当。フォールバック無し。
    /// `config.direct_host` が設定されていれば path0（`ssh_host`）+ path1（`direct_host`）の
    /// 受動的マルチパスで接続する。
    pub fn connect_multipath_isekai_pipe_quic(&self, config: MultipathIsekaiPipeQuicConfig) -> Result<(), SshError> {
        self.start_manual_connect(LastConnectAttempt::MultipathIsekaiPipeQuic(config))
    }

    /// Phase 10: `TransportPreference::IsekaiStunP2pQuic` 相当。relay 無し・
    /// STUN+SSH rendezvousによる直接 P2P QUIC。フォールバック無し（穴あけ不成立時は
    /// 接続失敗として扱う。`isekai_stun_p2p_transport.rs` 参照）。
    pub fn connect_isekai_stun_p2p(&self, config: IsekaiStunP2pConfig) -> Result<(), SshError> {
        self.start_manual_connect(LastConnectAttempt::IsekaiStunP2p(config))
    }

    /// Phase 10: `TransportPreference::IsekaiLinkRelayQuic` 相当。MASQUE relay 経由の
    /// P2P QUIC。フォールバック無し（`isekai_link_relay_transport.rs` 参照）。
    pub fn connect_isekai_link_relay(&self, config: IsekaiLinkRelayConfig) -> Result<(), SshError> {
        self.start_manual_connect(LastConnectAttempt::IsekaiLinkRelay(config))
    }

    pub fn disconnect(&self) {
        // 「これから来る`on_disconnected`はユーザー操作起因」の印を先に立てておく
        // (実際の切断はこの後`s.disconnect()`が非同期にコールバックを発火させる)。
        self.shared.state.lock().reconnect.user_initiated_disconnect = true;
        if let Some(s) = self.shared.session.lock().as_ref() {
            s.disconnect();
        }
    }

    /// 自動再接続ループを中止する。ループが動作中だった場合のみ`Disconnected`を
    /// 通知する(動いていない時に呼ばれても無音、UIは`isReconnecting`の間だけ
    /// 「中止」操作を出す想定)。
    pub fn cancel_reconnect(&self) {
        // 判断(epochを進めてループを止め、動作中だった場合だけ`Disconnected`)は
        // `ReconnectEvent::CancelReconnect`のapply(ADR §6 Step 3b/3c)。
        let ctx = EffectContext { reason: Some("reconnect cancelled by user".to_string()), ..EffectContext::default() };
        let effects = apply_reconnect(&mut self.shared.state.lock(), ReconnectEvent::CancelReconnect, &ctx);
        execute_reconnect_effects(&self.shared, effects, ctx);
    }

    // ── #20: バックグラウンド/フォアグラウンド遷移 ─────────────
    //
    // Kotlin/Swiftはこの4メソッドへOS由来の生イベントをそのまま転送するだけでよい
    // (`rust-ssot.md`)。「今すぐ再接続すべきか」の判断・実行は全てRust側(以下)が担う。

    /// アプリがバックグラウンドへ遷移した(iOSの`UIApplication.didEnterBackground`/
    /// Androidの`ProcessLifecycleOwner.onStop`相当)ことを通知する。`budget_ms`は
    /// `beginBackgroundTask`等が保証する猶予の目安として記録目的で受け取るが、
    /// 実際の期限管理(タイマー)はSwift/Kotlin側の責務のままにする(Rust/Swiftで
    /// 基準時計を共有していないため)。`Connected`または`Connecting`中のみ猶予追跡を
    /// 開始する(`Idle`は維持すべきセッションが無いので無視。`Connecting`中に
    /// バックグラウンド化し、その猶予中に接続が成立するケース(`on_connected()`
    /// 自体はこの状態に触れない)もカバーする必要があるため`Connecting`も対象に含める)。
    pub fn notify_did_enter_background(&self, _budget_ms: u32) {
        let mut s = self.shared.state.lock();
        // `app_foreground`は`phase`に関係なく無条件で更新する生の事実
        // (`tab_focused`と同じ扱い、上のフィールドdoc参照)。
        s.app_foreground = false;
        // #20 codexレビュー指摘: `Connecting`中にバックグラウンド化し、その猶予中に
        // 接続が成立するケース(`on_connected()`は`background_state`に触れない)も
        // 追跡対象に含める。`Idle`(そもそも維持すべきセッションが無い)は対象外のまま。
        if s.reconnect.phase == ConnPhase::Connected || s.reconnect.phase == ConnPhase::Connecting {
            s.reconnect.background_state = BackgroundState::Quiescing;
        }
    }

    /// バックグラウンド猶予が尽きた(`beginBackgroundTask`失効等)ことを通知する。
    /// 猶予追跡中(`Quiescing`)の場合のみ、次のフォアグラウンド復帰時に再接続が
    /// 必要な状態(`Suspended`)へ遷移する。
    pub fn notify_background_budget_expired(&self) {
        let mut s = self.shared.state.lock();
        if s.reconnect.background_state == BackgroundState::Quiescing {
            s.reconnect.background_state = BackgroundState::Suspended;
        }
    }

    /// メモリ逼迫警告(iOSの`didReceiveMemoryWarning`相当)。OSにプロセスを終了
    /// される可能性が高まったとみなし、猶予を待たず保守的に`Suspended`扱いにする
    /// (無言で固まった画面をユーザーに見せるより、次回復帰時に再接続する方が安全)。
    pub fn notify_memory_warning(&self) {
        let mut s = self.shared.state.lock();
        if s.reconnect.background_state == BackgroundState::Quiescing {
            s.reconnect.background_state = BackgroundState::Suspended;
        }
    }

    /// アプリがフォアグラウンドへ復帰した(iOSの`willEnterForeground`/Androidの
    /// `onStart`相当)ことを通知する。`Suspended`だった場合のみ、直前の接続設定
    /// (`last_connect_attempt`)で自動的に再接続を試みる(Kotlin/Swiftはこの生
    /// イベントを送るだけでよく、再接続要否の判断はしない)。既に自動再接続ループが
    /// 動作中、または他の接続試行が進行中の場合は二重に開始しない。`Quiescing`
    /// (猶予内復帰、接続は生きている前提)や`Foreground`(そもそも追跡対象外)では
    /// 何もしない。
    pub fn notify_will_enter_foreground(&self) {
        let (should_notify, did_reconnect, reconnect_with) = {
            let mut s = self.shared.state.lock();
            // `app_foreground`は`phase`/`background_state`に関係なく無条件で更新する
            // 生の事実(`notify_did_enter_background`と対称、上のフィールドdoc参照)。
            s.app_foreground = true;
            let was_foreground = s.reconnect.background_state == BackgroundState::Foreground;
            let was_suspended = s.reconnect.background_state == BackgroundState::Suspended;
            s.reconnect.background_state = BackgroundState::Foreground;
            let reconnect_with = if was_suspended && !s.reconnect.reconnect_loop_active && s.reconnect.phase != ConnPhase::Connecting {
                s.last_connect_attempt.clone()
            } else {
                None
            };
            // round-3レビューS-1: `was_suspended`だけでは、`Quiescing`(猶予内)の
            // 間にバックグラウンドで接続が切れ`handle_unexpected_disconnect`の
            // `Action::StartLoop`/`Suppress`経路(`background_state`を書き換えない
            // 経路)に入ったケースを見落とす。`reconnect_loop_active`/`phase`も
            // 合わせて見ることで、「復帰時点で接続が生きていない」を
            // `background_state`の値に関わらず正しく判定する(B2と同型の穴の再発防止)。
            //
            // `s.reconnect.reconnect_loop_active`の項は、`handle_unexpected_disconnect`/
            // `on_connected`の現在の実装だけを見れば`s.reconnect.phase != ConnPhase::Connected`
            // に包含され厳密には冗長(コードレビューで指摘・実装時に検証済み:
            // `reconnect_loop_active`をtrueにする唯一の経路(`:781`)は
            // 必ずその前に`phase = Idle`を設定済み(`:773`)であり、`on_connected`は
            // `phase = Connected`と`reconnect_loop_active = false`を同一ロック内で
            // 常に対にして設定する(`:526-530`))。**意図的に残してある**——
            // この式はまさに「復帰時点で本当に接続が生きていないか」を保証する
            // ためのものなので、将来どちらかの不変条件が崩れても(例:
            // `phase`の更新漏れがあっても)もう一方の条件が独立に安全側へ倒れる
            // 二重の保険にする。「冗長だから」と`s.reconnect.phase != ConnPhase::Connected`
            // だけに簡約しないこと——B2/S-1が示すとおり、この関数はまさに
            // そうした「暗黙の不変条件への依存」が実害を生んだ場所である。
            let did_reconnect =
                was_suspended || s.reconnect.reconnect_loop_active || s.reconnect.phase != ConnPhase::Connected;
            (!was_foreground, did_reconnect, reconnect_with)
        };
        // コードレビュー指摘: ここは`self.shared.state`のロックを解放した後なので、
        // 別スレッドが同時に発火させる無関係なイベント(例: 別経路の
        // `notify_network_path_changed`→`handle_unexpected_disconnect`による
        // `on_connection_state_changed`)がこの`on_foreground_resume`より先に
        // コールバックへ届く可能性はゼロではない。`did_reconnect`のトレイトdoc
        // (`lib.rs`)が保証する発火順序は、**この呼び出し自身が同期的に引き起こす**
        // `reconnect_attempt`/その失敗時の`on_connection_state_changed`との相対順序
        // のみであり、無関係な別スレッド発のイベントとの順序までは保証しない。
        // Y-Rの時点ではSwift/Kotlin側はログのみ(実UIはY-P3)なのでこの窓は
        // 無害だが、Y-P3で実UIを配線する際はこの限界を踏まえること
        // (`TASKS_IOS_ADR_YR_IMPL_REVIEW.md`のコードレビュー追記を参照)。
        if should_notify {
            self.shared.callback.on_foreground_resume(did_reconnect);
        }
        if let Some(attempt) = reconnect_with {
            // #20 codexレビュー指摘: `reconnect_attempt`(`connect_via`)は`phase`を
            // `Connecting`にしてから同期的に失敗し得る(ホスト鍵確認拒否・設定不備等)。
            // 自動再接続ループ(`spawn_reconnect_loop`)内の失敗は次のtickで暗黙に
            // リトライされるが、こちらは一回限りの呼び出しなので`Err`を握り潰すと
            // `phase`が`Connecting`のまま固まり、UIが「接続中…」から進まなくなる。
            // ループ経由の再試行に頼らず、この場で`Idle`へ戻し失敗を通知する。
            match (self.shared.reconnect_attempt)(&self.shared, attempt) {
                Ok(()) => {}
                Err(e) => {
                    log::warn!("orchestrator: foreground resume reconnect failed synchronously: {e:?}");
                    let ctx = EffectContext {
                        reason: Some(format!("foreground resume reconnect failed: {e}")),
                        ..EffectContext::default()
                    };
                    let effects =
                        apply_reconnect(&mut self.shared.state.lock(), ReconnectEvent::ForegroundReconnectFailedSync, &ctx);
                    execute_reconnect_effects(&self.shared, effects, ctx);
                }
            }
        }
    }

    /// #11: ユーザーが「今すぐWiFiに戻す」操作を行った(セルラーにフェイルオーバー中、
    /// ダウンロード中などで静けさ待ちを待たずに即座に戻したい場合)。疎通確認だけは
    /// 省略されない(`RebindManager::handle_manual_force_return`参照)。マルチパス以外の
    /// transportや未接続時は何もしない。
    pub fn force_return_to_wifi(&self) {
        if let Some(s) = self.shared.session.lock().as_ref() {
            s.force_return_to_wifi();
        }
    }

    /// Android `UpstreamHealthMonitor`(ConnectivityManagerの`NET_CAPABILITY_VALIDATED`
    /// 喪失検知、Rust側のQUICパスヘルスとは無関係な独自シグナル)から、生イベントを
    /// そのまま転送するために呼ぶ。判断・rebind実行は一切せず`RebindManager`
    /// (`RebindEvent::UpstreamHealthDegraded`)へ委譲するだけ(`rust-ssot.md`準拠)。
    /// マルチパス以外のtransportや未接続時、`enableUpstreamFailover`が無効な場合は
    /// Rust側で無視される。
    pub fn notify_upstream_health_degraded(&self) {
        if let Some(s) = self.shared.session.lock().as_ref() {
            s.notify_upstream_health_degraded();
        }
    }

    pub fn send(&self, data: Vec<u8>) {
        if let Some(s) = self.shared.session.lock().as_ref() {
            s.send(data);
        }
    }

    pub fn resize(&self, cols: u32, rows: u32) {
        if let Some(s) = self.shared.session.lock().as_ref() {
            s.resize(cols, rows);
        }
    }

    /// #60: OSのフォーカス変化(タブ/split pane切替・アプリのbackground/foreground等)を
    /// そのまま転送する。Kotlin/Swiftはこの生イベントを渡すだけでよく、フォーカス
    /// レポーティング(`CSI ?1004`)が有効かどうか・実際に`CSI I`/`CSI O`を送るかどうかの
    /// 判断は`Terminal`(rust-ssot)が一元的に持つ。未接続時は無視される。
    ///
    /// タスク#57: `state.tab_focused`にも同じ値を複製する(新しいUniFFIメソッドを
    /// 増やすのではなく既存の生イベント転送を再利用する、`rust-ssot.md`)。
    /// `OrchestratorAdapter::on_notify`がこれと`background_state`を合わせて見て、
    /// tmux hook通知をAndroid通知として見せるか抑制するかを判断する——未接続時
    /// (`session`が無い)でも`tab_focused`自体は更新する(接続前後でタブの
    /// フォーカス状態は独立に変化し得るため)。
    pub fn notify_focus_change(&self, focused: bool) {
        self.shared.state.lock().tab_focused = focused;
        if let Some(s) = self.shared.session.lock().as_ref() {
            s.notify_focus_change(focused);
        }
    }

    pub fn scrollback_len(&self) -> u32 {
        self.shared.session.lock().as_ref().map_or(0, |s| s.scrollback_len())
    }

    pub fn scrollback_cells(&self, offset: u32, rows: u32) -> Vec<CellData> {
        self.shared.session.lock().as_ref()
            .map_or_else(Vec::new, |s| s.scrollback_cells(offset, rows))
    }

    /// scrollbackを対象にした部分一致検索(タスク#37)。マッチ位置は
    /// [ScrollbackSearchMatch]のドキュメント参照。未接続時は空Vecを返す。
    pub fn search_scrollback(&self, query: String, case_sensitive: bool) -> Vec<ScrollbackSearchMatch> {
        self.shared.session.lock().as_ref()
            .map_or_else(Vec::new, |s| s.search_scrollback(query, case_sensitive))
    }

    /// OSC 133(タスク#13)「前のプロンプトへジャンプ」。既存のスクロールバック検索
    /// (`search_scrollback`)とは独立した機能——`from_scroll_offset`/
    /// `from_showing_scrollback`はKotlin側が今表示している位置(タスク#79と同じ
    /// `scrollOffset`/`showingScrollback`の規約)をそのまま渡す。結果は
    /// `OrchestratorCallback::on_prompt_jump`で非同期に返る(未接続時は無視される)。
    pub fn jump_to_previous_prompt(&self, from_scroll_offset: u32, from_showing_scrollback: bool) {
        if let Some(s) = self.shared.session.lock().as_ref() {
            s.jump_to_previous_prompt(from_scroll_offset, from_showing_scrollback);
        }
    }

    /// [jump_to_previous_prompt]の「次」版。
    pub fn jump_to_next_prompt(&self, from_scroll_offset: u32, from_showing_scrollback: bool) {
        if let Some(s) = self.shared.session.lock().as_ref() {
            s.jump_to_next_prompt(from_scroll_offset, from_showing_scrollback);
        }
    }

    /// OSC 133(タスク#13): タップされたセル(画面座標、0-indexed)が現在アクティブな
    /// 入力行上であれば、そこへカーソルを移動する矢印キー相当のバイト列を送る
    /// (Ghostty`cl=line`相当)。対象外なら無音でno-op。未接続時も無視される。
    pub fn click_to_prompt_cursor(&self, row: u32, col: u32) {
        if let Some(s) = self.shared.session.lock().as_ref() {
            s.click_to_prompt_cursor(row, col);
        }
    }

    /// OSC 133(タスク#13)「直前コマンドの出力だけをコピー」。結果は
    /// `OrchestratorCallback::on_prompt_output_copy_ready`で非同期に返る
    /// (該当コマンドがまだ無ければ`None`、未接続時は無視される)。
    pub fn copy_last_command_output(&self) {
        if let Some(s) = self.shared.session.lock().as_ref() {
            s.copy_last_command_output();
        }
    }

    pub fn trzsz_accept_download(&self) {
        let tid = self.shared.state.lock().current_transfer_id.clone();
        if let Some(tid) = tid {
            if let Some(s) = self.shared.session.lock().as_ref() {
                s.trzsz_accept_download(tid);
            }
        }
    }

    pub fn trzsz_accept_upload(&self, file_name: String, file_size: u64, mode: u32) {
        let tid = self.shared.state.lock().current_transfer_id.clone();
        if let Some(tid) = tid {
            if let Some(s) = self.shared.session.lock().as_ref() {
                s.trzsz_accept_upload(tid, file_name, file_size, mode);
            }
        }
    }

    pub fn trzsz_send_chunk(&self, data: Vec<u8>, is_last: bool) {
        let tid = self.shared.state.lock().current_transfer_id.clone();
        if let Some(tid) = tid {
            if let Some(s) = self.shared.session.lock().as_ref() {
                s.trzsz_send_chunk(tid, data, is_last);
            }
        }
    }

    pub fn trzsz_cancel(&self) {
        let tid = self.shared.state.lock().current_transfer_id.take();
        if let Some(tid) = tid {
            if let Some(s) = self.shared.session.lock().as_ref() {
                s.trzsz_cancel(tid);
                s.set_interactive_busy(false);
            }
        }
    }

    pub fn trzsz_dismiss(&self) {
        let mut s = self.shared.state.lock();
        s.trzsz_mode = None;
        s.current_transfer_id = None;
        drop(s);
        if let Some(s) = self.shared.session.lock().as_ref() {
            s.set_interactive_busy(false);
        }
        self.shared.callback.on_trzsz_state_changed(TrzszPublicState::Idle);
    }

    /// OS からネットワーク断（Wi-Fi/セルラー消失等）を通知された時の対応を決める。
    /// QUIC 接続はパス変更に自前で耐えられるため無視し、ハンドシェイク中や
    /// OS からのネットワークpath変化(`ConnectivityManager`/`NWPathMonitor`)をそのまま
    /// 転送してもらい、判断はここ(Rust側のSSOT)で行う。Kotlin/Swift側はイベントを
    /// そのまま転送するだけでよい。
    ///
    /// `Idle`/`Connecting`/`Connected && is_quic`は既存の即時判断ロジックのまま
    /// (ハンドシェイク中は自前の耐性がまだ無いので即abort、QUIC系は自前で耐えるので
    /// 何もしない)。`Connected && !is_quic`(プレーンTCP SSH)だけが新たに
    /// [`crate::net_health_policy`]のdebounceの対象になる — OS通知の瞬断で
    /// 即切断されていた実バグの唯一の発生源だったため。
    pub fn notify_network_path_changed(&self, is_satisfied: bool) {
        let (phase, is_quic) = {
            let s = self.shared.state.lock();
            (s.reconnect.phase(), s.is_quic())
        };
        match phase {
            ConnPhase::Idle => {
                // 自動再接続ループが動いている間の「ネットワーク復帰」通知は、
                // 固定間隔ポーリング待機を早期に打ち切って今すぐ試すシグナルに
                // 使う(`spawn_reconnect_loop`参照)。ループが動いていない・
                // 単なる喪失通知(`is_satisfied=false`)は何もしない —
                // 元々このphaseでは接続自体が無いので喪失に対して打てる手が無い。
                if is_satisfied && self.shared.state.lock().reconnect.reconnect_loop_active {
                    crate::debug_reconnect::record("notify_network_path_changed satisfied phase=Idle action=reconnect_wake");
                    self.shared.reconnect_wake.notify_one();
                } else if is_satisfied {
                    crate::debug_reconnect::record("notify_network_path_changed satisfied phase=Idle action=ignored");
                }
            }
            ConnPhase::Connecting => {
                if !is_satisfied {
                    log::warn!("orchestrator: network lost during handshake — aborting");
                    apply_network_lost(&self.shared);
                }
            }
            ConnPhase::Connected if is_quic => {
                if is_satisfied {
                    crate::debug_reconnect::record(
                        "notify_network_path_changed satisfied phase=Connected transport=quic action=reattach_wake"
                    );
                    crate::resume_client::notify_network_restored_for_reattach();
                }
                log::info!("orchestrator: network path changed — QUIC session, letting transport handle it");
            }
            ConnPhase::Connected => {
                let (epoch, decision) = self.shared.path_observer.lock().handle_update(is_satisfied);
                match decision {
                    net_health_policy::Decision::Ignore => {}
                    net_health_policy::Decision::NotifyAfterDebounce(dur) => {
                        let shared = self.shared.clone();
                        self.shared.rt.spawn(async move {
                            tokio::time::sleep(dur).await;
                            if shared.path_observer.lock().is_current(epoch) {
                                log::warn!(
                                    "orchestrator: network still lost after debounce — disconnecting TCP session"
                                );
                                apply_network_lost(&shared);
                            }
                        });
                    }
                }
            }
        }
    }

    /// Phase 12: このセッション(タブ)だけの配色テーマを差し替える(per-session theme)。
    /// アプリ全体の既定テーマ(`set_terminal_theme`)とは独立しており、以降このタブが
    /// 解決する SGR にのみ反映される(既に画面/scrollbackに積まれたセルは遡って
    /// 再着色されない、`set_terminal_theme`と同じ制約)。
    ///
    /// `ansi16`/`default_fg`/`default_bg`は`set_terminal_theme`と同じ形式。呼び出し側
    /// (Kotlin `TerminalTabsViewModel`)が「Global default → Profile default →
    /// Tab/session override」の解決を行い、結果をここへ渡す。
    pub fn set_session_theme(&self, ansi16: Vec<u32>, default_fg: u32, default_bg: u32) {
        let theme = crate::theme::from_raw(ansi16, default_fg, default_bg);
        if let Some(s) = self.shared.session.lock().as_ref() {
            s.set_theme(theme);
        }
    }

    /// `AI_INTEGRATION_DESIGN.md` §3: このタブのAIパネル機能(`presentDocument`/
    /// `presentForm`)を有効化/無効化する。既定はOFFで、Kotlin側
    /// (`TerminalTabsViewModel`)が`ConnectionProfile.enableAiPanel`を接続確立ごとに
    /// ここへ渡す(`set_session_theme`と同じ「値を保持せず接続ごとに送り直す」設計、
    /// `SessionCore::set_panel_enabled`のdocコメント参照)。未接続時は無視される。
    pub fn set_ai_panel_enabled(&self, enabled: bool) {
        if let Some(s) = self.shared.session.lock().as_ref() {
            s.set_panel_enabled(enabled);
        }
    }

    /// タスク#17(ファイルプレビュー機能): `isekai-pipe ctl file ls|cat|info`をリモート
    /// ホストで1回実行し、結果を`request_id`付きで非同期に`OrchestratorCallback::
    /// on_file_preview_result`へ返す。`request_id`は呼び出し側(Kotlin)が発行する
    /// 一意なID(例: UUID)——複数のディレクトリ一覧/catチャンク要求が同時に
    /// in-flightでも取り違えないようにするため。
    ///
    /// 未接続、またはセッションがこのexecに対応していない(現状は全トランスポートが
    /// 対応しているため実質「未接続」のみ)場合は、待たせず即座に
    /// `FilePreviewOutcome::Error`で応答する。
    pub fn file_preview_request(&self, request_id: String, kind: FilePreviewRequestKind) {
        let command_line = file_preview::build_command_line(&kind);
        self.shared.state.lock().pending_file_previews.insert(request_id.clone(), kind);

        let queued = self.shared.session.lock().as_ref()
            .map(|s| s.file_preview_exec(request_id.clone(), command_line))
            .unwrap_or(false);

        if !queued {
            self.shared.state.lock().pending_file_previews.remove(&request_id);
            self.shared.callback.on_file_preview_result(
                request_id,
                FilePreviewOutcome::Error { message: "not connected".to_string() },
            );
        }
    }
}

// タスク#61: 意図的に`#[uniffi::export]`を付けない別の`impl`ブロックに置く
// (この`impl`内の`pub(crate) fn`はUniFFI境界には一切現れない)。既存のリモート
// コマンド系API(`send`/`resize`等)はすべて「`TransportCommand`をfire-and-forgetで
// 投げ、結果は`OrchestratorCallback`経由の別イベントで非同期に返す」設計だが、
// exec結果はコマンドを呼んだRust側コードがその場で欲しい値(stdout/終了コード)
// そのものなので、ここだけ素直に`async fn`にして呼び出し元へ`Result`を返す
// （UniFFI越しのKotlin/Swiftから直接呼ぶ経路は今回のタスクのスコープ外——
// 将来tmux管理コマンド機能がRust側だけで完結して使う想定）。
impl SessionOrchestrator {
    /// 現在確立しているセッションの、既存のインタラクティブシェルチャネル/PTYには
    /// 一切触れずに、同じ(プール済み)SSH接続上で短命なコマンドを実行し、
    /// stdoutと終了ステータスを回収する。未接続/切断済みなら
    /// `ExecError::NotConnected`を返す。
    pub(crate) async fn run_exec(&self, command: String) -> Result<ExecOutput, ExecError> {
        let active = self.shared.session.lock().clone();
        match active {
            Some(active) => active.run_exec(command).await,
            None => Err(ExecError::NotConnected),
        }
    }
}

// ── タスク#60: tmux session group ensure/attach + ウィンドウ create-or-select ──
//
// #61(`run_exec`、直上)・#62(`tmux_locator.rs`)を実際に繋ぎ合わせる。
// コマンド組み立て/フォールバック判断そのものは`tmux_session::ensure_tab_window`に
// 委ね、ここは「その関数が要求する`RemoteTmuxCommandRunner`シームを、この
// `SessionOrchestrator`の`run_exec`へどう繋ぐか」というアダプタ配線と、UniFFI境界
// (Kotlin向け引数/戻り値の型変換)だけを持つ。

/// [`crate::tmux_session::ensure_tab_window`]が要求する
/// [`crate::tmux_locator::RemoteTmuxCommandRunner`]の、`SessionOrchestrator::run_exec`
/// (#61)への薄いアダプタ。`ExecOutput`→`Result<String, TmuxRunError>`の変換自体は
/// `tmux_locator::exec_output_to_tmux_result`(`SshHandleTmuxRunner`と共有)に委ねる。
struct OrchestratorTmuxRunner<'a> {
    orchestrator: &'a SessionOrchestrator,
}

impl<'a> crate::tmux_locator::RemoteTmuxCommandRunner for OrchestratorTmuxRunner<'a> {
    fn run(
        &self,
        cmd: &str,
    ) -> impl std::future::Future<Output = Result<String, crate::tmux_locator::TmuxRunError>> + Send {
        let orchestrator = self.orchestrator;
        let cmd = cmd.to_string();
        async move {
            use crate::tmux_locator::TmuxRunError;
            let output = orchestrator.run_exec(cmd.clone()).await.map_err(|e| TmuxRunError(e.to_string()))?;
            crate::tmux_locator::exec_output_to_tmux_result(&cmd, output)
        }
    }
}

#[uniffi::export]
impl SessionOrchestrator {
    /// タスク#60本体。Kotlin側(`TerminalTabsViewModel`)はタブを開いた際、
    /// primary paneについてのみこれを呼ぶ(split paneはtmuxへ反映しないMVP判断、
    /// `tmux_session.rs`のモジュールdoc参照)。判断("session groupが要るか"
    /// "既存タグが見つかるか"等)は一切Kotlin側に持ち出さず、ここで完結させる
    /// (`.claude/rules/rust-ssot.md`)。
    ///
    /// - `profile_identity`: 呼び出し側が決める安定な識別子(例:
    ///   `ConnectionProfile.id`の文字列化)。同じ値からは常に同じsession groupに
    ///   決定論的に解決される。
    /// - `client_id`: このアプリインストール固有の永続トークン(Kotlin側で1回だけ
    ///   生成し`SharedPreferences`等に保存、以後使い回す)。
    /// - `existing_tag`: Room(`tmux_tab_locators`)に永続化済みのタグがあればそれ、
    ///   無ければ`None`(新規タブ)。
    /// - `enable_notifications`: 呼び出し側の`ConnectionProfile.enableTabNotifications`。
    ///   `true`の場合のみ`install_notify_hooks`(タスク#57)がこのタブのリモート
    ///   tmuxサーバーへ通知フックを書き込む(`set-option -g remain-on-exit on`という
    ///   サーバー全体への恒久的副作用を、opt-inしていないユーザーにまで強制しない
    ///   ため、`tmux_notify.rs`のモジュールdoc参照)。
    ///
    /// 戻り値の`tag`を(新規作成時、またはリモート側で見失われて作り直された時のみ
    /// 実質的に変わる)Roomへ書き戻せば、次回以降の再接続で同じウィンドウに戻れる。
    pub async fn ensure_tmux_tab_window(
        &self,
        profile_identity: String,
        client_id: String,
        existing_tag: Option<String>,
        enable_notifications: bool,
    ) -> Result<crate::TmuxTabWindowInfo, crate::TmuxSessionError> {
        let runner = OrchestratorTmuxRunner { orchestrator: self };
        let (group_name, session_name, outcome) =
            crate::tmux_session::ensure_tab_window(runner, &profile_identity, &client_id, existing_tag)
                .await
                .map_err(crate::TmuxSessionError::from)?;
        // タスク#59/#57が読む`TMUX_LOCATOR_REGISTRY`(push_ctl_socket_to_tmux/
        // install_notify_hooksの参照先)へ、解決/新規作成したロケータを登録する。
        // ここを配線し忘れると両者は「ロケータ未登録」として黙ってno-opになり
        // (ssh_handler.rsのコメント参照)、tmux統合機能が本番で一切発火しない。
        let registry = &crate::tmux_locator::TMUX_LOCATOR_REGISTRY;
        // 実機検証(2026-07-27)で判明: `push_ctl_socket_to_tmux`はctl-socket forward
        // 確立直後にspawnされ(`ssh_handler.rs`)、この`ensure_tmux_tab_window`
        // (Kotlin側の接続確認コールバック→UniFFI経由のこの呼び出し、というひと往復
        // 分だけ遅れる)より先に完走するのが実際にはほぼ常であることを確認した
        // (「稀にロケータ未登録のことがあるopportunistic機能」という従来の想定より
        // 厳しい状況で、`isekai-pipe ctl notify`/`isekai-pipe ctl tab-color`が
        // 実機では常に一切届いていなかった)。
        //
        // 既知のctl_socket_pathには2つの由来がありうる:
        // (a) このapp_paneが真に初めての接続で、まだ一度も`register()`されて
        //     いない間に届いた分(`TmuxLocatorRegistry::pending_ctl_socket_paths`
        //     に退避されている、`take_pending_ctl_socket_path`で取り出すと消える)。
        // (b) 同じタブでの再接続で、既存エントリ(前回の`register()`が作った
        //     ロケータ)がまだ生きているために`push_ctl_socket_to_tmux`が直接
        //     書き込み済みの分(`ctl_socket_path_for`で読める、消費しない)。
        // どちらの場合も、ここで`register()`が単純に`ctl_socket_path=None`で
        // 上書きすると既に分かっている値を握りつぶしてしまう。両方を確認し、
        // 引き継いで登録した上で、ロケータが分かった今すぐ改めてtmuxへ
        // 書き込み直す(bの場合は直接pushで既に成功済みのはずだが、再送は
        // 無害なのでどちらの由来でも同じ経路で扱う)。
        let recovered_ctl_socket_path = {
            let mut reg = registry.lock();
            reg.take_pending_ctl_socket_path(&self.shared.app_pane_id)
                .or_else(|| reg.ctl_socket_path_for(&self.shared.app_pane_id).map(str::to_string))
        };
        registry.lock().register(
            self.shared.app_pane_id.clone(),
            outcome.locator.clone(),
            recovered_ctl_socket_path.clone(),
        );
        registry.lock().set_notify_hooks_enabled(&self.shared.app_pane_id, enable_notifications);
        // tmux hook通知(タスク#57: bell/activity/silence/pane-died)の
        // `install_notify_hooks`(`ssh_handler.rs`側でも同じくctl-socket forward
        // 確立直後にspawnされ、ロケータ未登録なら黙ってno-opになる)も、ロケータが
        // 分かった今すぐ改めて試す(有効化されていなければ内部で無害にno-opする)。
        if let Some(ctl_socket_path) = recovered_ctl_socket_path {
            let push_runner = OrchestratorTmuxRunner { orchestrator: self };
            if let Err(e) = crate::tmux_locator::push_ctl_socket_to_tmux(
                registry,
                &self.shared.app_pane_id,
                &ctl_socket_path,
                push_runner,
            )
            .await
            {
                log::debug!("tmux-ctl-sock: retroactive push after registration failed (best-effort): {e}");
            }
        }
        let notify_hooks_runner = OrchestratorTmuxRunner { orchestrator: self };
        if let Err(e) =
            crate::tmux_notify::install_notify_hooks(registry, &self.shared.app_pane_id, notify_hooks_runner).await
        {
            log::debug!("tmux-notify-hooks: retroactive install after registration failed (best-effort): {e}");
        }
        Ok(crate::TmuxTabWindowInfo {
            tag: outcome.locator.tag.0,
            window_index: outcome.coords.window_index,
            session_name,
            group_name,
            is_new_window: outcome.is_new_window,
        })
    }
}

// ── Tests ──────────────────────────────────────────────────
//
// この模块の状態遷移(`ConnPhase`の分岐、`OrchestratorAdapter`のtrzsz状態集約)は
// 実SSH/QUIC接続を一切必要としない純粋なロジックであり、本来実機は不要だったにも
// 関わらず`orchestrator.rs`にはテストが1つも無かった。`rust-ssot.md`が「Rust側の
// SSOTである」ことの根拠として挙げている`notify_network_lost()`自体が無テストだった
// ため、ここで最初にカバーする。`ActiveSession`は具体的なtransportセッション型しか
// 保持できない(trait objectではない)ため、`session: Mutex::new(None)`のまま
// (未接続として)テストする — `notify_network_lost`/`disconnect`は`None`の場合
// no-opになるよう書かれているので、これで分岐ロジックの検証は完結する。
//
// #60: `on_trzsz_download_chunk`が上限超過時に呼ぶ`session.trzsz_cancel(..)`も
// 同様に`None`の場合no-opになるよう書かれているので、trzszバッファ上限のロジック
// (実SSH/QUIC不要)もここで検証できる。
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    // ADR_FUNCTIONAL_CORE_EFFECTS.md §6 Step 13: callback契約goldenの生成・一致検査
    // (`src/orchestrator/tests/callback_contract_golden.rs`)。
    mod callback_contract_golden;

    #[test]
    fn disconnect_kind_classifies_graceful_remote_exit_by_prefix() {
        let reason = Some("remote process exited (status 0)".to_string());
        assert_eq!(DisconnectKind::classify(&reason), DisconnectKind::GracefulRemoteExit);
    }

    #[test]
    fn disconnect_kind_classifies_the_network_lost_literal() {
        let reason = Some(NETWORK_LOST_REASON.to_string());
        assert_eq!(DisconnectKind::classify(&reason), DisconnectKind::NetworkLost);
    }

    #[test]
    fn disconnect_kind_defaults_to_transport_error_for_anything_else() {
        assert_eq!(DisconnectKind::classify(&None), DisconnectKind::TransportError);
        assert_eq!(
            DisconnectKind::classify(&Some("PTY/shell request failed".to_string())),
            DisconnectKind::TransportError
        );
        // A reason that merely mentions "network lost" mid-string (not the
        // exact synthesized literal `apply_network_lost` sends) must not be
        // misclassified — only the precise, orchestrator-synthesized value
        // counts as `NetworkLost`.
        assert_eq!(
            DisconnectKind::classify(&Some("something about network lost here".to_string())),
            DisconnectKind::TransportError
        );
    }

    #[derive(Default)]
    struct RecordingCallback {
        connection_states: StdMutex<Vec<ConnectionPublicState>>,
        trzsz_states: StdMutex<Vec<TrzszPublicState>>,
        downloads: StdMutex<Vec<(Option<String>, Vec<u8>)>>,
        notifications: StdMutex<Vec<crate::NotifyKind>>,
        file_preview_outcomes: StdMutex<Vec<FilePreviewOutcome>>,
        foreground_resumes: StdMutex<Vec<bool>>,
        /// round-3レビューS-2: `on_foreground_resume`と`on_connection_state_changed`が
        /// 発火した相対順序を検証するため(`foreground_resumes`/`connection_states`は
        /// 別々の`Vec`なので単体では順序を観測できない)。
        event_order: StdMutex<Vec<&'static str>>,
        /// `on_host_key`へ渡された`(host, port, fingerprint)`。`host`/`port`は
        /// `OrchestratorState::current_target()`(=`last_connect_attempt`)からの
        /// 導出結果そのものなので、ミラーフィールド廃止後もUIへ正しい接続先が
        /// 伝わっているかをここで直接検証できるようにしてある。
        host_keys: StdMutex<Vec<(String, u16, String)>>,
        agent_sign_requests: StdMutex<Vec<String>>,
        clipboard_writes: StdMutex<Vec<ClipboardPayload>>,
        clipboard_pull_requests: StdMutex<u32>,
        wifi_fd_requests: StdMutex<u32>,
        cellular_fd_requests: StdMutex<u32>,
        rebind_states: StdMutex<Vec<crate::rebind_manager::RebindPublicState>>,
        prompt_jumps: StdMutex<Vec<Option<crate::PromptJumpTarget>>>,
        // 以下は`forward_if_current!`が生成する単純委譲コールバックの検証用
        // (`assert_forwards_only_while_current`参照)。`ForwardState`は
        // `PartialEq`を持たないのでidだけを記録する。
        data: StdMutex<Vec<Vec<u8>>>,
        no_viable_paths: StdMutex<u32>,
        forward_state_ids: StdMutex<Vec<String>>,
        prompt_output_copies: StdMutex<Vec<Option<String>>>,
        /// Step 8a′: `on_connection_edge`へ渡された`(edge, generation)`。
        edges: StdMutex<Vec<(crate::ConnectionEdge, u64)>>,
        /// PR #167レビューL-1のテスト用: `on_connection_state_changed`の配信中に呼ぶフック
        /// (配信と別の遷移が割り込む状況を、同じスレッドからの再入で決定論的に作る)。
        on_state_hook: StdMutex<Option<Box<dyn Fn(&ConnectionPublicState) + Send>>>,
        /// Step 11: 状態公開・接続エッジの列を記録し、不変条件(エッジ契約・状態公開の単調性)を
        /// 記録のたびと`Drop`で検査する(`trace_invariants`)。これにより既存のorchestratorテストすべてが
        /// 個々のassertに加えて列の不変条件の検査にもなる。
        trace: crate::trace_invariants::TraceRecorder,
    }

    impl OrchestratorCallback for RecordingCallback {
        fn on_connection_state_changed(&self, state: ConnectionPublicState) {
            self.event_order.lock().unwrap().push("connection_state_changed");
            self.connection_states.lock().unwrap().push(state.clone());
            self.trace.record_state(&state);
            // フックの実行中はMutexを持たない(フックが再入して公開を起こしてもデッドロックしない)。
            let hook = self.on_state_hook.lock().unwrap().take();
            if let Some(hook) = hook {
                hook(&state);
                *self.on_state_hook.lock().unwrap() = Some(hook);
            }
        }
        fn on_screen_update(&self, _update: ScreenUpdate) {}
        fn on_host_key(&self, host: String, port: u16, fingerprint: String) -> bool {
            self.host_keys.lock().unwrap().push((host, port, fingerprint));
            true
        }
        fn on_data(&self, data: Vec<u8>) {
            self.data.lock().unwrap().push(data);
        }
        fn on_trzsz_state_changed(&self, state: TrzszPublicState) {
            self.trzsz_states.lock().unwrap().push(state);
        }
        fn on_download_complete(&self, file_name: Option<String>, data: Vec<u8>) {
            self.downloads.lock().unwrap().push((file_name, data));
        }
        fn on_no_viable_path(&self) {
            *self.no_viable_paths.lock().unwrap() += 1;
        }
        fn on_forward_state_changed(&self, id: String, _state: ForwardState) {
            self.forward_state_ids.lock().unwrap().push(id);
        }
        fn on_agent_sign_request(&self, key_fingerprint: String) -> bool {
            self.agent_sign_requests.lock().unwrap().push(key_fingerprint);
            true
        }
        fn on_clipboard_write(&self, payload: ClipboardPayload) {
            self.clipboard_writes.lock().unwrap().push(payload);
        }
        fn on_clipboard_pull_request(&self) -> Option<ClipboardPayload> {
            *self.clipboard_pull_requests.lock().unwrap() += 1;
            Some(ClipboardPayload { mime: crate::ClipboardMimeKind::TextPlain, data: b"clip".to_vec() })
        }
        fn on_request_wifi_fd(&self) -> Option<crate::PlatformFd> {
            *self.wifi_fd_requests.lock().unwrap() += 1;
            Some(crate::PlatformFd { fd: 42, local_ip: "10.0.0.1".to_string() })
        }
        fn on_request_cellular_fd(&self) -> Option<crate::PlatformFd> {
            *self.cellular_fd_requests.lock().unwrap() += 1;
            Some(crate::PlatformFd { fd: 43, local_ip: "10.0.0.2".to_string() })
        }
        fn on_rebind_state_changed(&self, state: crate::rebind_manager::RebindPublicState) {
            self.rebind_states.lock().unwrap().push(state);
        }
        fn on_prompt_jump(&self, target: Option<crate::PromptJumpTarget>) {
            self.prompt_jumps.lock().unwrap().push(target);
        }
        fn on_prompt_output_copy_ready(&self, text: Option<String>) {
            self.prompt_output_copies.lock().unwrap().push(text);
        }
        fn on_file_preview_result(&self, _request_id: String, outcome: FilePreviewOutcome) {
            self.file_preview_outcomes.lock().unwrap().push(outcome);
        }
        fn on_notify(&self, kind: crate::NotifyKind) {
            self.notifications.lock().unwrap().push(kind);
        }
        fn on_foreground_resume(&self, did_reconnect: bool) {
            self.event_order.lock().unwrap().push("foreground_resume");
            self.foreground_resumes.lock().unwrap().push(did_reconnect);
        }
        fn on_connection_edge(&self, edge: crate::ConnectionEdge, generation: u64) {
            self.event_order.lock().unwrap().push(match edge {
                crate::ConnectionEdge::Established { .. } => "edge_established",
                crate::ConnectionEdge::Lost => "edge_lost",
            });
            self.trace.record_edge(&edge, generation);
            self.edges.lock().unwrap().push((edge, generation));
        }
    }

    /// `shared_with_phase`の`is_quic`が表現する「QUIC系トランスポートで接続中」の状態。
    /// `current_host`/`current_port`/`is_quic`のミラーフィールドを廃止した今、
    /// 「QUICで繋いでいる」は`last_connect_attempt`がQUIC系バリアントであることでしか
    /// 表現できない(=両者が食い違う状態を作れない、というのがこの変更の狙い)。
    /// host/portは旧ミラーフィールドの既定値と同じ`example.com:22`にしてある。
    fn test_quic_attempt() -> LastConnectAttempt {
        LastConnectAttempt::IsekaiPipeQuic(IsekaiPipeQuicConfig {
            ssh_host: "example.com".to_string(),
            ssh_port: 22,
            username: "tester".to_string(),
            auth: crate::SshAuth::Password { password: "unused".to_string() },
            cols: 80,
            rows: 24,
            jump: None,
            bind_port: None,
        })
    }

    /// `begin_connect`へ渡すプレーンSSHのattempt(接続先ホスト名だけ差し替える)。
    fn ssh_attempt(host: &str) -> LastConnectAttempt {
        let mut config = test_ssh_config();
        config.host = host.to_string();
        LastConnectAttempt::Ssh(config)
    }

    /// `is_quic == false`側を敢えて`last_connect_attempt: None`のままにしてあるのは、
    /// 多くのテストが「直前の接続設定が無いので自動再接続ループが始まらない」ことに
    /// 依存しているため(例: `on_disconnected_sets_phase_idle_and_forwards_reason`)。
    /// プレーンSSHのattemptが要るテストは各自`ssh_attempt`で設定する。
    fn shared_with_phase(phase: ConnPhase, is_quic: bool) -> (Arc<OrchestratorShared>, Arc<RecordingCallback>) {
        shared_with_phase_on(RUNTIME.handle().clone(), phase, is_quic)
    }

    /// [`shared_with_phase`]の、spawn先ランタイムを明示指定する版(Step 2.5)。
    /// 仮想時間(`#[tokio::test(start_paused = true)]`)で走らせたいテストは
    /// `tokio::runtime::Handle::current()`を渡す。暗黙の`try_current()`は使わない
    /// ([`OrchestratorShared::rt`]のdoc参照)。
    fn shared_with_phase_on(
        rt: tokio::runtime::Handle,
        phase: ConnPhase,
        is_quic: bool,
    ) -> (Arc<OrchestratorShared>, Arc<RecordingCallback>) {
        let callback = Arc::new(RecordingCallback::default());
        let shared = Arc::new(OrchestratorShared {
            state: Mutex::new(OrchestratorState {
                // `last_connect_attempt`と常に対で設定する(`set_last_connect_attempt`のdoc)。
                // `phase`はreducerの外から書けない(PR #167レビューL-2)のでテスト用コンストラクタで作る。
                reconnect: ReconnectState::for_test(phase, is_quic.then(|| AttemptRef::next_after(None))),
                publications: PublicationQueue::default(),
                current_transfer_id: None,
                trzsz_mode: None,
                download_buf: Vec::new(),
                size_limit_exceeded_for: None,
                pending_file_previews: HashMap::new(),
                session_generation: 0,
                last_connect_attempt: is_quic.then(test_quic_attempt),
                reconnect_policy: ReconnectPolicy::default(),
                tab_focused: false,
                app_foreground: true,
                recent_notify_seqs: std::collections::VecDeque::new(),
            }),
            callback: callback.clone(),
            session: Mutex::new(None),
            path_observer: Mutex::new(net_health_policy::PathObserver::default()),
            app_pane_id: crate::tmux_locator::AppPaneId::generate_process_local(),
            reconnect_attempt: Box::new(connect_via),
            reconnect_wake: tokio::sync::Notify::new(),
            rt,
        });
        (shared, callback)
    }

    fn orchestrator_with_phase(phase: ConnPhase, is_quic: bool) -> (SessionOrchestrator, Arc<RecordingCallback>) {
        let (shared, callback) = shared_with_phase(phase, is_quic);
        (SessionOrchestrator { shared }, callback)
    }

    /// `Connected && !is_quic`のdebounceを検証するテスト用に、debounce時間を短く
    /// 差し替えたオーケストレータを作る。
    fn orchestrator_connected_tcp_with_debounce(
        rt: tokio::runtime::Handle,
        debounce: std::time::Duration,
    ) -> (SessionOrchestrator, Arc<RecordingCallback>) {
        let (shared, callback) = shared_with_phase_on(rt, ConnPhase::Connected, false);
        *shared.path_observer.lock() =
            net_health_policy::PathObserver::new(net_health_policy::NetPathPolicy { debounce });
        (SessionOrchestrator { shared }, callback)
    }

    /// 自動再接続ループを検証するためのオーケストレータ。`Connected`かつ
    /// `last_connect_attempt`が設定済み(再接続可能)で、tick/retry_interval/timeoutを
    /// テスト用に短く差し替えてある。`connect_via`(実ネットワーク)は使わず、
    /// 呼び出し回数を記録するだけのフェイクに差し替えてある — `connect()`は
    /// 非同期fire-and-forgetで実際の接続結果は検証できないため、この粒度の
    /// 単体テストでは「正しいcadenceで試行が発火したか」だけを見る。
    /// 自動再接続ループのテスト群が共有する`OrchestratorState`の組み立て
    /// (opusレビューLow指摘: 以前は2つのヘルパー関数がこの約20行をほぼ丸ごと
    /// 複製していた)。`reconnect_attempt`(テストごとに異なるフェイク)は
    /// この関数の範囲外——呼び出し側が`OrchestratorShared`構築時に個別に指定する。
    fn reconnect_test_state(policy: ReconnectPolicy) -> OrchestratorState {
        OrchestratorState {
            // `last_connect_attempt`と常に対で設定する(`set_last_connect_attempt`のdoc)。
            reconnect: ReconnectState::for_test(ConnPhase::Connected, Some(AttemptRef::next_after(None))),
            publications: PublicationQueue::default(),
            current_transfer_id: None,
            trzsz_mode: None,
            download_buf: Vec::new(),
            size_limit_exceeded_for: None,
            pending_file_previews: HashMap::new(),
            session_generation: 0,
            last_connect_attempt: Some(LastConnectAttempt::Ssh(test_ssh_config())),
            reconnect_policy: policy,
            tab_focused: false,
            app_foreground: true,
            recent_notify_seqs: std::collections::VecDeque::new(),
        }
    }

    fn orchestrator_connected_with_reconnect_policy(
        policy: ReconnectPolicy,
    ) -> (SessionOrchestrator, Arc<RecordingCallback>, Arc<std::sync::atomic::AtomicUsize>) {
        orchestrator_connected_with_reconnect_policy_on(RUNTIME.handle().clone(), policy)
    }

    /// [`orchestrator_connected_with_reconnect_policy`]の、再接続ループのspawn先
    /// ランタイムを明示指定する版(Step 2.5)。tick/retry_interval/timeoutの経過を
    /// 待つテストは`#[tokio::test(start_paused = true)]`から
    /// `tokio::runtime::Handle::current()`を渡し、実時間の`std::thread::sleep`ではなく
    /// 仮想時間の`tokio::time::sleep`で進める(CI負荷によるflakyさを排除する)。
    fn orchestrator_connected_with_reconnect_policy_on(
        rt: tokio::runtime::Handle,
        policy: ReconnectPolicy,
    ) -> (SessionOrchestrator, Arc<RecordingCallback>, Arc<std::sync::atomic::AtomicUsize>) {
        let callback = Arc::new(RecordingCallback::default());
        let attempt_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = attempt_count.clone();
        let shared = Arc::new(OrchestratorShared {
            state: Mutex::new(reconnect_test_state(policy)),
            callback: callback.clone(),
            session: Mutex::new(None),
            path_observer: Mutex::new(net_health_policy::PathObserver::default()),
            app_pane_id: crate::tmux_locator::AppPaneId::generate_process_local(),
            reconnect_attempt: Box::new(move |_shared, _attempt| {
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            }),
            reconnect_wake: tokio::sync::Notify::new(),
            rt,
        });
        (SessionOrchestrator { shared }, callback, attempt_count)
    }

    fn test_ssh_config() -> SshConfig {
        SshConfig {
            host: "example.com".to_string(),
            port: 22,
            username: "tester".to_string(),
            auth: crate::SshAuth::Password { password: "unused".to_string() },
            cols: 80,
            rows: 24,
            forwards: Vec::new(),
            agent_forward: false,
            jump: None,
            allow_non_loopback_forward_bind: false,
        }
    }

    // ── notify_network_path_changed ──────────────────────────

    #[test]
    fn notify_network_path_changed_does_nothing_when_idle() {
        let (orch, cb) = orchestrator_with_phase(ConnPhase::Idle, false);
        orch.notify_network_path_changed(false);
        assert!(cb.connection_states.lock().unwrap().is_empty());
        assert!(orch.shared.state.lock().reconnect.phase == ConnPhase::Idle);
    }

    #[test]
    fn notify_network_path_changed_aborts_and_reports_disconnected_during_handshake() {
        let (orch, cb) = orchestrator_with_phase(ConnPhase::Connecting, false);
        orch.notify_network_path_changed(false);
        let events = cb.connection_states.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert!(matches!(
            &events[0],
            ConnectionPublicState::Disconnected { reason: Some(r), .. } if r == "network lost"
        ));
        assert!(orch.shared.state.lock().reconnect.phase == ConnPhase::Idle);
    }

    #[test]
    fn notify_network_path_changed_ignores_satisfied_updates_during_handshake() {
        // Connecting中は瞬断debounceの対象外 — 既存の即時abort挙動を維持する一方、
        // is_satisfied=trueはそもそも「断ではない」ので何もしないままで良い。
        let (orch, cb) = orchestrator_with_phase(ConnPhase::Connecting, false);
        orch.notify_network_path_changed(true);
        assert!(cb.connection_states.lock().unwrap().is_empty());
        assert!(orch.shared.state.lock().reconnect.phase == ConnPhase::Connecting);
    }

    #[test]
    fn notify_network_path_changed_ignores_quic_when_connected() {
        let (orch, cb) = orchestrator_with_phase(ConnPhase::Connected, true);
        orch.notify_network_path_changed(false);
        // QUICは経路変更に自前で耐えるため、切断扱いにせずphaseもConnectedのまま維持する。
        assert!(cb.connection_states.lock().unwrap().is_empty());
        assert!(orch.shared.state.lock().reconnect.phase == ConnPhase::Connected);
    }

    #[tokio::test(start_paused = true)]
    async fn notify_network_path_changed_disconnects_plain_tcp_after_debounce_elapses() {
        let (orch, cb) = orchestrator_connected_tcp_with_debounce(tokio::runtime::Handle::current(), std::time::Duration::from_millis(30));
        orch.notify_network_path_changed(false);
        assert!(
            cb.connection_states.lock().unwrap().is_empty(),
            "debounce前は即座に切断されないはず"
        );

        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let events = cb.connection_states.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], ConnectionPublicState::Disconnected { .. }));
        assert!(orch.shared.state.lock().reconnect.phase == ConnPhase::Idle);
    }

    #[tokio::test(start_paused = true)]
    async fn notify_network_path_changed_does_not_disconnect_plain_tcp_if_recovered_before_debounce_elapses() {
        let (orch, cb) = orchestrator_connected_tcp_with_debounce(tokio::runtime::Handle::current(), std::time::Duration::from_millis(30));
        orch.notify_network_path_changed(false);
        orch.notify_network_path_changed(true); // 瞬断から復旧 — 保留中のdebounceをキャンセルする

        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        assert!(
            cb.connection_states.lock().unwrap().is_empty(),
            "debounce中に復旧したので切断されないはず"
        );
        assert!(orch.shared.state.lock().reconnect.phase == ConnPhase::Connected);
    }

    #[tokio::test(start_paused = true)]
    async fn notify_network_path_changed_pending_debounce_is_cancelled_by_a_new_connect_attempt() {
        // レビューで指摘された不具合の再現: プレーンTCP接続中に瞬断でdebounceが
        // 保留中の間、手動で別のセッションへ再接続しても、古いdebounceの発火で
        // 新しいセッションを誤って切断してはいけない。
        let (orch, cb) = orchestrator_connected_tcp_with_debounce(tokio::runtime::Handle::current(), std::time::Duration::from_millis(30));
        orch.notify_network_path_changed(false);
        orch.begin_connect(ssh_attempt("other.example.com"))
            .expect("Connected中の新規connectは許可されるはず");

        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let events = cb.connection_states.lock().unwrap();
        assert!(
            events.iter().all(|e| !matches!(e, ConnectionPublicState::Disconnected { .. })),
            "新しい接続試行後は、古いdebounce発火由来のDisconnectedが飛んではいけない, got: {events:?}"
        );
        assert!(
            orch.shared.state.lock().reconnect.phase == ConnPhase::Connecting,
            "古いdebounce発火でphaseがIdleへ巻き戻されてはいけない"
        );
    }

    // ── begin_connect (Task #9: 真の二重start防止) ────────────

    #[test]
    fn begin_connect_rejects_a_second_call_while_already_connecting() {
        // 前の connect_* 呼び出しがまだ Connecting のまま(=in-flight)の間に、別スレッド等から
        // 新規 connect_* が呼ばれた場合の「真の二重start」を防ぐ。Kotlin側の
        // TerminalSession.guardedConnect() の check-then-act は複数スレッドから並行に
        // 呼ばれるとアトミックではないため、最終防衛はRust側のこのロックの中で行う。
        let (orch, _cb) = orchestrator_with_phase(ConnPhase::Connecting, false);
        orch.shared.state.lock().set_last_connect_attempt(ssh_attempt("example.com"));
        let result = orch.begin_connect(ssh_attempt("other.example.com"));
        assert!(matches!(result, Err(SshError::ConnectionFailed)));
        // 拒否された呼び出しは進行中の接続の host/port(=`last_connect_attempt`)を
        // 書き換えてはいけない。
        assert_eq!(
            orch.shared.state.lock().current_target(),
            Some(("example.com".to_string(), 22, false))
        );
    }

    #[test]
    fn begin_connect_allows_a_new_call_while_idle() {
        let (orch, _cb) = orchestrator_with_phase(ConnPhase::Idle, false);
        let result = orch.begin_connect(ssh_attempt("other.example.com"));
        assert!(result.is_ok());
        assert!(orch.shared.state.lock().reconnect.phase == ConnPhase::Connecting);
    }

    #[test]
    fn begin_connect_allows_replacing_a_connected_session() {
        // Connected中の新規connectは「別セッションへの手動切り替え」として意図的に許可する
        // (notify_network_path_changed_pending_debounce_is_cancelled_by_a_new_connect_attempt
        // が検証する正当な経路)。
        let (orch, _cb) = orchestrator_with_phase(ConnPhase::Connected, false);
        let result = orch.begin_connect(ssh_attempt("other.example.com"));
        assert!(result.is_ok());
        assert_eq!(
            orch.shared.state.lock().current_target(),
            Some(("other.example.com".to_string(), 22, false))
        );
    }

    // ── OrchestratorAdapter (SessionCallback実装) ────────────

    fn adapter_with_phase(phase: ConnPhase, is_quic: bool) -> (OrchestratorAdapter, Arc<OrchestratorShared>, Arc<RecordingCallback>) {
        let (shared, callback) = shared_with_phase(phase, is_quic);
        (OrchestratorAdapter::new(shared.clone()), shared, callback)
    }

    #[test]
    fn on_connected_sets_phase_connected_and_reports_current_host() {
        let (adapter, shared, cb) = adapter_with_phase(ConnPhase::Connecting, false);
        // 通知されるhostは`last_connect_attempt`からの導出結果(ミラーフィールドは無い)。
        shared.state.lock().set_last_connect_attempt(ssh_attempt("example.com"));
        adapter.on_connected();
        assert!(shared.state.lock().reconnect.phase == ConnPhase::Connected);
        let events = cb.connection_states.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert!(matches!(
            &events[0],
            ConnectionPublicState::Connected { host } if host == "example.com"
        ));
    }

    #[test]
    fn on_disconnected_sets_phase_idle_and_forwards_reason() {
        let (adapter, shared, cb) = adapter_with_phase(ConnPhase::Connected, false);
        adapter.on_disconnected(Some("peer closed".to_string()));
        assert!(shared.state.lock().reconnect.phase == ConnPhase::Idle);
        let events = cb.connection_states.lock().unwrap();
        assert!(matches!(
            &events[0],
            ConnectionPublicState::Disconnected { reason: Some(r), .. } if r == "peer closed"
        ));
    }

    // ── #19: Local Network Privacyヒント ──────────────────────

    fn test_multipath_config() -> MultipathIsekaiPipeQuicConfig {
        MultipathIsekaiPipeQuicConfig {
            ssh_host: "example.com".to_string(),
            ssh_port: 22,
            direct_host: None,
            cellular_remote_host: None,
            wifi_fd: None,
            wifi_local_ip: None,
            cellular_fd: None,
            cellular_local_ip: None,
            username: "tester".to_string(),
            auth: crate::SshAuth::Password { password: "unused".to_string() },
            cols: 80,
            rows: 24,
            jump: None,
            bind_port: None,
            enable_upstream_failover: false,
        }
    }

    #[test]
    fn host_port_is_quic_reports_the_right_host_port_and_transport_kind_per_variant() {
        // `connect_via`(自動再接続)がここから`(host, port, is_quic)`を取り出して
        // `OrchestratorState`へ反映するので、6腕のmatch(`IsekaiPipeQuic`/
        // `IsekaiPipeQuicAuto`は共有)それぞれのhost/port/is_quicがずれていないことを
        // 直接確認する。プレーンSSHだけが`is_quic == false`。
        assert_eq!(
            LastConnectAttempt::Ssh(test_ssh_config()).host_port_is_quic(),
            ("example.com".to_string(), 22, false)
        );
        assert_eq!(
            LastConnectAttempt::Quic(QuicConfig {
                tsshd_host: "100.100.1.1".to_string(), tsshd_port: 9999,
                ssh_host: "quic.example.com".to_string(), ssh_port: 2222,
                username: "tester".to_string(), auth: crate::SshAuth::Password { password: "unused".to_string() },
                cols: 80, rows: 24, skip_cert_verify: true,
            }).host_port_is_quic(),
            ("quic.example.com".to_string(), 2222, true)
        );
        let ipq_config = IsekaiPipeQuicConfig {
            ssh_host: "ipq.example.com".to_string(), ssh_port: 3333,
            username: "tester".to_string(), auth: crate::SshAuth::Password { password: "unused".to_string() },
            cols: 80, rows: 24, jump: None, bind_port: None,
        };
        assert_eq!(
            LastConnectAttempt::IsekaiPipeQuic(ipq_config.clone()).host_port_is_quic(),
            ("ipq.example.com".to_string(), 3333, true)
        );
        assert_eq!(
            LastConnectAttempt::IsekaiPipeQuicAuto(ipq_config).host_port_is_quic(),
            ("ipq.example.com".to_string(), 3333, true)
        );
        assert_eq!(
            LastConnectAttempt::MultipathIsekaiPipeQuic(test_multipath_config()).host_port_is_quic(),
            ("example.com".to_string(), 22, true)
        );
        assert_eq!(
            LastConnectAttempt::IsekaiStunP2p(IsekaiStunP2pConfig {
                ssh_host: "stun.example.com".to_string(), ssh_port: 4444,
                username: "tester".to_string(), auth: crate::SshAuth::Password { password: "unused".to_string() },
                cols: 80, rows: 24, jump: None, stun_servers: vec!["stun.l.google.com:19302".to_string()],
            }).host_port_is_quic(),
            ("stun.example.com".to_string(), 4444, true)
        );
        assert_eq!(
            LastConnectAttempt::IsekaiLinkRelay(IsekaiLinkRelayConfig {
                ssh_host: "relay.example.com".to_string(), ssh_port: 5555,
                username: "tester".to_string(), auth: crate::SshAuth::Password { password: "unused".to_string() },
                cols: 80, rows: 24, jump: None,
                relay_addr: "relay:443".to_string(), relay_sni: "relay.example.com".to_string(), relay_jwt: "jwt".to_string(),
            }).host_port_is_quic(),
            ("relay.example.com".to_string(), 5555, true)
        );
    }

    #[test]
    fn looks_like_local_network_target_matches_private_link_local_and_mdns() {
        for host in [
            "192.168.1.5", "10.0.0.5", "172.20.0.5", "169.254.1.1", "myhost.local", "fd12:3456::1", "fe80::1",
            // codexレビュー指摘: 大文字小文字・末尾ドット(FQDN表記)の揺れも同じmDNS名として扱う。
            "MacBook.LOCAL", "myhost.local.",
        ] {
            assert!(looks_like_local_network_target(host), "{host} should be classified as local");
        }
    }

    #[test]
    fn looks_like_local_network_target_excludes_public_and_tailscale_addresses() {
        // 100.64.0.0/10(TailscaleのCGNAT範囲)はRFC1918プライベートではないため、
        // Local Network Privacyの対象ではない(オーバーレイVPN経由でオンリンクの
        // ブロードキャストドメインではない) — 誤検知しないことを確認する。
        for host in ["example.com", "8.8.8.8", "100.64.1.2", "2001:db8::1"] {
            assert!(!looks_like_local_network_target(host), "{host} should not be classified as local");
        }
    }

    #[test]
    fn classify_disconnect_issue_hint_is_none_without_attempt() {
        assert_eq!(classify_disconnect_issue_hint(None), None);
    }

    #[test]
    fn classify_disconnect_issue_hint_is_none_for_public_host() {
        let attempt = LastConnectAttempt::Ssh(test_ssh_config());
        assert_eq!(classify_disconnect_issue_hint(Some(&attempt)), None);
    }

    #[test]
    fn classify_disconnect_issue_hint_uses_host_for_plain_ssh() {
        let mut config = test_ssh_config();
        config.host = "192.168.1.5".to_string();
        let attempt = LastConnectAttempt::Ssh(config);
        assert_eq!(
            classify_disconnect_issue_hint(Some(&attempt)),
            Some(ConnectionIssueHint::LocalNetworkPermissionPossiblyDenied)
        );
    }

    #[test]
    fn classify_disconnect_issue_hint_prefers_direct_host_for_multipath() {
        let mut config = test_multipath_config();
        config.ssh_host = "my-tailscale-host".to_string();
        config.direct_host = Some("192.168.1.5".to_string());
        let attempt = LastConnectAttempt::MultipathIsekaiPipeQuic(config);
        assert_eq!(
            classify_disconnect_issue_hint(Some(&attempt)),
            Some(ConnectionIssueHint::LocalNetworkPermissionPossiblyDenied)
        );
    }

    #[test]
    fn classify_disconnect_issue_hint_falls_back_to_ssh_host_when_direct_host_absent() {
        let mut config = test_multipath_config();
        config.ssh_host = "192.168.1.5".to_string();
        config.direct_host = None;
        let attempt = LastConnectAttempt::MultipathIsekaiPipeQuic(config);
        assert_eq!(
            classify_disconnect_issue_hint(Some(&attempt)),
            Some(ConnectionIssueHint::LocalNetworkPermissionPossiblyDenied)
        );
    }

    #[test]
    fn on_disconnected_before_ever_connected_carries_hint_for_local_target() {
        let (adapter, shared, cb) = adapter_with_phase(ConnPhase::Connecting, true);
        let mut config = test_multipath_config();
        config.direct_host = Some("192.168.1.5".to_string());
        shared.state.lock().set_last_connect_attempt(LastConnectAttempt::MultipathIsekaiPipeQuic(config));

        adapter.on_disconnected(Some("connect failed".to_string()));

        let events = cb.connection_states.lock().unwrap();
        assert!(matches!(
            &events[0],
            ConnectionPublicState::Disconnected {
                issue_hint: Some(ConnectionIssueHint::LocalNetworkPermissionPossiblyDenied), ..
            }
        ));
    }

    #[test]
    fn on_disconnected_after_being_connected_never_carries_hint_even_for_local_target() {
        // 一度Connectedになった後の切断(ここではユーザー切断)は、たとえ接続先が
        // プライベートアドレスでもLocal Network Privacy拒否とは無関係(既に許可が
        // 下りていたはず)なのでヒント対象外。
        let (adapter, shared, cb) = adapter_with_phase(ConnPhase::Connected, true);
        let mut config = test_multipath_config();
        config.direct_host = Some("192.168.1.5".to_string());
        {
            let mut s = shared.state.lock();
            s.set_last_connect_attempt(LastConnectAttempt::MultipathIsekaiPipeQuic(config));
            s.reconnect.user_initiated_disconnect = true;
        }

        adapter.on_disconnected(Some("user disconnected".to_string()));

        let events = cb.connection_states.lock().unwrap();
        assert!(matches!(&events[0], ConnectionPublicState::Disconnected { issue_hint: None, .. }));
    }

    #[test]
    fn on_host_key_reports_current_host_and_port_from_state() {
        let (adapter, shared, cb) = adapter_with_phase(ConnPhase::Connecting, false);
        // ホスト鍵確認ダイアログに出すhost/portは`last_connect_attempt`からの導出結果。
        // 別ホスト・別ポートのattemptを入れて、本当にそこから読んでいることを確認する。
        let mut config = test_ssh_config();
        config.host = "hostkey.example.com".to_string();
        config.port = 2022;
        shared.state.lock().set_last_connect_attempt(LastConnectAttempt::Ssh(config));

        assert!(adapter.on_host_key("aa:bb:cc".to_string()));

        assert_eq!(
            cb.host_keys.lock().unwrap().as_slice(),
            &[("hostkey.example.com".to_string(), 2022, "aa:bb:cc".to_string())]
        );
    }

    #[test]
    fn on_trzsz_request_records_transfer_and_clears_download_buf() {
        let (adapter, shared, cb) = adapter_with_phase(ConnPhase::Connected, false);
        shared.state.lock().download_buf = vec![1, 2, 3];
        shared.state.lock().size_limit_exceeded_for = Some("stale".to_string());
        adapter.on_trzsz_request(
            "t1".to_string(), "download".to_string(), Some("file.txt".to_string()), Some(100),
        );
        {
            let s = shared.state.lock();
            assert_eq!(s.current_transfer_id.as_deref(), Some("t1"));
            assert_eq!(s.trzsz_mode.as_deref(), Some("download"));
            assert!(s.download_buf.is_empty());
            assert!(s.size_limit_exceeded_for.is_none(), "新しい転送開始時に前回の状態を持ち越さない");
        }
        let events = cb.trzsz_states.lock().unwrap();
        assert!(matches!(&events[0], TrzszPublicState::WaitingUser { transfer_id, .. } if transfer_id == "t1"));
    }

    #[test]
    fn on_trzsz_download_chunk_accumulates_bytes_across_calls() {
        let (adapter, shared, _cb) = adapter_with_phase(ConnPhase::Connected, false);
        adapter.on_trzsz_download_chunk("t1".to_string(), vec![1, 2], false);
        adapter.on_trzsz_download_chunk("t1".to_string(), vec![3, 4], true);
        assert_eq!(shared.state.lock().download_buf, vec![1, 2, 3, 4]);
    }

    // #60: 上限超過時にOOMせず転送を中断し、download_bufを破棄することを確認する。
    // `vec![0u8; MAX_DOWNLOAD_BUF_BYTES]`はLinux上ではゼロページの遅延確保のため
    // 実メモリをほぼ消費せず高速(かつ本テストはそれ以上書き込まない)。
    #[test]
    fn on_trzsz_download_chunk_clears_buffer_and_marks_size_limit_when_cap_exceeded() {
        let (adapter, shared, cb) = adapter_with_phase(ConnPhase::Connected, false);
        shared.state.lock().current_transfer_id = Some("t1".to_string());
        shared.state.lock().trzsz_mode = Some("download".to_string());
        shared.state.lock().download_buf = vec![0u8; MAX_DOWNLOAD_BUF_BYTES];

        adapter.on_trzsz_download_chunk("t1".to_string(), vec![1], false);

        let s = shared.state.lock();
        assert!(s.download_buf.is_empty(), "上限超過時はOOM回避のためdownload_bufを破棄する");
        assert_eq!(s.size_limit_exceeded_for.as_deref(), Some("t1"));
        drop(s);
        // まだon_trzsz_finishedが来ていないので、この時点ではDoneはまだ出ていない
        assert!(cb.trzsz_states.lock().unwrap().is_empty());
    }

    #[test]
    fn on_trzsz_download_chunk_stays_under_cap_does_not_mark_size_limit() {
        let (adapter, shared, _cb) = adapter_with_phase(ConnPhase::Connected, false);
        adapter.on_trzsz_download_chunk("t1".to_string(), vec![1, 2, 3], false);
        let s = shared.state.lock();
        assert_eq!(s.download_buf, vec![1, 2, 3]);
        assert!(s.size_limit_exceeded_for.is_none());
    }

    // #60: 上限超過後、非同期のtrzsz_cancel往復で本物のon_trzsz_finishedが
    // (success=false, message="Cancelled"等の汎用文言で)届いた際に、ユーザーへ
    // 分かりやすい「大きすぎる」メッセージへ差し替えて伝えることを確認する。
    #[test]
    fn on_trzsz_finished_overrides_message_when_size_limit_was_exceeded() {
        let (adapter, shared, cb) = adapter_with_phase(ConnPhase::Connected, false);
        shared.state.lock().current_transfer_id = Some("t1".to_string());
        shared.state.lock().trzsz_mode = Some("download".to_string());
        shared.state.lock().download_buf = vec![0u8; MAX_DOWNLOAD_BUF_BYTES];
        adapter.on_trzsz_download_chunk("t1".to_string(), vec![1], false);

        // 実際のFSMはtrzsz_cancel経由で非同期に success=false, message="Cancelled" を
        // 返してくる。ここではそれをシミュレートする。
        adapter.on_trzsz_finished("t1".to_string(), false, Some("Cancelled".to_string()));

        assert!(cb.downloads.lock().unwrap().is_empty(), "中断された転送でdownload_completeを呼んではいけない");
        let events = cb.trzsz_states.lock().unwrap();
        assert!(matches!(
            &events[0],
            TrzszPublicState::Done { success: false, message: Some(m), .. } if m.contains("大きすぎる")
        ));
        assert!(shared.state.lock().size_limit_exceeded_for.is_none(), "一度使ったフラグは消費してクリアする");
    }

    // #60: 万一cancelが競合してsuccess=trueが返ってきても、上限超過を検知していた
    // 転送は成功扱いにしない(かつ空のdownload_bufをon_download_completeへ渡さない)。
    #[test]
    fn on_trzsz_finished_forces_failure_when_size_limit_was_exceeded_even_if_reported_success() {
        let (adapter, shared, cb) = adapter_with_phase(ConnPhase::Connected, false);
        shared.state.lock().current_transfer_id = Some("t1".to_string());
        shared.state.lock().trzsz_mode = Some("download".to_string());
        shared.state.lock().size_limit_exceeded_for = Some("t1".to_string());

        adapter.on_trzsz_finished("t1".to_string(), true, None);

        assert!(cb.downloads.lock().unwrap().is_empty());
        let events = cb.trzsz_states.lock().unwrap();
        assert!(matches!(&events[0], TrzszPublicState::Done { success: false, .. }));
    }

    #[test]
    fn on_trzsz_finished_download_success_emits_download_complete_with_accumulated_bytes() {
        let (adapter, shared, cb) = adapter_with_phase(ConnPhase::Connected, false);
        shared.state.lock().trzsz_mode = Some("download".to_string());
        adapter.on_trzsz_download_chunk("t1".to_string(), vec![9, 9, 9], true);
        adapter.on_trzsz_finished("t1".to_string(), true, None);
        let downloads = cb.downloads.lock().unwrap();
        assert_eq!(downloads.len(), 1);
        assert_eq!(downloads[0].1, vec![9, 9, 9]);
        // 完了後はtransfer_id/download_bufをクリアし、次の転送に持ち越さない。
        assert!(shared.state.lock().current_transfer_id.is_none());
        assert!(shared.state.lock().download_buf.is_empty());
    }

    #[test]
    fn on_trzsz_finished_failure_does_not_emit_download_complete() {
        let (adapter, shared, cb) = adapter_with_phase(ConnPhase::Connected, false);
        shared.state.lock().trzsz_mode = Some("download".to_string());
        adapter.on_trzsz_download_chunk("t1".to_string(), vec![9, 9, 9], true);
        adapter.on_trzsz_finished("t1".to_string(), false, Some("connection lost".to_string()));
        assert!(cb.downloads.lock().unwrap().is_empty());
        let events = cb.trzsz_states.lock().unwrap();
        assert!(matches!(&events[0], TrzszPublicState::Done { success: false, .. }));
    }

    #[test]
    fn on_trzsz_finished_upload_does_not_emit_download_complete_even_with_buffered_bytes() {
        // upload完了時にはdownload_bufは本来空のはずだが、万一何か残っていても
        // is_download判定がfalseならon_download_completeを呼んではいけない。
        let (adapter, shared, cb) = adapter_with_phase(ConnPhase::Connected, false);
        shared.state.lock().trzsz_mode = Some("upload".to_string());
        shared.state.lock().download_buf = vec![1, 2, 3];
        adapter.on_trzsz_finished("t1".to_string(), true, None);
        assert!(cb.downloads.lock().unwrap().is_empty());
    }

    #[test]
    fn on_trzsz_progress_defaults_mode_to_download_when_unset() {
        let (adapter, _shared, cb) = adapter_with_phase(ConnPhase::Connected, false);
        adapter.on_trzsz_progress("t1".to_string(), 50, Some(100));
        let events = cb.trzsz_states.lock().unwrap();
        assert!(matches!(
            &events[0],
            TrzszPublicState::InProgress { mode, transferred: 50, total: Some(100), .. } if mode == "download"
        ));
    }

    // ── session_generation(古いセッションからの遅延コールバックを無視) ──

    #[test]
    fn stale_adapter_callbacks_are_ignored_after_a_newer_session_starts() {
        let (shared, cb) = shared_with_phase(ConnPhase::Connecting, false);
        let stale = OrchestratorAdapter::new(shared.clone());
        // 新しいセッションが生成された(session_generationが進む)状況を模す。
        let _fresh = OrchestratorAdapter::new(shared.clone());

        stale.on_connected();
        assert!(
            cb.connection_states.lock().unwrap().is_empty(),
            "古いgenerationのon_connectedはphase/通知に一切影響してはいけない"
        );
        assert!(shared.state.lock().reconnect.phase == ConnPhase::Connecting, "phaseも書き換わってはいけない");

        stale.on_disconnected(Some("stale".to_string()));
        assert!(
            cb.connection_states.lock().unwrap().is_empty(),
            "古いgenerationのon_disconnectedも無視されるはず"
        );
    }

    #[test]
    fn on_host_key_returns_false_for_stale_generation() {
        let (shared, _cb) = shared_with_phase(ConnPhase::Connecting, false);
        let stale = OrchestratorAdapter::new(shared.clone());
        let _fresh = OrchestratorAdapter::new(shared.clone());
        assert!(!stale.on_host_key("aa:bb:cc".to_string()));
    }

    // ── OrchestratorAdapter (SessionCallback) の単純委譲群 ──────
    //
    // 以下はいずれも`forward_if_current!`が生成する「is_current()なら
    // `shared.callback`へそのまま委譲、staleなら何もしない/既定値を返す」という
    // 同型のパターン。共通の検証手順は`assert_forwards_only_while_current`に
    // まとめつつ、テストはコールバックごとに独立させてある —— マクロ展開が
    // 名前や引数を取り違えていれば(例: `on_prompt_jump`が別のコールバックへ
    // 委譲していれば)そのコールバック固有の記録内容の比較で落ちる。

    /// `forward_if_current!`が生成する単純委譲コールバック1つ分の検証。
    ///
    /// - `invoke`: 対象コールバックの呼び出し(戻り値も検証したいので返す)。
    /// - `expected_when_current` / `expected_when_stale`: 現行/stale時の戻り値
    ///   (staleは`Default::default()`相当 —— `()`/`false`/`None`)。
    /// - `recorded`: `RecordingCallback`が記録した「何が届いたか」。型が
    ///   コールバックごとに違うのでクロージャで取り出す。
    ///
    /// 現行adapterでの呼び出し後に`recorded`が`expected_recorded`と一致すること、
    /// および新しいadapter生成でstaleにした後の2回目の呼び出しでも`recorded`が
    /// **増えも変わりもしない**ことを見る(件数だけでなく内容まで比較するので、
    /// 「委譲された値が正しいか」と「staleでは委譲されないか」の両方を1本で
    /// カバーできる)。
    fn assert_forwards_only_while_current<T, R>(
        invoke: impl Fn(&OrchestratorAdapter) -> T,
        expected_when_current: T,
        expected_when_stale: T,
        recorded: impl Fn(&RecordingCallback) -> R,
        expected_recorded: R,
    ) where
        T: std::fmt::Debug + PartialEq,
        R: std::fmt::Debug + PartialEq,
    {
        let (shared, cb) = shared_with_phase(ConnPhase::Connected, false);
        let current = OrchestratorAdapter::new(shared.clone());

        assert_eq!(invoke(&current), expected_when_current, "currentなadapterは委譲するはず");
        assert_eq!(recorded(&cb), expected_recorded, "委譲された内容がそのまま届くはず");

        // 新しいセッションが生成された(session_generationが進む)状況を模す。
        let _fresh = OrchestratorAdapter::new(shared.clone());
        assert_eq!(invoke(&current), expected_when_stale, "staleなadapterは既定値を返すはず");
        assert_eq!(recorded(&cb), expected_recorded, "staleなadapterからは委譲されないはず");
    }

    #[test]
    fn on_agent_sign_request_forwards_and_is_suppressed_for_stale_generation() {
        assert_forwards_only_while_current(
            |adapter| adapter.on_agent_sign_request("aa:bb".to_string()),
            true,
            false,
            |cb| cb.agent_sign_requests.lock().unwrap().clone(),
            vec!["aa:bb".to_string()],
        );
    }

    #[test]
    fn on_clipboard_write_forwards_and_is_suppressed_for_stale_generation() {
        let payload = ClipboardPayload { mime: crate::ClipboardMimeKind::TextPlain, data: b"hello".to_vec() };
        let expected = vec![payload.clone()];
        assert_forwards_only_while_current(
            |adapter| adapter.on_clipboard_write(payload.clone()),
            (),
            (),
            |cb| cb.clipboard_writes.lock().unwrap().clone(),
            expected,
        );
    }

    #[test]
    fn on_clipboard_pull_request_forwards_and_is_suppressed_for_stale_generation() {
        assert_forwards_only_while_current(
            |adapter| adapter.on_clipboard_pull_request(),
            Some(ClipboardPayload { mime: crate::ClipboardMimeKind::TextPlain, data: b"clip".to_vec() }),
            None,
            |cb| *cb.clipboard_pull_requests.lock().unwrap(),
            1,
        );
    }

    #[test]
    fn on_request_wifi_fd_forwards_and_is_suppressed_for_stale_generation() {
        // `PlatformFd`は`PartialEq`を持たない(uniffi::Record)ので、比較可能な
        // タプルへ落としてから検証する。
        assert_forwards_only_while_current(
            |adapter| adapter.on_request_wifi_fd().map(|fd| (fd.fd, fd.local_ip)),
            Some((42, "10.0.0.1".to_string())),
            None,
            |cb| *cb.wifi_fd_requests.lock().unwrap(),
            1,
        );
    }

    #[test]
    fn on_request_cellular_fd_forwards_and_is_suppressed_for_stale_generation() {
        assert_forwards_only_while_current(
            |adapter| adapter.on_request_cellular_fd().map(|fd| (fd.fd, fd.local_ip)),
            Some((43, "10.0.0.2".to_string())),
            None,
            |cb| *cb.cellular_fd_requests.lock().unwrap(),
            1,
        );
    }

    #[test]
    fn on_rebind_state_changed_forwards_and_is_suppressed_for_stale_generation() {
        assert_forwards_only_while_current(
            |adapter| adapter.on_rebind_state_changed(crate::rebind_manager::RebindPublicState::FailedOverToCellular),
            (),
            (),
            |cb| cb.rebind_states.lock().unwrap().clone(),
            vec![crate::rebind_manager::RebindPublicState::FailedOverToCellular],
        );
    }

    #[test]
    fn on_prompt_jump_forwards_and_is_suppressed_for_stale_generation() {
        let target = Some(crate::PromptJumpTarget { scroll_offset: 5, is_live: false });
        assert_forwards_only_while_current(
            |adapter| adapter.on_prompt_jump(target),
            (),
            (),
            |cb| cb.prompt_jumps.lock().unwrap().clone(),
            vec![target],
        );
    }

    // 以下4つは、マクロ化するまで単純委譲であること自体が無検証だった残りの
    // コールバック。`RecordingCallback`側に記録を足して同じヘルパーで確認する
    // (`on_screen_update`だけは、`ScreenUpdate`が`PartialEq`を持たない大きな
    // Record型で値の合成コストに見合わないため対象外にしてある —— 委譲先の
    // 名前・引数はマクロが同じ`$name`から生成するのでコンパイラが保証しており、
    // 残る検証価値はstaleガードだけで、それは他の11メソッドで確認済み)。

    #[test]
    fn on_data_forwards_and_is_suppressed_for_stale_generation() {
        assert_forwards_only_while_current(
            |adapter| adapter.on_data(b"hello".to_vec()),
            (),
            (),
            |cb| cb.data.lock().unwrap().clone(),
            vec![b"hello".to_vec()],
        );
    }

    #[test]
    fn on_no_viable_path_forwards_and_is_suppressed_for_stale_generation() {
        assert_forwards_only_while_current(
            |adapter| adapter.on_no_viable_path(),
            (),
            (),
            |cb| *cb.no_viable_paths.lock().unwrap(),
            1,
        );
    }

    #[test]
    fn on_forward_state_changed_forwards_and_is_suppressed_for_stale_generation() {
        // `ForwardState`は`PartialEq`を持たないため、記録側でidだけを取っている。
        assert_forwards_only_while_current(
            |adapter| adapter.on_forward_state_changed("fwd-1".to_string(), ForwardState::Listening),
            (),
            (),
            |cb| cb.forward_state_ids.lock().unwrap().clone(),
            vec!["fwd-1".to_string()],
        );
    }

    #[test]
    fn on_prompt_output_copy_ready_forwards_and_is_suppressed_for_stale_generation() {
        assert_forwards_only_while_current(
            |adapter| adapter.on_prompt_output_copy_ready(Some("output".to_string())),
            (),
            (),
            |cb| cb.prompt_output_copies.lock().unwrap().clone(),
            vec![Some("output".to_string())],
        );
    }

    // ── 自動再接続ループ ──────────────────────────────────

    fn fast_test_policy() -> ReconnectPolicy {
        ReconnectPolicy {
            tick: Duration::from_millis(15),
            retry_interval: Duration::from_millis(30),
            timeout: Duration::from_millis(200),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn unexpected_disconnect_after_connected_starts_reconnect_loop_and_attempts_retry() {
        let (orch, cb, attempt_count) = orchestrator_connected_with_reconnect_policy_on(tokio::runtime::Handle::current(), fast_test_policy());
        let adapter = OrchestratorAdapter::new(orch.shared.clone());

        adapter.on_disconnected(Some("peer closed".to_string()));
        assert!(orch.shared.state.lock().reconnect.reconnect_loop_active, "ループが起動しているはず");

        tokio::time::sleep(Duration::from_millis(80)).await;

        let events = cb.connection_states.lock().unwrap();
        assert!(
            events.iter().any(|e| matches!(e, ConnectionPublicState::Reconnecting { .. })),
            "Reconnectingがライブ通知されるはず, got: {events:?}"
        );
        assert!(
            attempt_count.load(std::sync::atomic::Ordering::SeqCst) >= 1,
            "retry_interval経過後に再接続が試みられるはず"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn handle_unexpected_disconnect_suppresses_when_a_reconnect_loop_is_already_active() {
        // 自動再接続ループの1リトライ試行自体が失敗して起きる切断(=既に
        // reconnect_loop_activeがtrue)は、二重にループを起動せず・Disconnectedも
        // 通知せず、ループ自身のtickに任せるはず(`Action::Suppress`)。
        let (orch, cb, attempt_count) = orchestrator_connected_with_reconnect_policy_on(tokio::runtime::Handle::current(), fast_test_policy());
        orch.shared.state.lock().reconnect.reconnect_loop_active = true;
        let adapter = OrchestratorAdapter::new(orch.shared.clone());

        adapter.on_disconnected(Some("retry attempt itself failed".to_string()));

        assert!(
            cb.connection_states.lock().unwrap().is_empty(),
            "Suppress時はDisconnectedを通知してはいけない(ループ自身のtickに任せる)"
        );
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert_eq!(
            attempt_count.load(std::sync::atomic::Ordering::SeqCst), 0,
            "handle_unexpected_disconnect自身は新しいループを二重起動してはいけない"
        );
        assert!(orch.shared.state.lock().reconnect.phase == ConnPhase::Idle, "phase自体は他の分岐と同様Idleへ戻るはず");
    }

    #[tokio::test(start_paused = true)]
    async fn user_initiated_disconnect_does_not_start_reconnect_loop() {
        let (orch, cb, attempt_count) = orchestrator_connected_with_reconnect_policy_on(tokio::runtime::Handle::current(), fast_test_policy());
        orch.disconnect(); // session が None なので実際の切断処理は起きないが、フラグは立つ
        let adapter = OrchestratorAdapter::new(orch.shared.clone());

        adapter.on_disconnected(Some("peer closed".to_string()));
        assert!(!orch.shared.state.lock().reconnect.reconnect_loop_active);

        tokio::time::sleep(Duration::from_millis(60)).await;
        assert_eq!(attempt_count.load(std::sync::atomic::Ordering::SeqCst), 0);
        let events = cb.connection_states.lock().unwrap();
        assert!(matches!(&events[0], ConnectionPublicState::Disconnected { .. }));
    }

    #[test]
    fn disconnect_without_last_connect_attempt_does_not_start_reconnect_loop() {
        let (shared, cb) = shared_with_phase(ConnPhase::Connected, false);
        // last_connect_attemptは未設定(初回接続の失敗などを模す)。
        let adapter = OrchestratorAdapter::new(shared.clone());
        adapter.on_disconnected(Some("handshake failed".to_string()));
        assert!(!shared.state.lock().reconnect.reconnect_loop_active);
        let events = cb.connection_states.lock().unwrap();
        assert!(matches!(&events[0], ConnectionPublicState::Disconnected { .. }));
    }

    #[tokio::test(start_paused = true)]
    async fn graceful_remote_exit_does_not_start_reconnect_loop() {
        // リモートシェルの正常終了(`run_ssh_channel_loop`の`ChannelMsg::ExitStatus`)は
        // ネットワーク障害ではないので自動再接続してはいけない
        // (実際にこの区別が無かったことで`transport::pooling_e2e_tests::
        // one_tab_remote_exit_does_not_disconnect_sibling_tabs`が壊れた)。
        let (orch, cb, attempt_count) = orchestrator_connected_with_reconnect_policy_on(tokio::runtime::Handle::current(), fast_test_policy());
        let adapter = OrchestratorAdapter::new(orch.shared.clone());
        adapter.on_disconnected(Some("remote process exited (status 0)".to_string()));

        assert!(!orch.shared.state.lock().reconnect.reconnect_loop_active);
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert_eq!(attempt_count.load(std::sync::atomic::Ordering::SeqCst), 0);
        let events = cb.connection_states.lock().unwrap();
        assert!(matches!(&events[0], ConnectionPublicState::Disconnected { .. }));
    }

    #[tokio::test(start_paused = true)]
    async fn reconnect_loop_gives_up_after_timeout_and_notifies_disconnected() {
        let policy = ReconnectPolicy {
            tick: Duration::from_millis(10),
            // retry_intervalをtimeoutより長くして、試行を一切発火させずに
            // タイムアウトだけを検証する(実接続の副作用を避ける)。
            retry_interval: Duration::from_secs(60),
            timeout: Duration::from_millis(40),
        };
        let (orch, cb, attempt_count) = orchestrator_connected_with_reconnect_policy_on(tokio::runtime::Handle::current(), policy);
        let adapter = OrchestratorAdapter::new(orch.shared.clone());
        adapter.on_disconnected(Some("peer closed".to_string()));

        tokio::time::sleep(Duration::from_millis(200)).await;

        assert_eq!(attempt_count.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert!(!orch.shared.state.lock().reconnect.reconnect_loop_active, "タイムアウト後はループが終了しているはず");
        let events = cb.connection_states.lock().unwrap();
        assert!(matches!(
            events.last(),
            Some(ConnectionPublicState::Disconnected { reason: Some(r), .. }) if r.contains("timed out")
        ), "ギブアップ後は理由付きでDisconnectedが通知されるはず, got: {events:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn network_path_restored_while_idle_triggers_an_immediate_retry_bypassing_the_tick_cadence() {
        // retry_intervalをこのテストのsleep幅よりずっと長くしておくことで、
        // 通常のtick cadenceだけでは絶対に試行が発火しない状況を作る —
        // それでも試行が観測されれば、`notify_network_path_changed(true)`の
        // 早期ウェイクが実際に効いている証拠になる。
        let policy = ReconnectPolicy {
            tick: Duration::from_millis(10),
            retry_interval: Duration::from_secs(60),
            timeout: Duration::from_secs(60),
        };
        let (orch, _cb, attempt_count) = orchestrator_connected_with_reconnect_policy_on(tokio::runtime::Handle::current(), policy);
        let adapter = OrchestratorAdapter::new(orch.shared.clone());
        adapter.on_disconnected(Some("peer closed".to_string()));
        assert!(orch.shared.state.lock().reconnect.reconnect_loop_active);

        // ループが最初のtick待機に入るのを少し待ってから、ネットワーク復帰を通知する。
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(
            attempt_count.load(std::sync::atomic::Ordering::SeqCst), 0,
            "retry_intervalが60秒なので、通知前はまだ試行が発火していないはず"
        );

        orch.notify_network_path_changed(true);
        tokio::time::sleep(Duration::from_millis(50)).await;

        assert!(
            attempt_count.load(std::sync::atomic::Ordering::SeqCst) >= 1,
            "ネットワーク復帰通知でtick cadenceを待たずに即座に再試行するはず"
        );
    }

    /// eecba351 / ADR §6 Step 3a(rev6)のshell配線テスト: 試行中(`retry_attempt_in_flight`)に
    /// 届いたネットワーク復帰wakeは`pending_wake`として保持され、その試行の結果(現行世代の
    /// `on_disconnected`)を観測した直後に、tick cadence(ここでは60秒)を待たず次の試行になる。
    /// reducer側の不変条件(`reconnect_fsm::tests::wake_during_in_flight_attempt_is_never_lost`)を、
    /// 実際のループ・`Notify`・アダプタ経由で確かめる。
    #[tokio::test(start_paused = true)]
    async fn wake_during_in_flight_attempt_is_retried_right_after_the_attempt_result() {
        let policy = ReconnectPolicy {
            // tickを5秒にして、テスト内の30msの待機中には通常のtickが一度も来ないようにする。
            // こうすると最後の再試行は`WakeReconnectLoop`(試行結果の直後のwake通知)でしか起き得ず、
            // そのarmがno-opなら次のtick(5秒後)まで試行は1回のままでこのテストは失敗する
            // (tick=10msだと次のtickが`pending_wake`を拾ってしまい、wake通知の有無を区別できない)。
            tick: Duration::from_secs(5),
            retry_interval: Duration::from_secs(60),
            timeout: Duration::from_secs(120),
        };
        let (orch, _cb, attempt_count) =
            orchestrator_connected_with_reconnect_policy_on(tokio::runtime::Handle::current(), policy);
        let adapter = OrchestratorAdapter::new(orch.shared.clone());
        adapter.on_disconnected(Some("peer closed".to_string()));
        assert!(orch.shared.state.lock().reconnect.reconnect_loop_active);
        tokio::time::sleep(Duration::from_millis(30)).await;

        // 1回目のwake: 試行中でないので即座に試行する(フェイクは結果を報告しないので試行中のまま残る)。
        orch.notify_network_path_changed(true);
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(attempt_count.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(orch.shared.state.lock().reconnect.retry_attempt_in_flight);

        // 2回目のwake: 試行中なので重ねて試行せず、pending_wakeとして保持する。
        orch.notify_network_path_changed(true);
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(
            attempt_count.load(std::sync::atomic::Ordering::SeqCst), 1,
            "試行中に新しい試行を重ねてはいけない"
        );
        assert!(orch.shared.state.lock().reconnect.pending_wake, "wakeは捨てずに保持されるはず");

        // 試行の結果(この試行が作った現行世代のセッションの切断)が届くと、保持していた
        // wakeでループが起こされ、retry_interval(60秒)を待たずに次の試行になる。
        let attempt_adapter = OrchestratorAdapter::new(orch.shared.clone());
        attempt_adapter.on_disconnected(Some("retry attempt failed".to_string()));
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(
            attempt_count.load(std::sync::atomic::Ordering::SeqCst), 2,
            "保持していたwakeが試行結果の直後に再試行へ繋がるはず"
        );
        assert!(!orch.shared.state.lock().reconnect.pending_wake);
    }

    #[test]
    fn network_path_restored_while_idle_does_nothing_if_no_reconnect_loop_is_active() {
        let (orch, cb) = orchestrator_with_phase(ConnPhase::Idle, false);
        orch.notify_network_path_changed(true);
        assert!(
            cb.connection_states.lock().unwrap().is_empty(),
            "再接続ループが動いていない状態でのネットワーク復帰通知は何もしないはず"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn cancel_reconnect_stops_loop_and_notifies_disconnected() {
        let policy = ReconnectPolicy {
            tick: Duration::from_millis(10),
            retry_interval: Duration::from_secs(60),
            timeout: Duration::from_secs(60),
        };
        let (orch, cb, _attempt_count) = orchestrator_connected_with_reconnect_policy_on(tokio::runtime::Handle::current(), policy);
        let adapter = OrchestratorAdapter::new(orch.shared.clone());
        adapter.on_disconnected(Some("peer closed".to_string()));
        assert!(orch.shared.state.lock().reconnect.reconnect_loop_active);

        orch.cancel_reconnect();

        assert!(!orch.shared.state.lock().reconnect.reconnect_loop_active);
        let events = cb.connection_states.lock().unwrap();
        assert!(matches!(
            events.last(),
            Some(ConnectionPublicState::Disconnected { reason: Some(r), .. }) if r.contains("cancelled")
        ));

        // ループ自体もepoch不一致で自然終了するはず(次tickでretryが発火しない)。
        drop(events);
        tokio::time::sleep(Duration::from_millis(60)).await;
        // cancel_reconnect後に新規の接続試行は発火しない。
    }

    #[tokio::test(start_paused = true)]
    async fn a_new_manual_connect_invalidates_a_pending_reconnect_loop() {
        // レビューで指摘された既存の`notify_network_path_changed`パターンと同型:
        // 再接続ループが動いている最中に手動で新しい接続を始めたら、古いループの
        // 通知/試行が新しいセッションを誤って巻き戻してはいけない。
        let policy = ReconnectPolicy {
            tick: Duration::from_millis(10),
            retry_interval: Duration::from_millis(20),
            timeout: Duration::from_secs(60),
        };
        let (orch, cb, attempt_count) = orchestrator_connected_with_reconnect_policy_on(tokio::runtime::Handle::current(), policy);
        let adapter = OrchestratorAdapter::new(orch.shared.clone());
        adapter.on_disconnected(Some("peer closed".to_string()));
        assert!(orch.shared.state.lock().reconnect.reconnect_loop_active);

        // 手動で新しい接続を開始(begin_connect相当)。
        let _new_adapter = orch.begin_connect(ssh_attempt("other.example.com"))
            .expect("Idle中の新規connectは許可されるはず");
        assert!(!orch.shared.state.lock().reconnect.reconnect_loop_active, "新しい手動接続でループは無効化されるはず");

        tokio::time::sleep(Duration::from_millis(80)).await;
        assert_eq!(
            attempt_count.load(std::sync::atomic::Ordering::SeqCst), 0,
            "無効化された古いループはconnect_via相当を発火してはいけない"
        );
        let events = cb.connection_states.lock().unwrap();
        assert!(
            events.iter().all(|e| !matches!(e, ConnectionPublicState::Disconnected { .. })),
            "古いループ由来のDisconnectedが飛んではいけない, got: {events:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn apply_network_lost_on_connected_tcp_session_also_starts_reconnect_loop() {
        // always-connects.mdの実インシデント(網断debounce経路だけが自動復旧の
        // 対象外だった)の再発防止: apply_network_lost経由でも同じ
        // handle_unexpected_disconnectを通ることを確認する。
        let (orch, cb, attempt_count) = orchestrator_connected_with_reconnect_policy_on(tokio::runtime::Handle::current(), fast_test_policy());
        apply_network_lost(&orch.shared);
        assert!(orch.shared.state.lock().reconnect.reconnect_loop_active);

        tokio::time::sleep(Duration::from_millis(80)).await;
        assert!(attempt_count.load(std::sync::atomic::Ordering::SeqCst) >= 1);
        let events = cb.connection_states.lock().unwrap();
        assert!(events.iter().any(|e| matches!(e, ConnectionPublicState::Reconnecting { .. })));
    }

    #[tokio::test(start_paused = true)]
    async fn reconnect_success_stops_the_loop() {
        let policy = ReconnectPolicy {
            tick: Duration::from_millis(10),
            retry_interval: Duration::from_millis(500), // このテストでは試行が発火する前に成功させる
            timeout: Duration::from_secs(60),
        };
        let (orch, cb, _attempt_count) = orchestrator_connected_with_reconnect_policy_on(tokio::runtime::Handle::current(), policy);
        let adapter = OrchestratorAdapter::new(orch.shared.clone());
        adapter.on_disconnected(Some("peer closed".to_string()));
        assert!(orch.shared.state.lock().reconnect.reconnect_loop_active);

        // 別経路で再接続が成功した(例: 手動再接続やconnect_via経由の新しいセッション)ことを模す。
        let success_adapter = OrchestratorAdapter::new(orch.shared.clone());
        success_adapter.on_connected();
        // ループ自身の初回通知(spawn直後の非同期タスク)とこの成功呼び出しは別スレッドで
        // 走るため、"Connected"より前に1回だけ"Reconnecting"が紛れ込む可能性はあるが、
        // それは無害(UIは直後にConnectedへ収束する)。ここで決定的に検証できる/すべき
        // 性質は「ループ自身が停止すること」と「成功後に(タイムアウト由来の)Disconnectedが
        // 絶対に飛ばないこと」の2つ。
        // (Step 2.5以降このテストはcurrent_threadの仮想時間ランタイムで走るため、実際には
        // ループtaskはテストが最初に`.await`するまで動かない。上の「紛れ込み」の許容は
        // 本番のmulti-thread `RUNTIME`での性質として残しておく。)
        assert!(!orch.shared.state.lock().reconnect.reconnect_loop_active);

        tokio::time::sleep(Duration::from_millis(60)).await;
        let events = cb.connection_states.lock().unwrap();
        assert!(
            events.iter().any(|e| matches!(e, ConnectionPublicState::Connected { .. })),
            "Connectedが通知されるはず, got: {events:?}"
        );
        assert!(
            !events.iter().any(|e| matches!(e, ConnectionPublicState::Disconnected { .. })),
            "成功後にギブアップのDisconnectedが飛んではいけない, got: {events:?}"
        );
    }

    // ── Step 8a′: 接続エッジ(on_connection_edge)の退出経路ごとのshell配線テスト ──
    //
    // reducerのproptest(`reconnect_fsm::tests`)は「reducerが正しい」ことしか示さず、「shellが
    // 全経路でreducerにEventを渡している」ことは示さない(ADR §6 Step 8a′「shell側の検証」)。
    // そこで退出経路(a)〜(f)ごとに1本ずつ、`Established(g)`の後に`Lost(g)`が届くことを確かめる。

    /// 接続エッジのテスト用オーケストレータ。`Idle`・接続試行無しから始め、`begin_connect`で
    /// 本番と同じ遷移を踏む。`reconnect_attempt`は実接続をせず、本番の`connect_via`と同じ
    /// [`begin_reconnect_session`](phase遷移+新しい世代のアダプタ生成)だけを行い、そのアダプタを
    /// `adapters`へ積む(テストがそれに`on_connected`を呼んで「試行成功」を模す)。
    /// TCP網断のdebounceは30msに短縮してある。
    fn edge_test_orchestrator(
        rt: tokio::runtime::Handle,
        policy: ReconnectPolicy,
    ) -> (SessionOrchestrator, Arc<RecordingCallback>, Arc<StdMutex<Vec<OrchestratorAdapter>>>) {
        let callback = Arc::new(RecordingCallback::default());
        let adapters = Arc::new(StdMutex::new(Vec::new()));
        let sink = adapters.clone();
        let mut state = reconnect_test_state(policy);
        state.reconnect = ReconnectState::default();
        state.last_connect_attempt = None;
        let shared = Arc::new(OrchestratorShared {
            state: Mutex::new(state),
            callback: callback.clone(),
            session: Mutex::new(None),
            path_observer: Mutex::new(net_health_policy::PathObserver::new(net_health_policy::NetPathPolicy {
                debounce: Duration::from_millis(30),
            })),
            app_pane_id: crate::tmux_locator::AppPaneId::generate_process_local(),
            reconnect_attempt: Box::new(move |shared, _attempt| {
                let adapter = begin_reconnect_session(shared);
                sink.lock().unwrap().push(adapter);
                Ok(())
            }),
            reconnect_wake: tokio::sync::Notify::new(),
            rt,
        });
        (SessionOrchestrator { shared }, callback, adapters)
    }

    /// PR #167レビューL-1: `on_connection_state_changed`/`on_connection_edge`の配信順は、reducerが遷移を
    /// 適用した順序と一致する。`Connected`を配信している最中(=`Established(g)`を配信する前)に別の遷移
    /// (network-lost)が`Lost(g)`を出しても、`Lost(g)`は`Established(g)`の後に届く。別スレッドの割り込み
    /// (セッションtaskの`on_connected`とnetwork-lost debounceの競合)を、配信中のコールバックからの再入で
    /// 決定論的に再現している。以前の実装(各スレッドがロック解放後に自分で配信)では、再入した側の
    /// `Lost(g)`が先に配信され`[Lost(g), Established(g)]`になっていた。
    #[tokio::test(start_paused = true)]
    async fn edges_are_delivered_in_reducer_order_even_if_a_transition_interleaves_with_delivery() {
        let (orch, cb, _adapters) = edge_test_orchestrator(tokio::runtime::Handle::current(), fast_test_policy());
        let adapter = orch.begin_connect(ssh_attempt("example.com")).expect("Idle中のconnectは受理されるはず");
        let generation = adapter.generation;
        let weak = Arc::downgrade(&orch.shared);
        let fired = std::sync::atomic::AtomicBool::new(false);
        *cb.on_state_hook.lock().unwrap() = Some(Box::new(move |state: &ConnectionPublicState| {
            if matches!(state, ConnectionPublicState::Connected { .. })
                && !fired.swap(true, std::sync::atomic::Ordering::SeqCst)
            {
                if let Some(shared) = weak.upgrade() {
                    apply_network_lost(&shared);
                }
            }
        }));

        adapter.on_connected();

        assert_eq!(
            edges_of(&cb),
            vec![
                (crate::ConnectionEdge::Established { host: "example.com".to_string(), upstream_failover: false }, generation),
                (crate::ConnectionEdge::Lost, generation),
            ],
            "Lost(g)がEstablished(g)より先に配信された"
        );
        let states = cb.connection_states.lock().unwrap().clone();
        assert!(
            matches!(states.as_slice(), [ConnectionPublicState::Connecting, ConnectionPublicState::Connected { .. }]),
            "状態公開の順序: {states:?}"
        );
        assert!(orch.shared.state.lock().reconnect.reconnect_loop_active, "network-lostで再接続ループが起動しているはず");
    }

    /// `begin_connect`→`on_connected`で新しい世代のedgeを開く(本番の手動接続と同じ経路)。
    fn connect_and_establish(orch: &SessionOrchestrator, host: &str) -> OrchestratorAdapter {
        let adapter = orch.begin_connect(ssh_attempt(host)).expect("Idle/Connected中のconnectは受理されるはず");
        adapter.on_connected();
        adapter
    }

    fn edges_of(cb: &RecordingCallback) -> Vec<(crate::ConnectionEdge, u64)> {
        cb.edges.lock().unwrap().clone()
    }

    fn established(host: &str, generation: u64) -> (crate::ConnectionEdge, u64) {
        (crate::ConnectionEdge::Established { host: host.to_string(), upstream_failover: false }, generation)
    }

    fn lost(generation: u64) -> (crate::ConnectionEdge, u64) {
        (crate::ConnectionEdge::Lost, generation)
    }

    /// 届いたエッジ列の契約を検査する(ADR §6 Step 11 T1と同じ形):
    /// 各世代の`Established`は高々1回・単調増加で、`Lost(g)`は直前の`Established(g)`に対して正確に1回。
    fn assert_edge_contract(edges: &[(crate::ConnectionEdge, u64)]) {
        let mut open: Option<u64> = None;
        let mut last_established: Option<u64> = None;
        for (edge, g) in edges {
            match edge {
                crate::ConnectionEdge::Established { .. } => {
                    assert_eq!(open, None, "Lostより前に次のEstablished({g})が来た: {edges:?}");
                    assert!(!matches!(last_established, Some(last) if *g <= last),"Establishedの世代が単調増加でない: {edges:?}");
                    open = Some(*g);
                    last_established = Some(*g);
                }
                crate::ConnectionEdge::Lost => {
                    assert_eq!(open, Some(*g), "対応するEstablishedの無いLost({g}): {edges:?}");
                    open = None;
                }
            }
        }
    }

    /// `Connected`の公開の**後に**同じ呼び出し箇所から`Established`が届く(ADR §2.4-4の固定順)。
    /// 同一世代の`on_connected`重複では`Established`を出し直さない。
    #[tokio::test(start_paused = true)]
    async fn edge_established_follows_connected_publication_and_is_not_repeated() {
        let (orch, cb, _adapters) = edge_test_orchestrator(tokio::runtime::Handle::current(), fast_test_policy());
        let adapter = connect_and_establish(&orch, "example.com");
        assert_eq!(
            cb.event_order.lock().unwrap().as_slice(),
            &["connection_state_changed", "connection_state_changed", "edge_established"],
            "Connecting → Connected → Established の順で届くはず"
        );
        adapter.on_connected();
        assert_eq!(edges_of(&cb), vec![established("example.com", 1)]);
    }

    /// #175: マルチパス接続のattempt(物理fd無し)。`upstream_failover`はプロファイルの
    /// `enable_upstream_failover`に相当する。
    fn multipath_attempt(host: &str, upstream_failover: bool) -> LastConnectAttempt {
        let mut config = test_multipath_config();
        config.ssh_host = host.to_string();
        config.enable_upstream_failover = upstream_failover;
        LastConnectAttempt::MultipathIsekaiPipeQuic(config)
    }

    /// #175: 自動再接続ループの成功とフォアグラウンド復帰の再接続(どちらも`connect_*`=Kotlinの
    /// `connectPane`を通らない)で新しく開いた世代の`Established`にも、手動接続の世代と同じ
    /// `upstream_failover`が載る。Established/Lostの契約(各世代1回ずつ)もそのまま成り立つ。
    #[tokio::test(start_paused = true)]
    async fn edge_established_carries_upstream_failover_for_every_reconnected_generation() {
        for enabled in [true, false] {
            let (orch, cb, adapters) = edge_test_orchestrator(tokio::runtime::Handle::current(), fast_test_policy());
            let first = orch.begin_connect(multipath_attempt("example.com", enabled)).expect("Idle中のconnectは受理されるはず");
            first.on_connected();
            // 自動再接続ループの成功。
            first.on_disconnected(Some("peer closed".to_string()));
            let looped = {
                let mut waited = 0;
                loop {
                    if let Some(adapter) = adapters.lock().unwrap().pop() {
                        break adapter;
                    }
                    assert!(waited < 1000, "再接続ループが試行しなかった");
                    tokio::time::sleep(Duration::from_millis(5)).await;
                    waited += 5;
                }
            };
            looped.on_connected();
            // フォアグラウンド復帰の再接続。
            orch.notify_did_enter_background(30_000);
            orch.notify_background_budget_expired();
            orch.notify_will_enter_foreground();
            let resumed = adapters.lock().unwrap().pop().expect("フォアグラウンド復帰で再接続を試みるはず");
            resumed.on_connected();

            let edges = edges_of(&cb);
            assert_edge_contract(&edges);
            let established: Vec<_> = edges
                .iter()
                .filter_map(|(edge, g)| match edge {
                    crate::ConnectionEdge::Established { upstream_failover, .. } => Some((*g, *upstream_failover)),
                    crate::ConnectionEdge::Lost => None,
                })
                .collect();
            assert_eq!(established.len(), 3, "手動・ループ・フォアグラウンド復帰の3世代: {edges:?}");
            assert!(
                established.iter().all(|(_, flag)| *flag == enabled),
                "全世代のEstablishedがupstream_failover={enabled}を運ぶはず: {edges:?}"
            );
        }
    }

    /// #175: upstream failover監視を求めるのは`enable_upstream_failover`なマルチパス接続だけ。
    #[test]
    fn wants_upstream_failover_monitor_only_for_multipath_with_failover_enabled() {
        assert!(multipath_attempt("h", true).wants_upstream_failover_monitor());
        assert!(!multipath_attempt("h", false).wants_upstream_failover_monitor());
        assert!(!ssh_attempt("h").wants_upstream_failover_monitor());
    }

    /// #175: 再接続に渡すattemptからは、最初のセッションが引き取り済みの物理マルチパスfdを外す
    /// (同じfd番号を再び引き取ると、close済みか無関係なfdを奪う)。他の設定はそのまま。
    #[test]
    fn for_reconnect_strips_consumed_physical_multipath_fds_and_keeps_the_rest() {
        let mut config = test_multipath_config();
        config.wifi_fd = Some(41);
        config.wifi_local_ip = Some("192.168.0.2".to_string());
        config.cellular_fd = Some(42);
        config.cellular_local_ip = Some("10.0.0.2".to_string());
        config.enable_upstream_failover = true;
        let LastConnectAttempt::MultipathIsekaiPipeQuic(stripped) =
            LastConnectAttempt::MultipathIsekaiPipeQuic(config).for_reconnect()
        else {
            panic!("variantは変わらないはず");
        };
        assert_eq!(
            (stripped.wifi_fd, stripped.wifi_local_ip.as_deref(), stripped.cellular_fd, stripped.cellular_local_ip.as_deref()),
            (None, None, None, None)
        );
        assert!(stripped.enable_upstream_failover, "upstream failoverの設定は再接続でも維持する");
        assert_eq!(stripped.ssh_host, "example.com");
        let LastConnectAttempt::Ssh(ssh) = ssh_attempt("plain.example.com").for_reconnect() else {
            panic!("プレーンSSHはそのまま");
        };
        assert_eq!(ssh.host, "plain.example.com");
    }

    /// (a) ユーザー`disconnect()`。
    #[tokio::test(start_paused = true)]
    async fn edge_lost_on_user_disconnect() {
        let (orch, cb, _adapters) = edge_test_orchestrator(tokio::runtime::Handle::current(), fast_test_policy());
        let adapter = connect_and_establish(&orch, "example.com");
        orch.disconnect(); // sessionはNoneなので、実セッションの代わりに下でon_disconnectedを届ける
        adapter.on_disconnected(Some("closed by user".to_string()));
        tokio::time::sleep(Duration::from_millis(80)).await;

        assert_eq!(edges_of(&cb), vec![established("example.com", 1), lost(1)]);
        assert!(!orch.shared.state.lock().reconnect.reconnect_loop_active);
        let order = cb.event_order.lock().unwrap().clone();
        assert_eq!(&order[order.len() - 2..], &["edge_lost", "connection_state_changed"], "Lostは同じ箇所のDisconnected公開より前");
    }

    /// (b) トランスポートエラー(`on_disconnected`)。自動再接続ループが始まっても`Lost`は1回だけ。
    #[tokio::test(start_paused = true)]
    async fn edge_lost_on_transport_error() {
        let (orch, cb, _adapters) = edge_test_orchestrator(tokio::runtime::Handle::current(), fast_test_policy());
        let adapter = connect_and_establish(&orch, "example.com");
        adapter.on_disconnected(Some("peer closed".to_string()));
        assert!(orch.shared.state.lock().reconnect.reconnect_loop_active);
        tokio::time::sleep(Duration::from_millis(80)).await;

        let edges = edges_of(&cb);
        assert_eq!(edges, vec![established("example.com", 1), lost(1)]);
        assert_edge_contract(&edges);
    }

    /// (c) TCPのnetwork-lost debounce満了(`apply_network_lost`、ADR round 3 R3-1)。アダプタも世代も
    /// 経由しない経路でも`Lost`が即座に出て、後から旧セッションの`on_disconnected`が同じ世代で
    /// 届いても二重に出ない。
    #[tokio::test(start_paused = true)]
    async fn edge_lost_on_network_lost_debounce() {
        let policy = ReconnectPolicy {
            tick: Duration::from_millis(10),
            // 試行を発火させず、旧アダプタを現行世代のまま保つ。
            retry_interval: Duration::from_secs(60),
            timeout: Duration::from_secs(60),
        };
        let (orch, cb, _adapters) = edge_test_orchestrator(tokio::runtime::Handle::current(), policy);
        let adapter = connect_and_establish(&orch, "example.com");
        orch.notify_network_path_changed(false);
        assert_eq!(edges_of(&cb), vec![established("example.com", 1)], "debounce前はLostを出さない");

        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(edges_of(&cb), vec![established("example.com", 1), lost(1)]);
        assert!(orch.shared.state.lock().reconnect.reconnect_loop_active);

        // 旧セッションの遅延した切断通知(まだ現行世代)。
        assert!(adapter.is_current());
        adapter.on_disconnected(Some("broken pipe".to_string()));
        assert_eq!(edges_of(&cb), vec![established("example.com", 1), lost(1)], "同じ世代のLostを二重に出してはいけない");
    }

    /// (d) Connected中の手動`connect_*`(`begin_connect`はConnected中の呼び出しを受理する)。
    /// 旧世代の`Lost`は`Connecting`公開より前、新しい世代の`Established`より前に届く(ADR round 2 N-4)。
    #[tokio::test(start_paused = true)]
    async fn edge_lost_on_manual_connect_while_connected() {
        let (orch, cb, _adapters) = edge_test_orchestrator(tokio::runtime::Handle::current(), fast_test_policy());
        let old = connect_and_establish(&orch, "example.com");
        let new = orch.begin_connect(ssh_attempt("other.example.com")).expect("Connected中のconnectは受理されるはず");
        assert_eq!(edges_of(&cb), vec![established("example.com", 1), lost(1)]);
        {
            let order = cb.event_order.lock().unwrap();
            assert_eq!(&order[order.len() - 2..], &["edge_lost", "connection_state_changed"], "Lost(old)はConnecting公開より前");
        }

        old.on_connected(); // 旧世代の遅延コールバックは無視される
        new.on_connected();
        let edges = edges_of(&cb);
        assert_eq!(edges, vec![established("example.com", 1), lost(1), established("other.example.com", 2)]);
        assert_edge_contract(&edges);
    }

    /// (e) Suspended後のフォアグラウンド復帰による`connect_via`(ADR round 4 m-R4-5の手順どおり、
    /// 切断を起こさずConnectedのまま復帰する)。
    #[tokio::test(start_paused = true)]
    async fn edge_lost_on_foreground_resume_reconnect() {
        let (orch, cb, adapters) = edge_test_orchestrator(tokio::runtime::Handle::current(), fast_test_policy());
        let _old = connect_and_establish(&orch, "example.com");
        assert_eq!(edges_of(&cb), vec![established("example.com", 1)]);
        orch.notify_did_enter_background(30_000);
        orch.notify_background_budget_expired();
        assert_eq!(orch.shared.state.lock().reconnect.background_state, BackgroundState::Suspended);
        assert!(orch.shared.state.lock().reconnect.phase == ConnPhase::Connected, "切断を起こさずに復帰する手順");

        orch.notify_will_enter_foreground();
        assert_eq!(edges_of(&cb), vec![established("example.com", 1), lost(1)], "connect_viaのphase遷移でLost(old)");

        let new = adapters.lock().unwrap().pop().expect("フォアグラウンド復帰で再接続を試みるはず");
        new.on_connected();
        let edges = edges_of(&cb);
        assert_eq!(edges, vec![established("example.com", 1), lost(1), established("example.com", 2)]);
        assert_edge_contract(&edges);
    }

    /// (f) 切断→自動再接続ループの試行成功。
    #[tokio::test(start_paused = true)]
    async fn edge_reestablished_after_reconnect_loop_success() {
        let (orch, cb, adapters) = edge_test_orchestrator(tokio::runtime::Handle::current(), fast_test_policy());
        let adapter = connect_and_establish(&orch, "example.com");
        adapter.on_disconnected(Some("peer closed".to_string()));
        tokio::time::sleep(Duration::from_millis(80)).await;

        let attempt = adapters.lock().unwrap().pop().expect("retry_interval経過後に再接続を試みるはず");
        assert!(attempt.is_current());
        let generation = orch.shared.state.lock().session_generation;
        attempt.on_connected();
        assert!(!orch.shared.state.lock().reconnect.reconnect_loop_active);
        let edges = edges_of(&cb);
        assert_eq!(edges, vec![established("example.com", 1), lost(1), established("example.com", generation)]);
        assert_edge_contract(&edges);
    }

    // ── #20: バックグラウンド/フォアグラウンド遷移 ─────────────

    #[test]
    fn notify_did_enter_background_quiesces_only_when_connected() {
        let (orch, _cb) = orchestrator_with_phase(ConnPhase::Connected, false);
        orch.notify_did_enter_background(30_000);
        assert_eq!(orch.shared.state.lock().reconnect.background_state, BackgroundState::Quiescing);
    }

    #[test]
    fn notify_did_enter_background_is_noop_when_idle() {
        // Idle(そもそも維持すべきセッションが無い)はバックグラウンド化しても対象外。
        // `Connecting`は対象に含める(`notify_did_enter_background_while_connecting_...`参照)。
        let (orch, _cb) = orchestrator_with_phase(ConnPhase::Idle, false);
        orch.notify_did_enter_background(30_000);
        assert_eq!(orch.shared.state.lock().reconnect.background_state, BackgroundState::Foreground);
    }

    #[test]
    fn notify_background_budget_expired_transitions_quiescing_to_suspended() {
        let (orch, _cb) = orchestrator_with_phase(ConnPhase::Connected, false);
        orch.notify_did_enter_background(30_000);
        orch.notify_background_budget_expired();
        assert_eq!(orch.shared.state.lock().reconnect.background_state, BackgroundState::Suspended);
    }

    #[test]
    fn notify_background_budget_expired_is_noop_when_still_foreground() {
        let (orch, _cb) = orchestrator_with_phase(ConnPhase::Connected, false);
        orch.notify_background_budget_expired();
        assert_eq!(orch.shared.state.lock().reconnect.background_state, BackgroundState::Foreground);
    }

    #[test]
    fn notify_memory_warning_forces_suspended_while_quiescing() {
        let (orch, _cb) = orchestrator_with_phase(ConnPhase::Connected, false);
        orch.notify_did_enter_background(30_000);
        orch.notify_memory_warning();
        assert_eq!(orch.shared.state.lock().reconnect.background_state, BackgroundState::Suspended);
    }

    #[test]
    fn notify_memory_warning_is_noop_while_foreground() {
        let (orch, _cb) = orchestrator_with_phase(ConnPhase::Connected, false);
        orch.notify_memory_warning();
        assert_eq!(orch.shared.state.lock().reconnect.background_state, BackgroundState::Foreground);
    }

    #[test]
    fn notify_will_enter_foreground_within_budget_resumes_without_reconnecting() {
        let (orch, _cb, attempt_count) = orchestrator_connected_with_reconnect_policy(fast_test_policy());
        orch.notify_did_enter_background(30_000);
        assert_eq!(orch.shared.state.lock().reconnect.background_state, BackgroundState::Quiescing);

        orch.notify_will_enter_foreground();
        assert_eq!(orch.shared.state.lock().reconnect.background_state, BackgroundState::Foreground);
        assert_eq!(
            attempt_count.load(std::sync::atomic::Ordering::SeqCst), 0,
            "猶予内復帰(Quiescing)では再接続を試みてはいけない"
        );
    }

    #[test]
    fn notify_will_enter_foreground_within_budget_fires_on_foreground_resume_false() {
        let (orch, cb, _attempt_count) = orchestrator_connected_with_reconnect_policy(fast_test_policy());
        orch.notify_did_enter_background(30_000);

        orch.notify_will_enter_foreground();

        assert_eq!(cb.foreground_resumes.lock().unwrap().as_slice(), &[false]);
    }

    #[test]
    fn notify_will_enter_foreground_after_budget_expired_triggers_reconnect() {
        let (orch, _cb, attempt_count) = orchestrator_connected_with_reconnect_policy(fast_test_policy());
        orch.notify_did_enter_background(30_000);
        orch.notify_background_budget_expired();
        assert_eq!(orch.shared.state.lock().reconnect.background_state, BackgroundState::Suspended);

        orch.notify_will_enter_foreground();
        assert_eq!(orch.shared.state.lock().reconnect.background_state, BackgroundState::Foreground);
        assert_eq!(
            attempt_count.load(std::sync::atomic::Ordering::SeqCst), 1,
            "猶予切れ(Suspended)からの復帰では直前の接続設定で再接続を試みるはず"
        );
    }

    #[test]
    fn notify_will_enter_foreground_after_budget_expired_fires_on_foreground_resume_true() {
        let (orch, cb, _attempt_count) = orchestrator_connected_with_reconnect_policy(fast_test_policy());
        orch.notify_did_enter_background(30_000);
        orch.notify_background_budget_expired();

        orch.notify_will_enter_foreground();

        assert_eq!(cb.foreground_resumes.lock().unwrap().as_slice(), &[true]);
    }

    #[test]
    fn notify_will_enter_foreground_does_not_double_trigger_when_reconnect_loop_already_active() {
        let (orch, _cb, attempt_count) = orchestrator_connected_with_reconnect_policy(fast_test_policy());
        orch.notify_did_enter_background(30_000);
        orch.notify_background_budget_expired();

        // 既に(別経路の)自動再接続ループが動作中だとして立てておく。
        orch.shared.state.lock().reconnect.reconnect_loop_active = true;

        orch.notify_will_enter_foreground();
        assert_eq!(
            attempt_count.load(std::sync::atomic::Ordering::SeqCst), 0,
            "既に自動再接続ループが動作中なら二重に接続を試みてはいけない"
        );
    }

    #[test]
    fn notify_will_enter_foreground_fires_true_when_reconnect_loop_already_active() {
        let (orch, cb, attempt_count) = orchestrator_connected_with_reconnect_policy(fast_test_policy());
        orch.notify_did_enter_background(30_000);
        orch.notify_background_budget_expired();

        orch.shared.state.lock().reconnect.reconnect_loop_active = true;

        orch.notify_will_enter_foreground();
        assert_eq!(attempt_count.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(cb.foreground_resumes.lock().unwrap().as_slice(), &[true]);
    }

    #[test]
    fn notify_will_enter_foreground_does_not_trigger_while_a_connect_is_already_in_flight() {
        let (orch, _cb, attempt_count) = orchestrator_connected_with_reconnect_policy(fast_test_policy());
        orch.notify_did_enter_background(30_000);
        orch.notify_background_budget_expired();

        orch.shared.state.lock().reconnect.force_phase_for_test(ConnPhase::Connecting);

        orch.notify_will_enter_foreground();
        assert_eq!(
            attempt_count.load(std::sync::atomic::Ordering::SeqCst), 0,
            "既に接続試行中なら二重に接続を試みてはいけない"
        );
    }

    #[test]
    fn notify_will_enter_foreground_fires_true_when_a_connect_is_already_in_flight() {
        let (orch, cb, attempt_count) = orchestrator_connected_with_reconnect_policy(fast_test_policy());
        orch.notify_did_enter_background(30_000);
        orch.notify_background_budget_expired();

        orch.shared.state.lock().reconnect.force_phase_for_test(ConnPhase::Connecting);

        orch.notify_will_enter_foreground();
        assert_eq!(attempt_count.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(cb.foreground_resumes.lock().unwrap().as_slice(), &[true]);
    }

    #[test]
    fn notify_will_enter_foreground_fires_true_when_quiescing_with_reconnect_loop_active() {
        // 実装後レビューS-1の再発防止線: `handle_unexpected_disconnect`の
        // `Action::StartLoop`/`Suppress`経路(`:776-784`)は`background_state`を
        // 書き換えない。したがって`Quiescing`(猶予内)の間にバックグラウンドで
        // 接続が切れて自動再接続ループが始まっても`background_state`は
        // `Quiescing`のままになりうる。`was_suspended`だけを見ると
        // このケースを見落として`false`(「接続は維持されています」)を
        // 発火してしまう(B2と同型の穴)。`reconnect_loop_active`も見ることで
        // これを塞ぐ。
        let (orch, cb, _attempt_count) = orchestrator_connected_with_reconnect_policy(fast_test_policy());
        orch.notify_did_enter_background(30_000);
        assert_eq!(orch.shared.state.lock().reconnect.background_state, BackgroundState::Quiescing);

        orch.shared.state.lock().reconnect.reconnect_loop_active = true;

        orch.notify_will_enter_foreground();

        assert_eq!(cb.foreground_resumes.lock().unwrap().as_slice(), &[true]);
    }

    #[test]
    fn notify_will_enter_foreground_fires_true_when_quiescing_with_phase_not_connected() {
        // 実装後レビューS-1の再発防止線(2件目): `reconnect_loop_active`は立って
        // いないが`phase`が`Connected`でない(例: 切断直後で`Idle`)場合も同様に
        // 「復帰時点で接続が生きていない」ので`true`が正しい。
        let (orch, cb, _attempt_count) = orchestrator_connected_with_reconnect_policy(fast_test_policy());
        orch.notify_did_enter_background(30_000);

        orch.shared.state.lock().reconnect.force_phase_for_test(ConnPhase::Idle);

        orch.notify_will_enter_foreground();

        assert_eq!(cb.foreground_resumes.lock().unwrap().as_slice(), &[true]);
    }

    #[test]
    fn notify_will_enter_foreground_is_noop_without_prior_backgrounding() {
        let (orch, cb, attempt_count) = orchestrator_connected_with_reconnect_policy(fast_test_policy());
        orch.notify_will_enter_foreground();
        assert_eq!(attempt_count.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(orch.shared.state.lock().reconnect.background_state, BackgroundState::Foreground);
        assert!(cb.foreground_resumes.lock().unwrap().is_empty());
    }

    #[test]
    fn begin_connect_resets_background_state_to_foreground() {
        let (orch, _cb) = orchestrator_with_phase(ConnPhase::Idle, false);
        orch.shared.state.lock().reconnect.background_state = BackgroundState::Suspended;
        let _ = orch.begin_connect(ssh_attempt("example.com"));
        assert_eq!(orch.shared.state.lock().reconnect.background_state, BackgroundState::Foreground);
    }

    #[test]
    fn handle_unexpected_disconnect_without_auto_reconnect_resets_background_state() {
        // 自動再接続ループが始まらない切断(ここではuser_initiated)は、以降の
        // notify_will_enter_foreground()が誤って再接続を試みないようbackground_stateを
        // Foregroundへ戻す。
        let (adapter, shared, _cb) = adapter_with_phase(ConnPhase::Connected, false);
        {
            let mut s = shared.state.lock();
            s.reconnect.background_state = BackgroundState::Suspended;
            s.reconnect.user_initiated_disconnect = true;
        }
        adapter.on_disconnected(Some("user disconnected".to_string()));
        assert_eq!(shared.state.lock().reconnect.background_state, BackgroundState::Foreground);
    }

    #[test]
    fn notify_did_enter_background_while_connecting_survives_into_quiescing_after_connected() {
        // codexレビュー指摘の再現: Connecting中にバックグラウンド化し、その猶予中に
        // 接続が成立したケース。on_connected()自体はbackground_stateに触れないため、
        // notify_did_enter_background()の時点でConnectingも対象に含めておく必要がある。
        let (orch, cb) = orchestrator_with_phase(ConnPhase::Connecting, false);
        orch.shared.state.lock().set_last_connect_attempt(LastConnectAttempt::Ssh(test_ssh_config()));

        orch.notify_did_enter_background(30_000);
        assert_eq!(orch.shared.state.lock().reconnect.background_state, BackgroundState::Quiescing);

        let adapter = OrchestratorAdapter::new(orch.shared.clone());
        adapter.on_connected();
        assert_eq!(
            orch.shared.state.lock().reconnect.background_state, BackgroundState::Quiescing,
            "on_connected()はbackground_stateに触れないので猶予追跡は続いているはず"
        );

        orch.notify_background_budget_expired();
        assert_eq!(orch.shared.state.lock().reconnect.background_state, BackgroundState::Suspended);
        let _ = cb;
    }

    fn orchestrator_connected_with_failing_reconnect(
    ) -> (SessionOrchestrator, Arc<RecordingCallback>, Arc<std::sync::atomic::AtomicUsize>) {
        let callback = Arc::new(RecordingCallback::default());
        let attempt_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = attempt_count.clone();
        let shared = Arc::new(OrchestratorShared {
            state: Mutex::new(reconnect_test_state(ReconnectPolicy::default())),
            callback: callback.clone(),
            session: Mutex::new(None),
            path_observer: Mutex::new(net_health_policy::PathObserver::default()),
            app_pane_id: crate::tmux_locator::AppPaneId::generate_process_local(),
            // codexレビュー指摘: 実際の`connect_via`は`phase = Connecting`にしてから
            // 同期的に失敗し得るため、フェイクも同じ手順(先にConnectingへ変更してから
            // Errを返す)を踏んで、`notify_will_enter_foreground`側の`phase`復旧処理が
            // 本当に固着状態を解消しているかを検証できるようにする。
            reconnect_attempt: Box::new(move |shared, _attempt| {
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let _ = begin_reconnect_session(shared);
                Err(SshError::ConnectionFailed)
            }),
            reconnect_wake: tokio::sync::Notify::new(),
            rt: RUNTIME.handle().clone(),
        });
        (SessionOrchestrator { shared }, callback, attempt_count)
    }

    #[test]
    fn notify_will_enter_foreground_resets_phase_and_notifies_when_reconnect_fails_synchronously() {
        // codexレビュー指摘の再現: フォアグラウンド復帰契機の再接続がホスト鍵拒否等で
        // 同期的に失敗した場合、phaseがConnectingへ固まらずIdleへ戻り、UIへ
        // Disconnectedが通知されることを確認する(自動再接続ループのように次tickでの
        // 暗黙リトライが無い一回限りの呼び出しのため、Errを握り潰してはいけない)。
        let (orch, cb, attempt_count) = orchestrator_connected_with_failing_reconnect();
        orch.notify_did_enter_background(30_000);
        orch.notify_background_budget_expired();
        assert_eq!(orch.shared.state.lock().reconnect.background_state, BackgroundState::Suspended);

        orch.notify_will_enter_foreground();

        assert_eq!(attempt_count.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(
            orch.shared.state.lock().reconnect.phase == ConnPhase::Idle,
            "同期失敗後にphaseがConnectingのまま固まってはいけない"
        );
        let events = cb.connection_states.lock().unwrap();
        assert!(
            events.iter().any(|e| matches!(e, ConnectionPublicState::Disconnected { .. })),
            "同期失敗はDisconnectedとして通知されるはず, got: {events:?}"
        );
    }

    #[test]
    fn notify_will_enter_foreground_fires_true_even_when_reconnect_attempt_fails_synchronously() {
        let (orch, cb, attempt_count) = orchestrator_connected_with_failing_reconnect();
        orch.notify_did_enter_background(30_000);
        orch.notify_background_budget_expired();

        orch.notify_will_enter_foreground();

        assert_eq!(attempt_count.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(cb.foreground_resumes.lock().unwrap().as_slice(), &[true]);
        let events = cb.connection_states.lock().unwrap();
        assert!(
            events.iter().any(|e| matches!(e, ConnectionPublicState::Disconnected { .. })),
            "同期失敗はDisconnectedとして通知されるはず, got: {events:?}"
        );
        drop(events);
        assert_eq!(
            cb.event_order.lock().unwrap().as_slice(),
            &["foreground_resume", "connection_state_changed"],
            "on_foreground_resumeはreconnect_attempt(および同期失敗時のon_connection_state_changed)より前に発火するはず(round-3レビューS1)"
        );
    }

    // ── OrchestratorAdapter::on_notify (タスク#57) ───────────────

    #[test]
    fn on_notify_delivers_when_not_focused() {
        let (adapter, shared, cb) = adapter_with_phase(ConnPhase::Connected, false);
        shared.state.lock().tab_focused = false;
        adapter.on_notify(crate::NotifyKind::Bell, "tag-a".to_string(), 1);
        assert_eq!(cb.notifications.lock().unwrap().as_slice(), &[crate::NotifyKind::Bell]);
    }

    #[test]
    fn on_notify_suppresses_when_foreground_and_tab_focused() {
        let (adapter, shared, cb) = adapter_with_phase(ConnPhase::Connected, false);
        {
            let mut s = shared.state.lock();
            s.tab_focused = true;
            s.app_foreground = true;
        }
        adapter.on_notify(crate::NotifyKind::Activity, "tag-a".to_string(), 1);
        assert!(cb.notifications.lock().unwrap().is_empty());
    }

    #[test]
    fn on_notify_delivers_when_tab_focused_but_app_backgrounded() {
        // タブ自体はフォーカスされていても(Compose側の直近状態が古い等)、
        // アプリ全体がバックグラウンドならユーザーは見ていないので配信する。
        // `background_state`(再接続バジェットFSM)ではなく`app_foreground`
        // (生の前景/背景の事実)で判断することを確認する(2026-07-28の実機バグ修正)。
        let (adapter, shared, cb) = adapter_with_phase(ConnPhase::Connected, false);
        {
            let mut s = shared.state.lock();
            s.tab_focused = true;
            s.app_foreground = false;
        }
        adapter.on_notify(crate::NotifyKind::Silence, "tag-a".to_string(), 1);
        assert_eq!(cb.notifications.lock().unwrap().as_slice(), &[crate::NotifyKind::Silence]);
    }

    #[test]
    fn on_notify_suppresses_when_tab_focused_even_if_background_state_is_quiescing() {
        // regression: `background_state`が`Quiescing`(#20の再接続バジェットFSM)でも、
        // `app_foreground`が真の前景/背景の事実(`notify_did_enter_background`が
        // 未呼び出しでデフォルトのtrueのまま等)であれば抑制すべき——on_notifyは
        // `background_state`を一切見てはいけない。
        let (adapter, shared, cb) = adapter_with_phase(ConnPhase::Connected, false);
        {
            let mut s = shared.state.lock();
            s.tab_focused = true;
            s.app_foreground = true;
            s.reconnect.background_state = BackgroundState::Quiescing;
        }
        adapter.on_notify(crate::NotifyKind::Bell, "tag-a".to_string(), 1);
        assert!(cb.notifications.lock().unwrap().is_empty());
    }

    #[test]
    fn notify_did_enter_background_sets_app_foreground_false_unconditionally() {
        let (orch, _cb, _events) = orchestrator_connected_with_reconnect_policy(ReconnectPolicy::default());
        orch.notify_did_enter_background(0);
        assert!(!orch.shared.state.lock().app_foreground);
    }

    #[test]
    fn notify_will_enter_foreground_sets_app_foreground_true_unconditionally() {
        let (orch, _cb, _events) = orchestrator_connected_with_reconnect_policy(ReconnectPolicy::default());
        orch.notify_did_enter_background(0);
        assert!(!orch.shared.state.lock().app_foreground);
        orch.notify_will_enter_foreground();
        assert!(orch.shared.state.lock().app_foreground);
    }

    #[test]
    fn on_notify_drops_exact_duplicate_tag_and_seq() {
        let (adapter, shared, cb) = adapter_with_phase(ConnPhase::Connected, false);
        shared.state.lock().tab_focused = false;
        adapter.on_notify(crate::NotifyKind::Bell, "tag-a".to_string(), 5);
        adapter.on_notify(crate::NotifyKind::Bell, "tag-a".to_string(), 5);
        assert_eq!(
            cb.notifications.lock().unwrap().as_slice(),
            &[crate::NotifyKind::Bell],
            "the exact same (tmux_tag, seq) pair must be delivered only once"
        );
    }

    #[test]
    fn on_notify_delivers_a_different_seq_for_the_same_tag() {
        let (adapter, shared, cb) = adapter_with_phase(ConnPhase::Connected, false);
        shared.state.lock().tab_focused = false;
        adapter.on_notify(crate::NotifyKind::Bell, "tag-a".to_string(), 5);
        adapter.on_notify(crate::NotifyKind::Bell, "tag-a".to_string(), 6);
        assert_eq!(cb.notifications.lock().unwrap().len(), 2);
    }

    #[test]
    fn on_notify_drops_duplicate_even_when_a_different_tag_arrived_in_between() {
        // 直前1件だけを覚える実装(Option<(String, u64)>)だと、session group内の
        // 別ウィンドウのタグが交互に届いた場合に重複排除が破れていた(opusレビュー
        // 指摘)。recent_notify_seqsが複数件覚えることで、tag-aの重複がtag-bを
        // 挟んでも検出できることを確認する。
        let (adapter, shared, cb) = adapter_with_phase(ConnPhase::Connected, false);
        shared.state.lock().tab_focused = false;
        adapter.on_notify(crate::NotifyKind::Bell, "tag-a".to_string(), 5);
        adapter.on_notify(crate::NotifyKind::Bell, "tag-b".to_string(), 1);
        adapter.on_notify(crate::NotifyKind::Bell, "tag-a".to_string(), 5);
        assert_eq!(
            cb.notifications.lock().unwrap().len(),
            2,
            "tag-aの重複は、間にtag-bが挟まっても検出されるはず"
        );
    }

    #[test]
    fn on_notify_ignored_when_adapter_is_stale() {
        let (adapter, shared, cb) = adapter_with_phase(ConnPhase::Connected, false);
        shared.state.lock().tab_focused = false;
        // 新しいアダプタを生成すると`session_generation`が進み、古い`adapter`は
        // stale扱いになる(`is_current()`参照)。
        let _fresh = OrchestratorAdapter::new(shared.clone());
        adapter.on_notify(crate::NotifyKind::Bell, "tag-a".to_string(), 1);
        assert!(cb.notifications.lock().unwrap().is_empty());
    }

    // ── タスク#17: ファイルプレビュー ────────────────────

    #[test]
    fn on_file_preview_exec_result_resolves_a_pending_ls_request() {
        let (adapter, shared, cb) = adapter_with_phase(ConnPhase::Connected, false);
        shared.state.lock().pending_file_previews.insert(
            "req-1".to_string(),
            FilePreviewRequestKind::Ls { path: "/tmp".to_string() },
        );

        let stdout = br#"{"entries":[{"name":"a.txt","is_dir":false,"is_symlink":false,"size":3,"modified_unix":null}]}"#;
        adapter.on_file_preview_exec_result("req-1".to_string(), stdout.to_vec(), Some(0));

        assert!(
            !shared.state.lock().pending_file_previews.contains_key("req-1"),
            "解決済みのrequest_idはpendingマップから取り除かれるべき"
        );
        let outcomes = cb.file_preview_outcomes.lock().unwrap();
        assert_eq!(outcomes.len(), 1);
        match &outcomes[0] {
            FilePreviewOutcome::Ls { entries } => {
                assert_eq!(entries.len(), 1);
                assert_eq!(entries[0].name, "a.txt");
            }
            other => panic!("expected Ls outcome, got {other:?}"),
        }
    }

    #[test]
    fn on_file_preview_exec_result_for_unknown_request_id_reports_error() {
        let (adapter, _shared, cb) = adapter_with_phase(ConnPhase::Connected, false);
        adapter.on_file_preview_exec_result("never-requested".to_string(), b"{}".to_vec(), Some(0));
        let outcomes = cb.file_preview_outcomes.lock().unwrap();
        assert_eq!(outcomes.len(), 1);
        assert!(matches!(&outcomes[0], FilePreviewOutcome::Error { .. }));
    }

    #[test]
    fn on_file_preview_exec_result_ignored_when_adapter_is_stale() {
        // #10/#22と同じ「古い世代からの遅延コールバックは無視する」パターン。
        let (shared, cb) = shared_with_phase(ConnPhase::Connected, false);
        let stale = OrchestratorAdapter::new(shared.clone());
        let _fresh = OrchestratorAdapter::new(shared.clone());
        shared.state.lock().pending_file_previews.insert(
            "req-1".to_string(),
            FilePreviewRequestKind::Ls { path: "/tmp".to_string() },
        );

        stale.on_file_preview_exec_result("req-1".to_string(), b"{\"entries\":[]}".to_vec(), Some(0));

        assert!(cb.file_preview_outcomes.lock().unwrap().is_empty());
        // 古いadapterからの呼び出しは無視されるので、pendingエントリも消費されずに残る。
        assert!(shared.state.lock().pending_file_previews.contains_key("req-1"));
    }

    #[test]
    fn file_preview_request_when_not_connected_reports_error_immediately() {
        let (orch, cb) = orchestrator_with_phase(ConnPhase::Idle, false);
        orch.file_preview_request("req-1".to_string(), FilePreviewRequestKind::Ls { path: "/tmp".to_string() });

        let outcomes = cb.file_preview_outcomes.lock().unwrap();
        assert_eq!(outcomes.len(), 1);
        assert!(matches!(&outcomes[0], FilePreviewOutcome::Error { .. }));
        assert!(
            !orch.shared.state.lock().pending_file_previews.contains_key("req-1"),
            "即座にエラー応答した場合はpendingマップに残してはいけない"
        );
    }

    /// `SessionOrchestrator`の公開API(resize/disconnect/scrollback_*)が、実際に
    /// `session.lock()`へ格納された`ActiveSession`まで届いているかを検証するe2eテスト群。
    ///
    /// 既存のテストヘルパー(`orchestrator_with_phase`等)は`session: Mutex::new(None)`
    /// で固定されており、これらのメソッドが呼ぶ`self.shared.session.lock().as_ref()`が
    /// 常に`None`のまま——つまり委譲先のコードが一度も実行されない。これがcargo-mutants
    /// (2026-07-24、orchestrator.rs全218ミュータント走査)でSessionOrchestrator本体の
    /// 公開メソッドの大半がmissed判定になった直接の原因。ここでは
    /// `transport::ssh_handler::pooling_e2e_tests`と同じパターン(in-process russh
    /// serverへの実接続)で`SessionOrchestrator::connect()`を実際に呼び、`session`へ
    /// 本物の`ActiveSession::Ssh`が格納された状態を作ってから各メソッドを検証する
    /// (`isekai-ssh-e2e-test-self-containment-convention`に倣い、モックサーバーは
    /// このモジュール内に自己完結させ、`ssh_handler.rs`側とは共有しない)。
    mod session_orchestrator_e2e_tests {
        use super::*;
        use russh::server::{self, Auth, Msg as ServerMsg, Session as ServerSession};
        use russh::{Channel as RusshChannel, ChannelId, CryptoVec, Pty};
        use russh_keys::ssh_key::private::Ed25519Keypair;
        use std::net::SocketAddr;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::time::Duration;
        use tokio::net::TcpListener as TokioTcpListener;
        use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};
        use crate::SshAuth;
        // transport/ssh_handler.rs・transport/forward.rsのテストと同型(このファイルは
        // その2つの合併集合そのもの)だったOrchestratorCallbackのno-op寄りテストダブルを
        // test_callbacks.rsへ共通化した。
        use crate::test_callbacks::{
            ForwardingOrchestratorCallback as TestCallback, OrchestratorTestEvent as TestEvent,
        };

        /// `MockServer::data`の挙動切り替え。`RecordingServer`(受信データをそのまま
        /// echoし返す)と`ScriptedServer`(echoせず`received`に記録するだけで、代わりに
        /// `shell_request`時に保存した`server::Handle`からテストコードが任意タイミングで
        /// バイトを能動的に送り込める)は、以前は別々の`Server`/`Handler`ペアだったが、
        /// `data()`以外の全メソッド(auth/pty/shell/window_change/channel_close/exec)は
        /// バイト単位で同一実装だったため1つの`MockServer`に統合し、モードだけで分岐する。
        #[derive(Clone, Copy, PartialEq)]
        enum MockServerMode {
            /// scrollback/focus/resize系のテスト向け(旧`RecordingServer`)。
            Echo,
            /// trzsz系のテスト向け(旧`ScriptedServer`)。
            RecordOnly,
        }

        #[derive(Clone)]
        struct MockServer {
            mode: MockServerMode,
            window_changes: Arc<StdMutex<Vec<(u32, u32)>>>,
            channel_closed: Arc<AtomicBool>,
            channel_handle: Arc<StdMutex<Option<(ChannelId, server::Handle)>>>,
            received: Arc<StdMutex<Vec<u8>>>,
        }

        impl MockServer {
            fn new(mode: MockServerMode) -> Self {
                Self {
                    mode,
                    window_changes: Arc::new(StdMutex::new(Vec::new())),
                    channel_closed: Arc::new(AtomicBool::new(false)),
                    channel_handle: Arc::new(StdMutex::new(None)),
                    received: Arc::new(StdMutex::new(Vec::new())),
                }
            }
        }

        impl server::Server for MockServer {
            type Handler = MockServer;
            fn new_client(&mut self, _: Option<SocketAddr>) -> MockServer {
                self.clone()
            }
        }

        #[async_trait::async_trait]
        impl server::Handler for MockServer {
            type Error = russh::Error;

            async fn auth_publickey(
                &mut self, _user: &str, _public_key: &russh_keys::ssh_key::PublicKey,
            ) -> Result<Auth, Self::Error> {
                Ok(Auth::Accept)
            }

            async fn channel_open_session(
                &mut self, _channel: RusshChannel<ServerMsg>, _session: &mut ServerSession,
            ) -> Result<bool, Self::Error> {
                Ok(true)
            }

            async fn pty_request(
                &mut self, channel: ChannelId, _term: &str, _cols: u32, _rows: u32,
                _pix_width: u32, _pix_height: u32, _modes: &[(Pty, u32)], session: &mut ServerSession,
            ) -> Result<(), Self::Error> {
                session.channel_success(channel)?;
                Ok(())
            }

            async fn shell_request(
                &mut self, channel: ChannelId, session: &mut ServerSession,
            ) -> Result<(), Self::Error> {
                session.channel_success(channel)?;
                // ScriptedServer相当の用途(trzsz系)でのみ実際に読まれるが、常に保存して
                // おいても他モードには無害(誰も読まない)。
                *self.channel_handle.lock().unwrap() = Some((channel, session.handle()));
                Ok(())
            }

            async fn window_change_request(
                &mut self, _channel: ChannelId, col_width: u32, row_height: u32,
                _pix_width: u32, _pix_height: u32, _session: &mut ServerSession,
            ) -> Result<(), Self::Error> {
                self.window_changes.lock().unwrap().push((col_width, row_height));
                Ok(())
            }

            async fn data(
                &mut self, channel: ChannelId, data: &[u8], session: &mut ServerSession,
            ) -> Result<(), Self::Error> {
                match self.mode {
                    MockServerMode::Echo => {
                        session.data(channel, CryptoVec::from(data.to_vec()))?;
                    }
                    MockServerMode::RecordOnly => {
                        self.received.lock().unwrap().extend_from_slice(data);
                    }
                }
                Ok(())
            }

            async fn channel_close(
                &mut self, _channel: ChannelId, _session: &mut ServerSession,
            ) -> Result<(), Self::Error> {
                self.channel_closed.store(true, Ordering::SeqCst);
                Ok(())
            }

            /// `file_preview_exec`(タスク#17)が開く別チャネルでの`isekai-pipe ctl file`
            /// exec要求。コマンド内容は見ず、常に`ctl_file.rs`の`ls`成功レスポンス
            /// (JSON)を1件返す——本テストで検証したいのは`SessionOrchestrator::
            /// file_preview_request`が実transportまで委譲されるかどうかであり、
            /// exec先の実際のコマンド分岐はこのモジュールの対象外。
            async fn exec_request(
                &mut self, channel: ChannelId, _data: &[u8], session: &mut ServerSession,
            ) -> Result<(), Self::Error> {
                session.channel_success(channel)?;
                let stdout = br#"{"entries":[{"name":"a.txt","is_dir":false,"is_symlink":false,"size":5,"modified_unix":1700000000}]}"#;
                session.data(channel, CryptoVec::from(stdout.to_vec()))?;
                session.exit_status_request(channel, 0)?;
                session.close(channel)?;
                Ok(())
            }
        }

        async fn spawn_mock_server(mode: MockServerMode) -> (SocketAddr, MockServer) {
            let keypair = Ed25519Keypair::from_seed(&[7u8; 32]);
            let host_key = russh_keys::PrivateKey::from(keypair);
            let config = Arc::new(server::Config {
                keys: vec![host_key],
                ..Default::default()
            });
            let listener = TokioTcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let handle = MockServer::new(mode);
            let mut sh = handle.clone();
            tokio::spawn(async move {
                use server::Server as _;
                let _ = sh.run_on_socket(config, &listener).await;
            });
            (addr, handle)
        }

        async fn spawn_recording_server() -> (SocketAddr, Arc<StdMutex<Vec<(u32, u32)>>>, Arc<AtomicBool>) {
            let (addr, sh) = spawn_mock_server(MockServerMode::Echo).await;
            (addr, sh.window_changes, sh.channel_closed)
        }

        async fn spawn_scripted_server() -> (SocketAddr, Arc<StdMutex<Option<(ChannelId, server::Handle)>>>, Arc<StdMutex<Vec<u8>>>) {
            let (addr, sh) = spawn_mock_server(MockServerMode::RecordOnly).await;
            (addr, sh.channel_handle, sh.received)
        }

        fn key_auth(seed: u8) -> SshAuth {
            let keypair = Ed25519Keypair::from_seed(&[seed; 32]);
            let key = russh_keys::PrivateKey::from(keypair);
            SshAuth::PublicKey {
                private_key_pem: key.to_openssh(Default::default()).unwrap().as_bytes().to_vec(),
            }
        }

        fn ssh_config(host: SocketAddr, auth: SshAuth) -> SshConfig {
            SshConfig {
                host: host.ip().to_string(),
                port: host.port(),
                username: "tester".into(),
                auth,
                cols: 80,
                rows: 24,
                forwards: Vec::new(),
                agent_forward: false,
                jump: None,
                allow_non_loopback_forward_bind: false,
            }
        }

        async fn wait_connected(rx: &mut UnboundedReceiver<TestEvent>) {
            for _ in 0..50 {
                match tokio::time::timeout(Duration::from_millis(200), rx.recv()).await {
                    Ok(Some(TestEvent::Connection(ConnectionPublicState::Connected { .. }))) => return,
                    Ok(Some(TestEvent::Connection(ConnectionPublicState::Error { message }))) => {
                        panic!("connection reported Error before Connected: {message}");
                    }
                    _ => continue,
                }
            }
            panic!("did not become Connected within timeout");
        }

        async fn wait_disconnected(rx: &mut UnboundedReceiver<TestEvent>) {
            for _ in 0..50 {
                match tokio::time::timeout(Duration::from_millis(200), rx.recv()).await {
                    Ok(Some(TestEvent::Connection(ConnectionPublicState::Disconnected { .. }))) => return,
                    _ => continue,
                }
            }
            panic!("did not become Disconnected within timeout");
        }

        async fn wait_echo(rx: &mut UnboundedReceiver<TestEvent>, expected: &[u8]) {
            let mut got = Vec::new();
            for _ in 0..50 {
                if got.windows(expected.len().max(1)).any(|w| w == expected) {
                    return;
                }
                match tokio::time::timeout(Duration::from_millis(200), rx.recv()).await {
                    Ok(Some(TestEvent::Data(data))) => got.extend_from_slice(&data),
                    _ => continue,
                }
            }
            panic!("did not observe expected echo {:?} within timeout, got {:?}", expected, got);
        }

        async fn wait_file_preview_result(rx: &mut UnboundedReceiver<TestEvent>, expected_id: &str) -> FilePreviewOutcome {
            for _ in 0..50 {
                match tokio::time::timeout(Duration::from_millis(200), rx.recv()).await {
                    Ok(Some(TestEvent::FilePreview(id, outcome))) if id == expected_id => return outcome,
                    _ => continue,
                }
            }
            panic!("did not observe a FilePreviewOutcome for id={} within timeout", expected_id);
        }

        /// VTEでの画面反映は`on_screen_update`コールバック駆動の非同期処理なので、
        /// scrollbackへの反映が追いつくまで短時間ポーリングして`scrollback_len()`が
        /// 0より大きくなるのを待つ(2つのテストがこの手順を独立に持っていたので共通化)。
        /// 反映されなければ最後に観測した値(0)を返し、呼び出し側で`assert!(len > 0, ...)`
        /// させる(パニックメッセージをテストごとに変えたいため、ここではpanicしない)。
        async fn wait_scrollback_nonzero(orch: &SessionOrchestrator) -> u32 {
            let mut len = 0u32;
            for _ in 0..50 {
                len = orch.scrollback_len();
                if len > 0 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            len
        }

        async fn connect_orchestrator() -> (Arc<SessionOrchestrator>, UnboundedReceiver<TestEvent>, SocketAddr, Arc<StdMutex<Vec<(u32, u32)>>>, Arc<AtomicBool>) {
            let (addr, window_changes, channel_closed) = spawn_recording_server().await;
            let (tx, mut rx) = unbounded_channel::<TestEvent>();
            let orch = create_session_orchestrator(Box::new(TestCallback::new(tx)));
            orch.connect(ssh_config(addr, key_auth(1))).expect("connect should not fail synchronously");
            wait_connected(&mut rx).await;
            (orch, rx, addr, window_changes, channel_closed)
        }

        #[test]
        fn resize_forwards_window_change_to_the_real_transport() {
            crate::init_logger();
            let rt = tokio::runtime::Runtime::new().expect("failed to build test runtime");
            rt.block_on(async {
                let (orch, _rx, _addr, window_changes, _channel_closed) = connect_orchestrator().await;

                orch.resize(100, 40);

                // window_change_requestはchannelの非同期送信なので、サーバー側での
                // 記録が届くまで短時間ポーリングする。
                for _ in 0..50 {
                    if !window_changes.lock().unwrap().is_empty() {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                assert_eq!(
                    window_changes.lock().unwrap().as_slice(),
                    &[(100, 40)],
                    "SessionOrchestrator::resize()が実際のトランスポートまで届いていない \
                     (ActiveSession::resizeがno-opに変異してもこのテストは検知できる)"
                );
            });
        }

        #[test]
        fn disconnect_tears_down_the_real_transport_and_notifies_disconnected() {
            crate::init_logger();
            let rt = tokio::runtime::Runtime::new().expect("failed to build test runtime");
            rt.block_on(async {
                let (orch, mut rx, _addr, _window_changes, _channel_closed) = connect_orchestrator().await;

                orch.disconnect();
                // ActiveSession::disconnectが実際に呼ばれない(no-opに変異する)限り、
                // 実コネクションは生きたままなのでDisconnectedコールバックは発火しない
                // ——wait_disconnectedのタイムアウトpanicがこの変異を検知する。
                wait_disconnected(&mut rx).await;
            });
        }

        #[test]
        fn scrollback_len_and_cells_reflect_real_terminal_output() {
            crate::init_logger();
            let rt = tokio::runtime::Runtime::new().expect("failed to build test runtime");
            rt.block_on(async {
                let (orch, mut rx, _addr, _window_changes, _channel_closed) = connect_orchestrator().await;

                // 80x24の画面をあふれさせるのに十分な行数を送り、実際にscrollbackへ
                // 積ませる。各行末で改行しechoさせる。
                for i in 0..60 {
                    orch.send(format!("line-{i:03}\r\n").into_bytes());
                }
                wait_echo(&mut rx, b"line-059").await;
                // VTEでの画面反映は非同期(on_screen_updateコールバック駆動)なので、
                // scrollbackへの反映が追いつくまで短時間ポーリングする。
                let len = wait_scrollback_nonzero(&orch).await;
                assert!(
                    len > 0,
                    "SessionOrchestrator::scrollback_len()が実際のtransport/terminal状態を \
                     反映していない(ActiveSession::scrollback_lenが0固定に変異してもこのテストは検知できる)"
                );

                let cells = orch.scrollback_cells(0, 1);
                assert!(
                    !cells.is_empty(),
                    "SessionOrchestrator::scrollback_cells()が実際のterminal状態を反映していない \
                     (ActiveSession::scrollback_cellsがvec![]に変異してもこのテストは検知できる)"
                );
            });
        }

        // ── trzsz_accept_download / trzsz_accept_upload / trzsz_cancel ──
        //
        // 上のRecordingServer相当(`MockServerMode::Echo`)はクライアントからのデータを
        // 無条件にechoするだけなので、サーバー側が任意タイミングで能動的にバイトを送れる
        // `MockServerMode::RecordOnly`を代わりに使う(`session.handle()`を`shell_request`
        // 時に保存し、テストコードから`Handle::data()`で直接送り込む)。これにより実物の
        // trzszトリガー/CFG/NUMフレームをワイヤ上に流し、`SessionOrchestrator::
        // trzsz_accept_download`等の委譲がno-opに変異した場合に実際に検知できるテストを
        // 組む(`spawn_scripted_server`は上の`MockServer`定義の直後を参照)。

        async fn connect_scripted_orchestrator() -> (
            Arc<SessionOrchestrator>, UnboundedReceiver<TestEvent>,
            Arc<StdMutex<Option<(ChannelId, server::Handle)>>>, Arc<StdMutex<Vec<u8>>>,
        ) {
            let (addr, channel_handle, received) = spawn_scripted_server().await;
            let (tx, mut rx) = unbounded_channel::<TestEvent>();
            let orch = create_session_orchestrator(Box::new(TestCallback::new(tx)));
            orch.connect(ssh_config(addr, key_auth(1))).expect("connect should not fail synchronously");
            wait_connected(&mut rx).await;
            (orch, rx, channel_handle, received)
        }

        /// `shell_request`が届き`MockServer`(`RecordOnly`モード)が`Handle`を保存するまで待ってから、
        /// テストコードから能動的にバイトをクライアントへ送り込む
        /// (実物のtrzszトリガー/CFG/NUMフレームをそのままワイヤに流すために使う)。
        async fn send_from_server(slot: &Arc<StdMutex<Option<(ChannelId, server::Handle)>>>, bytes: Vec<u8>) {
            let (id, handle) = {
                let mut found = None;
                for _ in 0..50 {
                    if let Some(v) = slot.lock().unwrap().clone() {
                        found = Some(v);
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                found.expect("shell was not requested within timeout")
            };
            handle.data(id, CryptoVec::from(bytes)).await.expect("failed to send scripted bytes to client");
        }

        async fn wait_received_contains(received: &Arc<StdMutex<Vec<u8>>>, needle: &[u8]) {
            for _ in 0..50 {
                if received.lock().unwrap().windows(needle.len().max(1)).any(|w| w == needle) {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            panic!(
                "did not observe expected bytes {:?} arriving at the server within timeout, got {:?}",
                String::from_utf8_lossy(needle), String::from_utf8_lossy(&received.lock().unwrap())
            );
        }

        fn trzsz_trigger(mode: &str) -> Vec<u8> {
            format!("::TRZSZ:TRANSFER:{mode}:1.1.7:0000004e\n").into_bytes()
        }

        fn trzsz_frame(typ: &str, payload: &str) -> Vec<u8> {
            format!("#{typ}:{payload}\n").into_bytes()
        }

        fn trzsz_encode_bytes(buf: &[u8]) -> String {
            use std::io::Write;
            use base64::Engine;
            let mut enc = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
            let _ = enc.write_all(buf);
            let compressed = enc.finish().unwrap_or_default();
            base64::engine::general_purpose::STANDARD.encode(compressed)
        }

        fn trzsz_frame_bin(typ: &str, buf: &[u8]) -> Vec<u8> {
            trzsz_frame(typ, &trzsz_encode_bytes(buf))
        }

        fn trzsz_frame_int(typ: &str, val: u64) -> Vec<u8> {
            trzsz_frame(typ, &val.to_string())
        }

        fn trzsz_cfg_frame() -> Vec<u8> {
            let json = r#"{"lang":"go","version":"1.1.5","binary":false,"directory":false,"bufsize":1048576,"timeout":10}"#;
            trzsz_frame_bin("CFG", json.as_bytes())
        }

        #[test]
        fn trzsz_accept_download_then_cancel_drive_the_real_transfer_fsm() {
            crate::init_logger();
            let rt = tokio::runtime::Runtime::new().expect("failed to build test runtime");
            rt.block_on(async {
                let (orch, _rx, channel_handle, received) = connect_scripted_orchestrator().await;

                // 1. サーバー(送信側)から実物のdownloadトリガーを送る。クライアントは
                //    トリガー検出直後に自動でACTを実transport経由で送り返す。
                send_from_server(&channel_handle, trzsz_trigger("S")).await;
                wait_received_contains(&received, b"#ACT:").await;

                // 2. CFGを先行して送っておく。この時点ではまだWaitingKotlin状態なので
                //    proto_bufにバッファされるだけで応答は起きない。
                send_from_server(&channel_handle, trzsz_cfg_frame()).await;

                // 3. accept_downloadを呼ぶ。ActiveSession::trzsz_accept_downloadが
                //    no-opに変異していれば状態はWaitingKotlinのまま変わらず、
                //    バッファされたCFGは永遠に処理されない。
                orch.trzsz_accept_download();

                // 4. accept_downloadが実transportまで届いていれば、バッファ済みCFGが
                //    即座に処理されWaitNum状態になっているはず。ここでNUMを送り、
                //    クライアントがSUCC:1で応答することを確認する
                //    (mutantが生きていればこのSUCCは永遠に届かずタイムアウトする)。
                send_from_server(&channel_handle, trzsz_frame_int("NUM", 1)).await;
                wait_received_contains(&received, b"#SUCC:1\n").await;

                // 5. trzsz_cancel()が実transportにCtrl+C(0x03)を送ることを確認する
                //    (ActiveSession::trzsz_cancelがno-opに変異してもこのテストは検知できる)。
                orch.trzsz_cancel();
                wait_received_contains(&received, &[0x03]).await;
            });
        }

        #[test]
        fn trzsz_accept_upload_drives_the_real_transfer_fsm() {
            crate::init_logger();
            let rt = tokio::runtime::Runtime::new().expect("failed to build test runtime");
            rt.block_on(async {
                let (orch, _rx, channel_handle, received) = connect_scripted_orchestrator().await;

                // 1. サーバーから実物のuploadトリガー("R")を送る → クライアントは
                //    ACTを実transport経由で送り返す。
                send_from_server(&channel_handle, trzsz_trigger("R")).await;
                wait_received_contains(&received, b"#ACT:").await;

                // 2. accept_uploadを呼ぶ。ActiveSession::trzsz_accept_uploadがno-opに
                //    変異していれば状態はWaitingKotlinのまま変わらない。
                orch.trzsz_accept_upload("scripted.bin".to_string(), 5, 0o644);

                // 3. accept_uploadが実transportまで届いていれば、次にCFGを受け取った
                //    瞬間にNUM/NAME/SIZEを能動的に送り返してくるはず
                //    (mutantが生きていればCFGはWaitingKotlinのproto_bufに積まれるだけで
                //    何も送り返されずタイムアウトする)。
                send_from_server(&channel_handle, trzsz_cfg_frame()).await;
                wait_received_contains(&received, b"#NUM:1\n").await;
                wait_received_contains(&received, b"#NAME:").await;
                wait_received_contains(&received, b"#SIZE:5\n").await;

                orch.trzsz_cancel();
                wait_received_contains(&received, &[0x03]).await;
            });
        }

        // ── notify_focus_change ──

        #[test]
        fn notify_focus_change_forwards_focus_events_to_the_real_transport() {
            crate::init_logger();
            let rt = tokio::runtime::Runtime::new().expect("failed to build test runtime");
            rt.block_on(async {
                let (orch, _rx, channel_handle, received) = connect_scripted_orchestrator().await;

                // フォーカスレポーティング(CSI ?1004h、タスク#60)はterminalの
                // 明示的なDECSETが無い限り既定offなので、まずサーバーから有効化させる。
                send_from_server(&channel_handle, b"\x1b[?1004h".to_vec()).await;

                // DECSETのVTE処理は`on_data`コールバック駆動の非同期パスなので、
                // 有効化が反映されるまで`notify_focus_change`を無害に(有効化前は
                // encode_focus_eventがNoneを返すだけで何も送られない)ポーリングする。
                // ActiveSession::notify_focus_changeがno-opに変異していれば、
                // 有効化後もずっとバイトが届かずタイムアウトする。
                for _ in 0..50 {
                    orch.notify_focus_change(true);
                    if received.lock().unwrap().windows(3).any(|w| w == b"\x1b[I") {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                wait_received_contains(&received, b"\x1b[I").await;

                orch.notify_focus_change(false);
                wait_received_contains(&received, b"\x1b[O").await;
            });
        }

        // ── set_session_theme ──

        #[test]
        fn set_session_theme_recolors_newly_written_cells_via_the_real_transport() {
            crate::init_logger();
            let rt = tokio::runtime::Runtime::new().expect("failed to build test runtime");
            rt.block_on(async {
                let (orch, mut rx, _addr, _window_changes, _channel_closed) = connect_orchestrator().await;

                const CUSTOM_FG: u32 = 0xFF123456;
                const CUSTOM_BG: u32 = 0xFF654321;
                orch.set_session_theme(Vec::new(), CUSTOM_FG, CUSTOM_BG);

                // 80x24をあふれさせるのに十分な行数を送り、テーマ変更後に書かれた
                // セルを実際にscrollbackへ積ませる(既存のscrollback_len_and_cellsテストと
                // 同じ手法。set_session_themeは「以降書かれるセルにのみ反映され、
                // 既にscrollbackへ積まれたセルは遡って再着色されない」設計なので、
                // テーマ変更を送信より先に行う必要がある)。
                //
                // SGR属性の実効色は「実行時点」でtheme.default_fg/bgから解決されて
                // cur_attrsにスナップショットされる(以降テーマが変わっても、明示的な
                // SGRリセットが無い限り再解決されない)。MockServer(Echoモード)が素通しで
                // echoする性質を利用し、`\x1b[0m`をecho往復させてからテキストを送ることで
                // 新テーマでの解決を強制する。
                orch.send(b"\x1b[0m".to_vec());
                for i in 0..60 {
                    orch.send(format!("line-{i:03}\r\n").into_bytes());
                }
                wait_echo(&mut rx, b"line-059").await;

                let len = wait_scrollback_nonzero(&orch).await;
                assert!(len > 0, "scrollbackへの反映を待つ準備ができていない");

                let cells = orch.scrollback_cells(0, 1);
                assert!(!cells.is_empty());
                assert_eq!(
                    cells[0].fg, CUSTOM_FG,
                    "SessionOrchestrator::set_session_themeが実transportまで届いていない \
                     (ActiveSession::set_themeがno-opに変異してもこのテストは検知できる)"
                );
                assert_eq!(cells[0].bg, CUSTOM_BG);
            });
        }

        // ── file_preview_request ──

        #[test]
        fn file_preview_request_execs_over_the_real_transport_and_reports_the_result() {
            crate::init_logger();
            let rt = tokio::runtime::Runtime::new().expect("failed to build test runtime");
            rt.block_on(async {
                let (orch, mut rx, _addr, _window_changes, _channel_closed) = connect_orchestrator().await;

                orch.file_preview_request(
                    "req-1".to_string(),
                    crate::file_preview::FilePreviewRequestKind::Ls { path: "/tmp".to_string() },
                );

                // SessionOrchestrator::file_preview_requestがno-op(session.file_preview_execを
                // 呼ばない)に変異していれば、`queued`は常にfalseとなり即座に
                // FilePreviewOutcome::Error{"not connected"}が同期的に返る。実transportまで
                // 委譲されていれば、MockServerのexec_requestが返す実物のls JSONが
                // 非同期に届くはず。
                let outcome = wait_file_preview_result(&mut rx, "req-1").await;
                match outcome {
                    FilePreviewOutcome::Ls { entries } => {
                        assert_eq!(entries.len(), 1);
                        assert_eq!(entries[0].name, "a.txt");
                    }
                    other => panic!(
                        "expected a real FilePreviewOutcome::Ls from the exec channel, got {:?} \
                         (ActiveSession::file_preview_execがno-opに変異すると即座に \
                         Error{{\"not connected\"}}が返る)",
                        other
                    ),
                }
            });
        }
    }
}
