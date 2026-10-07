//! 接続プール(`pool.rs`)の1エントリぶんの参照カウントとアイドル削除タイマーの判断
//! (ADR_FUNCTIONAL_CORE_EFFECTS.md §2.2-1 / §6 Step 4)。
//!
//! `pool.rs`は`parking_lot::Mutex`・`tokio::sync::watch`・`RUNTIME`へのspawnを持つshellで、
//! ここはそのうち「いつ削除タイマーをarmし、満了したタイマーで実際に削除してよいか」の判断だけを
//! I/Oなしの§2.1標準形reducerとして切り出したもの。タイマーはtoken付きEffect
//! (`IdleEffect::ArmIdleTimer{generation}`)として返し、shellが満了時に
//! `IdleEvent::IdleExpired{generation}`を戻す。非現行の`generation`(=その間に新規アタッチや
//! 再度の0到達が起きた)の満了はStateを変えず、Effectも返さない(§2.2の必須プロパティ)。
//!
//! キー(`K`)はshell側(`pool.rs`の`PoolEffect::ArmIdleTimer{key, generation, after}`)で付与する:
//! このreducerは1エントリぶんの台帳なので、どのキーの台帳かは呼び出し元のmapが知っている。
//!
//! 既知の制約(Step 4以前からの挙動をそのまま保存している): `generation`はエントリごとに0から
//! 数え直すので、エントリが削除されて同じキーで作り直された後に、削除前にarmされた古いタイマーが
//! 新エントリの現行`generation`と偶然一致して満了すると、新エントリをgrace前に削除しうる
//! (ABA)。削除はrefcount==0のときだけなので、影響は「アイドルなプール接続が早めに閉じられ、
//! 次のタブが新規接続する」に留まる。
// 純粋モジュール(`pure_modules.toml`登録、ADR_FUNCTIONAL_CORE_EFFECTS.md §2.3)。
// 時計・RNG・ロック・I/O型の直接使用を`clippy.toml`の`disallowed-*`で禁止する。
#![deny(clippy::disallowed_methods, clippy::disallowed_types)]

use std::time::Duration;

/// 1プールエントリの参照カウントとアイドルタイマー世代。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct IdleLedger {
    refcount: u32,
    /// アイドルタイマーの世代(stale-guard token)。新規アタッチと0到達のたびに進む。
    generation: u64,
}

/// 台帳に起きた事実。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IdleEvent {
    /// 既存エントリへの新規アタッチ(refcount+1)。pendingな削除タイマーを無効化する。
    Attached,
    /// あるタブがエントリの利用を終えた(refcount-1)。0到達で削除タイマーをarmする。
    Released { idle_grace: Duration },
    /// shellがarmした削除タイマーの満了。`generation`はarm時に受け取ったtoken。
    IdleExpired { generation: u64 },
}

/// shellにやってほしいこと。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IdleEffect {
    /// `after`経過後に`IdleEvent::IdleExpired{generation}`を戻すタイマーをarmする。
    /// 取消Effectは無い(古いタイマーは`generation`不一致で無害化される)。
    ArmIdleTimer { generation: u64, after: Duration },
    /// このエントリをプールから削除する。
    Remove,
}

impl IdleLedger {
    /// 新規エントリを作ったアタッチ(=最初の保持者)の台帳。
    pub(crate) const fn first_holder() -> Self {
        Self { refcount: 1, generation: 0 }
    }

    #[cfg(test)]
    pub(crate) fn refcount(&self) -> u32 {
        self.refcount
    }

