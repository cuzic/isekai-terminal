//! `SessionOrchestrator`の自動再接続に関する判断を、時計・ロック・spawn・コールバックから
//! 切り離した純粋reducer(docs/adr/0019-functional-core-effects.md §2.1 / §6 Step 3a)。
//!
//! ## 集約(`ReconnectState`)
//!
//! `OrchestratorState`(`orchestrator.rs`)は`reconnect: ReconnectState`を**フィールドとして**
//! 持つ。別ストアではなく、所有者は常に`OrchestratorState`1つ(ADR §4.3、`rust-ssot.md`)。
//! Step 3aの時点では集約の**定義**は全体だが、`apply`経由に移行した遷移は下表の
//! 「`apply`経由か」が「はい」のものだけで、それ以外の書き手(`disconnect`の
//! `user_initiated_disconnect`・バックグラウンド系の`background_state`)は従来どおり`pub(crate)`
//! フィールドを直接書く。**`phase`の書き手はStep 8a′ですべて`apply`経由**になり、
//! `reconnect_epoch`/`reconnect_loop_active`/`retry_attempt_in_flight`/`pending_wake`の本番の書き手も
//! Step 3b/3c(ループのtick会計・タイムアウト・`cancel_reconnect`)ですべて`apply`経由になった。
//!
//! `session_generation`(古いアダプタからの遅延コールバックを捨てる世代カウンタ)は
//! **この集約に含めない**。`reconnect_epoch`(ループの生存確認)とは別物で、統合すると
//! 古いコールバックが受理されうる(`orchestrator.rs`の`OrchestratorState::session_generation`
//! のdoc)。Eventには`generation`を別フィールドとして載せる(Step 8a′のedge判定で使う)。
//!
//! 秘密情報(`SshConfig`/`SshAuth`)は載せない(ADR §3-3)。直前の接続設定は不透明な
//! [`AttemptRef`]だけを持ち、実体の`LastConnectAttempt`はshell(`OrchestratorState`)が持つ。
//!
//! ## エントリポイント → Event 対応表(ADR §6 Step 3a)
//!
//! | エントリポイント(`orchestrator.rs`) | Event | 3aで`apply`経由か |
//! |---|---|---|
//! | `OrchestratorAdapter::on_connected` | [`ReconnectEvent::AttemptConnected`] | はい |
//! | `OrchestratorAdapter::on_disconnected` → `handle_unexpected_disconnect` | [`ReconnectEvent::AttemptDisconnected`] | はい |
//! | `apply_network_lost`(debounce満了) → `handle_unexpected_disconnect` | `AttemptDisconnected{generation: 現行, kind: NetworkLost}` | はい(同じ遷移) |
//! | `OrchestratorAdapter::new` | [`ReconnectEvent::SessionCreated`] | はい(8a′: 別世代のedgeが開いていれば`Lost(old)`) |
//! | `connect_via`(再接続ループの試行・フォアグラウンド復帰) | [`ReconnectEvent::ReconnectSessionStarting`] | はい(8a′、→Connecting) |
//! | フォアグラウンド復帰の再接続の同期失敗 | [`ReconnectEvent::ForegroundReconnectFailedSync`] | はい(8a′、→Idle) |
//! | 再接続ループtaskの起動直後 | [`ReconnectEvent::LoopStarted`] | はい(3b/3c: 初回`Reconnecting`+最初のタイマー) |
//! | 再接続ループのネットワークwake | [`ReconnectEvent::ReconnectWake`] | はい(rev6: `pending_wake`。3b/3c: tick会計に触れない`woke_early`分岐) |
//! | 再接続ループのtick | [`ReconnectEvent::ReconnectTick`] | はい(3b/3c: `elapsed`/`tick_count`・`due`・ギブアップもreducer) |
//! | 再接続ループの同期エラー | [`ReconnectEvent::AttemptFailedSync`] | はい |
//! | ループ起動の`AttemptRef`が解決できなかった(到達しない想定) | [`ReconnectEvent::LoopStartAborted`] | はい(3b/3c) |
//! | `begin_connect` | [`ReconnectEvent::ManualConnectStarted`] | はい(8a′、→Connecting) |
//! | 手動接続(`start_manual_connect`)の同期失敗 | [`ReconnectEvent::ManualConnectFailedSync`] | はい(RC-29、→Idle) |
//! | `disconnect` | [`ReconnectEvent::UserDisconnect`] | はい(RC-03: ループ動作中ならループを止め、進行中の試行を中断する) |
//! | `cancel_reconnect` | [`ReconnectEvent::CancelReconnect`] | はい(3b/3c: epochを進める書き手。RC-04: 進行中の試行も中断する) |
//! | `notify_did_enter_background` | `EnteredBackground{budget_ms}` | いいえ |
//! | `notify_background_budget_expired` | `BackgroundBudgetExpired` | いいえ |
//! | `notify_memory_warning` | `MemoryWarning` | いいえ |
//! | `notify_will_enter_foreground` | `WillEnterForeground` | いいえ |
//! | `notify_network_path_changed` | `NetworkPathChanged{satisfied}` | いいえ |
//! | `debug_set_reconnect_policy`系 | `PolicyChanged` | いいえ |
//!
//! 未移行のEventはenumにまだ定義しない(定義だけしてno-opにすると、任意Event列の
//! proptestが「未移行の入口は何もしない」という誤ったモデルを検証してしまうため)。
//! 移行するStepで追加する。
//!
//! ## タイマー(ADR §2.2-1)とtick会計(ADR §6 Step 3b/3c)
//!
//! 再接続ループのtickは`reconnect_epoch`をtokenとするタイマーである:
//! [`ReconnectEffect::StartReconnectLoop`]`{epoch}`でshellがtokio taskをspawnし、taskは
//! [`ReconnectEvent::LoopStarted`]`{epoch}`を戻す。以後reducerが返す
//! [`ReconnectEffect::ArmLoopTimer`]`{epoch, after}`ごとにshellは`after`だけ待ち(ネットワーク復帰の
//! `Notify`とレース)、満了なら[`ReconnectEvent::ReconnectTick`]、早期起床なら
//! [`ReconnectEvent::ReconnectWake`]を戻す。`ArmLoopTimer`が返らなければtaskは終了する。
//! 非現行epoch(またはループ非動作中)の`LoopStarted`/`ReconnectTick`/`ReconnectWake`と
//! 非現行epochの`AttemptFailedSync`/`LoopStartAborted`はStateを変えずEffectも返さない(proptestで検証)。
//!
//! `elapsed`/`tick_count`(ループ1本分の会計、`LoopClock`)・`due`の計算・タイムアウトによる
//! ギブアップはreducerが持つ(3b/3c)。ギブアップは`reconnect_epoch`を進める
//! (Step 3aレビューm5: 進めないと、ループ非動作なのに現行epochのtick/wakeが試行を開始しうる)。
//! 時計は読まない: 経過時間は「満了したタイマーの`after`の和」として数える(旧ループと同じ会計)。
//!
//! ## 接続エッジ(ADR §6 Step 8a′)
//!
//! Kotlin/Swiftが`ConnectionPublicState`の変化から「未接続→接続」「接続→未接続」の
//! エッジを自前のミラー状態(`prevConnected`)で検出する代わりに、このreducerが
//! 世代(`session_generation`)付きのエッジを明示的に出す
//! ([`ReconnectEffect::EdgeEstablished`]/[`ReconnectEffect::EdgeLost`]。shellが
//! `OrchestratorCallback::on_connection_edge`として公開する)。
//!
//! - `Established(g)`: 世代`g`の`AttemptConnected`で、まだその世代以降のエッジを出していなければ出す
//!   (同一世代の`AttemptConnected`重複ではエッジを出し直さない)。
//! - `Lost(g)`: 特定のEventではなく**phase遷移**で定義する(round 3 R3-1)。`edge_open == Some(g)`の
//!   まま`phase`をConnectedから他の値へ動かすapplyは、**同じapplyで**`Lost(g)`を出し`edge_open`を
//!   `None`にする(`ReconnectState::set_phase`が唯一の実装)。phaseの書き手5箇所
//!   (`on_connected`・`handle_unexpected_disconnect`・`connect_via`・`begin_connect`・
//!   フォアグラウンド復帰の同期失敗)はすべて`apply`経由なので、この定義は経路に依らず成り立つ。
//! - `SessionCreated`は、phase遷移を伴わずに新しいセッションが作られる経路が将来できた場合の保険
//!   (別世代のedgeが開いていれば`Lost(old)`)。現状の2経路(`begin_connect`/`connect_via`)では、
//!   直前のphase遷移のapplyで既に`Lost(old)`が出ているので何も出さない。
//! - 不変条件(proptest): 任意のEvent列で「各`g`について`Established(g)`は高々1回、その後
//!   `Established(g'>g)`より前に`Lost(g)`が正確に1回」。
//! - `phase`/`edge_open`/`last_established`はこのモジュールの外から**書けない**(PR #167レビューL-2)。
//!   `phase`は[`ReducerOwned`]で包んで公開し(読み取りと`==`比較だけ可能)、他の2つはprivate。
//!   orchestratorのshellが`phase`を直接書いて`Lost`を飛ばす経路は、コンパイルエラーになる。
//! - 範囲外(PR #167レビューL-3): orchestratorの破棄(Kotlinの`close()`/drop)はphase遷移ではないので
//!   `Lost`を出さない。Kotlinの`closePaneSession`は`disconnect()`の後に購読を止め、接続に紐づく
//!   ハンドルを自分で閉じるので、破棄後の`Lost`を必要としない。
// 純粋モジュール(`pure_modules.toml`登録、docs/adr/0019-functional-core-effects.md §2.3)。
// 時計・RNG・ロック・I/O型の直接使用を`clippy.toml`の`disallowed-*`で禁止する。
#![deny(clippy::disallowed_methods, clippy::disallowed_types)]

use std::time::Duration;

use crate::ConnectionIssueHint;

/// reducer(このモジュール)だけが書ける値(PR #167レビューL-2)。中身はprivateなので、
/// モジュール外では`get()`での読み取りと`==`比較しかできず、新しい値を作って代入できない
/// (例: orchestratorのshellが`s.reconnect.phase = ConnPhase::Idle`と書くとコンパイルエラーになり、
/// `ReconnectState::set_phase`が出す`Lost`を飛ばす経路を作れない)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ReducerOwned<T>(T);

impl<T: Copy> ReducerOwned<T> {
    pub(crate) fn get(self) -> T {
        self.0
    }
}

impl<T: PartialEq> PartialEq<T> for ReducerOwned<T> {
    fn eq(&self, other: &T) -> bool {
        self.0 == *other
    }
}

/// 自動再接続ループのタイミング(`OrchestratorState::reconnect_policy`、テストでは短い値に差し替える)。
/// ループは毎回のタイマー設定時にshellが読み直した値を[`ReconnectEvent`]に載せて渡す
/// (`debug_set_reconnect_policy`の即時反映、旧ループの「tickごとに読み直す」と同じ)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ReconnectPolicy {
    /// UIへライブ通知する間隔。
    pub(crate) tick: Duration,
    /// 実際に`connect_via`を試みる間隔(tickの整数倍)。
    pub(crate) retry_interval: Duration,
    /// これを超えて再接続できなければギブアップする。
    pub(crate) timeout: Duration,
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

impl ReconnectPolicy {
    /// 「何tickごとに1回試みるか」。経過時間を秒に丸めてから割り算すると、テスト用の
    /// サブ秒ポリシー(tick=10msなど)で常に0になり判定が壊れるため、tick単位で比較する。
    fn ticks_per_retry(&self) -> u128 {
        (self.retry_interval.as_nanos() / self.tick.as_nanos().max(1)).max(1)
    }

    fn timeout_secs(&self) -> u32 {
        self.timeout.as_secs() as u32
    }
}

/// 動作中の再接続ループ1本分のtick会計(ADR §6 Step 3b/3c)。`LoopStarted`で作られ、
/// ループが止まる遷移(成功・手動接続・中止・ギブアップ)で捨てられる。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LoopClock {
    /// 満了したtickタイマーの`after`の和(早期起床は数えない)。
    elapsed: Duration,
    /// 満了したtickの回数(早期起床は数えない)。
    tick_count: u64,
    /// 現在待っているタイマーを設定したときのポリシー。満了時の会計(`elapsed`への加算・
    /// `due`・タイムアウト判定)はこの値で行う(旧ループがsleep前に読んだ値で会計していたのと同じ)。
    armed: ReconnectPolicy,
}

