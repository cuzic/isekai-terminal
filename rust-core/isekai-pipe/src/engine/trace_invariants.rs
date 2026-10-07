//! テスト専用: `isekai-pipe serve`のEffect列の不変条件検査(docs/adr/0019-functional-core-effects.md §6 Step 11)。
//!
//! [`ServeAggregate::apply`]の1回ごとに、入力Eventと出力Effectを**秘密を含まない形**
//! ([`ServeStep`]: variant名とleaseの番号、apply前に判定した「現行か」のフラグだけ)へ写して列にし、
//! 純粋な検査関数[`check_serve_trace`]に通す。記録点は`serve_fsm`のproptest(`run_ops`)と、
//! shellの`AttachRuntime::apply_with`(`#[cfg(test)]`のフック)。engineの既存テスト
//! (`attach_runtime`を通るもの)はすべて自動的にこの検査も受ける。本番ビルドには含まれない。
//!
//! ## 秘密情報(§3-3)
//!
//! `AttachToken`(`TargetConnected`/`Activated`/`SendReady`が運ぶ)とsession id(RESUMEで
//! セッションを引く鍵)は記録しない。leaseは全session間で一意な番号なので、それだけで足りる。
//!
//! ## 不変条件(T3、§4.1「破棄経路で必ずfencing slotが解放されること」のEffect列版)
//!
//! `ServeEffect::Discard`はindexエントリの除去とarbiter slotの解放(`RelayEnded`)を同じapplyで
//! 行ったことの報告(`serve_fsm`の「唯一の除去経路」)なので、Discardの回数=slot解放の回数として
//! 次を検査する。
//!
//! - **D1**: `Discard{lease}`は`RegisterIo{lease}`で登録された(=Establishedになった)leaseに
//!   対してだけ、**高々1回**(二重解放・未登録の解放は違反)。
//! - **D2**: 登録済みで未破棄のleaseに対する`RelayEnded{lease}`、現行incarnationに対する
//!   `RelayTerminated`は、**同じapplyで**`Discard{lease}`を返す(破棄経路の解放漏れは違反)。
//! - **D3**: `Discard`後のleaseに対して、ソケットを扱うEffect(`RegisterIo`/`StoreParked`/
//!   `ResumeGranted`/`RequestPreempt`/`StartRelay`)を返さない。
//! - **D4**: `StartRelay{lease}`は同一leaseに高々1回(Step 1の不変条件のshell側の確認)。
//! - **D5(stale timer)**: 非現行leaseの`PendingExpired`(pending-activationタイマーの満了)は
//!   Effectを1つも返さない(§2.2の必須プロパティの列版)。
//!
//! どれも接頭辞について閉じているので、[`ServeTraceRecorder`]は記録のたびに逐次検査し、
//! `Drop`でも再検査する(テスト自体のpanic中は省く)。

use std::collections::BTreeSet;
use std::fmt;
use std::sync::Mutex as StdMutex;

use super::attach_arbiter::{AttachEffect, LeaseId};
use super::serve_fsm::{ServeAggregate, ServeEffect, ServeEvent};

/// applyの入力(秘密を除いた形)。`current`はapply**前**の状態で判定する。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ServeTraceEvent {
    RelayEnded { lease: u64 },
    /// `current`: `(id, lease)`がそのidの現incarnationか。
    RelayTerminated { lease: u64, current: bool },
    /// `current`: `lease`が現在`PendingActivation`のleaseか(=タイマーのtokenが現行か)。
    PendingExpired { lease: u64, current: bool },
    Other(&'static str),
}

/// applyの出力1件(秘密を除いた形)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ServeTraceEffect {
    RegisterIo { lease: u64 },
    Discard { lease: u64 },
    StartRelay { lease: u64 },
    /// `StoreParked`/`ResumeGranted`/`RequestPreempt`(既存incarnationのソケットを扱う)。
    Touch { lease: u64, what: &'static str },
    Other(&'static str),
}

/// 1回のapply。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ServeStep {
    pub(crate) event: ServeTraceEvent,
    pub(crate) effects: Vec<ServeTraceEffect>,
}

