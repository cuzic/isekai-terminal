# ADR: SSH接続プール(`pool.rs`)のstale handle再利用により`TransportPreference::Auto`の自動再接続が構造的に失敗する

- **Status**: Draft(2026-09-16起草、2026-09-17 rev2。issue #120として先行報告済み、
  実機Spike 5の副産物として発見。opus-adversarial-consult round 1で根本原因の因果関係
  そのものが誤っていたことが判明し、rev2で全面的に書き直した——rev1は「90秒の猶予 >
  60秒の再接続タイムアウト」という枠組みだったが、実際は`try_attach`が毎回
  `idle_generation`を進めて削除タイマーを無効化するため、**猶予 > `retry_interval`(3秒)
  である限り、値に関わらず削除タイマーが永久に成熟しないライブロック**が真因だった)
- **対象**: `rust-core/src/pool.rs`(`try_attach`・`release`・`publish_failure`・
  `EntryState`)、`rust-core/src/transport/ssh_handler.rs`(`run_ssh_channel_loop`)、
  `rust-core/src/isekai_pipe_quic_transport.rs`(`ISEKAI_PIPE_QUIC_IDLE_GRACE`・
  `release`呼び出し箇所・`connect_auto`のフォールバック判定)、`rust-core/src/lib.rs`
  (プレーンSSHプールの本番`release`呼び出し箇所)
