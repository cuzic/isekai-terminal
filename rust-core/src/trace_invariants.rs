//! テスト専用: Effect/callback列の不変条件検査(ADR_FUNCTIONAL_CORE_EFFECTS.md §6 Step 11)。
//!
//! テストが自分で起こしたcallback(`on_connection_state_changed`/`on_connection_edge`)や
//! reducerのEffectを、**テスト内メモリ上の列**([`TraceEvent`])として記録し、純粋な検査関数
//! [`check_trace`]に通す。`lib.rs`で`#[cfg(test)]`付きで宣言されているので、本番ビルドには
//! 一切含まれない(本番コードに記録機構を入れず、ファイルにも書き出さない。§2.6/Q12の
//! 「実機記録+replay」とは別物)。
//!
//! ## 秘密情報(§3-3)
//!
//! [`TraceEvent`]はvariant名・世代/token・公開状態のタグだけを持つ。`SshConfig`/`SshAuth`/
//! `LastConnectAttempt`は参照せず、`Connected{host}`/`Established{host}`のhost文字列や
//! 切断理由の文字列も記録しない(検査に不要)。
//!
//! ## 不変条件
//!
//! - **T1/T2(Step 8a′のエッジ契約)**: `Established(g)`の後、次の`Established(g'>g)`より前に
//!   `Lost(g)`が正確に1回。`Established`の世代は単調増加(=各世代について高々1回)、`Lost(g)`は
//!   開いている`Established(g)`に対してだけ出る(二重`Lost`・`Lost`→`Established`の逆転は違反)。
//!   配信は`PublicationQueue`がreducerの適用順に直列化している(PR #167レビューL-1)ので、
//!   スレッドをまたいでも順序まで検査できる。列の末尾でedgeが開いたままなのは許す(テストが
//!   接続中のまま終わることがある)。
//! - **状態公開の単調性**: 世代`g`の`Connected`(同じapplyで`Established(g)`が続く)の後、
//!   `Lost(g)`より前に`Reconnecting`が届いてはいけない(=前の再接続ループの古い`Reconnecting`が、
//!   同じ世代の`Connected`の後に届く逆転)。「edgeが開いている間の`Reconnecting`」として検査する:
//!   edgeを伴わない`Connected`の再公開(同一世代の`on_connected`重複等)の後の`Reconnecting`は、
//!   列だけからは新しい切断によるものと区別できないので対象にしない。
//! - **stale timer(§2.2の必須プロパティのshell/列版)**: 非現行tokenの`TimerFired`
//!   (再接続ループのtick/wake、poolの`IdleExpired`等)はEffectを1つも生まない。
//!
//! すべての不変条件は**接頭辞について閉じている**(違反は列のある時点で確定し、後から
//! 解消されない)ので、[`TraceRecorder`]は記録のたびに逐次検査し、違反した時点でpanicする
//! (テストスレッド上ならその場でテストが落ちる)。別スレッドで起きてpanicが握りつぶされた
//! 場合に備え、記録器の`Drop`でも検査する(テスト自体のpanic中は二重panicを避けて省く)。

use std::fmt;
use std::sync::Mutex as StdMutex;

use crate::{ConnectionEdge, ConnectionPublicState};

/// 公開状態のタグ(中身の文字列は持たない、§3-3)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StateTag {
    Disconnected,
    Connecting,
    Connected,
    Error,
    Reconnecting,
}

impl From<&ConnectionPublicState> for StateTag {
    fn from(state: &ConnectionPublicState) -> Self {
        match state {
            ConnectionPublicState::Disconnected { .. } => StateTag::Disconnected,
            ConnectionPublicState::Connecting => StateTag::Connecting,
            ConnectionPublicState::Connected { .. } => StateTag::Connected,
            ConnectionPublicState::Error { .. } => StateTag::Error,
            ConnectionPublicState::Reconnecting { .. } => StateTag::Reconnecting,
        }
    }
}

/// 記録する1件。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TraceEvent {
    /// `on_connection_state_changed`(または`Publish*` Effect)。
    State(StateTag),
    /// `on_connection_edge(Established, g)`(または`EdgeEstablished{g}` Effect)。
    Established(u64),
    /// `on_connection_edge(Lost, g)`(または`EdgeLost{g}` Effect)。
    Lost(u64),
    /// tokenを運ぶタイマー満了Eventを1回applyした結果。`current`はapply**前**の現行token、
    /// `effects`はそのapplyが返したEffectの数。
    TimerFired { token: u64, current: u64, effects: usize },
}

impl TraceEvent {
    pub(crate) fn edge(edge: &ConnectionEdge, generation: u64) -> Self {
        match edge {
            ConnectionEdge::Established { .. } => TraceEvent::Established(generation),
            ConnectionEdge::Lost => TraceEvent::Lost(generation),
        }
    }
}

