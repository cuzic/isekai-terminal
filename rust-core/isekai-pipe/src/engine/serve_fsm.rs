//! `isekai-pipe serve`の単一集約 [`ServeAggregate`](ADR_FUNCTIONAL_CORE_EFFECTS.md §6 Step 2a)。
//!
//! fencing([`AttachArbiter`])とresume用のsession index([`IndexEntry`]の`BTreeMap`)を
//! **1つの値**にまとめた純粋reducer。shell(`attach_runtime.rs`の`AttachRuntime`)はこれと
//! parkedソケット(`SessionIo`)を**1つのロック**で守り、`apply`とin-lock effectの解釈を
//! 同じ臨界区間で行う(§2.4-1,2)。これにより次が「呼び忘れ」ではなく「同じ遷移の中にある」
//! 性質になる(§4.1):
//!
//! - indexからの除去とarbiter slotの解放(`RelayEnded`)は常に同じ`apply`内で起こり、
//!   そのことを[`ServeEffect::Discard`]として1回だけ報告する。removalはreducer起点のみ
//!   (shellが直接「消す」Eventは無い)。
//! - sweepの「期限切れ判定」と「除去」、RESUMEの「parked確認」と「unpark」は、それぞれ
//!   1回の`apply`で行われる(旧`SessionTable::sweep_expired_parked`の2段階TOCTOUの解消)。
//!
//! 時刻はshellがロック取得後に刻む`now: Millis`としてのみ入り、差は`saturating_sub`でのみ
//! 計算する(§2.2)。既存エンティティに関する「事実」Event(`Parked`/`RelayTerminated`/
//! `RelayEnded`)はそのincarnationの`LeaseId`を運び、reducerは[`IndexEntry::lease`]と
//! 一致しないものを無視する(§2.2 N-2、不変条件I-e/I-f)。クライアントが選んだidで引く
//! **要求**(`ResumeRequested`)は1回の`apply`で解決し、続きに必要な世代トークン(lease)を
//! reducerが返す(§2.2 R3-2)。
//!
//! 新規sessionのadmission(`--max-sessions`)も、容量判定・最古parkedの立ち退き・fencing slotの
//! 確保(`HelloReceived`)を**1回の`apply`**で行う([`ServeEvent::AdmitRequested`]、Step 2b)。
//! 旧`admit_new_session`は判定(`session_count() < max_sessions()`)とslot確保(`hello()`)の間で
//! ロックを手放していたため、同時に来た2つの新規sessionが両方とも判定を通り、max+1になり得た。
//!
//! 不変条件(§4.1 I-a〜I-j、およびStep 2bのI-k「slot数は`max_sessions`を超えない」)は
//! このモジュールのproptestで検証する。
// 純粋モジュール(`pure_modules.toml`登録、ADR_FUNCTIONAL_CORE_EFFECTS.md §2.3)。
#![deny(clippy::disallowed_methods, clippy::disallowed_types)]

use std::collections::BTreeMap;
use std::time::Duration;

use isekai_protocol::attach::{AttachKey, AttachRejectReason, AttachToken};
use isekai_protocol::{Millis, SessionId};

use super::attach_arbiter::{AttachArbiter, AttachEffect, AttachEvent, AttachState, LeaseId, TargetHandleId};

/// indexのキー(`resume::SessionId`と同じ`[u8; 16]`)。`isekai_protocol::SessionId`は`Ord`を
/// 持たないので、決定論的な`BTreeMap`のキーにはこちらを使う(§3-4)。
pub type SessionKey = [u8; 16];

