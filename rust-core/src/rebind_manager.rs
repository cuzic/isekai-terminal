//! WiFi⇔セルラーの物理マルチパス切替を統括する純粋な状態機械。
//!
//! カフェ等で WiFi のリンクは繋がったまま上流だけがサイレントに死ぬケースを
//! `isekai-transport::path_health` が検知して [`RebindEvent::NoViablePath`] を
//! 送ってきたら、即座にセルラーへ片方向フェイルオーバーする。その後 WiFi の
//! 上流が復活したら、疎通確認 → ヒステリシス(連続成功回数 + 安定時間 +
//! セルラー最小滞在) → 通信が静かなタイミングを待つ、という段階を経て
//! 自動的に WiFi へ復帰する。ユーザーが「今すぐ戻す」を要求した場合は
//! 疎通確認だけを残してこれらのヒステリシス/静けさ待ちをすべてバイパスする。
//!
//! [`trzsz::TrzszTransferFsm`](crate::trzsz::TrzszTransferFsm) と同じく
//! `timed_fsm::TimedStateMachine` に乗せてある: この型は `tokio` にも実際の
//! `noq::Endpoint`/fd にも一切触れない。タイマーは `Response` 経由で宣言的に
//! 返すだけで、実際に時間を計測して `on_timeout` を呼び戻すのは async 側の
//! Driver(`RebindTimerRuntime`、`session.rs` の `TokioTimerRuntime` と同じ
//! パターン)の役目。疎通確認・実際の rebind 実行・fd 取得などの実I/Oも
//! すべて Driver 側が持つ trait 実装(`WifiProbeExecutor`/`RebindExecutor`/
//! `PlatformFdSource`)を介して行われ、この型自身は一切呼び出さない。

use std::time::Duration;
use timed_fsm::{Response, TimedStateMachine};

// ── FSM の入出力型 ───────────────────────────────────────

/// [`RebindManager`] への入力イベント。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RebindEvent {
    /// `isekai-transport::path_health` が現在の WiFi パスに応答が一切
    /// 無くなったと判定した(3回連続無応答、約9〜10秒)。
    NoViablePath,
    /// Driver が WiFi-bound 一時 Endpoint で行った疎通確認が成功した。
    WifiProbeSucceeded,
    /// 同、失敗した。
    WifiProbeFailed,
    /// Driver が観測した通信量(UDP実測 + trzsz転送中フラグ)が「静か」と
    /// 判定できる状態になった。
    TrafficQuietDetected,
    /// 通信量が再び増えた。静けさ待ち中に来ても即座に何かをするわけでは
    /// ないが、`TrafficQuietDetected` が来ていないことの裏付けとして
    /// Driver から送られてくる。
    TrafficBusyDetected,
    /// ユーザーが「今すぐ WiFi に戻す」操作を行った(`force_return_to_wifi`)。
    ManualForceReturnRequested,
}

/// [`RebindManager`] が返す出力アクション。実際の実行は Driver が担う。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RebindAction {
    /// 実際にセルラーへ rebind せよ(`RebindExecutor::rebind`)。
    PerformRebindToCellular,
    /// WiFi-bound 一時 fd を取得して疎通確認を1回試みよ
    /// (`PlatformFdSource` → `WifiProbeExecutor::probe`)。
    StartWifiProbe,
    /// 実際に WiFi へ rebind せよ(`RebindExecutor::rebind`)。
    PerformRebindToWifi,
    /// UI へ現在状態を公開せよ。
    PublishState(RebindPublicState),
    /// #22: `QuietTrafficSource`の定期ポーリングを開始せよ(`WaitingQuietToReturn`に
    /// 入った時)。`TrafficQuietDetected`/`TrafficBusyDetected`として結果が返ってくる。
    StartQuietWatch,
    /// 同、停止せよ(`WaitingQuietToReturn`を抜けた時。冪等 — 開始していなくても無害)。
    StopQuietWatch,
}

/// UI(Compose/SwiftUI)へ公開する簡略化された状態。
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum RebindPublicState {
    /// WiFi 上で通常運用中。
    OnWifi,
    /// セルラーへフェイルオーバー済み。WiFi 復活を探っている。
    FailedOverToCellular,
    /// WiFi 復活の疎通確認・ヒステリシスを満たし、通信が静かになるのを
    /// 待っている(この間だけ手動即時切替の「今すぐ戻す」がより有効)。
    WaitingQuietToReturn,
}