/// 接続状態の SSOT。`ConnectionPublicState` の Connecting/Connected の別を
/// Rust 側でも保持し、`notify_network_path_changed` がミラー無しで判断できるようにする。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConnPhase {
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
pub(crate) enum BackgroundState {
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

/// `apply_network_lost`(`orchestrator.rs`)が`handle_unexpected_disconnect`へ渡す合成理由文字列。
/// [`DisconnectKind::classify`]がこの定数を直接比較するので、二重に書かないよう
/// 定数化してある(ADR round 3 m-R3-6: `DisconnectKind`と一緒にこのファイルへ移した)。
pub(crate) const NETWORK_LOST_REASON: &str = "network lost";

/// `handle_unexpected_disconnect`が受け取る`reason`文字列のRust内部用分類。
///
/// `SessionCallback::on_disconnected(reason: Option<String>)`には現状「切断理由の
/// 種別」を運ぶ専用フィールドが無く、`reason`文字列に頼っている ── この
/// trait(`SessionCallback`)には本番用の`OrchestratorAdapter`以外にテスト専用の
/// 実装が4箇所あり、シグネチャ変更・UniFFI経由でKotlin側に公開される文字列の
/// 変更はそれら全ての更新を要する大きめの変更になるため見送っている。この型は
/// あくまで`reason`文字列を読んだ*後*にRustのプロセス内だけで使う分類であり、
/// `on_disconnected`のシグネチャにも公開文字列そのものにも影響しない。
/// 分類ロジックを一箇所に一元化することで、`starts_with`/文字列比較が呼び出し側に
/// 増殖するのを防ぐ(rust-ssot.mdの「判断ロジックをRust側に一元化する」原則そのもの)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DisconnectKind {
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
    pub(crate) fn classify(reason: &Option<String>) -> Self {
        match reason.as_deref() {
            Some(r) if r.starts_with("remote process exited") => Self::GracefulRemoteExit,
            Some(r) if r == NETWORK_LOST_REASON => Self::NetworkLost,
            _ => Self::TransportError,
        }
    }
}

/// 直前の手動接続(`begin_connect`)を指す不透明な参照(ADR §3-3、round 2 m-R2-6)。
///
/// `begin_connect`だけが進める単調なID。実体の`LastConnectAttempt`(秘密を含む
/// `SshConfig`等)はshell側(`OrchestratorState::last_connect_attempt`)に置き、
/// このreducerには載せない。中身の数値は比較以外に意味を持たない。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct AttemptRef(u64);

impl AttemptRef {
    /// `prev`の次の参照を発行する(`prev == None`なら最初の参照)。
    pub(crate) fn next_after(prev: Option<AttemptRef>) -> AttemptRef {
        AttemptRef(prev.map_or(0, |p| p.0.wrapping_add(1)))
    }
}

/// 再接続ループが1回の試行を開始した理由(ログ用。判断には使わない)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AttemptSource {
    /// `retry_interval`ごとの通常のtick。
    Tick,
    /// 試行中に届いたネットワーク復帰wake(`pending_wake`)を、次のtickで消化した。
    PendingWake,
    /// ネットワーク復帰通知による早期起床(ボーナス試行)。
    NetworkWake,
}

/// 再接続に関する集約(ADR §6 Step 3a)。ソケット・`Notify`・ロック・時計を持たない。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReconnectState {
    /// 書き手は`ReconnectState::set_phase`だけ(モジュール外からは読み取りのみ、L-2)。
    pub(crate) phase: ReducerOwned<ConnPhase>,
    /// 自動再接続ループ自身の生存確認用epoch(ループタイマーのtoken)。新しい`connect_*`呼び出し・
    /// `cancel_reconnect()`・再接続成功・ループのギブアップのいずれかでインクリメントされ、
    /// ループは次のtickで自分のepochが古いと分かれば静かに終了する。
    pub(crate) reconnect_epoch: u64,
    /// 自動再接続ループが現在動作中かどうか。`on_disconnected`が二重にループを
    /// 起動しない・二重に`Disconnected`を通知しないための判定に使う。
    pub(crate) reconnect_loop_active: bool,
    /// ループが`connect_via`を発火してから、その試行の結果(generation一致の
    /// `on_connected`/`on_disconnected`)を観測するまでの間true。次のtickで
    /// 新しい試行を重ねて発火しないためのガード(ホスト鍵確認プロンプトの
    /// 多重発生を防ぐ)。
    pub(crate) retry_attempt_in_flight: bool,
    /// in-flight中に届いたnetwork restored wakeを、試行完了直後の再試行へ繋ぐ(eecba351)。
    pub(crate) pending_wake: bool,
    /// `SessionOrchestrator::disconnect()`が呼ばれた際に立てる。ユーザーが
    /// 明示的に切断した場合は自動再接続しない(tsshの「唯一の例外」と同じ)。
    /// 読み取った直後にfalseへ戻す一度きりのフラグ。
    pub(crate) user_initiated_disconnect: bool,
    /// #20: バックグラウンド遷移とセッション再接続要否のSSOT。
    pub(crate) background_state: BackgroundState,
    /// 直前の`begin_connect`の参照。`Some`のときだけ予期しない切断から自動再接続ループを
    /// 起動できる。shell側`OrchestratorState::last_connect_attempt`と常に同時に書く
    /// (`OrchestratorState::set_last_connect_attempt`)。
    pub(crate) last_attempt: Option<AttemptRef>,
    /// Step 8a′: `Established(g)`を出してまだ`Lost(g)`を出していない世代。phaseをConnected以外へ
    /// 動かす遷移は必ずこれを閉じる(`ReconnectState::set_phase`)ので、`Some`の間は`phase == Connected`。
    edge_open: Option<u64>,
    /// Step 8a′: 最後に`Established`を出した世代。これ以下の世代には`Established`を出し直さない
    /// (「各世代について`Established`は高々1回」を、遅延・重複した`AttemptConnected`に対しても守る)。
    last_established: Option<u64>,
    /// Step 3b/3c: 動作中のループのtick会計。`Some`の間は`reconnect_loop_active`。`StartReconnectLoop`
    /// から`LoopStarted`までの間(taskのspawn待ち)は`None`。
    loop_clock: Option<LoopClock>,
}

impl Default for ReconnectState {
    fn default() -> Self {
        Self {
            phase: ReducerOwned(ConnPhase::Idle),
            reconnect_epoch: 0,
            reconnect_loop_active: false,
            retry_attempt_in_flight: false,
            pending_wake: false,
            user_initiated_disconnect: false,
            background_state: BackgroundState::Foreground,
            last_attempt: None,
            edge_open: None,
            last_established: None,
            loop_clock: None,
        }
    }
}

/// 起きた事実(ADR §2.1)。遅れて届く事実は世代トークン(`generation`/`epoch`)を運ぶ(§2.2)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ReconnectEvent {
    /// 現行世代のセッションが接続を確立した(`OrchestratorAdapter::on_connected`)。
    /// `generation`の照合(`is_current()`)はshellが既に済ませている。`generation`は
    /// 接続エッジ(`Established`/`Lost`)の判定に使う(Step 8a′)。
    AttemptConnected { generation: u64 },
    /// 現行世代のセッションが切断した(アダプタ経由)、またはnetwork-lost debounceが満了した
    /// (`apply_network_lost`、`generation`は現行の`session_generation`)。
    /// `targets_local_network`はshellが`last_connect_attempt`から事前計算した
    /// 「接続先がプライベート/リンクローカル/mDNS名か」(#19のLocal Networkヒント判定材料)。
    /// `Lost`はphase遷移で決まる(ADR round 3 R3-1)ので、`generation`は判断に使わない。
    AttemptDisconnected {
        #[allow(dead_code)] // 判断には使わない(Lostはphase遷移で定義、ADR round 3 R3-1)
        generation: u64,
        kind: DisconnectKind,
        targets_local_network: bool,
    },
    /// `OrchestratorAdapter::new`が`session_generation`を進めた(同じ臨界区間でapplyする)。
    /// 別世代のedgeが開いていれば`Lost(old)`を出す保険(Step 8a′)。
    SessionCreated { new_generation: u64 },
    /// 手動接続(`begin_connect`)の開始。`attempt`はshellが[`AttemptRef::next_after`]で発行した
    /// 新しい参照で、受理された場合だけ`last_attempt`になる(実体の`LastConnectAttempt`はshellが
    /// 同じ臨界区間で記録する)。既に`Connecting`なら拒否する(Task #9の二重start防止)。
    ManualConnectStarted { attempt: AttemptRef },
    /// 自動再接続の1試行・フォアグラウンド復帰が新しいセッションを作る直前(`connect_via`、→Connecting)。
    ReconnectSessionStarting,
    /// フォアグラウンド復帰契機の再接続(`connect_via`)が同期的に失敗した(→Idle)。
    ForegroundReconnectFailedSync,
    /// `StartReconnectLoop{epoch}`でspawnされたループtaskが走り始めた。`policy`はshellが
    /// このapplyと同じ臨界区間で読んだ現在のポリシー(最初のタイマーの長さと会計に使う)。
    LoopStarted { epoch: u64, policy: ReconnectPolicy },
    /// 再接続ループ(`epoch`)のタイマー待ちが、満了前にネットワーク復帰通知で起こされた
    /// (旧`woke_early`分岐)。tick会計には触れない。`policy`は次のタイマー用(`LoopStarted`と同じ)。
    ReconnectWake { epoch: u64, policy: ReconnectPolicy },
    /// 再接続ループ(`epoch`)のタイマーが満了した(通常のtick)。会計は満了したタイマーを設定した
    /// ときのポリシーで行い、`policy`は次のタイマー用(`LoopStarted`と同じ)。
    ReconnectTick { epoch: u64, policy: ReconnectPolicy },
    /// 再接続ループ(`epoch`)が開始した試行が同期的に失敗した(`reconnect_attempt`が`Err`)。
    AttemptFailedSync { epoch: u64 },
    /// `cancel_reconnect()`(ユーザーによる自動再接続の中止)。
    CancelReconnect,
    /// `disconnect()`(ユーザー操作による切断・タブclose、RC-03)。ループ動作中なら`CancelReconnect`と
    /// 同じくループを止めて進行中の試行を中断し、Rust側から`Disconnected`を公開する(Kotlinに
    /// `cancel_reconnect`の併用を求めない、`rust-ssot.md`)。ループ非動作中は`user_initiated_disconnect`を
    /// 立てて現在のセッションを切断させ、結果はそのセッションの切断通知で届く。
    UserDisconnect,
    /// 手動接続(`start_manual_connect`)の`connect()`が同期的に失敗した(RC-29、→Idle)。shellは
    /// その試行の世代がまだ現行であることを同じ臨界区間で確かめてからapplyする。
    ManualConnectFailedSync,
    /// `StartReconnectLoop{epoch}`の`AttemptRef`をshellが解決できず、ループを起動しなかった
    /// (`set_last_connect_attempt`の不変条件が破れない限り到達しない。Step 3aレビューm2)。
    LoopStartAborted { epoch: u64 },
}

/// shellにやってほしいこと(ADR §2.1)。`apply`が返した順に解釈する。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ReconnectEffect {
    /// 保留中のnetwork-loss debounceを無効化する(`path_observer.invalidate()`)。
    /// `Connected`から離脱するあらゆる経路の合流点である`AttemptDisconnected`で必ず出す
    /// (4c02e605: これが1経路で抜けていて重複した"network lost"通知が出ていた)。
    InvalidatePathObserver,
    /// 動作中の再接続ループを起こす(`reconnect_wake.notify_one()`)。試行中に届いていた
    /// `pending_wake`を、試行結果の観測直後に再試行へ繋ぐ(eecba351)。
    WakeReconnectLoop,
    /// 自動再接続ループ(tokenは`epoch`のtickタイマー)を起動する。`attempt`の実体は
    /// shellが**同じ臨界区間内で**`last_connect_attempt`から解決する(§2.4-2のin-lock解決)。
    StartReconnectLoop { attempt: AttemptRef, epoch: u64 },
    /// `ConnectionPublicState::Disconnected{reason, issue_hint}`を公開する(`reason`はshellが持つ)。
    PublishDisconnected { issue_hint: Option<ConnectionIssueHint> },
    /// `ConnectionPublicState::Connected{host}`を公開する(`host`はshellが解決する)。
    PublishConnected,
    /// 再接続ループ(`epoch`)が今1回の試行(`reconnect_attempt`)を開始する。
    /// `retry_attempt_in_flight`は既に立っている。
    StartAttempt { epoch: u64, source: AttemptSource },
    /// 試行中に届いたwakeを`pending_wake`として保持した(ログ用。shellは記録するだけ)。
    PendingWakeRecorded { epoch: u64 },
    /// `ConnectionPublicState::Connecting`を公開する(`begin_connect`)。
    PublishConnecting,
    /// `ManualConnectStarted`を拒否した(既に`Connecting`)。Stateは変えていない。shell
    /// (`begin_connect`)は解釈せずに`Err(SshError::ConnectionFailed)`を返す。
    ManualConnectRejected,
    /// Step 8a′: 接続エッジ`Established(generation)`を公開する(`host`はshellが解決する)。
    /// `PublishConnected`の**後に**同じapplyから出る(ADR §2.4-4の固定順)。
    EdgeEstablished { generation: u64 },
    /// Step 8a′: 接続エッジ`Lost(generation)`を公開する。`edge_open == Some(generation)`のまま
    /// phaseをConnectedから動かしたapply(または別世代の`SessionCreated`/`AttemptConnected`)が出す。
    EdgeLost { generation: u64 },
    /// Step 3b/3c: ループ(`epoch`)の次のタイマーを`after`後に設定する(§2.2-1のArmTimer)。shellは満了で
    /// `ReconnectTick`、ネットワーク復帰の早期起床で`ReconnectWake`を戻す。これを返さなかったapplyの後、
    /// ループtaskは終了する。`elapsed_secs`はログ用。
    ArmLoopTimer { epoch: u64, after: Duration, elapsed_secs: u64 },
    /// Step 3b/3c: `ConnectionPublicState::Reconnecting{elapsed_secs, timeout_secs, reason}`を公開する
    /// (`reason`はループが起動時に受け取った切断理由で、shellが持つ)。
    PublishReconnecting { elapsed_secs: u32, timeout_secs: u32 },
    /// Step 3b/3c: ループがタイムアウトでギブアップした。shellは
    /// `Disconnected{reason: "reconnect timed out after {timeout_secs}s (last: ..)", issue_hint}`を公開する
    /// (`issue_hint`はループの接続設定からshellが計算する)。
    PublishReconnectTimedOut { timeout_secs: u32 },
    /// Step 3b/3c(ログ用): ループ(`epoch`)がネットワーク復帰通知で早期に起床した。
    LoopWokeEarly { epoch: u64 },
    /// Step 3b/3c(ログ用): ループ(`epoch`)のtickを1回数えた。
    LoopTicked { epoch: u64, tick_count: u64, elapsed_secs: u64 },
    /// RC-03/RC-04: ループを止めた遷移(中止・ユーザー切断・ギブアップ)で、進行中だったかもしれない
    /// 試行を中断する。shellは**applyと同じ臨界区間で**`session_generation`を進め(試行のセッションの
    /// 遅延コールバックを無効化し、後から成功しても`Connected`へ戻らないようにする)、ロック解放後に
    /// 現在のセッションを切断し、そのセッションに紐づく保留中の要求・転送状態を片付ける。
    AbortInFlightAttempt,
    /// RC-03: ループ非動作中のユーザー切断。shellは現在のセッションを切断する(結果は
    /// `user_initiated_disconnect`付きの`AttemptDisconnected`で届き、自動再接続しない)。
    DisconnectSession,
}