/// apply**前**に呼ぶ: Eventを記録用の形へ写す(現行かどうかは今の`agg`で判定する)。
pub(crate) fn observe_event(agg: &ServeAggregate, event: &ServeEvent) -> ServeTraceEvent {
    match *event {
        ServeEvent::RelayEnded { lease } => ServeTraceEvent::RelayEnded { lease: lease.raw_for_trace() },
        ServeEvent::RelayTerminated { id, lease, .. } => ServeTraceEvent::RelayTerminated {
            lease: lease.raw_for_trace(),
            current: agg.index_entry(&id).map(|e| e.lease) == Some(lease),
        },
        ServeEvent::PendingExpired { lease } => ServeTraceEvent::PendingExpired {
            lease: lease.raw_for_trace(),
            current: agg.arbiter().is_pending_activation_lease(lease),
        },
        ServeEvent::AdmitRequested { .. } => ServeTraceEvent::Other("AdmitRequested"),
        ServeEvent::TargetConnected { .. } => ServeTraceEvent::Other("TargetConnected"),
        ServeEvent::TargetConnectFailed { .. } => ServeTraceEvent::Other("TargetConnectFailed"),
        ServeEvent::CancelReceived { .. } => ServeTraceEvent::Other("CancelReceived"),
        ServeEvent::LeaseStopped { .. } => ServeTraceEvent::Other("LeaseStopped"),
        ServeEvent::Activated { .. } => ServeTraceEvent::Other("Activated"),
        ServeEvent::Parked { .. } => ServeTraceEvent::Other("Parked"),
        ServeEvent::ResumeUnavailable { .. } => ServeTraceEvent::Other("ResumeUnavailable"),
        ServeEvent::Sweep { .. } => ServeTraceEvent::Other("Sweep"),
        ServeEvent::ResumeRequested { .. } => ServeTraceEvent::Other("ResumeRequested"),
    }
}

fn lease_of(lease: LeaseId) -> u64 {
    lease.raw_for_trace()
}

/// Effect列を記録用の形へ写す。
pub(crate) fn observe_effects(effects: &[ServeEffect]) -> Vec<ServeTraceEffect> {
    effects
        .iter()
        .map(|effect| match *effect {
            ServeEffect::RegisterIo { lease, .. } => ServeTraceEffect::RegisterIo { lease: lease_of(lease) },
            ServeEffect::Discard { lease, .. } => ServeTraceEffect::Discard { lease: lease_of(lease) },
            ServeEffect::StoreParked { lease, .. } => ServeTraceEffect::Touch { lease: lease_of(lease), what: "StoreParked" },
            ServeEffect::ResumeGranted { lease, .. } => {
                ServeTraceEffect::Touch { lease: lease_of(lease), what: "ResumeGranted" }
            }
            ServeEffect::RequestPreempt { lease, .. } => {
                ServeTraceEffect::Touch { lease: lease_of(lease), what: "RequestPreempt" }
            }
            ServeEffect::ResumeRejected { .. } => ServeTraceEffect::Other("ResumeRejected"),
            ServeEffect::Attach(AttachEffect::StartRelay { lease, .. }) => {
                ServeTraceEffect::StartRelay { lease: lease_of(lease) }
            }
            ServeEffect::Attach(AttachEffect::ConnectTarget { .. }) => ServeTraceEffect::Other("ConnectTarget"),
            ServeEffect::Attach(AttachEffect::CancelLease { .. }) => ServeTraceEffect::Other("CancelLease"),
            ServeEffect::Attach(AttachEffect::SendReady { .. }) => ServeTraceEffect::Other("SendReady"),
            ServeEffect::Attach(AttachEffect::SendReject { .. }) => ServeTraceEffect::Other("SendReject"),
            ServeEffect::Attach(AttachEffect::SchedulePendingTimeout { .. }) => {
                ServeTraceEffect::Other("SchedulePendingTimeout")
            }
        })
        .collect()
}

/// 不変条件違反。`at`は違反したapplyの列内の位置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ServeTraceViolation {
    /// D1: 既に破棄したleaseを再び`Discard`した(slotの二重解放)。
    DoubleDiscard { at: usize, lease: u64 },
    /// D1: 登録(`RegisterIo`)されていないleaseを`Discard`した。
    DiscardOfUnregistered { at: usize, lease: u64 },
    /// D2: 破棄経路(`RelayEnded`/現行の`RelayTerminated`)が同じapplyで`Discard`を返さなかった。
    MissingDiscard { at: usize, lease: u64, event: ServeTraceEvent },
    /// D3: 破棄済みleaseのソケットを扱うEffectを返した。
    EffectOnDiscardedLease { at: usize, lease: u64, what: &'static str },
    /// D4: 同一leaseへの2回目の`StartRelay`。
    DoubleStartRelay { at: usize, lease: u64 },
    /// D5: 非現行leaseの`PendingExpired`がEffectを返した。
    StaleTimerProducedEffects { at: usize, lease: u64, effects: usize },
}

