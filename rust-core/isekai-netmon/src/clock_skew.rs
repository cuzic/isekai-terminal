//! Platform-independent "the host was suspended/asleep and just resumed"
//! detector, merged into every platform's [`crate::system_monitor`] result
//! alongside the interface-change backend (`windows.rs`/`macos.rs`/
//! `linux.rs`).
//!
//! Why this needs to exist at all: `isekai-transport`'s QUIC connections use
//! an idle-timeout/keepalive (`isekai-transport::system`) to notice a dead
//! peer, timed against `std::time::Instant` (`CLOCK_MONOTONIC` on Linux).
//! That clock is documented to exclude time the host spent suspended, while
//! wall-clock time (`SystemTime`/`CLOCK_REALTIME`) does not. So across a
//! real suspend, the idle-timeout logic can undercount how much real time
//! actually passed — right when it matters most, since the peer (a relay
//! server that was never suspended) has had the *full* wall-clock gap to
//! decide the connection is dead and discard the parked session
//! (`PLAN.md`'s Phase 0-5 records the same concern on iOS;
//! `.claude/rules/always-connects.md` is why this project treats "recovery
//! requires a manual command" as a bug). `isekai-transport::resume`'s own
//! `TRANSPORT_STEP_TIMEOUT` bounds the damage once a reconnect is attempted,
//! but doesn't make the attempt happen any sooner — this watchdog is what
//! does that, the same way the interface-change backends make a Wi-Fi drop
//! trigger an early reconnect instead of waiting out the idle timeout blind.
//!
//! No OS suspend/resume API (Linux `systemd-logind` D-Bus `PrepareForSleep`,
//! Windows `WM_POWERBROADCAST`, macOS `NSWorkspace` sleep/wake
//! notifications) is used here on purpose: `isekai-ssh`/`isekai-pipe
//! connect` is a short-lived CLI process, not a long-running GUI
//! application, so none of those integrate cleanly (D-Bus pulls in a
//! dependency that doesn't play well with this project's musl static
//! builds and is simply absent in containers/WSL; `WM_POWERBROADCAST`
//! needs a hidden window and a message loop; `NSWorkspace` notifications
//! need a run loop, both awkward to host in a CLI with no GUI event loop of
//! its own). Comparing a monotonic clock against a wall clock needs neither
//! an OS registration nor a new dependency, behaves identically on every
//! platform (including ones with no interface-change backend at all), and
//! degrades to "no false positives, just a slightly later reconnect" rather
//! than "no signal at all" if the assumption about monotonic clocks turns
//! out not to hold on some platform.

use std::time::Duration;

use async_trait::async_trait;
use tokio::time::Instant;

use crate::{NetworkChangeCause, NetworkChangeEvent, NetworkChangeMonitor};

/// How often to compare the two clocks. Short enough that a real suspend is
/// noticed promptly after resume (the next tick just picks up wherever the
/// process left off — there's nothing to "miss" while suspended, since
/// nothing runs then), long enough to be negligible background CPU cost for
/// a process that's normally alive for an entire interactive SSH session.
const TICK_INTERVAL: Duration = Duration::from_secs(1);

/// How much further the wall clock is allowed to run ahead of the monotonic
/// clock across one tick before this is treated as a suspend rather than
/// ordinary scheduling jitter. Generous relative to realistic jitter (OS
/// scheduling delay under load is normally sub-second even when this
/// sandbox is busy with concurrent agents — see the
/// `robolectric-test-flakiness-under-load` class of issue) while still far
/// shorter than `isekai-transport`'s 15s QUIC idle-timeout, so this fires
/// meaningfully earlier than that fallback would.
const SUSPEND_GAP_THRESHOLD: Duration = Duration::from_secs(5);

/// Pure decision core: given how much the monotonic and wall clocks each
/// advanced across the same real interval, does the gap between them look
/// like the host was suspended for a while rather than ordinary scheduling
/// jitter? Extracted so it's testable without any real clock or `sleep`
/// (matching this codebase's usual pattern of separating decision logic
/// from real I/O, e.g. `isekai-pipe`'s `update_unknown_session_streak`).
///
/// `wall_elapsed` is expected to be `>= mono_elapsed` in the ordinary case
/// (a monotonic clock can only run slower than wall-clock time, never
/// faster, suspend or not) — this only asks whether the *gap* between them
/// exceeds `threshold`, not which one is "correct".
fn suspend_gap_detected(mono_elapsed: Duration, wall_elapsed: Duration, threshold: Duration) -> bool {
    wall_elapsed > mono_elapsed.saturating_add(threshold)
}

/// [`NetworkChangeMonitor`] that never talks to the OS: it just samples
/// `tokio::time::Instant::now()` (monotonic) and `SystemTime::now()`
/// (wall-clock) once per [`TICK_INTERVAL`] and yields a
/// [`NetworkChangeCause::Wake`] event the first time
/// [`suspend_gap_detected`] says the gap since the previous tick looks like
/// a suspend. See this module's docs for why comparing clocks, rather than
/// an OS suspend/resume notification, is this crate's approach.
pub struct ClockSkewWatchdog {
    tick_interval: Duration,
    threshold: Duration,
    last_mono: Instant,
    last_wall: std::time::SystemTime,
}