impl ReconnectState {
    /// 1つのEventを適用し、shellが解釈すべきEffect列を返す(ADR §2.1)。
    pub(crate) fn apply(&mut self, ev: ReconnectEvent) -> Vec<ReconnectEffect> {
        match ev {
            ReconnectEvent::AttemptConnected { generation } => self.on_attempt_connected(generation),
            ReconnectEvent::AttemptDisconnected { generation: _, kind, targets_local_network } => {
                self.on_attempt_disconnected(kind, targets_local_network)
            }
            ReconnectEvent::SessionCreated { new_generation } => {
                let mut effects = Vec::new();
                if self.edge_open.is_some_and(|g| g != new_generation) {
                    self.close_edge(&mut effects);
                }
                effects
            }
            ReconnectEvent::ManualConnectStarted { attempt } => self.on_manual_connect_started(attempt),
            ReconnectEvent::ReconnectSessionStarting => {
                let mut effects = Vec::new();
                self.set_phase(ConnPhase::Connecting, &mut effects);
                effects
            }
            ReconnectEvent::ForegroundReconnectFailedSync => {
                // #20 codexレビュー指摘: 一回限りの呼び出しなので、`Connecting`のまま
                // 固まらないようこの場で`Idle`へ戻し失敗を通知する(`reason`はshellが持つ)。
                let mut effects = Vec::new();
                self.set_phase(ConnPhase::Idle, &mut effects);
                effects.push(ReconnectEffect::PublishDisconnected { issue_hint: None });
                effects
            }
            ReconnectEvent::LoopStarted { epoch, policy } => self.on_loop_started(epoch, policy),
            ReconnectEvent::ReconnectWake { epoch, policy } => self.on_reconnect_wake(epoch, policy),
            ReconnectEvent::ReconnectTick { epoch, policy } => self.on_reconnect_tick(epoch, policy),
            ReconnectEvent::AttemptFailedSync { epoch } => {
                let mut effects = Vec::new();
                if epoch == self.reconnect_epoch {
                    self.retry_attempt_in_flight = false;
                    // RC-29: 試行(`connect_via`)は`ReconnectSessionStarting`でphaseを`Connecting`にしてから
                    // 同期的に失敗する。戻さないと、後の手動接続が`ManualConnectStarted`の二重start防止で
                    // 永久に拒否される。
                    if self.phase() == ConnPhase::Connecting {
                        self.set_phase(ConnPhase::Idle, &mut effects);
                    }
                }
                effects
            }
            ReconnectEvent::CancelReconnect => {
                // ループが動作中だった場合のみ`Disconnected`を通知する(動いていない時に呼ばれても
                // 無音、UIは`isReconnecting`の間だけ「中止」操作を出す想定)。理由文字列はshellが持つ。
                let was_active = self.reconnect_loop_active;
                self.stop_loop();
                if was_active {
                    let mut effects = Vec::new();
                    self.abort_loop_attempt(&mut effects);
                    effects.push(ReconnectEffect::PublishDisconnected { issue_hint: None });
                    effects
                } else {
                    Vec::new()
                }
            }
            ReconnectEvent::UserDisconnect => {
                if self.reconnect_loop_active {
                    self.stop_loop();
                    self.user_initiated_disconnect = false;
                    let mut effects = vec![ReconnectEffect::InvalidatePathObserver];
                    self.abort_loop_attempt(&mut effects);
                    effects.push(ReconnectEffect::PublishDisconnected { issue_hint: None });
                    effects
                } else {
                    // 「これから来る切断通知はユーザー操作起因」の印(`AttemptDisconnected`が読んで下ろす)。
                    self.user_initiated_disconnect = true;
                    vec![ReconnectEffect::DisconnectSession]
                }
            }
            ReconnectEvent::ManualConnectFailedSync => {
                let mut effects = Vec::new();
                if self.phase() == ConnPhase::Connecting {
                    self.set_phase(ConnPhase::Idle, &mut effects);
                }
                effects
            }
            ReconnectEvent::LoopStartAborted { epoch } => {
                if epoch != self.reconnect_epoch {
                    return Vec::new();
                }
                // ループを起動できなかった切断は、`on_attempt_disconnected`の「直前の接続設定が無い」
                // 分岐(`last_attempt == None`)と同じ形に揃える: ループ非動作・`background_state`は
                // Foreground(`pending_wake`・`retry_attempt_in_flight`はその切断の遷移で既に下りている)。
                self.reconnect_loop_active = false;
                self.loop_clock = None;
                self.background_state = BackgroundState::Foreground;
                vec![ReconnectEffect::PublishDisconnected { issue_hint: None }]
            }
        }
    }

    /// 現在の接続phase(読み取り専用、L-2)。
    pub(crate) fn phase(&self) -> ConnPhase {
        self.phase.get()
    }

    /// テスト専用: `phase`と`last_attempt`を指定した初期状態(edgeは開いていない)。
    /// `phase`はモジュール外から書けない(L-2)ので、orchestratorのテストの初期状態はこれで作る。
    #[cfg(test)]
    pub(crate) fn for_test(phase: ConnPhase, last_attempt: Option<AttemptRef>) -> Self {
        Self { phase: ReducerOwned(phase), last_attempt, ..Self::default() }
    }

    /// テスト専用: `phase`だけを書き換える(Connected以外ならedgeは`Lost`を出さずに捨てる)。本番コードからは
    /// 呼べない(`cfg(test)`)ので、L-2の「phaseの書き手は`set_phase`だけ」は本番ビルドで保たれる。
    #[cfg(test)]
    pub(crate) fn force_phase_for_test(&mut self, phase: ConnPhase) {
        self.phase = ReducerOwned(phase);
        if phase != ConnPhase::Connected {
            self.edge_open = None;
        }
    }

    /// `phase`の唯一の書き手(Step 8a′)。Connected以外へ動かすとき、edgeが開いていれば
    /// 同じapplyの出力に`Lost(g)`を積んでedgeを閉じる(ADR round 3 R3-1の定義そのもの)。
    fn set_phase(&mut self, phase: ConnPhase, effects: &mut Vec<ReconnectEffect>) {
        if phase != ConnPhase::Connected {
            self.close_edge(effects);
        }
        self.phase = ReducerOwned(phase);
    }

    /// 動作中のループを止める(epochを進めて旧ループのタイマー・wake・試行結果を無効化する)。
    /// 再接続成功・手動接続・中止・ギブアップの共通部分。
    fn stop_loop(&mut self) {
        self.reconnect_epoch = self.reconnect_epoch.wrapping_add(1);
        self.reconnect_loop_active = false;
        self.retry_attempt_in_flight = false;
        self.pending_wake = false;
        self.loop_clock = None;
    }

    /// 止めたループ(`stop_loop`の後)が進めていたかもしれない試行を中断する(RC-03/RC-04)。phaseを
    /// `Idle`へ戻し(試行中は`Connecting`)、ループが追跡していたバックグラウンド遷移状態も手放す
    /// (自動ループが始まらなかった切断と同じ扱い)。試行のセッション自体の無効化・切断はshellが
    /// [`ReconnectEffect::AbortInFlightAttempt`]で行う。
    fn abort_loop_attempt(&mut self, effects: &mut Vec<ReconnectEffect>) {
        self.set_phase(ConnPhase::Idle, effects);
        self.background_state = BackgroundState::Foreground;
        effects.push(ReconnectEffect::AbortInFlightAttempt);
    }

    /// ループ`epoch`が現行で、`LoopStarted`済みで動作中か。そうでないtick/wakeはstale扱い。
    fn loop_is_live(&self, epoch: u64) -> bool {
        epoch == self.reconnect_epoch && self.reconnect_loop_active && self.loop_clock.is_some()
    }

    /// 次のタイマーを`policy`で設定する(`loop_clock`が`Some`のときだけ呼ぶ)。
    fn arm_loop_timer(&mut self, epoch: u64, policy: ReconnectPolicy, effects: &mut Vec<ReconnectEffect>) {
        if let Some(clock) = self.loop_clock.as_mut() {
            clock.armed = policy;
            effects.push(ReconnectEffect::ArmLoopTimer {
                epoch,
                after: policy.tick,
                elapsed_secs: clock.elapsed.as_secs(),
            });
        }
    }

    /// ループtaskの起動。`StartReconnectLoop`から起動までの間に主導権が移っていた(epochが古い・
    /// ループが止まった)場合と、同じepochの2回目の`LoopStarted`は何も返さず、taskは初回の
    /// `Reconnecting`通知すら出さずに終了する。
    fn on_loop_started(&mut self, epoch: u64, policy: ReconnectPolicy) -> Vec<ReconnectEffect> {
        if epoch != self.reconnect_epoch || !self.reconnect_loop_active || self.loop_clock.is_some() {
            return Vec::new();
        }
        self.loop_clock = Some(LoopClock { elapsed: Duration::ZERO, tick_count: 0, armed: policy });
        let mut effects =
            vec![ReconnectEffect::PublishReconnecting { elapsed_secs: 0, timeout_secs: policy.timeout_secs() }];
        self.arm_loop_timer(epoch, policy, &mut effects);
        effects
    }

    fn close_edge(&mut self, effects: &mut Vec<ReconnectEffect>) {
        if let Some(generation) = self.edge_open.take() {
            effects.push(ReconnectEffect::EdgeLost { generation });
        }
    }

    fn on_attempt_connected(&mut self, generation: u64) -> Vec<ReconnectEffect> {
        let mut effects = Vec::new();
        // 別世代のedgeが開いたままなら、新しい世代の`Established`より前に閉じる(ADR N-4)。
        if self.edge_open.is_some_and(|g| g != generation) {
            self.close_edge(&mut effects);
        }
        self.set_phase(ConnPhase::Connected, &mut effects);
        // 再接続ループが動いていたなら、成功したのでここで止める。
        self.stop_loop();
        effects.push(ReconnectEffect::PublishConnected);
        // 同一世代の`AttemptConnected`重複(edgeが既にその世代で開いている、または既に
        // その世代以降のedgeを出した)ではエッジを出し直さない(round 3 m-R3-2)。
        if self.edge_open.is_none() && !matches!(self.last_established, Some(last) if generation <= last) {
            self.edge_open = Some(generation);
            self.last_established = Some(generation);
            effects.push(ReconnectEffect::EdgeEstablished { generation });
        }
        effects
    }

