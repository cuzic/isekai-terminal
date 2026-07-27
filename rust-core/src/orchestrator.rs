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
use crate::tmux_locator::{RemoteTmuxCommandRunner, TmuxLocator, TmuxRunError};
use crate::tmux_scrollback::fetch_tmux_scrollback_history;

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
/// 純粋な委譲にのみ使う。`add_local_forward`/`remove_forward`のように一部の
/// トランスポートで挙動が違うメソッドは対象外とし、手書きのmatchのままにする。
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
    /// (`Terminal`/`SessionCore`はトランスポート非依存のため`add_local_forward`と違い
    /// 対象外の分岐は無い)。
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
    /// (`transport/ssh_handler.rs`のモジュールdoc参照)、`add_local_forward`と違い
    /// トランスポート別の対応可否分岐は無い——全バリアントで同じ委譲でよい。
    fn file_preview_exec(&self, request_id: String, command_line: String) -> bool {
        dispatch_all!(self, file_preview_exec, request_id, command_line)
    }
    fn add_local_forward(&self, id: String, bind_address: String, bind_port: u16, remote_host: String, remote_port: u16) {
        match self {
            Self::Ssh(s) => s.add_local_forward(id, bind_address, bind_port, remote_host, remote_port),
            Self::Quic(s) => s.add_local_forward(id, bind_address, bind_port, remote_host, remote_port),
            // ポートフォワードは MVP スコープ上プレーン SSH / tsshd QUIC のみ対応。
            // isekai-helper 経由の QUIC 系トランスポートは未対応（対象外）。
            Self::IsekaiPipeQuic(_) | Self::MultipathIsekaiPipeQuic(_) | Self::IsekaiStunP2p(_) | Self::IsekaiLinkRelay(_) => {
                log::warn!("add_local_forward: not supported over helper-QUIC transports");
            }
        }
    }
    fn remove_forward(&self, id: String) {
        match self {
            Self::Ssh(s) => s.remove_forward(id),
            Self::Quic(s) => s.remove_forward(id),
            Self::IsekaiPipeQuic(_) | Self::MultipathIsekaiPipeQuic(_) | Self::IsekaiStunP2p(_) | Self::IsekaiLinkRelay(_) => {
                log::warn!("remove_forward: not supported over helper-QUIC transports");
            }
        }
    }
    /// Phase 12: per-session theme。全トランスポート共通(`Terminal`/`SessionCore`は
    /// トランスポート非依存)なので、`add_local_forward`と違い対象外の分岐は無い。
    fn set_theme(&self, theme: crate::theme::Theme) {
        dispatch_all!(self, set_theme, theme)
    }
    /// タスク#61: 既存のインタラクティブチャネル/PTYに触れず、この(プール済み)
    /// 接続上で短命なexecコマンドを実行する。全トランスポート共通
    /// (`SessionCore::run_exec`)なので`add_local_forward`と違い対象外の分岐は無いが、
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
    /// タスク#58: tmux scrollback backfillのバッチ注入。全トランスポート共通
    /// (`SessionCore::inject_scrollback_history`)なので`dispatch_all!`でよい。
    fn inject_scrollback_history(&self, lines: Vec<String>) {
        dispatch_all!(self, inject_scrollback_history, lines)
    }
}

// ── Shared internal state ─────────────────────────────────

/// 接続状態の SSOT。`ConnectionPublicState` の Connecting/Connected の別を
/// Rust 側でも保持し、`notify_network_path_changed` がミラー無しで判断できるようにする。
#[derive(Clone, Copy, PartialEq, Eq)]
enum ConnPhase {
    Idle,
    Connecting,
    Connected,
}

/// #20: アプリのバックグラウンド遷移とセッション再接続要否のSSOT。
/// `session_supervisor.rs`が実装していた`SessionState`×`ExecutionMode`の8状態FSMを、
/// `SessionOrchestrator`本体の`ConnPhase`/`last_connect_attempt`と統合する形で
/// 必要最小限に絞り込んだもの(`Closing`/`Closed`はSwift/Kotlinの`disconnect()`と
/// アプリ終了処理で十分カバーされるため持たない、`Connecting`/`Resuming`は既存の
/// `ConnPhase`で表現済み)。UniFFIへは公開しない(Kotlin/Swiftは生イベントを送るだけで
/// よく、この状態自体を読んで分岐してはいけない、`rust-ssot.md`)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BackgroundState {
    /// フォアグラウンド相当、またはバックグラウンド遷移がそもそも意味を持たない
    /// (未接続・既に切断済み等)。
    Foreground,
    /// バックグラウンドへ遷移したが、まだ猶予(`budget_ms`)内。接続は生きている
    /// 前提でそのまま維持を試みる。
    Quiescing,
    /// バックグラウンド猶予が尽きた、またはメモリ逼迫警告を受けた。次の
    /// フォアグラウンド復帰時には接続が失われている前提で自動的に再接続する。
    Suspended,
}

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

/// 自動再接続ループのタイミング定数。`net_health_policy::NetPathPolicy`と同じ理由
/// (テストで短い値に差し替えられるようにする)で構造体化する。既定値はMVPとして
/// ハードコード(設定UIは作らない): tssh の `aliveTimeout` 相当が60秒。
#[derive(Debug, Clone, Copy)]
struct ReconnectPolicy {
    /// UIへライブ通知する間隔。
    tick: Duration,
    /// 実際に`connect_via`を試みる間隔(tickの整数倍)。
    retry_interval: Duration,
    /// これを超えて再接続できなければギブアップする。
    timeout: Duration,
}

impl Default for ReconnectPolicy {
    fn default() -> Self {
        Self {
            tick: Duration::from_secs(1),
            retry_interval: Duration::from_secs(3),
            timeout: Duration::from_secs(60),
        }
    }
}

/// trzsz ダウンロードの累積バッファに設ける上限(#60)。trzsz プロトコルの
/// `SIZE`(申告値)はサーバー側の自己申告に過ぎず強制されないため、悪意ある/
/// 壊れたサーバーが巨大な SIZE を申告して DATA を送り続けると `download_buf` が
/// 無制限に肥大化し端末が OOM でクラッシュし得る。実際に受信したバイト数の実測値
/// (`download_buf.len() + 今回のchunk長`)がこの上限を超えたら転送を中断する。
const MAX_DOWNLOAD_BUF_BYTES: usize = 2 * 1024 * 1024 * 1024; // 2 GiB

struct OrchestratorState {
    current_host: Option<String>,
    current_port: u16,
    is_quic: bool,
    phase: ConnPhase,
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
    /// 自動再接続ループ自身の生存確認用epoch。新しい`connect_*`呼び出し・
    /// `cancel_reconnect()`・再接続成功のいずれかでインクリメントされ、
    /// ループは次のtickで自分のepochが古いと分かれば静かに終了する。
    reconnect_epoch: u64,
    /// 自動再接続ループが現在動作中かどうか。`on_disconnected`が二重にループを
    /// 起動しない・二重に`Disconnected`を通知しないための判定に使う。
    reconnect_loop_active: bool,
    /// ループが`connect_via`を発火してから、その試行の結果(generation一致の
    /// `on_connected`/`on_disconnected`)を観測するまでの間true。次のtickで
    /// 新しい試行を重ねて発火しないためのガード(ホスト鍵確認プロンプトの
    /// 多重発生を防ぐ)。
    retry_attempt_in_flight: bool,
    /// `SessionOrchestrator::disconnect()`が呼ばれた際に立てる。ユーザーが
    /// 明示的に切断した場合は自動再接続しない(tsshの「唯一の例外」と同じ)。
    /// 読み取った直後にfalseへ戻す一度きりのフラグ。
    user_initiated_disconnect: bool,
    /// 直前に成功した(あるいは試みた)`connect_*`。予期しない切断時にこれを
    /// 使って自動的に再接続を試みる。
    last_connect_attempt: Option<LastConnectAttempt>,
    /// 再接続ループのタイミング。テストでは短い値に差し替える。
    reconnect_policy: ReconnectPolicy,
    /// #20: バックグラウンド遷移とセッション再接続要否のSSOT。
    background_state: BackgroundState,
    /// タスク#57: このタブが現在フォーカスされている(=Compose側の
    /// `isActive && hasFocus`)かどうか。`notify_focus_change`が
    /// `Terminal`のCSIフォーカスレポーティングと同じ生イベントから複製する
    /// (`rust-ssot.md`: 新しい判断ロジックのために新しいUniFFIメソッドを増やすのでは
    /// なく、既存の生イベント転送経路を再利用する)。`OrchestratorAdapter::on_notify`が
    /// `background_state`と合わせて「今この瞬間ユーザーがこのタブを見ているか」の
    /// 抑制判断に使う。
    tab_focused: bool,
    /// タスク#57: 直近配信した`(tmux_tag, seq)`の小さなリングバッファ。
    /// `isekai_protocol::CtlMessage::Notify`のdocコメントが想定する重複配信
    /// (tmux hookの再発火・session group内の複数メンバーからの重複起動、
    /// `tmux_notify.rs`のモジュールdoc参照)を、同じペアが来たら黙って無視する
    /// ことで検出する。1件だけ(`Option`)だと、session group内の別ウィンドウの
    /// タグが交互に届いた場合に重複排除が破れる(opusレビュー指摘)ため、
    /// [`RECENT_NOTIFY_SEQ_CAPACITY`]件までは覚えておく。
    recent_notify_seqs: std::collections::VecDeque<(String, u64)>,
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
    /// タスク#58: このオーケストレータが担当するタブ/ペインに対応するtmux
    /// ロケータ。フル再接続後のscrollback backfill(`spawn_tmux_scrollback_backfill`)
    /// が対象ペインを特定するために読む。`None`(既定)なら単にbackfillを
    /// fail-openでスキップする —— `ensure_tmux_tab_window`成功時に
    /// `set_tmux_backfill_locator`経由で設定される(#60が#62の
    /// `TmuxLocatorRegistry`へ登録するのと同じタイミング)。
    tmux_backfill_locator: Mutex<Option<TmuxLocator>>,
    /// タスク#58: `reconnect_attempt`(`connect_via`)が同期的に成功した直後
    /// (`spawn_reconnect_loop`の2箇所、および`notify_will_enter_foreground`の
    /// フォアグラウンド復帰時再接続)に一度だけ呼ばれるフック。実運用の既定は
    /// `spawn_tmux_scrollback_backfill`(tmux capture-paneベースのbackfillを
    /// `RUNTIME.spawn`で開始するだけで、この呼び出し自体はブロックしない)。
    /// `reconnect_attempt`と同じ理由(実ネットワーク/実tmuxに触れず「いつ・何回
    /// 呼ばれるか」だけを単体テストするため)でテストでは差し替え可能にしてある。
    /// 手動の`connect_*`(`SessionOrchestrator::connect`等)はこのフックを一切
    /// 経由しない —— `reconnect_attempt`同様`connect_via`専用の経路であり、
    /// 「resumeが尽きた/初回接続ではない」ことがこのフィールドが呼ばれる時点で
    /// 既に保証されている。
    after_reconnect_success: Box<dyn Fn(&Arc<OrchestratorShared>) + Send + Sync>,
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
    fn new(shared: Arc<OrchestratorShared>) -> Self {
        let generation = {
            let mut s = shared.state.lock();
            s.session_generation += 1;
            s.session_generation
        };
        Self { shared, generation }
    }

    /// このアダプタが今も「現行の」セッションのものかどうか。古い(既に見捨てられた)
    /// セッションからの遅延コールバックはこれがfalseになり、呼び出し元は無視する。
    fn is_current(&self) -> bool {
        self.shared.state.lock().session_generation == self.generation
    }
}

impl SessionCallback for OrchestratorAdapter {
    fn on_data(&self, data: Vec<u8>) {
        if !self.is_current() { return; }
        self.shared.callback.on_data(data);
    }

    fn on_host_key(&self, fingerprint: String) -> bool {
        if !self.is_current() { return false; }
        let (host, port) = {
            let s = self.shared.state.lock();
            (s.current_host.clone().unwrap_or_default(), s.current_port)
        };
        self.shared.callback.on_host_key(host, port, fingerprint)
    }

    fn on_connected(&self) {
        if !self.is_current() { return; }
        let host = {
            let mut s = self.shared.state.lock();
            s.phase = ConnPhase::Connected;
            // 再接続ループが動いていたなら、成功したのでここで止める。
            s.reconnect_epoch += 1;
            s.reconnect_loop_active = false;
            s.retry_attempt_in_flight = false;
            s.current_host.clone().unwrap_or_default()
        };
        self.shared.callback.on_connection_state_changed(
            ConnectionPublicState::Connected { host }
        );
    }

    fn on_disconnected(&self, reason: Option<String>) {
        if !self.is_current() { return; }
        handle_unexpected_disconnect(&self.shared, reason);
    }