impl fmt::Display for ServeTraceViolation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DoubleDiscard { at, lease } => write!(f, "#{at}: lease {lease}を2回Discardした(slotの二重解放)"),
            Self::DiscardOfUnregistered { at, lease } => {
                write!(f, "#{at}: RegisterIoされていないlease {lease}をDiscardした")
            }
            Self::MissingDiscard { at, lease, event } => {
                write!(f, "#{at}: {event:?}が同じapplyでlease {lease}のDiscard(slot解放)を返さなかった")
            }
            Self::EffectOnDiscardedLease { at, lease, what } => {
                write!(f, "#{at}: 破棄済みlease {lease}に{what}を返した")
            }
            Self::DoubleStartRelay { at, lease } => write!(f, "#{at}: lease {lease}へ2回目のStartRelay"),
            Self::StaleTimerProducedEffects { at, lease, effects } => {
                write!(f, "#{at}: 非現行lease {lease}のPendingExpiredが{effects}個のEffectを返した")
            }
        }
    }
}

/// [`check_serve_trace`]の逐次版。
#[derive(Debug, Default, Clone)]
pub(crate) struct ServeTraceChecker {
    at: usize,
    live: BTreeSet<u64>,
    discarded: BTreeSet<u64>,
    started: BTreeSet<u64>,
}

impl ServeTraceChecker {
    pub(crate) fn step(&mut self, step: &ServeStep) -> Result<(), ServeTraceViolation> {
        let at = self.at;
        self.at += 1;
        let live_before = self.live.clone();
        for effect in &step.effects {
            match *effect {
                ServeTraceEffect::RegisterIo { lease } => {
                    if self.discarded.contains(&lease) {
                        return Err(ServeTraceViolation::EffectOnDiscardedLease { at, lease, what: "RegisterIo" });
                    }
                    self.live.insert(lease);
                }
                ServeTraceEffect::Discard { lease } => {
                    if self.discarded.contains(&lease) {
                        return Err(ServeTraceViolation::DoubleDiscard { at, lease });
                    }
                    if !self.live.remove(&lease) {
                        return Err(ServeTraceViolation::DiscardOfUnregistered { at, lease });
                    }
                    self.discarded.insert(lease);
                }
                ServeTraceEffect::StartRelay { lease } => {
                    if self.discarded.contains(&lease) {
                        return Err(ServeTraceViolation::EffectOnDiscardedLease { at, lease, what: "StartRelay" });
                    }
                    if !self.started.insert(lease) {
                        return Err(ServeTraceViolation::DoubleStartRelay { at, lease });
                    }
                }
                ServeTraceEffect::Touch { lease, what } => {
                    if self.discarded.contains(&lease) {
                        return Err(ServeTraceViolation::EffectOnDiscardedLease { at, lease, what });
                    }
                }
                ServeTraceEffect::Other(_) => {}
            }
        }
        let discarded_here = |lease: u64| step.effects.contains(&ServeTraceEffect::Discard { lease });
        match step.event {
            ServeTraceEvent::RelayEnded { lease } if live_before.contains(&lease) && !discarded_here(lease) => {
                return Err(ServeTraceViolation::MissingDiscard { at, lease, event: step.event });
            }
            ServeTraceEvent::RelayTerminated { lease, current: true } if !discarded_here(lease) => {
                return Err(ServeTraceViolation::MissingDiscard { at, lease, event: step.event });
            }
            ServeTraceEvent::PendingExpired { lease, current: false } if !step.effects.is_empty() => {
                return Err(ServeTraceViolation::StaleTimerProducedEffects { at, lease, effects: step.effects.len() });
            }
            ServeTraceEvent::RelayEnded { .. }
            | ServeTraceEvent::RelayTerminated { .. }
            | ServeTraceEvent::PendingExpired { .. }
            | ServeTraceEvent::Other(_) => {}
        }
        Ok(())
    }
}