    /// 手動接続の開始(`begin_connect`)。`Connecting`中(=前の`connect_*`がまだ実行中)は
    /// 拒否し、Stateを一切変えない。`Connected`中は「別セッションへの手動切り替え」として
    /// 意図的に受理する(この遷移で旧世代の`Lost`が出る、ADR round 2 N-4)。
    fn on_manual_connect_started(&mut self, attempt: AttemptRef) -> Vec<ReconnectEffect> {
        if self.phase() == ConnPhase::Connecting {
            return vec![ReconnectEffect::ManualConnectRejected];
        }
        let mut effects = Vec::new();
        self.set_phase(ConnPhase::Connecting, &mut effects);
        self.last_attempt = Some(attempt);
        // 新しい手動接続が始まった以上、直前のdisconnect()由来のフラグや
        // 実行中だったかもしれない自動再接続ループは無関係になる。
        self.user_initiated_disconnect = false;
        self.stop_loop();
        // #20: 手動接続はフォアグラウンドの操作でしか起こり得ない。直前の
        // バックグラウンド遷移状態は無関係になる。
        self.background_state = BackgroundState::Foreground;
        // 新しい接続試行が始まった時点で、直前のセッションに対して保留中だった
        // network-path debounceは無効化する。そうしないと、瞬断のdebounce待機中に
        // 手動で切断/別transportへ再接続した場合、無関係な新しいセッションを
        // 誤って切断してしまう(レビューで指摘された実際の不具合)。
        effects.push(ReconnectEffect::InvalidatePathObserver);
        effects.push(ReconnectEffect::PublishConnecting);
        effects
    }

    /// 予期しない切断(アダプタ経由・network-lost debounce満了の両方)の共通遷移。
    /// 一度`Connected`になっていて・ユーザーが明示的に切断したのでなく・リモートプロセスの
    /// 正常終了でもなく・直前の接続設定が分かっていれば自動再接続ループを起動する。
    /// 既にループが動作中の切断(=1回のリトライ試行自体の失敗)は、二重にループを
    /// 起動せず・連続で`Disconnected`を通知もせず、ループ自身のtickに任せる。
    fn on_attempt_disconnected(
        &mut self,
        kind: DisconnectKind,
        targets_local_network: bool,
    ) -> Vec<ReconnectEffect> {
        let was_connected = self.phase() == ConnPhase::Connected;
        let user_initiated = self.user_initiated_disconnect;
        let graceful_exit = kind == DisconnectKind::GracefulRemoteExit;
        let wake_reconnect_loop = self.reconnect_loop_active && self.pending_wake;
        self.user_initiated_disconnect = false;
        let mut effects = vec![ReconnectEffect::InvalidatePathObserver];
        self.set_phase(ConnPhase::Idle, &mut effects);
        self.retry_attempt_in_flight = false;

        if self.reconnect_loop_active {
            // `pending_wake`はここでは下ろさない: 起こされたループの`ReconnectWake`が
            // 試行開始と同時に下ろす(試行が始まらなかった場合も次のtickで消化される)。
            if wake_reconnect_loop {
                effects.push(ReconnectEffect::WakeReconnectLoop);
            }
        } else if was_connected && !user_initiated && !graceful_exit {
            self.pending_wake = false;
            match self.last_attempt {
                Some(attempt) => {
                    self.reconnect_loop_active = true;
                    self.reconnect_epoch = self.reconnect_epoch.wrapping_add(1);
                    // 新しいループのtick会計は、spawnされたtaskの`LoopStarted`で始まる。
                    self.loop_clock = None;
                    effects.push(ReconnectEffect::StartReconnectLoop { attempt, epoch: self.reconnect_epoch });
                }
                None => {
                    // #20: 自動ループが始まらない=以降フォアグラウンド復帰時の
                    // 自動再接続もこの切断イベントの責務ではなくなる。
                    self.background_state = BackgroundState::Foreground;
                    effects.push(ReconnectEffect::PublishDisconnected { issue_hint: None });
                }
            }
        } else {
            self.pending_wake = false;
            // #19: 一度もConnectedに至らず切断された(=接続試行そのものの失敗)場合
            // だけLocal Network Privacyヒントの対象にする。Connected後の正常終了/
            // ユーザー切断ではヒントを付けても意味がない。
            // NOTE: `targets_local_network: bool`で足りるのは`ConnectionIssueHint`が現状1 variant
            // だけだから。variantが増えたら、shellの事前計算をヒントの種類ごとの入力に広げること。
            let issue_hint = (!was_connected && targets_local_network)
                .then_some(ConnectionIssueHint::LocalNetworkPermissionPossiblyDenied);
            // #20: 自動ループが始まらない切断は、バックグラウンド遷移の追跡対象外に戻す
            // (ユーザー切断・正常終了・そもそも接続失敗だった場合を含む)。
            self.background_state = BackgroundState::Foreground;
            effects.push(ReconnectEffect::PublishDisconnected { issue_hint });
        }
        effects
    }

    /// ネットワーク復帰通知による早期起床: tick会計には触れず、試行中でなければ
    /// 「今すぐ1回試す」ボーナス試行を開始する。試行中なら`pending_wake`として保持し、
    /// 試行結果の観測直後(`AttemptDisconnected`の`WakeReconnectLoop`)か次のtickで消化する。
    /// 早期起床の後は(会計を進めずに)次のタイマーを満額で設定し直す(旧ループの`continue`と同じ)。
    fn on_reconnect_wake(&mut self, epoch: u64, policy: ReconnectPolicy) -> Vec<ReconnectEffect> {
        if !self.loop_is_live(epoch) {
            return Vec::new();
        }
        let mut effects = vec![ReconnectEffect::LoopWokeEarly { epoch }];
        if !self.retry_attempt_in_flight {
            self.retry_attempt_in_flight = true;
            self.pending_wake = false;
            effects.push(ReconnectEffect::StartAttempt { epoch, source: AttemptSource::NetworkWake });
        } else {
            self.pending_wake = true;
            effects.push(ReconnectEffect::PendingWakeRecorded { epoch });
        }
        self.arm_loop_timer(epoch, policy, &mut effects);
        effects
    }

    /// 通常のtick(タイマー満了): 満了したタイマーを設定したときのポリシーで`elapsed`/`tick_count`を
    /// 進め、`timeout`に達していればギブアップする(epochを進め、次のタイマーを設定しない)。
    /// そうでなければ`Reconnecting`を公開し、試行中でなく`retry_interval`に達したか(`due`)保持中の
    /// `pending_wake`があれば1回の試行を開始し、次のタイマーを`policy`で設定する。
    fn on_reconnect_tick(&mut self, epoch: u64, policy: ReconnectPolicy) -> Vec<ReconnectEffect> {
        if !self.loop_is_live(epoch) {
            return Vec::new();
        }
        let Some(clock) = self.loop_clock.as_mut() else {
            return Vec::new();
        };
        let armed = clock.armed;
        clock.elapsed = clock.elapsed.saturating_add(armed.tick);
        clock.tick_count = clock.tick_count.saturating_add(1);
        let (elapsed, tick_count) = (clock.elapsed, clock.tick_count);
        let timeout_secs = armed.timeout_secs();
        let mut effects =
            vec![ReconnectEffect::LoopTicked { epoch, tick_count, elapsed_secs: elapsed.as_secs() }];

        if elapsed >= armed.timeout {
            // ギブアップ。epochを進めるので、既に送出済みの試行の遅延結果(同epochの
            // `AttemptFailedSync`)や、万一残ったtick/wakeは以後stale(Step 3aレビューm5)。
            self.stop_loop();
            // RC-04: 送出済みの試行が後から成功して`Connected`へ戻らないよう中断する。
            self.abort_loop_attempt(&mut effects);
            effects.push(ReconnectEffect::PublishReconnectTimedOut { timeout_secs });
            return effects;
        }

        effects.push(ReconnectEffect::PublishReconnecting { elapsed_secs: elapsed.as_secs() as u32, timeout_secs });
        let due = u128::from(tick_count) % armed.ticks_per_retry() == 0;
        if !self.retry_attempt_in_flight && (due || self.pending_wake) {
            let source = if self.pending_wake { AttemptSource::PendingWake } else { AttemptSource::Tick };
            self.retry_attempt_in_flight = true;
            self.pending_wake = false;
            effects.push(ReconnectEffect::StartAttempt { epoch, source });
        }
        self.arm_loop_timer(epoch, policy, &mut effects);
        effects
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, clippy::disallowed_types)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    // ── 参照モデル: Step 3a以前の`orchestrator.rs`の判断をそのまま書き写したもの ──
    // (`handle_unexpected_disconnect`のローカル`enum Action`と、`spawn_reconnect_loop`の
    // wake/tick分岐)。reducerが挙動を変えていないことをこれとの一致で確かめる。

    #[derive(Debug, PartialEq, Eq)]
    enum LegacyAction {
        Suppress { wake_reconnect_loop: bool },
        StartLoop(u64),
        NotifyDisconnected(Option<ConnectionIssueHint>),
    }

    fn legacy_handle_unexpected_disconnect(
        s: &mut ReconnectState,
        reason: &Option<String>,
        targets_local_network: bool,
    ) -> LegacyAction {
        let was_connected = s.phase == ConnPhase::Connected;
        let user_initiated = s.user_initiated_disconnect;
        let graceful_exit = DisconnectKind::classify(reason) == DisconnectKind::GracefulRemoteExit;
        let wake_reconnect_loop = s.reconnect_loop_active && s.pending_wake;
        s.user_initiated_disconnect = false;
        s.phase = ReducerOwned(ConnPhase::Idle);
        s.retry_attempt_in_flight = false;
        if s.reconnect_loop_active {
            LegacyAction::Suppress { wake_reconnect_loop }
        } else if was_connected && !user_initiated && !graceful_exit {
            s.pending_wake = false;
            match s.last_attempt {
                Some(_) => {
                    s.reconnect_loop_active = true;
                    s.reconnect_epoch = s.reconnect_epoch.wrapping_add(1);
                    LegacyAction::StartLoop(s.reconnect_epoch)
                }
                None => {
                    s.background_state = BackgroundState::Foreground;
                    LegacyAction::NotifyDisconnected(None)
                }
            }
        } else {
            s.pending_wake = false;
            let issue_hint = if !was_connected && targets_local_network {
                Some(ConnectionIssueHint::LocalNetworkPermissionPossiblyDenied)
            } else {
                None
            };
            s.background_state = BackgroundState::Foreground;
            LegacyAction::NotifyDisconnected(issue_hint)
        }
    }

    /// 旧`woke_early`分岐。戻り値は(試行開始したか)。
    fn legacy_wake(s: &mut ReconnectState, epoch: u64) -> bool {
        if s.reconnect_epoch == epoch && !s.retry_attempt_in_flight {
            s.retry_attempt_in_flight = true;
            s.pending_wake = false;
            true
        } else {
            if s.reconnect_epoch == epoch && s.retry_attempt_in_flight {
                s.pending_wake = true;
            }
            false
        }
    }

    /// 旧tick分岐の試行判定。
    fn legacy_tick(s: &mut ReconnectState, epoch: u64, due: bool) -> bool {
        let pending_wake = s.pending_wake;
        if s.reconnect_epoch == epoch && !s.retry_attempt_in_flight && (due || pending_wake) {
            s.retry_attempt_in_flight = true;
            s.pending_wake = false;
            true
        } else {
            false
        }
    }

    fn reason_for(kind: DisconnectKind) -> Option<String> {
        match kind {
            DisconnectKind::GracefulRemoteExit => Some("remote process exited (status 0)".to_string()),
            DisconnectKind::NetworkLost => Some(NETWORK_LOST_REASON.to_string()),
            DisconnectKind::TransportError => Some("peer closed".to_string()),
        }
    }

    /// Step 8a′の接続エッジEffectを除いた列(3a以前の判断との比較用)。
    fn without_edges(effects: &[ReconnectEffect]) -> Vec<ReconnectEffect> {
        effects.iter().filter(|e| !is_edge(e)).cloned().collect()
    }

    fn is_edge(e: &ReconnectEffect) -> bool {
        matches!(e, ReconnectEffect::EdgeEstablished { .. } | ReconnectEffect::EdgeLost { .. })
    }

    fn effect_to_legacy(effects: &[ReconnectEffect]) -> LegacyAction {
        let effects = without_edges(effects);
        assert_eq!(effects.first(), Some(&ReconnectEffect::InvalidatePathObserver));
        match &effects[1..] {
            [] => LegacyAction::Suppress { wake_reconnect_loop: false },
            [ReconnectEffect::WakeReconnectLoop] => LegacyAction::Suppress { wake_reconnect_loop: true },
            [ReconnectEffect::StartReconnectLoop { epoch, .. }] => LegacyAction::StartLoop(*epoch),
            [ReconnectEffect::PublishDisconnected { issue_hint }] => LegacyAction::NotifyDisconnected(*issue_hint),
            other => panic!("unexpected effects: {other:?}"),
        }
    }

    /// 旧`spawn_reconnect_loop`の1周(sleepから戻った後)の観測可能な出力。
    #[derive(Debug, PartialEq, Eq)]
    enum LoopOut {
        Reconnecting { elapsed_secs: u32, timeout_secs: u32 },
        TimedOut { timeout_secs: u32 },
        Attempt(AttemptSource),
        PendingWake,
    }

    /// 旧`spawn_reconnect_loop`のtask側(tick会計・`woke_early`分岐・ギブアップ)を、Step 3b/3c以前の
    /// `orchestrator.rs`からそのまま書き写したもの。試行の判断は旧`legacy_wake`/`legacy_tick`。
    struct LegacyLoop {
        epoch: u64,
        /// ループ先頭で読んだポリシー(この周のsleepの長さと会計に使う)。
        policy: ReconnectPolicy,
        elapsed: Duration,
        tick_count: u128,
        exited: bool,
    }