    fn on_screen_update(&self, update: ScreenUpdate) {
        if !self.is_current() { return; }
        self.shared.callback.on_screen_update(update);
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

    fn on_no_viable_path(&self) {
        if !self.is_current() { return; }
        self.shared.callback.on_no_viable_path();
    }

    fn on_forward_state_changed(&self, id: String, state: ForwardState) {
        if !self.is_current() { return; }
        self.shared.callback.on_forward_state_changed(id, state);
    }

    fn on_agent_sign_request(&self, key_fingerprint: String) -> bool {
        if !self.is_current() { return false; }
        self.shared.callback.on_agent_sign_request(key_fingerprint)
    }

    fn on_clipboard_write(&self, payload: ClipboardPayload) {
        if !self.is_current() { return; }
        self.shared.callback.on_clipboard_write(payload);
    }

    fn on_clipboard_pull_request(&self) -> Option<ClipboardPayload> {
        if !self.is_current() { return None; }
        self.shared.callback.on_clipboard_pull_request()
    }

    fn on_request_wifi_fd(&self) -> Option<crate::PlatformFd> {
        if !self.is_current() { return None; }
        self.shared.callback.on_request_wifi_fd()
    }

    fn on_request_cellular_fd(&self) -> Option<crate::PlatformFd> {
        if !self.is_current() { return None; }
        self.shared.callback.on_request_cellular_fd()
    }

    fn on_rebind_state_changed(&self, state: crate::rebind_manager::RebindPublicState) {
        if !self.is_current() { return; }
        self.shared.callback.on_rebind_state_changed(state);
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
    /// 冗長 — 抑制する。この判断はセッション状態(`background_state`)とUOSの生
    /// フォーカスイベントの複製(`tab_focused`)に基づくため`rust-ssot.md`の対象
    /// (Kotlin側にミラー状態を作って分岐させない)。per-tab通知ON/OFF設定自体は
    /// UI設定でありKotlin側(`OrchestratorCallback::on_notify`実装)の責務。
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
                !(s.tab_focused && s.background_state == BackgroundState::Foreground)
            }
        };
        if should_deliver {
            self.shared.callback.on_notify(kind);
        }
    }

    fn on_prompt_jump(&self, target: Option<crate::PromptJumpTarget>) {
        if !self.is_current() { return; }
        self.shared.callback.on_prompt_jump(target);
    }

    fn on_prompt_output_copy_ready(&self, text: Option<String>) {
        if !self.is_current() { return; }
        self.shared.callback.on_prompt_output_copy_ready(text);
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
/// `RUNTIME.spawn`されたtokio task(`Arc<OrchestratorShared>`のcloneしか持たない)から
/// 呼ばれるため — `SessionOrchestrator::disconnect`(セッションを切るだけの2行)と
/// 中身は同じだが、`&self`経由ではなく`shared`に対して直接操作する。
///
/// [[always-connects.md]]の実インシデント(網断debounce発火の経路だけが自動復旧の
/// 対象外になっていた)と同じ見落としを繰り返さないよう、`OrchestratorAdapter::
/// on_disconnected`と同じ`handle_unexpected_disconnect`を経由させる —
/// 個別に「phase=Idle + Disconnected通知」を書かない。
/// `apply_network_lost`が`handle_unexpected_disconnect`へ渡す合成理由文字列。
/// [`DisconnectKind::classify`]がこの定数を直接比較するので、二重に書かないよう
/// 定数化してある。
const NETWORK_LOST_REASON: &str = "network lost";

fn apply_network_lost(shared: &Arc<OrchestratorShared>) {
    if let Some(s) = shared.session.lock().as_ref() {
        s.disconnect();
    }
    handle_unexpected_disconnect(shared, Some(NETWORK_LOST_REASON.to_string()));
}

/// `handle_unexpected_disconnect`が受け取る`reason`文字列のRust内部用分類。
///
/// `SessionCallback::on_disconnected(reason: Option<String>)`には現状「切断理由の
/// 種別」を運ぶ専用フィールドが無く、`reason`文字列に頼っている ── この
/// trait(`SessionCallback`)には本番用の`OrchestratorAdapter`以外にテスト専用の
/// 実装が4箇所あり、シグネチャ変更・UniFFI経由でKotlin側に公開される文字列の
/// 変更はそれら全ての更新を要する大きめの変更になるため見送っている。この型は
/// あくまで`reason`文字列を読んだ*後*にRustのプロセス内だけで使う分類であり、
/// `on_disconnected`のシグネチャにも公開文字列そのものにも影響しない。
/// 分類ロジックを`handle_unexpected_disconnect`一箇所に一元化することで、
/// `starts_with`/文字列比較が呼び出し側に増殖するのを防ぐ(rust-ssot.mdの
/// 「判断ロジックをRust側に一元化する」原則そのもの)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DisconnectKind {
    /// `transport::ssh_handler::run_ssh_channel_loop`の`ChannelMsg::ExitStatus`
    /// (リモートプロセスの正常終了、例: ユーザーがシェルで`exit`した)由来の切断。
    /// ネットワーク/トランスポート障害ではないので、tssh風の自動再接続の対象に
    /// しない(勝手に新しいシェルを張り直すのは意図しない挙動)。
    GracefulRemoteExit,
    /// `apply_network_lost`が合成する、OS側のネットワークパス消失由来の切断。
    /// トランスポート層自体は特に何も報告していない(自動再接続の対象)。
    NetworkLost,
    /// 上記以外 ── russh/QUICエラー・認証失敗・PTY/shellリクエスト失敗・
    /// `reason: None`(ピア/ローカルからの切断)等。自動再接続の対象。
    TransportError,
}

impl DisconnectKind {
    fn classify(reason: &Option<String>) -> Self {
        match reason.as_deref() {
            Some(r) if r.starts_with("remote process exited") => Self::GracefulRemoteExit,
            Some(r) if r == NETWORK_LOST_REASON => Self::NetworkLost,
            _ => Self::TransportError,
        }
    }
}

/// 予期しない切断(`OrchestratorAdapter::on_disconnected`・`apply_network_lost`の
/// 両方から呼ばれる)の共通処理。一度`Connected`になっていて・ユーザーが明示的に
/// 切断したのでなく・リモートプロセスの正常終了でもなく・直前の接続設定が分かって
/// いれば自動再接続ループを起動する。既にループが動作中の切断(＝1回のリトライ
/// 試行自体の失敗)は、二重にループを起動せず・連続で`Disconnected`を通知もせず、
/// ループ自身のtickに任せる。
fn handle_unexpected_disconnect(shared: &Arc<OrchestratorShared>, reason: Option<String>) {
    enum Action {
        Suppress,
        StartLoop(LastConnectAttempt, u64),
        NotifyDisconnected(Option<ConnectionIssueHint>),
    }

    let action = {
        let mut s = shared.state.lock();
        let was_connected = s.phase == ConnPhase::Connected;
        let user_initiated = s.user_initiated_disconnect;
        let graceful_exit = DisconnectKind::classify(&reason) == DisconnectKind::GracefulRemoteExit;
        s.user_initiated_disconnect = false;
        s.phase = ConnPhase::Idle;
        s.retry_attempt_in_flight = false;

        if s.reconnect_loop_active {
            Action::Suppress
        } else if was_connected && !user_initiated && !graceful_exit {
            match s.last_connect_attempt.clone() {
                Some(attempt) => {
                    s.reconnect_loop_active = true;
                    s.reconnect_epoch += 1;
                    Action::StartLoop(attempt, s.reconnect_epoch)
                }
                None => {
                    // #20: 自動ループが始まらない=以降フォアグラウンド復帰時の
                    // 自動再接続もこの切断イベントの責務ではなくなる。
                    s.background_state = BackgroundState::Foreground;
                    Action::NotifyDisconnected(None)
                }
            }
        } else {
            // #19: 一度もConnectedに至らず切断された(=接続試行そのものの失敗)場合
            // だけLocal Network Privacyヒントの対象にする。Connected後の正常終了/
            // ユーザー切断ではヒントを付けても意味がない。
            let issue_hint = if !was_connected {
                classify_disconnect_issue_hint(s.last_connect_attempt.as_ref())
            } else {
                None
            };
            // #20: 自動ループが始まらない切断は、バックグラウンド遷移の追跡対象外に戻す
            // (ユーザー切断・正常終了・そもそも接続失敗だった場合を含む)。
            s.background_state = BackgroundState::Foreground;
            Action::NotifyDisconnected(issue_hint)
        }
    };

    match action {
        Action::Suppress => {}
        Action::StartLoop(attempt, epoch) => {
            spawn_reconnect_loop(shared.clone(), attempt, reason, epoch);
        }
        Action::NotifyDisconnected(issue_hint) => {
            shared.callback.on_connection_state_changed(
                ConnectionPublicState::Disconnected { reason, issue_hint }
            );
        }
    }
}

/// リトライ専用のセッション生成。`begin_connect()`(手動接続の開始、`Connecting`通知・
/// `reconnect_epoch`無効化を伴う)とは別関数にしてある — リトライのたびに`begin_connect()`
/// を呼ぶと、リトライループ自身の`reconnect_epoch`を無効化してしまい自己終了してしまう。
fn connect_via(shared: &Arc<OrchestratorShared>, attempt: LastConnectAttempt) -> Result<(), SshError> {
    let (host, port, is_quic) = attempt.host_port_is_quic();
    {
        let mut s = shared.state.lock();
        s.current_host = Some(host);
        s.current_port = port;
        s.is_quic = is_quic;
        s.phase = ConnPhase::Connecting;
    }
    let adapter = OrchestratorAdapter::new(shared.clone());
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
            session.connect(Box::new(adapter))?;
            ActiveSession::IsekaiPipeQuic(session)
        }
        LastConnectAttempt::IsekaiPipeQuicAuto(config) => {
            let session = crate::isekai_pipe_quic_transport::create_isekai_pipe_quic_session(config);
            session.connect_auto(Box::new(adapter))?;
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

// ── タスク#58: フル再接続直後のtmux scrollback backfill ──────

/// `ActiveSession::run_exec`(タスク#61のexecチャンネル)を、`tmux_locator`/
/// `tmux_scrollback`が要求する[`RemoteTmuxCommandRunner`]シームへ薄く適合させる
/// アダプタ。コマンドの組み立て・出力のパースといった純粋なロジックは全て
/// `tmux_scrollback`モジュール側の自由関数にあり、ここでは「execを呼んで結果を
/// `Result<String, TmuxRunError>`へ変換する」ことだけを行う。
struct ActiveSessionTmuxRunner(ActiveSession);

impl RemoteTmuxCommandRunner for ActiveSessionTmuxRunner {
    fn run(&self, cmd: &str) -> impl std::future::Future<Output = Result<String, TmuxRunError>> + Send {
        let session = self.0.clone();
        let cmd = cmd.to_string();
        async move {
            let output = session.run_exec(cmd).await.map_err(|e| TmuxRunError(e.to_string()))?;
            if !crate::tmux_locator::tmux_exit_status_is_success(output.exit_status) {
                return Err(TmuxRunError(format!(
                    "tmux command exited with status {:?}",
                    output.exit_status
                )));
            }
            String::from_utf8(output.stdout)
                .map_err(|e| TmuxRunError(format!("tmux output was not valid UTF-8: {e}")))
        }
    }
}

/// [`OrchestratorShared::after_reconnect_success`]の実運用の既定実装。
/// `shared.tmux_backfill_locator`が設定されていて、かつ現在`ActiveSession`が
/// 存在する(=直前の`connect_via`が本当に成功していた)場合にのみ、
/// `RUNTIME.spawn`でバックグラウンドタスクを起こし、tmuxのscrollback履歴を
/// 取得してこのタブのローカルscrollbackへバッチ注入する。
///
/// 呼び出し自体は同期・即座に返る(実際のexec/capture-paneは`RUNTIME.spawn`
/// されたタスク側で行う)—— `connect_via`のごく直後、まだライブのPTY出力が
/// 届き始めるより十分前のタイミングで呼ばれる想定だが、たとえ多少ライブ出力と
/// 競合してもscrollbackへの注入は加算的(`push_front`)なので致命的な破壊は
/// 起きない。ロケータ未設定・exec失敗・tmux未検出・出力が空、いずれの場合も
/// fail-open(ログを出すだけで接続自体には一切影響しない)。
fn spawn_tmux_scrollback_backfill(shared: &Arc<OrchestratorShared>) {
    let Some(locator) = shared.tmux_backfill_locator.lock().clone() else {
        log::debug!("orchestrator: no tmux locator registered for this pane yet, skipping scrollback backfill");
        return;
    };
    let Some(session) = shared.session.lock().clone() else {
        log::debug!("orchestrator: no active session to backfill scrollback onto, skipping");
        return;
    };
    RUNTIME.spawn(async move {
        let runner = ActiveSessionTmuxRunner(session.clone());
        match fetch_tmux_scrollback_history(&runner, &locator, crate::session::SCROLLBACK_LIMIT).await {
            Ok(lines) if lines.is_empty() => {
                log::debug!("orchestrator: tmux scrollback backfill found no history above the visible screen");
            }
            Ok(lines) => {
                log::info!(
                    "orchestrator: backfilling {} line(s) of tmux scrollback history after full reconnect",
                    lines.len()
                );
                session.inject_scrollback_history(lines);
            }
            Err(e) => {
                log::warn!(
                    "orchestrator: tmux scrollback backfill failed ({e}), continuing with an empty scrollback for this reconnect"
                );
            }
        }
    });
}

/// `spawn_reconnect_loop`の1 tick分の待機。`tick`を素通しで待つのと、
/// `wake`(`OrchestratorShared::reconnect_wake`)がネットワーク復帰通知で
/// 起こされるのをレースさせる — 戻り値は「`wake`側で早期に起きたか」。
/// 早期に起きた場合、呼び出し側は`elapsed`/`tick_count`の通常の会計には
/// 一切触れずに「今すぐ1回試す」ボーナス試行だけ行い、次のループでまた
/// 通常のtick待機に戻る(isekai-pipe側`resume_loop::wait_backoff_or_network_change`
/// と同じ「バックオフ待機とOS通知をレースさせる」発想を、こちらは
/// elapsed/timeoutの会計を一切歪めない形で移植したもの)。
async fn sleep_tick_or_network_restored(tick: Duration, wake: &tokio::sync::Notify) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(tick) => false,
        _ = wake.notified() => true,
    }
}