/// 純粋な検査関数(ADR §6 Step 11の`check_trace`のengine版)。
pub(crate) fn check_serve_trace(trace: &[ServeStep]) -> Result<(), ServeTraceViolation> {
    let mut checker = ServeTraceChecker::default();
    trace.iter().try_for_each(|step| checker.step(step))
}

#[derive(Default)]
struct RecorderInner {
    steps: Vec<ServeStep>,
    checker: ServeTraceChecker,
    violation: Option<ServeTraceViolation>,
}

/// `AttachRuntime`(テストビルドのみ)が持つ記録器。記録のたびに逐次検査して違反時にpanicし、
/// `Drop`でも再検査する(別タスクでpanicが握りつぶされた場合のバックストップ)。
#[derive(Default)]
pub(crate) struct ServeTraceRecorder {
    inner: StdMutex<RecorderInner>,
}

impl ServeTraceRecorder {
    /// `observed`は同じapplyの前に[`observe_event`]で取ったもの。
    pub(crate) fn record(&self, observed: ServeTraceEvent, effects: &[ServeEffect]) {
        let step = ServeStep { event: observed, effects: observe_effects(effects) };
        let failure = {
            let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            let result = if inner.violation.is_some() { Ok(()) } else { inner.checker.step(&step) };
            inner.steps.push(step);
            match result {
                Ok(()) => None,
                Err(v) => {
                    inner.violation = Some(v.clone());
                    Some((v, inner.steps.clone()))
                }
            }
        };
        if let Some((v, steps)) = failure {
            panic!("Step 11 serve trace invariant violated: {v}\ntrace: {steps:?}");
        }
    }
}