    impl LegacyLoop {
        /// spawn直後: epochが古ければ初回の`Reconnecting`すら出さずに終了する。
        fn start(s: &ReconnectState, epoch: u64, policy: ReconnectPolicy) -> (Self, Vec<LoopOut>) {
            let exited = s.reconnect_epoch != epoch;
            let out = if exited {
                Vec::new()
            } else {
                vec![LoopOut::Reconnecting { elapsed_secs: 0, timeout_secs: policy.timeout.as_secs() as u32 }]
            };
            (LegacyLoop { epoch, policy, elapsed: Duration::ZERO, tick_count: 0, exited }, out)
        }

        /// sleepから戻った後の1周。`next_policy`は次の周の先頭で読み直すポリシー。
        fn step(&mut self, s: &mut ReconnectState, woke_early: bool, next_policy: ReconnectPolicy) -> Vec<LoopOut> {
            if self.exited {
                return Vec::new();
            }
            if s.reconnect_epoch != self.epoch {
                self.exited = true;
                return Vec::new();
            }
            let mut out = Vec::new();
            if woke_early {
                if legacy_wake(s, self.epoch) {
                    out.push(LoopOut::Attempt(AttemptSource::NetworkWake));
                } else {
                    out.push(LoopOut::PendingWake);
                }
                self.policy = next_policy;
                return out;
            }
            let ticks_per_retry = (self.policy.retry_interval.as_nanos() / self.policy.tick.as_nanos().max(1)).max(1);
            let timeout_secs = self.policy.timeout.as_secs() as u32;
            self.elapsed = self.elapsed.saturating_add(self.policy.tick);
            self.tick_count += 1;
            if self.elapsed >= self.policy.timeout {
                if s.reconnect_epoch == self.epoch {
                    s.reconnect_loop_active = false;
                    s.retry_attempt_in_flight = false;
                    s.pending_wake = false;
                }
                self.exited = true;
                out.push(LoopOut::TimedOut { timeout_secs });
                return out;
            }
            out.push(LoopOut::Reconnecting { elapsed_secs: self.elapsed.as_secs() as u32, timeout_secs });
            let due = self.tick_count % ticks_per_retry == 0;
            let pending_wake = s.pending_wake;
            if legacy_tick(s, self.epoch, due) {
                out.push(LoopOut::Attempt(if pending_wake { AttemptSource::PendingWake } else { AttemptSource::Tick }));
            }
            self.policy = next_policy;
            out
        }
    }

    /// 旧`cancel_reconnect`。戻り値は`Disconnected`を通知したか。
    fn legacy_cancel(s: &mut ReconnectState) -> bool {
        let was_active = s.reconnect_loop_active;
        s.reconnect_epoch = s.reconnect_epoch.wrapping_add(1);
        s.reconnect_loop_active = false;
        s.retry_attempt_in_flight = false;
        s.pending_wake = false;
        was_active
    }

    /// reducerのEffect列から、旧ループと比較できる出力だけを取り出す。
    fn loop_outputs(effects: &[ReconnectEffect]) -> Vec<LoopOut> {
        effects
            .iter()
            .filter_map(|e| match e {
                ReconnectEffect::PublishReconnecting { elapsed_secs, timeout_secs } => {
                    Some(LoopOut::Reconnecting { elapsed_secs: *elapsed_secs, timeout_secs: *timeout_secs })
                }
                ReconnectEffect::PublishReconnectTimedOut { timeout_secs } => {
                    Some(LoopOut::TimedOut { timeout_secs: *timeout_secs })
                }
                ReconnectEffect::StartAttempt { source, .. } => Some(LoopOut::Attempt(*source)),
                ReconnectEffect::PendingWakeRecorded { .. } => Some(LoopOut::PendingWake),
                _ => None,
            })
            .collect()
    }

    fn arms_timer(effects: &[ReconnectEffect]) -> bool {
        effects.iter().any(|e| matches!(e, ReconnectEffect::ArmLoopTimer { .. }))
    }

    /// 旧実装と比較するStateの射影(epoch・loop_clock・edgeはStep 3b/3c/8a′で意図的に変わったので除く)。
    #[allow(clippy::type_complexity)]
    fn projection(s: &ReconnectState) -> (ConnPhase, bool, bool, bool, bool, BackgroundState, Option<AttemptRef>) {
        (
            s.phase(),
            s.reconnect_loop_active,
            s.retry_attempt_in_flight,
            s.pending_wake,
            s.user_initiated_disconnect,
            s.background_state,
            s.last_attempt,
        )
    }

    /// 接続済み・再接続可能な状態から予期しない切断でループを起動し、その`epoch`を返す。
    fn start_loop_from_connected(s: &mut ReconnectState) -> u64 {
        let effects = s.apply(ReconnectEvent::AttemptDisconnected {
            generation: 0,
            kind: DisconnectKind::TransportError,
            targets_local_network: false,
        });
        match effects.iter().find_map(|e| match e {
            ReconnectEffect::StartReconnectLoop { epoch, .. } => Some(*epoch),
            _ => None,
        }) {
            Some(epoch) => epoch,
            None => panic!("loop did not start: {effects:?}"),
        }
    }

    /// ループ動作中に起きうる1ステップ(ループtaskの起床、または外から届くEvent)。
    #[derive(Debug, Clone)]
    enum LoopStep {
        Woke { early: bool, next: ReconnectPolicy },
        FailedSync,
        Disconnected(DisconnectKind),
        Cancel,
        Connected,
    }

    fn loop_step_strategy() -> impl Strategy<Value = LoopStep> {
        prop_oneof![
            8 => (any::<bool>(), policy_strategy()).prop_map(|(early, next)| LoopStep::Woke { early, next }),
            2 => Just(LoopStep::FailedSync),
            2 => kind_strategy().prop_map(LoopStep::Disconnected),
            1 => Just(LoopStep::Cancel),
            1 => Just(LoopStep::Connected),
        ]
    }

    // ── strategies ──

    fn phase_strategy() -> impl Strategy<Value = ConnPhase> {
        prop_oneof![Just(ConnPhase::Idle), Just(ConnPhase::Connecting), Just(ConnPhase::Connected)]
    }

    fn background_strategy() -> impl Strategy<Value = BackgroundState> {
        prop_oneof![
            Just(BackgroundState::Foreground),
            Just(BackgroundState::Quiescing),
            Just(BackgroundState::Suspended)
        ]
    }

    fn kind_strategy() -> impl Strategy<Value = DisconnectKind> {
        prop_oneof![
            Just(DisconnectKind::GracefulRemoteExit),
            Just(DisconnectKind::NetworkLost),
            Just(DisconnectKind::TransportError)
        ]
    }

    /// ループのポリシー。`retry_interval`はtickの整数倍とそうでない値の両方、`timeout`は数tick分
    /// (ギブアップに頻繁に到達する)から十分長いものまで取る。
    fn policy_strategy() -> impl Strategy<Value = ReconnectPolicy> {
        (1u64..40, 1u64..4, 0u64..3, 0u64..10).prop_map(|(tick_ms, tpr, extra_ms, timeout_ticks)| {
            let tick = Duration::from_millis(tick_ms);
            ReconnectPolicy {
                tick,
                retry_interval: tick * tpr as u32 + Duration::from_millis(extra_ms),
                // 0は「最初のtickでギブアップ」の境界値。
                timeout: tick * timeout_ticks as u32,
            }
        })
    }

    fn clock_strategy() -> impl Strategy<Value = LoopClock> {
        (0u64..6, policy_strategy()).prop_map(|(ticks, armed)| LoopClock {
            elapsed: armed.tick * ticks as u32,
            tick_count: ticks,
            armed,
        })
    }

    /// ギブアップに到達しない(timeoutが十分長い)ポリシー。`every_tick_due`なら毎tickが`due`。
    fn long_policy(every_tick_due: bool) -> ReconnectPolicy {
        ReconnectPolicy {
            tick: Duration::from_millis(10),
            retry_interval: if every_tick_due { Duration::from_millis(10) } else { Duration::from_secs(3600) },
            timeout: Duration::from_secs(3600),
        }
    }

    /// `s`のループを「`LoopStarted`済みで動作中」にする(会計は0から、ポリシーは`policy`)。
    fn make_loop_live(s: &mut ReconnectState, policy: ReconnectPolicy) {
        s.reconnect_loop_active = true;
        s.loop_clock = Some(LoopClock { elapsed: Duration::ZERO, tick_count: 0, armed: policy });
    }

    /// 他の(未移行の)書き手が作りうる状態も含め、全フィールドの任意の組み合わせ。
    fn state_strategy() -> impl Strategy<Value = ReconnectState> {
        (
            phase_strategy(),
            0u64..4,
            any::<bool>(),
            any::<bool>(),
            any::<bool>(),
            any::<bool>(),
            background_strategy(),
            proptest::option::of(0u64..3),
            (proptest::option::of(0u64..4), proptest::option::of(0u64..4), proptest::option::of(clock_strategy())),
        )
            .prop_map(|(phase, epoch, loop_active, in_flight, pending_wake, user, bg, attempt, (edge, last, clock))| {
                // Step 8a′のedge不変条件(`edge_open`が`Some`なら`phase == Connected`で、
                // `last_established`はその世代)と、Step 3b/3cのループ不変条件(`loop_clock`が`Some`なら
                // `reconnect_loop_active`)を満たす範囲で任意に取る。
                let edge_open = if phase == ConnPhase::Connected { edge } else { None };
                ReconnectState {
                    phase: ReducerOwned(phase),
                    reconnect_epoch: epoch,
                    reconnect_loop_active: loop_active,
                    retry_attempt_in_flight: in_flight,
                    pending_wake,
                    user_initiated_disconnect: user,
                    background_state: bg,
                    last_attempt: attempt.map(AttemptRef),
                    edge_open,
                    last_established: edge_open.or(last),
                    loop_clock: if loop_active { clock } else { None },
                }
            })
    }

    /// epochは小さい範囲から取り、現行epochと非現行epochの両方が頻繁に出るようにする。
    fn event_strategy() -> impl Strategy<Value = ReconnectEvent> {
        prop_oneof![
            (0u64..4).prop_map(|generation| ReconnectEvent::AttemptConnected { generation }),
            (0u64..4, kind_strategy(), any::<bool>()).prop_map(|(generation, kind, targets_local_network)| {
                ReconnectEvent::AttemptDisconnected { generation, kind, targets_local_network }
            }),
            (0u64..4).prop_map(|new_generation| ReconnectEvent::SessionCreated { new_generation }),
            (0u64..6, policy_strategy()).prop_map(|(epoch, policy)| ReconnectEvent::LoopStarted { epoch, policy }),
            (0u64..6, policy_strategy()).prop_map(|(epoch, policy)| ReconnectEvent::ReconnectWake { epoch, policy }),
            (0u64..6, policy_strategy()).prop_map(|(epoch, policy)| ReconnectEvent::ReconnectTick { epoch, policy }),
            (0u64..6).prop_map(|epoch| ReconnectEvent::AttemptFailedSync { epoch }),
            (0u64..3).prop_map(|a| ReconnectEvent::ManualConnectStarted { attempt: AttemptRef(a) }),
            Just(ReconnectEvent::ReconnectSessionStarting),
            Just(ReconnectEvent::ForegroundReconnectFailedSync),
            Just(ReconnectEvent::CancelReconnect),
            Just(ReconnectEvent::UserDisconnect),
            Just(ReconnectEvent::ManualConnectFailedSync),
            (0u64..6).prop_map(|epoch| ReconnectEvent::LoopStartAborted { epoch }),
        ]
    }

    /// shellの`session_generation`のように世代が単調に進むEvent列(Step 8a′のedge契約用)。
    /// `NewSession`だけが`SessionCreated`で世代を進め、`Connected`/`Disconnected`は現行世代を運ぶ。
    #[derive(Debug, Clone)]
    enum ShellStep {
        NewSession,
        Connected,
        StaleConnected { back: u64 },
        Disconnected(DisconnectKind),
        Other(ReconnectEvent),
    }

    fn shell_step_strategy() -> impl Strategy<Value = ShellStep> {
        prop_oneof![
            Just(ShellStep::NewSession),
            Just(ShellStep::Connected),
            (1u64..3).prop_map(|back| ShellStep::StaleConnected { back }),
            kind_strategy().prop_map(ShellStep::Disconnected),
            event_strategy().prop_map(ShellStep::Other),
        ]
    }

    fn starts_attempt(effects: &[ReconnectEffect]) -> bool {
        effects.iter().any(|e| matches!(e, ReconnectEffect::StartAttempt { .. }))
    }

    // ── 網羅表テスト: DisconnectKind × was_connected(phase) × user_initiated ×
    //    reconnect_loop_active × pending_wake × last_attempt × targets_local_network ──