/// 自動再接続ループ本体。`RUNTIME.spawn`されたtokio task。tsshのUDPモード
/// reconnectと同じく、1秒ごとに`Reconnecting`をライブ通知しつつ、
/// `retry_interval`ごとに実際の再接続(`connect_via`)を試みる。
/// `retry_attempt_in_flight`により、1回の試行の結果(成功/失敗)が判明するまで
/// 次の試行を重ねて発火しない(ホスト鍵確認プロンプトの多重発生を防ぐ)。
///
/// 通常のtickに加え、`shared.reconnect_wake`(`notify_network_path_changed`の
/// `ConnPhase::Idle`分岐がネットワーク復帰時に鳴らす)で早期に起こされた場合は
/// `retry_interval`のcadenceを待たず、その場で1回だけボーナス試行する
/// (`sleep_tick_or_network_restored`参照)。
fn spawn_reconnect_loop(
    shared: Arc<OrchestratorShared>,
    attempt: LastConnectAttempt,
    reason: Option<String>,
    epoch: u64,
) {
    RUNTIME.spawn(async move {
        let policy = shared.state.lock().reconnect_policy;
        let timeout_secs = policy.timeout.as_secs() as u32;
        // tickの整数倍でretry_intervalを表す(「何tickごとに1回試みるか」)。
        // 経過時間を`.as_secs()`で秒に丸めてから割り算すると、テスト用の
        // サブ秒ポリシー(tick=10msなど)で常に0になり判定が壊れるため、
        // tick単位のカウンタで比較する。
        let ticks_per_retry = (policy.retry_interval.as_nanos() / policy.tick.as_nanos().max(1)).max(1);
        let mut elapsed = Duration::ZERO;
        let mut tick_count: u128 = 0;

        if shared.state.lock().reconnect_epoch != epoch {
            // spawnされてから最初のtickに至るまでの間に、既に別の何か(即座の
            // 手動再接続・cancel_reconnect等)に主導権が移っていた場合、初回の
            // Reconnecting通知すら出さずに静かに終了する。
            return;
        }
        shared.callback.on_connection_state_changed(ConnectionPublicState::Reconnecting {
            elapsed_secs: 0,
            timeout_secs,
            reason: reason.clone(),
        });

        loop {
            let woke_early = sleep_tick_or_network_restored(policy.tick, &shared.reconnect_wake).await;

            if shared.state.lock().reconnect_epoch != epoch {
                // 別の何か(新しい手動接続・cancel_reconnect・再接続成功)に
                // 主導権が移った。静かに終了する。
                return;
            }

            if woke_early {
                // ネットワーク復帰通知による早期起床: 通常のelapsed/tick_countの
                // 会計には触れず、`retry_attempt_in_flight`が空いていれば
                // 「今すぐ1回試す」ボーナス試行だけ行って、次のループでまた
                // 通常のtick待機に戻る。
                let should_attempt = {
                    let mut s = shared.state.lock();
                    if s.reconnect_epoch == epoch && !s.retry_attempt_in_flight {
                        s.retry_attempt_in_flight = true;
                        true
                    } else {
                        false
                    }
                };
                if should_attempt {
                    log::info!(
                        "orchestrator: network path restored while reconnecting; retrying immediately instead of waiting out the rest of this tick"
                    );
                    match (shared.reconnect_attempt)(&shared, attempt.clone()) {
                        Ok(()) => (shared.after_reconnect_success)(&shared),
                        Err(e) => {
                            log::warn!("orchestrator: reconnect attempt failed synchronously: {e:?}");
                            let mut s = shared.state.lock();
                            if s.reconnect_epoch == epoch {
                                s.retry_attempt_in_flight = false;
                            }
                        }
                    }
                }
                continue;
            }

            elapsed = elapsed.saturating_add(policy.tick);
            tick_count += 1;

            if elapsed >= policy.timeout {
                let mut s = shared.state.lock();
                if s.reconnect_epoch == epoch {
                    s.reconnect_loop_active = false;
                    s.retry_attempt_in_flight = false;
                }
                drop(s);
                log::warn!("orchestrator: reconnect loop gave up after {timeout_secs}s");
                shared.callback.on_connection_state_changed(ConnectionPublicState::Disconnected {
                    reason: Some(format!(
                        "reconnect timed out after {timeout_secs}s (last: {})",
                        reason.clone().unwrap_or_else(|| "unknown".to_string())
                    )),
                    issue_hint: classify_disconnect_issue_hint(Some(&attempt)),
                });
                return;
            }

            shared.callback.on_connection_state_changed(ConnectionPublicState::Reconnecting {
                elapsed_secs: elapsed.as_secs() as u32,
                timeout_secs,
                reason: reason.clone(),
            });

            let should_attempt = {
                let mut s = shared.state.lock();
                let due = tick_count % ticks_per_retry == 0;
                if s.reconnect_epoch == epoch && !s.retry_attempt_in_flight && due {
                    s.retry_attempt_in_flight = true;
                    true
                } else {
                    false
                }
            };
            if should_attempt {
                match (shared.reconnect_attempt)(&shared, attempt.clone()) {
                    Ok(()) => (shared.after_reconnect_success)(&shared),
                    Err(e) => {
                        log::warn!("orchestrator: reconnect attempt failed synchronously: {e:?}");
                        let mut s = shared.state.lock();
                        if s.reconnect_epoch == epoch {
                            s.retry_attempt_in_flight = false;
                        }
                    }
                }
            }
        }
    });
}

// ── SessionOrchestrator ───────────────────────────────────

#[derive(uniffi::Object)]
pub struct SessionOrchestrator {
    shared: Arc<OrchestratorShared>,
}

#[uniffi::export]
pub fn create_session_orchestrator(callback: Box<dyn OrchestratorCallback>) -> Arc<SessionOrchestrator> {
    crate::init_logger();
    let shared = Arc::new(OrchestratorShared {
        state: Mutex::new(OrchestratorState {
            current_host: None,
            current_port: 22,
            is_quic: false,
            phase: ConnPhase::Idle,
            current_transfer_id: None,
            trzsz_mode: None,
            download_buf: Vec::new(),
            size_limit_exceeded_for: None,
            pending_file_previews: HashMap::new(),
            session_generation: 0,
            reconnect_epoch: 0,
            reconnect_loop_active: false,
            retry_attempt_in_flight: false,
            user_initiated_disconnect: false,
            last_connect_attempt: None,
            reconnect_policy: ReconnectPolicy::default(),
            background_state: BackgroundState::Foreground,
            tab_focused: false,
            recent_notify_seqs: std::collections::VecDeque::new(),
        }),
        callback: Arc::from(callback),
        session: Mutex::new(None),
        path_observer: Mutex::new(crate::net_health_policy::PathObserver::default()),
        app_pane_id: crate::tmux_locator::AppPaneId::generate_process_local(),
        reconnect_attempt: Box::new(connect_via),
        reconnect_wake: tokio::sync::Notify::new(),
        tmux_backfill_locator: Mutex::new(None),
        after_reconnect_success: Box::new(spawn_tmux_scrollback_backfill),
    });
    Arc::new(SessionOrchestrator { shared })
}

impl SessionOrchestrator {
    /// 各`connect_*`が共通で行う「state更新→Connecting通知→adapter生成」を
    /// 一箇所にまとめる。session生成・接続・`ActiveSession`格納は呼び出し側が
    /// トランスポートごとに行う（`connect`のエラー型/セッション型がそれぞれ違うため）。
    ///
    /// phaseが既に`Connecting`(=前の`connect_*`呼び出しがまだ実行中)の間の新規呼び出しは
    /// 拒否する(真の二重start防止、Task #9)。`Connected`中の呼び出しは意図的に許可する
    /// ——「保留中のnetwork-path debounceをキャンセルしつつ別セッションへ手動で切り替える」
    /// 正当な経路であり(下記invalidate呼び出し、および
    /// `notify_network_path_changed_pending_debounce_is_cancelled_by_a_new_connect_attempt`
    /// テスト参照)、`Idle`と同様に受理してよい。
    fn begin_connect(&self, host: String, port: u16, is_quic: bool) -> Result<OrchestratorAdapter, SshError> {
        {
            let mut s = self.shared.state.lock();
            if s.phase == ConnPhase::Connecting {
                return Err(SshError::ConnectionFailed);
            }
            s.current_host = Some(host);
            s.current_port = port;
            s.is_quic = is_quic;
            s.phase = ConnPhase::Connecting;
            // 新しい手動接続が始まった以上、直前のdisconnect()由来のフラグや
            // 実行中だったかもしれない自動再接続ループは無関係になる。
            s.user_initiated_disconnect = false;
            s.reconnect_epoch += 1;
            s.reconnect_loop_active = false;
            s.retry_attempt_in_flight = false;
            // #20: 手動接続はフォアグラウンドの操作でしか起こり得ない。直前の
            // バックグラウンド遷移状態は無関係になる。
            s.background_state = BackgroundState::Foreground;
        }
        // 新しい接続試行が始まった時点で、直前のセッションに対して保留中だった
        // network-path debounceは無効化する。そうしないと、瞬断のdebounce待機中に
        // 手動で切断/別transportへ再接続した場合、無関係な新しいセッションを
        // 誤って切断してしまう(レビューで指摘された実際の不具合)。
        self.shared.path_observer.lock().invalidate();
        self.shared.callback.on_connection_state_changed(ConnectionPublicState::Connecting);
        Ok(OrchestratorAdapter::new(self.shared.clone()))
    }

    /// タスク#58: このオーケストレータが担当するタブ/ペインのtmuxロケータを
    /// 設定/更新する。フル再接続後のscrollback backfill(`spawn_tmux_scrollback_backfill`)
    /// が対象ペインを特定するために読む値そのもの。`TmuxLocator`自体が
    /// UniFFI境界を越えない内部専用の型のため、この関数もUniFFIへは公開しない
    /// ——`ensure_tmux_tab_window`が成功のたびに呼ぶ(#62の`TmuxLocatorRegistry`への
    /// 登録と同じタイミング)。呼ばれなければ`tmux_backfill_locator`は`None`のままで、
    /// backfillはfail-openで単にスキップされる。
    pub(crate) fn set_tmux_backfill_locator(&self, locator: Option<TmuxLocator>) {
        *self.shared.tmux_backfill_locator.lock() = locator;
    }
}

#[uniffi::export]
impl SessionOrchestrator {
    pub fn connect(&self, config: SshConfig) -> Result<(), SshError> {
        let adapter = self.begin_connect(config.host.clone(), config.port, false)?;
        self.shared.state.lock().last_connect_attempt = Some(LastConnectAttempt::Ssh(config.clone()));
        let session = crate::create_ssh_session(config);
        session.connect(Box::new(adapter), self.shared.app_pane_id.clone())?;
        *self.shared.session.lock() = Some(ActiveSession::Ssh(session));
        Ok(())
    }

    pub fn connect_quic(&self, config: QuicConfig) -> Result<(), SshError> {
        let adapter = self.begin_connect(config.ssh_host.clone(), config.ssh_port, true)?;
        self.shared.state.lock().last_connect_attempt = Some(LastConnectAttempt::Quic(config.clone()));
        let session = crate::quic_transport::create_quic_session(config);
        session.connect(Box::new(adapter))?;
        *self.shared.session.lock() = Some(ActiveSession::Quic(session));
        Ok(())
    }

    /// Phase 7: 自作ヘルパー（isekai-helper）経由の QUIC 接続。フォールバック無し
    /// （`TransportPreference::IsekaiPipeQuic` 相当、明示選択時に使う）。
    pub fn connect_isekai_pipe_quic(&self, config: IsekaiPipeQuicConfig) -> Result<(), SshError> {
        let adapter = self.begin_connect(config.ssh_host.clone(), config.ssh_port, true)?;
        self.shared.state.lock().last_connect_attempt = Some(LastConnectAttempt::IsekaiPipeQuic(config.clone()));
        let session = crate::isekai_pipe_quic_transport::create_isekai_pipe_quic_session(config);
        session.connect(Box::new(adapter))?;
        *self.shared.session.lock() = Some(ActiveSession::IsekaiPipeQuic(session));
        Ok(())
    }

    /// Phase 7: `TransportPreference::Auto` 相当。自作ヘルパー経由 QUIC のブートストラップ/
    /// 接続に失敗した場合、内部で自動的に通常の TCP SSH にフォールバックする。
    pub fn connect_isekai_pipe_quic_auto(&self, config: IsekaiPipeQuicConfig) -> Result<(), SshError> {
        let adapter = self.begin_connect(config.ssh_host.clone(), config.ssh_port, true)?;
        self.shared.state.lock().last_connect_attempt = Some(LastConnectAttempt::IsekaiPipeQuicAuto(config.clone()));
        let session = crate::isekai_pipe_quic_transport::create_isekai_pipe_quic_session(config);
        session.connect_auto(Box::new(adapter))?;
        *self.shared.session.lock() = Some(ActiveSession::IsekaiPipeQuic(session));
        Ok(())
    }