/// 不変条件違反。`at`は違反した記録の列内の位置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TraceViolation {
    /// `Established(open)`の`Lost`より前に`Established(new)`が来た。
    EstablishedWhileOpen { at: usize, open: u64, new: u64 },
    /// `Established`の世代が単調増加でない(同じ世代の再送を含む)。
    EstablishedNotMonotonic { at: usize, last: u64, new: u64 },
    /// 開いている`Established(generation)`の無い`Lost(generation)`(二重`Lost`・逆転・世代違い)。
    LostWithoutEstablished { at: usize, generation: u64, open: Option<u64> },
    /// `Established(generation)`(=同世代の`Connected`)の後、`Lost(generation)`より前に`Reconnecting`が届いた。
    StaleReconnecting { at: usize, generation: u64 },
    /// 非現行tokenのタイマー満了がEffectを生んだ。
    StaleTimerProducedEffects { at: usize, token: u64, current: u64, effects: usize },
}

impl fmt::Display for TraceViolation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TraceViolation::EstablishedWhileOpen { at, open, new } => {
                write!(f, "#{at}: Lost({open})より前に次のEstablished({new})が来た")
            }
            TraceViolation::EstablishedNotMonotonic { at, last, new } => {
                write!(f, "#{at}: Establishedの世代が単調増加でない(直前{last}、今回{new})")
            }
            TraceViolation::LostWithoutEstablished { at, generation, open } => {
                write!(f, "#{at}: 対応するEstablishedの無いLost({generation})(開いているedge: {open:?})")
            }
            TraceViolation::StaleReconnecting { at, generation } => {
                write!(f, "#{at}: 世代{generation}のConnected/Establishedの後、Lost({generation})より前にReconnectingが届いた")
            }
            TraceViolation::StaleTimerProducedEffects { at, token, current, effects } => write!(
                f,
                "#{at}: 非現行token({token}、現行{current})のタイマー満了が{effects}個のEffectを生んだ"
            ),
        }
    }
}

/// [`check_trace`]の逐次版(記録器が1件ずつ検査するため)。
#[derive(Debug, Default, Clone)]
pub(crate) struct TraceChecker {
    at: usize,
    /// `Established(g)`を受けて`Lost(g)`をまだ受けていない世代。
    open: Option<u64>,
    last_established: Option<u64>,
}

impl TraceChecker {
    pub(crate) fn step(&mut self, ev: &TraceEvent) -> Result<(), TraceViolation> {
        let at = self.at;
        self.at += 1;
        match *ev {
            TraceEvent::Established(new) => {
                if let Some(open) = self.open {
                    return Err(TraceViolation::EstablishedWhileOpen { at, open, new });
                }
                if let Some(last) = self.last_established {
                    if new <= last {
                        return Err(TraceViolation::EstablishedNotMonotonic { at, last, new });
                    }
                }
                self.open = Some(new);
                self.last_established = Some(new);
            }
            TraceEvent::Lost(generation) => {
                if self.open != Some(generation) {
                    return Err(TraceViolation::LostWithoutEstablished { at, generation, open: self.open });
                }
                self.open = None;
            }
            TraceEvent::State(StateTag::Reconnecting) => {
                if let Some(generation) = self.open {
                    return Err(TraceViolation::StaleReconnecting { at, generation });
                }
            }
            TraceEvent::State(StateTag::Connected | StateTag::Disconnected | StateTag::Connecting | StateTag::Error) => {}
            TraceEvent::TimerFired { token, current, effects } => {
                if token != current && effects > 0 {
                    return Err(TraceViolation::StaleTimerProducedEffects { at, token, current, effects });
                }
            }
        }
        Ok(())
    }
}

/// 純粋な検査関数(ADR §6 Step 11の`check_trace`)。
pub(crate) fn check_trace(trace: &[TraceEvent]) -> Result<(), TraceViolation> {
    let mut checker = TraceChecker::default();
    trace.iter().try_for_each(|ev| checker.step(ev))
}

#[derive(Default)]
struct RecorderInner {
    events: Vec<TraceEvent>,
    checker: TraceChecker,
    violation: Option<TraceViolation>,
}

/// テスト用のcallback実装(`RecordingCallback`/`ForwardingOrchestratorCallback`)に持たせる記録器。
/// 記録のたびに逐次検査し、違反した時点でpanicする。`Drop`でも再検査する(別スレッドで
/// panicが握りつぶされた場合のバックストップ)。
#[derive(Default)]
pub(crate) struct TraceRecorder {
    inner: StdMutex<RecorderInner>,
}

impl TraceRecorder {
    pub(crate) fn record(&self, ev: TraceEvent) {
        let failure = {
            let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            inner.events.push(ev);
            if inner.violation.is_some() {
                None
            } else {
                match inner.checker.step(&ev) {
                    Ok(()) => None,
                    Err(v) => {
                        inner.violation = Some(v.clone());
                        Some((v, inner.events.clone()))
                    }
                }
            }
        };
        // Mutexを持たずにpanicする(poisonさせない)。
        if let Some((v, events)) = failure {
            panic!("Step 11 trace invariant violated: {v}\ntrace: {events:?}");
        }
    }

    pub(crate) fn record_state(&self, state: &ConnectionPublicState) {
        self.record(TraceEvent::State(StateTag::from(state)));
    }

