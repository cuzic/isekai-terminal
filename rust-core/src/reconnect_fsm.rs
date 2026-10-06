//! `SessionOrchestrator`の自動再接続に関する判断を、時計・ロック・spawn・コールバックから
//! 切り離した純粋reducer(ADR_FUNCTIONAL_CORE_EFFECTS.md §2.1 / §6 Step 3a)。
//!
//! ## 集約(`ReconnectState`)
//!
//! `OrchestratorState`(`orchestrator.rs`)は`reconnect: ReconnectState`を**フィールドとして**
//! 持つ。別ストアではなく、所有者は常に`OrchestratorState`1つ(ADR §4.3、`rust-ssot.md`)。
//! Step 3aの時点では集約の**定義**は全体だが、`apply`経由に移行した遷移は下表の
//! 「`apply`経由か」が「はい」のものだけで、それ以外の書き手(`cancel_reconnect`・
//! バックグラウンド系・ループのタイムアウト等)は従来どおり`pub(crate)`フィールドを直接書く。
//! ただし**`phase`の書き手はStep 8a′ですべて`apply`経由**になった(下の「接続エッジ」参照)。
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
//! | 再接続ループのネットワークwake | [`ReconnectEvent::ReconnectWake`] | はい(rev6: `pending_wake`) |
//! | 再接続ループのtick | [`ReconnectEvent::ReconnectTick`] | はい(`due`はshellが計算。tick会計は3b/3c) |
//! | 再接続ループの同期エラー | [`ReconnectEvent::AttemptFailedSync`] | はい |
//! | `begin_connect` | [`ReconnectEvent::ManualConnectStarted`] | はい(8a′、→Connecting) |
//! | `disconnect` | `UserDisconnect` | いいえ |
//! | `cancel_reconnect` | `CancelReconnect` | いいえ |
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
//! ## タイマー(ADR §2.2-1)
//!
//! 再接続ループのtickは`reconnect_epoch`をtokenとするタイマーである:
//! [`ReconnectEffect::StartReconnectLoop`]`{epoch}`がタイマーの起動(shellがtokio taskを
//! spawnし、tickごとに[`ReconnectEvent::ReconnectTick`]`{epoch}`を戻す)にあたり、
//! 非現行epochの`ReconnectTick`/`ReconnectWake`/`AttemptFailedSync`はStateを変えず
//! Effectも返さない(proptestで検証)。
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
// 純粋モジュール(`pure_modules.toml`登録、ADR_FUNCTIONAL_CORE_EFFECTS.md §2.3)。
// 時計・RNG・ロック・I/O型の直接使用を`clippy.toml`の`disallowed-*`で禁止する。
#![deny(clippy::disallowed_methods, clippy::disallowed_types)]

use crate::ConnectionIssueHint;

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
    pub(crate) phase: ConnPhase,
    /// 自動再接続ループ自身の生存確認用epoch(ループタイマーのtoken)。新しい`connect_*`呼び出し・
    /// `cancel_reconnect()`・再接続成功のいずれかでインクリメントされ、
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
    pub(crate) edge_open: Option<u64>,
    /// Step 8a′: 最後に`Established`を出した世代。これ以下の世代には`Established`を出し直さない
    /// (「各世代について`Established`は高々1回」を、遅延・重複した`AttemptConnected`に対しても守る)。
    pub(crate) last_established: Option<u64>,
}

impl Default for ReconnectState {
    fn default() -> Self {
        Self {
            phase: ConnPhase::Idle,
            reconnect_epoch: 0,
            reconnect_loop_active: false,
            retry_attempt_in_flight: false,
            pending_wake: false,
            user_initiated_disconnect: false,
            background_state: BackgroundState::Foreground,
            last_attempt: None,
            edge_open: None,
            last_established: None,
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
    /// 再接続ループ(`epoch`)がネットワーク復帰通知で早期に起床した。
    ReconnectWake { epoch: u64 },
    /// 再接続ループ(`epoch`)の通常のtick。`due`は「`retry_interval`に達したか」で、
    /// tick会計(`elapsed`/`tick_count`)はshellが計算する(3b/3cの範囲として再評価に残す)。
    ReconnectTick { epoch: u64, due: bool },
    /// 再接続ループ(`epoch`)が開始した試行が同期的に失敗した(`reconnect_attempt`が`Err`)。
    AttemptFailedSync { epoch: u64 },
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
            ReconnectEvent::ReconnectWake { epoch } => self.on_reconnect_wake(epoch),
            ReconnectEvent::ReconnectTick { epoch, due } => self.on_reconnect_tick(epoch, due),
            ReconnectEvent::AttemptFailedSync { epoch } => {
                if epoch == self.reconnect_epoch {
                    self.retry_attempt_in_flight = false;
                }
                Vec::new()
            }
        }
    }