/// タイマー識別子。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RebindTimer {
    /// セルラー最小滞在(60秒)。フェイルオーバーした episode 全体で1回だけ
    /// 計測する(ラウンドをやり直しても再スタートしない)。
    CellularMinDwell,
    /// 連続成功の安定確認窓(15秒)。失敗すると kill され、次の成功で
    /// 再スタートする。
    StabilityWindow,
    /// WiFi-bound 一時 Endpoint での疎通確認を定期的に行うための再帰タイマー。
    ProbeCadence,
    /// 静けさ待ちの最大時間。これを超えたら今回は諦めてバックオフへ。
    QuietWait,
    /// 復帰ラウンドを諦めた後、次のプローブ再開までの待ち時間
    /// (2分 → 5分 → 10分、以降は10分に張り付く)。
    Backoff,
}

const CELLULAR_MIN_DWELL: Duration = Duration::from_secs(60);
const STABILITY_WINDOW: Duration = Duration::from_secs(15);
const STABILITY_CONSECUTIVE_SUCCESSES: u32 = 5;
const PROBE_CADENCE: Duration = Duration::from_secs(10);
/// 「静けさを待つ最大時間」は60〜120秒のレンジで設計されている(中間値)。
const QUIET_WAIT_TIMEOUT: Duration = Duration::from_secs(90);
/// 復帰ラウンドを諦めた後のバックオフ。最後の要素で頭打ちにする。
const BACKOFF_STEPS: [Duration; 3] =
    [Duration::from_secs(120), Duration::from_secs(300), Duration::from_secs(600)];

// ── FSM 内部状態 ──────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    OnWifi,
    FailedOverToCellular,
    WaitingQuietToReturn,
}

/// WiFi⇔セルラー rebind の純粋な状態機械。
pub struct RebindManager {
    phase: Phase,
    /// 現在の復帰ラウンドで連続して成功した WiFi 疎通確認の回数。
    /// `WifiProbeFailed` で 0 にリセットされる。
    consecutive_wifi_probe_successes: u32,
    /// `StabilityWindow` タイマーが(直近の失敗を挟まずに)満了したか。
    stability_satisfied: bool,
    /// `CellularMinDwell` タイマーが満了したか。episode 全体で1回だけ立つ。
    cellular_min_dwell_satisfied: bool,
    /// `ManualForceReturnRequested` を受けて、次の疎通確認結果を
    /// ヒステリシス無視の即時復帰として扱うべきか。
    manual_force_pending: bool,
    /// 復帰ラウンドを諦めた回数に応じた `BACKOFF_STEPS` のインデックス。
    /// 実際に WiFi へ復帰できたらリセットする。
    backoff_index: usize,
}

impl RebindManager {
    pub fn new() -> Self {
        RebindManager {
            phase: Phase::OnWifi,
            consecutive_wifi_probe_successes: 0,
            stability_satisfied: false,
            cellular_min_dwell_satisfied: false,
            manual_force_pending: false,
            backoff_index: 0,
        }
    }

    /// 現在の公開状態(UI向け)。
    pub fn public_state(&self) -> RebindPublicState {
        match self.phase {
            Phase::OnWifi => RebindPublicState::OnWifi,
            Phase::FailedOverToCellular => RebindPublicState::FailedOverToCellular,
            Phase::WaitingQuietToReturn => RebindPublicState::WaitingQuietToReturn,
        }
    }

    fn reset_return_round(&mut self) {
        self.consecutive_wifi_probe_successes = 0;
        self.stability_satisfied = false;
        // cellular_min_dwell_satisfied は episode 全体で1回だけなのでリセットしない。
    }

    fn reset_episode(&mut self) {
        self.reset_return_round();
        self.cellular_min_dwell_satisfied = false;
        self.manual_force_pending = false;
    }

    fn current_backoff(&self) -> Duration {
        BACKOFF_STEPS[self.backoff_index.min(BACKOFF_STEPS.len() - 1)]
    }

    fn advance_backoff_index(&mut self) {
        if self.backoff_index < BACKOFF_STEPS.len() - 1 {
            self.backoff_index += 1;
        }
    }