impl ClockSkewWatchdog {
    pub fn new() -> Self {
        Self::with_params(TICK_INTERVAL, SUSPEND_GAP_THRESHOLD)
    }

    fn with_params(tick_interval: Duration, threshold: Duration) -> Self {
        Self { tick_interval, threshold, last_mono: Instant::now(), last_wall: std::time::SystemTime::now() }
    }
}

impl Default for ClockSkewWatchdog {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl NetworkChangeMonitor for ClockSkewWatchdog {
    async fn next_change(&mut self) -> Option<NetworkChangeEvent> {
        loop {
            tokio::time::sleep(self.tick_interval).await;

            let now_mono = Instant::now();
            let now_wall = std::time::SystemTime::now();
            let mono_elapsed = now_mono.saturating_duration_since(self.last_mono);
            // A backward wall-clock jump (NTP step, manual clock change)
            // isn't a suspend — `unwrap_or(ZERO)` treats it as "no gap",
            // never as a suspend signal.
            let wall_elapsed = now_wall.duration_since(self.last_wall).unwrap_or(Duration::ZERO);
            self.last_mono = now_mono;
            self.last_wall = now_wall;

            if suspend_gap_detected(mono_elapsed, wall_elapsed, self.threshold) {
                log::info!(
                    "isekai-netmon: wall clock ran {wall_elapsed:?} ahead of the monotonic clock over \
                     one {:?} tick (threshold {:?}) — treating this as a host suspend/resume and \
                     signaling an early reconnect",
                    self.tick_interval,
                    self.threshold,
                );
                return Some(NetworkChangeEvent { cause: NetworkChangeCause::Wake });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_ticking_is_not_a_suspend_gap() {
        assert!(!suspend_gap_detected(Duration::from_secs(1), Duration::from_secs(1), Duration::from_secs(5)));
    }

    #[test]
    fn wall_clock_slightly_ahead_within_threshold_is_not_a_suspend_gap() {
        // Ordinary scheduling jitter: the wall clock can run a little ahead
        // of the monotonic one even with no suspend involved.
        assert!(!suspend_gap_detected(Duration::from_millis(900), Duration::from_secs(1), Duration::from_secs(5)));
    }

    #[test]
    fn wall_clock_far_ahead_is_a_suspend_gap() {
        // A 10-minute suspend: the monotonic clock only advanced this
        // tick's ordinary ~1s, the wall clock advanced the full gap.
        assert!(suspend_gap_detected(Duration::from_secs(1), Duration::from_secs(601), Duration::from_secs(5)));
    }

    #[test]
    fn exactly_at_threshold_is_not_yet_a_suspend_gap() {
        assert!(!suspend_gap_detected(Duration::ZERO, Duration::from_secs(5), Duration::from_secs(5)));
    }

    #[test]
    fn just_past_threshold_is_a_suspend_gap() {
        assert!(suspend_gap_detected(Duration::ZERO, Duration::from_secs(5) + Duration::from_millis(1), Duration::from_secs(5)));
    }

    #[test]
    fn monotonic_never_runs_ahead_of_wall_clock_in_practice_but_would_not_false_positive_if_it_did() {
        assert!(!suspend_gap_detected(Duration::from_secs(10), Duration::from_secs(1), Duration::from_secs(5)));
    }

    /// Real-clock smoke test, deliberately not asserting a `Wake` ever
    /// fires (this sandbox cannot trigger a real OS suspend) — only that
    /// running the actual watchdog for several real ticks under whatever
    /// CPU contention this environment happens to have right now does
    /// *not* false-positive. `threshold` here is intentionally the same as
    /// production, not shrunk, so this only proves the property this
    /// module actually promises: a live system without a suspend doesn't
    /// spuriously drift more than `SUSPEND_GAP_THRESHOLD` between the two
    /// clocks.
    #[tokio::test]
    async fn does_not_false_positive_over_several_real_ticks_under_no_suspend() {
        let mut watchdog = ClockSkewWatchdog::with_params(Duration::from_millis(50), SUSPEND_GAP_THRESHOLD);
        tokio::select! {
            ev = watchdog.next_change() => panic!("must not report a suspend with no real suspend involved, got {ev:?}"),
            _ = tokio::time::sleep(Duration::from_millis(400)) => {}
        }
    }

    #[tokio::test]
    async fn fires_once_the_pure_condition_is_synthetically_met() {
        // Can't simulate a real suspend, but can prove the wiring calls
        // `suspend_gap_detected` correctly by shrinking the threshold below
        // what a real tick's wall-clock/monotonic-clock gap will exceed
        // even without any suspend (ordinary tokio scheduling overhead is
        // enough once the threshold is this tight).
        let mut watchdog = ClockSkewWatchdog::with_params(Duration::from_millis(10), Duration::ZERO);
        let ev = tokio::time::timeout(Duration::from_secs(5), watchdog.next_change())
            .await
            .expect("must fire well before the test timeout with threshold=0")
            .expect("must yield Some, not a permanent stop");
        assert_eq!(ev.cause, NetworkChangeCause::Wake);
    }
}
