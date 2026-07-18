//! [`spawn_health_monitor`](crate::path_health::spawn_health_monitor)の
//! ping→settle待ち→stats評価サイクルを、`isekai-terminal-core`の
//! `rebind_manager.rs`/`trzsz::TrzszTransferFsm`と同じ
//! `timed_fsm::TimedStateMachine`パターンに乗せた純粋な状態機械。
//!
//! この型自身は`noq::Connection`/`tokio`に一切触れない。ping送出・
//! stats読み取りといった実I/Oは[`PathHealthAction`]として宣言的に返すだけで、
//! 実際に叩くのは呼び出し側(`spawn_health_monitor`のDriverループ)の役目。
//! `classify_path_health`/`has_zero_response`(pure, unit-tested)の判定結果を
//! 元に、連続成功/連続無応答のヒステリシスをどう遷移させるかという、旧実装では
//! `spawn_health_monitor`の`loop`本体に埋め込まれていて未テストだった部分を
//! ここに切り出し、`noq::Connection`無しでunit testできるようにしてある。

use std::time::Duration;

use timed_fsm::{Response, TimedStateMachine};

use crate::path_health::{classify_path_health, has_zero_response};

/// `path.ping()`(PING frame送出)→ 少し待つ → `path.stats()`を定期的に行う間隔。
pub const HEALTH_CHECK_INTERVAL: Duration = Duration::from_secs(3);
/// pingを送ってから`stats()`を読むまでの待ち時間。
pub const PING_SETTLE_DELAY: Duration = Duration::from_millis(300);
/// これを超えるRTTはDegraded扱い。
pub const DEGRADED_RTT_THRESHOLD: Duration = Duration::from_millis(800);
/// 直近チェック区間でのロス率がこれを超えるとDegraded扱い。
pub const DEGRADED_LOSS_RATIO: f64 = 0.2;
/// Degraded→Validated復帰に必要な連続healthy回数。
pub const RECOVERY_CONSECUTIVE_CHECKS: u32 = 2;
/// 完全な無応答(zero response)と判定するために必要な連続検出回数
/// (単発では実ネットワークのジッタで容易に偽陽性になるため、実機検証で
/// 複数回連続を要求する設計に変更された — `has_zero_response`のdocも参照)。
pub const NO_RESPONSE_CONSECUTIVE_CHECKS: u32 = 3;

/// [`PathHealthFsm`]への入力イベント。
// `noq::PathStats`は`#[non_exhaustive]`な複数統計フィールドを持つため`Start`より
// かなり大きいが、この間隔(3秒毎)ではホットパスにならずBox化の価値が無い。
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathHealthFsmEvent {
    /// Driverが監視ループを開始した直後に一度だけ送る。最初の
    /// [`PathHealthTimer::CheckInterval`]を起動するためのキック。
    Start,
    /// Driverが`SettleDelay`満了後に`path.stats()`を読み取った結果。
    StatsReceived(noq::PathStats),
}

/// [`PathHealthFsm`]が返す出力アクション。実際の実行はDriverが担う。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PathHealthAction {
    /// `path.ping()`を送出せよ。pathが既に閉じている/送出失敗の場合、
    /// Driverは監視ループそのものを終了させる(タイマーの再アームは行わない)。
    SendPing,
    /// `path.stats()`を読み取り、結果を
    /// [`PathHealthFsmEvent::StatsReceived`]としてこのFSMへ返せ。
    ReadStats,
    /// 完全な無応答(zero response)が`NO_RESPONSE_CONSECUTIVE_CHECKS`回連続した。
    /// Driverは`tracker`をDegradedにし、`notify_if_no_viable_path`を呼ぶこと
    /// (`path.set_status()`はここでは呼ばない — 元実装通り、RTT/ロス based の
    /// 判定とは独立した信号のため)。
    DegradeZeroResponse { consecutive: u32 },
    /// RTT/ロス率/black hole判定で不健全と判定された(かつ、まだDegradedでは
    /// なかった)。Driverは`path.set_status(Backup)`し`tracker`をDegradedにすること。
    DegradeUnhealthy { rtt: Duration },
    /// Degradedから、連続healthy回数がしきい値に達して復帰した。Driverは
    /// `path.set_status(Available)`し`tracker`をValidatedにすること。
    Recover,
}

/// タイマー識別子。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PathHealthTimer {
    /// 次のping送出までの間隔(再帰的に自分自身を再アームする)。
    CheckInterval,
    /// ping送出後、statsを読むまでの待ち時間。
    SettleDelay,
}

/// 1本のpathのping駆動ヘルスチェックサイクルを表す純粋な状態機械
/// (`spawn_health_monitor`のDriverが1インスタンスにつき1本のpathを監視する)。
pub struct PathHealthFsm {
    prev_stats: Option<noq::PathStats>,
    /// 直近の連続healthy回数(`WifiProbeSucceeded`相当)。
    consecutive_healthy: u32,
    /// 直近の連続zero-response回数。
    consecutive_no_response: u32,
    /// このFSM視点での現在のDegraded/Validated状態。Driver側の
    /// `PathHealthTracker`はこのFSMだけがDegraded遷移を書き込む
    /// (path確立時のValidated/Failedセットの後は本FSMが唯一の書き手)ので、
    /// ここでのローカルミラーは常にtrackerの実値と一致する。
    degraded: bool,
}