    fn handle_no_viable_path(&mut self) -> Response<RebindAction, RebindTimer> {
        if self.phase != Phase::OnWifi {
            // 既にセルラー側にいる/復帰試行中に再度 NoViablePath が来た場合
            // (セルラー自体も死んだ等)は本FSMのスコープ外。上位の
            // path_health/多重フェイルオーバー検知に委ねる。
            return Response::consume();
        }
        self.phase = Phase::FailedOverToCellular;
        self.reset_episode();
        Response::emit(vec![
            RebindAction::PerformRebindToCellular,
            RebindAction::PublishState(RebindPublicState::FailedOverToCellular),
            RebindAction::StartWifiProbe,
        ])
        .with_timer(RebindTimer::CellularMinDwell, CELLULAR_MIN_DWELL)
        .with_timer(RebindTimer::ProbeCadence, PROBE_CADENCE)
    }

    fn handle_probe_succeeded(&mut self) -> Response<RebindAction, RebindTimer> {
        if self.manual_force_pending {
            return self.complete_manual_return();
        }
        match self.phase {
            Phase::OnWifi => Response::consume(),
            Phase::FailedOverToCellular | Phase::WaitingQuietToReturn => {
                self.consecutive_wifi_probe_successes =
                    self.consecutive_wifi_probe_successes.saturating_add(1);
                let mut resp = Response::consume();
                if self.consecutive_wifi_probe_successes == 1 {
                    resp = resp.with_timer(RebindTimer::StabilityWindow, STABILITY_WINDOW);
                }
                self.maybe_enter_waiting_quiet(resp)
            }
        }
    }

    fn handle_probe_failed(&mut self) -> Response<RebindAction, RebindTimer> {
        if self.manual_force_pending {
            // 手動即時切替の疎通確認が失敗しただけ。ヒステリシス上のペナルティは
            // 課さず、通常の復帰探索(ProbeCadence)に戻す。
            self.manual_force_pending = false;
            return Response::consume();
        }
        match self.phase {
            Phase::OnWifi => Response::consume(),
            Phase::FailedOverToCellular => {
                self.consecutive_wifi_probe_successes = 0;
                Response::consume().with_kill_timer(RebindTimer::StabilityWindow)
            }
            Phase::WaitingQuietToReturn => {
                // 静けさ待ち中に WiFi がまた劣化した: このラウンドは諦めて
                // バックオフへ(見落としリスクとしてCodexが指摘した経路)。
                self.give_up_round()
            }
        }
    }

    fn maybe_enter_waiting_quiet(
        &mut self,
        resp: Response<RebindAction, RebindTimer>,
    ) -> Response<RebindAction, RebindTimer> {
        if self.phase != Phase::FailedOverToCellular
            || self.consecutive_wifi_probe_successes < STABILITY_CONSECUTIVE_SUCCESSES
            || !self.stability_satisfied
            || !self.cellular_min_dwell_satisfied
        {
            return resp;
        }
        self.phase = Phase::WaitingQuietToReturn;
        let mut actions = resp.actions;
        actions.push(RebindAction::PublishState(RebindPublicState::WaitingQuietToReturn));
        actions.push(RebindAction::StartQuietWatch);
        let mut timers = resp.timers;
        timers.extend(
            Response::<RebindAction, RebindTimer>::consume()
                .with_timer(RebindTimer::QuietWait, QUIET_WAIT_TIMEOUT)
                .timers,
        );
        Response { consumed: true, actions, timers }
    }

    fn handle_quiet_detected(&mut self) -> Response<RebindAction, RebindTimer> {
        if self.phase != Phase::WaitingQuietToReturn {
            return Response::consume();
        }
        self.complete_automatic_return()
    }

    fn complete_automatic_return(&mut self) -> Response<RebindAction, RebindTimer> {
        self.phase = Phase::OnWifi;
        self.backoff_index = 0;
        self.reset_episode();
        Response::emit(vec![
            RebindAction::PerformRebindToWifi,
            RebindAction::PublishState(RebindPublicState::OnWifi),
            RebindAction::StopQuietWatch,
        ])
        .with_kill_timer(RebindTimer::QuietWait)
        .with_kill_timer(RebindTimer::ProbeCadence)
        .with_kill_timer(RebindTimer::StabilityWindow)
        .with_kill_timer(RebindTimer::Backoff)
        .with_kill_timer(RebindTimer::CellularMinDwell)
    }