    #[test]
    fn attempt_disconnected_matches_legacy_decision_for_every_combination() {
        let kinds = [DisconnectKind::GracefulRemoteExit, DisconnectKind::NetworkLost, DisconnectKind::TransportError];
        let phases = [ConnPhase::Idle, ConnPhase::Connecting, ConnPhase::Connected];
        let bgs = [BackgroundState::Foreground, BackgroundState::Quiescing, BackgroundState::Suspended];
        let bools = [false, true];
        let mut cases = 0;
        for kind in kinds {
            for phase in phases {
                for user in bools {
                    for loop_active in bools {
                        for pending_wake in bools {
                            for in_flight in bools {
                                for has_attempt in bools {
                                    for targets_local in bools {
                                        for bg in bgs {
                                            let initial = ReconnectState {
                                                phase: ReducerOwned(phase),
                                                reconnect_epoch: 7,
                                                reconnect_loop_active: loop_active,
                                                retry_attempt_in_flight: in_flight,
                                                pending_wake,
                                                user_initiated_disconnect: user,
                                                background_state: bg,
                                                last_attempt: has_attempt.then(|| AttemptRef::next_after(None)),
                                                edge_open: None,
                                                last_established: None,
                                                loop_clock: None,
                                            };
                                            let mut legacy = initial.clone();
                                            let expected = legacy_handle_unexpected_disconnect(
                                                &mut legacy,
                                                &reason_for(kind),
                                                targets_local,
                                            );
                                            let mut actual = initial.clone();
                                            let effects = actual.apply(ReconnectEvent::AttemptDisconnected {
                                                generation: 1,
                                                kind,
                                                targets_local_network: targets_local,
                                            });
                                            assert_eq!(effect_to_legacy(&effects), expected, "initial={initial:?} kind={kind:?}");
                                            assert_eq!(actual, legacy, "initial={initial:?} kind={kind:?}");
                                            cases += 1;
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        assert_eq!(cases, 3 * 3 * 2 * 2 * 2 * 2 * 2 * 2 * 3);
    }

    #[test]
    fn network_lost_reason_classifies_as_network_lost() {
        assert_eq!(DisconnectKind::classify(&Some(NETWORK_LOST_REASON.to_string())), DisconnectKind::NetworkLost);
    }

    #[test]
    fn attempt_ref_is_monotonic() {
        let a = AttemptRef::next_after(None);
        let b = AttemptRef::next_after(Some(a));
        assert!(b > a);
    }

    proptest! {
        /// §2.2必須プロパティ: 非現行token(epoch)のタイマー(`LoopStarted`/`ReconnectTick`)・wake・
        /// 試行結果・ループ起動失敗はStateを変えず、Effectも返さない(ループtaskはそこで終了する)。
        #[test]
        fn stale_epoch_events_are_noops(
            initial in state_strategy(),
            offset in 1u64..5,
            policy in policy_strategy(),
            which in 0u8..5,
        ) {
            let stale = initial.reconnect_epoch.wrapping_add(offset);
            let ev = match which {
                0 => ReconnectEvent::ReconnectWake { epoch: stale, policy },
                1 => ReconnectEvent::ReconnectTick { epoch: stale, policy },
                2 => ReconnectEvent::LoopStarted { epoch: stale, policy },
                3 => ReconnectEvent::LoopStartAborted { epoch: stale },
                _ => ReconnectEvent::AttemptFailedSync { epoch: stale },
            };
            let mut s = initial.clone();
            let effects = s.apply(ev);
            prop_assert!(effects.is_empty());
            prop_assert_eq!(s, initial);
        }

        /// Step 3aレビューm5: 現行epochでも、ループが動作中でない(または`LoopStarted`前の)tick/wakeは
        /// staleと同じく何もしない。ループ非動作中に試行を始めることはない。
        #[test]
        fn tick_and_wake_without_a_live_loop_are_noops(
            initial in state_strategy(),
            policy in policy_strategy(),
            is_wake in any::<bool>(),
        ) {
            let mut initial = initial;
            if initial.reconnect_loop_active && initial.loop_clock.is_some() {
                initial.loop_clock = None;
            }
            let epoch = initial.reconnect_epoch;
            let ev = if is_wake {
                ReconnectEvent::ReconnectWake { epoch, policy }
            } else {
                ReconnectEvent::ReconnectTick { epoch, policy }
            };
            let mut s = initial.clone();
            prop_assert!(s.apply(ev).is_empty());
            prop_assert_eq!(s, initial);
        }

        /// 動作中のループへのwakeの遷移は旧ループ実装(`woke_early`分岐)と一致し、tick会計には触れず、
        /// 次のタイマーを`policy`の満額で設定し直す。
        #[test]
        fn wake_on_a_live_loop_matches_legacy(
            initial in state_strategy(),
            clock in clock_strategy(),
            policy in policy_strategy(),
        ) {
            let mut initial = initial;
            initial.reconnect_loop_active = true;
            initial.loop_clock = Some(clock);
            let epoch = initial.reconnect_epoch;

            let mut legacy = initial.clone();
            let legacy_started = legacy_wake(&mut legacy, epoch);
            let mut s = initial.clone();
            let effects = s.apply(ReconnectEvent::ReconnectWake { epoch, policy });
            prop_assert_eq!(starts_attempt(&effects), legacy_started);
            prop_assert_eq!(effects.first(), Some(&ReconnectEffect::LoopWokeEarly { epoch }));
            prop_assert_eq!(
                effects.last(),
                Some(&ReconnectEffect::ArmLoopTimer { epoch, after: policy.tick, elapsed_secs: clock.elapsed.as_secs() })
            );
            // 会計は進まず、次のタイマーのポリシーだけが変わる。
            legacy.loop_clock = Some(LoopClock { armed: policy, ..clock });
            prop_assert_eq!(&s, &legacy);
        }

        /// `AttemptDisconnected`の不変条件(ADR §6 Step 3a「得られるCI検証」)。
        #[test]
        fn attempt_disconnected_invariants(
            initial in state_strategy(),
            kind in kind_strategy(),
            targets_local_network in any::<bool>(),
            generation in 0u64..4,
        ) {
            let mut s = initial.clone();
            let effects = s.apply(ReconnectEvent::AttemptDisconnected { generation, kind, targets_local_network });

            // 4c02e605: Connectedから離脱する合流点では必ずdebounceを無効化する。
            prop_assert_eq!(effects.first(), Some(&ReconnectEffect::InvalidatePathObserver));
            prop_assert_eq!(s.phase, ConnPhase::Idle);
            prop_assert!(!s.retry_attempt_in_flight);
            prop_assert!(!s.user_initiated_disconnect);

            let started = effects.iter().any(|e| matches!(e, ReconnectEffect::StartReconnectLoop { .. }));
            let published = effects.iter().any(|e| matches!(e, ReconnectEffect::PublishDisconnected { .. }));
            // ループ動作中は二重起動せず、Disconnectedも連続通知しない。
            if initial.reconnect_loop_active {
                prop_assert!(!started && !published);
                prop_assert_eq!(s.reconnect_epoch, initial.reconnect_epoch);
                prop_assert!(s.reconnect_loop_active);
            }
            // リモートプロセスの正常終了・ユーザー切断・Connected未到達では再接続しない。
            if kind == DisconnectKind::GracefulRemoteExit
                || initial.user_initiated_disconnect
                || initial.phase != ConnPhase::Connected
            {
                prop_assert!(!started);
            }
            // ループを起動したなら、その epoch が新しい現行epochで、ループは動作中。
            for e in &effects {
                if let ReconnectEffect::StartReconnectLoop { attempt, epoch } = e {
                    prop_assert_eq!(*epoch, s.reconnect_epoch);
                    prop_assert!(s.reconnect_loop_active);
                    prop_assert_eq!(Some(*attempt), initial.last_attempt);
                }
                // Connected未到達の失敗だけにLocal Networkヒント。
                if let ReconnectEffect::PublishDisconnected { issue_hint: Some(_) } = e {
                    prop_assert!(initial.phase != ConnPhase::Connected);
                    prop_assert!(targets_local_network);
                }
            }
            // ループを起動しなかった切断では`pending_wake`を持ち越さない(ループ動作中を除く)。
            if !s.reconnect_loop_active {
                prop_assert!(!s.pending_wake);
            }
        }

        /// 任意のEvent列で: 試行中(`retry_attempt_in_flight`)には新しい試行を重ねない
        /// (ホスト鍵確認プロンプトの多重発生防止)。ループ動作中にはループを二重起動しない。
        /// Step 3b/3c: 試行の開始・タイマー設定は動作中の現行ループに対してだけ起き(Step 3aレビューm5)、
        /// `loop_clock`が`Some`なら必ず`reconnect_loop_active`。
        #[test]
        fn no_overlapping_attempts_or_loops(
            initial in state_strategy(),
            events in proptest::collection::vec(event_strategy(), 0..40),
        ) {
            let mut s = initial;
            for ev in events {
                let before = s.clone();
                let effects = s.apply(ev.clone());
                if before.retry_attempt_in_flight {
                    prop_assert!(!starts_attempt(&effects), "in-flight中に試行開始: {:?}", before);
                }
                if before.reconnect_loop_active {
                    prop_assert!(!effects.iter().any(|e| matches!(e, ReconnectEffect::StartReconnectLoop { .. })), "ループ動作中にループを二重起動した");
                }
                for e in &effects {
                    if let ReconnectEffect::StartAttempt { epoch, .. } = e {
                        prop_assert_eq!(*epoch, s.reconnect_epoch);
                        prop_assert!(s.retry_attempt_in_flight);
                        prop_assert!(s.reconnect_loop_active, "ループ非動作中に試行を開始した: {:?}", ev);
                    }
                    if let ReconnectEffect::ArmLoopTimer { epoch, .. } = e {
                        prop_assert_eq!(*epoch, s.reconnect_epoch);
                        prop_assert!(s.reconnect_loop_active && s.loop_clock.is_some(), "動作中でないループのタイマーを設定した: {:?}", ev);
                    }
                }
                if s.loop_clock.is_some() {
                    prop_assert!(s.reconnect_loop_active, "loop_clockが残ったままループが止まった: {:?}", ev);
                }
            }
        }

        /// rev6(eecba351): ループ動作中に届いた現行epochの`ReconnectWake`は、試行中でも失われない。
        /// 試行中に追加のwake/tickがいくつ挟まっても、その後の試行結果
        /// (`AttemptDisconnected`/`AttemptFailedSync`)のapplyか次の`ReconnectTick`のapplyで、
        /// 必ず試行開始かwake通知のEffectになる(ギブアップと区別するためtimeoutは十分長くする)。
        #[test]
        fn wake_during_in_flight_attempt_is_never_lost(
            initial in state_strategy(),
            noise in proptest::collection::vec(any::<bool>(), 0..6),
            result_is_disconnect in any::<bool>(),
            kind in kind_strategy(),
            targets_local_network in any::<bool>(),
            every_tick_due in any::<bool>(),
        ) {
            let policy = long_policy(every_tick_due);
            let mut s = initial;
            make_loop_live(&mut s, policy);
            let epoch = s.reconnect_epoch;

            let effects = s.apply(ReconnectEvent::ReconnectWake { epoch, policy });
            if starts_attempt(&effects) {
                return Ok(()); // 試行中でなかったので即座に試行開始した(wakeは消化済み)。
            }
            prop_assert!(effects.contains(&ReconnectEffect::PendingWakeRecorded { epoch }), "試行中のwakeが記録されなかった: {:?}", effects);

            // 試行結果が来るまでの間のwake/tick(試行中なので試行は始まらない)。
            for is_wake in noise {
                let ev = if is_wake {
                    ReconnectEvent::ReconnectWake { epoch, policy }
                } else {
                    ReconnectEvent::ReconnectTick { epoch, policy }
                };
                prop_assert!(!starts_attempt(&s.apply(ev)));
            }

            let result = if result_is_disconnect {
                ReconnectEvent::AttemptDisconnected { generation: 0, kind, targets_local_network }
            } else {
                ReconnectEvent::AttemptFailedSync { epoch }
            };
            let effects = s.apply(result);
            if effects.contains(&ReconnectEffect::WakeReconnectLoop) {
                // shellがループを起こすと`ReconnectWake`が届き、試行が始まる。
                prop_assert!(starts_attempt(&s.apply(ReconnectEvent::ReconnectWake { epoch, policy })), "wake通知後のReconnectWakeで試行が始まらなかった");
            } else {
                prop_assert!(!result_is_disconnect, "試行結果の切断でwakeが通知されなかった");
                prop_assert!(starts_attempt(&s.apply(ReconnectEvent::ReconnectTick { epoch, policy })), "同期失敗後のtickで保持中のwakeが試行にならなかった");
            }
        }

        /// ADR §4.2(有界到達性): ループ動作中の任意の状態から、試行結果の観測+(dueな)tickで
        /// 必ず試行開始に到達し、試行成功で必ず`Connected`を公開する。
        #[test]
        fn loop_reaches_attempt_and_connected(initial in state_strategy(), generation in 0u64..4) {
            let policy = long_policy(true);
            let mut s = initial;
            make_loop_live(&mut s, policy);
            let epoch = s.reconnect_epoch;
            s.apply(ReconnectEvent::AttemptFailedSync { epoch });
            prop_assert!(starts_attempt(&s.apply(ReconnectEvent::ReconnectTick { epoch, policy })), "試行結果の観測+tickで試行開始に到達しなかった");
            prop_assert_eq!(
                without_edges(&s.apply(ReconnectEvent::AttemptConnected { generation })),
                vec![ReconnectEffect::PublishConnected]
            );
            prop_assert_eq!(s.phase, ConnPhase::Connected);
            prop_assert!(!s.reconnect_loop_active && !s.retry_attempt_in_flight && !s.pending_wake);
            prop_assert_eq!(s.loop_clock, None);
            prop_assert_ne!(s.reconnect_epoch, epoch, "成功で旧ループのepochは無効化される");
        }

        /// `SessionCreated`はedge以外のStateを変えず、別世代のedgeが開いているときだけ`Lost(old)`を出す。
        #[test]
        fn session_created_only_closes_a_stale_edge(initial in state_strategy(), g in 0u64..6) {
            let mut s = initial.clone();
            let effects = s.apply(ReconnectEvent::SessionCreated { new_generation: g });
            match initial.edge_open {
                Some(old) if old != g => {
                    prop_assert_eq!(effects, vec![ReconnectEffect::EdgeLost { generation: old }]);
                    prop_assert_eq!(s.edge_open, None);
                }
                _ => {
                    prop_assert!(effects.is_empty());
                    prop_assert_eq!(s.edge_open, initial.edge_open);
                }
            }
            s.edge_open = initial.edge_open;
            prop_assert_eq!(s, initial);
        }

        /// Step 8a′ 不変条件(2): applyの前後で`phase`がConnectedから離れ、apply前に`edge_open`が
        /// `Some(g)`だったなら、そのapplyの出力に`Lost(g)`が含まれ、edgeは閉じる。逆に`Lost`を
        /// 出すのは開いていたedgeの世代だけで、edgeが開いている間は常に`phase == Connected`。
        #[test]
        fn leaving_connected_with_an_open_edge_emits_lost_in_the_same_apply(
            initial in state_strategy(),
            events in proptest::collection::vec(event_strategy(), 0..40),
        ) {
            let mut s = initial;
            for ev in events {
                let before = s.clone();
                let effects = s.apply(ev.clone());
                let lost: Vec<u64> = effects
                    .iter()
                    .filter_map(|e| match e { ReconnectEffect::EdgeLost { generation } => Some(*generation), _ => None })
                    .collect();
                if let Some(g) = before.edge_open {
                    if s.phase != ConnPhase::Connected {
                        prop_assert_eq!(&lost, &vec![g], "Connectedを離れたapplyでLost({})が出なかった: {:?}", g, ev);
                    }
                }
                prop_assert!(lost.len() <= 1);
                for g in &lost {
                    prop_assert_eq!(before.edge_open, Some(*g), "開いていないedgeのLostを出した");
                }
                if s.edge_open.is_some() {
                    prop_assert_eq!(s.phase, ConnPhase::Connected, "edgeが開いたままConnected以外になった");
                }
            }
        }

        /// Step 8a′ 不変条件(1)(ADR round 2 N-4): shellと同じく世代が単調に進む任意のEvent列で、
        /// 出力されたエッジ列は「各`g`について`Established(g)`は高々1回、その後
        /// `Established(g'>g)`より前に`Lost(g)`が正確に1回」。最後に切断で終われば全edgeが閉じる。
        #[test]
        fn every_established_is_followed_by_exactly_one_lost(
            steps in proptest::collection::vec(shell_step_strategy(), 0..60),
            final_kind in kind_strategy(),
        ) {
            let mut s = ReconnectState::default();
            let mut generation = 0u64;
            let mut edges: Vec<ReconnectEffect> = Vec::new();
            for step in steps {
                let ev = match step {
                    ShellStep::NewSession => {
                        generation += 1;
                        ReconnectEvent::SessionCreated { new_generation: generation }
                    }
                    ShellStep::Connected => ReconnectEvent::AttemptConnected { generation },
                    // shellは`is_current()`で古い世代を捨てるが、reducerはそれに依存しない。
                    ShellStep::StaleConnected { back } => {
                        ReconnectEvent::AttemptConnected { generation: generation.saturating_sub(back) }
                    }
                    ShellStep::Disconnected(kind) => {
                        ReconnectEvent::AttemptDisconnected { generation, kind, targets_local_network: false }
                    }
                    ShellStep::Other(ev) => ev,
                };
                edges.extend(s.apply(ev).into_iter().filter(is_edge));
            }
            edges.extend(
                s.apply(ReconnectEvent::AttemptDisconnected { generation, kind: final_kind, targets_local_network: false })
                    .into_iter()
                    .filter(is_edge),
            );

            let mut open: Option<u64> = None;
            let mut established: Vec<u64> = Vec::new();
            for e in &edges {
                match e {
                    ReconnectEffect::EdgeEstablished { generation: g } => {
                        prop_assert_eq!(open, None, "Lostより前に次のEstablished({})が出た: {:?}", g, edges);
                        prop_assert!(!matches!(established.last(), Some(last) if g <= last),"Establishedの世代が単調増加でない: {:?}", edges);
                        established.push(*g);
                        open = Some(*g);
                    }
                    ReconnectEffect::EdgeLost { generation: g } => {
                        prop_assert_eq!(open, Some(*g), "対応するEstablishedの無いLost({}): {:?}", g, edges);
                        open = None;
                    }
                    _ => unreachable!(),
                }
            }
            prop_assert_eq!(open, None, "切断で終わったのにedgeが開いたまま: {:?}", edges);
            prop_assert_eq!(s.edge_open, None);
        }

        /// 同一世代の`AttemptConnected`重複は`Established`を出し直さない(round 3 m-R3-2)。
        #[test]
        fn duplicate_attempt_connected_does_not_reemit_established(initial in state_strategy(), g in 0u64..6) {
            let mut s = initial;
            s.apply(ReconnectEvent::AttemptConnected { generation: g });
            let again = s.apply(ReconnectEvent::AttemptConnected { generation: g });
            prop_assert!(!again.iter().any(is_edge), "重複したAttemptConnectedでエッジが出た: {:?}", again);
        }

        /// `ManualConnectStarted`: `Connecting`中は拒否してStateを変えない。それ以外では旧`begin_connect`と
        /// 同じフィールドを書き、`Connecting`を公開する(Connectedからなら同じapplyで`Lost(old)`)。
        #[test]
        fn manual_connect_started_matches_legacy_begin_connect(initial in state_strategy(), a in 0u64..3) {
            let attempt = AttemptRef(a);
            let mut s = initial.clone();
            let effects = s.apply(ReconnectEvent::ManualConnectStarted { attempt });
            if initial.phase == ConnPhase::Connecting {
                prop_assert_eq!(effects, vec![ReconnectEffect::ManualConnectRejected]);
                prop_assert_eq!(s, initial);
                return Ok(());
            }
            let mut legacy = initial.clone();
            legacy.last_attempt = Some(attempt);
            legacy.phase = ReducerOwned(ConnPhase::Connecting);
            legacy.user_initiated_disconnect = false;
            legacy.reconnect_epoch = legacy.reconnect_epoch.wrapping_add(1);
            legacy.reconnect_loop_active = false;
            legacy.retry_attempt_in_flight = false;
            legacy.pending_wake = false;
            legacy.loop_clock = None;
            legacy.background_state = BackgroundState::Foreground;
            legacy.edge_open = None;
            prop_assert_eq!(&s, &legacy);
            let mut expected: Vec<ReconnectEffect> =
                initial.edge_open.map(|generation| ReconnectEffect::EdgeLost { generation }).into_iter().collect();
            expected.push(ReconnectEffect::InvalidatePathObserver);
            expected.push(ReconnectEffect::PublishConnecting);
            prop_assert_eq!(effects, expected);
        }

        /// Step 3b/3c: tick会計・`woke_early`分岐・ギブアップ・`due`の計算をreducerへ移しても、旧ループと
        /// 同じ`Reconnecting`/ギブアップ/試行開始/`pending_wake`の列を出し、同じタイミングで終了する。
        /// ループtaskの起床(満了/早期・途中のポリシー変更)と、外から届く試行結果・中止・成功を任意に
        /// インターリーブする。違いはギブアップでepochを進めること(m5、このテストの射影の外)だけ。
        #[test]
        fn loop_matches_legacy_loop_model(
            policy in policy_strategy(),
            steps in proptest::collection::vec(loop_step_strategy(), 0..60),
        ) {
            let mut s = ReconnectState::for_test(ConnPhase::Connected, Some(AttemptRef(0)));
            let epoch = start_loop_from_connected(&mut s);
            let mut legacy_s = s.clone();

            let effects = s.apply(ReconnectEvent::LoopStarted { epoch, policy });
            let (mut legacy, out) = LegacyLoop::start(&legacy_s, epoch, policy);
            prop_assert_eq!(loop_outputs(&effects), out);
            let mut task_running = arms_timer(&effects);
            prop_assert_eq!(task_running, !legacy.exited);

            for step in steps {
                match step {
                    LoopStep::Woke { early, next } => {
                        if !task_running {
                            continue; // ループtaskは終了済み(shellはもうEventを送らない)。
                        }
                        let ev = if early {
                            ReconnectEvent::ReconnectWake { epoch, policy: next }
                        } else {
                            ReconnectEvent::ReconnectTick { epoch, policy: next }
                        };
                        let effects = s.apply(ev);
                        let out = legacy.step(&mut legacy_s, early, next);
                        prop_assert_eq!(loop_outputs(&effects), out);
                        task_running = arms_timer(&effects);
                        prop_assert_eq!(task_running, !legacy.exited, "ループの終了タイミングが旧実装と違う: {:?}", effects);
                    }
                    LoopStep::FailedSync => {
                        s.apply(ReconnectEvent::AttemptFailedSync { epoch });
                        if legacy_s.reconnect_epoch == epoch {
                            legacy_s.retry_attempt_in_flight = false;
                        }
                    }
                    LoopStep::Disconnected(kind) => {
                        let effects = s.apply(ReconnectEvent::AttemptDisconnected { generation: 0, kind, targets_local_network: false });
                        let expected = legacy_handle_unexpected_disconnect(&mut legacy_s, &reason_for(kind), false);
                        // 新しいループのepochの値はギブアップでのepoch進行(m5)の分だけ旧実装とずれうるので比べない。
                        let normalize = |a: LegacyAction| match a {
                            LegacyAction::StartLoop(_) => LegacyAction::StartLoop(0),
                            other => other,
                        };
                        prop_assert_eq!(normalize(effect_to_legacy(&effects)), normalize(expected));
                    }
                    LoopStep::Cancel => {
                        let effects = s.apply(ReconnectEvent::CancelReconnect);
                        let published = legacy_cancel(&mut legacy_s);
                        prop_assert_eq!(effects.contains(&ReconnectEffect::PublishDisconnected { issue_hint: None }), published);
                    }
                    LoopStep::Connected => {
                        s.apply(ReconnectEvent::AttemptConnected { generation: 1 });
                        legacy_s.phase = ReducerOwned(ConnPhase::Connected);
                        legacy_s.reconnect_epoch = legacy_s.reconnect_epoch.wrapping_add(1);
                        legacy_s.reconnect_loop_active = false;
                        legacy_s.retry_attempt_in_flight = false;
                        legacy_s.pending_wake = false;
                    }
                }
                prop_assert_eq!(projection(&s), projection(&legacy_s));
                // 切断で2本目のループが起動したら、それはこのテストの範囲外(ループは1本)。
                if s.reconnect_loop_active && s.reconnect_epoch != epoch {
                    return Ok(());
                }
            }
        }

        /// ギブアップの判断(Step 3b/3c): 満了したtickの`after`の和が`timeout`に達した最初のtick
        /// (= max(1, ceil(timeout / tick))回目)でちょうどギブアップし、それより前には決してしない。
        /// 早期起床(wake)はいくつ挟まっても会計に数えない。ギブアップはepochを進め(m5)、以後その
        /// epochのtick/wake/試行結果/`LoopStarted`/`LoopStartAborted`は何もしない。
        #[test]
        fn give_up_exactly_when_elapsed_reaches_timeout_and_bumps_the_epoch(
            policy in policy_strategy(),
            wakes in proptest::collection::vec(0usize..3, 20),
            in_flight_results in proptest::collection::vec(any::<bool>(), 20),
        ) {
            let mut s = ReconnectState::for_test(ConnPhase::Connected, Some(AttemptRef(0)));
            let epoch = start_loop_from_connected(&mut s);
            prop_assert!(arms_timer(&s.apply(ReconnectEvent::LoopStarted { epoch, policy })), "LoopStartedで最初のタイマーが設定されなかった");
            // 同じepochの2回目の`LoopStarted`(=2本目のtask)は何もしない。
            let before = s.clone();
            prop_assert!(s.apply(ReconnectEvent::LoopStarted { epoch, policy }).is_empty(), "同じepochの2回目のLoopStartedがEffectを返した");
            prop_assert_eq!(&s, &before);

            let tick_ns = policy.tick.as_nanos();
            let expected_ticks = policy.timeout.as_nanos().div_ceil(tick_ns).max(1) as usize;
            let mut ticks = 0usize;
            let mut gave_up = false;
            for i in 0..20 {
                for _ in 0..wakes[i] {
                    let effects = s.apply(ReconnectEvent::ReconnectWake { epoch, policy });
                    prop_assert!(arms_timer(&effects), "wakeでループが止まった: {:?}", effects);
                }
                if in_flight_results[i] {
                    // 試行結果が届いて試行中が解除される(試行の有無はギブアップ判断に影響しない)。
                    s.apply(ReconnectEvent::AttemptFailedSync { epoch });
                }
                let effects = s.apply(ReconnectEvent::ReconnectTick { epoch, policy });
                ticks += 1;
                let timed_out = effects.contains(&ReconnectEffect::PublishReconnectTimedOut { timeout_secs: policy.timeout_secs() });
                if ticks < expected_ticks {
                    prop_assert!(!timed_out, "{}回目のtickで早すぎるギブアップ(期待{}回目)", ticks, expected_ticks);
                    prop_assert!(arms_timer(&effects), "ギブアップ前にタイマーが止まった");
                    let expected_elapsed = (policy.tick * ticks as u32).as_secs() as u32;
                    prop_assert!(
                        effects.contains(&ReconnectEffect::PublishReconnecting { elapsed_secs: expected_elapsed, timeout_secs: policy.timeout_secs() }),
                        "Reconnectingの経過秒が会計と合わない: {:?}", effects
                    );
                } else {
                    prop_assert_eq!(ticks, expected_ticks);
                    prop_assert!(timed_out, "{}回目のtickでギブアップしなかった: {:?}", ticks, effects);
                    prop_assert!(!arms_timer(&effects) && !starts_attempt(&effects), "ギブアップと同時にタイマー/試行を出した: {:?}", effects);
                    gave_up = true;
                    break;
                }
            }
            prop_assert!(gave_up);

            prop_assert_ne!(s.reconnect_epoch, epoch, "ギブアップでepochが進まなかった");
            prop_assert!(!s.reconnect_loop_active && !s.retry_attempt_in_flight && !s.pending_wake);
            prop_assert_eq!(s.loop_clock, None);
            let after = s.clone();
            for ev in [
                ReconnectEvent::ReconnectTick { epoch, policy },
                ReconnectEvent::ReconnectWake { epoch, policy },
                ReconnectEvent::AttemptFailedSync { epoch },
                ReconnectEvent::LoopStarted { epoch, policy },
                ReconnectEvent::LoopStartAborted { epoch },
            ] {
                prop_assert!(s.apply(ev.clone()).is_empty(), "ギブアップ後の旧epochのEventがEffectを返した: {:?}", ev);
                prop_assert_eq!(&s, &after);
            }
        }

        /// `CancelReconnect`は旧`cancel_reconnect`と同じフィールドを書き(epochを進める)、ループが
        /// 動作中だったときだけ`Disconnected`を公開する。tick会計も捨てる。
        /// RC-04: ループが動作中だったときは、進行中だったかもしれない試行を中断する(phaseを`Idle`へ戻し、
        /// `AbortInFlightAttempt`を`Disconnected`の公開より前に出す)。
        #[test]
        fn cancel_reconnect_matches_legacy(initial in state_strategy()) {
            let mut legacy = initial.clone();
            let published = legacy_cancel(&mut legacy);
            legacy.loop_clock = None;
            let mut expected = Vec::new();
            if published {
                if let Some(generation) = legacy.edge_open.take() {
                    expected.push(ReconnectEffect::EdgeLost { generation });
                }
                legacy.phase = ReducerOwned(ConnPhase::Idle);
                legacy.background_state = BackgroundState::Foreground;
                expected.push(ReconnectEffect::AbortInFlightAttempt);
                expected.push(ReconnectEffect::PublishDisconnected { issue_hint: None });
            }
            let mut s = initial.clone();
            let effects = s.apply(ReconnectEvent::CancelReconnect);
            prop_assert_eq!(&s, &legacy);
            prop_assert_eq!(effects, expected);
        }

        /// RC-03: `UserDisconnect`は、ループ動作中ならループを止めて進行中の試行を中断し、Rust側から
        /// `Disconnected`を公開する(この遷移の後、ループはもう試行を始めない)。ループ非動作中は
        /// `user_initiated_disconnect`を立ててセッションの切断だけを依頼し、他のフィールドは変えない。
        #[test]
        fn user_disconnect_stops_a_live_loop_or_marks_the_disconnect_as_user_initiated(
            initial in state_strategy(),
            policy in policy_strategy(),
        ) {
            let mut s = initial.clone();
            let effects = s.apply(ReconnectEvent::UserDisconnect);
            if initial.reconnect_loop_active {
                prop_assert!(!s.reconnect_loop_active && !s.retry_attempt_in_flight && !s.pending_wake);
                prop_assert_eq!(s.loop_clock, None);
                prop_assert_ne!(s.reconnect_epoch, initial.reconnect_epoch, "ループのepochが無効化されなかった");
                prop_assert_eq!(s.phase, ConnPhase::Idle);
                prop_assert!(!s.user_initiated_disconnect);
                prop_assert_eq!(s.background_state, BackgroundState::Foreground);
                let abort = effects.iter().position(|e| *e == ReconnectEffect::AbortInFlightAttempt);
                let publish = effects.iter().position(|e| matches!(e, ReconnectEffect::PublishDisconnected { .. }));
                prop_assert!(abort.is_some() && publish.is_some() && abort < publish, "試行の中断がDisconnectedより前に出ない: {:?}", effects);
                // 旧ループのtick/wakeはもう何もしない。
                let after = s.clone();
                for ev in [
                    ReconnectEvent::ReconnectTick { epoch: initial.reconnect_epoch, policy },
                    ReconnectEvent::ReconnectWake { epoch: initial.reconnect_epoch, policy },
                ] {
                    prop_assert!(s.apply(ev).is_empty(), "ユーザー切断の後に旧ループのEventがEffectを返した");
                    prop_assert_eq!(&s, &after);
                }
            } else {
                prop_assert_eq!(effects, vec![ReconnectEffect::DisconnectSession]);
                let mut expected = initial.clone();
                expected.user_initiated_disconnect = true;
                prop_assert_eq!(s, expected);
            }
        }

        /// RC-04: 動作中のループが試行中(phase `Connecting`)のとき、ループを止める遷移(中止・ユーザー切断・
        /// ギブアップ)はどれもphaseを`Idle`へ戻し、試行の中断を依頼する。
        #[test]
        fn stopping_a_loop_with_an_attempt_in_flight_aborts_the_attempt(
            initial in state_strategy(),
            which in 0u8..3,
        ) {
            let policy = ReconnectPolicy {
                tick: Duration::from_millis(10),
                retry_interval: Duration::from_millis(10),
                timeout: Duration::from_millis(10),
            };
            let mut s = initial;
            s.apply(ReconnectEvent::ReconnectSessionStarting);
            make_loop_live(&mut s, policy);
            s.retry_attempt_in_flight = true;
            let epoch = s.reconnect_epoch;
            let ev = match which {
                0 => ReconnectEvent::CancelReconnect,
                1 => ReconnectEvent::UserDisconnect,
                // timeout == tickなので最初のtickでギブアップする。
                _ => ReconnectEvent::ReconnectTick { epoch, policy },
            };
            let effects = s.apply(ev.clone());
            prop_assert_eq!(s.phase, ConnPhase::Idle, "ループを止めた後もConnectingのまま: {:?}", ev);
            prop_assert!(effects.contains(&ReconnectEffect::AbortInFlightAttempt), "試行の中断を依頼しなかった: {:?} -> {:?}", ev, effects);
            prop_assert!(!s.reconnect_loop_active);
        }

        /// RC-29: 手動接続・再接続ループの試行の同期失敗はphaseを`Connecting`から`Idle`へ戻すので、
        /// 次の手動接続が二重start防止に拒否され続けない。
        #[test]
        fn sync_connect_failures_do_not_leave_the_phase_stuck_in_connecting(initial in state_strategy(), a in 0u64..3) {
            let mut s = initial.clone();
            s.apply(ReconnectEvent::ManualConnectFailedSync);
            if initial.phase == ConnPhase::Connecting {
                prop_assert_eq!(s.phase, ConnPhase::Idle);
            } else {
                prop_assert_eq!(&s, &initial);
            }
            let retried = s.apply(ReconnectEvent::ManualConnectStarted { attempt: AttemptRef(a) });
            prop_assert!(!retried.contains(&ReconnectEffect::ManualConnectRejected), "同期失敗の後の手動接続が拒否された: {:?}", retried);

            let mut s = initial.clone();
            s.apply(ReconnectEvent::ReconnectSessionStarting);
            let epoch = s.reconnect_epoch;
            s.apply(ReconnectEvent::AttemptFailedSync { epoch });
            prop_assert_eq!(s.phase, ConnPhase::Idle);
            prop_assert!(!s.retry_attempt_in_flight);
        }
    }

    // ── Step 11: Effect列の不変条件検査(`trace_invariants`) ──

    /// reducerのEffectを、shellが公開するcallbackと同じ形の[`TraceEvent`]へ写す(状態公開・接続エッジ)。
    /// `PublishReconnectTimedOut`はshellが`Disconnected`として公開する。
    fn effect_trace(effect: &ReconnectEffect) -> Option<crate::trace_invariants::TraceEvent> {
        use crate::trace_invariants::{StateTag, TraceEvent};
        match effect {
            ReconnectEffect::PublishDisconnected { .. } | ReconnectEffect::PublishReconnectTimedOut { .. } => {
                Some(TraceEvent::State(StateTag::Disconnected))
            }
            ReconnectEffect::PublishConnected => Some(TraceEvent::State(StateTag::Connected)),
            ReconnectEffect::PublishConnecting => Some(TraceEvent::State(StateTag::Connecting)),
            ReconnectEffect::PublishReconnecting { .. } => Some(TraceEvent::State(StateTag::Reconnecting)),
            ReconnectEffect::EdgeEstablished { generation } => Some(TraceEvent::Established(*generation)),
            ReconnectEffect::EdgeLost { generation } => Some(TraceEvent::Lost(*generation)),
            ReconnectEffect::InvalidatePathObserver
            | ReconnectEffect::WakeReconnectLoop
            | ReconnectEffect::StartReconnectLoop { .. }
            | ReconnectEffect::ManualConnectRejected
            | ReconnectEffect::StartAttempt { .. }
            | ReconnectEffect::PendingWakeRecorded { .. }
            | ReconnectEffect::ArmLoopTimer { .. }
            | ReconnectEffect::LoopWokeEarly { .. }
            | ReconnectEffect::LoopTicked { .. }
            | ReconnectEffect::AbortInFlightAttempt
            | ReconnectEffect::DisconnectSession => None,
        }
    }

    proptest! {
        /// Step 11: shellと同じく世代が単調に進む任意のEvent列で、reducerのEffect列が
        /// `trace_invariants::check_trace`の不変条件を満たす: エッジ契約(T1/T2)、edgeが開いている間に
        /// `Reconnecting`を公開しないこと(状態公開の単調性)、非現行epochのtick/wake(ループタイマーの
        /// token)がEffectを生まないこと。
        #[test]
        fn effect_trace_satisfies_step11_invariants(
            steps in proptest::collection::vec(shell_step_strategy(), 0..80),
        ) {
            use crate::trace_invariants::{check_trace, TraceEvent};
            let mut s = ReconnectState::default();
            let mut generation = 0u64;
            let mut trace: Vec<TraceEvent> = Vec::new();
            for step in steps {
                let ev = match step {
                    ShellStep::NewSession => {
                        generation += 1;
                        ReconnectEvent::SessionCreated { new_generation: generation }
                    }
                    ShellStep::Connected => ReconnectEvent::AttemptConnected { generation },
                    ShellStep::StaleConnected { back } => {
                        ReconnectEvent::AttemptConnected { generation: generation.saturating_sub(back) }
                    }
                    ShellStep::Disconnected(kind) => {
                        ReconnectEvent::AttemptDisconnected { generation, kind, targets_local_network: false }
                    }
                    ShellStep::Other(ev) => ev,
                };
                let timer_token = match &ev {
                    ReconnectEvent::ReconnectTick { epoch, .. } | ReconnectEvent::ReconnectWake { epoch, .. } => Some(*epoch),
                    _ => None,
                };
                let current = s.reconnect_epoch;
                let effects = s.apply(ev);
                if let Some(token) = timer_token {
                    trace.push(TraceEvent::TimerFired { token, current, effects: effects.len() });
                }
                trace.extend(effects.iter().filter_map(effect_trace));
            }
            if let Err(violation) = check_trace(&trace) {
                prop_assert!(false, "Step 11 trace invariant violated: {} / trace: {:?}", violation, trace);
            }
        }
    }
}