impl PathHealthFsm {
    pub fn new() -> Self {
        PathHealthFsm { prev_stats: None, consecutive_healthy: 0, consecutive_no_response: 0, degraded: false }
    }

    fn handle_stats_received(&mut self, stats: noq::PathStats) -> Response<PathHealthAction, PathHealthTimer> {
        let rtt = stats.rtt;
        let zero_response = has_zero_response(self.prev_stats.as_ref(), &stats);
        // `zero_response`をそのまま`healthy`にも反映させる。そうしないと、完全な
        // 無応答下でも`classify_path_health`(RTT/ロス率/black hole)だけは「健全」と
        // 読み続けてしまい(statsが更新自体止まるため)、下のリカバリ判定が
        // `zero_response`側のDegraded降格と競合して即座にValidatedへ戻してしまう。
        let healthy = classify_path_health(self.prev_stats.as_ref(), &stats) && !zero_response;
        self.prev_stats = Some(stats);

        let mut actions = Vec::new();

        if zero_response {
            self.consecutive_no_response = self.consecutive_no_response.saturating_add(1);
            if self.consecutive_no_response >= NO_RESPONSE_CONSECUTIVE_CHECKS {
                self.degraded = true;
                actions.push(PathHealthAction::DegradeZeroResponse { consecutive: self.consecutive_no_response });
            }
        } else {
            self.consecutive_no_response = 0;
        }

        if healthy {
            self.consecutive_healthy = self.consecutive_healthy.saturating_add(1);
            if self.degraded && self.consecutive_healthy >= RECOVERY_CONSECUTIVE_CHECKS {
                self.degraded = false;
                actions.push(PathHealthAction::Recover);
            }
        } else {
            self.consecutive_healthy = 0;
            if !self.degraded {
                self.degraded = true;
                actions.push(PathHealthAction::DegradeUnhealthy { rtt });
            }
        }

        Response::emit(actions).with_timer(PathHealthTimer::CheckInterval, HEALTH_CHECK_INTERVAL)
    }
}

impl Default for PathHealthFsm {
    fn default() -> Self {
        Self::new()
    }
}

impl TimedStateMachine for PathHealthFsm {
    type Event = PathHealthFsmEvent;
    type Action = PathHealthAction;
    type TimerId = PathHealthTimer;

    fn on_event(&mut self, event: PathHealthFsmEvent) -> Response<PathHealthAction, PathHealthTimer> {
        match event {
            PathHealthFsmEvent::Start => {
                Response::consume().with_timer(PathHealthTimer::CheckInterval, HEALTH_CHECK_INTERVAL)
            }
            PathHealthFsmEvent::StatsReceived(stats) => self.handle_stats_received(stats),
        }
    }

    fn on_timeout(&mut self, id: PathHealthTimer) -> Response<PathHealthAction, PathHealthTimer> {
        match id {
            PathHealthTimer::CheckInterval => Response::emit(vec![PathHealthAction::SendPing])
                .with_timer(PathHealthTimer::SettleDelay, PING_SETTLE_DELAY),
            PathHealthTimer::SettleDelay => Response::emit(vec![PathHealthAction::ReadStats]),
        }
    }
}

