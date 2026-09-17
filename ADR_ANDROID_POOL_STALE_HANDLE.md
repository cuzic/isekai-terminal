# ADR: SSH接続プール(`pool.rs`)のstale handle再利用により`TransportPreference::Auto`の自動再接続が構造的に失敗する

- **Status**: Draft(2026-09-16起草。issue #120として先行報告済み、実機Spike 5の
  副産物として発見。まだレビュー・実装着手前)
- **対象**: `rust-core/src/pool.rs`(`try_attach`・`release`・`EntryState`)、
  `rust-core/src/transport/ssh_handler.rs`(`run_ssh_channel_loop`)、
  `rust-core/src/isekai_pipe_quic_transport.rs`(`ISEKAI_PIPE_QUIC_IDLE_GRACE`・
  `release`呼び出し箇所)
- **入力**: [issue #120](https://github.com/cuzic/isekai-terminal/issues/120)
  (実機Sony XQ-DQ44、`feat/android-reconnect-spike-infra`のdebug APKでの
  100%決定的な再現ログを含む)。`ANDROID_RECONNECT_SPIKE_PLAN.md`スパイク5の
  実施結果として発見。
- **拘束される既存ルール**: `.claude/rules/always-connects.md`、`.claude/rules/rust-ssot.md`

---

## 1. 背景

`TransportPreference::Auto`(UI上「Smart Connect(推奨)」、既定の推奨設定)は
`ISEKAI_PIPE_QUIC_POOL`経由で、認証済みSSH `client::Handle`を複数タブ間で
共有する(`pool.rs`冒頭のドキュメンテーションコメント参照、実装は
`archive/ISEKAI_SSH_DESIGN.md`「2026-07-07: 上記オープンな課題の調査・設計確定」節)。
この設計自体は正しい——同一ホストへの複数タブが認証を毎回やり直すのを避けるための
最適化である。

問題は、この最適化とorchestratorの自動再接続ループが組み合わさったときに生じる。

## 2. 問題: 90秒の解放猶予 vs 60秒の再接続タイムアウト

### 2.1 プールの解放猶予(idle grace)

`pool::release`(`rust-core/src/pool.rs:143-166`)は、参照カウントが0になっても
即座にエントリを削除しない。`idle_grace`だけタイマーを立ててから
(その間に新規アタッチが無ければ)削除する:

```rust
pub(crate) fn release<K, T>(pool: &'static PoolMap<K, T>, key: K, idle_grace: Duration)
where ...
{
    let mut map = pool.lock();
    let Some(entry) = map.get_mut(&key) else { return };
    entry.refcount = entry.refcount.saturating_sub(1);
    if entry.refcount != 0 { return; }
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
```

`isekai_pipe_quic_transport.rs:137-139`・`167-172`が`run_ssh_channel_loop`
終了後(=タブのSSHチャネルが理由を問わず閉じた後)、無条件でこの`release`を呼ぶ:

```rust
run_ssh_channel_loop(&pooled, cols, rows, false, false, cmd_rx, event_tx, app_pane_id).await;
crate::pool::release(&ISEKAI_PIPE_QUIC_POOL, key, ISEKAI_PIPE_QUIC_IDLE_GRACE);
```

`ISEKAI_PIPE_QUIC_IDLE_GRACE = Duration::from_secs(90)`
(`isekai_pipe_quic_transport.rs:668`。プレーンSSHの`PLAIN_SSH_IDLE_GRACE`
(`pool.rs:230`、30秒)より長い理由は、isekai-pipe QUIC経路の再確立コストが
TCPよりも明らかに高いため、と同ファイル666行目のコメントに明記)。

### 2.2 プールの生存確認なし

`try_attach`(`pool.rs:65-88`)の`Ready`分岐は、キャッシュされた
`Arc<PooledSshHandle>`をそのまま返すだけで、**それが実際にまだ生きているか
一切確認しない**:

```rust
Some(entry) => {
    entry.refcount += 1;
    entry.idle_generation = entry.idle_generation.wrapping_add(1);
    match &entry.state {
        EntryState::Ready(v) => AttachOutcome::Ready(v.clone()),  // ← 生存確認なし
        EntryState::Connecting(tx) => AttachOutcome::Waiter(tx.subscribe()),
    }
}
```

### 2.3 `run_ssh_channel_loop`は成功/失敗を区別せず`()`を返す

`run_ssh_channel_loop`(`transport/ssh_handler.rs:786`)の戻り値は`()`。
たとえば`channel_open_session()`が失敗した場合(796-802行目)は
`TransportEvent::Disconnected`イベントを送って`return`するが、これは
「正常にチャネルを開いてI/Oループを終えた」場合と全く同じ戻り値になる。
つまり**呼び出し元(`isekai_pipe_quic_transport.rs`の137/167行目)には、
「セッションが失敗で死んだ」のか「タブが正常に閉じられた」のかを区別する
手段が無い**。両者に同じ`release(..., ISEKAI_PIPE_QUIC_IDLE_GRACE)`が
無条件で適用される。

### 2.4 orchestratorの再接続タイムアウトは既定60秒

`SessionOrchestrator`の`ReconnectPolicy::default()`(`orchestrator.rs`)は
既定タイムアウト60秒。これは`ADR_ANDROID_RECONNECT_TIMEOUT.md`が扱う既存の
設計判断で、本ADRとは独立に決まった値。

### 2.5 組み合わせた結果: 90秒 > 60秒 なので自動再接続は原理的に不可能

セッションがエラーで死ぬと:

1. `run_ssh_channel_loop`が`()`を返す(失敗由来かどうかは呼び出し元には不明)。
2. `release(ISEKAI_PIPE_QUIC_IDLE_GRACE=90s)`が呼ばれ、死んだハンドルが
   プールに**90秒間**残り続ける。
3. orchestratorの自動再接続ループが即座に(reattach層の失敗を受けて)始まる。
   再試行のたびに`try_attach`が呼ばれるが、90秒間は`EntryState::Ready`
   (死んだArc)がヒットし続けるため、`AttachOutcome::Ready(dead_arc)`が
   返る(2.2)。
4. 死んだ`Arc<PooledSshHandle>`で`channel_open_session()`を呼ぶと、
   russh内部で送信先タスクが既に終了しているため
   `Error::SendError`("Channel send error"、
   `russh-0.48.2/src/lib.rs:268`)が即座に返る。
5. orchestratorの再接続タイムアウトは60秒。**60 < 90**なので、
   プールエントリが解放されるより先に必ずタイムアウトする。
6. `TransportPreference::Auto`/`IsekaiPipeQuic`は、ミッドセッション切断から
   **100%決定的に**自動復旧できない(`.claude/rules/always-connects.md`違反)。

90秒経過後の手動再接続(タブを閉じて開き直す、または明示的な再接続操作)は、
`try_attach`が空のプールに新規`Establisher`として当たるため即座に成功する
——これが実機ログで観測された「手動再接続は成功した」の説明。

## 3. 対応方針の検討

### 3.1 案A: `run_ssh_channel_loop`に成功/失敗の区別を持たせ、失敗時は即時evictする

`run_ssh_channel_loop`の戻り値を`()`から「チャネルが失敗で終わったか」を
示す型(例: `enum ChannelOutcome { GracefulClose, Failed }`)に変える。
呼び出し元(`isekai_pipe_quic_transport.rs:137-139`・`167-172`)は`Failed`
の場合、`release(..., idle_grace)`ではなく新しい`pool::evict_immediately`
(即座に`EntryState`を`map.remove`する、猶予無し)を呼ぶ。

- **長所**: 根本原因(失敗と正常終了の混同)に直接対処する。プレーンSSH側
  (`ssh_handler.rs:1698,1727,1830`)にも同じ問題が理論上あるはず(未検証、
  `PLAIN_SSH_IDLE_GRACE`は30秒とタイムアウトより短いため実害が出ていない
  だけの可能性がある)なので、同じ修正で両方守れる。
- **短所**: `run_ssh_channel_loop`内の全ての早期return箇所
  (`channel_open_session`失敗・PTY/shell要求失敗・I/Oループ中の切断等、
  複数箇所ある)を洗い出して分類する必要があり、実装コストが最も高い。
  分類を誤ると(本来failedな経路をgracefulと誤判定)、この修正自体が
  新しい「evictされるべきなのにされない」経路を生みかねない。

### 3.2 案B: `try_attach`にプール側の生存確認を追加する

`EntryState::Ready(Arc<PooledSshHandle>)`を返す前に、何らかの軽量な
生存確認(例: `PooledSshHandle`が内部に持つrussh `client::Handle`に
`is_closed()`相当のチェックがあるか調査した上でそれを使う、または
無効なハンドルでの`channel_open_session()`失敗をここで先取りして
即座に`AttachOutcome::Establisher`にフォールバックする)を行う。

- **長所**: `run_ssh_channel_loop`側の分類ロジックに触れずに済む。
  「使う直前に確認する」ので、失敗理由の分類漏れによる新しいバグを
  生みにくい。
- **短所**: russh `client::Handle`に軽量な生存確認APIが無い場合、
  実質「試しに`channel_open_session()`を呼んでみる」しかなく、それ自体が
  非同期I/Oを伴う(プール取得のたびに追加レイテンシが乗る)。失敗した
  場合の分岐(Establisherへフォールバック→新規確立)の実装も、既存の
  `try_attach`が同期関数である前提を崩すため、呼び出し元の非同期化が
  連鎖する可能性がある(要調査)。

### 3.3 案C(却下): 数値だけ動かす

`ISEKAI_PIPE_QUIC_IDLE_GRACE`を60秒未満に縮める、または
`ReconnectPolicy::default().timeout`を90秒超に伸ばす、のどちらか片方だけの
変更。

- **却下理由**: 根本原因(生存確認が無い、失敗と正常終了を区別しない)を
  一切直さないまま、単に競争条件の「窓」を動かすだけ。将来どちらかの値が
  再度動かされた瞬間に同じバグが再発する。`ADR_ANDROID_RECONNECT_TIMEOUT.md`
  §3.4が導入するUniFFI-freeな「無制限」タイムアウト設定等、独立した理由で
  タイムアウト値が変わりうる設計が既に進行中であることを踏まえると、
  値同士の大小関係に依存する暗黙の前提を作ること自体がリスク。

### 3.4 現時点の推奨

案Aと案Bは排他ではなく、**案Aを優先実装し、案Bは将来の追加防御線として
検討する**のが良さそうに見える(未確定、実装着手前にレビューで検証する)。
理由: 案Bのみでは「生存確認のタイミングと実際に壊れるタイミングの間に
再度レースが生じる」余地が残るが、案Aは「壊れた原因(失敗)をその場で
検知して即evictする」ため構造的により確実。プレーンSSHプール
(`SSH_POOL`)側で同種の問題が実際に起きているかは、この根本原因分析
だけでは確認できていない(§3.1参照)ため、案A実装時に合わせて調査する。

## 4. 未決事項

- `PooledSshHandle`の内部構造(`transport`モジュール)がrussh
  `client::Handle`の生存確認に使える既存APIを持つか、実装着手前に
  必ず確認する。
- 案A実装時、`run_ssh_channel_loop`内のどの早期return分岐が
  「failed」でどれが「graceful」かの一覧化が必要(タブをユーザーが
  明示的に閉じた場合のシグナルが現在どう伝わっているか含めて要調査)。
- プレーンSSHプール(`SSH_POOL`、本番の解放呼び出しは`lib.rs:1706`の
  `pool::release(&pool::SSH_POOL, key, pool::PLAIN_SSH_IDLE_GRACE)`で
  `PLAIN_SSH_IDLE_GRACE=30秒`(`pool.rs:230`)。`ssh_handler.rs`内のテストコードが
  使う`Duration::from_millis(30)`等はテスト専用の短縮値で無関係)側で
  同種のバグが実害を持つかどうかの検証。

## 5. 次のステップ

本ADRのレビュー(opus-adversarial-consult、対象を絞るかは要相談)の後、
実装に着手する。実装後は実機での再現テスト(issue #120のスクリプトを
再利用)で修正を検証する。
