//! ネットワークpath変化イベント(`SessionOrchestrator::notify_network_path_changed`)を
//! どれだけ即座/どれだけdebounceして扱うかの純粋な判断ロジック。
//! `ios/Sources/IsekaiTerminalCoreLogic/NetworkPathPolicy.swift`の
//! epochベースdebounce-cancelタイマーをRustへ移植したもの(健全性(healthy/degraded)の
//! 分岐はここでは扱わない — 呼び出し側の`orchestrator.rs`が既に持つ`ConnPhase`/`is_quic`で
//! 判断済みの上でこのモジュールを呼ぶ、という役割分担。詳細はPLAN参照)。
//!
//! I/Oなしの純粋なデータ型 + 判断関数として実装し、実際のタイマー(`tokio::time::sleep`)は
//! 呼び出し側(`orchestrator.rs`)が`RUNTIME.spawn`で扱う — `AttachArbiter`系コードと同じ
//! 「判断はpure、I/Oは呼び出し側」の分離。

use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Ignore,
    NotifyAfterDebounce(Duration),
}

#[derive(Debug, Clone, Copy)]
pub struct NetPathPolicy {
    pub debounce: Duration,
}

impl Default for NetPathPolicy {
    fn default() -> Self {
        Self { debounce: Duration::from_millis(400) }
    }
}

impl NetPathPolicy {
    /// `is_satisfied=true`は(呼び出し側の`PathObserver`がepochを進めることで)
    /// 保留中のdebounceがあればキャンセルする以外に意味を持たないため無視する。
    /// `is_satisfied=false`は毎回debounce対象にする — 瞬断で即座に切断扱いにしない。
    pub fn decide(&self, is_satisfied: bool) -> Decision {
        if is_satisfied {
            Decision::Ignore
        } else {
            Decision::NotifyAfterDebounce(self.debounce)
        }
    }
}

pub type Epoch = u64;

/// epochを進めながら`NetPathPolicy`の判断を返す。
#[derive(Debug)]
pub struct PathObserver {
    policy: NetPathPolicy,
    epoch: Epoch,
}

impl Default for PathObserver {
    fn default() -> Self {
        Self::new(NetPathPolicy::default())
    }
}

impl PathObserver {
    pub fn new(policy: NetPathPolicy) -> Self {
        Self { policy, epoch: 0 }
    }

    /// epochを進め、`(新しいepoch, 判断)`を返す。呼び出し側は
    /// `NotifyAfterDebounce`の場合、返ってきたepochを覚えておき、実際に待ってから
    /// `is_current(epoch)`で「その間に新しいupdateが来ていないか」を確認してから行動する。
    pub fn handle_update(&mut self, is_satisfied: bool) -> (Epoch, Decision) {
        self.epoch += 1;
        (self.epoch, self.policy.decide(is_satisfied))
    }

    pub fn is_current(&self, epoch: Epoch) -> bool {
        self.epoch == epoch
    }

    /// 直前のネットワークpathイベントが指していたセッションと、現在のセッションが
    /// 別物になった(新しい接続試行が始まった)ときに呼ぶ。保留中のdebounce発火が
    /// あれば、次に`is_current`を確認する時点で古いものとしてキャンセルされる —
    /// そうしないと、瞬断のdebounce待機中にユーザーが手動で切断/別transportへ
    /// 再接続した場合、無関係な新しいセッションを誤って切断してしまう
    /// (レビューで指摘された実際の不具合)。
    pub fn invalidate(&mut self) {
        self.epoch += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn satisfied_update_is_ignored() {
        let policy = NetPathPolicy { debounce: Duration::from_millis(300) };
        assert_eq!(policy.decide(true), Decision::Ignore);
    }

    #[test]
    fn unsatisfied_update_debounces() {
        let policy = NetPathPolicy { debounce: Duration::from_millis(300) };
        assert_eq!(policy.decide(false), Decision::NotifyAfterDebounce(Duration::from_millis(300)));
    }

    #[test]
    fn epoch_increments_on_every_update() {
        let mut observer = PathObserver::default();
        assert!(observer.is_current(0));
        let (epoch1, _) = observer.handle_update(true);
        assert_eq!(epoch1, 1);
        let (epoch2, _) = observer.handle_update(true);
        assert_eq!(epoch2, 2);
    }

    #[tokio::test]
    async fn debounced_notification_fires_after_delay() {
        let mut observer = PathObserver::new(NetPathPolicy { debounce: Duration::from_millis(50) });
        let (epoch, decision) = observer.handle_update(false);
        assert_eq!(decision, Decision::NotifyAfterDebounce(Duration::from_millis(50)));
        assert!(observer.is_current(epoch), "何も新しいupdateが来ていない間はまだ現行epochのまま");

        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(observer.is_current(epoch), "他のupdateが来ない限りepochは変わらない");
    }

    #[tokio::test]
    async fn newer_update_cancels_pending_debounced_notification() {
        let mut observer = PathObserver::new(NetPathPolicy { debounce: Duration::from_millis(50) });
        let (pending_epoch, decision) = observer.handle_update(false);
        assert_eq!(decision, Decision::NotifyAfterDebounce(Duration::from_millis(50)));

        // 新しいupdateが来た(例: 復旧)ため、保留中だったepochはもう最新ではない。
        let (newer_epoch, _) = observer.handle_update(true);
        assert_ne!(pending_epoch, newer_epoch);

        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!observer.is_current(pending_epoch), "古いepochの発火はキャンセルされたとみなされるべき");
        assert!(observer.is_current(newer_epoch));
    }
}