// ── テスト ───────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use timed_fsm::TimerCommand;

    // `noq::PathStats`/`UdpStats`は`#[non_exhaustive]`なので他クレートからは
    // 構造体リテラル(`..Default::default()`併用でも)で作れない。
    // `Default::default()`してからpubフィールドへ代入する形にする
    // (`path_health.rs`のテストヘルパと同じ)。
    fn stats_with(rtt: Duration, datagrams: u64, recvd_datagrams: u64, lost_packets: u64, black_holes_detected: u64) -> noq::PathStats {
        let mut udp_tx = noq::UdpStats::default();
        udp_tx.datagrams = datagrams;
        let mut udp_rx = noq::UdpStats::default();
        udp_rx.datagrams = recvd_datagrams;
        let mut stats = noq::PathStats::default();
        stats.rtt = rtt;
        stats.udp_tx = udp_tx;
        stats.udp_rx = udp_rx;
        stats.lost_packets = lost_packets;
        stats.black_holes_detected = black_holes_detected;
        stats
    }

    fn healthy_stats(sent: u64, recvd: u64) -> noq::PathStats {
        stats_with(Duration::from_millis(50), sent, recvd, 0, 0)
    }

    fn has_action(resp: &Response<PathHealthAction, PathHealthTimer>, action: PathHealthAction) -> bool {
        resp.actions.contains(&action)
    }

    fn timer_set(resp: &Response<PathHealthAction, PathHealthTimer>, id: PathHealthTimer) -> bool {
        resp.timers.iter().any(|t| matches!(t, TimerCommand::Set { id: i, .. } if *i == id))
    }

    #[test]
    fn start_arms_check_interval() {
        let mut fsm = PathHealthFsm::new();
        let r = fsm.on_event(PathHealthFsmEvent::Start);
        r.assert_consumed();
        assert!(timer_set(&r, PathHealthTimer::CheckInterval));
    }

    #[test]
    fn check_interval_timeout_pings_and_arms_settle_delay() {
        let mut fsm = PathHealthFsm::new();
        fsm.on_event(PathHealthFsmEvent::Start);
        let r = fsm.on_timeout(PathHealthTimer::CheckInterval);
        assert!(has_action(&r, PathHealthAction::SendPing));
        assert!(timer_set(&r, PathHealthTimer::SettleDelay));
    }

    #[test]
    fn settle_delay_timeout_requests_stats() {
        let mut fsm = PathHealthFsm::new();
        fsm.on_event(PathHealthFsmEvent::Start);
        fsm.on_timeout(PathHealthTimer::CheckInterval);
        let r = fsm.on_timeout(PathHealthTimer::SettleDelay);
        assert_eq!(r.actions, vec![PathHealthAction::ReadStats]);
        // 次サイクルのCheckIntervalはstats評価後(StatsReceived)で再アームされる。
        assert!(r.timers.is_empty());
    }

    #[test]
    fn first_healthy_stats_reading_produces_no_actions_but_rearms() {
        let mut fsm = PathHealthFsm::new();
        let r = fsm.on_event(PathHealthFsmEvent::StatsReceived(healthy_stats(0, 0)));
        assert!(r.actions.is_empty());
        assert!(timer_set(&r, PathHealthTimer::CheckInterval));
    }

    #[test]
    fn high_rtt_degrades_immediately() {
        let mut fsm = PathHealthFsm::new();
        let bad = stats_with(Duration::from_millis(900), 10, 10, 0, 0);
        let r = fsm.on_event(PathHealthFsmEvent::StatsReceived(bad));
        assert!(has_action(&r, PathHealthAction::DegradeUnhealthy { rtt: Duration::from_millis(900) }));
    }

    #[test]
    fn repeated_degraded_reading_does_not_re_emit_degrade_action() {
        let mut fsm = PathHealthFsm::new();
        let bad = stats_with(Duration::from_millis(900), 10, 10, 0, 0);
        fsm.on_event(PathHealthFsmEvent::StatsReceived(bad));
        let r = fsm.on_event(PathHealthFsmEvent::StatsReceived(bad));
        assert!(r.actions.is_empty(), "既にDegradedなら再度DegradeUnhealthyは出さない");
    }

    #[test]
    fn recovers_after_consecutive_healthy_checks() {
        let mut fsm = PathHealthFsm::new();
        let bad = stats_with(Duration::from_millis(900), 10, 10, 0, 0);
        fsm.on_event(PathHealthFsmEvent::StatsReceived(bad));

        let r1 = fsm.on_event(PathHealthFsmEvent::StatsReceived(healthy_stats(20, 20)));
        assert!(r1.actions.is_empty(), "1回目の健全readingだけではまだ復帰しない");

        let r2 = fsm.on_event(PathHealthFsmEvent::StatsReceived(healthy_stats(30, 30)));
        assert!(has_action(&r2, PathHealthAction::Recover));
    }

    #[test]
    fn zero_response_forces_unhealthy_and_needs_consecutive_checks_to_notify() {
        let mut fsm = PathHealthFsm::new();
        fsm.on_event(PathHealthFsmEvent::StatsReceived(healthy_stats(0, 0)));

        // sent_delta > 0 だが recv_delta == 0: 送ったのに何も返ってこない。
        // zero-response通知自体は1回だけでは出さないが(NO_RESPONSE_CONSECUTIVE_CHECKS未満)、
        // `zero_response`は`healthy`もfalseにするので、RTT/ロス側は初回から不健全と
        // 判定してDegradeUnhealthyを出す(元実装通り、2つの判定は独立)。
        let r1 = fsm.on_event(PathHealthFsmEvent::StatsReceived(healthy_stats(10, 0)));
        assert!(!has_action(&r1, PathHealthAction::DegradeZeroResponse { consecutive: 1 }));
        assert!(has_action(&r1, PathHealthAction::DegradeUnhealthy { rtt: Duration::from_millis(50) }));

        let r2 = fsm.on_event(PathHealthFsmEvent::StatsReceived(healthy_stats(20, 0)));
        assert!(r2.actions.is_empty(), "既にDegradedなのでDegradeUnhealthyは再度出ない、閾値もまだ届かない");

        let r3 = fsm.on_event(PathHealthFsmEvent::StatsReceived(healthy_stats(30, 0)));
        assert!(has_action(&r3, PathHealthAction::DegradeZeroResponse { consecutive: 3 }), "3回連続で無応答通知が出る");
    }

    #[test]
    fn every_stats_reading_rearms_check_interval() {
        let mut fsm = PathHealthFsm::new();
        for i in 0..5 {
            let r = fsm.on_event(PathHealthFsmEvent::StatsReceived(healthy_stats(i, i)));
            assert!(timer_set(&r, PathHealthTimer::CheckInterval));
        }
    }
}