/// あるsessionの現incarnationについてreducerが知っていること。ソケット等の資源は
/// shell側の`SessionIo`が持つ(§2.1: Stateはソケット・Notify・ロックを持たない)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexEntry {
    /// このincarnationの`Established` lease。RESUMEで再開しても同じleaseのまま。
    pub lease: LeaseId,
    /// parkされた時刻(`Some`=parked、`None`=active)。
    pub parked_since: Option<Millis>,
    /// ATTACHのACKで約束した実効resume-grace。sweepはグローバルな`max_parked`との短い方を使う。
    pub negotiated_grace_secs: Option<u32>,
    /// 容量超過で登録された(旧`InsertOutcome::Rejected`)。RESUME不可・LRU対象外・
    /// table側の容量計数に含めない。parkは`Discard{Unresumable}`になる(I-g)。
    pub unresumable: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiscardCause {
    /// park期限切れ(sweep)。
    Expired,
    /// `--max-sessions`超過で最古のparkedを立ち退かせた(Activated時/admission時)。
    Evicted,
    /// target TCPが死んだ(`RelayEnded`/`RelayTerminated{TcpDied}`)。
    TcpDied,
    /// `SessionTableEntryGuard`のDropバックストップ(タスクがpanic等で後始末に到達しなかった)。
    GuardDropped,
    /// unresumableなエントリのpark(旧: 孤児park→恒久的なslotリーク、Step 2aの意図した挙動変更)。
    Unresumable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminateReason {
    TcpDied,
    GuardDropped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServeEvent {
    // ---- 要求: ATTACH_HELLOのadmission(Step 2b) ----
    /// 新規sessionのadmissionと`HelloReceived`を**同じapply**で行う(Step 2b)。
    /// arbiterが既にslotを持つsession_id(再送・再ATTACH・supersede)は容量判定を素通りして
    /// そのまま`HelloReceived`へ。新規session_idは、slot数が`max_sessions`未満ならそのまま、
    /// 満杯なら最古parkedを`Discard{Evicted}`してから、立ち退けるparkedが無ければ
    /// `SendReject{BusyOtherSession}`(slotは確保しない)。
    ///
    /// ADRの`AdmitRequested{id}`に対し`key`全体を運ぶのは、判定とslot確保(`HelloReceived`は
    /// `key`を要する)を分けると、そのapplyの間で同じcheck-then-actが再発するため。
    AdmitRequested { key: AttachKey },
    // ---- ATTACH v2(`AttachArbiter`へ委譲するもの) ----
    TargetConnected { lease: LeaseId, target: TargetHandleId, attach_token: AttachToken },
    TargetConnectFailed { lease: LeaseId },
    CancelReceived { key: AttachKey },
    LeaseStopped { lease: LeaseId },
    PendingExpired { lease: LeaseId },
    /// `AttachActivate`。成功すれば同じapplyでindexへ登録する(activate→insertの窓を消す、I-c)。
    Activated { key: AttachKey, attach_token: AttachToken, negotiated_grace_secs: Option<u32> },
    /// 中継が終わった(`EstablishedLease::release`/`Drop`)。現`Established` leaseなら同じ遷移で
    /// indexエントリも除き`Discard{TcpDied}`を返す(I-j)。
    RelayEnded { lease: LeaseId },
    // ---- 事実(既存incarnationのleaseを運ぶ) ----
    Parked { id: SessionKey, lease: LeaseId, now: Millis },
    RelayTerminated { id: SessionKey, lease: LeaseId, reason: TerminateReason },
    Sweep { now: Millis, max_parked: Duration },
    // ---- 要求(1回のapplyで解決する) ----
    ResumeRequested { id: SessionKey },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServeEffect {
    /// 既存の`AttachEffect`(out-of-lock: 接続・timer・waiterへの応答)。
    Attach(AttachEffect),
    /// in-lock: `Activated`で登録したincarnationの`SessionIo`(出力バッファhandle・Notify)を
    /// shell側のソケットマップへ登録する。
    RegisterIo { id: SessionKey, lease: LeaseId, unresumable: bool },
    /// in-lock: `(id, lease)`の`SessionIo`をソケットマップから除去してdropする(=parked TCPのclose)。
    /// arbiter slotの解放は同じapply内のState遷移で既に済んでいる。shellの解釈は1箇所・冪等で、
    /// leaseが違えば(別incarnation)触らない。
    Discard { id: SessionKey, lease: LeaseId, cause: DiscardCause },
    /// in-lock: shellが手元に持っているソケットを`SessionIo.parked_tcp`へ格納し、
    /// out-of-lockで`reparked`を通知する。
    StoreParked { id: SessionKey, lease: LeaseId },
    /// in-lock: parkedソケットと出力バッファhandleを**一緒に**shellへ引き渡す(I-i)。
    ResumeGranted { id: SessionKey, lease: LeaseId },
    /// out-of-lock: 現在中継中の接続へ`preempt`を通知する(Q9: shellは`reparked`を待ってから
    /// `ResumeRequested`を1回だけ再送する)。
    RequestPreempt { id: SessionKey, lease: LeaseId },
    /// RESUMEをUnknownTokenで拒否する。状態は変えていない。
    ResumeRejected { id: SessionKey },
}

/// `AttachArbiter` + session index。§2.1の標準形(`apply(&mut self, Event) -> Vec<Effect>`)。
#[derive(Debug)]
// テスト専用: 有界網羅探索(ADR Step 10-2、このモジュールのテスト)が状態を複製するため。本番ビルドには影響しない。
#[cfg_attr(test, derive(Clone))]
pub struct ServeAggregate {
    arbiter: AttachArbiter,
    index: BTreeMap<SessionKey, IndexEntry>,
    /// table側の容量上限(`--max-sessions`)。unresumableなエントリは数えない(I-c、round 3 m-R3-3)。
    max_sessions: usize,
}

impl ServeAggregate {
    pub fn new(max_sessions: usize) -> Self {
        Self { arbiter: AttachArbiter::new(), index: BTreeMap::new(), max_sessions }
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn max_sessions(&self) -> usize {
        self.max_sessions
    }

    /// 読み取りクエリ(`is_vacant`の`session_count`、`established_lease_for`等)。admissionの判定には
    /// 使わない(判定は`AdmitRequested`の同じapply内で行う、Step 2b)。
    pub fn arbiter(&self) -> &AttachArbiter {
        &self.arbiter
    }

    pub fn index_entry(&self, id: &SessionKey) -> Option<&IndexEntry> {
        self.index.get(id)
    }

    pub fn apply(&mut self, event: ServeEvent) -> Vec<ServeEffect> {
        match event {
            ServeEvent::AdmitRequested { key } => self.on_admit_requested(key),
            ServeEvent::TargetConnected { lease, target, attach_token } => {
                self.forward(AttachEvent::TargetConnected { lease, target, attach_token })
            }
            ServeEvent::TargetConnectFailed { lease } => self.forward(AttachEvent::TargetConnectFailed { lease }),
            ServeEvent::CancelReceived { key } => self.forward(AttachEvent::CancelReceived { key }),
            ServeEvent::LeaseStopped { lease } => self.forward(AttachEvent::LeaseStopped { lease }),
            ServeEvent::PendingExpired { lease } => self.forward(AttachEvent::PendingExpired { lease }),
            ServeEvent::Activated { key, attach_token, negotiated_grace_secs } => {
                self.on_activated(key, attach_token, negotiated_grace_secs)
            }
            ServeEvent::RelayEnded { lease } => self.on_relay_ended(lease),
            ServeEvent::Parked { id, lease, now } => self.on_parked(id, lease, now),
            ServeEvent::RelayTerminated { id, lease, reason } => self.on_relay_terminated(id, lease, reason),
            ServeEvent::Sweep { now, max_parked } => self.on_sweep(now, max_parked),
            ServeEvent::ResumeRequested { id } => self.on_resume_requested(id),
        }
    }

    /// `Established`に触れない(=indexと無関係な)ATTACH Eventをarbiterへそのまま渡す。
    /// `HelloReceived`(`on_admit_requested`経由)は`Established`を拒否するだけ、他は`Connecting`/`PendingActivation`/
    /// `ClosingForSupersede`しか遷移させないので、I-c(Established ⇔ indexエントリ)を壊さない。
    fn forward(&mut self, event: AttachEvent) -> Vec<ServeEffect> {
        self.arbiter.apply(event).into_iter().map(ServeEffect::Attach).collect()
    }

    fn established_lease(&self, id: SessionKey) -> Option<LeaseId> {
        match self.arbiter.state_for(SessionId::from_bytes(id)) {
            Some(AttachState::Established { lease, .. }) => Some(*lease),
            Some(AttachState::Connecting { .. })
            | Some(AttachState::PendingActivation { .. })
            | Some(AttachState::ClosingForSupersede { .. })
            | None => None,
        }
    }

    /// 最古のparkedエントリ。`(parked_since, id)`の辞書順で決定論的にタイブレークする(§3-4)。
    /// unresumableなエントリはparkされない(I-g)ので自然に対象外。
    fn oldest_parked(&self) -> Option<SessionKey> {
        self.index
            .iter()
            .filter_map(|(id, e)| e.parked_since.map(|since| (since, *id)))
            .min()
            .map(|(_, id)| id)
    }

    /// **唯一の除去経路**: indexエントリの除去とarbiter slotの解放を同じ遷移で行う(I-a)。
    fn discard(&mut self, id: SessionKey, cause: DiscardCause) -> Vec<ServeEffect> {
        let Some(entry) = self.index.remove(&id) else { return vec![] };
        let mut out = vec![ServeEffect::Discard { id, lease: entry.lease, cause }];
        out.extend(self.forward(AttachEvent::RelayEnded { lease: entry.lease }));
        out
    }

    fn on_activated(
        &mut self,
        key: AttachKey,
        attach_token: AttachToken,
        negotiated_grace_secs: Option<u32>,
    ) -> Vec<ServeEffect> {
        let id = *key.session_id.as_bytes();
        let was_established = self.established_lease(id).is_some();
        let attach_effects = self.arbiter.apply(AttachEvent::Activated { key, attach_token });
        let mut out = Vec::new();
        if let (false, Some(lease)) = (was_established, self.established_lease(id)) {
            // 旧`insert_existing`と同じ容量判定(table側の計数。unresumableは数えない)。
            let live = self.index.values().filter(|e| !e.unresumable).count();
            let mut unresumable = false;
            if live >= self.max_sessions && !self.index.contains_key(&id) {
                match self.oldest_parked() {
                    Some(victim) => out.extend(self.discard(victim, DiscardCause::Evicted)),
                    None => unresumable = true,
                }
            }
            self.index.insert(id, IndexEntry { lease, parked_since: None, negotiated_grace_secs, unresumable });
            out.push(ServeEffect::RegisterIo { id, lease, unresumable });
        }
        out.extend(attach_effects.into_iter().map(ServeEffect::Attach));
        out
    }

    fn on_relay_ended(&mut self, lease: LeaseId) -> Vec<ServeEffect> {
        // leaseは全session間で一意(attach_arbiter.rs)なので、一致するエントリは高々1つ。
        let owner = self.index.iter().find(|(_, e)| e.lease == lease).map(|(id, _)| *id);
        match owner {
            Some(id) => self.discard(id, DiscardCause::TcpDied),
            // indexに無いlease: 非現行(stale)なら arbiter側も no-op。
            None => self.forward(AttachEvent::RelayEnded { lease }),
        }
    }

    fn on_parked(&mut self, id: SessionKey, lease: LeaseId, now: Millis) -> Vec<ServeEffect> {
        let Some(entry) = self.index.get_mut(&id) else { return vec![] };
        if entry.lease != lease {
            return vec![];
        }
        if entry.unresumable {
            // I-g: unresumableなエントリはparked-and-Establishedに到達しない。
            return self.discard(id, DiscardCause::Unresumable);
        }
        entry.parked_since = Some(now);
        vec![ServeEffect::StoreParked { id, lease }]
    }

    fn on_relay_terminated(&mut self, id: SessionKey, lease: LeaseId, reason: TerminateReason) -> Vec<ServeEffect> {
        if self.index.get(&id).map(|e| e.lease) != Some(lease) {
            return vec![];
        }
        let cause = match reason {
            TerminateReason::TcpDied => DiscardCause::TcpDied,
            TerminateReason::GuardDropped => DiscardCause::GuardDropped,
        };
        self.discard(id, cause)
    }

    fn on_sweep(&mut self, now: Millis, max_parked: Duration) -> Vec<ServeEffect> {
        let expired: Vec<SessionKey> = self
            .index
            .iter()
            .filter_map(|(id, e)| {
                let since = e.parked_since?;
                (now.saturating_sub(since) >= effective_deadline(e, max_parked)).then_some(*id)
            })
            .collect();
        expired.into_iter().flat_map(|id| self.discard(id, DiscardCause::Expired)).collect()
    }

    fn on_resume_requested(&mut self, id: SessionKey) -> Vec<ServeEffect> {
        let Some(entry) = self.index.get(&id).copied() else {
            return vec![ServeEffect::ResumeRejected { id }];
        };
        if entry.unresumable || self.established_lease(id) != Some(entry.lease) {
            // slotの無いsessionへソケットを戻す旧分岐(mod.rs:1431-1435)はここで消える:
            // ソケットに触れずに拒否する(Step 2aの意図した挙動変更その3、I-i)。
            return vec![ServeEffect::ResumeRejected { id }];
        }
        match entry.parked_since {
            Some(_) => {
                if let Some(e) = self.index.get_mut(&id) {
                    e.parked_since = None;
                }
                vec![ServeEffect::ResumeGranted { id, lease: entry.lease }]
            }
            None => vec![ServeEffect::RequestPreempt { id, lease: entry.lease }],
        }
    }

    /// Step 2b: 容量判定・立ち退き・slot確保を1回のapplyで(旧`admit_new_session`の
    /// check-then-act解消)。判定に使うのはarbiterのslot数(Connecting/PendingActivation/
    /// Established(unresumable含む)/ClosingForSupersedeすべて、I-cのadmission側の計数)で、
    /// 現状と同じ。立ち退きは現状どおり1つだけ: このapplyだけがslot数を増やすので、
    /// slot数は常に`max_sessions`以下(I-k)であり、満杯=ちょうど`max_sessions`から1つ空ければ足りる。
    fn on_admit_requested(&mut self, key: AttachKey) -> Vec<ServeEffect> {
        let mut out = Vec::new();
        if !self.arbiter.has_session(key.session_id) && self.arbiter.session_count() >= self.max_sessions {
            match self.oldest_parked() {
                Some(victim) => out.extend(self.discard(victim, DiscardCause::Evicted)),
                None => {
                    // 本当に満杯(全sessionがactive)。新しいwire reasonは足さず、クライアントの
                    // 既存の`retry_while_busy_other_session`(resume_loop.rs)に任せる(旧挙動どおり)。
                    return vec![ServeEffect::Attach(AttachEffect::SendReject {
                        key,
                        reason: AttachRejectReason::BusyOtherSession,
                    })];
                }
            }
        }
        out.extend(self.forward(AttachEvent::HelloReceived { key }));
        out
    }
}

#[cfg(test)]
impl ServeAggregate {
    /// admissionを通さずに`HelloReceived`だけを適用する(テスト専用)。Step 2b以前の
    /// check-then-act競合が作れた「容量超過のslot」(→unresumable登録)を再現するためだけに使う。
    /// unresumable経路そのものはStep 2cで扱う。
    pub(crate) fn hello_bypassing_admission(&mut self, key: AttachKey) -> Vec<ServeEffect> {
        self.forward(AttachEvent::HelloReceived { key })
    }
}

/// 実際の締切はグローバルな`max_parked`(`--resume-window`)とACKで約束した値の**短い方**
/// (旧`sweep_expired_parked`のFableレビュー指摘)。
pub fn effective_deadline(entry: &IndexEntry, max_parked: Duration) -> Duration {
    match entry.negotiated_grace_secs {
        Some(secs) => max_parked.min(Duration::from_secs(u64::from(secs))),
        None => max_parked,
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, clippy::disallowed_types)]
mod tests {
    use super::*;
    use isekai_protocol::attach::{AttemptId, ConnectionGeneration};
    use proptest::prelude::*;
    use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};

    fn key(session: u8, generation: u64, attempt: u8) -> AttachKey {
        AttachKey {
            session_id: SessionId::from_bytes([session; 16]),
            generation: ConnectionGeneration::new(generation),
            attempt_id: AttemptId::from_bytes([attempt; 16]),
        }
    }

    fn token(b: u8) -> AttachToken {
        AttachToken::new([b; 16])
    }

    fn id(s: u8) -> SessionKey {
        [s; 16]
    }

    /// admission+HELLO → TargetConnected → Activated で`s`を`Established`にし、そのleaseを返す。
    fn establish(agg: &mut ServeAggregate, s: u8, grace: Option<u32>) -> (LeaseId, Vec<ServeEffect>) {
        let k = key(s, 0, 1);
        let lease = match agg.apply(ServeEvent::AdmitRequested { key: k }).as_slice() {
            [ServeEffect::Attach(AttachEffect::ConnectTarget { lease })] => *lease,
            other => panic!("unexpected admission effects {other:?}"),
        };
        finish_establish(agg, s, k, lease, grace)
    }

    /// admissionを通さずに`establish`する(Step 2b以前の競合でしか作れなかった容量超過のslot)。
    fn establish_bypassing_admission(agg: &mut ServeAggregate, s: u8, grace: Option<u32>) -> (LeaseId, Vec<ServeEffect>) {
        let k = key(s, 0, 1);
        let lease = match agg.hello_bypassing_admission(k).as_slice() {
            [ServeEffect::Attach(AttachEffect::ConnectTarget { lease })] => *lease,
            other => panic!("unexpected hello effects {other:?}"),
        };
        finish_establish(agg, s, k, lease, grace)
    }

    fn finish_establish(
        agg: &mut ServeAggregate,
        s: u8,
        k: AttachKey,
        lease: LeaseId,
        grace: Option<u32>,
    ) -> (LeaseId, Vec<ServeEffect>) {
        agg.apply(ServeEvent::TargetConnected { lease, target: TargetHandleId(s as u64), attach_token: token(s) });
        let effects = agg.apply(ServeEvent::Activated { key: k, attach_token: token(s), negotiated_grace_secs: grace });
        (lease, effects)
    }

    fn established(agg: &ServeAggregate, s: u8) -> Option<LeaseId> {
        agg.established_lease(id(s))
    }

    #[test]
    fn activated_registers_index_entry_in_the_same_apply() {
        let mut agg = ServeAggregate::new(4);
        let (lease, effects) = establish(&mut agg, 1, Some(30));
        assert_eq!(
            effects,
            vec![
                ServeEffect::RegisterIo { id: id(1), lease, unresumable: false },
                ServeEffect::Attach(AttachEffect::StartRelay { lease, target: TargetHandleId(1) }),
            ]
        );
        assert_eq!(
            agg.index_entry(&id(1)),
            Some(&IndexEntry { lease, parked_since: None, negotiated_grace_secs: Some(30), unresumable: false })
        );
    }

    #[test]
    fn park_then_resume_grants_the_same_lease() {
        let mut agg = ServeAggregate::new(4);
        let (lease, _) = establish(&mut agg, 1, None);
        assert_eq!(
            agg.apply(ServeEvent::Parked { id: id(1), lease, now: Millis(10) }),
            vec![ServeEffect::StoreParked { id: id(1), lease }]
        );
        assert_eq!(agg.apply(ServeEvent::ResumeRequested { id: id(1) }), vec![ServeEffect::ResumeGranted { id: id(1), lease }]);
        assert_eq!(agg.index_entry(&id(1)).unwrap().parked_since, None);
        // 2回目は既にactive → preempt要求。
        assert_eq!(agg.apply(ServeEvent::ResumeRequested { id: id(1) }), vec![ServeEffect::RequestPreempt { id: id(1), lease }]);
        assert_eq!(established(&agg, 1), Some(lease));
    }

    #[test]
    fn resume_for_unknown_session_is_rejected_without_state_change() {
        let mut agg = ServeAggregate::new(4);
        assert_eq!(agg.apply(ServeEvent::ResumeRequested { id: id(9) }), vec![ServeEffect::ResumeRejected { id: id(9) }]);
    }

    #[test]
    fn relay_ended_discards_index_entry_and_slot_in_one_transition() {
        let mut agg = ServeAggregate::new(4);
        let (lease, _) = establish(&mut agg, 1, None);
        assert_eq!(
            agg.apply(ServeEvent::RelayEnded { lease }),
            vec![ServeEffect::Discard { id: id(1), lease, cause: DiscardCause::TcpDied }]
        );
        assert_eq!(agg.index_entry(&id(1)), None);
        assert_eq!(established(&agg, 1), None);
        // 続いて届くRelayTerminated{TcpDied}は冪等なno-op。
        assert_eq!(agg.apply(ServeEvent::RelayTerminated { id: id(1), lease, reason: TerminateReason::TcpDied }), vec![]);
    }

    #[test]
    fn stale_lease_facts_for_a_reused_id_do_not_touch_the_new_incarnation() {
        let mut agg = ServeAggregate::new(4);
        let (old, _) = establish(&mut agg, 1, None);
        agg.apply(ServeEvent::RelayEnded { lease: old });
        let (new, _) = establish(&mut agg, 1, None);
        assert_ne!(old, new);
        assert_eq!(agg.apply(ServeEvent::RelayTerminated { id: id(1), lease: old, reason: TerminateReason::GuardDropped }), vec![]);
        assert_eq!(agg.apply(ServeEvent::Parked { id: id(1), lease: old, now: Millis(5) }), vec![]);
        assert_eq!(agg.apply(ServeEvent::RelayEnded { lease: old }), vec![]);
        assert_eq!(established(&agg, 1), Some(new));
        assert_eq!(agg.index_entry(&id(1)).unwrap().parked_since, None);
    }

    // ---- 旧`resume.rs`の`SessionTable`テストの移植(Millisベース) ----

    #[test]
    fn sweep_discards_only_expired_parked_entries() {
        let mut agg = ServeAggregate::new(8);
        let (expired, _) = establish(&mut agg, 1, None);
        let (fresh, _) = establish(&mut agg, 2, None);
        let _active = establish(&mut agg, 3, None);
        agg.apply(ServeEvent::Parked { id: id(1), lease: expired, now: Millis(0) });
        agg.apply(ServeEvent::Parked { id: id(2), lease: fresh, now: Millis(50_000) });
        let effects = agg.apply(ServeEvent::Sweep { now: Millis(60_000), max_parked: Duration::from_secs(30) });
        assert_eq!(effects, vec![ServeEffect::Discard { id: id(1), lease: expired, cause: DiscardCause::Expired }]);
        assert_eq!(established(&agg, 1), None, "expired park releases its fencing slot in the same apply");
        assert!(agg.index_entry(&id(2)).is_some());
        assert!(agg.index_entry(&id(3)).is_some(), "active sessions are never swept");
    }

    #[test]
    fn sweep_honors_a_shorter_negotiated_grace_than_the_global_window() {
        let mut agg = ServeAggregate::new(8);
        let (lease, _) = establish(&mut agg, 1, Some(5));
        agg.apply(ServeEvent::Parked { id: id(1), lease, now: Millis(0) });
        let effects = agg.apply(ServeEvent::Sweep { now: Millis(10_000), max_parked: Duration::from_secs(3600) });
        assert_eq!(effects, vec![ServeEffect::Discard { id: id(1), lease, cause: DiscardCause::Expired }]);
    }

    #[test]
    fn sweep_falls_back_to_the_global_window_without_a_negotiated_grace() {
        let mut agg = ServeAggregate::new(8);
        let (lease, _) = establish(&mut agg, 1, None);
        agg.apply(ServeEvent::Parked { id: id(1), lease, now: Millis(0) });
        assert_eq!(agg.apply(ServeEvent::Sweep { now: Millis(10_000), max_parked: Duration::from_secs(30) }), vec![]);
    }

    #[test]
    fn sweep_with_time_going_backwards_never_expires() {
        // I-h: now < parked_since(時刻逆行)は「未経過」扱い。
        let mut agg = ServeAggregate::new(8);
        let (lease, _) = establish(&mut agg, 1, None);
        agg.apply(ServeEvent::Parked { id: id(1), lease, now: Millis(100_000) });
        assert_eq!(agg.apply(ServeEvent::Sweep { now: Millis(5), max_parked: Duration::from_secs(1) }), vec![]);
        assert!(agg.index_entry(&id(1)).is_some());
    }

    // ---- Step 2b: admission(旧`admit_new_session` + `claim_oldest_parked`)----

    fn busy(k: AttachKey) -> Vec<ServeEffect> {
        vec![ServeEffect::Attach(AttachEffect::SendReject { key: k, reason: AttachRejectReason::BusyOtherSession })]
    }

    #[test]
    fn admission_rejects_busy_without_touching_state_when_full_of_active_sessions() {
        let mut agg = ServeAggregate::new(1);
        let (active, _) = establish(&mut agg, 1, None);
        assert_eq!(agg.apply(ServeEvent::AdmitRequested { key: key(2, 0, 1) }), busy(key(2, 0, 1)));
        assert!(!agg.arbiter.has_session(SessionId::from_bytes(id(2))), "a rejected admission claims no slot");
        assert_eq!(established(&agg, 1), Some(active), "an active session must never be evicted");
        assert_eq!(agg.arbiter.session_count(), 1);
    }

    #[test]
    fn admission_when_full_evicts_the_oldest_parked_with_id_tie_break_in_the_same_apply() {
        let mut agg = ServeAggregate::new(3);
        let (l3, _) = establish(&mut agg, 3, None);
        let (l2, _) = establish(&mut agg, 2, None);
        let (l1, _) = establish(&mut agg, 1, None);
        agg.apply(ServeEvent::Parked { id: id(3), lease: l3, now: Millis(10) });
        agg.apply(ServeEvent::Parked { id: id(2), lease: l2, now: Millis(10) });
        agg.apply(ServeEvent::Parked { id: id(1), lease: l1, now: Millis(20) });
        // parked_sinceが同じ(10)なら小さいidが先(HashMap反復順に依存しない)。
        let effects = agg.apply(ServeEvent::AdmitRequested { key: key(4, 0, 1) });
        assert_eq!(effects[0], ServeEffect::Discard { id: id(2), lease: l2, cause: DiscardCause::Evicted });
        assert!(matches!(&effects[1..], [ServeEffect::Attach(AttachEffect::ConnectTarget { .. })]));
        assert_eq!(established(&agg, 2), None);
        assert_eq!(agg.arbiter.session_count(), 3, "evict one, claim one: still exactly max_sessions");
    }

    #[test]
    fn admission_passes_a_session_that_already_holds_a_slot_through_even_when_full() {
        // 再送・再ATTACH・supersedeは容量判定の対象外(旧`has_session`の素通しと同じ)。
        let mut agg = ServeAggregate::new(1);
        establish(&mut agg, 1, None);
        let effects = agg.apply(ServeEvent::AdmitRequested { key: key(1, 1, 2) });
        assert!(
            !effects.iter().any(|e| matches!(
                e,
                ServeEffect::Discard { .. }
                    | ServeEffect::Attach(AttachEffect::SendReject { reason: AttachRejectReason::BusyOtherSession, .. })
            )),
            "a known session_id is never rejected as busy nor evicts anyone: {effects:?}"
        );
    }

    /// Step 2bが閉じる競合の再現(reducer単体、決定論的): 旧`admit_new_session`のように
    /// 2つの新規sessionの「判定」が両方とも「slot確保」より先に走るとmax+1になる。
    /// 同じ2つの要求を`AdmitRequested`(判定と確保が1回のapply)で流すと、2つ目は拒否される。
    #[test]
    fn split_check_then_hello_admits_max_plus_one_but_atomic_admission_does_not() {
        let old_check = |agg: &ServeAggregate, s: u8| {
            agg.arbiter.has_session(SessionId::from_bytes(id(s))) || agg.arbiter.session_count() < agg.max_sessions()
        };
        let mut old = ServeAggregate::new(1);
        let (a_ok, b_ok) = (old_check(&old, 1), old_check(&old, 2));
        assert!(a_ok && b_ok, "both checks pass before either claims a slot");
        old.hello_bypassing_admission(key(1, 0, 1));
        old.hello_bypassing_admission(key(2, 0, 1));
        assert_eq!(old.arbiter.session_count(), 2, "the pre-2b split admission over-admits (max+1)");

        let mut new = ServeAggregate::new(1);
        assert!(matches!(
            new.apply(ServeEvent::AdmitRequested { key: key(1, 0, 1) }).as_slice(),
            [ServeEffect::Attach(AttachEffect::ConnectTarget { .. })]
        ));
        assert_eq!(new.apply(ServeEvent::AdmitRequested { key: key(2, 0, 1) }), busy(key(2, 0, 1)));
        assert_eq!(new.arbiter.session_count(), 1);
    }

    // Step 2b以降、`Activated`時の立ち退き・unresumable登録はshellからは到達しない(admissionが
    // 先に容量を空けるので、Activated時点のlive数は常にmax_sessions未満)。reducerの分岐自体は
    // Step 2cまで残るので、admissionを迂回して作った容量超過のslotで検証し続ける。
    #[test]
    fn activation_when_full_evicts_oldest_parked_first() {
        let mut agg = ServeAggregate::new(2);
        let (older, _) = establish(&mut agg, 1, None);
        let (newer, _) = establish(&mut agg, 2, None);
        agg.apply(ServeEvent::Parked { id: id(1), lease: older, now: Millis(0) });
        agg.apply(ServeEvent::Parked { id: id(2), lease: newer, now: Millis(10) });
        let (lease3, effects) = establish_bypassing_admission(&mut agg, 3, None);
        assert_eq!(effects[0], ServeEffect::Discard { id: id(1), lease: older, cause: DiscardCause::Evicted });
        assert_eq!(effects[1], ServeEffect::RegisterIo { id: id(3), lease: lease3, unresumable: false });
        assert_eq!(established(&agg, 1), None);
        assert!(agg.index_entry(&id(2)).is_some());
    }

    #[test]
    fn activation_when_full_of_active_sessions_registers_unresumable() {
        let mut agg = ServeAggregate::new(1);
        let (active, _) = establish(&mut agg, 1, None);
        let (lease2, effects) = establish_bypassing_admission(&mut agg, 2, None);
        assert_eq!(effects[0], ServeEffect::RegisterIo { id: id(2), lease: lease2, unresumable: true });
        assert!(agg.index_entry(&id(1)).is_some(), "active sessions are never evicted");
        assert_eq!(established(&agg, 1), Some(active));
        // 現状どおりRESUME不可。
        agg.apply(ServeEvent::Parked { id: id(1), lease: active, now: Millis(0) });
        assert_eq!(agg.apply(ServeEvent::ResumeRequested { id: id(2) }), vec![ServeEffect::ResumeRejected { id: id(2) }]);
        // LRU対象外(parkedのid(1)が選ばれる)。
        assert_eq!(
            agg.apply(ServeEvent::AdmitRequested { key: key(3, 0, 1) })[0],
            ServeEffect::Discard { id: id(1), lease: active, cause: DiscardCause::Evicted }
        );
    }

    #[test]
    fn parking_an_unresumable_entry_discards_it_and_frees_the_slot() {
        // Step 2aの意図した挙動変更その2(I-g、旧: 孤児park→恒久的なslotリーク)。
        let mut agg = ServeAggregate::new(1);
        establish(&mut agg, 1, None);
        let (lease2, _) = establish_bypassing_admission(&mut agg, 2, None);
        assert_eq!(
            agg.apply(ServeEvent::Parked { id: id(2), lease: lease2, now: Millis(0) }),
            vec![ServeEffect::Discard { id: id(2), lease: lease2, cause: DiscardCause::Unresumable }]
        );
        assert_eq!(established(&agg, 2), None);
        assert_eq!(agg.index_entry(&id(2)), None);
        // 同じsession_idのslotが空いた(旧: AttachAlreadyEstablishedで永久拒否)。
        assert!(!agg.arbiter.has_session(SessionId::from_bytes(id(2))));
        assert!(matches!(
            agg.hello_bypassing_admission(key(2, 1, 1)).as_slice(),
            [ServeEffect::Attach(AttachEffect::ConnectTarget { .. })]
        ));
    }

    // ---- proptest: §4.1 I-a〜I-j(任意Event列、非単調now・stale lease・id再利用・要求と事実の交錯) ----

    const SESSIONS: u8 = 3;

    #[derive(Debug, Clone)]
    enum Op {
        /// `AdmitRequested`(admission+HELLO、Step 2b)。
        Admit { s: u8, g: u8, at: u8 },
        /// admissionを迂回したHELLO(Step 2b以前の競合でしか作れなかった容量超過slot。
        /// I-gなどunresumable経路の到達性のため。I-kのproptestでは生成しない)。
        HelloBypass { s: u8, g: u8, at: u8 },
        /// 現在`Connecting`のleaseに対するTargetConnected(到達性を上げるため)。
        ConnectCurrent { s: u8, tok: u8 },
        TargetConnected { l: usize, tok: u8 },
        TargetConnectFailed { l: usize },
        /// 現在`PendingActivation`の鍵とtokenでの`Activated`。
        ActivateCurrent { s: u8, grace: Option<u32> },
        Activated { s: u8, g: u8, at: u8, tok: u8 },
        Cancel { s: u8, g: u8, at: u8 },
        LeaseStopped { l: usize },
        PendingExpired { l: usize },
        RelayEnded { l: usize },
        Parked { s: u8, l: usize, now: u64 },
        RelayTerminated { s: u8, l: usize, dropped: bool },
        Sweep { now: u64, max_parked_ms: u64 },
        Resume { s: u8 },
    }

    /// `allow_bypass`: `HelloBypass`を生成するか(偽なら代わりに`Admit`=実際のshellと同じ入口だけ)。
    fn op_strategy(allow_bypass: bool) -> impl Strategy<Value = Op> {
        let s = || 0u8..SESSIONS;
        let l = || 0usize..64;
        // 非単調な`now`: 各Eventが独立に任意の値を取る(後のEventほど小さいこともある)。
        let now = || 0u64..100_000;
        prop_oneof![
            3 => (s(), 0u8..3, 0u8..2).prop_map(|(s, g, at)| Op::Admit { s, g, at }),
            1 => (s(), 0u8..3, 0u8..2).prop_map(move |(s, g, at)| if allow_bypass {
                Op::HelloBypass { s, g, at }
            } else {
                Op::Admit { s, g, at }
            }),
            3 => (s(), 0u8..3).prop_map(|(s, tok)| Op::ConnectCurrent { s, tok }),
            1 => (l(), 0u8..3).prop_map(|(l, tok)| Op::TargetConnected { l, tok }),
            1 => l().prop_map(|l| Op::TargetConnectFailed { l }),
            4 => (s(), proptest::option::of(0u32..60)).prop_map(|(s, grace)| Op::ActivateCurrent { s, grace }),
            1 => (s(), 0u8..3, 0u8..2, 0u8..3).prop_map(|(s, g, at, tok)| Op::Activated { s, g, at, tok }),
            1 => (s(), 0u8..3, 0u8..2).prop_map(|(s, g, at)| Op::Cancel { s, g, at }),
            1 => l().prop_map(|l| Op::LeaseStopped { l }),
            1 => l().prop_map(|l| Op::PendingExpired { l }),
            2 => l().prop_map(|l| Op::RelayEnded { l }),
            4 => (s(), l(), now()).prop_map(|(s, l, now)| Op::Parked { s, l, now }),
            2 => (s(), l(), any::<bool>()).prop_map(|(s, l, dropped)| Op::RelayTerminated { s, l, dropped }),
            3 => (now(), 0u64..60_000).prop_map(|(now, max_parked_ms)| Op::Sweep { now, max_parked_ms }),
            3 => s().prop_map(|s| Op::Resume { s }),
        ]
    }

    /// 「k番目に発行されたlease」。一部のindexは一度も発行されていないleaseを作る(=stale扱い)。
    fn lease_at(issued: &[LeaseId], fabricated: LeaseId, l: usize) -> LeaseId {
        if issued.is_empty() || l % 8 == 7 {
            fabricated
        } else {
            issued[l % issued.len()]
        }
    }

    /// 一度も発行されないlease: 発行済みleaseを100万個ずらした値は得られないので、
    /// 「別のServeAggregateで十分先まで発行したlease」を使う。
    fn fabricated_lease() -> LeaseId {
        let mut other = ServeAggregate::new(0);
        let mut last = None;
        for n in 0..=200u64 {
            let k = AttachKey {
                session_id: SessionId::from_bytes([0xFF; 16]),
                generation: ConnectionGeneration::new(n),
                attempt_id: AttemptId::from_bytes([0xFF; 16]),
            };
            for e in other.hello_bypassing_admission(k) {
                if let ServeEffect::Attach(AttachEffect::ConnectTarget { lease }) = e { last = Some(lease); }
                if let ServeEffect::Attach(AttachEffect::CancelLease { lease }) = e {
                    for e2 in other.apply(ServeEvent::LeaseStopped { lease }) {
                        if let ServeEffect::Attach(AttachEffect::ConnectTarget { lease }) = e2 { last = Some(lease); }
                    }
                }
            }
        }
        last.expect("fabricated lease")
    }

    /// 主proptestと同じ生成器(`(max_sessions, ops)`)。カバレッジテストが同じ分布を測るために共有する。
    fn sequence_strategy() -> impl Strategy<Value = (usize, Vec<Op>)> {
        (0usize..4, proptest::collection::vec(op_strategy(true), 0..120))
    }

    type Snapshot = (Vec<Option<AttachState>>, Vec<(SessionKey, IndexEntry)>);

    fn snapshot(agg: &ServeAggregate) -> Snapshot {
        let states = (0..SESSIONS).map(|s| agg.arbiter.state_for(SessionId::from_bytes(id(s))).cloned()).collect();
        let index = agg.index.iter().map(|(k, v)| (*k, *v)).collect();
        (states, index)
    }

    /// `HelloBypass`は`ServeEvent`ではないので呼び出し側で別に扱う(ここでは`None`)。
    fn resolve(agg: &ServeAggregate, op: &Op, issued: &[LeaseId], fabricated: LeaseId) -> Option<ServeEvent> {
        Some(match *op {
            Op::Admit { s, g, at } => ServeEvent::AdmitRequested { key: key(s, g as u64, at) },
            Op::HelloBypass { .. } => return None,
            Op::ConnectCurrent { s, tok } => match agg.arbiter.state_for(SessionId::from_bytes(id(s))) {
                Some(AttachState::Connecting { lease, .. }) => {
                    ServeEvent::TargetConnected { lease: *lease, target: TargetHandleId(s as u64), attach_token: token(tok) }
                }
                _ => return None,
            },
            Op::TargetConnected { l, tok } => ServeEvent::TargetConnected {
                lease: lease_at(issued, fabricated, l),
                target: TargetHandleId(l as u64),
                attach_token: token(tok),
            },
            Op::TargetConnectFailed { l } => ServeEvent::TargetConnectFailed { lease: lease_at(issued, fabricated, l) },
            Op::ActivateCurrent { s, grace } => match agg.arbiter.state_for(SessionId::from_bytes(id(s))) {
                Some(AttachState::PendingActivation { key, attach_token, .. }) => {
                    ServeEvent::Activated { key: *key, attach_token: *attach_token, negotiated_grace_secs: grace }
                }
                _ => return None,
            },
            Op::Activated { s, g, at, tok } => {
                ServeEvent::Activated { key: key(s, g as u64, at), attach_token: token(tok), negotiated_grace_secs: None }
            }
            Op::Cancel { s, g, at } => ServeEvent::CancelReceived { key: key(s, g as u64, at) },
            Op::LeaseStopped { l } => ServeEvent::LeaseStopped { lease: lease_at(issued, fabricated, l) },
            Op::PendingExpired { l } => ServeEvent::PendingExpired { lease: lease_at(issued, fabricated, l) },
            Op::RelayEnded { l } => ServeEvent::RelayEnded { lease: lease_at(issued, fabricated, l) },
            Op::Parked { s, l, now } => {
                // 半分は「そのidの現lease」(現実のshellが運ぶ値)、半分は任意(stale)。
                let lease = match agg.index.get(&id(s)) {
                    Some(e) if l % 2 == 0 => e.lease,
                    _ => lease_at(issued, fabricated, l),
                };
                ServeEvent::Parked { id: id(s), lease, now: Millis(now) }
            }
            Op::RelayTerminated { s, l, dropped } => {
                let lease = match agg.index.get(&id(s)) {
                    Some(e) if l % 2 == 0 => e.lease,
                    _ => lease_at(issued, fabricated, l),
                };
                let reason = if dropped { TerminateReason::GuardDropped } else { TerminateReason::TcpDied };
                ServeEvent::RelayTerminated { id: id(s), lease, reason }
            }
            Op::Sweep { now, max_parked_ms } => {
                ServeEvent::Sweep { now: Millis(now), max_parked: Duration::from_millis(max_parked_ms) }
            }
            Op::Resume { s } => ServeEvent::ResumeRequested { id: id(s) },
        })
    }

    fn check_structural_invariants(agg: &ServeAggregate) -> Result<(), TestCaseError> {
        for s in 0..SESSIONS {
            let est = agg.established_lease(id(s));
            let entry = agg.index.get(&id(s));
            // I-c: Established ⇔ indexエントリ(同じlease)。
            prop_assert_eq!(est, entry.map(|e| e.lease), "Established/index mismatch for session {}", s);
            if let Some(e) = entry {
                // I-b: parked ⇒ Established(上のI-cで同じleaseまで確認済み)。
                // I-g: unresumableはparkedに到達しない。
                prop_assert!(!(e.unresumable && e.parked_since.is_some()), "unresumable entry is parked");
            }
        }
        // table側の容量: unresumableでないエントリは max_sessions 以下。
        let live = agg.index.values().filter(|e| !e.unresumable).count();
        prop_assert!(live <= agg.max_sessions, "live entries {} exceed max_sessions {}", live, agg.max_sessions);
        Ok(())
    }

    /// 任意のop列を流し、各apply後にI-a〜I-j(と`check_capacity`ならI-k)を検査する。
    /// `check_capacity`: admissionを迂回しない列(`HelloBypass`無し)でだけ真にできる。
    fn run_ops(max_sessions: usize, ops: Vec<Op>, check_capacity: bool) -> Result<(), TestCaseError> {
        let mut agg = ServeAggregate::new(max_sessions);
        let mut issued: Vec<LeaseId> = Vec::new();
        let mut issued_set: HashSet<LeaseId> = HashSet::new();
        let fabricated = fabricated_lease();
        // Step 11: 各applyの(Event, Effect列)を秘密を除いた形で記録し、最後にT3/stale timerの列不変条件を検査する。
        let mut trace: Vec<super::super::trace_invariants::ServeStep> = Vec::new();

        for op in ops {
            if let Op::HelloBypass { s, g, at } = op {
                prop_assert!(!check_capacity, "HelloBypass in a capacity-checked run");
                let bypass_effects = agg.hello_bypassing_admission(key(s, g as u64, at));
                trace.push(super::super::trace_invariants::ServeStep {
                    event: super::super::trace_invariants::ServeTraceEvent::Other("HelloBypassingAdmission"),
                    effects: super::super::trace_invariants::observe_effects(&bypass_effects),
                });
                for e in bypass_effects {
                    if let ServeEffect::Attach(AttachEffect::ConnectTarget { lease }) = e {
                        prop_assert!(issued_set.insert(lease), "lease minted twice");
                        prop_assert_ne!(lease, fabricated);
                        issued.push(lease);
                    }
                }
                check_structural_invariants(&agg)?;
                continue;
            }
            let Some(event) = resolve(&agg, &op, &issued, fabricated) else { continue };
            let before_count = agg.arbiter.session_count();
            let before = snapshot(&agg);
            let before_index = agg.index.clone();
            let observed = super::super::trace_invariants::observe_event(&agg, &event);
            let effects = agg.apply(event);
            trace.push(super::super::trace_invariants::ServeStep {
                event: observed,
                effects: super::super::trace_invariants::observe_effects(&effects),
            });
            let after = snapshot(&agg);

            for e in &effects {
                if let ServeEffect::Attach(AttachEffect::ConnectTarget { lease }) = e {
                    prop_assert!(issued_set.insert(*lease), "lease minted twice");
                    prop_assert_ne!(*lease, fabricated);
                    issued.push(*lease);
                }
            }

            check_transition(&agg, max_sessions, check_capacity, event, before_count, &before, &after, &before_index, &effects)?;
        }
        if let Err(violation) = super::super::trace_invariants::check_serve_trace(&trace) {
            prop_assert!(false, "Step 11 serve trace invariant violated: {} / trace: {:?}", violation, trace);
        }
        Ok(())
    }

    /// One apply's worth of checks (I-a〜I-k and the per-event properties), shared by the
    /// random-sequence proptests (`run_ops`) and the bounded exhaustive exploration (Step 10-2).
    #[allow(clippy::too_many_arguments)]
    fn check_transition(
        agg: &ServeAggregate,
        max_sessions: usize,
        check_capacity: bool,
        event: ServeEvent,
        before_count: usize,
        before: &Snapshot,
        after: &Snapshot,
        before_index: &BTreeMap<SessionKey, IndexEntry>,
        effects: &Vec<ServeEffect>,
    ) -> Result<(), TestCaseError> {
        check_structural_invariants(agg)?;

        if check_capacity {
            // I-k(Step 2b): fencing slotの数は決して`max_sessions`を超えない。旧`admit_new_session`の
            // check-then-actでは、2つの新規sessionの判定が両方ともslot確保より先に走るとmax+1になった。
            prop_assert!(
                agg.arbiter.session_count() <= max_sessions,
                "slots {} exceed max_sessions {} after {:?}", agg.arbiter.session_count(), max_sessions, event
            );
            // 系: admissionが先に容量を空けるので、Activated時点で容量超過になることはなく、
            // unresumable登録には到達しない(この経路の削除自体はStep 2c)。
            prop_assert!(
                !effects.iter().any(|e| matches!(e, ServeEffect::RegisterIo { unresumable: true, .. })),
                "unresumable registration reached through admission"
            );
        }

        for e in effects {
            if let ServeEffect::Discard { id: did, lease, cause } = *e {
                // I-a: Discard後、idはindexにもarbiterのEstablishedにも無い……
                // ただしActivatedの立ち退きでは別idが登録されるだけなので、そのidについて見る。
                prop_assert!(agg.index.get(&did).map(|x| x.lease) != Some(lease));
                prop_assert!(agg.established_lease(did) != Some(lease));
                prop_assert!(agg.index.get(&did).is_none(), "discarded id still indexed");
                prop_assert!(agg.established_lease(did).is_none(), "discarded id still Established");
                // Discardは直前に存在したincarnationに対してのみ出る(同じlease)。
                prop_assert_eq!(before_index.get(&did).map(|x| x.lease), Some(lease));
                match cause {
                    // I-d: activeなidにはEvicted/Expiredを出さない。
                    DiscardCause::Evicted | DiscardCause::Expired => {
                        let since = before_index[&did].parked_since;
                        prop_assert!(since.is_some(), "evicted/expired an active session");
                        if let (DiscardCause::Expired, ServeEvent::Sweep { now, max_parked }) = (cause, event) {
                            // I-h: 非単調nowでも、park後の経過が締切未満なら出さない。
                            let since = since.unwrap_or(Millis(0));
                            let deadline = effective_deadline(&before_index[&did], max_parked);
                            // 経過は§2.2どおり`saturating_sub`で測る(時刻逆行=経過0)。締切0(grace 0 や
                            // max_parked 0)なら経過0でも期限切れなので、`now >= since`そのものは要求しない
                            // (Step 2bのproptestが`grace: Some(0)`+時刻逆行で見つけた、検査側の過剰な要求)。
                            let elapsed = now.saturating_sub(since);
                            prop_assert!(elapsed >= deadline, "expired before its deadline (elapsed {:?} < {:?})", elapsed, deadline);
                            prop_assert!(deadline.is_zero() || now.0 >= since.0, "expired with time going backwards");
                        }
                        if cause == DiscardCause::Evicted {
                            // 決定論的タイブレーク: (parked_since, id)最小のparkedを選ぶ。
                            let oldest = before_index
                                .iter()
                                .filter_map(|(k, v)| v.parked_since.map(|s| (s, *k)))
                                .min()
                                .map(|(_, k)| k);
                            prop_assert_eq!(oldest, Some(did));
                        }
                    }
                    DiscardCause::Unresumable => {
                        prop_assert!(before_index[&did].unresumable);
                        prop_assert!(matches!(event, ServeEvent::Parked { .. }), "Unresumable discard from non-Parked event");
                    }
                    DiscardCause::TcpDied | DiscardCause::GuardDropped => {}
                }
            }
        }

        match event {
            // I-e / I-f: 非現行leaseを運ぶ事実EventはStateを変えず、Effectも返さず、
            // 決してDiscardを起こさない。
            ServeEvent::Parked { id: pid, lease, .. } | ServeEvent::RelayTerminated { id: pid, lease, .. } => {
                if before_index.get(&pid).map(|e| e.lease) != Some(lease) {
                    prop_assert_eq!(&before, &after, "stale fact mutated state: {:?}", event);
                    prop_assert!(effects.is_empty(), "stale fact produced effects: {:?}", effects);
                }
            }
            ServeEvent::RelayEnded { lease } => {
                match before_index.iter().find(|(_, e)| e.lease == lease) {
                    // I-j: 現Established leaseのRelayEndedは同じapplyでindexエントリも除く。
                    Some((rid, _)) => {
                        prop_assert!(agg.index.get(rid).is_none());
                        prop_assert_eq!(effects.len(), 1);
                    }
                    None => {
                        // indexに無いleaseはEstablishedでもない(I-c)ので、arbiterでも状態は
                        // 変わらない(RelayEndedはEstablishedにしか作用しない)。
                        prop_assert_eq!(&before, &after);
                        prop_assert!(effects.is_empty());
                    }
                }
            }
            // I-i: ResumeGrantedは、parked かつ Established かつ !unresumable のときだけ、
            // そのEstablished leaseで出る。それ以外は状態を変えない。
            ServeEvent::ResumeRequested { id: rid } => {
                let b = before_index.get(&rid).copied();
                match effects.as_slice() {
                    [ServeEffect::ResumeGranted { id: gid, lease }] => {
                        prop_assert_eq!(*gid, rid);
                        let b = b.expect("granted without entry");
                        prop_assert!(b.parked_since.is_some() && !b.unresumable);
                        prop_assert_eq!(b.lease, *lease);
                        let before_est = match &before.0[usize::from(rid[0])] {
                            Some(AttachState::Established { lease, .. }) => Some(*lease),
                            _ => None,
                        };
                        prop_assert_eq!(before_est, Some(*lease));
                        prop_assert_eq!(agg.index[&rid].parked_since, None);
                        prop_assert_eq!(agg.index[&rid].lease, *lease);
                    }
                    [ServeEffect::RequestPreempt { id: gid, lease }] => {
                        prop_assert_eq!(*gid, rid);
                        let b = b.expect("preempt without entry");
                        prop_assert!(b.parked_since.is_none() && !b.unresumable);
                        prop_assert_eq!(b.lease, *lease);
                        prop_assert_eq!(&before, &after);
                    }
                    [ServeEffect::ResumeRejected { id: gid }] => {
                        prop_assert_eq!(*gid, rid);
                        prop_assert!(b.map_or(true, |e| e.unresumable));
                        prop_assert_eq!(&before, &after);
                    }
                    other => prop_assert!(false, "unexpected resume effects {:?}", other),
                }
            }
            ServeEvent::AdmitRequested { key: k } => {
                let known = before.0.get(usize::from(k.session_id.as_bytes()[0])).is_some_and(Option::is_some);
                let any_parked = before_index.values().any(|e| e.parked_since.is_some());
                let rejected_busy = effects.iter().any(|e| {
                    matches!(e, ServeEffect::Attach(AttachEffect::SendReject { reason: AttachRejectReason::BusyOtherSession, .. }))
                });
                if rejected_busy {
                    // 本当に満杯(新規session_id・slot数>=max・立ち退けるparked無し)のときだけ拒否し、
                    // 状態は一切変えない。
                    prop_assert!(!known && before_count >= max_sessions && !any_parked);
                    prop_assert_eq!(effects.len(), 1);
                    prop_assert_eq!(&before, &after);
                } else if !known && before_count >= max_sessions {
                    // 満杯の新規session_idは、同じapplyで最古parkedを1つだけ立ち退かせてから入る
                    // (Evictedの検査は上のDiscardループ)。
                    let evicted = effects.iter().filter(|e| matches!(e, ServeEffect::Discard { .. })).count();
                    prop_assert_eq!(evicted, 1);
                } else {
                    prop_assert!(!effects.iter().any(|e| matches!(e, ServeEffect::Discard { .. })), "admission with room evicted");
                    prop_assert_eq!(&before.1, &after.1);
                }
            }
            ServeEvent::TargetConnected { .. }
            | ServeEvent::TargetConnectFailed { .. }
            | ServeEvent::CancelReceived { .. }
            | ServeEvent::LeaseStopped { .. }
            | ServeEvent::PendingExpired { .. } => {
                // indexを変えない(Established以外にしか作用しない)。
                prop_assert_eq!(&before.1, &after.1);
                prop_assert!(!effects.iter().any(|e| matches!(e, ServeEffect::Discard { .. })), "ATTACH event produced a Discard");
            }
            // Sweepの完全性(レビューD-2): 締切に達したparkedエントリは**全て**除かれ、
            // それ以外(期限前のparked・active・unresumable)は**一切**変わらない。
            // 「何も期限切れにしない」reducerはここで落ちる。
            ServeEvent::Sweep { now, max_parked } => {
                let is_expired = |e: &IndexEntry| {
                    e.parked_since.is_some_and(|since| {
                        Duration::from_millis(now.0.saturating_sub(since.0)) >= effective_deadline(e, max_parked)
                    })
                };
                let expected: BTreeSet<SessionKey> =
                    before_index.iter().filter(|(_, e)| is_expired(e)).map(|(k, _)| *k).collect();
                let discarded: BTreeSet<SessionKey> = effects
                    .iter()
                    .filter_map(|e| match e {
                        ServeEffect::Discard { id, cause: DiscardCause::Expired, .. } => Some(*id),
                        _ => None,
                    })
                    .collect();
                prop_assert_eq!(&discarded, &expected, "sweep did not discard exactly the expired entries");
                let mut survivors = before_index.clone();
                survivors.retain(|k, _| !expected.contains(k));
                prop_assert_eq!(&agg.index, &survivors, "sweep touched a non-expired entry");
                prop_assert!(!agg.index.values().any(is_expired), "an expired parked entry survived the sweep");
            }
            ServeEvent::Activated { .. } => {}
        }
        Ok(())
    }

    proptest! {
        /// I-a〜I-j。admissionを迂回したHELLOも混ぜ、unresumable経路(I-g)にも到達させる。
        #[test]
        fn serve_aggregate_invariants_hold_for_arbitrary_event_sequences(
            (max_sessions, ops) in sequence_strategy(),
        ) {
            run_ops(max_sessions, ops, false)?;
        }

        /// I-k(Step 2b): 実際のshellと同じ入口(`AdmitRequested`)だけからなる任意の列で、
        /// slot数が`max_sessions`を超えない。要求同士・事実との任意の交錯を含む(shellの並行admissionは
        /// 集約ロック下のapplyの列に直列化されるので、この列がその全インターリーブを表す)。
        /// `max_sessions < SESSIONS`に限る: idの種類がmax以下だとreducerが何をしてもslot数はmaxを
        /// 超えられず、そのケースはI-kについて何も検査しない(空振り)ため。
        #[test]
        fn concurrent_admissions_never_exceed_max_sessions(
            max_sessions in 0usize..usize::from(SESSIONS),
            ops in proptest::collection::vec(op_strategy(false), 0..160),
        ) {
            run_ops(max_sessions, ops, true)?;
        }

        /// Sweepの完全性を、主proptestより多いsession数で独立oracle(生の`u64`演算)と照合する
        /// (レビューD-2): 締切に達したparkedエントリ**全て**が、**それだけ**が、slotごと除かれる。
        #[test]
        fn sweep_discards_every_expired_entry_and_only_those(
            entries in proptest::collection::vec(
                (proptest::option::of(0u64..100_000), proptest::option::of(0u32..120)),
                1..8,
            ),
            now in 0u64..200_000,
            max_parked_ms in 0u64..120_000,
        ) {
            let mut agg = ServeAggregate::new(8);
            let mut leases = Vec::new();
            for (s, (park_at, grace)) in entries.iter().enumerate() {
                let s = u8::try_from(s).expect("< 8 sessions");
                let (lease, _) = establish(&mut agg, s, *grace);
                if let Some(at) = park_at {
                    agg.apply(ServeEvent::Parked { id: id(s), lease, now: Millis(*at) });
                }
                leases.push(lease);
            }
            let effects = agg.apply(ServeEvent::Sweep { now: Millis(now), max_parked: Duration::from_millis(max_parked_ms) });
            let mut expired_count = 0;
            for (s, (park_at, grace)) in entries.iter().enumerate() {
                let deadline_ms = match grace {
                    Some(g) => max_parked_ms.min(u64::from(*g) * 1000),
                    None => max_parked_ms,
                };
                let expired = park_at.is_some_and(|at| now.saturating_sub(at) >= deadline_ms);
                expired_count += usize::from(expired);
                let s8 = u8::try_from(s).expect("< 8 sessions");
                let discard = ServeEffect::Discard { id: id(s8), lease: leases[s], cause: DiscardCause::Expired };
                prop_assert_eq!(effects.contains(&discard), expired, "session {} discard mismatch", s);
                prop_assert_eq!(agg.index.contains_key(&id(s8)), !expired, "session {} index mismatch", s);
                prop_assert_eq!(established(&agg, s8).is_some(), !expired, "session {} slot mismatch", s);
            }
            let discards = effects.iter().filter(|e| matches!(e, ServeEffect::Discard { .. })).count();
            prop_assert_eq!(discards, expired_count);
        }

        /// §2.2必須プロパティ: 非単調な`now`列を与えても、park後`max_parked`未満の
        /// エントリに`Discard{Expired}`を出さない(I-h)。now列は任意(独立な一様乱数なので
        /// ほぼ常に非単調だが、単調な列も排除はしない)。時刻逆行は経過0として扱うので、
        /// 締切0(`grace == Some(0)`)のparkだけは逆行時刻のsweepでも満了しうる。
        #[test]
        fn non_monotone_now_never_expires_a_fresh_park(
            park_at in 0u64..1_000_000,
            sweeps in proptest::collection::vec(0u64..2_000_000, 1..20),
            max_parked_ms in 1u64..500_000,
            grace in proptest::option::of(0u32..600),
        ) {
            let mut agg = ServeAggregate::new(4);
            let (lease, _) = establish(&mut agg, 1, grace);
            agg.apply(ServeEvent::Parked { id: id(1), lease, now: Millis(park_at) });
            let deadline = effective_deadline(&agg.index[&id(1)], Duration::from_millis(max_parked_ms));
            for now in sweeps {
                let alive_before = agg.index.contains_key(&id(1));
                let effects = agg.apply(ServeEvent::Sweep { now: Millis(now), max_parked: Duration::from_millis(max_parked_ms) });
                let fresh = Duration::from_millis(now.saturating_sub(park_at)) < deadline;
                if alive_before && fresh {
                    prop_assert!(effects.is_empty(), "fresh park expired at now={} (parked at {})", now, park_at);
                    prop_assert!(agg.index.contains_key(&id(1)));
                }
            }
        }
    }

    // ---- Step 10-2: 有界網羅探索(ADR Q15既定案: 手書きBFS、依存追加なし・Cargo.lock不変) ----
    //
    // 宇宙: session 2個、generation {0,1} × attempt {0,1}、park時刻 {0, 2000}ms、sweep時刻
    // {0, 1000, 2000, ∞}ms(時刻逆行を含む)、`max_parked` 1000ms、grace {なし, 0}、stale lease 1個
    // (一度も発行されないlease。leaseは等値比較にしか使われないので、indexに無い全leaseと同値)。
    // この宇宙で到達可能な状態を**閉包まで**全列挙し、全遷移で`check_transition`(I-a〜I-k・各Eventの
    // 性質、proptestと同じ検査)を、全状態で「宇宙内のEvent列で全slotを解放した状態へ戻れる」
    // (§4.2の有界到達性のサーバー版、always-connectsのslotリーク類型)を検査する。
    // 状態の同一視はleaseの付け替えに関して正規化した指紋で行う(`x_fingerprint`)。

    const X_SESSIONS: u8 = 2;
    const X_MAX_PARKED: Duration = Duration::from_millis(1000);
    /// 閉包に達せずこれを超えたら失敗(宇宙を広げすぎてCI時間を食う変更の検出)。
    const X_STATE_CAP: usize = 100_000;

    #[derive(Debug, Clone, Copy)]
    enum XOp {
        Admit { s: u8, g: u8, at: u8 },
        HelloBypass { s: u8 },
        ConnectCurrent { s: u8 },
        ConnectFailCurrent { s: u8 },
        ActivateCurrent { s: u8, grace: Option<u32> },
        ActivateWrongToken { s: u8 },
        Cancel { s: u8, g: u8, at: u8 },
        LeaseStoppedCurrent { s: u8 },
        PendingExpiredCurrent { s: u8 },
        RelayEndedCurrent { s: u8 },
        ParkedCurrent { s: u8, now: u64 },
        TerminatedCurrent { s: u8, dropped: bool },
        /// stale leaseを運ぶ事実: 0=Parked, 1=RelayTerminated, 2=RelayEnded, 3=LeaseStopped, 4=TargetConnected。
        StaleFact { s: u8, kind: u8 },
        Sweep { now: u64 },
        Resume { s: u8 },
    }

    fn x_ops(bypass: bool) -> Vec<XOp> {
        let mut ops = Vec::new();
        for s in 0..X_SESSIONS {
            for g in 0..2u8 {
                for at in 0..2u8 {
                    ops.push(XOp::Admit { s, g, at });
                    ops.push(XOp::Cancel { s, g, at });
                }
            }
            if bypass {
                ops.push(XOp::HelloBypass { s });
            }
            ops.push(XOp::ConnectCurrent { s });
            ops.push(XOp::ConnectFailCurrent { s });
            ops.push(XOp::ActivateCurrent { s, grace: None });
            ops.push(XOp::ActivateCurrent { s, grace: Some(0) });
            ops.push(XOp::ActivateWrongToken { s });
            ops.push(XOp::LeaseStoppedCurrent { s });
            ops.push(XOp::PendingExpiredCurrent { s });
            ops.push(XOp::RelayEndedCurrent { s });
            ops.push(XOp::ParkedCurrent { s, now: 0 });
            ops.push(XOp::ParkedCurrent { s, now: 2000 });
            ops.push(XOp::TerminatedCurrent { s, dropped: false });
            ops.push(XOp::TerminatedCurrent { s, dropped: true });
            for kind in 0..5u8 {
                ops.push(XOp::StaleFact { s, kind });
            }
            ops.push(XOp::Resume { s });
        }
        for now in [0, 1000, 2000, u64::MAX] {
            ops.push(XOp::Sweep { now });
        }
        ops
    }

    enum XStep {
        Event(ServeEvent),
        Bypass(AttachKey),
    }

    fn x_state(agg: &ServeAggregate, s: u8) -> Option<&AttachState> {
        agg.arbiter.state_for(SessionId::from_bytes(id(s)))
    }

    /// `op`を現在の状態に対して具体的なEventにする(「現在のlease」を要するopは、該当状態でなければ`None`)。
    fn x_resolve(agg: &ServeAggregate, op: XOp, stale: LeaseId) -> Option<XStep> {
        let event = match op {
            XOp::Admit { s, g, at } => ServeEvent::AdmitRequested { key: key(s, u64::from(g), at) },
            XOp::HelloBypass { s } => return Some(XStep::Bypass(key(s, 0, 0))),
            XOp::ConnectCurrent { s } => match x_state(agg, s) {
                Some(AttachState::Connecting { lease, .. }) => ServeEvent::TargetConnected {
                    lease: *lease,
                    target: TargetHandleId(u64::from(s)),
                    attach_token: token(1),
                },
                _ => return None,
            },
            XOp::ConnectFailCurrent { s } => match x_state(agg, s) {
                Some(AttachState::Connecting { lease, .. }) => ServeEvent::TargetConnectFailed { lease: *lease },
                _ => return None,
            },
            XOp::ActivateCurrent { s, grace } => match x_state(agg, s) {
                Some(AttachState::PendingActivation { key: k, attach_token, .. }) => {
                    ServeEvent::Activated { key: *k, attach_token: *attach_token, negotiated_grace_secs: grace }
                }
                _ => return None,
            },
            XOp::ActivateWrongToken { s } => match x_state(agg, s) {
                Some(AttachState::PendingActivation { key: k, .. }) => {
                    ServeEvent::Activated { key: *k, attach_token: token(9), negotiated_grace_secs: None }
                }
                _ => return None,
            },
            XOp::Cancel { s, g, at } => ServeEvent::CancelReceived { key: key(s, u64::from(g), at) },
            XOp::LeaseStoppedCurrent { s } => match x_state(agg, s) {
                Some(AttachState::ClosingForSupersede { old_lease, .. }) => ServeEvent::LeaseStopped { lease: *old_lease },
                _ => return None,
            },
            XOp::PendingExpiredCurrent { s } => match x_state(agg, s) {
                Some(AttachState::PendingActivation { lease, .. }) => ServeEvent::PendingExpired { lease: *lease },
                _ => return None,
            },
            XOp::RelayEndedCurrent { s } => match x_state(agg, s) {
                Some(AttachState::Established { lease, .. }) => ServeEvent::RelayEnded { lease: *lease },
                _ => return None,
            },
            XOp::ParkedCurrent { s, now } => {
                let lease = agg.index.get(&id(s))?.lease;
                ServeEvent::Parked { id: id(s), lease, now: Millis(now) }
            }
            XOp::TerminatedCurrent { s, dropped } => {
                let lease = agg.index.get(&id(s))?.lease;
                let reason = if dropped { TerminateReason::GuardDropped } else { TerminateReason::TcpDied };
                ServeEvent::RelayTerminated { id: id(s), lease, reason }
            }
            XOp::StaleFact { s, kind } => match kind {
                0 => ServeEvent::Parked { id: id(s), lease: stale, now: Millis(0) },
                1 => ServeEvent::RelayTerminated { id: id(s), lease: stale, reason: TerminateReason::GuardDropped },
                2 => ServeEvent::RelayEnded { lease: stale },
                3 => ServeEvent::LeaseStopped { lease: stale },
                _ => ServeEvent::TargetConnected { lease: stale, target: TargetHandleId(9), attach_token: token(1) },
            },
            XOp::Sweep { now } => ServeEvent::Sweep { now: Millis(now), max_parked: X_MAX_PARKED },
            XOp::Resume { s } => ServeEvent::ResumeRequested { id: id(s) },
        };
        Some(XStep::Event(event))
    }

    /// leaseの付け替えに関して正規化した状態の指紋。leaseは等値比較にしか使われない(発行は常に
    /// 現存しない新しい値)ので、固定の走査順で「初出順の番号」に置き換えれば、付け替えで重なる状態は
    /// 同じ指紋になる。pendingのtokenは宇宙内で常に`token(1)`なので省く。
    fn x_fingerprint(agg: &ServeAggregate) -> String {
        let mut leases: Vec<LeaseId> = Vec::new();
        let mut label = |l: LeaseId| -> usize {
            match leases.iter().position(|x| *x == l) {
                Some(i) => i,
                None => {
                    leases.push(l);
                    leases.len() - 1
                }
            }
        };
        let mut out = String::new();
        for s in 0..X_SESSIONS {
            let state = match x_state(agg, s) {
                None => "-".to_owned(),
                Some(AttachState::Connecting { key: k, lease }) => format!("C{k:?}#{}", label(*lease)),
                Some(AttachState::PendingActivation { key: k, lease, target, .. }) => {
                    format!("P{k:?}#{}t{}", label(*lease), target.0)
                }
                Some(AttachState::Established { key: k, lease }) => format!("E{k:?}#{}", label(*lease)),
                Some(AttachState::ClosingForSupersede { old_lease, next }) => format!("X{next:?}#{}", label(*old_lease)),
            };
            let entry = match agg.index.get(&id(s)) {
                None => "-".to_owned(),
                Some(e) => format!(
                    "#{}:{:?}:{:?}:{}",
                    label(e.lease),
                    e.parked_since,
                    e.negotiated_grace_secs,
                    e.unresumable
                ),
            };
            out.push_str(&format!("{state}|{entry};"));
        }
        out
    }

    #[derive(Debug, Default)]
    struct XReport {
        states: usize,
        transitions: usize,
        max_depth: usize,
        parked_states: usize,
        closing_states: usize,
        unresumable_states: usize,
    }

    fn x_annotate(e: TestCaseError, path: &[XOp], op: XOp, max_sessions: usize) -> TestCaseError {
        TestCaseError::fail(format!("{e}\n  reached by {path:?} then {op:?} (max_sessions={max_sessions})"))
    }

    /// 宇宙内の到達可能状態を閉包までBFSし、全遷移・全状態を検査する。
    fn x_explore(max_sessions: usize, bypass: bool) -> Result<XReport, TestCaseError> {
        let stale = fabricated_lease();
        let ops = x_ops(bypass);
        let root = ServeAggregate::new(max_sessions);
        let mut seen: HashMap<String, usize> = HashMap::new();
        seen.insert(x_fingerprint(&root), 0);
        let mut states: Vec<ServeAggregate> = vec![root];
        let mut paths: Vec<Vec<XOp>> = vec![Vec::new()];
        let mut preds: Vec<Vec<usize>> = vec![Vec::new()];
        let mut queue: VecDeque<usize> = VecDeque::from([0]);
        let mut report = XReport::default();

        while let Some(n) = queue.pop_front() {
            report.max_depth = report.max_depth.max(paths[n].len());
            for &op in &ops {
                let mut agg = states[n].clone();
                let Some(step) = x_resolve(&agg, op, stale) else { continue };
                let before_count = agg.arbiter.session_count();
                let before = snapshot(&agg);
                let before_index = agg.index.clone();
                match step {
                    XStep::Event(event) => {
                        let effects = agg.apply(event);
                        let after = snapshot(&agg);
                        check_transition(
                            &agg,
                            max_sessions,
                            !bypass,
                            event,
                            before_count,
                            &before,
                            &after,
                            &before_index,
                            &effects,
                        )
                        .map_err(|e| x_annotate(e, &paths[n], op, max_sessions))?;
                    }
                    XStep::Bypass(k) => {
                        agg.hello_bypassing_admission(k);
                        check_structural_invariants(&agg).map_err(|e| x_annotate(e, &paths[n], op, max_sessions))?;
                    }
                }
                report.transitions += 1;
                let fp = x_fingerprint(&agg);
                let m = match seen.get(&fp) {
                    Some(&m) => m,
                    None => {
                        let m = states.len();
                        prop_assert!(m < X_STATE_CAP, "the exploration exceeded {} states without closing", X_STATE_CAP);
                        if agg.index.values().any(|e| e.parked_since.is_some()) {
                            report.parked_states += 1;
                        }
                        if agg.index.values().any(|e| e.unresumable) {
                            report.unresumable_states += 1;
                        }
                        let closing = (0..X_SESSIONS)
                            .any(|s| matches!(x_state(&agg, s), Some(AttachState::ClosingForSupersede { .. })));
                        if closing {
                            report.closing_states += 1;
                        }
                        let mut path = paths[n].clone();
                        path.push(op);
                        paths.push(path);
                        states.push(agg);
                        preds.push(Vec::new());
                        seen.insert(fp, m);
                        queue.push_back(m);
                        m
                    }
                };
                preds[m].push(n);
            }
        }
        report.states = states.len();

        // 全slotを解放した状態(slot 0・indexが空)へ、宇宙内のEvent列で戻れない状態があってはならない
        // (サーバー側の恒久slotリーク = クライアントの再試行では回復できない、always-connects.md)。
        let mut can_drain: Vec<bool> =
            states.iter().map(|a| a.arbiter.session_count() == 0 && a.index.is_empty()).collect();
        let mut work: VecDeque<usize> = (0..states.len()).filter(|&i| can_drain[i]).collect();
        while let Some(m) = work.pop_front() {
            for &p in &preds[m] {
                if !can_drain[p] {
                    can_drain[p] = true;
                    work.push_back(p);
                }
            }
        }
        let stuck = can_drain.iter().position(|ok| !ok);
        prop_assert!(
            stuck.is_none(),
            "state reached by {:?} can never release all its fencing slots",
            stuck.map(|i| &paths[i])
        );
        Ok(report)
    }

    /// Step 10-2(ADR Q15既定案、手書きBFS): 有界宇宙の全到達状態について、全遷移でI-a〜I-k
    /// (proptestと同じ`check_transition`)、全状態で全slotの解放可能性を証明する。admissionを迂回する
    /// 宇宙(unresumable経路、I-kは検査しない)も別に回す。状態数は閉包に達したことを含めて固定なので、
    /// 結果は決定論的(乱数なし)。
    #[test]
    fn bounded_exhaustive_exploration_of_the_serve_aggregate() {
        for (max_sessions, bypass) in [(1usize, false), (2, false), (1, true)] {
            let report = x_explore(max_sessions, bypass).unwrap_or_else(|e| panic!("{e}"));
            eprintln!("Step 10-2 exhaustive: max_sessions={max_sessions} bypass={bypass}: {report:?}");
            assert!(
                report.parked_states > 0 && report.closing_states > 0,
                "the universe is too small to mean anything: {report:?}"
            );
            assert!(!bypass || report.unresumable_states > 0, "bypass universe never registered unresumable: {report:?}");
        }
    }

    fn bump(hits: &mut BTreeMap<String, usize>, what: &str) {
        *hits.entry(what.to_owned()).or_default() += 1;
    }

    /// レビューD-1: 主proptest(`sequence_strategy`)が空振り(vacuous)していないことの確認。
    /// 同じ生成器から決定論的なseedで主proptestの既定ケース数(256)分の列を生成して実際に適用し、
    /// 不変条件が意味を持つ状態(park・RESUMEの3結果・各`DiscardCause`・unresumable登録・
    /// 同じidの別lease再確立(ABA)・生きているidへのstale事実)に**実際に**到達していることを
    /// 数える。生成器の重みや`resolve`を変えてこれらに届かなくなったら、ここで落ちる。
    #[test]
    fn sequence_strategy_reaches_the_interesting_states() {
        use proptest::strategy::ValueTree;
        use proptest::test_runner::TestRunner;

        let mut runner = TestRunner::deterministic();
        let strategy = sequence_strategy();
        let fabricated = fabricated_lease();
        let mut hits: BTreeMap<String, usize> = BTreeMap::new();
        for _ in 0..256 {
            let (max_sessions, ops) = strategy.new_tree(&mut runner).expect("generate a sequence").current();
            let mut agg = ServeAggregate::new(max_sessions);
            let mut issued: Vec<LeaseId> = Vec::new();
            let mut last_lease: BTreeMap<SessionKey, LeaseId> = BTreeMap::new();
            for op in ops {
                if let Op::HelloBypass { s, g, at } = op {
                    for e in agg.hello_bypassing_admission(key(s, g as u64, at)) {
                        if let ServeEffect::Attach(AttachEffect::ConnectTarget { lease }) = e {
                            issued.push(lease);
                        }
                    }
                    continue;
                }
                let Some(event) = resolve(&agg, &op, &issued, fabricated) else { continue };
                let before_index = agg.index.clone();
                let effects = agg.apply(event);
                if let ServeEvent::AdmitRequested { .. } = event {
                    // `discard Evicted`はバイパス限定のActivated経路でも出るので、admission自身の
                    // 立ち退き分岐(Step 2b)に届いていることは別に数える。
                    if effects.iter().any(|e| matches!(e, ServeEffect::Discard { cause: DiscardCause::Evicted, .. })) {
                        bump(&mut hits, "admission evicted");
                    }
                }
                if let ServeEvent::Parked { id: pid, lease, .. } | ServeEvent::RelayTerminated { id: pid, lease, .. } = event {
                    if before_index.get(&pid).is_some_and(|e| e.lease != lease) {
                        bump(&mut hits, "stale fact for a live id");
                    }
                }
                for e in &effects {
                    match *e {
                        ServeEffect::Attach(AttachEffect::ConnectTarget { lease }) => issued.push(lease),
                        ServeEffect::Attach(AttachEffect::SendReject { reason: AttachRejectReason::BusyOtherSession, .. }) => {
                            bump(&mut hits, "admission rejected busy");
                        }
                        ServeEffect::Attach(_) => {}
                        ServeEffect::RegisterIo { id: rid, lease, unresumable } => {
                            if unresumable {
                                bump(&mut hits, "registered unresumable");
                            }
                            if last_lease.insert(rid, lease).is_some_and(|old| old != lease) {
                                bump(&mut hits, "id re-established with a new lease");
                            }
                        }
                        ServeEffect::StoreParked { .. } => bump(&mut hits, "parked"),
                        ServeEffect::ResumeGranted { .. } => bump(&mut hits, "resume granted"),
                        ServeEffect::RequestPreempt { .. } => bump(&mut hits, "resume preempt"),
                        ServeEffect::ResumeRejected { .. } => bump(&mut hits, "resume rejected"),
                        ServeEffect::Discard { cause, .. } => bump(&mut hits, &format!("discard {cause:?}")),
                    }
                }
            }
        }
        let required = [
            "parked",
            "resume granted",
            "resume preempt",
            "resume rejected",
            "registered unresumable",
            "id re-established with a new lease",
            "stale fact for a live id",
            "admission rejected busy",
            "admission evicted",
            "discard Expired",
            "discard Evicted",
            "discard TcpDied",
            "discard GuardDropped",
            "discard Unresumable",
        ];
        let missing: Vec<&str> = required.iter().copied().filter(|k| !hits.contains_key(*k)).collect();
        assert!(missing.is_empty(), "the generated sequences never reached {missing:?} (hits: {hits:?})");
    }
}
