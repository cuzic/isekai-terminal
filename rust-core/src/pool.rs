//! SSH接続プーリング(archive/ISEKAI_SSH_DESIGN.md「2026-07-07: 上記オープンな課題の調査・
//! 設計確定」節で確定した設計の実装)。
//!
//! 複数タブが同一ホスト/ユーザー/鍵(+isekai-pipe系ならQUIC確立方式)へ接続する場合、
//! 認証済みの`client::Handle`(プレーンSSH)またはネストしたSSH `client::Handle`
//! (isekai-pipe QUIC系)を使い回し、2本目以降のタブは`channel_open_session()`だけで
//! 開始できるようにする。判断ロジックは全てここ(Rust側)に閉じ、Kotlin側は一切関知しない
//! (`.claude/rules/rust-ssot.md`)。
//!
//! ここに置くのはtransport非依存の汎用プリミティブ(`try_attach_with`/`wait_for_establish`/
//! `publish_success`/`publish_failure`/`release`)と、プレーンSSH用の`SshPoolKey`・
//! `SSH_POOL`。isekai-pipe QUIC系のキー・プールstaticは`isekai_pipe_quic_transport.rs`に置く
//! (この関心は`pool.rs`ではなく個々のtransportモジュールに閉じる方が自然なため)。

use std::collections::HashMap;
use std::hash::Hash;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use parking_lot::Mutex;
use tokio::sync::watch;

use crate::transport::PooledSshHandle;
use crate::{JumpConfig, SshAuth};

// ── 汎用プールプリミティブ ─────────────────────────────

enum EntryState<T> {
    /// 確立中。`watch::Sender`経由で結果を待っているタブへブロードキャストする。
    Connecting(watch::Sender<Option<Result<Arc<T>, String>>>),
    Ready(Arc<T>),
    /// 外部経路で死んだことが分かったエントリ。値は保持しない。
    Dead,
}

pub(crate) struct PoolEntry<T> {
    state: EntryState<T>,
    refcount: u32,
    /// アイドルタイマーの世代。`release`が0への到達時にインクリメントしてタイマーを
    /// spawnする。新規アタッチ(`try_attach_with`)や再度の0到達でも進む。タイマー発火時に
    /// 世代が一致しなければ「その間に別のイベントが起きた」ことを意味するので何もしない
    /// (`AbortHandle`を持ち回らずに古いタイマーを無効化する)。
    idle_generation: u64,
}

pub(crate) type PoolMap<K, T> = Mutex<HashMap<K, PoolEntry<T>>>;

pub(crate) fn new_pool_map<K, T>() -> PoolMap<K, T> {
    Mutex::new(HashMap::new())
}

/// [try_attach_with]の結果。
///
/// どの結果であっても、呼び出し元はこのアタッチに対応する[release]を必ず1回呼ぶ。
/// [AttachOutcome::Establisher]の場合は、確立結果を[publish_success]または
/// [publish_failure]で公開したうえで、失敗分岐でも[release]を呼ぶ必要がある。
pub(crate) enum AttachOutcome<T> {
    /// 既存の確立済みエントリを再利用できる。
    Ready(Arc<T>),
    /// 別のタブが確立中。その完了を[wait_for_establish]で待つ。
    Waiter(watch::Receiver<Option<Result<Arc<T>, String>>>),
    /// このタブが確立を担当する。
    Establisher,
}

/// [key]に対応するエントリを検索し、無ければ`Connecting`のプレースホルダを作って
/// 呼び出し元を確立担当にする。既存エントリがあれば参照カウントを増やし、アイドル
/// タイマーを無効化する(新規アタッチなので古い削除タイマーは意味を失う)。
/// `Ready`エントリは[is_alive]で生存確認し、死んでいれば同じスロットを
/// `Connecting`へ差し替えて呼び出し元を確立担当にする。
pub(crate) fn try_attach_with<K, T>(
    pool: &PoolMap<K, T>,
    key: &K,
    is_alive: impl Fn(&T) -> bool,
) -> AttachOutcome<T>
where
    K: Hash + Eq + Clone,
{
    let mut map = pool.lock();
    match map.get_mut(key) {
        None => {
            let (tx, _rx) = watch::channel(None);
            map.insert(
                key.clone(),
                PoolEntry { state: EntryState::Connecting(tx), refcount: 1, idle_generation: 0 },
            );
            AttachOutcome::Establisher
        }
        Some(entry) => {
            entry.refcount += 1;
            entry.idle_generation = entry.idle_generation.wrapping_add(1);
            match &entry.state {
                EntryState::Connecting(tx) => AttachOutcome::Waiter(tx.subscribe()),
                EntryState::Ready(v) if is_alive(v) => AttachOutcome::Ready(v.clone()),
                EntryState::Ready(_) | EntryState::Dead => {
                    let (tx, _rx) = watch::channel(None);
                    entry.state = EntryState::Connecting(tx);
                    AttachOutcome::Establisher
                }
            }
        }
    }
}

