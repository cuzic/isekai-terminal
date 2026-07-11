//! Path health tracking and rebind-trigger decisions for multipath QUIC
//! connections — ported and generalized from `isekai-terminal-core`'s
//! `multipath_transport.rs` (`PathBroker`/`spawn_health_monitor`/
//! `classify_path_health`/`has_zero_response`, Phase 9-5, real-hardware
//! verified: `PLAN.md` "Phase 9-5 実機検証結果").
//!
//! # What this module does NOT cover
//!
//! `multipath_transport.rs`'s Phase 9-4 ("同時に複数の物理インタフェース
//! (Wi-Fi/セルラー)を`noq::Connection::open_path(local_ip=Some(..))`で同時保持する")
//! is a confirmed dead end: [noq issue #738](https://github.com/n0-computer/noq/issues/738)
//! means `PATH_RESPONSE` frames for such `local_ip`-bound paths never reach
//! noq's internal dispatch, so the path is always abandoned as
//! `ValidationFailed` — on real Android hardware this was downgraded to a
//! non-functional experimental flag (`PLAN.md` 1448行目以降). This module
//! does not port that mechanism.
//!
//! What *is* proven working, and what this module exists to generalize:
//! - Holding multiple **remote-address** paths simultaneously via
//!   `open_path` with `local_ip: None` (OS default routing) — Android's
//!   path0/path1 (Tailscale⇔direct address), Phase 9-2/9-3.
//! - Reactive physical-interface failover via
//!   [`quicmux::AnyMuxRebinder::rebind`] —
//!   the same operation as `noq::Endpoint::rebind_abstract()`
//!   (`multipath_transport.rs`'s Phase 9-4b), triggered by exactly the
//!   health/zero-response signals this module classifies.
//!
//! Both cases need the same "is this path still good, and has it stopped
//! responding entirely" classification, which is what
//! [`classify_path_health`]/[`has_zero_response`] (pure, unit-tested against
//! synthetic `noq::PathStats`) and [`spawn_health_monitor`] (the
//! ping-then-poll-stats loop that drives them against a live
//! `noq::Connection`) provide.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use log::{info, warn};

/// A path's identity for logging/tracking purposes, chosen by the caller
/// (e.g. `"primary"`, `"secondary"`, `"physical-wifi"` on Android;
/// `"tethering"` for a PC warm-standby path). Opaque to this module beyond
/// equality/hashing — unlike `multipath_transport.rs`'s `PathCandidateId`,
/// this crate has no fixed vocabulary of path roles (Android-specific
/// concepts like "physical Wi-Fi" don't belong in a crate the CLI also
/// depends on).
pub type PathLabel = Cow<'static, str>;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathState {
    Unknown,
    Validated,
    /// 到達はしているがRTT/ロス/black hole検出が閾値を超えている状態。
    Degraded,
    Failed,
}

/// このプロセス内で監視中の全パスに何一つ`Validated`が無くなったことを
/// [`notify_if_no_viable_path`]が検知したときの通知。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathHealthEvent {
    NoViablePath,
}

/// 各[`PathLabel`]の状態を追跡する薄いトラッカー
/// (`multipath_transport.rs`の`PathBroker`の一般化)。実際にどのpathで
/// バイトを送るかは最終的に`noq::Connection`自身が選ぶが、
/// `Path::set_status()`でこちらから優先度のヒントを与える
/// (Available/Backupの切り替え)。`path_ids`は`noq::PathId` → labelの
/// 対応付けで、path確立時([`PathHealthTracker::register_path`])に記録し、
/// `PathEvent`/ヘルスチェックタスクが後から同じpathを引けるようにする。
#[derive(Clone, Default)]
pub struct PathHealthTracker {
    states: Arc<StdMutex<HashMap<PathLabel, PathState>>>,
    path_ids: Arc<StdMutex<HashMap<noq::PathId, PathLabel>>>,
}