    /// Phase 9: `TransportPreference::IsekaiPipeQuicMultipath` 相当。フォールバック無し。
    /// `config.direct_host` が設定されていれば path0（`ssh_host`）+ path1（`direct_host`）の
    /// 受動的マルチパスで接続する。
    pub fn connect_multipath_isekai_pipe_quic(&self, config: MultipathIsekaiPipeQuicConfig) -> Result<(), SshError> {
        let adapter = self.begin_connect(config.ssh_host.clone(), config.ssh_port, true)?;
        self.shared.state.lock().last_connect_attempt = Some(LastConnectAttempt::MultipathIsekaiPipeQuic(config.clone()));
        let session = crate::multipath_transport::create_multipath_isekai_pipe_quic_session(config);
        session.connect(Box::new(adapter))?;
        *self.shared.session.lock() = Some(ActiveSession::MultipathIsekaiPipeQuic(session));
        Ok(())
    }

    /// Phase 10: `TransportPreference::IsekaiStunP2pQuic` 相当。relay 無し・
    /// STUN+SSH rendezvousによる直接 P2P QUIC。フォールバック無し（穴あけ不成立時は
    /// 接続失敗として扱う。`isekai_stun_p2p_transport.rs` 参照）。
    pub fn connect_isekai_stun_p2p(&self, config: IsekaiStunP2pConfig) -> Result<(), SshError> {
        let adapter = self.begin_connect(config.ssh_host.clone(), config.ssh_port, true)?;
        self.shared.state.lock().last_connect_attempt = Some(LastConnectAttempt::IsekaiStunP2p(config.clone()));
        let session = crate::isekai_stun_p2p_transport::create_isekai_stun_p2p_session(config);
        session.connect(Box::new(adapter))?;
        *self.shared.session.lock() = Some(ActiveSession::IsekaiStunP2p(session));
        Ok(())
    }

    /// Phase 10: `TransportPreference::IsekaiLinkRelayQuic` 相当。MASQUE relay 経由の
    /// P2P QUIC。フォールバック無し（`isekai_link_relay_transport.rs` 参照）。
    pub fn connect_isekai_link_relay(&self, config: IsekaiLinkRelayConfig) -> Result<(), SshError> {
        let adapter = self.begin_connect(config.ssh_host.clone(), config.ssh_port, true)?;
        self.shared.state.lock().last_connect_attempt = Some(LastConnectAttempt::IsekaiLinkRelay(config.clone()));
        let session = crate::isekai_link_relay_transport::create_isekai_link_relay_session(config);
        session.connect(Box::new(adapter))?;
        *self.shared.session.lock() = Some(ActiveSession::IsekaiLinkRelay(session));
        Ok(())
    }

    pub fn disconnect(&self) {
        // 「これから来る`on_disconnected`はユーザー操作起因」の印を先に立てておく
        // (実際の切断はこの後`s.disconnect()`が非同期にコールバックを発火させる)。
        self.shared.state.lock().user_initiated_disconnect = true;
        if let Some(s) = self.shared.session.lock().as_ref() {
            s.disconnect();
        }
    }

    /// 自動再接続ループを中止する。ループが動作中だった場合のみ`Disconnected`を
    /// 通知する(動いていない時に呼ばれても無音、UIは`isReconnecting`の間だけ
    /// 「中止」操作を出す想定)。
    pub fn cancel_reconnect(&self) {
        let was_active = {
            let mut s = self.shared.state.lock();
            let was_active = s.reconnect_loop_active;
            s.reconnect_epoch += 1;
            s.reconnect_loop_active = false;
            s.retry_attempt_in_flight = false;
            was_active
        };
        if was_active {
            self.shared.callback.on_connection_state_changed(
                ConnectionPublicState::Disconnected {
                    reason: Some("reconnect cancelled by user".to_string()),
                    issue_hint: None,
                }
            );
        }
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
        // #20 codexレビュー指摘: `Connecting`中にバックグラウンド化し、その猶予中に
        // 接続が成立するケース(`on_connected()`は`background_state`に触れない)も
        // 追跡対象に含める。`Idle`(そもそも維持すべきセッションが無い)は対象外のまま。
        if s.phase == ConnPhase::Connected || s.phase == ConnPhase::Connecting {
            s.background_state = BackgroundState::Quiescing;
        }
    }

    /// バックグラウンド猶予が尽きた(`beginBackgroundTask`失効等)ことを通知する。
    /// 猶予追跡中(`Quiescing`)の場合のみ、次のフォアグラウンド復帰時に再接続が
    /// 必要な状態(`Suspended`)へ遷移する。
    pub fn notify_background_budget_expired(&self) {
        let mut s = self.shared.state.lock();
        if s.background_state == BackgroundState::Quiescing {
            s.background_state = BackgroundState::Suspended;
        }
    }