    pub(crate) fn record_edge(&self, edge: &ConnectionEdge, generation: u64) {
        self.record(TraceEvent::edge(edge, generation));
    }
}

impl Drop for TraceRecorder {
    fn drop(&mut self) {
        if std::thread::panicking() {
            return;
        }
        let inner = self.inner.get_mut().unwrap_or_else(|e| e.into_inner());
        if let Err(v) = check_trace(&inner.events) {
            panic!("Step 11 trace invariant violated (detected at drop): {v}\ntrace: {:?}", inner.events);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::TraceEvent::{Established as E, Lost as L, State as S};
    use super::*;

    #[test]
    fn a_well_formed_connect_disconnect_reconnect_trace_passes() {
        let trace = [
            S(StateTag::Connecting),
            S(StateTag::Connected),
            E(1),
            L(1),
            S(StateTag::Reconnecting),
            S(StateTag::Reconnecting),
            S(StateTag::Connected),
            E(2),
            // 同一世代の`Connected`再公開(on_connected重複)はエッジを出し直さない。
            S(StateTag::Connected),
            TraceEvent::TimerFired { token: 3, current: 3, effects: 2 },
            TraceEvent::TimerFired { token: 1, current: 3, effects: 0 },
        ];
        assert_eq!(check_trace(&trace), Ok(()));
    }

    #[test]
    fn checker_fails_on_reversed_lost_and_established() {
        assert_eq!(
            check_trace(&[L(1), E(1)]),
            Err(TraceViolation::LostWithoutEstablished { at: 0, generation: 1, open: None })
        );
    }

    #[test]
    fn checker_fails_on_double_lost() {
        assert_eq!(
            check_trace(&[E(1), L(1), L(1)]),
            Err(TraceViolation::LostWithoutEstablished { at: 2, generation: 1, open: None })
        );
    }

    #[test]
    fn checker_fails_on_a_missing_lost_before_the_next_established() {
        assert_eq!(check_trace(&[E(1), E(2)]), Err(TraceViolation::EstablishedWhileOpen { at: 1, open: 1, new: 2 }));
    }

    #[test]
    fn checker_fails_on_a_repeated_or_decreasing_established() {
        assert_eq!(
            check_trace(&[E(2), L(2), E(2)]),
            Err(TraceViolation::EstablishedNotMonotonic { at: 2, last: 2, new: 2 })
        );
        assert_eq!(
            check_trace(&[E(2), L(2), E(1)]),
            Err(TraceViolation::EstablishedNotMonotonic { at: 2, last: 2, new: 1 })
        );
    }

    #[test]
    fn checker_fails_on_a_lost_for_another_generation() {
        assert_eq!(
            check_trace(&[E(2), L(1)]),
            Err(TraceViolation::LostWithoutEstablished { at: 1, generation: 1, open: Some(2) })
        );
    }

    #[test]
    fn checker_fails_on_a_stale_reconnecting_after_connected() {
        assert_eq!(
            check_trace(&[S(StateTag::Connected), E(1), S(StateTag::Reconnecting)]),
            Err(TraceViolation::StaleReconnecting { at: 2, generation: 1 })
        );
        // Lost(1)を挟めば(新しい切断による)正当なReconnecting。
        assert_eq!(check_trace(&[S(StateTag::Connected), E(1), L(1), S(StateTag::Reconnecting)]), Ok(()));
    }

    #[test]
    fn checker_fails_on_a_stale_timer_that_produced_effects() {
        assert_eq!(
            check_trace(&[TraceEvent::TimerFired { token: 1, current: 2, effects: 1 }]),
            Err(TraceViolation::StaleTimerProducedEffects { at: 0, token: 1, current: 2, effects: 1 })
        );
    }

    #[test]
    fn recorder_panics_at_the_first_violation() {
        let recorder = TraceRecorder::default();
        recorder.record(E(1));
        recorder.record(L(1));
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| recorder.record(L(1))));
        let message = result.expect_err("二重Lostで記録器がpanicしなかった");
        let message = message.downcast_ref::<String>().cloned().unwrap_or_default();
        assert!(message.contains("Lost(1)"), "panicメッセージ: {message}");
        // 記録済みの違反は`Drop`でも検出される(このテストではそれを確かめた上で捨てる)。
        assert!(check_trace(&recorder.inner.lock().unwrap_or_else(|e| e.into_inner()).events).is_err());
        std::mem::forget(recorder);
    }

    #[test]
    fn recorder_drop_backstop_detects_a_violation_recorded_on_another_thread() {
        let recorder = std::sync::Arc::new(TraceRecorder::default());
        let r = recorder.clone();
        // 別スレッドでの逐次panicは(テストのスレッドには)伝わらない。
        let _ = std::thread::spawn(move || {
            r.record(E(1));
            r.record(E(2));
        })
        .join();
        let recorder = std::sync::Arc::try_unwrap(recorder).unwrap_or_else(|_| panic!("Arcが共有されたまま"));
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || drop(recorder)));
        assert!(result.is_err(), "Dropのバックストップが別スレッドで記録された違反を検出しなかった");
    }
}