    fn complete_manual_return(&mut self) -> Response<RebindAction, RebindTimer> {
        self.manual_force_pending = false;
        // OnWifi 中の手動要求は本来 StartWifiProbe を出していないので
        // ここに来ることはないが、念のため OnWifi でも安全に no-op化する。
        if self.phase == Phase::OnWifi {
            return Response::consume();
        }
        self.complete_automatic_return()
    }

    fn give_up_round(&mut self) -> Response<RebindAction, RebindTimer> {
        self.phase = Phase::FailedOverToCellular;
        self.reset_return_round();
        let backoff = self.current_backoff();
        self.advance_backoff_index();
        Response::emit(vec![
            RebindAction::PublishState(RebindPublicState::FailedOverToCellular),
            RebindAction::StopQuietWatch,
        ])
            .with_kill_timer(RebindTimer::QuietWait)
            .with_kill_timer(RebindTimer::ProbeCadence)
            .with_timer(RebindTimer::Backoff, backoff)
    }

    fn handle_manual_force_return(&mut self) -> Response<RebindAction, RebindTimer> {
        if self.phase == Phase::OnWifi {
            return Response::consume();
        }
        self.manual_force_pending = true;
        Response::emit(vec![RebindAction::StartWifiProbe])
    }
}

impl Default for RebindManager {
    fn default() -> Self {
        Self::new()
    }
}

impl TimedStateMachine for RebindManager {
    type Event = RebindEvent;
    type Action = RebindAction;
    type TimerId = RebindTimer;

    fn on_event(&mut self, event: RebindEvent) -> Response<RebindAction, RebindTimer> {
        match event {
            RebindEvent::NoViablePath => self.handle_no_viable_path(),
            RebindEvent::WifiProbeSucceeded => self.handle_probe_succeeded(),
            RebindEvent::WifiProbeFailed => self.handle_probe_failed(),
            RebindEvent::TrafficQuietDetected => self.handle_quiet_detected(),
            RebindEvent::TrafficBusyDetected => Response::consume(),
            RebindEvent::ManualForceReturnRequested => self.handle_manual_force_return(),
        }
    }

    fn on_timeout(&mut self, id: RebindTimer) -> Response<RebindAction, RebindTimer> {
        match id {
            RebindTimer::CellularMinDwell => {
                if self.phase == Phase::OnWifi {
                    return Response::pass_through();
                }
                self.cellular_min_dwell_satisfied = true;
                self.maybe_enter_waiting_quiet(Response::consume())
            }
            RebindTimer::StabilityWindow => {
                if self.phase == Phase::OnWifi {
                    return Response::pass_through();
                }
                self.stability_satisfied = true;
                self.maybe_enter_waiting_quiet(Response::consume())
            }
            RebindTimer::ProbeCadence => {
                if self.phase == Phase::OnWifi {
                    return Response::pass_through();
                }
                Response::emit(vec![RebindAction::StartWifiProbe])
                    .with_timer(RebindTimer::ProbeCadence, PROBE_CADENCE)
            }
            RebindTimer::QuietWait => {
                if self.phase != Phase::WaitingQuietToReturn {
                    return Response::pass_through();
                }
                self.give_up_round()
            }
            RebindTimer::Backoff => {
                if self.phase == Phase::OnWifi {
                    return Response::pass_through();
                }
                // バックオフ明け: プロービングを再開する。
                Response::emit(vec![RebindAction::StartWifiProbe])
                    .with_timer(RebindTimer::ProbeCadence, PROBE_CADENCE)
            }
        }
    }
}