/// [AttachOutcome::Waiter]を受け取ったタブが、確立担当タブの結果を待つ。
pub(crate) async fn wait_for_establish<T>(
    mut rx: watch::Receiver<Option<Result<Arc<T>, String>>>,
) -> Result<Arc<T>, String> {
    loop {
        if let Some(result) = rx.borrow_and_update().clone() {
            return result;
        }
        if rx.changed().await.is_err() {
            return Err("pool: establishing task ended without a result".to_string());
        }
    }
}

/// 確立担当タブが接続確立に成功した時に呼ぶ。エントリを`Ready`にし、待機中の全タブへ
/// 結果をブロードキャストする。
pub(crate) fn publish_success<K, T>(pool: &PoolMap<K, T>, key: &K, value: T) -> Arc<T>
where
    K: Hash + Eq + Clone,
{
    let arc = Arc::new(value);
    let mut map = pool.lock();
    if let Some(entry) = map.get_mut(key) {
        if let EntryState::Connecting(tx) = &entry.state {
            let _ = tx.send(Some(Ok(arc.clone())));
        }
        entry.state = EntryState::Ready(arc.clone());
    }
    arc
}

/// 確立担当タブが接続確立に失敗した時に呼ぶ。エントリをtombstone化し、待機中の全タブへ
/// 同じエラーをブロードキャストする。呼び出し元自身も含め、この失敗したアタッチに
/// 対応する[release]は別途必ず1回呼ぶ。
pub(crate) fn publish_failure<K, T>(pool: &PoolMap<K, T>, key: &K, message: String)
where
    K: Hash + Eq + Clone,
{
    let mut map = pool.lock();
    if let Some(entry) = map.get(key) {
        if let EntryState::Connecting(tx) = &entry.state {
            let _ = tx.send(Some(Err(message)));
        }
    }
    if let Some(entry) = map.get_mut(key) {
        entry.state = EntryState::Dead;
    }
}

/// 外部経路で既存エントリが死んだと分かった時に呼ぶ。refcount/idle_generationは触らず、
/// 通常の[release]が最後の保持者から呼ばれることで削除タイマーをarmする。
/// 既に別の確立が始まっていたり、新しい値へ置き換わっていたりする場合は触らない。
pub(crate) fn mark_dead_if_same<K, T>(pool: &PoolMap<K, T>, key: &K, value: &Arc<T>)
where
    K: Hash + Eq + Clone,
{
    let mut map = pool.lock();
    if let Some(entry) = map.get_mut(key) {
        if matches!(&entry.state, EntryState::Ready(current) if Arc::ptr_eq(current, value)) {
            entry.state = EntryState::Dead;
        }
    }
}