    /// メモリ逼迫警告(iOSの`didReceiveMemoryWarning`相当)。OSにプロセスを終了
    /// される可能性が高まったとみなし、猶予を待たず保守的に`Suspended`扱いにする
    /// (無言で固まった画面をユーザーに見せるより、次回復帰時に再接続する方が安全)。
    pub fn notify_memory_warning(&self) {
        let mut s = self.shared.state.lock();
        if s.background_state == BackgroundState::Quiescing {
            s.background_state = BackgroundState::Suspended;
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
        let reconnect_with = {
            let mut s = self.shared.state.lock();
            let was_suspended = s.background_state == BackgroundState::Suspended;
            s.background_state = BackgroundState::Foreground;
            if was_suspended && !s.reconnect_loop_active && s.phase != ConnPhase::Connecting {
                s.last_connect_attempt.clone()
            } else {
                None
            }
        };
        if let Some(attempt) = reconnect_with {
            // #20 codexレビュー指摘: `reconnect_attempt`(`connect_via`)は`phase`を
            // `Connecting`にしてから同期的に失敗し得る(ホスト鍵確認拒否・設定不備等)。
            // 自動再接続ループ(`spawn_reconnect_loop`)内の失敗は次のtickで暗黙に
            // リトライされるが、こちらは一回限りの呼び出しなので`Err`を握り潰すと
            // `phase`が`Connecting`のまま固まり、UIが「接続中…」から進まなくなる。
            // ループ経由の再試行に頼らず、この場で`Idle`へ戻し失敗を通知する。
            match (self.shared.reconnect_attempt)(&self.shared, attempt) {
                Ok(()) => (self.shared.after_reconnect_success)(&self.shared),
                Err(e) => {
                    log::warn!("orchestrator: foreground resume reconnect failed synchronously: {e:?}");
                    let mut s = self.shared.state.lock();
                    s.phase = ConnPhase::Idle;
                    drop(s);
                    self.shared.callback.on_connection_state_changed(ConnectionPublicState::Disconnected {
                        reason: Some(format!("foreground resume reconnect failed: {e}")),
                        issue_hint: None,
                    });
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

    pub fn is_quic(&self) -> bool {
        self.shared.state.lock().is_quic
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
            (s.phase, s.is_quic)
        };
        match phase {
            ConnPhase::Idle => {
                // 自動再接続ループが動いている間の「ネットワーク復帰」通知は、
                // 固定間隔ポーリング待機を早期に打ち切って今すぐ試すシグナルに
                // 使う(`spawn_reconnect_loop`参照)。ループが動いていない・
                // 単なる喪失通知(`is_satisfied=false`)は何もしない —
                // 元々このphaseでは接続自体が無いので喪失に対して打てる手が無い。
                if is_satisfied && self.shared.state.lock().reconnect_loop_active {
                    self.shared.reconnect_wake.notify_one();
                }
            }
            ConnPhase::Connecting => {
                if !is_satisfied {
                    log::warn!("orchestrator: network lost during handshake — aborting");
                    apply_network_lost(&self.shared);
                }
            }
            ConnPhase::Connected if is_quic => {
                log::info!("orchestrator: network path changed — QUIC session, letting transport handle it");
            }
            ConnPhase::Connected => {
                let (epoch, decision) = self.shared.path_observer.lock().handle_update(is_satisfied);
                match decision {
                    net_health_policy::Decision::Ignore => {}
                    net_health_policy::Decision::NotifyAfterDebounce(dur) => {
                        let shared = self.shared.clone();
                        RUNTIME.spawn(async move {
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

    /// 接続中にローカルポートフォワード(-L)を動的に追加する。
    /// MVP の UI は接続前に `SshConfig.forwards` へまとめて設定するだけなので現状未使用だが、
    /// 将来「接続したまま転送を足す」UI を追加する際の入り口として用意している。
    pub fn add_local_forward(&self, id: String, bind_address: String, bind_port: u16, remote_host: String, remote_port: u16) {
        if let Some(s) = self.shared.session.lock().as_ref() {
            s.add_local_forward(id, bind_address, bind_port, remote_host, remote_port);
        }
    }

    pub fn remove_forward(&self, id: String) {
        if let Some(s) = self.shared.session.lock().as_ref() {
            s.remove_forward(id);
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

    pub fn notify_error(&self, message: String) {
        self.shared.callback.on_connection_state_changed(
            ConnectionPublicState::Error { message }
        );
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
/// (#61)への薄いアダプタ。`ExecOutput`(stdout + 終了ステータス)を
/// `RemoteTmuxCommandRunner`が期待する`Result<String, TmuxRunError>`へ変換する
/// (非ゼロ終了・非UTF-8出力もここでエラーとして畳み込む)。
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
            let output = orchestrator.run_exec(cmd).await.map_err(|e| TmuxRunError(e.to_string()))?;
            if !crate::tmux_locator::tmux_exit_status_is_success(output.exit_status) {
                return Err(TmuxRunError(format!(
                    "tmux command exited with status {:?} (stdout: {:?})",
                    output.exit_status,
                    String::from_utf8_lossy(&output.stdout),
                )));
            }
            String::from_utf8(output.stdout).map_err(|e| TmuxRunError(format!("non-utf8 tmux output: {e}")))
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
        // 実機では常に一切届いていなかった)。そのため`push_ctl_socket_to_tmux`は
        // ロケータが無いまま`ctl_socket_path`だけを対応表(まず登録、後から値を
        // 確定という構成の逆)に書き込んで抜ける。ここで`register()`が単純に
        // `ctl_socket_path=None`で上書きすると、その既に分かっているパスを
        // 永久に握りつぶしてしまっていた。既知のパスがあれば引き継いで登録し、
        // ロケータが分かった今すぐ改めてtmuxへ書き込み直す。
        let pending_ctl_socket_path =
            registry.lock().ctl_socket_path_for(&self.shared.app_pane_id).map(str::to_string);
        registry.lock().register(
            self.shared.app_pane_id.clone(),
            outcome.locator.clone(),
            pending_ctl_socket_path.clone(),
        );
        registry.lock().set_notify_hooks_enabled(&self.shared.app_pane_id, enable_notifications);
        // タスク#58: `spawn_tmux_scrollback_backfill`が読む`tmux_backfill_locator`
        // (`OrchestratorShared`フィールドのdoc参照)も同じタイミングで配線する。
        // ここを呼ばないと`tmux_backfill_locator`が常に`None`のままとなり、
        // フル再接続後のscrollback backfillがfail-openで常にスキップされ続ける
        // (上のTMUX_LOCATOR_REGISTRY登録漏れと同種の、配線し忘れによる無効化)。
        self.set_tmux_backfill_locator(Some(outcome.locator.clone()));
        // 上と同じ理由で、tmux hook通知(タスク#57: bell/activity/silence/pane-died)の
        // `install_notify_hooks`(`ssh_handler.rs`側でも同じくctl-socket forward
        // 確立直後にspawnされ、ロケータ未登録なら黙ってno-opになる)も、ロケータが
        // 分かった今すぐ改めて試す(有効化されていなければ内部で無害にno-opする)。
        if let Some(ctl_socket_path) = pending_ctl_socket_path {
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
        agent_sign_requests: StdMutex<Vec<String>>,
        clipboard_writes: StdMutex<Vec<ClipboardPayload>>,
        clipboard_pull_requests: StdMutex<u32>,
        wifi_fd_requests: StdMutex<u32>,
        cellular_fd_requests: StdMutex<u32>,
        rebind_states: StdMutex<Vec<crate::rebind_manager::RebindPublicState>>,
        prompt_jumps: StdMutex<Vec<Option<crate::PromptJumpTarget>>>,
    }

    impl OrchestratorCallback for RecordingCallback {
        fn on_connection_state_changed(&self, state: ConnectionPublicState) {
            self.connection_states.lock().unwrap().push(state);
        }
        fn on_screen_update(&self, _update: ScreenUpdate) {}
        fn on_host_key(&self, _host: String, _port: u16, _fingerprint: String) -> bool {
            true
        }
        fn on_data(&self, _data: Vec<u8>) {}
        fn on_trzsz_state_changed(&self, state: TrzszPublicState) {
            self.trzsz_states.lock().unwrap().push(state);
        }
        fn on_download_complete(&self, file_name: Option<String>, data: Vec<u8>) {
            self.downloads.lock().unwrap().push((file_name, data));
        }
        fn on_no_viable_path(&self) {}
        fn on_forward_state_changed(&self, _id: String, _state: ForwardState) {}
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
        fn on_prompt_output_copy_ready(&self, _text: Option<String>) {}
        fn on_file_preview_result(&self, _request_id: String, outcome: FilePreviewOutcome) {
            self.file_preview_outcomes.lock().unwrap().push(outcome);
        }
        fn on_notify(&self, kind: crate::NotifyKind) {
            self.notifications.lock().unwrap().push(kind);
        }
    }

    fn shared_with_phase(phase: ConnPhase, is_quic: bool) -> (Arc<OrchestratorShared>, Arc<RecordingCallback>) {
        let callback = Arc::new(RecordingCallback::default());
        let shared = Arc::new(OrchestratorShared {
            state: Mutex::new(OrchestratorState {
                current_host: Some("example.com".to_string()),
                current_port: 22,
                is_quic,
                phase,
                current_transfer_id: None,
                trzsz_mode: None,
                download_buf: Vec::new(),
                size_limit_exceeded_for: None,
                pending_file_previews: HashMap::new(),
                session_generation: 0,
                reconnect_epoch: 0,
                reconnect_loop_active: false,
                retry_attempt_in_flight: false,
                user_initiated_disconnect: false,
                last_connect_attempt: None,
                reconnect_policy: ReconnectPolicy::default(),
                background_state: BackgroundState::Foreground,
                tab_focused: false,
                recent_notify_seqs: std::collections::VecDeque::new(),
            }),
            callback: callback.clone(),
            session: Mutex::new(None),
            path_observer: Mutex::new(net_health_policy::PathObserver::default()),
            app_pane_id: crate::tmux_locator::AppPaneId::generate_process_local(),
            reconnect_attempt: Box::new(connect_via),
            reconnect_wake: tokio::sync::Notify::new(),
            tmux_backfill_locator: Mutex::new(None),
            after_reconnect_success: Box::new(|_shared| {}),
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
        debounce: std::time::Duration,
    ) -> (SessionOrchestrator, Arc<RecordingCallback>) {
        let (shared, callback) = shared_with_phase(ConnPhase::Connected, false);
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
    /// 複製していた)。`reconnect_attempt`/`after_reconnect_success`(テストごとに
    /// 異なるフェイク)はこの関数の範囲外——呼び出し側が`OrchestratorShared`
    /// 構築時に個別に指定する。
    fn reconnect_test_state(policy: ReconnectPolicy) -> OrchestratorState {
        OrchestratorState {
            current_host: Some("example.com".to_string()),
            current_port: 22,
            is_quic: false,
            phase: ConnPhase::Connected,
            current_transfer_id: None,
            trzsz_mode: None,
            download_buf: Vec::new(),
            size_limit_exceeded_for: None,
            pending_file_previews: HashMap::new(),
            session_generation: 0,
            reconnect_epoch: 0,
            reconnect_loop_active: false,
            retry_attempt_in_flight: false,
            user_initiated_disconnect: false,
            last_connect_attempt: Some(LastConnectAttempt::Ssh(test_ssh_config())),
            reconnect_policy: policy,
            background_state: BackgroundState::Foreground,
            tab_focused: false,
            recent_notify_seqs: std::collections::VecDeque::new(),
        }
    }

    fn orchestrator_connected_with_reconnect_policy(
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
            tmux_backfill_locator: Mutex::new(None),
            after_reconnect_success: Box::new(|_shared| {}),
        });
        (SessionOrchestrator { shared }, callback, attempt_count)
    }

    /// タスク#58: `after_reconnect_success`フックが「自動再接続が成功した回数」
    /// と正確に連動して呼ばれること(失敗した試行では呼ばれないこと)を検証する
    /// ためだけの専用ヘルパー。`reconnect_attempt`自体は`should_fail`が指す
    /// 呼び出し回数(1始まり)だけ`Err`を返し、それ以外は`Ok`にする——「毎回
    /// 成功」だけでなく「一部の試行が失敗する」cadenceも再現できるようにする。
    fn orchestrator_connected_with_reconnect_policy_and_backfill_counter(
        policy: ReconnectPolicy,
        fail_on_attempt_numbers: Vec<usize>,
    ) -> (
        SessionOrchestrator,
        Arc<RecordingCallback>,
        Arc<std::sync::atomic::AtomicUsize>,
        Arc<std::sync::atomic::AtomicUsize>,
    ) {
        let callback = Arc::new(RecordingCallback::default());
        let attempt_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let backfill_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = attempt_count.clone();
        let backfill_counter_for_hook = backfill_count.clone();
        let shared = Arc::new(OrchestratorShared {
            state: Mutex::new(reconnect_test_state(policy)),
            callback: callback.clone(),
            session: Mutex::new(None),
            path_observer: Mutex::new(net_health_policy::PathObserver::default()),
            reconnect_attempt: Box::new(move |_shared, _attempt| {
                let n = counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                if fail_on_attempt_numbers.contains(&n) {
                    Err(SshError::ConnectionFailed)
                } else {
                    Ok(())
                }
            }),
            reconnect_wake: tokio::sync::Notify::new(),
            tmux_backfill_locator: Mutex::new(None),
            app_pane_id: crate::tmux_locator::AppPaneId::generate_process_local(),
            after_reconnect_success: Box::new(move |_shared| {
                backfill_counter_for_hook.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }),
        });
        (SessionOrchestrator { shared }, callback, attempt_count, backfill_count)
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
        assert!(orch.shared.state.lock().phase == ConnPhase::Idle);
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
        assert!(orch.shared.state.lock().phase == ConnPhase::Idle);
    }

    #[test]
    fn notify_network_path_changed_ignores_satisfied_updates_during_handshake() {
        // Connecting中は瞬断debounceの対象外 — 既存の即時abort挙動を維持する一方、
        // is_satisfied=trueはそもそも「断ではない」ので何もしないままで良い。
        let (orch, cb) = orchestrator_with_phase(ConnPhase::Connecting, false);
        orch.notify_network_path_changed(true);
        assert!(cb.connection_states.lock().unwrap().is_empty());
        assert!(orch.shared.state.lock().phase == ConnPhase::Connecting);
    }

    #[test]
    fn notify_network_path_changed_ignores_quic_when_connected() {
        let (orch, cb) = orchestrator_with_phase(ConnPhase::Connected, true);
        orch.notify_network_path_changed(false);
        // QUICは経路変更に自前で耐えるため、切断扱いにせずphaseもConnectedのまま維持する。
        assert!(cb.connection_states.lock().unwrap().is_empty());
        assert!(orch.shared.state.lock().phase == ConnPhase::Connected);
    }

    #[test]
    fn notify_network_path_changed_disconnects_plain_tcp_after_debounce_elapses() {
        let (orch, cb) = orchestrator_connected_tcp_with_debounce(std::time::Duration::from_millis(30));
        orch.notify_network_path_changed(false);
        assert!(
            cb.connection_states.lock().unwrap().is_empty(),
            "debounce前は即座に切断されないはず"
        );

        std::thread::sleep(std::time::Duration::from_millis(200));

        let events = cb.connection_states.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], ConnectionPublicState::Disconnected { .. }));
        assert!(orch.shared.state.lock().phase == ConnPhase::Idle);
    }

    #[test]
    fn notify_network_path_changed_does_not_disconnect_plain_tcp_if_recovered_before_debounce_elapses() {
        let (orch, cb) = orchestrator_connected_tcp_with_debounce(std::time::Duration::from_millis(30));
        orch.notify_network_path_changed(false);
        orch.notify_network_path_changed(true); // 瞬断から復旧 — 保留中のdebounceをキャンセルする

        std::thread::sleep(std::time::Duration::from_millis(200));

        assert!(
            cb.connection_states.lock().unwrap().is_empty(),
            "debounce中に復旧したので切断されないはず"
        );
        assert!(orch.shared.state.lock().phase == ConnPhase::Connected);
    }

    #[test]
    fn notify_network_path_changed_pending_debounce_is_cancelled_by_a_new_connect_attempt() {
        // レビューで指摘された不具合の再現: プレーンTCP接続中に瞬断でdebounceが
        // 保留中の間、手動で別のセッションへ再接続しても、古いdebounceの発火で
        // 新しいセッションを誤って切断してはいけない。
        let (orch, cb) = orchestrator_connected_tcp_with_debounce(std::time::Duration::from_millis(30));
        orch.notify_network_path_changed(false);
        orch.begin_connect("other.example.com".to_string(), 22, false)
            .expect("Connected中の新規connectは許可されるはず");

        std::thread::sleep(std::time::Duration::from_millis(200));

        let events = cb.connection_states.lock().unwrap();
        assert!(
            events.iter().all(|e| !matches!(e, ConnectionPublicState::Disconnected { .. })),
            "新しい接続試行後は、古いdebounce発火由来のDisconnectedが飛んではいけない, got: {events:?}"
        );
        assert!(
            orch.shared.state.lock().phase == ConnPhase::Connecting,
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
        let result = orch.begin_connect("other.example.com".to_string(), 22, false);
        assert!(matches!(result, Err(SshError::ConnectionFailed)));
        // 拒否された呼び出しは進行中の接続の host/port を書き換えてはいけない。
        assert_eq!(orch.shared.state.lock().current_host.as_deref(), Some("example.com"));
    }

    #[test]
    fn begin_connect_allows_a_new_call_while_idle() {
        let (orch, _cb) = orchestrator_with_phase(ConnPhase::Idle, false);
        let result = orch.begin_connect("other.example.com".to_string(), 22, false);
        assert!(result.is_ok());
        assert!(orch.shared.state.lock().phase == ConnPhase::Connecting);
    }

    #[test]
    fn begin_connect_allows_replacing_a_connected_session() {
        // Connected中の新規connectは「別セッションへの手動切り替え」として意図的に許可する
        // (notify_network_path_changed_pending_debounce_is_cancelled_by_a_new_connect_attempt
        // が検証する正当な経路)。
        let (orch, _cb) = orchestrator_with_phase(ConnPhase::Connected, false);
        let result = orch.begin_connect("other.example.com".to_string(), 22, false);
        assert!(result.is_ok());
        assert_eq!(orch.shared.state.lock().current_host.as_deref(), Some("other.example.com"));
    }

    // ── OrchestratorAdapter (SessionCallback実装) ────────────

    fn adapter_with_phase(phase: ConnPhase, is_quic: bool) -> (OrchestratorAdapter, Arc<OrchestratorShared>, Arc<RecordingCallback>) {
        let (shared, callback) = shared_with_phase(phase, is_quic);
        (OrchestratorAdapter::new(shared.clone()), shared, callback)
    }

    #[test]
    fn on_connected_sets_phase_connected_and_reports_current_host() {
        let (adapter, shared, cb) = adapter_with_phase(ConnPhase::Connecting, false);
        adapter.on_connected();
        assert!(shared.state.lock().phase == ConnPhase::Connected);
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
        assert!(shared.state.lock().phase == ConnPhase::Idle);
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
        shared.state.lock().last_connect_attempt = Some(LastConnectAttempt::MultipathIsekaiPipeQuic(config));

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
            s.last_connect_attempt = Some(LastConnectAttempt::MultipathIsekaiPipeQuic(config));
            s.user_initiated_disconnect = true;
        }

        adapter.on_disconnected(Some("user disconnected".to_string()));

        let events = cb.connection_states.lock().unwrap();
        assert!(matches!(&events[0], ConnectionPublicState::Disconnected { issue_hint: None, .. }));
    }

    #[test]
    fn on_host_key_reports_current_host_and_port_from_state() {
        let (adapter, _shared, _cb) = adapter_with_phase(ConnPhase::Connecting, false);
        // RecordingCallback::on_host_key always returns true; verifying it forwards
        // without panicking exercises the host/port read out of shared state.
        assert!(adapter.on_host_key("aa:bb:cc".to_string()));
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
        assert!(shared.state.lock().phase == ConnPhase::Connecting, "phaseも書き換わってはいけない");

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
    // 以下はいずれも「is_current()なら`shared.callback`へそのまま委譲、staleなら
    // 何もしない/既定値を返す」という同型のパターン。1テストで両方(委譲される値の
    // 正しさ・staleになった後は委譲が止まること)を確認する。

    #[test]
    fn on_agent_sign_request_forwards_and_is_suppressed_for_stale_generation() {
        let (shared, cb) = shared_with_phase(ConnPhase::Connected, false);
        let current = OrchestratorAdapter::new(shared.clone());
        assert!(current.on_agent_sign_request("aa:bb".to_string()));
        assert_eq!(cb.agent_sign_requests.lock().unwrap().as_slice(), &["aa:bb".to_string()]);

        let _fresh = OrchestratorAdapter::new(shared.clone());
        assert!(!current.on_agent_sign_request("cc:dd".to_string()), "staleなadapterはfalseを返すべき");
        assert_eq!(
            cb.agent_sign_requests.lock().unwrap().as_slice(), &["aa:bb".to_string()],
            "staleなadapterからのforwardは発生しないはず"
        );
    }

    #[test]
    fn on_clipboard_write_forwards_and_is_suppressed_for_stale_generation() {
        let (shared, cb) = shared_with_phase(ConnPhase::Connected, false);
        let current = OrchestratorAdapter::new(shared.clone());
        let payload = ClipboardPayload { mime: crate::ClipboardMimeKind::TextPlain, data: b"hello".to_vec() };
        current.on_clipboard_write(payload.clone());
        assert_eq!(cb.clipboard_writes.lock().unwrap().as_slice(), &[payload]);

        let _fresh = OrchestratorAdapter::new(shared.clone());
        current.on_clipboard_write(ClipboardPayload { mime: crate::ClipboardMimeKind::TextPlain, data: b"stale".to_vec() });
        assert_eq!(
            cb.clipboard_writes.lock().unwrap().len(), 1,
            "staleなadapterからのon_clipboard_writeは転送されないはず"
        );
    }

    #[test]
    fn on_clipboard_pull_request_forwards_and_is_suppressed_for_stale_generation() {
        let (shared, cb) = shared_with_phase(ConnPhase::Connected, false);
        let current = OrchestratorAdapter::new(shared.clone());
        assert_eq!(
            current.on_clipboard_pull_request(),
            Some(ClipboardPayload { mime: crate::ClipboardMimeKind::TextPlain, data: b"clip".to_vec() })
        );
        assert_eq!(*cb.clipboard_pull_requests.lock().unwrap(), 1);

        let _fresh = OrchestratorAdapter::new(shared.clone());
        assert_eq!(current.on_clipboard_pull_request(), None, "staleなadapterはNoneを返すべき");
        assert_eq!(*cb.clipboard_pull_requests.lock().unwrap(), 1, "staleなadapterからは転送されないはず");
    }

    #[test]
    fn on_request_wifi_fd_forwards_and_is_suppressed_for_stale_generation() {
        let (shared, cb) = shared_with_phase(ConnPhase::Connected, false);
        let current = OrchestratorAdapter::new(shared.clone());
        let fd = current.on_request_wifi_fd().expect("current adapter should forward");
        assert_eq!((fd.fd, fd.local_ip.as_str()), (42, "10.0.0.1"));
        assert_eq!(*cb.wifi_fd_requests.lock().unwrap(), 1);

        let _fresh = OrchestratorAdapter::new(shared.clone());
        assert!(current.on_request_wifi_fd().is_none(), "staleなadapterはNoneを返すべき");
        assert_eq!(*cb.wifi_fd_requests.lock().unwrap(), 1, "staleなadapterからは転送されないはず");
    }

    #[test]
    fn on_request_cellular_fd_forwards_and_is_suppressed_for_stale_generation() {
        let (shared, cb) = shared_with_phase(ConnPhase::Connected, false);
        let current = OrchestratorAdapter::new(shared.clone());
        let fd = current.on_request_cellular_fd().expect("current adapter should forward");
        assert_eq!((fd.fd, fd.local_ip.as_str()), (43, "10.0.0.2"));
        assert_eq!(*cb.cellular_fd_requests.lock().unwrap(), 1);

        let _fresh = OrchestratorAdapter::new(shared.clone());
        assert!(current.on_request_cellular_fd().is_none(), "staleなadapterはNoneを返すべき");
        assert_eq!(*cb.cellular_fd_requests.lock().unwrap(), 1, "staleなadapterからは転送されないはず");
    }

    #[test]
    fn on_rebind_state_changed_forwards_and_is_suppressed_for_stale_generation() {
        let (shared, cb) = shared_with_phase(ConnPhase::Connected, false);
        let current = OrchestratorAdapter::new(shared.clone());
        current.on_rebind_state_changed(crate::rebind_manager::RebindPublicState::FailedOverToCellular);
        assert_eq!(
            cb.rebind_states.lock().unwrap().as_slice(),
            &[crate::rebind_manager::RebindPublicState::FailedOverToCellular]
        );

        let _fresh = OrchestratorAdapter::new(shared.clone());
        current.on_rebind_state_changed(crate::rebind_manager::RebindPublicState::OnWifi);
        assert_eq!(
            cb.rebind_states.lock().unwrap().len(), 1,
            "staleなadapterからのon_rebind_state_changedは転送されないはず"
        );
    }

    #[test]
    fn on_prompt_jump_forwards_and_is_suppressed_for_stale_generation() {
        let (shared, cb) = shared_with_phase(ConnPhase::Connected, false);
        let current = OrchestratorAdapter::new(shared.clone());
        let target = Some(crate::PromptJumpTarget { scroll_offset: 5, is_live: false });
        current.on_prompt_jump(target);
        assert_eq!(cb.prompt_jumps.lock().unwrap().as_slice(), &[target]);

        let _fresh = OrchestratorAdapter::new(shared.clone());
        current.on_prompt_jump(None);
        assert_eq!(
            cb.prompt_jumps.lock().unwrap().len(), 1,
            "staleなadapterからのon_prompt_jumpは転送されないはず"
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

    #[test]
    fn unexpected_disconnect_after_connected_starts_reconnect_loop_and_attempts_retry() {
        let (orch, cb, attempt_count) = orchestrator_connected_with_reconnect_policy(fast_test_policy());
        let adapter = OrchestratorAdapter::new(orch.shared.clone());

        adapter.on_disconnected(Some("peer closed".to_string()));
        assert!(orch.shared.state.lock().reconnect_loop_active, "ループが起動しているはず");

        std::thread::sleep(Duration::from_millis(80));

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

    // ── タスク#58: フル再接続成功後のtmux scrollback backfillフック ──────

    #[test]
    fn after_reconnect_success_fires_once_per_successful_automatic_reconnect_attempt() {
        // resumeが尽きて自動再接続ループ(`spawn_reconnect_loop`)が実際に
        // `connect_via`相当のフェイクを成功させるたびに、`after_reconnect_success`
        // フックが呼ばれることを確認する——これがオーケストレータ側の
        // 「resume失敗/フル再接続」シグナルそのもの(このフェイクは常に成功する
        // ので、成功回数と`attempt_count`は常に一致するはず)。
        let (orch, _cb, attempt_count, backfill_count) =
            orchestrator_connected_with_reconnect_policy_and_backfill_counter(fast_test_policy(), Vec::new());
        let adapter = OrchestratorAdapter::new(orch.shared.clone());

        adapter.on_disconnected(Some("peer closed".to_string()));
        std::thread::sleep(Duration::from_millis(80));

        let attempts = attempt_count.load(std::sync::atomic::Ordering::SeqCst);
        assert!(attempts >= 1, "少なくとも1回は再接続試行が起きるはず");
        // opusレビューM3: attempt_countとbackfill_countは別々のタイミングでloadする
        // 2つの独立したアトミックのため、この2回のload()の間にループがもう1回
        // 試行を開始してattempt_countだけ先に進んでいる(その試行のbackfillはまだ
        // 記録されていない)ことがある(高負荷環境で特に踏みやすい)。「backfillは
        // 成功した試行の数」という不変条件は、現在進行中の1試行分の遅延を許容した
        // 範囲(attempts-1 <= backfill <= attempts)としてなら常に成り立つ。
        let backfill = backfill_count.load(std::sync::atomic::Ordering::SeqCst);
        assert!(
            backfill == attempts || backfill == attempts - 1,
            "backfillは成功した試行の回数と一致するか、直近1試行分だけ遅れているはず, \
             attempts={attempts} backfill={backfill}"
        );
    }

    #[test]
    fn after_reconnect_success_does_not_fire_when_the_reconnect_attempt_fails() {
        // 再接続の試行自体が(同期的に)失敗した場合は、まだ「フル再接続に成功
        // した」わけではないので、backfillフックを呼んではいけない
        // ("NOT on every reconnect" —— 成功した試行にだけ反応する)。
        let (orch, _cb, attempt_count, backfill_count) = orchestrator_connected_with_reconnect_policy_and_backfill_counter(
            fast_test_policy(),
            vec![1, 2, 3, 4, 5, 6, 7, 8], // 観測しうる試行回数を広く先回りして失敗させる
        );
        let adapter = OrchestratorAdapter::new(orch.shared.clone());

        adapter.on_disconnected(Some("peer closed".to_string()));
        std::thread::sleep(Duration::from_millis(80));

        assert!(
            attempt_count.load(std::sync::atomic::Ordering::SeqCst) >= 1,
            "再接続の試行自体は起きるはず(失敗するだけ)"
        );
        assert_eq!(
            backfill_count.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "1回も成功していないのでbackfillフックは1度も呼ばれないはず"
        );
    }

    #[test]
    fn after_reconnect_success_does_not_fire_when_no_disconnect_ever_happens() {
        // resumeがtransport層で透過的に成功した場合、`on_disconnected`自体が
        // 一度も発火しないため自動再接続ループも`connect_via`も一切呼ばれない
        // ——「resume成功パスを一切妨げない」ことは、この経路がそもそも
        // `reconnect_attempt`/`after_reconnect_success`に触れないという構造上の
        // 性質としてすでに保証されている(この回帰を検出するための明示テスト)。
        let (orch, _cb, attempt_count, backfill_count) =
            orchestrator_connected_with_reconnect_policy_and_backfill_counter(fast_test_policy(), Vec::new());
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(attempt_count.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(backfill_count.load(std::sync::atomic::Ordering::SeqCst), 0);
        let _ = orch;
    }

    #[test]
    fn handle_unexpected_disconnect_suppresses_when_a_reconnect_loop_is_already_active() {
        // 自動再接続ループの1リトライ試行自体が失敗して起きる切断(=既に
        // reconnect_loop_activeがtrue)は、二重にループを起動せず・Disconnectedも
        // 通知せず、ループ自身のtickに任せるはず(`Action::Suppress`)。
        let (orch, cb, attempt_count) = orchestrator_connected_with_reconnect_policy(fast_test_policy());
        orch.shared.state.lock().reconnect_loop_active = true;
        let adapter = OrchestratorAdapter::new(orch.shared.clone());

        adapter.on_disconnected(Some("retry attempt itself failed".to_string()));

        assert!(
            cb.connection_states.lock().unwrap().is_empty(),
            "Suppress時はDisconnectedを通知してはいけない(ループ自身のtickに任せる)"
        );
        std::thread::sleep(Duration::from_millis(60));
        assert_eq!(
            attempt_count.load(std::sync::atomic::Ordering::SeqCst), 0,
            "handle_unexpected_disconnect自身は新しいループを二重起動してはいけない"
        );
        assert!(orch.shared.state.lock().phase == ConnPhase::Idle, "phase自体は他の分岐と同様Idleへ戻るはず");
    }

    #[test]
    fn user_initiated_disconnect_does_not_start_reconnect_loop() {
        let (orch, cb, attempt_count) = orchestrator_connected_with_reconnect_policy(fast_test_policy());
        orch.disconnect(); // session が None なので実際の切断処理は起きないが、フラグは立つ
        let adapter = OrchestratorAdapter::new(orch.shared.clone());

        adapter.on_disconnected(Some("peer closed".to_string()));
        assert!(!orch.shared.state.lock().reconnect_loop_active);

        std::thread::sleep(Duration::from_millis(60));
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
        assert!(!shared.state.lock().reconnect_loop_active);
        let events = cb.connection_states.lock().unwrap();
        assert!(matches!(&events[0], ConnectionPublicState::Disconnected { .. }));
    }

    #[test]
    fn graceful_remote_exit_does_not_start_reconnect_loop() {
        // リモートシェルの正常終了(`run_ssh_channel_loop`の`ChannelMsg::ExitStatus`)は
        // ネットワーク障害ではないので自動再接続してはいけない
        // (実際にこの区別が無かったことで`transport::pooling_e2e_tests::
        // one_tab_remote_exit_does_not_disconnect_sibling_tabs`が壊れた)。
        let (orch, cb, attempt_count) = orchestrator_connected_with_reconnect_policy(fast_test_policy());
        let adapter = OrchestratorAdapter::new(orch.shared.clone());
        adapter.on_disconnected(Some("remote process exited (status 0)".to_string()));

        assert!(!orch.shared.state.lock().reconnect_loop_active);
        std::thread::sleep(Duration::from_millis(60));
        assert_eq!(attempt_count.load(std::sync::atomic::Ordering::SeqCst), 0);
        let events = cb.connection_states.lock().unwrap();
        assert!(matches!(&events[0], ConnectionPublicState::Disconnected { .. }));
    }

    #[test]
    fn reconnect_loop_gives_up_after_timeout_and_notifies_disconnected() {
        let policy = ReconnectPolicy {
            tick: Duration::from_millis(10),
            // retry_intervalをtimeoutより長くして、試行を一切発火させずに
            // タイムアウトだけを検証する(実接続の副作用を避ける)。
            retry_interval: Duration::from_secs(60),
            timeout: Duration::from_millis(40),
        };
        let (orch, cb, attempt_count) = orchestrator_connected_with_reconnect_policy(policy);
        let adapter = OrchestratorAdapter::new(orch.shared.clone());
        adapter.on_disconnected(Some("peer closed".to_string()));

        std::thread::sleep(Duration::from_millis(200));

        assert_eq!(attempt_count.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert!(!orch.shared.state.lock().reconnect_loop_active, "タイムアウト後はループが終了しているはず");
        let events = cb.connection_states.lock().unwrap();
        assert!(matches!(
            events.last(),
            Some(ConnectionPublicState::Disconnected { reason: Some(r), .. }) if r.contains("timed out")
        ), "ギブアップ後は理由付きでDisconnectedが通知されるはず, got: {events:?}");
    }

    #[test]
    fn network_path_restored_while_idle_triggers_an_immediate_retry_bypassing_the_tick_cadence() {
        // retry_intervalをこのテストのsleep幅よりずっと長くしておくことで、
        // 通常のtick cadenceだけでは絶対に試行が発火しない状況を作る —
        // それでも試行が観測されれば、`notify_network_path_changed(true)`の
        // 早期ウェイクが実際に効いている証拠になる。
        let policy = ReconnectPolicy {
            tick: Duration::from_millis(10),
            retry_interval: Duration::from_secs(60),
            timeout: Duration::from_secs(60),
        };
        let (orch, _cb, attempt_count) = orchestrator_connected_with_reconnect_policy(policy);
        let adapter = OrchestratorAdapter::new(orch.shared.clone());
        adapter.on_disconnected(Some("peer closed".to_string()));
        assert!(orch.shared.state.lock().reconnect_loop_active);

        // ループが最初のtick待機に入るのを少し待ってから、ネットワーク復帰を通知する。
        std::thread::sleep(Duration::from_millis(30));
        assert_eq!(
            attempt_count.load(std::sync::atomic::Ordering::SeqCst), 0,
            "retry_intervalが60秒なので、通知前はまだ試行が発火していないはず"
        );

        orch.notify_network_path_changed(true);
        std::thread::sleep(Duration::from_millis(50));

        assert!(
            attempt_count.load(std::sync::atomic::Ordering::SeqCst) >= 1,
            "ネットワーク復帰通知でtick cadenceを待たずに即座に再試行するはず"
        );
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

    #[test]
    fn cancel_reconnect_stops_loop_and_notifies_disconnected() {
        let policy = ReconnectPolicy {
            tick: Duration::from_millis(10),
            retry_interval: Duration::from_secs(60),
            timeout: Duration::from_secs(60),
        };
        let (orch, cb, _attempt_count) = orchestrator_connected_with_reconnect_policy(policy);
        let adapter = OrchestratorAdapter::new(orch.shared.clone());
        adapter.on_disconnected(Some("peer closed".to_string()));
        assert!(orch.shared.state.lock().reconnect_loop_active);

        orch.cancel_reconnect();

        assert!(!orch.shared.state.lock().reconnect_loop_active);
        let events = cb.connection_states.lock().unwrap();
        assert!(matches!(
            events.last(),
            Some(ConnectionPublicState::Disconnected { reason: Some(r), .. }) if r.contains("cancelled")
        ));

        // ループ自体もepoch不一致で自然終了するはず(次tickでretryが発火しない)。
        drop(events);
        std::thread::sleep(Duration::from_millis(60));
        // cancel_reconnect後に新規の接続試行は発火しない。
    }

    #[test]
    fn a_new_manual_connect_invalidates_a_pending_reconnect_loop() {
        // レビューで指摘された既存の`notify_network_path_changed`パターンと同型:
        // 再接続ループが動いている最中に手動で新しい接続を始めたら、古いループの
        // 通知/試行が新しいセッションを誤って巻き戻してはいけない。
        let policy = ReconnectPolicy {
            tick: Duration::from_millis(10),
            retry_interval: Duration::from_millis(20),
            timeout: Duration::from_secs(60),
        };
        let (orch, cb, attempt_count) = orchestrator_connected_with_reconnect_policy(policy);
        let adapter = OrchestratorAdapter::new(orch.shared.clone());
        adapter.on_disconnected(Some("peer closed".to_string()));
        assert!(orch.shared.state.lock().reconnect_loop_active);

        // 手動で新しい接続を開始(begin_connect相当)。
        let _new_adapter = orch.begin_connect("other.example.com".to_string(), 22, false)
            .expect("Idle中の新規connectは許可されるはず");
        assert!(!orch.shared.state.lock().reconnect_loop_active, "新しい手動接続でループは無効化されるはず");

        std::thread::sleep(Duration::from_millis(80));
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

    #[test]
    fn apply_network_lost_on_connected_tcp_session_also_starts_reconnect_loop() {
        // always-connects.mdの実インシデント(網断debounce経路だけが自動復旧の
        // 対象外だった)の再発防止: apply_network_lost経由でも同じ
        // handle_unexpected_disconnectを通ることを確認する。
        let (orch, cb, attempt_count) = orchestrator_connected_with_reconnect_policy(fast_test_policy());
        apply_network_lost(&orch.shared);
        assert!(orch.shared.state.lock().reconnect_loop_active);

        std::thread::sleep(Duration::from_millis(80));
        assert!(attempt_count.load(std::sync::atomic::Ordering::SeqCst) >= 1);
        let events = cb.connection_states.lock().unwrap();
        assert!(events.iter().any(|e| matches!(e, ConnectionPublicState::Reconnecting { .. })));
    }

    #[test]
    fn reconnect_success_stops_the_loop() {
        let policy = ReconnectPolicy {
            tick: Duration::from_millis(10),
            retry_interval: Duration::from_millis(500), // このテストでは試行が発火する前に成功させる
            timeout: Duration::from_secs(60),
        };
        let (orch, cb, _attempt_count) = orchestrator_connected_with_reconnect_policy(policy);
        let adapter = OrchestratorAdapter::new(orch.shared.clone());
        adapter.on_disconnected(Some("peer closed".to_string()));
        assert!(orch.shared.state.lock().reconnect_loop_active);

        // 別経路で再接続が成功した(例: 手動再接続やconnect_via経由の新しいセッション)ことを模す。
        let success_adapter = OrchestratorAdapter::new(orch.shared.clone());
        success_adapter.on_connected();
        // ループ自身の初回通知(spawn直後の非同期タスク)とこの成功呼び出しは別スレッドで
        // 走るため、"Connected"より前に1回だけ"Reconnecting"が紛れ込む可能性はあるが、
        // それは無害(UIは直後にConnectedへ収束する)。ここで決定的に検証できる/すべき
        // 性質は「ループ自身が停止すること」と「成功後に(タイムアウト由来の)Disconnectedが
        // 絶対に飛ばないこと」の2つ。
        assert!(!orch.shared.state.lock().reconnect_loop_active);

        std::thread::sleep(Duration::from_millis(60));
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

    // ── #20: バックグラウンド/フォアグラウンド遷移 ─────────────

    #[test]
    fn notify_did_enter_background_quiesces_only_when_connected() {
        let (orch, _cb) = orchestrator_with_phase(ConnPhase::Connected, false);
        orch.notify_did_enter_background(30_000);
        assert_eq!(orch.shared.state.lock().background_state, BackgroundState::Quiescing);
    }

    #[test]
    fn notify_did_enter_background_is_noop_when_idle() {
        // Idle(そもそも維持すべきセッションが無い)はバックグラウンド化しても対象外。
        // `Connecting`は対象に含める(`notify_did_enter_background_while_connecting_...`参照)。
        let (orch, _cb) = orchestrator_with_phase(ConnPhase::Idle, false);
        orch.notify_did_enter_background(30_000);
        assert_eq!(orch.shared.state.lock().background_state, BackgroundState::Foreground);
    }

    #[test]
    fn notify_background_budget_expired_transitions_quiescing_to_suspended() {
        let (orch, _cb) = orchestrator_with_phase(ConnPhase::Connected, false);
        orch.notify_did_enter_background(30_000);
        orch.notify_background_budget_expired();
        assert_eq!(orch.shared.state.lock().background_state, BackgroundState::Suspended);
    }

    #[test]
    fn notify_background_budget_expired_is_noop_when_still_foreground() {
        let (orch, _cb) = orchestrator_with_phase(ConnPhase::Connected, false);
        orch.notify_background_budget_expired();
        assert_eq!(orch.shared.state.lock().background_state, BackgroundState::Foreground);
    }

    #[test]
    fn notify_memory_warning_forces_suspended_while_quiescing() {
        let (orch, _cb) = orchestrator_with_phase(ConnPhase::Connected, false);
        orch.notify_did_enter_background(30_000);
        orch.notify_memory_warning();
        assert_eq!(orch.shared.state.lock().background_state, BackgroundState::Suspended);
    }

    #[test]
    fn notify_memory_warning_is_noop_while_foreground() {
        let (orch, _cb) = orchestrator_with_phase(ConnPhase::Connected, false);
        orch.notify_memory_warning();
        assert_eq!(orch.shared.state.lock().background_state, BackgroundState::Foreground);
    }

    #[test]
    fn notify_will_enter_foreground_within_budget_resumes_without_reconnecting() {
        let (orch, _cb, attempt_count) = orchestrator_connected_with_reconnect_policy(fast_test_policy());
        orch.notify_did_enter_background(30_000);
        assert_eq!(orch.shared.state.lock().background_state, BackgroundState::Quiescing);

        orch.notify_will_enter_foreground();
        assert_eq!(orch.shared.state.lock().background_state, BackgroundState::Foreground);
        assert_eq!(
            attempt_count.load(std::sync::atomic::Ordering::SeqCst), 0,
            "猶予内復帰(Quiescing)では再接続を試みてはいけない"
        );
    }

    #[test]
    fn notify_will_enter_foreground_after_budget_expired_triggers_reconnect() {
        let (orch, _cb, attempt_count) = orchestrator_connected_with_reconnect_policy(fast_test_policy());
        orch.notify_did_enter_background(30_000);
        orch.notify_background_budget_expired();
        assert_eq!(orch.shared.state.lock().background_state, BackgroundState::Suspended);

        orch.notify_will_enter_foreground();
        assert_eq!(orch.shared.state.lock().background_state, BackgroundState::Foreground);
        assert_eq!(
            attempt_count.load(std::sync::atomic::Ordering::SeqCst), 1,
            "猶予切れ(Suspended)からの復帰では直前の接続設定で再接続を試みるはず"
        );
    }

    #[test]
    fn notify_will_enter_foreground_after_budget_expired_also_fires_the_backfill_hook_on_success() {
        // タスク#58: バックグラウンド猶予切れからのフォアグラウンド復帰再接続
        // (`notify_will_enter_foreground`が直接`reconnect_attempt`を呼ぶ経路)も
        // `spawn_reconnect_loop`と同じく「フル再接続に成功した」経路なので、
        // 同じ`after_reconnect_success`フックを経由するはず。
        let (orch, _cb, attempt_count, backfill_count) =
            orchestrator_connected_with_reconnect_policy_and_backfill_counter(fast_test_policy(), Vec::new());
        orch.notify_did_enter_background(30_000);
        orch.notify_background_budget_expired();

        orch.notify_will_enter_foreground();

        assert_eq!(attempt_count.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(
            backfill_count.load(std::sync::atomic::Ordering::SeqCst), 1,
            "フォアグラウンド復帰再接続の成功でもbackfillフックが呼ばれるはず"
        );
    }

    #[test]
    fn notify_will_enter_foreground_does_not_double_trigger_when_reconnect_loop_already_active() {
        let (orch, _cb, attempt_count) = orchestrator_connected_with_reconnect_policy(fast_test_policy());
        orch.notify_did_enter_background(30_000);
        orch.notify_background_budget_expired();

        // 既に(別経路の)自動再接続ループが動作中だとして立てておく。
        orch.shared.state.lock().reconnect_loop_active = true;

        orch.notify_will_enter_foreground();
        assert_eq!(
            attempt_count.load(std::sync::atomic::Ordering::SeqCst), 0,
            "既に自動再接続ループが動作中なら二重に接続を試みてはいけない"
        );
    }

    #[test]
    fn notify_will_enter_foreground_does_not_trigger_while_a_connect_is_already_in_flight() {
        let (orch, _cb, attempt_count) = orchestrator_connected_with_reconnect_policy(fast_test_policy());
        orch.notify_did_enter_background(30_000);
        orch.notify_background_budget_expired();

        orch.shared.state.lock().phase = ConnPhase::Connecting;

        orch.notify_will_enter_foreground();
        assert_eq!(
            attempt_count.load(std::sync::atomic::Ordering::SeqCst), 0,
            "既に接続試行中なら二重に接続を試みてはいけない"
        );
    }

    #[test]
    fn notify_will_enter_foreground_is_noop_without_prior_backgrounding() {
        let (orch, _cb, attempt_count) = orchestrator_connected_with_reconnect_policy(fast_test_policy());
        orch.notify_will_enter_foreground();
        assert_eq!(attempt_count.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(orch.shared.state.lock().background_state, BackgroundState::Foreground);
    }

    #[test]
    fn begin_connect_resets_background_state_to_foreground() {
        let (orch, _cb) = orchestrator_with_phase(ConnPhase::Idle, false);
        orch.shared.state.lock().background_state = BackgroundState::Suspended;
        let _ = orch.begin_connect("example.com".to_string(), 22, false);
        assert_eq!(orch.shared.state.lock().background_state, BackgroundState::Foreground);
    }

    #[test]
    fn handle_unexpected_disconnect_without_auto_reconnect_resets_background_state() {
        // 自動再接続ループが始まらない切断(ここではuser_initiated)は、以降の
        // notify_will_enter_foreground()が誤って再接続を試みないようbackground_stateを
        // Foregroundへ戻す。
        let (adapter, shared, _cb) = adapter_with_phase(ConnPhase::Connected, false);
        {
            let mut s = shared.state.lock();
            s.background_state = BackgroundState::Suspended;
            s.user_initiated_disconnect = true;
        }
        adapter.on_disconnected(Some("user disconnected".to_string()));
        assert_eq!(shared.state.lock().background_state, BackgroundState::Foreground);
    }

    #[test]
    fn notify_did_enter_background_while_connecting_survives_into_quiescing_after_connected() {
        // codexレビュー指摘の再現: Connecting中にバックグラウンド化し、その猶予中に
        // 接続が成立したケース。on_connected()自体はbackground_stateに触れないため、
        // notify_did_enter_background()の時点でConnectingも対象に含めておく必要がある。
        let (orch, cb) = orchestrator_with_phase(ConnPhase::Connecting, false);
        orch.shared.state.lock().last_connect_attempt = Some(LastConnectAttempt::Ssh(test_ssh_config()));

        orch.notify_did_enter_background(30_000);
        assert_eq!(orch.shared.state.lock().background_state, BackgroundState::Quiescing);

        let adapter = OrchestratorAdapter::new(orch.shared.clone());
        adapter.on_connected();
        assert_eq!(
            orch.shared.state.lock().background_state, BackgroundState::Quiescing,
            "on_connected()はbackground_stateに触れないので猶予追跡は続いているはず"
        );

        orch.notify_background_budget_expired();
        assert_eq!(orch.shared.state.lock().background_state, BackgroundState::Suspended);
        let _ = cb;
    }

    fn orchestrator_connected_with_failing_reconnect(
    ) -> (SessionOrchestrator, Arc<RecordingCallback>, Arc<std::sync::atomic::AtomicUsize>) {
        let callback = Arc::new(RecordingCallback::default());
        let attempt_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = attempt_count.clone();
        let shared = Arc::new(OrchestratorShared {
            state: Mutex::new(OrchestratorState {
                current_host: Some("example.com".to_string()),
                current_port: 22,
                is_quic: false,
                phase: ConnPhase::Connected,
                current_transfer_id: None,
                trzsz_mode: None,
                download_buf: Vec::new(),
                size_limit_exceeded_for: None,
                pending_file_previews: HashMap::new(),
                session_generation: 0,
                reconnect_epoch: 0,
                reconnect_loop_active: false,
                retry_attempt_in_flight: false,
                user_initiated_disconnect: false,
                last_connect_attempt: Some(LastConnectAttempt::Ssh(test_ssh_config())),
                reconnect_policy: ReconnectPolicy::default(),
                background_state: BackgroundState::Foreground,
                tab_focused: false,
                recent_notify_seqs: std::collections::VecDeque::new(),
            }),
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
                shared.state.lock().phase = ConnPhase::Connecting;
                Err(SshError::ConnectionFailed)
            }),
            reconnect_wake: tokio::sync::Notify::new(),
            tmux_backfill_locator: Mutex::new(None),
            after_reconnect_success: Box::new(|_shared| {}),
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
        assert_eq!(orch.shared.state.lock().background_state, BackgroundState::Suspended);

        orch.notify_will_enter_foreground();

        assert_eq!(attempt_count.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(
            orch.shared.state.lock().phase == ConnPhase::Idle,
            "同期失敗後にphaseがConnectingのまま固まってはいけない"
        );
        let events = cb.connection_states.lock().unwrap();
        assert!(
            events.iter().any(|e| matches!(e, ConnectionPublicState::Disconnected { .. })),
            "同期失敗はDisconnectedとして通知されるはず, got: {events:?}"
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
            s.background_state = BackgroundState::Foreground;
        }
        adapter.on_notify(crate::NotifyKind::Activity, "tag-a".to_string(), 1);
        assert!(cb.notifications.lock().unwrap().is_empty());
    }

    #[test]
    fn on_notify_delivers_when_tab_focused_but_app_backgrounded() {
        // タブ自体はフォーカスされていても(Compose側の直近状態が古い等)、
        // アプリ全体がバックグラウンドならユーザーは見ていないので配信する。
        let (adapter, shared, cb) = adapter_with_phase(ConnPhase::Connected, false);
        {
            let mut s = shared.state.lock();
            s.tab_focused = true;
            s.background_state = BackgroundState::Suspended;
        }
        adapter.on_notify(crate::NotifyKind::Silence, "tag-a".to_string(), 1);
        assert_eq!(cb.notifications.lock().unwrap().as_slice(), &[crate::NotifyKind::Silence]);
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
        use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
        use crate::SshAuth;

        #[allow(dead_code)]
        enum TestEvent {
            Connection(ConnectionPublicState),
            Data(Vec<u8>),
            Forward(String, ForwardState),
            FilePreview(String, FilePreviewOutcome),
        }

        struct TestCallback {
            tx: UnboundedSender<TestEvent>,
        }

        impl OrchestratorCallback for TestCallback {
            fn on_connection_state_changed(&self, state: ConnectionPublicState) {
                let _ = self.tx.send(TestEvent::Connection(state));
            }
            fn on_screen_update(&self, _update: ScreenUpdate) {}
            fn on_host_key(&self, _host: String, _port: u16, _fingerprint: String) -> bool { true }
            fn on_data(&self, data: Vec<u8>) {
                let _ = self.tx.send(TestEvent::Data(data));
            }
            fn on_trzsz_state_changed(&self, _state: TrzszPublicState) {}
            fn on_download_complete(&self, _file_name: Option<String>, _data: Vec<u8>) {}
            fn on_no_viable_path(&self) {}
            fn on_forward_state_changed(&self, id: String, state: ForwardState) {
                let _ = self.tx.send(TestEvent::Forward(id, state));
            }
            fn on_agent_sign_request(&self, _key_fingerprint: String) -> bool { true }
            fn on_clipboard_write(&self, _payload: ClipboardPayload) {}
            fn on_clipboard_pull_request(&self) -> Option<ClipboardPayload> { None }
            fn on_request_wifi_fd(&self) -> Option<crate::PlatformFd> { None }
            fn on_request_cellular_fd(&self) -> Option<crate::PlatformFd> { None }
            fn on_rebind_state_changed(&self, _state: crate::rebind_manager::RebindPublicState) {}
            fn on_prompt_jump(&self, _target: Option<crate::PromptJumpTarget>) {}
            fn on_prompt_output_copy_ready(&self, _text: Option<String>) {}
            fn on_file_preview_result(&self, request_id: String, outcome: FilePreviewOutcome) {
                let _ = self.tx.send(TestEvent::FilePreview(request_id, outcome));
            }
            fn on_notify(&self, _kind: crate::NotifyKind) {}
        }

        /// 公開鍵認証を無条件で受け入れ、`window_change_request`と`channel_close`を
        /// 記録しつつ、受信データをそのままechoし返す最小SSHサーバ。
        #[derive(Clone)]
        struct RecordingServer {
            window_changes: Arc<StdMutex<Vec<(u32, u32)>>>,
            channel_closed: Arc<AtomicBool>,
        }

        impl server::Server for RecordingServer {
            type Handler = RecordingHandler;
            fn new_client(&mut self, _: Option<SocketAddr>) -> RecordingHandler {
                RecordingHandler {
                    window_changes: self.window_changes.clone(),
                    channel_closed: self.channel_closed.clone(),
                }
            }
        }

        #[derive(Clone)]
        struct RecordingHandler {
            window_changes: Arc<StdMutex<Vec<(u32, u32)>>>,
            channel_closed: Arc<AtomicBool>,
        }

        #[async_trait::async_trait]
        impl server::Handler for RecordingHandler {
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
                session.data(channel, CryptoVec::from(data.to_vec()))?;
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

        async fn spawn_recording_server() -> (SocketAddr, Arc<StdMutex<Vec<(u32, u32)>>>, Arc<AtomicBool>) {
            let keypair = Ed25519Keypair::from_seed(&[7u8; 32]);
            let host_key = russh_keys::PrivateKey::from(keypair);
            let config = Arc::new(server::Config {
                keys: vec![host_key],
                ..Default::default()
            });
            let listener = TokioTcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let window_changes = Arc::new(StdMutex::new(Vec::new()));
            let channel_closed = Arc::new(AtomicBool::new(false));
            let mut sh = RecordingServer {
                window_changes: window_changes.clone(),
                channel_closed: channel_closed.clone(),
            };
            tokio::spawn(async move {
                use server::Server as _;
                let _ = sh.run_on_socket(config, &listener).await;
            });
            (addr, window_changes, channel_closed)
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

        async fn wait_forward_state(rx: &mut UnboundedReceiver<TestEvent>, expected_id: &str, expect_listening: bool) {
            for _ in 0..50 {
                match tokio::time::timeout(Duration::from_millis(200), rx.recv()).await {
                    Ok(Some(TestEvent::Forward(id, state))) if id == expected_id => {
                        match (&state, expect_listening) {
                            (ForwardState::Listening, true) => return,
                            (ForwardState::Stopped, false) => return,
                            _ => continue,
                        }
                    }
                    _ => continue,
                }
            }
            panic!(
                "did not observe expected ForwardState for id={} (listening={}) within timeout",
                expected_id, expect_listening
            );
        }

        async fn connect_orchestrator() -> (Arc<SessionOrchestrator>, UnboundedReceiver<TestEvent>, SocketAddr, Arc<StdMutex<Vec<(u32, u32)>>>, Arc<AtomicBool>) {
            let (addr, window_changes, channel_closed) = spawn_recording_server().await;
            let (tx, mut rx) = unbounded_channel::<TestEvent>();
            let orch = create_session_orchestrator(Box::new(TestCallback { tx }));
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
                let mut len = 0u32;
                for _ in 0..50 {
                    len = orch.scrollback_len();
                    if len > 0 {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
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
        // 上のRecordingServerはクライアントからのデータを無条件にechoするだけなので、
        // サーバー側が任意タイミングで能動的にバイトを送れる`ScriptedServer`を別途
        // 用意する(`session.handle()`を`shell_request`時に保存し、テストコードから
        // `Handle::data()`で直接送り込む)。これにより実物のtrzszトリガー/CFG/NUM
        // フレームをワイヤ上に流し、`SessionOrchestrator::trzsz_accept_download`等の
        // 委譲がno-opに変異した場合に実際に検知できるテストを組む。

        #[derive(Clone)]
        struct ScriptedServer {
            channel_handle: Arc<StdMutex<Option<(ChannelId, server::Handle)>>>,
            received: Arc<StdMutex<Vec<u8>>>,
        }

        impl server::Server for ScriptedServer {
            type Handler = ScriptedHandler;
            fn new_client(&mut self, _: Option<SocketAddr>) -> ScriptedHandler {
                ScriptedHandler {
                    channel_handle: self.channel_handle.clone(),
                    received: self.received.clone(),
                }
            }
        }

        #[derive(Clone)]
        struct ScriptedHandler {
            channel_handle: Arc<StdMutex<Option<(ChannelId, server::Handle)>>>,
            received: Arc<StdMutex<Vec<u8>>>,
        }

        #[async_trait::async_trait]
        impl server::Handler for ScriptedHandler {
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
                *self.channel_handle.lock().unwrap() = Some((channel, session.handle()));
                Ok(())
            }

            async fn data(
                &mut self, _channel: ChannelId, data: &[u8], _session: &mut ServerSession,
            ) -> Result<(), Self::Error> {
                self.received.lock().unwrap().extend_from_slice(data);
                Ok(())
            }
        }

        async fn spawn_scripted_server() -> (SocketAddr, Arc<StdMutex<Option<(ChannelId, server::Handle)>>>, Arc<StdMutex<Vec<u8>>>) {
            let keypair = Ed25519Keypair::from_seed(&[7u8; 32]);
            let host_key = russh_keys::PrivateKey::from(keypair);
            let config = Arc::new(server::Config {
                keys: vec![host_key],
                ..Default::default()
            });
            let listener = TokioTcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let channel_handle = Arc::new(StdMutex::new(None));
            let received = Arc::new(StdMutex::new(Vec::new()));
            let mut sh = ScriptedServer {
                channel_handle: channel_handle.clone(),
                received: received.clone(),
            };
            tokio::spawn(async move {
                use server::Server as _;
                let _ = sh.run_on_socket(config, &listener).await;
            });
            (addr, channel_handle, received)
        }

        async fn connect_scripted_orchestrator() -> (
            Arc<SessionOrchestrator>, UnboundedReceiver<TestEvent>,
            Arc<StdMutex<Option<(ChannelId, server::Handle)>>>, Arc<StdMutex<Vec<u8>>>,
        ) {
            let (addr, channel_handle, received) = spawn_scripted_server().await;
            let (tx, mut rx) = unbounded_channel::<TestEvent>();
            let orch = create_session_orchestrator(Box::new(TestCallback { tx }));
            orch.connect(ssh_config(addr, key_auth(1))).expect("connect should not fail synchronously");
            wait_connected(&mut rx).await;
            (orch, rx, channel_handle, received)
        }

        /// `shell_request`が届き`ScriptedHandler`が`Handle`を保存するまで待ってから、
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

        // ── add_local_forward / remove_forward ──

        #[test]
        fn add_local_forward_then_remove_forward_drive_the_real_transport() {
            crate::init_logger();
            let rt = tokio::runtime::Runtime::new().expect("failed to build test runtime");
            rt.block_on(async {
                let (orch, mut rx, _addr, _window_changes, _channel_closed) = connect_orchestrator().await;

                // bind_port=0でOSにポートを選ばせる(このテストは実際に転送先へ
                // 接続するのではなく、実transportまでコマンドが届いて本当に
                // リスナーが立ち上がった/畳まれたことをForwardStateコールバックで
                // 確認するだけなので、具体的なポート番号は不要)。
                orch.add_local_forward(
                    "fwd-1".to_string(), "127.0.0.1".to_string(), 0,
                    "example.invalid".to_string(), 80,
                );
                // ActiveSession::add_local_forwardがno-opに変異していれば
                // TransportCommandが送られず、リスナーも立ち上がらないため
                // ForwardState::Listeningは永遠に届かずタイムアウトする。
                wait_forward_state(&mut rx, "fwd-1", true).await;

                orch.remove_forward("fwd-1".to_string());
                // 同様にActiveSession::remove_forwardがno-opに変異していれば
                // リスナーは立ったままでForwardState::Stoppedは届かない。
                wait_forward_state(&mut rx, "fwd-1", false).await;
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
                // SGRリセットが無い限り再解決されない)。RecordingServerが素通しで
                // echoする性質を利用し、`\x1b[0m`をecho往復させてからテキストを送ることで
                // 新テーマでの解決を強制する。
                orch.send(b"\x1b[0m".to_vec());
                for i in 0..60 {
                    orch.send(format!("line-{i:03}\r\n").into_bytes());
                }
                wait_echo(&mut rx, b"line-059").await;

                let mut len = 0u32;
                for _ in 0..50 {
                    len = orch.scrollback_len();
                    if len > 0 {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
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
                // 委譲されていれば、RecordingServerのexec_requestが返す実物のls JSONが
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