impl PathHealthTracker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set(&self, label: PathLabel, state: PathState) {
        self.states.lock().unwrap().insert(label, state);
    }

    pub fn get(&self, label: &PathLabel) -> PathState {
        *self.states.lock().unwrap().get(label).unwrap_or(&PathState::Unknown)
    }

    pub fn any_validated(&self) -> bool {
        self.states.lock().unwrap().values().any(|s| *s == PathState::Validated)
    }

    /// `open_path`/初回接続が成功した直後に、noqが割り振った`PathId`と
    /// このトラッカー内のラベルを紐付ける。
    pub fn register_path(&self, path_id: noq::PathId, label: PathLabel) {
        self.path_ids.lock().unwrap().insert(path_id, label);
    }

    pub fn label_for(&self, path_id: noq::PathId) -> Option<PathLabel> {
        self.path_ids.lock().unwrap().get(&path_id).cloned()
    }
}

/// 現在Validatedなpathが1本も無くなった(＝手元のQUICコネクション視点で
/// 「応答が一切返ってこない」)ことを検知したら[`PathHealthEvent::NoViablePath`]を送る。
/// キャプティブポータル等はQUICから見れば100%ロスと区別が付かないため、OSの
/// キャプティブポータル検知より先にこちらで直接検知できる。Degraded/Abandoned
/// 遷移のたびに呼ばれる想定だが、`any_validated()`がtrueのままなら何もしないので
/// 連呼にはならない。
pub fn notify_if_no_viable_path(tracker: &PathHealthTracker, event_tx: &tokio::sync::mpsc::Sender<PathHealthEvent>) {
    if tracker.any_validated() {
        return;
    }
    warn!("path_health: no viable path left (all paths degraded/failed)");
    let _ = event_tx.try_send(PathHealthEvent::NoViablePath);
}

/// 直近の統計から、そのpathが健全とみなせるかを判定する純粋関数
/// (実ネットワーク不要でunit testできるようにここだけ切り出してある)。
/// `prev`が`None`(初回チェック)の場合は差分ベースの判定(ロス率・black hole増分)は
/// スキップし、RTTのみで判定する。
pub fn classify_path_health(prev: Option<&noq::PathStats>, curr: &noq::PathStats) -> bool {
    if curr.rtt > DEGRADED_RTT_THRESHOLD {
        return false;
    }
    if let Some(prev) = prev {
        let sent_delta = curr.udp_tx.datagrams.saturating_sub(prev.udp_tx.datagrams);
        let lost_delta = curr.lost_packets.saturating_sub(prev.lost_packets);
        if sent_delta > 0 && (lost_delta as f64 / sent_delta as f64) > DEGRADED_LOSS_RATIO {
            return false;
        }
        if curr.black_holes_detected > prev.black_holes_detected {
            return false;
        }
    }
    true
}

/// キャプティブポータル等の完全な無応答(100%ロス)検出用の純粋関数。noqの
/// `lost_packets`/`black_holes_detected`はconnection全体が輻輳制御的に送信を
/// 止めてしまうと増加が止まり、rtt推定も更新されず古い健全値のまま固まる
/// ([`classify_path_health`]のping駆動チェックだけでは検知できない)。一方
/// `udp_rx.datagrams`(受信側カウンタ)は極めて直接的な信号 — このチェック区間で
/// 何か送った(sent_delta > 0)のに何も受信していなければ、それだけで応答が一切
/// 無いことを意味する。ただし実ネットワークのジッタ(応答が[`PING_SETTLE_DELAY`]内に
/// 間に合わないだけ)でも単発では容易に真になるため、呼び出し側
/// ([`spawn_health_monitor`])で連続回数を要求すること。
pub fn has_zero_response(prev: Option<&noq::PathStats>, curr: &noq::PathStats) -> bool {
    let Some(prev) = prev else { return false };
    let sent_delta = curr.udp_tx.datagrams.saturating_sub(prev.udp_tx.datagrams);
    let recv_delta = curr.udp_rx.datagrams.saturating_sub(prev.udp_rx.datagrams);
    sent_delta > 0 && recv_delta == 0
}