// ── テスト ───────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use timed_fsm::TimerCommand;

    fn has_action(resp: &Response<RebindAction, RebindTimer>, action: RebindAction) -> bool {
        resp.actions.contains(&action)
    }

    fn timer_set(resp: &Response<RebindAction, RebindTimer>, id: RebindTimer) -> bool {
        resp.timers.iter().any(|t| matches!(t, TimerCommand::Set { id: i, .. } if *i == id))
    }

    fn timer_killed(resp: &Response<RebindAction, RebindTimer>, id: RebindTimer) -> bool {
        resp.timers.iter().any(|t| matches!(t, TimerCommand::Kill { id: i } if *i == id))
    }

    /// WiFi が5回連続成功+安定窓+最小滞在をすべて満たすまで進める便利関数。
    /// 呼び出し順は実装の意図通り: dwellとstabilityのタイマーを先に満了させてから
    /// 最後の成功で WaitingQuietToReturn に入る。
    fn drive_to_waiting_quiet(m: &mut RebindManager) {
        m.on_event(RebindEvent::NoViablePath);
        for _ in 0..(STABILITY_CONSECUTIVE_SUCCESSES - 1) {
            m.on_event(RebindEvent::WifiProbeSucceeded);
        }
        m.on_timeout(RebindTimer::StabilityWindow);
        m.on_timeout(RebindTimer::CellularMinDwell);
        let resp = m.on_event(RebindEvent::WifiProbeSucceeded);
        assert!(has_action(&resp, RebindAction::PublishState(RebindPublicState::WaitingQuietToReturn)));
        assert_eq!(m.public_state(), RebindPublicState::WaitingQuietToReturn);
    }

    #[test]
    fn on_wifi_ignores_probe_and_quiet_events() {
        let mut m = RebindManager::new();
        assert_eq!(m.public_state(), RebindPublicState::OnWifi);
        let r1 = m.on_event(RebindEvent::WifiProbeSucceeded);
        assert!(r1.actions.is_empty());
        let r2 = m.on_event(RebindEvent::TrafficQuietDetected);
        assert!(r2.actions.is_empty());
        assert_eq!(m.public_state(), RebindPublicState::OnWifi);
    }

    #[test]
    fn no_viable_path_immediately_fails_over_and_starts_probing() {
        let mut m = RebindManager::new();
        let r = m.on_event(RebindEvent::NoViablePath);
        assert!(has_action(&r, RebindAction::PerformRebindToCellular));
        assert!(has_action(&r, RebindAction::StartWifiProbe));
        assert!(has_action(&r, RebindAction::PublishState(RebindPublicState::FailedOverToCellular)));
        assert!(timer_set(&r, RebindTimer::CellularMinDwell));
        assert!(timer_set(&r, RebindTimer::ProbeCadence));
        assert_eq!(m.public_state(), RebindPublicState::FailedOverToCellular);
    }

    #[test]
    fn repeated_no_viable_path_while_failed_over_is_noop() {
        let mut m = RebindManager::new();
        m.on_event(RebindEvent::NoViablePath);
        let r = m.on_event(RebindEvent::NoViablePath);
        assert!(r.actions.is_empty());
        assert!(r.timers.is_empty());
    }

    #[test]
    fn single_success_does_not_yet_satisfy_hysteresis() {
        let mut m = RebindManager::new();
        m.on_event(RebindEvent::NoViablePath);
        let r = m.on_event(RebindEvent::WifiProbeSucceeded);
        assert!(!has_action(&r, RebindAction::PerformRebindToWifi));
        assert_eq!(m.public_state(), RebindPublicState::FailedOverToCellular);
        assert!(timer_set(&r, RebindTimer::StabilityWindow), "1回目の成功でStabilityWindowを開始する");
    }

    #[test]
    fn probe_failure_resets_consecutive_success_streak() {
        let mut m = RebindManager::new();
        m.on_event(RebindEvent::NoViablePath);
        m.on_event(RebindEvent::WifiProbeSucceeded);
        let r = m.on_event(RebindEvent::WifiProbeFailed);
        assert!(timer_killed(&r, RebindTimer::StabilityWindow));
        assert_eq!(m.consecutive_wifi_probe_successes, 0);

        // カウンタがリセットされているので、素朴に4回成功させても閾値5に届かない
        for _ in 0..4 {
            let r = m.on_event(RebindEvent::WifiProbeSucceeded);
            assert!(!has_action(&r, RebindAction::PerformRebindToWifi));
        }
    }

    #[test]
    fn dwell_and_stability_and_five_successes_all_required_before_waiting_quiet() {
        let mut m = RebindManager::new();
        m.on_event(RebindEvent::NoViablePath);

        // 5回連続成功はしたが、まだ安定窓・最小滞在タイマーが満了していない
        for _ in 0..STABILITY_CONSECUTIVE_SUCCESSES {
            let r = m.on_event(RebindEvent::WifiProbeSucceeded);
            assert!(!has_action(&r, RebindAction::PerformRebindToWifi));
            assert_ne!(m.public_state(), RebindPublicState::WaitingQuietToReturn);
        }

        // StabilityWindowだけ満了 → まだ足りない(CellularMinDwellが未満了)
        let r = m.on_timeout(RebindTimer::StabilityWindow);
        assert!(!has_action(&r, RebindAction::PublishState(RebindPublicState::WaitingQuietToReturn)));

        // CellularMinDwellも満了 → ようやくWaitingQuietToReturnへ
        let r = m.on_timeout(RebindTimer::CellularMinDwell);
        assert!(has_action(&r, RebindAction::PublishState(RebindPublicState::WaitingQuietToReturn)));
        assert!(has_action(&r, RebindAction::StartQuietWatch), "#22: WaitingQuietToReturnに入ったらQuietWatchを開始する");
        assert!(timer_set(&r, RebindTimer::QuietWait));
        assert_eq!(m.public_state(), RebindPublicState::WaitingQuietToReturn);
    }

    #[test]
    fn quiet_detected_while_waiting_triggers_immediate_return_to_wifi() {
        let mut m = RebindManager::new();
        drive_to_waiting_quiet(&mut m);

        let r = m.on_event(RebindEvent::TrafficQuietDetected);
        assert!(has_action(&r, RebindAction::PerformRebindToWifi));
        assert!(has_action(&r, RebindAction::PublishState(RebindPublicState::OnWifi)));
        assert!(has_action(&r, RebindAction::StopQuietWatch));
        assert!(timer_killed(&r, RebindTimer::QuietWait));
        assert!(timer_killed(&r, RebindTimer::ProbeCadence));
        assert_eq!(m.public_state(), RebindPublicState::OnWifi);
    }

    #[test]
    fn traffic_busy_while_waiting_does_not_return_or_cancel_wait() {
        let mut m = RebindManager::new();
        drive_to_waiting_quiet(&mut m);

        let r = m.on_event(RebindEvent::TrafficBusyDetected);
        assert!(r.actions.is_empty());
        assert!(r.timers.is_empty());
        assert_eq!(m.public_state(), RebindPublicState::WaitingQuietToReturn);
    }

    #[test]
    fn quiet_wait_timeout_gives_up_round_and_starts_backoff_not_forced_return() {
        let mut m = RebindManager::new();
        drive_to_waiting_quiet(&mut m);

        let r = m.on_timeout(RebindTimer::QuietWait);
        assert!(!has_action(&r, RebindAction::PerformRebindToWifi), "静けさが来ないまま強制切替はしない");
        assert!(has_action(&r, RebindAction::PublishState(RebindPublicState::FailedOverToCellular)));
        assert!(has_action(&r, RebindAction::StopQuietWatch));
        assert!(timer_set(&r, RebindTimer::Backoff));
        assert!(timer_killed(&r, RebindTimer::ProbeCadence));
        assert_eq!(m.public_state(), RebindPublicState::FailedOverToCellular);
    }

    #[test]
    fn wifi_probe_failed_while_waiting_quiet_abandons_round_and_backs_off() {
        let mut m = RebindManager::new();
        drive_to_waiting_quiet(&mut m);

        let r = m.on_event(RebindEvent::WifiProbeFailed);
        assert!(has_action(&r, RebindAction::PublishState(RebindPublicState::FailedOverToCellular)));
        assert!(has_action(&r, RebindAction::StopQuietWatch));
        assert!(timer_set(&r, RebindTimer::Backoff));
        assert_eq!(m.public_state(), RebindPublicState::FailedOverToCellular);
    }

    #[test]
    fn backoff_timeout_resumes_probing() {
        let mut m = RebindManager::new();
        drive_to_waiting_quiet(&mut m);
        m.on_timeout(RebindTimer::QuietWait);

        let r = m.on_timeout(RebindTimer::Backoff);
        assert!(has_action(&r, RebindAction::StartWifiProbe));
        assert!(timer_set(&r, RebindTimer::ProbeCadence));
    }

    #[test]
    fn backoff_escalates_then_caps_at_last_step() {
        let mut m = RebindManager::new();
        drive_to_waiting_quiet(&mut m);
        let r1 = m.on_timeout(RebindTimer::QuietWait);
        assert!(r1.timers.iter().any(|t| matches!(t,
            TimerCommand::Set { id: RebindTimer::Backoff, duration } if *duration == BACKOFF_STEPS[0])));
        m.on_timeout(RebindTimer::Backoff);

        drive_to_waiting_quiet(&mut m);
        let r2 = m.on_timeout(RebindTimer::QuietWait);
        assert!(r2.timers.iter().any(|t| matches!(t,
            TimerCommand::Set { id: RebindTimer::Backoff, duration } if *duration == BACKOFF_STEPS[1])));
    }

    #[test]
    fn successful_return_resets_backoff_index() {
        let mut m = RebindManager::new();
        drive_to_waiting_quiet(&mut m);
        m.on_timeout(RebindTimer::QuietWait); // 諦めてbackoff_index=1へ進む
        m.on_timeout(RebindTimer::Backoff); // バックオフ明け、プロービング再開
        drive_to_waiting_quiet_after_backoff(&mut m);
        let r = m.on_event(RebindEvent::TrafficQuietDetected);
        assert!(has_action(&r, RebindAction::PerformRebindToWifi));
        assert_eq!(m.backoff_index, 0);
    }

    /// バックオフ明けで再度5回成功+dwell(既に満了済み)+stabilityを満たしてWaitingQuietへ。
    fn drive_to_waiting_quiet_after_backoff(m: &mut RebindManager) {
        for _ in 0..(STABILITY_CONSECUTIVE_SUCCESSES - 1) {
            m.on_event(RebindEvent::WifiProbeSucceeded);
        }
        m.on_timeout(RebindTimer::StabilityWindow);
        m.on_event(RebindEvent::WifiProbeSucceeded);
        assert_eq!(m.public_state(), RebindPublicState::WaitingQuietToReturn);
    }

    #[test]
    fn manual_force_return_bypasses_hysteresis_but_requires_probe_success() {
        let mut m = RebindManager::new();
        m.on_event(RebindEvent::NoViablePath);
        // まだ1回も成功していない(dwell/stability未満了)状態でも、手動要求は
        // 即座にStartWifiProbeを出す。
        let r = m.on_event(RebindEvent::ManualForceReturnRequested);
        assert!(has_action(&r, RebindAction::StartWifiProbe));
        assert_eq!(m.public_state(), RebindPublicState::FailedOverToCellular);

        // 疎通確認が成功した瞬間、ヒステリシスを満たしていなくても即復帰する。
        let r2 = m.on_event(RebindEvent::WifiProbeSucceeded);
        assert!(has_action(&r2, RebindAction::PerformRebindToWifi));
        assert!(has_action(&r2, RebindAction::PublishState(RebindPublicState::OnWifi)));
        assert_eq!(m.public_state(), RebindPublicState::OnWifi);
    }

    #[test]
    fn manual_force_return_failure_does_not_penalize_with_backoff() {
        let mut m = RebindManager::new();
        m.on_event(RebindEvent::NoViablePath);
        m.on_event(RebindEvent::ManualForceReturnRequested);

        let r = m.on_event(RebindEvent::WifiProbeFailed);
        assert!(!timer_set(&r, RebindTimer::Backoff), "手動失敗はバックオフの対象にしない");
        assert_eq!(m.public_state(), RebindPublicState::FailedOverToCellular);
    }

    #[test]
    fn manual_force_return_on_wifi_is_noop() {
        let mut m = RebindManager::new();
        let r = m.on_event(RebindEvent::ManualForceReturnRequested);
        assert!(r.actions.is_empty());
        assert!(r.timers.is_empty());
    }

    #[test]
    fn probe_cadence_timeout_reprobes_and_rearms_itself() {
        let mut m = RebindManager::new();
        m.on_event(RebindEvent::NoViablePath);
        let r = m.on_timeout(RebindTimer::ProbeCadence);
        assert!(has_action(&r, RebindAction::StartWifiProbe));
        assert!(timer_set(&r, RebindTimer::ProbeCadence));
    }

    #[test]
    fn stray_timeouts_on_wifi_pass_through() {
        let mut m = RebindManager::new();
        for id in [
            RebindTimer::CellularMinDwell,
            RebindTimer::StabilityWindow,
            RebindTimer::ProbeCadence,
            RebindTimer::QuietWait,
            RebindTimer::Backoff,
        ] {
            let r = m.on_timeout(id);
            r.assert_pass_through();
        }
    }
}