    /// 現行の世代(テストのモデル照合用。shellは`ArmIdleTimer`で受け取ったtokenだけを使う)。
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }

    pub(crate) fn apply(&mut self, ev: IdleEvent) -> Vec<IdleEffect> {
        match ev {
            IdleEvent::Attached => {
                self.refcount = self.refcount.saturating_add(1);
                self.generation = self.generation.wrapping_add(1);
                Vec::new()
            }
            IdleEvent::Released { idle_grace } => {
                self.refcount = self.refcount.saturating_sub(1);
                if self.refcount != 0 {
                    return Vec::new();
                }
                self.generation = self.generation.wrapping_add(1);
                vec![IdleEffect::ArmIdleTimer { generation: self.generation, after: idle_grace }]
            }
            IdleEvent::IdleExpired { generation } => {
                if self.refcount == 0 && self.generation == generation {
                    vec![IdleEffect::Remove]
                } else {
                    Vec::new()
                }
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, clippy::disallowed_types)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const GRACE: Duration = Duration::from_secs(30);

    #[test]
    fn release_to_zero_arms_a_timer_with_the_new_generation() {
        let mut l = IdleLedger::first_holder();
        assert_eq!(
            l.apply(IdleEvent::Released { idle_grace: GRACE }),
            vec![IdleEffect::ArmIdleTimer { generation: 1, after: GRACE }]
        );
        assert_eq!(l.apply(IdleEvent::IdleExpired { generation: 1 }), vec![IdleEffect::Remove]);
    }

    #[test]
    fn release_with_remaining_holders_arms_nothing() {
        let mut l = IdleLedger::first_holder();
        assert!(l.apply(IdleEvent::Attached).is_empty());
        assert!(l.apply(IdleEvent::Released { idle_grace: GRACE }).is_empty());
        assert_eq!(l.refcount(), 1);
    }

    #[test]
    fn reattach_then_release_makes_the_first_timer_stale() {
        let mut l = IdleLedger::first_holder();
        let first = l.apply(IdleEvent::Released { idle_grace: GRACE });
        assert!(l.apply(IdleEvent::Attached).is_empty());
        let second = l.apply(IdleEvent::Released { idle_grace: GRACE });
        let (IdleEffect::ArmIdleTimer { generation: g1, .. }, IdleEffect::ArmIdleTimer { generation: g2, .. }) =
            (first[0], second[0])
        else {
            panic!("both releases to zero must arm a timer");
        };
        assert_ne!(g1, g2);
        let before = l;
        assert!(l.apply(IdleEvent::IdleExpired { generation: g1 }).is_empty(), "stale timer must be a no-op");
        assert_eq!(l, before);
        assert_eq!(l.apply(IdleEvent::IdleExpired { generation: g2 }), vec![IdleEffect::Remove]);
    }

    #[test]
    fn current_generation_expiry_with_a_holder_is_a_no_op() {
        // 0到達→arm→(新規アタッチは世代を進めるので)同じ世代のまま保持者が居る状態は作れないが、
        // reducerは防御的にrefcount>0なら削除しない。
        let mut l = IdleLedger { refcount: 1, generation: 5 };
        assert!(l.apply(IdleEvent::IdleExpired { generation: 5 }).is_empty());
        assert_eq!(l, IdleLedger { refcount: 1, generation: 5 });
    }

    #[derive(Debug, Clone, Copy)]
    enum Op {
        Attach,
        Release,
        /// 過去にarmされたtokenのどれか(index、armed列の長さで剰余)で満了させる。
        FireArmed(usize),
        /// 任意のtoken値で満了させる(armされていない値も含む)。
        FireArbitrary(u64),
    }

    fn op_strategy() -> impl Strategy<Value = Op> {
        prop_oneof![
            3 => Just(Op::Attach),
            3 => Just(Op::Release),
            2 => any::<usize>().prop_map(Op::FireArmed),
            1 => (0u64..12).prop_map(Op::FireArbitrary),
        ]
    }

    proptest! {
        /// §2.2必須プロパティ: 非現行tokenの`IdleExpired`はStateを変えず、Effectも返さない。
        /// 加えて、refcountはアタッチ-リリースと一致し、`Remove`は「refcount==0かつ現行token」の
        /// 満了でだけ返り、armされるtokenは常に新しい現行世代である。
        #[test]
        fn stale_idle_expired_never_changes_state(ops in proptest::collection::vec(op_strategy(), 1..80)) {
            let mut l = IdleLedger::first_holder();
            let mut holders: u32 = 1;
            let mut armed: Vec<u64> = Vec::new();
            // Step 11: 満了したタイマーの(token, 現行token, Effect数)の列を`trace_invariants`で検査する。
            let mut trace: Vec<crate::trace_invariants::TraceEvent> = Vec::new();
            for op in ops {
                let before = l;
                let ev = match op {
                    Op::Attach => IdleEvent::Attached,
                    Op::Release => IdleEvent::Released { idle_grace: GRACE },
                    Op::FireArmed(i) if !armed.is_empty() => IdleEvent::IdleExpired { generation: armed[i % armed.len()] },
                    Op::FireArmed(_) => continue,
                    Op::FireArbitrary(g) => IdleEvent::IdleExpired { generation: g },
                };
                let fx = l.apply(ev);
                if let IdleEvent::IdleExpired { generation } = ev {
                    trace.push(crate::trace_invariants::TraceEvent::TimerFired {
                        token: generation,
                        current: before.generation(),
                        effects: fx.len(),
                    });
                }
                match ev {
                    IdleEvent::Attached => {
                        holders += 1;
                        prop_assert!(fx.is_empty(), "Attached must not emit effects");
                        prop_assert_ne!(l.generation(), before.generation(), "Attached must advance the generation");
                    }
                    IdleEvent::Released { .. } => {
                        holders = holders.saturating_sub(1);
                        if holders == 0 {
                            prop_assert_eq!(fx.clone(), vec![IdleEffect::ArmIdleTimer { generation: l.generation(), after: GRACE }], "release to zero must arm the current generation");
                            // tokenは1台帳の中では再利用されない(古いタイマーと取り違えない)。
                            prop_assert!(!armed.contains(&l.generation()), "a token must never be reused");
                            armed.push(l.generation());
                        } else {
                            prop_assert!(fx.is_empty(), "release with holders left must not arm");
                        }
                    }
                    IdleEvent::IdleExpired { generation } => {
                        prop_assert_eq!(l, before, "IdleExpired never mutates the ledger");
                        if generation != before.generation() {
                            prop_assert!(fx.is_empty(), "stale token must not emit effects");
                        } else if before.refcount() == 0 {
                            prop_assert_eq!(fx.clone(), vec![IdleEffect::Remove], "current-token expiry at zero must remove");
                        } else {
                            prop_assert!(fx.is_empty(), "expiry with holders must not remove");
                        }
                    }
                }
                prop_assert_eq!(l.refcount(), holders, "refcount must match attaches minus releases");
            }
            if let Err(violation) = crate::trace_invariants::check_trace(&trace) {
                prop_assert!(false, "Step 11 trace invariant violated: {} / trace: {:?}", violation, trace);
            }
        }
    }
}