/// `path.ping()`(PING frame送出)→ 少し待つ → `path.stats()`を`noq`のAPIだけで
/// 定期的に行い、閾値超過ならそのpathを`PathStatus::Backup`に格下げする
/// (他に`Available`なpathがあればnoq自身がそちらを優先して使う)。独自のping/pong
/// ワイヤープロトコルは作らない — `noq::Path`が既に持っている機能をそのまま使うだけ。
///
/// 返す[`tokio::task::JoinHandle`]をdropしてもタスクは止まらない(detachされたまま
/// 動き続ける) — 呼び出し元が明示的に止めたい場合は`abort()`すること。
pub fn spawn_health_monitor(
    conn: noq::Connection,
    path_id: noq::PathId,
    label: PathLabel,
    tracker: PathHealthTracker,
    event_tx: tokio::sync::mpsc::Sender<PathHealthEvent>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut prev_stats: Option<noq::PathStats> = None;
        let mut consecutive_healthy = 0u32;
        // `has_zero_response`は実ネットワークのジッタ(1回だけ応答がPING_SETTLE_DELAY内に
        // 間に合わなかった等)でも単発では簡単に真になる(実機検証で245ms RTT——閾値800msの
        // 範囲内——でも1回だけ「この区間は受信0」になるケースを確認済み)。そのため
        // `classify_path_health`のRTT/ロス率/black hole判定(Backup降格用、単発判定のまま)
        // とは別に、NoViablePath通知だけは連続ミスを要求してノイズを除去する。
        let mut consecutive_no_response = 0u32;
        loop {
            tokio::time::sleep(HEALTH_CHECK_INTERVAL).await;
            let Some(path) = conn.path(path_id) else { break };
            if path.ping().is_err() {
                break; // path はもう閉じている
            }
            tokio::time::sleep(PING_SETTLE_DELAY).await;
            let Some(path) = conn.path(path_id) else { break };
            let stats = path.stats();
            let zero_response = has_zero_response(prev_stats.as_ref(), &stats);
            // `zero_response`をそのまま`healthy`にも反映させる。そうしないと、完全な
            // 無応答下でも`classify_path_health`(RTT/ロス率/black hole)だけは「健全」と
            // 読み続けてしまい(statsが更新自体止まるため)、下のリカバリ判定が
            // `zero_response`側のDegraded降格と競合して即座にValidatedへ戻してしまう。
            let healthy = classify_path_health(prev_stats.as_ref(), &stats) && !zero_response;
            prev_stats = Some(stats);

            if zero_response {
                consecutive_no_response = consecutive_no_response.saturating_add(1);
                if consecutive_no_response >= NO_RESPONSE_CONSECUTIVE_CHECKS {
                    warn!("path_health: path {label:?} got zero responses for {consecutive_no_response} consecutive checks");
                    tracker.set(label.clone(), PathState::Degraded);
                    notify_if_no_viable_path(&tracker, &event_tx);
                }
            } else {
                consecutive_no_response = 0;
            }

            if healthy {
                consecutive_healthy = consecutive_healthy.saturating_add(1);
                if tracker.get(&label) == PathState::Degraded && consecutive_healthy >= RECOVERY_CONSECUTIVE_CHECKS {
                    info!("path_health: path {label:?} recovered, marking Available");
                    let _ = path.set_status(noq::PathStatus::Available);
                    tracker.set(label.clone(), PathState::Validated);
                }
            } else {
                consecutive_healthy = 0;
                if tracker.get(&label) != PathState::Degraded {
                    warn!("path_health: path {label:?} degraded (rtt={:?}), demoting to Backup", stats.rtt);
                    let _ = path.set_status(noq::PathStatus::Backup);
                    tracker.set(label.clone(), PathState::Degraded);
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // `noq::PathStats`/`UdpStats`は`#[non_exhaustive]`なので他クレートからは
    // 構造体リテラル(`..Default::default()`併用でも)で作れない。
    // `Default::default()`してからpubフィールドへ代入する形にする。

    fn stats_with_rtt(rtt: Duration) -> noq::PathStats {
        let mut stats = noq::PathStats::default();
        stats.rtt = rtt;
        stats
    }

    /// `recvd_datagrams`は受信側カウンタ(`udp_rx.datagrams`)。「送ったのに何も
    /// 受信していない」＝完全な無応答検出のテストに必要。
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

    #[test]
    fn low_rtt_first_check_is_healthy() {
        assert!(classify_path_health(None, &stats_with_rtt(Duration::from_millis(50))));
    }

    #[test]
    fn high_rtt_is_degraded() {
        assert!(!classify_path_health(None, &stats_with_rtt(Duration::from_millis(900))));
    }

    #[test]
    fn rtt_at_threshold_boundary_is_still_healthy() {
        assert!(classify_path_health(None, &stats_with_rtt(DEGRADED_RTT_THRESHOLD)));
    }

    #[test]
    fn high_loss_ratio_since_prev_check_is_degraded() {
        let prev = stats_with(Duration::from_millis(50), 100, 100, 0, 0);
        // 100 new datagrams sent, 30 lost => 30% loss ratio > 20% threshold
        let curr = stats_with(Duration::from_millis(50), 200, 170, 30, 0);
        assert!(!classify_path_health(Some(&prev), &curr));
    }

    #[test]
    fn low_loss_ratio_since_prev_check_is_healthy() {
        let prev = stats_with(Duration::from_millis(50), 100, 100, 0, 0);
        let curr = stats_with(Duration::from_millis(50), 200, 198, 2, 0); // 2%
        assert!(classify_path_health(Some(&prev), &curr));
    }

    #[test]
    fn new_black_hole_detection_is_degraded() {
        let prev = stats_with(Duration::from_millis(50), 0, 0, 0, 0);
        let curr = stats_with(Duration::from_millis(50), 0, 0, 0, 1);
        assert!(!classify_path_health(Some(&prev), &curr));
    }

    #[test]
    fn no_new_datagrams_sent_skips_loss_ratio_check() {
        // sent_delta == 0 (idle path) must not divide by zero / falsely flag as degraded.
        let prev = stats_with(Duration::from_millis(50), 100, 100, 5, 0);
        let curr = stats_with(Duration::from_millis(50), 100, 100, 5, 0);
        assert!(classify_path_health(Some(&prev), &curr));
    }

    /// 送ったのに何も受信していなければ、loss_ratio/black_holeがまだ増分に
    /// 反映されていなくても即座にunhealthyと判定できる — captive portal等の
    /// 「応答が一切返って来ない」状況の直接検出。
    #[test]
    fn sent_but_nothing_received_is_zero_response() {
        let prev = stats_with(Duration::from_millis(50), 100, 100, 0, 0);
        // sent 10 more datagrams, but udp_rx didn't move at all, and noq hasn't
        // (yet) counted these as lost_packets/black_holes — that's the whole point.
        let curr = stats_with(Duration::from_millis(50), 110, 100, 0, 0);
        assert!(has_zero_response(Some(&prev), &curr));
        // classify_path_health only looks at rtt/loss-ratio/black-holes, so on its
        // own this same scenario still reads as "healthy" (that's why callers need
        // has_zero_response as a separate, stricter signal — see spawn_health_monitor).
        assert!(classify_path_health(Some(&prev), &curr));
    }

    #[test]
    fn received_something_is_not_zero_response() {
        let prev = stats_with(Duration::from_millis(50), 100, 100, 0, 0);
        let curr = stats_with(Duration::from_millis(50), 110, 105, 0, 0);
        assert!(!has_zero_response(Some(&prev), &curr));
    }

    #[test]
    fn nothing_sent_is_not_zero_response() {
        // idle path (sent_delta == 0) must not be flagged as zero-response.
        let prev = stats_with(Duration::from_millis(50), 100, 100, 0, 0);
        let curr = stats_with(Duration::from_millis(50), 100, 100, 0, 0);
        assert!(!has_zero_response(Some(&prev), &curr));
    }

    #[test]
    fn first_check_is_never_zero_response() {
        assert!(!has_zero_response(None, &stats_with(Duration::from_millis(50), 5, 0, 0, 0)));
    }

    #[test]
    fn tracker_register_and_degraded_transition() {
        let tracker = PathHealthTracker::new();
        let label: PathLabel = Cow::Borrowed("primary");
        tracker.register_path(noq::PathId::ZERO, label.clone());
        tracker.set(label.clone(), PathState::Validated);

        assert_eq!(tracker.label_for(noq::PathId::ZERO), Some(label.clone()));
        assert_eq!(tracker.get(&label), PathState::Validated);

        tracker.set(label.clone(), PathState::Degraded);
        assert_eq!(tracker.get(&label), PathState::Degraded);
        // Degraded はValidatedではないので any_validated には数えない。
        assert!(!tracker.any_validated());
    }
}