    /// `phase`の唯一の書き手(Step 8a′)。Connected以外へ動かすとき、edgeが開いていれば
    /// 同じapplyの出力に`Lost(g)`を積んでedgeを閉じる(ADR round 3 R3-1の定義そのもの)。
    fn set_phase(&mut self, phase: ConnPhase, effects: &mut Vec<ReconnectEffect>) {
        if phase != ConnPhase::Connected {
            self.close_edge(effects);
        }
        self.phase = phase;
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
        self.reconnect_epoch = self.reconnect_epoch.wrapping_add(1);
        self.reconnect_loop_active = false;
        self.retry_attempt_in_flight = false;
        self.pending_wake = false;
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
        if self.phase == ConnPhase::Connecting {
            return vec![ReconnectEffect::ManualConnectRejected];
        }
        let mut effects = Vec::new();
        self.set_phase(ConnPhase::Connecting, &mut effects);
        self.last_attempt = Some(attempt);
        // 新しい手動接続が始まった以上、直前のdisconnect()由来のフラグや
        // 実行中だったかもしれない自動再接続ループは無関係になる。
        self.user_initiated_disconnect = false;
        self.reconnect_epoch = self.reconnect_epoch.wrapping_add(1);
        self.reconnect_loop_active = false;
        self.retry_attempt_in_flight = false;
        self.pending_wake = false;
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
        let was_connected = self.phase == ConnPhase::Connected;
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
    fn on_reconnect_wake(&mut self, epoch: u64) -> Vec<ReconnectEffect> {
        if epoch != self.reconnect_epoch {
            return Vec::new();
        }
        if !self.retry_attempt_in_flight {
            self.retry_attempt_in_flight = true;
            self.pending_wake = false;
            vec![ReconnectEffect::StartAttempt { epoch, source: AttemptSource::NetworkWake }]
        } else {
            self.pending_wake = true;
            vec![ReconnectEffect::PendingWakeRecorded { epoch }]
        }
    }

    /// 通常のtick: 試行中でなく、`retry_interval`に達したか保持中の`pending_wake`があれば
    /// 1回の試行を開始する。
    fn on_reconnect_tick(&mut self, epoch: u64, due: bool) -> Vec<ReconnectEffect> {
        if epoch != self.reconnect_epoch || self.retry_attempt_in_flight || !(due || self.pending_wake) {
            return Vec::new();
        }
        let source = if self.pending_wake { AttemptSource::PendingWake } else { AttemptSource::Tick };
        self.retry_attempt_in_flight = true;
        self.pending_wake = false;
        vec![ReconnectEffect::StartAttempt { epoch, source }]
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
        s.phase = ConnPhase::Idle;
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
            (proptest::option::of(0u64..4), proptest::option::of(0u64..4)),
        )
            .prop_map(|(phase, epoch, loop_active, in_flight, pending_wake, user, bg, attempt, (edge, last))| {
                // Step 8a′のedge不変条件(`edge_open`が`Some`なら`phase == Connected`で、
                // `last_established`はその世代)を満たす範囲で任意に取る。
                let edge_open = if phase == ConnPhase::Connected { edge } else { None };
                ReconnectState {
                    phase,
                    reconnect_epoch: epoch,
                    reconnect_loop_active: loop_active,
                    retry_attempt_in_flight: in_flight,
                    pending_wake,
                    user_initiated_disconnect: user,
                    background_state: bg,
                    last_attempt: attempt.map(AttemptRef),
                    edge_open,
                    last_established: edge_open.or(last),
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
            (0u64..6).prop_map(|epoch| ReconnectEvent::ReconnectWake { epoch }),
            (0u64..6, any::<bool>()).prop_map(|(epoch, due)| ReconnectEvent::ReconnectTick { epoch, due }),
            (0u64..6).prop_map(|epoch| ReconnectEvent::AttemptFailedSync { epoch }),
            (0u64..3).prop_map(|a| ReconnectEvent::ManualConnectStarted { attempt: AttemptRef(a) }),
            Just(ReconnectEvent::ReconnectSessionStarting),
            Just(ReconnectEvent::ForegroundReconnectFailedSync),
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
                                                phase,
                                                reconnect_epoch: 7,
                                                reconnect_loop_active: loop_active,
                                                retry_attempt_in_flight: in_flight,
                                                pending_wake,
                                                user_initiated_disconnect: user,
                                                background_state: bg,
                                                last_attempt: has_attempt.then(|| AttemptRef::next_after(None)),
                                                edge_open: None,
                                                last_established: None,
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
        /// §2.2必須プロパティ: 非現行token(epoch)のタイマー/wake/試行結果はStateを変えず、Effectも返さない。
        #[test]
        fn stale_epoch_events_are_noops(
            initial in state_strategy(),
            offset in 1u64..5,
            due in any::<bool>(),
            which in 0u8..3,
        ) {
            let stale = initial.reconnect_epoch.wrapping_add(offset);
            let ev = match which {
                0 => ReconnectEvent::ReconnectWake { epoch: stale },
                1 => ReconnectEvent::ReconnectTick { epoch: stale, due },
                _ => ReconnectEvent::AttemptFailedSync { epoch: stale },
            };
            let mut s = initial.clone();
            let effects = s.apply(ev);
            prop_assert!(effects.is_empty());
            prop_assert_eq!(s, initial);
        }

        /// wake/tickの遷移は旧ループ実装と一致する。
        #[test]
        fn wake_and_tick_match_legacy(initial in state_strategy(), epoch in 0u64..6, due in any::<bool>()) {
            let mut legacy = initial.clone();
            let legacy_started = legacy_wake(&mut legacy, epoch);
            let mut s = initial.clone();
            let effects = s.apply(ReconnectEvent::ReconnectWake { epoch });
            prop_assert_eq!(starts_attempt(&effects), legacy_started);
            prop_assert_eq!(&s, &legacy);

            let mut legacy = initial.clone();
            let legacy_started = legacy_tick(&mut legacy, epoch, due);
            let mut s = initial.clone();
            let effects = s.apply(ReconnectEvent::ReconnectTick { epoch, due });
            prop_assert_eq!(starts_attempt(&effects), legacy_started);
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
        #[test]
        fn no_overlapping_attempts_or_loops(
            initial in state_strategy(),
            events in proptest::collection::vec(event_strategy(), 0..40),
        ) {
            let mut s = initial;
            for ev in events {
                let before = s.clone();
                let effects = s.apply(ev);
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
                    }
                }
            }
        }

        /// rev6(eecba351): ループ動作中に届いた現行epochの`ReconnectWake`は、試行中でも失われない。
        /// 試行中に追加のwake/tickがいくつ挟まっても、その後の試行結果
        /// (`AttemptDisconnected`/`AttemptFailedSync`)のapplyか次の`ReconnectTick`のapplyで、
        /// 必ず試行開始かwake通知のEffectになる。
        #[test]
        fn wake_during_in_flight_attempt_is_never_lost(
            initial in state_strategy(),
            noise in proptest::collection::vec((any::<bool>(), any::<bool>()), 0..6),
            result_is_disconnect in any::<bool>(),
            kind in kind_strategy(),
            targets_local_network in any::<bool>(),
            due in any::<bool>(),
        ) {
            let mut s = initial;
            s.reconnect_loop_active = true;
            let epoch = s.reconnect_epoch;

            let effects = s.apply(ReconnectEvent::ReconnectWake { epoch });
            if starts_attempt(&effects) {
                return Ok(()); // 試行中でなかったので即座に試行開始した(wakeは消化済み)。
            }
            prop_assert_eq!(effects, vec![ReconnectEffect::PendingWakeRecorded { epoch }]);

            // 試行結果が来るまでの間のwake/tick(試行中なので試行は始まらない)。
            for (is_wake, tick_due) in noise {
                let ev = if is_wake {
                    ReconnectEvent::ReconnectWake { epoch }
                } else {
                    ReconnectEvent::ReconnectTick { epoch, due: tick_due }
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
                prop_assert!(starts_attempt(&s.apply(ReconnectEvent::ReconnectWake { epoch })), "wake通知後のReconnectWakeで試行が始まらなかった");
            } else {
                prop_assert!(!result_is_disconnect, "試行結果の切断でwakeが通知されなかった");
                prop_assert!(starts_attempt(&s.apply(ReconnectEvent::ReconnectTick { epoch, due })), "同期失敗後のtickで保持中のwakeが試行にならなかった");
            }
        }

        /// ADR §4.2(有界到達性): ループ動作中の任意の状態から、試行結果の観測+tickで
        /// 必ず試行開始に到達し、試行成功で必ず`Connected`を公開する。
        #[test]
        fn loop_reaches_attempt_and_connected(initial in state_strategy(), generation in 0u64..4) {
            let mut s = initial;
            s.reconnect_loop_active = true;
            let epoch = s.reconnect_epoch;
            s.apply(ReconnectEvent::AttemptFailedSync { epoch });
            prop_assert!(starts_attempt(&s.apply(ReconnectEvent::ReconnectTick { epoch, due: true })), "試行結果の観測+tickで試行開始に到達しなかった");
            prop_assert_eq!(
                without_edges(&s.apply(ReconnectEvent::AttemptConnected { generation })),
                vec![ReconnectEffect::PublishConnected]
            );
            prop_assert_eq!(s.phase, ConnPhase::Connected);
            prop_assert!(!s.reconnect_loop_active && !s.retry_attempt_in_flight && !s.pending_wake);
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
            legacy.phase = ConnPhase::Connecting;
            legacy.user_initiated_disconnect = false;
            legacy.reconnect_epoch = legacy.reconnect_epoch.wrapping_add(1);
            legacy.reconnect_loop_active = false;
            legacy.retry_attempt_in_flight = false;
            legacy.pending_wake = false;
            legacy.background_state = BackgroundState::Foreground;
            legacy.edge_open = None;
            prop_assert_eq!(&s, &legacy);
            let mut expected: Vec<ReconnectEffect> =
                initial.edge_open.map(|generation| ReconnectEffect::EdgeLost { generation }).into_iter().collect();
            expected.push(ReconnectEffect::InvalidatePathObserver);
            expected.push(ReconnectEffect::PublishConnecting);
            prop_assert_eq!(effects, expected);
        }
    }
}