/// あるタブがチャネル(接続)の利用を終えた時に呼ぶ。参照カウントを減らし、0に
/// 到達したら即座にエントリを消さず、`idle_grace`だけ猶予を置いてから
/// (その間に新規アタッチが無ければ)削除する。
///
/// `pool`は`'static`参照であることを要求する(プロセス全体シングルトンの`LazyLock`
/// static以外から呼ぶ想定が無いため)。
pub(crate) fn release<K, T>(pool: &'static PoolMap<K, T>, key: K, idle_grace: Duration)
where
    K: Hash + Eq + Clone + Send + Sync + 'static,
    T: Send + Sync + 'static,
{
    let mut map = pool.lock();
    let Some(entry) = map.get_mut(&key) else { return };
    entry.refcount = entry.refcount.saturating_sub(1);
    if entry.refcount != 0 {
        return;
    }
    entry.idle_generation = entry.idle_generation.wrapping_add(1);
    let my_generation = entry.idle_generation;
    drop(map);
    crate::RUNTIME.spawn(async move {
        tokio::time::sleep(idle_grace).await;
        let mut map = pool.lock();
        if let Some(entry) = map.get(&key) {
            if entry.refcount == 0 && entry.idle_generation == my_generation {
                map.remove(&key);
            }
        }
    });
}

// ── プレーンSSH用プールキー ────────────────────────────

/// 認証済み`client::Handle`を複数タブで共有してよいかを決める識別子。
/// フィールドの根拠は`archive/ISEKAI_SSH_DESIGN.md`の該当節を参照。
#[derive(Clone, PartialEq, Eq, Hash)]
pub(crate) struct SshPoolKey {
    host: String,
    port: u16,
    username: String,
    /// 公開鍵のSHA256フィンガープリント。パスワード認証は`for_target`が`None`を返す
    /// (常にプール対象外)ため、この型が存在する時点で必ず公開鍵認証である。
    auth_identity: String,
    agent_forward: bool,
    jump: Option<Box<SshPoolKey>>,
}

impl SshPoolKey {
    /// パスワード認証の場合や、公開鍵のパースに失敗した場合は`None`(=プール対象外、
    /// 呼び出し元は毎回新規接続する)。
    pub(crate) fn for_target(
        host: &str,
        port: u16,
        username: &str,
        auth: &SshAuth,
        agent_forward: bool,
        jump: &Option<JumpConfig>,
    ) -> Option<SshPoolKey> {
        let auth_identity = auth_identity_fingerprint(auth)?;
        let jump_key = match jump {
            None => None,
            Some(j) => Some(Box::new(SshPoolKey::for_target(
                &j.host, j.port, &j.username, &j.auth, false, &None,
            )?)),
        };
        Some(SshPoolKey {
            host: host.to_string(),
            port,
            username: username.to_string(),
            auth_identity,
            agent_forward,
            jump: jump_key,
        })
    }
}

pub(crate) fn auth_identity_fingerprint(auth: &SshAuth) -> Option<String> {
    match auth {
        SshAuth::Password { .. } => None,
        SshAuth::PublicKey { private_key_pem } => {
            russh_keys::PrivateKey::from_openssh(private_key_pem)
                .ok()
                .map(|k| k.public_key().fingerprint(russh_keys::HashAlg::Sha256).to_string())
        }
    }
}

pub(crate) static SSH_POOL: LazyLock<PoolMap<SshPoolKey, PooledSshHandle>> =
    LazyLock::new(new_pool_map);

/// プールエントリの共有Handleを閉じる前に持たせる猶予時間。タブを閉じてすぐ開き直す
/// (ホスト鍵確認後の再接続、タブの素早い開閉等)程度のケースを吸収する。
/// 値の根拠は`archive/ISEKAI_SSH_DESIGN.md`参照。ユーザー向け設定は設けない。
pub(crate) const PLAIN_SSH_IDLE_GRACE: Duration = Duration::from_secs(30);

#[cfg(test)]
mod tests {
    use super::*;

    fn alive(_: &u32) -> bool {
        true
    }

    async fn wait_until_removed(key: &'static str) -> bool {
        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(20)).await;
            if !RELEASE_TEST_POOL.lock().contains_key(&key) {
                return true;
            }
        }
        false
    }

    fn password_auth() -> SshAuth {
        SshAuth::Password { password: "hunter2".into() }
    }

    fn key_auth(seed: u8) -> SshAuth {
        use russh_keys::ssh_key::private::Ed25519Keypair;
        use russh_keys::PrivateKey;
        let keypair = Ed25519Keypair::from_seed(&[seed; 32]);
        let key = PrivateKey::from(keypair);
        SshAuth::PublicKey {
            private_key_pem: key.to_openssh(Default::default()).unwrap().as_bytes().to_vec(),
        }
    }

    #[test]
    fn password_auth_never_produces_a_pool_key() {
        let key = SshPoolKey::for_target("host", 22, "user", &password_auth(), false, &None);
        assert!(key.is_none());
    }

    #[test]
    fn same_pubkey_and_target_produce_equal_keys() {
        let a = SshPoolKey::for_target("host", 22, "user", &key_auth(1), false, &None).unwrap();
        let b = SshPoolKey::for_target("host", 22, "user", &key_auth(1), false, &None).unwrap();
        assert!(a == b);
    }

    #[test]
    fn different_keys_produce_different_pool_keys() {
        let a = SshPoolKey::for_target("host", 22, "user", &key_auth(1), false, &None).unwrap();
        let b = SshPoolKey::for_target("host", 22, "user", &key_auth(2), false, &None).unwrap();
        assert!(a != b);
    }

    #[test]
    fn different_agent_forward_produces_different_pool_keys() {
        let a = SshPoolKey::for_target("host", 22, "user", &key_auth(1), false, &None).unwrap();
        let b = SshPoolKey::for_target("host", 22, "user", &key_auth(1), true, &None).unwrap();
        assert!(a != b);
    }

    #[test]
    fn different_jump_produces_different_pool_keys() {
        let jump_a = JumpConfig { host: "jump-a".into(), port: 22, username: "j".into(), auth: key_auth(9) };
        let jump_b = JumpConfig { host: "jump-b".into(), port: 22, username: "j".into(), auth: key_auth(9) };
        let a = SshPoolKey::for_target("host", 22, "user", &key_auth(1), false, &Some(jump_a)).unwrap();
        let b = SshPoolKey::for_target("host", 22, "user", &key_auth(1), false, &Some(jump_b)).unwrap();
        assert!(a != b);
    }

    #[tokio::test]
    async fn try_attach_first_caller_becomes_establisher_second_becomes_waiter() {
        let pool: PoolMap<&'static str, u32> = new_pool_map();
        match try_attach_with(&pool, &"k", alive) {
            AttachOutcome::Establisher => {}
            _ => panic!("first attach should be Establisher"),
        }
        match try_attach_with(&pool, &"k", alive) {
            AttachOutcome::Waiter(_) => {}
            _ => panic!("second attach while connecting should be Waiter"),
        }
        {
            let map = pool.lock();
            assert_eq!(map.get(&"k").unwrap().refcount, 2);
        }
        let value = publish_success(&pool, &"k", 42u32);
        assert_eq!(*value, 42);
        match try_attach_with(&pool, &"k", alive) {
            AttachOutcome::Ready(v) => assert_eq!(*v, 42),
            _ => panic!("attach after publish_success should be Ready"),
        }
    }

    #[tokio::test]
    async fn waiter_receives_establisher_result() {
        let pool: PoolMap<&'static str, u32> = new_pool_map();
        try_attach_with(&pool, &"k", alive);
        let rx = match try_attach_with(&pool, &"k", alive) {
            AttachOutcome::Waiter(rx) => rx,
            _ => panic!("expected Waiter"),
        };
        publish_success(&pool, &"k", 7u32);
        let value = wait_for_establish(rx).await.expect("waiter should see success");
        assert_eq!(*value, 7);
    }

    #[tokio::test]
    async fn waiter_receives_establisher_failure_and_entry_is_tombstoned() {
        let pool: PoolMap<&'static str, u32> = new_pool_map();
        try_attach_with(&pool, &"k", alive);
        let rx = match try_attach_with(&pool, &"k", alive) {
            AttachOutcome::Waiter(rx) => rx,
            _ => panic!("expected Waiter"),
        };
        publish_failure(&pool, &"k", "boom".to_string());
        let err = wait_for_establish(rx).await.expect_err("waiter should see failure");
        assert_eq!(err, "boom");
        let map = pool.lock();
        let entry = map.get(&"k").expect("failed entry should remain as tombstone");
        assert!(matches!(entry.state, EntryState::Dead));
        assert_eq!(entry.refcount, 2);
    }

    #[tokio::test]
    async fn three_way_concurrent_waiters_all_observe_the_same_establisher_result() {
        let pool: PoolMap<&'static str, u32> = new_pool_map();
        try_attach_with(&pool, &"k", alive); // establisher
        let rx1 = match try_attach_with(&pool, &"k", alive) {
            AttachOutcome::Waiter(rx) => rx,
            _ => panic!("expected Waiter"),
        };
        let rx2 = match try_attach_with(&pool, &"k", alive) {
            AttachOutcome::Waiter(rx) => rx,
            _ => panic!("expected Waiter"),
        };
        assert_eq!(pool.lock().get(&"k").unwrap().refcount, 3, "establisher + 2 waiters");

        publish_success(&pool, &"k", 99u32);

        let v1 = wait_for_establish(rx1).await.expect("waiter 1 should see success");
        let v2 = wait_for_establish(rx2).await.expect("waiter 2 should see success");
        assert_eq!(*v1, 99);
        assert_eq!(*v2, 99);
        assert!(Arc::ptr_eq(&v1, &v2), "all waiters should share the exact same Arc instance");
    }

    #[tokio::test]
    async fn multiple_waiters_all_observe_the_same_establisher_failure() {
        let pool: PoolMap<&'static str, u32> = new_pool_map();
        try_attach_with(&pool, &"k", alive); // establisher
        let rx1 = match try_attach_with(&pool, &"k", alive) {
            AttachOutcome::Waiter(rx) => rx,
            _ => panic!("expected Waiter"),
        };
        let rx2 = match try_attach_with(&pool, &"k", alive) {
            AttachOutcome::Waiter(rx) => rx,
            _ => panic!("expected Waiter"),
        };

        publish_failure(&pool, &"k", "dial failed".to_string());

        let e1 = wait_for_establish(rx1).await.expect_err("waiter 1 should see failure");
        let e2 = wait_for_establish(rx2).await.expect_err("waiter 2 should see failure");
        assert_eq!(e1, "dial failed");
        assert_eq!(e2, "dial failed");
    }

    #[tokio::test]
    async fn try_attach_with_dead_ready_value_becomes_establisher_without_returning_it() {
        let pool: PoolMap<&'static str, u32> = new_pool_map();
        try_attach_with(&pool, &"k", alive);
        publish_success(&pool, &"k", 1u32);

        match try_attach_with(&pool, &"k", |_| false) {
            AttachOutcome::Establisher => {}
            AttachOutcome::Ready(_) => panic!("dead Ready value must not be returned"),
            AttachOutcome::Waiter(_) => panic!("dead Ready value should make caller Establisher"),
        }

        let map = pool.lock();
        let entry = map.get(&"k").expect("entry should remain in-place");
        assert!(matches!(entry.state, EntryState::Connecting(_)));
        assert_eq!(entry.refcount, 2);
    }

    // ── release: アイドルタイマーのライフサイクル ────────────

    static RELEASE_TEST_POOL: LazyLock<PoolMap<&'static str, u32>> = LazyLock::new(new_pool_map);

    #[tokio::test]
    async fn release_to_zero_removes_entry_after_idle_grace_elapses() {
        try_attach_with(&RELEASE_TEST_POOL, &"release-removes-after-grace", alive);
        publish_success(&RELEASE_TEST_POOL, &"release-removes-after-grace", 1u32);

        release(&RELEASE_TEST_POOL, "release-removes-after-grace", Duration::from_millis(30));

        // 猶予中はまだ残っている。
        assert!(
            RELEASE_TEST_POOL.lock().contains_key(&"release-removes-after-grace"),
            "entry should still exist during the idle grace window"
        );

        // 猶予経過後は削除される(バックグラウンドタスクなので少し待ってポーリングする)。
        assert!(
            wait_until_removed("release-removes-after-grace").await,
            "entry should be removed once the idle grace window elapses"
        );
    }

    #[tokio::test]
    async fn release_with_remaining_refcount_does_not_start_a_removal_timer() {
        try_attach_with(&RELEASE_TEST_POOL, &"release-keeps-while-refcount-positive", alive);
        try_attach_with(&RELEASE_TEST_POOL, &"release-keeps-while-refcount-positive", alive); // refcount = 2
        publish_success(&RELEASE_TEST_POOL, &"release-keeps-while-refcount-positive", 2u32);

        release(&RELEASE_TEST_POOL, "release-keeps-while-refcount-positive", Duration::from_millis(20));

        // refcountはまだ1残っているはずなので、猶予時間を過ぎても消えない。
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            RELEASE_TEST_POOL.lock().contains_key(&"release-keeps-while-refcount-positive"),
            "entry must survive while at least one tab still holds it"
        );
    }

    #[tokio::test]
    async fn reattaching_during_idle_grace_cancels_the_pending_removal() {
        try_attach_with(&RELEASE_TEST_POOL, &"release-reattach-cancels-timer", alive);
        publish_success(&RELEASE_TEST_POOL, &"release-reattach-cancels-timer", 3u32);

        release(&RELEASE_TEST_POOL, "release-reattach-cancels-timer", Duration::from_millis(30));
        // タイマー発火前に新規タブがアタッチ(=世代が進む)。
        tokio::time::sleep(Duration::from_millis(5)).await;
        match try_attach_with(&RELEASE_TEST_POOL, &"release-reattach-cancels-timer", alive) {
            AttachOutcome::Ready(_) => {}
            _ => panic!("reattach before removal should observe Ready"),
        }

        // 元のタイマーが発火するはずだった時刻を過ぎても、世代が進んでいるので削除されない。
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            RELEASE_TEST_POOL.lock().contains_key(&"release-reattach-cancels-timer"),
            "a reattach before the grace window elapses must cancel the stale removal timer"
        );
    }

    #[tokio::test]
    async fn publish_failure_tombstone_is_removed_after_final_release() {
        let key = "publish-failure-tombstone-removed";
        try_attach_with(&RELEASE_TEST_POOL, &key, alive);
        let rx = match try_attach_with(&RELEASE_TEST_POOL, &key, alive) {
            AttachOutcome::Waiter(rx) => rx,
            _ => panic!("expected Waiter"),
        };

        publish_failure(&RELEASE_TEST_POOL, &key, "boom".to_string());
        assert_eq!(wait_for_establish(rx).await.expect_err("waiter should see failure"), "boom");
        release(&RELEASE_TEST_POOL, key, Duration::from_millis(30));
        release(&RELEASE_TEST_POOL, key, Duration::from_millis(30));

        assert!(
            wait_until_removed(key).await,
            "publish_failure tombstone should be removed by the normal idle timer"
        );
    }

    #[tokio::test]
    async fn replacing_dead_ready_preserves_refcount_for_late_release() {
        let key = "dead-ready-replace-preserves-refcount";
        try_attach_with(&RELEASE_TEST_POOL, &key, alive);
        try_attach_with(&RELEASE_TEST_POOL, &key, alive);
        publish_success(&RELEASE_TEST_POOL, &key, 1u32);

        release(&RELEASE_TEST_POOL, key, Duration::from_millis(30));
        match try_attach_with(&RELEASE_TEST_POOL, &key, |_| false) {
            AttachOutcome::Establisher => {}
            _ => panic!("dead Ready should be replaced in-place by a new establisher"),
        }
        publish_success(&RELEASE_TEST_POOL, &key, 2u32);
        release(&RELEASE_TEST_POOL, key, Duration::from_millis(30));

        tokio::time::sleep(Duration::from_millis(100)).await;
        let map = RELEASE_TEST_POOL.lock();
        let entry = map.get(&key).expect("new Ready entry must not be removed by late release");
        assert_eq!(entry.refcount, 1);
        match &entry.state {
            EntryState::Ready(v) => assert_eq!(**v, 2),
            _ => panic!("replacement entry should be Ready"),
        }
        drop(map);
        release(&RELEASE_TEST_POOL, key, Duration::from_millis(30));
        assert!(
            wait_until_removed(key).await,
            "replacement entry should be removable after the final holder releases it"
        );
    }

    #[tokio::test]
    async fn out_of_band_tombstone_is_removed_after_final_release() {
        let key = "out-of-band-tombstone-removed";
        try_attach_with(&RELEASE_TEST_POOL, &key, alive);
        let value = publish_success(&RELEASE_TEST_POOL, &key, 1u32);
        mark_dead_if_same(&RELEASE_TEST_POOL, &key, &value);

        {
            let map = RELEASE_TEST_POOL.lock();
            let entry = map.get(&key).expect("entry should remain as tombstone");
            assert!(matches!(entry.state, EntryState::Dead));
            assert_eq!(entry.refcount, 1);
        }

        release(&RELEASE_TEST_POOL, key, Duration::from_millis(30));
        assert!(
            wait_until_removed(key).await,
            "Dead entry should be removed by the normal idle timer"
        );
    }

    #[tokio::test]
    async fn publish_failure_refcount_can_reach_zero() {
        let key = "publish-failure-refcount-zero";
        try_attach_with(&RELEASE_TEST_POOL, &key, alive);
        try_attach_with(&RELEASE_TEST_POOL, &key, alive);
        publish_failure(&RELEASE_TEST_POOL, &key, "boom".to_string());

        release(&RELEASE_TEST_POOL, key, Duration::from_millis(30));
        release(&RELEASE_TEST_POOL, key, Duration::from_millis(30));

        {
            let map = RELEASE_TEST_POOL.lock();
            let entry = map.get(&key).expect("entry should remain until idle grace elapses");
            assert!(matches!(entry.state, EntryState::Dead));
            assert_eq!(entry.refcount, 0);
        }

        assert!(
            wait_until_removed(key).await,
            "zero-refcount publish_failure tombstone should be removed"
        );
    }

    // ── ランダム操作列によるモデルベーステスト ─────────────
    //
    // 個別のテストは「決めた順序の操作」しか見ない。ここでは`try_attach_with`/`publish_success`/
    // `publish_failure`/`mark_dead_if_same`/`release`の**任意の操作列**を、単純な参照モデル
    // (期待する状態・refcountを別に持つ)と突き合わせる。refcountのリークや、tombstone・
    // 差し替え(Ready→Connecting)を跨いだ状態の食い違い(#120と同種の「エラー後に状態が
    // 残る」バグ)を、操作の組み合わせから探す。
    //
    // 削除タイマーの満了は対象外: `release`のgraceを1時間にして発火させない(タイマー満了の
    // 検証は上のgrace系テストが担当)。これによりモデルが決定論的になり、実時間に依存しない。
    mod model_based {
        use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
        use std::sync::{Arc, LazyLock};
        use std::time::Duration;

        use proptest::prelude::*;

        use crate::pool::{
            mark_dead_if_same, new_pool_map, publish_failure, publish_success, release,
            try_attach_with, AttachOutcome, EntryState, PoolMap,
        };

        struct ModelVal {
            alive: AtomicBool,
        }

        impl ModelVal {
            fn is_alive(&self) -> bool {
                self.alive.load(Ordering::SeqCst)
            }
        }

        static MODEL_POOL: LazyLock<PoolMap<u32, ModelVal>> = LazyLock::new(new_pool_map);
        /// ケースごとに別のキーを使い、共有staticのプールでケース間が干渉しないようにする。
        static NEXT_KEY: AtomicU32 = AtomicU32::new(1_000_000);
        const NEVER_EXPIRES: Duration = Duration::from_secs(3600);

        #[derive(Debug, Clone, Copy)]
        enum Op {
            Attach,
            PublishOk,
            PublishFail,
            /// 現在のReadyの値を「死んだ」ことにする(`is_alive`がfalseを返す)。
            Kill,
            /// 現在のReadyの値に対して`mark_dead_if_same`を呼ぶ。
            MarkDeadCurrent,
            /// 既に置き換わった古い値に対して`mark_dead_if_same`を呼ぶ(何も起きてはならない)。
            MarkDeadStale,
            Release,
        }

        enum ModelState {
            Absent,
            Connecting,
            Ready(Arc<ModelVal>),
            Dead,
        }

        fn op_strategy() -> impl Strategy<Value = Op> {
            prop_oneof![
                4 => Just(Op::Attach),
                3 => Just(Op::Release),
                2 => Just(Op::PublishOk),
                1 => Just(Op::PublishFail),
                1 => Just(Op::Kill),
                1 => Just(Op::MarkDeadCurrent),
                1 => Just(Op::MarkDeadStale),
            ]
        }

        fn outcome_label(o: &AttachOutcome<ModelVal>) -> &'static str {
            match o {
                AttachOutcome::Ready(_) => "Ready",
                AttachOutcome::Waiter(_) => "Waiter",
                AttachOutcome::Establisher => "Establisher",
            }
        }

        fn check_against_model(key: u32, state: &ModelState, holders: u32, ctx: &str) {
            let map = MODEL_POOL.lock();
            match map.get(&key) {
                None => assert!(
                    matches!(state, ModelState::Absent) && holders == 0,
                    "{ctx}: entry is absent but the model expects one (holders={holders})"
                ),
                Some(entry) => {
                    assert_eq!(entry.refcount, holders, "{ctx}: refcount diverged from the model");
                    match (&entry.state, state) {
                        (EntryState::Connecting(_), ModelState::Connecting) => {}
                        (EntryState::Dead, ModelState::Dead) => {}
                        (EntryState::Ready(actual), ModelState::Ready(expected)) => {
                            assert!(Arc::ptr_eq(actual, expected), "{ctx}: a different Ready value than the model expects")
                        }
                        _ => panic!("{ctx}: entry state diverged from the model"),
                    }
                }
            }
        }

        fn run_model(ops: &[Op]) {
            let pool: &'static PoolMap<u32, ModelVal> = &MODEL_POOL;
            let key = NEXT_KEY.fetch_add(1, Ordering::SeqCst);
            let mut state = ModelState::Absent;
            let mut holders: u32 = 0;
            let mut stale: Option<Arc<ModelVal>> = None;

            for (step, op) in ops.iter().enumerate() {
                let ctx = format!("step {step} ({op:?})");
                match op {
                    Op::Attach => {
                        let outcome = try_attach_with(pool, &key, ModelVal::is_alive);
                        holders += 1;
                        // 借用の都合で、状態の更新はmatchの外で行う。
                        let next_state = match (&state, outcome) {
                            (ModelState::Absent | ModelState::Dead, AttachOutcome::Establisher) => {
                                Some(ModelState::Connecting)
                            }
                            (ModelState::Connecting, AttachOutcome::Waiter(_)) => None,
                            (ModelState::Ready(v), AttachOutcome::Ready(got)) if v.is_alive() => {
                                assert!(Arc::ptr_eq(v, &got), "{ctx}: returned a different value than the pooled one");
                                None
                            }
                            (ModelState::Ready(v), AttachOutcome::Establisher) if !v.is_alive() => {
                                // 死んだReadyは返さず、同じスロットを新しい確立へ差し替える。
                                stale = Some(v.clone());
                                Some(ModelState::Connecting)
                            }
                            (_, got) => panic!(
                                "{ctx}: unexpected outcome {} for the model state (a dead value must never be returned as Ready)",
                                outcome_label(&got)
                            ),
                        };
                        if let Some(next) = next_state {
                            state = next;
                        }
                    }
                    Op::PublishOk => {
                        if matches!(state, ModelState::Connecting) {
                            let arc = publish_success(pool, &key, ModelVal { alive: AtomicBool::new(true) });
                            state = ModelState::Ready(arc);
                        }
                    }
                    Op::PublishFail => {
                        if matches!(state, ModelState::Connecting) {
                            publish_failure(pool, &key, "model failure".to_string());
                            state = ModelState::Dead;
                        }
                    }
                    Op::Kill => {
                        if let ModelState::Ready(v) = &state {
                            v.alive.store(false, Ordering::SeqCst);
                        }
                    }
                    Op::MarkDeadCurrent => {
                        let current = match &state {
                            ModelState::Ready(v) => Some(v.clone()),
                            _ => None,
                        };
                        if let Some(v) = current {
                            mark_dead_if_same(pool, &key, &v);
                            stale = Some(v);
                            state = ModelState::Dead;
                        }
                    }
                    Op::MarkDeadStale => {
                        if let Some(old) = &stale {
                            mark_dead_if_same(pool, &key, old);
                        }
                    }
                    Op::Release => {
                        if holders > 0 {
                            release(pool, key, NEVER_EXPIRES);
                            holders -= 1;
                        }
                    }
                }
                check_against_model(key, &state, holders, &ctx);
            }

            // 後始末: 確立担当が残っていれば失敗を告げ、全保持者がreleaseしたときrefcountが
            // 必ず0まで到達する(=どの経路でもattachとreleaseが1対1で対応する)ことを確認する。
            if matches!(state, ModelState::Connecting) {
                publish_failure(pool, &key, "model teardown".to_string());
                state = ModelState::Dead;
            }
            while holders > 0 {
                release(pool, key, NEVER_EXPIRES);
                holders -= 1;
            }
            check_against_model(key, &state, 0, "teardown");
        }

        proptest! {
            #[test]
            fn pool_matches_reference_model_under_random_operations(
                ops in proptest::collection::vec(op_strategy(), 1..60)
            ) {
                run_model(&ops);
            }
        }
    }
}