impl Drop for ServeTraceRecorder {
    fn drop(&mut self) {
        if std::thread::panicking() {
            return;
        }
        let inner = self.inner.get_mut().unwrap_or_else(|e| e.into_inner());
        if let Err(v) = check_serve_trace(&inner.steps) {
            panic!("Step 11 serve trace invariant violated (detected at drop): {v}\ntrace: {:?}", inner.steps);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ServeTraceEffect::{Discard, Other, RegisterIo, StartRelay, Touch};
    use super::*;

    fn step(event: ServeTraceEvent, effects: Vec<ServeTraceEffect>) -> ServeStep {
        ServeStep { event, effects }
    }

    fn activated(lease: u64) -> ServeStep {
        step(ServeTraceEvent::Other("Activated"), vec![RegisterIo { lease }, StartRelay { lease }])
    }

    #[test]
    fn a_well_formed_lifecycle_passes() {
        let trace = [
            activated(1),
            step(ServeTraceEvent::Other("Parked"), vec![Touch { lease: 1, what: "StoreParked" }]),
            step(ServeTraceEvent::Other("ResumeRequested"), vec![Touch { lease: 1, what: "ResumeGranted" }]),
            step(ServeTraceEvent::RelayEnded { lease: 1 }, vec![Discard { lease: 1 }]),
            // 破棄済みleaseの遅延RelayEnded/非現行のRelayTerminatedは何も返さなくてよい。
            step(ServeTraceEvent::RelayEnded { lease: 1 }, vec![]),
            step(ServeTraceEvent::RelayTerminated { lease: 1, current: false }, vec![]),
            activated(2),
            step(ServeTraceEvent::Other("Sweep"), vec![Discard { lease: 2 }]),
            step(ServeTraceEvent::PendingExpired { lease: 3, current: true }, vec![Other("CancelLease")]),
            step(ServeTraceEvent::PendingExpired { lease: 3, current: false }, vec![]),
        ];
        assert_eq!(check_serve_trace(&trace), Ok(()));
    }

    #[test]
    fn checker_fails_on_a_missing_relay_ended_discard() {
        let trace = [activated(1), step(ServeTraceEvent::RelayEnded { lease: 1 }, vec![])];
        assert_eq!(
            check_serve_trace(&trace),
            Err(ServeTraceViolation::MissingDiscard { at: 1, lease: 1, event: ServeTraceEvent::RelayEnded { lease: 1 } })
        );
        let trace = [activated(1), step(ServeTraceEvent::RelayTerminated { lease: 1, current: true }, vec![])];
        assert!(matches!(check_serve_trace(&trace), Err(ServeTraceViolation::MissingDiscard { at: 1, lease: 1, .. })));
    }

    #[test]
    fn checker_fails_on_a_double_discard() {
        let trace = [
            activated(1),
            step(ServeTraceEvent::Other("Sweep"), vec![Discard { lease: 1 }]),
            step(ServeTraceEvent::Other("AdmitRequested"), vec![Discard { lease: 1 }]),
        ];
        assert_eq!(check_serve_trace(&trace), Err(ServeTraceViolation::DoubleDiscard { at: 2, lease: 1 }));
    }

    #[test]
    fn checker_fails_on_a_discard_of_an_unregistered_lease() {
        let trace = [step(ServeTraceEvent::Other("Sweep"), vec![Discard { lease: 7 }])];
        assert_eq!(check_serve_trace(&trace), Err(ServeTraceViolation::DiscardOfUnregistered { at: 0, lease: 7 }));
    }

    #[test]
    fn checker_fails_on_an_effect_for_a_discarded_lease() {
        let trace = [
            activated(1),
            step(ServeTraceEvent::RelayEnded { lease: 1 }, vec![Discard { lease: 1 }]),
            step(ServeTraceEvent::Other("ResumeRequested"), vec![Touch { lease: 1, what: "ResumeGranted" }]),
        ];
        assert_eq!(
            check_serve_trace(&trace),
            Err(ServeTraceViolation::EffectOnDiscardedLease { at: 2, lease: 1, what: "ResumeGranted" })
        );
    }

    #[test]
    fn checker_fails_on_a_double_start_relay() {
        let trace = [activated(1), step(ServeTraceEvent::Other("Activated"), vec![StartRelay { lease: 1 }])];
        assert_eq!(check_serve_trace(&trace), Err(ServeTraceViolation::DoubleStartRelay { at: 1, lease: 1 }));
    }

    #[test]
    fn checker_fails_on_a_stale_pending_timer_that_produced_effects() {
        let trace = [step(ServeTraceEvent::PendingExpired { lease: 4, current: false }, vec![Other("CancelLease")])];
        assert_eq!(
            check_serve_trace(&trace),
            Err(ServeTraceViolation::StaleTimerProducedEffects { at: 0, lease: 4, effects: 1 })
        );
    }

    #[test]
    fn real_aggregate_lifecycle_is_recorded_and_passes() {
        // 実際のreducerを通した列(Activated→RelayEnded)が記録器を通ること(記録点の配線の確認)。
        use isekai_protocol::attach::{AttachKey, AttachToken, AttemptId, ConnectionGeneration};
        use isekai_protocol::SessionId;
        let recorder = ServeTraceRecorder::default();
        let mut agg = ServeAggregate::new(4);
        let apply = |agg: &mut ServeAggregate, ev: ServeEvent| {
            let observed = observe_event(agg, &ev);
            let effects = agg.apply(ev);
            recorder.record(observed, &effects);
            effects
        };
        let key = AttachKey {
            session_id: SessionId::from_bytes([1; 16]),
            generation: ConnectionGeneration::new(1),
            attempt_id: AttemptId::from_bytes([1; 16]),
        };
        let lease = apply(&mut agg, ServeEvent::AdmitRequested { key })
            .into_iter()
            .find_map(|e| match e {
                ServeEffect::Attach(AttachEffect::ConnectTarget { lease }) => Some(lease),
                _ => None,
            })
            .expect("ConnectTarget");
        let token = AttachToken::new([9; isekai_protocol::attach::ATTACH_TOKEN_LEN]);
        apply(
            &mut agg,
            ServeEvent::TargetConnected { lease, target: super::super::attach_arbiter::TargetHandleId(0), attach_token: token },
        );
        apply(&mut agg, ServeEvent::Activated { key, attach_token: token, negotiated_grace_secs: None });
        let ended = apply(&mut agg, ServeEvent::RelayEnded { lease });
        assert!(matches!(ended.as_slice(), [ServeEffect::Discard { .. }]), "RelayEndedの出力: {ended:?}");
        // 非現行になったleaseのPendingExpiredは何も返さない(D5)。
        assert!(apply(&mut agg, ServeEvent::PendingExpired { lease }).is_empty());
        let steps = recorder.inner.lock().unwrap_or_else(|e| e.into_inner()).steps.clone();
        assert_eq!(steps.len(), 5);
        assert_eq!(check_serve_trace(&steps), Ok(()));
    }
}
