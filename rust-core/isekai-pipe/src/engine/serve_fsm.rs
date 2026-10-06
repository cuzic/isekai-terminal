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
//! 不変条件(§4.1 I-a〜I-j)はこのモジュールのproptestで検証する。
// 純粋モジュール(`pure_modules.toml`登録、ADR_FUNCTIONAL_CORE_EFFECTS.md §2.3)。
#![deny(clippy::disallowed_methods, clippy::disallowed_types)]

use std::collections::BTreeMap;
use std::time::Duration;

use isekai_protocol::attach::{AttachKey, AttachToken};
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
    // ---- ATTACH v2(`AttachArbiter`へ委譲するもの) ----
    Hello { key: AttachKey },
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
    /// admissionのための立ち退き要求(旧`SessionTable::claim_oldest_parked`)。
    /// admissionのcheck-then-act自体の解消はStep 2bの範囲。
    EvictOldestParked,
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

    pub fn max_sessions(&self) -> usize {
        self.max_sessions
    }

    /// 読み取りクエリ(admissionの`session_count`/`has_session`、`established_lease_for`等)。
    pub fn arbiter(&self) -> &AttachArbiter {
        &self.arbiter
    }

    pub fn index_entry(&self, id: &SessionKey) -> Option<&IndexEntry> {
        self.index.get(id)
    }

    pub fn apply(&mut self, event: ServeEvent) -> Vec<ServeEffect> {
        match event {
            ServeEvent::Hello { key } => self.forward(AttachEvent::HelloReceived { key }),
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
            ServeEvent::EvictOldestParked => self.on_evict_oldest_parked(),
        }
    }

    /// `Established`に触れない(=indexと無関係な)ATTACH Eventをarbiterへそのまま渡す。
    /// `HelloReceived`は`Established`を拒否するだけ、他は`Connecting`/`PendingActivation`/
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

    fn on_evict_oldest_parked(&mut self) -> Vec<ServeEffect> {
        match self.oldest_parked() {
            Some(victim) => self.discard(victim, DiscardCause::Evicted),
            None => vec![],
        }
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
    use std::collections::HashSet;

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

    /// HELLO → TargetConnected → Activated で`s`を`Established`にし、そのleaseを返す。
    fn establish(agg: &mut ServeAggregate, s: u8, grace: Option<u32>) -> (LeaseId, Vec<ServeEffect>) {
        let k = key(s, 0, 1);
        let lease = match agg.apply(ServeEvent::Hello { key: k }).as_slice() {
            [ServeEffect::Attach(AttachEffect::ConnectTarget { lease })] => *lease,
            other => panic!("unexpected hello effects {other:?}"),
        };
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

    #[test]
    fn evict_oldest_parked_returns_nothing_when_nothing_is_parked() {
        let mut agg = ServeAggregate::new(8);
        establish(&mut agg, 1, None);
        assert_eq!(agg.apply(ServeEvent::EvictOldestParked), vec![]);
        assert!(agg.index_entry(&id(1)).is_some(), "an active session must never be evicted");
    }

    #[test]
    fn evict_oldest_parked_picks_the_oldest_with_id_tie_break() {
        let mut agg = ServeAggregate::new(8);
        let (l3, _) = establish(&mut agg, 3, None);
        let (l2, _) = establish(&mut agg, 2, None);
        let (l1, _) = establish(&mut agg, 1, None);
        agg.apply(ServeEvent::Parked { id: id(3), lease: l3, now: Millis(10) });
        agg.apply(ServeEvent::Parked { id: id(2), lease: l2, now: Millis(10) });
        agg.apply(ServeEvent::Parked { id: id(1), lease: l1, now: Millis(20) });
        // parked_sinceが同じ(10)なら小さいidが先(HashMap反復順に依存しない)。
        assert_eq!(
            agg.apply(ServeEvent::EvictOldestParked),
            vec![ServeEffect::Discard { id: id(2), lease: l2, cause: DiscardCause::Evicted }]
        );
        assert_eq!(established(&agg, 2), None);
    }

    #[test]
    fn activation_when_full_evicts_oldest_parked_first() {
        let mut agg = ServeAggregate::new(2);
        let (older, _) = establish(&mut agg, 1, None);
        let (newer, _) = establish(&mut agg, 2, None);
        agg.apply(ServeEvent::Parked { id: id(1), lease: older, now: Millis(0) });
        agg.apply(ServeEvent::Parked { id: id(2), lease: newer, now: Millis(10) });
        let (lease3, effects) = establish(&mut agg, 3, None);
        assert_eq!(effects[0], ServeEffect::Discard { id: id(1), lease: older, cause: DiscardCause::Evicted });
        assert_eq!(effects[1], ServeEffect::RegisterIo { id: id(3), lease: lease3, unresumable: false });
        assert_eq!(established(&agg, 1), None);
        assert!(agg.index_entry(&id(2)).is_some());
    }

    #[test]
    fn activation_when_full_of_active_sessions_registers_unresumable() {
        let mut agg = ServeAggregate::new(1);
        let (active, _) = establish(&mut agg, 1, None);
        let (lease2, effects) = establish(&mut agg, 2, None);
        assert_eq!(effects[0], ServeEffect::RegisterIo { id: id(2), lease: lease2, unresumable: true });
        assert!(agg.index_entry(&id(1)).is_some(), "active sessions are never evicted");
        assert_eq!(established(&agg, 1), Some(active));
        // 現状どおりRESUME不可。
        agg.apply(ServeEvent::Parked { id: id(1), lease: active, now: Millis(0) });
        assert_eq!(agg.apply(ServeEvent::ResumeRequested { id: id(2) }), vec![ServeEffect::ResumeRejected { id: id(2) }]);
        // LRU対象外(parkedのid(1)が選ばれる)。
        assert_eq!(
            agg.apply(ServeEvent::EvictOldestParked),
            vec![ServeEffect::Discard { id: id(1), lease: active, cause: DiscardCause::Evicted }]
        );
    }

    #[test]
    fn parking_an_unresumable_entry_discards_it_and_frees_the_slot() {
        // Step 2aの意図した挙動変更その2(I-g、旧: 孤児park→恒久的なslotリーク)。
        let mut agg = ServeAggregate::new(1);
        establish(&mut agg, 1, None);
        let (lease2, _) = establish(&mut agg, 2, None);
        assert_eq!(
            agg.apply(ServeEvent::Parked { id: id(2), lease: lease2, now: Millis(0) }),
            vec![ServeEffect::Discard { id: id(2), lease: lease2, cause: DiscardCause::Unresumable }]
        );
        assert_eq!(established(&agg, 2), None);
        assert_eq!(agg.index_entry(&id(2)), None);
        // 同じsession_idの再ATTACHが受理される(旧: AttachAlreadyEstablishedで永久拒否)。
        assert!(matches!(
            agg.apply(ServeEvent::Hello { key: key(2, 1, 1) }).as_slice(),
            [ServeEffect::Attach(AttachEffect::ConnectTarget { .. })]
        ));
    }

    // ---- proptest: §4.1 I-a〜I-j(任意Event列、非単調now・stale lease・id再利用・要求と事実の交錯) ----

    const SESSIONS: u8 = 3;

    #[derive(Debug, Clone)]
    enum Op {
        Hello { s: u8, g: u8, at: u8 },
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
        Evict,
    }

    fn op_strategy() -> impl Strategy<Value = Op> {
        let s = || 0u8..SESSIONS;
        let l = || 0usize..64;
        // 非単調な`now`: 各Eventが独立に任意の値を取る(後のEventほど小さいこともある)。
        let now = || 0u64..100_000;
        prop_oneof![
            3 => (s(), 0u8..3, 0u8..2).prop_map(|(s, g, at)| Op::Hello { s, g, at }),
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
            1 => Just(Op::Evict),
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

    type Snapshot = (Vec<Option<AttachState>>, Vec<(SessionKey, IndexEntry)>);

    fn snapshot(agg: &ServeAggregate) -> Snapshot {
        let states = (0..SESSIONS).map(|s| agg.arbiter.state_for(SessionId::from_bytes(id(s))).cloned()).collect();
        let index = agg.index.iter().map(|(k, v)| (*k, *v)).collect();
        (states, index)
    }

    fn resolve(agg: &ServeAggregate, op: &Op, issued: &[LeaseId], fabricated: LeaseId) -> Option<ServeEvent> {
        Some(match *op {
            Op::Hello { s, g, at } => ServeEvent::Hello { key: key(s, g as u64, at) },
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
            Op::Evict => ServeEvent::EvictOldestParked,
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

    proptest! {
        #[test]
        fn serve_aggregate_invariants_hold_for_arbitrary_event_sequences(
            max_sessions in 0usize..4,
            ops in proptest::collection::vec(op_strategy(), 0..120),
        ) {
            let mut agg = ServeAggregate::new(max_sessions);
            let mut issued: Vec<LeaseId> = Vec::new();
            let mut issued_set: HashSet<LeaseId> = HashSet::new();
            // 一度も発行されないlease: 発行済みleaseを100万個ずらした値は得られないので、
            // 「別のServeAggregateで十分先まで発行したlease」を使う。
            let fabricated = {
                let mut other = ServeAggregate::new(0);
                let mut last = None;
                for n in 0..=200u64 {
                    let k = AttachKey {
                        session_id: SessionId::from_bytes([0xFF; 16]),
                        generation: ConnectionGeneration::new(n),
                        attempt_id: AttemptId::from_bytes([0xFF; 16]),
                    };
                    for e in other.apply(ServeEvent::Hello { key: k }) {
                        if let ServeEffect::Attach(AttachEffect::ConnectTarget { lease }) = e { last = Some(lease); }
                        if let ServeEffect::Attach(AttachEffect::CancelLease { lease }) = e {
                            for e2 in other.apply(ServeEvent::LeaseStopped { lease }) {
                                if let ServeEffect::Attach(AttachEffect::ConnectTarget { lease }) = e2 { last = Some(lease); }
                            }
                        }
                    }
                }
                last.expect("fabricated lease")
            };

            for op in ops {
                let Some(event) = resolve(&agg, &op, &issued, fabricated) else { continue };
                let before = snapshot(&agg);
                let before_index = agg.index.clone();
                let effects = agg.apply(event);
                let after = snapshot(&agg);

                for e in &effects {
                    if let ServeEffect::Attach(AttachEffect::ConnectTarget { lease }) = e {
                        prop_assert!(issued_set.insert(*lease), "lease minted twice");
                        prop_assert_ne!(*lease, fabricated);
                        issued.push(*lease);
                    }
                }

                check_structural_invariants(&agg)?;

                for e in &effects {
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
                                    prop_assert!(now.0 >= since.0, "expired with time going backwards");
                                    prop_assert!(Duration::from_millis(now.0 - since.0) >= deadline);
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
                    ServeEvent::Hello { .. }
                    | ServeEvent::TargetConnected { .. }
                    | ServeEvent::TargetConnectFailed { .. }
                    | ServeEvent::CancelReceived { .. }
                    | ServeEvent::LeaseStopped { .. }
                    | ServeEvent::PendingExpired { .. } => {
                        // indexを変えない(Established以外にしか作用しない)。
                        prop_assert_eq!(&before.1, &after.1);
                        prop_assert!(!effects.iter().any(|e| matches!(e, ServeEffect::Discard { .. })), "ATTACH event produced a Discard");
                    }
                    ServeEvent::Activated { .. }
                    | ServeEvent::Sweep { .. }
                    | ServeEvent::EvictOldestParked => {}
                }
            }
        }

        /// §2.2必須プロパティ: 非単調な`now`列を与えても、park後`max_parked`未満の
        /// エントリに`Discard{Expired}`を出さない(I-h)。単調でないnow列だけを生成する。
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
                let fresh = now < park_at || Duration::from_millis(now - park_at) < deadline;
                if alive_before && fresh {
                    prop_assert!(effects.is_empty(), "fresh park expired at now={} (parked at {})", now, park_at);
                    prop_assert!(agg.index.contains_key(&id(1)));
                }
            }
        }
    }
}