- **入力**: [issue #120](https://github.com/cuzic/isekai-terminal/issues/120)
  (実機Sony XQ-DQ44、`feat/android-reconnect-spike-infra`のdebug APKでの
  100%決定的な再現ログを含む)。`ANDROID_RECONNECT_SPIKE_PLAN.md`スパイク5の
  実施結果として発見。rev2はopus-adversarial-consult round 1
  (`/tmp/claude-1001/-home-cuzic-isekai-terminal/2285f8d6-e7bb-4a20-83c5-de317b68d9c7/scratchpad/opus-review-pool-stale-handle.md`)
  の指摘を反映
- **拘束される既存ルール**: `.claude/rules/always-connects.md`、`.claude/rules/rust-ssot.md`

---

## 1. 背景

`TransportPreference::Auto`(UI上「Smart Connect(推奨)」、既定の推奨設定)は
`ISEKAI_PIPE_QUIC_POOL`経由で、認証済みSSH `client::Handle`を複数タブ間で
共有する(`pool.rs`冒頭のドキュメンテーションコメント参照、実装は
`archive/ISEKAI_SSH_DESIGN.md`「2026-07-07: 上記オープンな課題の調査・設計確定」節)。
この設計自体は正しい——同一ホストへの複数タブが認証を毎回やり直すのを避けるための
最適化である。プレーンSSH(`SSH_POOL`)にも同じ仕組みが並行して存在する。

**スコープ**: この問題は公開鍵認証のセッションにのみ影響する。
`SshPoolKey::for_target`(`pool.rs:216`)・`IsekaiPipeQuicPoolKey::for_config`は
パスワード認証に対して`None`を返しプールキーが作られないため、パスワード認証の
セッションはそもそもプールされず本バグの影響を受けない。

## 2. 問題: `try_attach`のタイマー無効化ロジックが、失敗した再試行と組み合わさって恒久的なライブロックを作る

### 2.1 プールの解放猶予(idle grace)と、削除タイマーを無効化する仕組み

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

削除タイマーは`idle_generation`が発火時点の値と一致する場合のみ実際に削除する。
そして**`try_attach`(`pool.rs:65-88`)自身が、既存エントリへの新規アタッチのたびに
`idle_generation`を進める**(`pool.rs:81`)——これは「再アタッチが来たら保留中の
削除を無効化する」ための意図的な設計であり、それ自体は正しい(短い間隔でタブを
閉じて開き直すケースを想定したもの)。

```rust
Some(entry) => {
    entry.refcount += 1;
    entry.idle_generation = entry.idle_generation.wrapping_add(1);  // ← 保留中の削除タイマーを無効化
    match &entry.state {
        EntryState::Ready(v) => AttachOutcome::Ready(v.clone()),  // ← 生存確認なし
        EntryState::Connecting(tx) => AttachOutcome::Waiter(tx.subscribe()),
    }
}
```

### 2.2 プールの生存確認なし

上記の`Ready`分岐は、キャッシュされた`Arc<PooledSshHandle>`をそのまま返すだけで、
**それが実際にまだ生きているか一切確認しない**。

### 2.3 `run_ssh_channel_loop`は成功/失敗を区別せず`()`を返す

`run_ssh_channel_loop`(`transport/ssh_handler.rs:786`)の戻り値は`()`。
たとえば`channel_open_session()`が失敗した場合(796-802行目)は
`TransportEvent::Disconnected`イベントを送って`return`するが、これは
「正常にチャネルを開いてI/Oループを終えた」場合と全く同じ戻り値になる。
呼び出し元(`isekai_pipe_quic_transport.rs`の137/167行目)には、
「セッションが失敗で死んだ」のか「タブが正常に閉じられた」のかを区別する
手段が無い。両者に同じ`release(..., ISEKAI_PIPE_QUIC_IDLE_GRACE)`が
無条件で適用される。

### 2.4 真因: `idle_grace > retry_interval`である限り、削除タイマーは値に関わらず永久に成熟しない

`SessionOrchestrator`の自動再接続ループは`retry_interval = 3秒`
(`orchestrator.rs:285`)ごとに再試行する。公開鍵認証・`TransportPreference::Auto`・
単一タブでミッドセッション切断が起きた場合の実際の時系列:

| t(秒) | 出来事 | refcount | idle_generation | 削除タイマー |
|---|---|---|---|---|
| 0 | セッションが死に`run_ssh_channel_loop`が`return`(`channel.wait()==None`) | | | |
| 0 | `release`(`isekai_pipe_quic_transport.rs:172`) | 1→0 | G→G+1 | T₁をspawn(t=90発火予定、要件gen==G+1) |
| 3 | 再試行#1 → `try_attach`が`Ready`ヒット(`pool.rs:83`) | 0→1 | G+1→G+2 | T₁は既に無効(世代不一致) |
| 3 | `channel_open_session()` → `SendError` → `return` | | | |
| 3 | `release` | 1→0 | G+2→G+3 | T₂をspawn(t=93発火予定、要件gen==G+3) |
| 6 | 再試行#2 → `Ready`ヒット | 0→1 | →G+4 | T₂は既に無効 |
| … | 3秒ごとに同じことが繰り返される | | | 直前のタイマーが成熟する前に毎回無効化される |
| 60 | orchestratorの再接続ループがタイムアウトして諦める(`orchestrator.rs`) | 0 | | 最後のタイマーはt≈150で成熟予定 |

つまり**削除タイマーを無効化しているのは`release`の`idle_grace`の値そのものではなく、
`try_attach`の`idle_generation`インクリメントである**。失敗する再試行が
`retry_interval(3秒)`ごとに来続ける限り、`idle_grace`が90秒だろうと61秒だろうと
10秒だろうと——**`retry_interval(3秒)`より大きい限り**——エントリは一度も
削除されない、というライブロックになる。rev1が書いていた「90秒 > 60秒だから
必ずタイムアウトする」という説明は、この意味で不正確だった(60秒という
タイムアウト値は実は無関係で、削除が起こらないこと自体が原因)。

**このことが意味する帰結:**

- **プレーンSSHプール(`SSH_POOL`)も同一構造で100%壊れている、"未検証"ではなく
  "確認済み"**: 本番の解放呼び出しは`lib.rs:1706`の
  `pool::release(&pool::SSH_POOL, key, pool::PLAIN_SSH_IDLE_GRACE)`
  (`PLAIN_SSH_IDLE_GRACE=30秒`、`pool.rs:230`。`ssh_handler.rs`内の
  `Duration::from_millis(30)`等はテスト専用の短縮値で無関係、本番経路ではない)。
  `30秒 > retry_interval(3秒)`なので、QUIC側と全く同じ理屈でライブロックする。
  プレーンSSHが既定transportでないため今まで報告されていなかっただけで、
  これを防ぐ仕組みは何も無い。
- **`connect_auto`のプレーンSSHフォールバックはpool hit時にバイパスされる**:
  `connect_auto`(`isekai_pipe_quic_transport.rs:159-197`)がプレーンSSHへ
  フォールバックするのは`AcquireOutcome::DialFailed`のときだけで、これは
  `establish_fresh`(新規確立、プールmiss時のみ通る経路)からしか返らない。
  プールhit(`AttachOutcome::Ready`)経由の失敗は`run_ssh_channel_loop`の
  内部で起きるため`DialFailed`に分類されず、フォールバックが一切発動しない。
  これが「なぜこのバグが何にも隠蔽されず表面化するのか」の説明でもある。
- **手動再接続が成功する理由は「死んでから90秒」ではない**: `try_attach`は
  手動再接続を含むあらゆるアタッチのたびに`idle_generation`を進める
  (§2.1)。したがって削除は「死んでから90秒後」ではなく「**直近の
  アタッチから90秒後**」に起こる。自動再接続ループが60秒で諦めた後、
  ユーザーがそこからさらに90秒以上待ってから手動再接続すれば
  (=死んでから合計150秒以上)、その間にエントリが削除されており
  新規`Establisher`として成功する——実機で観測された「手動再接続は
  成功した」は、たまたま十分待ってから試したことの結果であり、
  「90秒の壁」という単純な説明ではない。

## 3. 対応方針の検討

### 3.1 案A: `run_ssh_channel_loop`に成功/失敗の区別を持たせ、失敗時は即時evictする

`run_ssh_channel_loop`の戻り値を`()`から「チャネルが失敗で終わったか」を
示す型に変え、呼び出し元が`Failed`の場合は猶予無しの即時eviction
(`pool::evict_immediately`のような新関数)を呼ぶ。

`run_ssh_channel_loop`(`ssh_handler.rs:786-1090`)の早期return/break箇所を
全て洗い出すと5箇所あり、分類の難易度は箇所ごとに大きく異なる:

| # | 行 | 経路 | 分類 |
|---|---|---|---|
| A1 | 801 | `channel_open_session()`失敗 | **失敗、曖昧さ無し**。今回報告したバグが通る経路。 |
| A2 | 819 | `request_pty`/`request_shell`失敗 | **曖昧**。死んだセッションの場合もあれば、サーバーが単にPTY要求を拒否した健全なセッションの場合もある。しかも1つの`if`が2種類のリクエストの失敗を`.is_err()`だけでまとめており、どちらが・なぜ失敗したかすら区別できない。 |
| A3 | 955 | `ChannelMsg::ExitStatus` → `break` | **正常終了、曖昧さ無し**(ユーザーが`exit`と入力した等)。 |
| A4 | 960 | `channel.wait() == None`("channel closed by peer") → `break` | **本質的に曖昧、かつ今回報告したバグが実際に通る経路**。russhはセッション全体が死んだ場合もこのチャネルだけがサーバー側で閉じられた場合も同じ`None`を返す。 |
| A5 | 1066 | `TransportCommand::Disconnect`または`cmd_rx`クローズ(`None`) → `break` | **性質の異なる2つが1つの腕に混在**。`Disconnect`は正常系だが、`cmd_rx`が閉じる(=`SessionCore`が消えた)ケースも同じ腕に落ちており区別できない。 |

**A4(今回のバグが実際に通る経路)が最も曖昧な分類になる**、というのが
このアプローチの核心的な難点であり、rev1の§3.1「短所」はこれを
過小評価していた。

誤分類のコストは非対称: 「正常終了→failed」と誤判定するコストは
1回分の不要な再確立(タブを閉じてすぐ開き直すケースで、プールが本来
防ぎたかった無駄なコスト)で済むが、「失敗→正常終了」と誤判定すると
それは今回のバグそのものになる。したがって「デフォルトはFailed扱い、
A3だけをgracefulとして明示的に許可リスト化する」という安全側の実装は
可能だが、それは「短い間隔でタブを閉じて開き直す」というプールの
設計目的(`pool.rs:227-229`)をほぼ無効化することを意味する。

さらに、`orchestrator.rs:736-760`には既に`DisconnectKind::{
GracefulRemoteExit, NetworkLost, TransportError}`という
正常/異常の分類ロジック(`DisconnectKind::classify`)が存在し、そのdoc
コメントは「判断ロジックをRust側に一元化する」(`rust-ssot.md`の原則)
ためにここに集約したと明記している。案Aは同じ種類の判定を transport
層にもう1つ作ることになり、`rust-ssot.md`が警告する「同種の判定ロジックの
重複」そのものであり、2つの分類器が将来ズレていくリスクがある。

- **長所**: 「壊れた原因(失敗)をその場で検知して即evictする」ため、
  タイミング的には最速(3秒早く気付ける)。
- **短所**: 上記の通り分類の難易度が高く、しかも最重要のバグ経路(A4)が
  最も曖昧。加えて`run_ssh_channel_loop`の戻り値型を変えると、実際には
  呼び出し箇所が2箇所ではなく**6箇所**ある
  (`lib.rs:1699`・`isekai_pipe_quic_transport.rs:137,167`・
  `quic_transport.rs:288`・`isekai_stun_p2p_transport.rs:319`・
  `isekai_link_relay_transport.rs:234`・`multipath_transport.rs:1084`。
  後者4つはプールを使わない経路なので戻り値を捨てるだけの純粋な変更ノイズ)。
- **決定的な弱点**: セッションが**アイドル中(refcount=0でプールに
  留まっている間)に死ぬケース**(例: 最後のタブを閉じた10秒後にピアが
  QUIC/TCP接続を切る)を案Aは原理的に検知できない——そのときは
  `run_ssh_channel_loop`自体が動いていないため、失敗を報告する主体が
  存在しない。

### 3.2 案B: `try_attach`にプール側の生存確認を追加する

`EntryState::Ready(Arc<PooledSshHandle>)`を返す前に生存確認を行う。
`russh::client::Handle`には同期・非ブロッキング・無料の`is_closed()`が
既にある:

```rust
// russh-0.48.2/src/client/mod.rs:236
impl<H: Handler> Handle<H> {
    pub fn is_closed(&self) -> bool { self.sender.is_closed() }
}
```

これは今回観測している`SendError`とちょうど同じ条件を見ている
(`client/mod.rs:473-477`が`self.sender.send(...).await.map_err(|_|
Error::SendError)`を行っているため、`is_closed() == true`⟺観測された
`SendError`)。rev1では「`channel_open_session()`を実際に呼んでみるしか
ない、非同期I/Oが伴う」と書いていたが、これは誤りだった——`is_closed()`は
同期・無I/Oで、`try_attach`が同期関数のままで使える。

```rust
// pool.rs: 汎用テスト(PoolMap<&str, u32>)がコンパイルし続けるよう、
// 生存確認は呼び出し元が渡すクロージャにする(PooledSshHandle固有の知識を
// pool.rsに持ち込まない)。
pub(crate) fn try_attach_with<K, T>(
    pool: &PoolMap<K, T>, key: &K, is_alive: impl Fn(&T) -> bool,
) -> AttachOutcome<T>
```

`PooledSshHandle`向けの実装は次のようになる想定
(`tokio::sync::Mutex::try_lock()`も同期なので`parking_lot`のプール
ロックを握ったまま呼べる。ロックが取れない=使用中と判断し「生きている」
側へ倒す):

```rust
|p| p.handle.try_lock().map(|h| !h.is_closed()).unwrap_or(true)
```

**このアプローチが「案Aより構造的に確実」である理由**: 死んだキャッシュ
済みハンドルが害を及ぼす経路は必ず「次の`try_attach`」を通る。したがって
`try_attach`側で検知すれば、run_ssh_channel_loopの分類がどうであれ、
そして**§3.1で案Aが原理的に検知できないと指摘した「アイドル中に死ぬ」
ケースを含め**、すべての経路を一律に捕捉できる。rev1の§3.4は「案Aの方が
構造的に確実」としていたが、これは逆であることが分かった。

- **長所**: 同期・無料・分類ロジック不要。既存の`run_ssh_channel_loop`に
  一切触れない。案Aが原理的に検知できないアイドル中死亡ケースも捕捉する。
- **限界(実装前に明記しておくべき事項)**:
  - `is_closed()`はrussh側のセッションタスクが実際に終了して初めて
    `true`になる。`keepalive_interval=60秒・keepalive_max=3`
    (`isekai_pipe_quic_transport.rs:691-694`・`lib.rs:1641-1645`)、かつ
    `inactivity_timeout: None`(russhの既定、`client/mod.rs:1509`)という
    設定では、**サイレントに死んだ経路が検出されるまで最大約4分かかりうる**。
    その間`is_closed()`は`false`のままなので、`channel_open_session()`が
    失敗ではなく**ハングする**——`retry_attempt_in_flight`
    (`orchestrator.rs:963-1000`)が並行試行をブロックするため、1回の
    ハングした試行が後続の全リトライを60秒タイムアウトまで塞ぐ。案Aも
    この盲点は解決しない(ループがまだ終了していないため)。この盲点への
    対処(最初の`channel_open_session()`にタイムアウトを設け、タイムアウトも
    失敗として扱いevictする)は、案A・案Bどちらとも独立した第3の改善として
    別途検討する価値がある(§3.4参照)。
  - 今回報告したバグそのものについては、セッションタスクは既に終了済み
    (`SendError`はそこから来る)なので`is_closed()`は確実に`true`を返す。
    上記の盲点は「別種の、より広いロバスト性」の話であり、今回のバグの
    修正としては案Bは完全。

### 3.3 案C(却下): 数値だけ動かす

`ISEKAI_PIPE_QUIC_IDLE_GRACE`を60秒未満に縮める、または
`ReconnectPolicy::default().timeout`を90秒超に伸ばす、のどちらか片方だけの
変更。

- **却下理由(§2.4により当初より強い理由になった)**: rev1時点では「暗黙の
  数値的前提(grace < timeout)を作ること自体がリスク」という理由で
  却下していたが、§2.4の分析により実際に壊れずに済む条件は
  **`grace < retry_interval(3秒)`** であることが分かった——これは
  「タイムアウトより短くする」よりもさらに驚くほど厳しく、かつ無関係な
  変更で簡単に破られる暗黙の前提になる。根本原因(生存確認が無い、
  失敗と正常終了を区別しない)を一切直さないまま単に競争条件の窓を
  動かすだけ、という却下理由はそのまま維持しつつ、その根拠は強化される。

### 3.4 推奨する対応順序(rev2で全面的に変更)

opus-adversarial-consult round 1の指摘により、rev1の「案Aを優先、案Bは
追加防御線」という推奨は逆だったことが判明した。以下の順序を推奨する:

1. **案B + タイムスタンプ削除ではなく「その場でtombstone化」(採用)**:
   `try_attach`が`is_closed()==true`のキャッシュ済みハンドルを拒否し、
   そのエントリをその場で無効化(tombstone化)してから新規`Establisher`に
   フォールバックする。無料・同期・分類リスク無し・両プール(QUIC/プレーンSSH)を
   1箇所の変更で同時に直す。ただし素朴に`map.remove`でevictすると
   下記§3.5のレース(R1・R2)を生むため、「削除」ではなく
   「`EntryState::Ready`を`EntryState::Dead`へ置き換え、`refcount`は
   触らない」という実装にする(詳細は§3.5)。
2. **初回使用時タイムアウト(新規、案Aより価値が高い)**: プールhit直後の
   最初の`channel_open_session()`にタイムアウトを設け、タイムアウトも
   失敗としてtombstone化する。これにより§3.2で述べた「keepaliveによる
   最大4分のサイレント死亡検出ラグ」の盲点を塞ぐ。案Aはこの盲点を
   解決しないため、これは案Aの代替ではなく別の懸念に対する改善。
3. **案A(任意、優先度最低)**: やるとしても新しい`enum ChannelOutcome`は
   作らず、既存の`DisconnectKind::classify`(`orchestrator.rs:751-759`)を
   再利用する。A3(`ExitStatus`)だけをgracefulとして明示的に扱い、それ以外は
   すべてfailed扱いにする。得られる利益は「3秒早く気付ける」程度に限られる。

### 3.5 案A・案Bどちらにも共通する未対処のレース(opus round 1で発見)

**R1 — evict後の`release`のABA問題**: 今日のコードでは`try_attach`と
`release`は必ず1対1で対応しており、refcountの整合性が保たれている。
`map.remove`によるevictionはこの対応関係を壊す:

1. タブA・Bがエントリ E を共有(refcount=2)。セッションが死ぬ。
2. Aのループが終了→`release`→refcount=1。Aが再試行し E をevict、
   `Establisher`として新規エントリ E′ を作り確立に成功、
   `publish_success`。refcount(E′)=1。
3. 少し遅れてBのループが終了→`release(key, …)`が**E′を減算**→
   refcount=0→生きている・使用中のエントリに削除タイマーが立つ。
4. 猶予後にE′がプールから消え、タブCは同一ホストへの3つ目のセッションを
   フルにブートストラップ+QUIC+認証してしまう。

`saturating_sub`のためパニックはしないが、この結果は「プーリングが
静かに壊れる恒久的な退行」として現れる——クラッシュより発見しにくい
最悪の種類。

**R2 — `Connecting`状態のエントリをevictすると無関係な待機者を巻き込む**:
evictionを無条件の`map.remove`として書くと、あるタブの失敗が届いた
瞬間に別のタブが確立中(`Connecting(tx)`)だった場合、エントリごと`tx`が
消え、待機中の全タブの`wait_for_establish`が
`Err("pool: establishing task ended without a result")`(`pool.rs:99`)を
返す。その後の`publish_success`は`map.get_mut(key)`が`None`
(`pool.rs:112`)のため静かに無視され、確立したはずのタブはプールされない
まま孤立し、他の待機者は不必要に失敗する。**evictionは`EntryState::Ready`
にのみ、できれば`Arc::ptr_eq`で対象の値そのものが死んでいる場合にのみ
適用すべき**——別の成功したセッションを巻き込んで消してはならない。

**R3 — この2つを同時に回避する設計: "削除"ではなく"その場でtombstone化"**:
`map.remove`する代わりに、`EntryState::Dead`という新しい状態を追加し
(または`Connecting`へスワップし直す)、**`refcount`には一切触れない**:

- refcountの加算/減算は同じスロット上で継続するため、R1は`entry_id`の
  ような仕組みを新設せずとも自然に消える。
- `try_attach`が`Dead`を見たら`refcount += 1; state = Connecting(tx);
  → Establisher`とする。
- R2は「`Ready`状態のみtombstone化対象」という構造そのものによって
  防がれる。
- 1点注意: `publish_failure`(`pool.rs:124-135`)は現在`map.remove(key)`を
  行っているため、tombstone化後の再確立フローに同じABA問題(R1)を
  再導入しうる。`publish_failure`も同様に「削除ではなくtombstone化
  (またはrefcountが実際に0の場合のみ削除)」に変更する必要がある。

この設計は案A・案Bどちらの実装よりも小さく、並行性の議論が最も
単純になるため、実装時はこれを採用する。

### 3.6 既存テストとの整合性

`pool.rs:286-444`は`PoolMap<&'static str, u32>`という汎用型でプールの
基本プリミティブを検証しており、`RELEASE_TEST_POOL`はプロセス全体で
共有される`LazyLock`(複数の`#[tokio::test]`間で共有)。したがって:

- 生存確認は`T`へのtrait境界ではなく、呼び出し元が渡すクロージャ/
  パラメータにする必要がある(§3.2の`try_attach_with`シグネチャの理由)。
- `reattaching_during_idle_grace_cancels_the_pending_removal`
  (`pool.rs:426-444`)は、まさに今回のバグの原因である
  「世代インクリメントによる削除タイマー無効化」という挙動自体を
  固定するテストであり、修正後もこれは緑のまま維持する必要がある。
- その上で、新しい回帰テストとして「**失敗する**再アタッチは削除を
  無期限に先送りしない」ことを検証するテストを追加する。これが
  今回のバグが破っている不変条件そのものであり、実機なしで検証できる。

## 4. 未決事項(rev1からの更新)

- ~~`PooledSshHandle`の内部構造がrussh `client::Handle`の生存確認に
  使える既存APIを持つか~~ → **解決済み**: `Handle::is_closed()`が
  存在し、同期・無料(§3.2)。
- ~~プレーンSSHプール側で同種のバグが実害を持つかどうか~~ →
  **解決済み**: コードから確認済み、同一の構造で100%再現する(§2.4)。
- 案A実装時にどの早期return分岐が「failed」でどれが「graceful」かの
  一覧化 → §3.1で完了(A1〜A5)。ただし§3.4により案Aは優先度最低のため、
  この分類の精緻化自体が今すぐ必要というわけではない。
- 新規: 初回使用時タイムアウト(§3.4項目2)の具体的な秒数値は未決定
  (`TRANSPORT_STEP_TIMEOUT=15秒`など既存の値との整合性を実装時に検討する)。

## 5. 次のステップ

rev2をopus-adversarial-consultの同じレビュアーへ再送し、収束を確認した
上で実装に着手する。実装は§3.4の順序(案B+tombstone化 →
初回使用時タイムアウト → 案Aは任意)で進める。実装後は実機での再現テスト
(issue #120のスクリプトを再利用)で修正を検証する。
