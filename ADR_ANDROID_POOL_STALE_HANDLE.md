# ADR: SSH接続プール(`pool.rs`)のstale handle再利用により`TransportPreference::Auto`の自動再接続が構造的に失敗する

- **Status**: Draft(2026-09-16起草、2026-09-17 rev4。issue #120として先行報告済み、
  実機Spike 5の副産物として発見。opus-adversarial-consult round 1で根本原因の因果関係
  そのものが誤っていたことが判明し、rev2で全面的に書き直した——rev1は「90秒の猶予 >
  60秒の再接続タイムアウト」という枠組みだったが、実際は`try_attach`が毎回
  `idle_generation`を進めて削除タイマーを無効化するため、**猶予 > `retry_interval`(3秒)
  である限り、値に関わらず削除タイマーが永久に成熟しないライブロック**が真因だった。
  round 2レビューでrev2の§3.5「tombstone化」設計が前提としていた「アタッチとreleaseは
  必ず1対1で対応する」という不変条件が現状のコードでは4つの失敗分岐で崩れていることが
  判明し、rev3でI1〜I3の明示的な不変条件セットへ書き直した。round 3レビューは
  「I1〜I3・Dead entry回収の議論は現在のコードから起きうる全ケースについて正しく
  完全」と結論しつつ、(a)既存テスト`waiter_receives_establisher_failure_and_entry_is_removed`
  がI3導入で壊れる指摘漏れ、(b)`refcount==0`でtombstone化する将来機構を禁じない
  不変条件セットの穴(I4として追加)、(c)§3.2に残っていたI3前提条件化と矛盾する
  rev2時代の記述、の3点(+軽微な指摘4点)を残し、rev4でこれらを反映して収束
  ("convergence-level feedback"、追加の異論無しとの回答)した)
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
  の指摘を反映、rev3はround 2
  (`/tmp/claude-1001/-home-cuzic-isekai-terminal/2285f8d6-e7bb-4a20-83c5-de317b68d9c7/scratchpad/opus-review-pool-stale-handle-round2.md`)
  の指摘を反映、rev4はround 3
  (`/tmp/claude-1001/-home-cuzic-isekai-terminal/2285f8d6-e7bb-4a20-83c5-de317b68d9c7/scratchpad/opus-review-pool-stale-handle-round3.md`、
  "convergence-level feedback"との回答)の指摘を反映
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
たとえば`channel_open_session()`が失敗した場合(796-803行目)は
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
  これを防ぐ仕組みは何も無い。**本番の`pool::release`呼び出し箇所はこの
  2つ(`lib.rs:1706`と`isekai_pipe_quic_transport.rs:139,172`)を含めて
  全部で3箇所のみであることをコード全体のgrepで確認済み**(`run_ssh_channel_loop`
  自体の呼び出し元は他に4箇所あるが、`quic_transport.rs:288`・
  `isekai_stun_p2p_transport.rs:319`・`isekai_link_relay_transport.rs:234`・
  `multipath_transport.rs:1084`はいずれもプールを介さず毎回新規ハンドルを
  作る経路なので本バグの対象外)。
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
  「90秒の壁」という単純な説明ではない。**逆に言えば、90秒より短い間隔で
  「再接続」を繰り返しタップし続けるユーザーは、そのタップ自体が
  毎回削除タイマーを再無効化するため永久に回復できない**——
  `always-connects.md`が禁じる「ユーザーの手動操作でしか回復しない」を
  さらに悪化させ、「ユーザーが辛抱強く手動操作を繰り返すほど回復しなくなる」
  という最も鋭い形の違反になっている。

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
| A5 | 1066 | `TransportCommand::Disconnect`または`cmd_rx`クローズ(`None`) → `break` | **性質の異なる2つが1つの腕に混在**。`Disconnect`は正常系だが、`cmd_rx`が閉じる(=`SessionCore`が消えた)ケースも同じ腕に落ちており区別できない。加えて、もし§3.4項目3のように既存の`DisconnectKind::classify`(`orchestrator.rs:751-759`)を再利用して案Aを実装する場合、A5はどちらの場合も`Disconnected { reason: None }`を発生させ、`classify`の`_`腕で`TransportError`(再接続可能)に分類される——つまりユーザーが意図して切断した場合まで「失敗」としてevictされてしまう。実害は非対称性の議論(下記)により小さいが、「既存分類器の再利用はそのまま流用できる」という主張ほどクリーンではない点は記録しておく。 |

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
    失敗として扱いevictする)は、§3.4項目2として**項目1(本節の案B)の
    前提条件**に位置づけている——項目1単独では、この盲点のケースで
    `try_lock`フォールバックがハングした保持者に「生きている」と
    騙され続け、元の100%失敗がそのまま再現してしまうため(詳細は§3.4)。
  - 今回報告したバグそのものについては、セッションタスクは既に終了済み
    (`SendError`はそこから来る)なので`is_closed()`は確実に`true`を返す。
    上記の盲点は「別種の、より広いロバスト性」の話であり、今回のバグの
    修正としては案Bは完全。
  - 生存確認は`try_attach`の時点でしか行わない。その直後(チェックと
    実使用の間)にハンドルが死ぬ、あるいは`Establisher`が公開した直後の
    ハンドルが即座に死んで`Waiter`に渡ってしまう、というケースは
    残る——ただしこれは1回分の失敗した試行で済み、**次の再試行の
    `try_attach`がそのエントリをtombstone化する**(まさに本修正の
    設計そのもの)ため自己修復する。

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

### 3.4 推奨する対応順序(rev2で全面的に変更、rev3でround 2の指摘を反映)

opus-adversarial-consult round 1の指摘により、rev1の「案Aを優先、案Bは
追加防御線」という推奨は逆だったことが判明した。round 2の指摘により、
下記項目1と項目2は独立した優先順位ではなく**項目2が項目1の前提条件**
であることが判明した(項目2無しで項目1だけ実装すると、ハングした
保持者がロックを握り続けるケースで元の100%失敗が再現してしまう——
詳細は項目2の説明を参照)。以下の順序を推奨する:

1. **案B: `try_attach`に`is_closed()`ベースの生存確認を追加し、
   死んだエントリは削除ではなくその場でtombstone化する(採用、
   ただし項目2とセットで初めて安全)**:
   `try_attach`が`is_closed()==true`のキャッシュ済みハンドルを拒否し、
   そのエントリをその場で無効化(tombstone化、詳細は§3.5)してから
   新規`Establisher`にフォールバックする。無料・同期・分類ロジック不要。
   ただし変更箇所は「`pool.rs`1箇所」だけでは済まない——本番の
   アタッチ呼び出し箇所は`isekai_pipe_quic_transport.rs:722`
   (`acquire_pooled_handle`)と`lib.rs:1666`(`run_russh_transport`)の
   実質2箇所(§2.4のN1で数えた`release`呼び出し3箇所とは別に、
   アタッチ側は2関数)だが、§3.5のI1(アタッチとreleaseの1対1対応)を
   満たすには**さらに4つの失敗分岐**(`Waiter→Err`が2箇所、
   `Establisher→Err`が2箇所)に`release`呼び出しを追加する必要がある
   ——正確には「`pool.rs`1箇所 + 呼び出し側2関数 + I1のための失敗分岐4箇所」
   という規模になる。プレーンSSHプールを守るために「デフォルト付きの
   従来`try_attach`ラッパー」を`try_attach_with`と並存させてはならない
   ——次に新しいプールが追加されたときにデフォルトのまま同じバグを
   静かに引き継いでしまう。
2. **初回使用時タイムアウト(新規。項目1の前提条件であり、案Aより価値が高い)**:
   round 2レビューで判明した通り、`run_ssh_channel_loop`は
   `pooled.handle.lock().await.channel_open_session().await`
   (`ssh_handler.rs:796`、および同型のパターンが`:834`
   `streamlocal_forward`・`:1010` `tcpip_forward`・`:1079`
   `cancel_streamlocal_forward`にもある)という形で、**awaitの間
   tokio Mutexを握ったまま**になる。§3.2の`is_closed()`実装は
   ロック取得に失敗した場合「使用中=生きている」側へフォールバックする
   設計なので、まさに§3.2が盲点として挙げた「サイレントに死んで
   `channel_open_session()`がハングする」ケースでは、ハングした
   保持者自身がロックを握り続け、後続の全リトライの`try_lock`が
   失敗し続けて「生きている」と誤判定され、**元の100%決定的な失敗が
   そのまま再現する**。したがって初回使用時タイムアウトは項目1の
   「追加のロバスト性」ではなく、項目1の`try_lock`フォールバックを
   安全にするための前提条件として扱う。タイムアウトは**リトライ経路
   だけでなく`run_ssh_channel_loop`の`:796`自体**(=あらゆる保持者)に
   適用しないと、ロック競合の窓そのものが塞がらない点に注意。
   秒数値は新規に決めるまでもなく、`ssh_handler.rs:679-689`に既に
   `RUN_EXEC_TIMEOUT=10秒`として定義され、`:695-700`でまさに同じ
   `handle.lock().await.channel_open_session().await`を
   `tokio::time::timeout`で包む前例がある——これを再利用するか、
   隣に姉妹定数を立てるのが最も筋が良い(§4参照)。
3. **案A(任意、優先度最低)**: やるとしても新しい`enum ChannelOutcome`は
   作らず、既存の`DisconnectKind::classify`(`orchestrator.rs:751-759`)を
   再利用する。A3(`ExitStatus`)だけをgracefulとして明示的に扱い、それ以外は
   すべてfailed扱いにする(A5の再利用に伴う細かい非対称性は§3.1のA5行
   参照)。得られる利益は「3秒早く気付ける」程度に限られる。

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

**R3 — この2つを同時に回避する設計: "削除"ではなく"その場でtombstone化"**
(rev2で提案、round 2レビューで前提条件の欠落が判明し、rev3でI1〜I3の
明示的な不変条件セットとして書き直した)。

rev2は「refcountの加算/減算は同じスロット上で継続するため、R1は
`entry_id`のような仕組みを新設せずとも自然に消える」と書いたが、
これは**「あらゆる`AttachOutcome`が最終的に1回の`release`と対応する」**
という前提の上でのみ成り立つ。この前提は今日のコードでは以下の
4つの分岐で成り立っていない(本番のアタッチ経路2関数を全数監査した結果):

| 分岐 | 発生箇所 | refcount | 対応する`release`は? |
|---|---|---|---|
| `Waiter(rx)`→`Err(m)`→`OtherFailed` | `isekai_pipe_quic_transport.rs`の`acquire_pooled_handle`(:722-748) | +1 | ❌ 無い(`connect`/`connect_auto`の`OtherFailed`/`DialFailed`腕は`release`を呼ばない) |
| `Establisher`→`Err`→`publish_failure` | 同上 | +1 | ❌ 無い |
| `Waiter(rx)`→`Err`(`:1673-1678`) | `lib.rs`の`run_russh_transport`(:1666-1706) | +1 | ❌ 無い |
| `Establisher`→`Err`(`:1687-1694`) | 同上 | +1 | ❌ 無い |

今日これが害を及ぼさないのは、**`publish_failure`(`pool.rs:124-135`)が
`map.remove(key)`でエントリ全体を消し、上記の帳尻が合っていない`+1`ごと
道連れにして捨てているから**であり、「アカウンティングが正しいから」
ではなく「ぶっ壊して捨てているから」正しく見えているに過ぎない。

したがって、rev2が書いていた「`publish_failure`も削除ではなく
tombstone化(またはrefcountが実際に0の場合のみ削除)に変える」を
そのまま実装すると、この4つの`+1`が**恒久的に相殺されずに残る**:
`refcount`が二度と0に戻らなくなり、`release`が削除タイマーを一度も
armできなくなる。つまりR1(こっそりプーリングが壊れる)を、
「エントリが永久に不滅になる」というR1よりさらに気付きにくい退行に
置き換えてしまう——このADR自身が§3.1で挙げた「クラッシュより発見しにくい
最悪の種類」の失敗モードそのものである。

**正しい設計は次の3つの不変条件として書く:**

- **I1 — あらゆる`AttachOutcome`は例外なく1回の`release`と対応する。**
  上記4つの失敗分岐それぞれに`release`呼び出しを追加する(呼び出し元は
  そのアーム内でまだ`key`を保持しているため、`acquire_pooled_handle`
  自身の中、および`run_russh_transport`の2つの`return`の直前に足すのが
  最も安上がり。ただし`run_russh_transport`側は`match &pool_key`
  [`lib.rs:1652`]が参照を束縛するため、追加する2箇所の`release`は
  `key.clone()`が必要になる点に注意——借用チェッカー上の問題ではなく、
  単に失敗パスで`String`を1〜2個複製するだけの些細なコスト)。これは
  tombstone設計全体を支える前提でありながら、rev2には一言も
  書かれていなかった。あわせて`AttachOutcome`のdocコメント
  (`pool.rs:56-59`、確立担当者は`publish_success`/`publish_failure`を
  呼ぶことしか要求していない)と`publish_failure`のdocコメント
  (`pool.rs:121-123`、「この後`Disconnected`等の通常のエラー経路で
  処理を続ける」=releaseは不要であるかのように読める)の両方を、
  「release呼び出しも必須」と明記するよう更新しないと、次にこのコードを
  触る人がこのリークを再導入しうる。
- **I2 — tombstone化「そのもの」は`state`だけを書き換える。`refcount`にも
  `idle_generation`にも一切触れない。** ここで言う「tombstone化」とは
  「あるエントリを`Ready(dead)`から`Dead`または`Connecting`へ変える」
  という操作それ自体を指す。案B(§3.2)の主経路のように、tombstone化が
  `try_attach`内でアタッチと**同時に**起きる場合、そのアタッチ自身が
  `refcount += 1`・`idle_generation`のインクリメントを行うのは通常通り
  正しい(`pool.rs:82-83`)——これは「アタッチが触っている」のであって
  「tombstone化が触っている」のではない、という区別である。I2が
  禁じているのは、tombstone化という操作**単体**が`refcount`/
  `idle_generation`を書き換えること(§1.4以降で説明するアイドル削除
  タイマーの整合性を壊すため)であって、同じ操作の中で正当なアタッチが
  同時に起きることまで禁じるものではない。特に`idle_generation`に
  触れないことが重要で、理由はDead entryの回収を扱う下記で説明する。
- **I3 — `publish_failure`はエラーをブロードキャストしてtombstone化する
  だけにする。削除は例外なく通常の削除タイマーの仕事とする。** I1が
  成り立っていれば、これはもうrefcountの特別扱いを一切必要としない
  (rev2の「またはrefcountが実際に0の場合のみ削除」という代替案は、
  I1無しでは「実質削除しない」に退化し、I1が成り立てば単に不要になる
  ——通常の削除タイマーが既にその仕事をする)。
- **I4 — `try_attach`の外からtombstone化する経路は、その時点で自分が
  アタッチトークンを保持している(`refcount ≥ 1`)ことを保証するか、
  さもなくば自分で削除タイマーをarmすること。** I1〜I3だけでは、
  `refcount == 0`のエントリをtombstone化して立ち去るケースを禁じて
  いない——その場合`release`が二度と呼ばれずタイマーもarmされないため、
  I1が成り立っていても不滅の`Dead` entryができてしまう。現在の設計にある
  4つのtombstone経路(案B自身の`try_attach`内検知・I3の`publish_failure`・
  §3.4項目2の初回使用時タイムアウト・任意で実装する場合の案A)は
  いずれも自分がアタッチトークンを保持した状態(`refcount ≥ 1`)で
  tombstone化するため、この時点では問題にならない。ただし将来
  「プールを巡回して死んだアイドルエントリを掃除する」ような、
  §3.1が案Aの構造的な弱点として指摘した「アイドル中に死ぬケース」を
  能動的に処理する機構(バックグラウンドの健全性スイーパー等)を
  追加する場合は、この条件を明示的に満たす実装にする必要がある。

I1〜I4が揃えば、R1・R2はどちらも構造的に発生しえなくなり、`entry_id`の
ような新しい識別子を導入する必要も無い——これはR3が本来目指していた
性質そのものである。

**「tombstone化されたDead entryはいつ回収されるのか」への明示的な答え**
(この問いはR3の欠落と表裏一体だった): I1・I2が成り立つ前提で、
Dead entryは**新しい仕組み無しに、通常の削除タイマーがそのまま回収する**。
tombstone化は`idle_generation`を進めない(I2)ため、既にarmされていた
タイマーはそのまま生き続け、発火時に`refcount == 0 &&
idle_generation == my_generation`が成立して`map.remove`が実行される
(`pool.rs:60-64`)。tombstone化の時点で`refcount > 0`だった場合
(`publish_failure`のケースや、項目2の初回使用時タイムアウトでタブが
まだトークンを保持しているケース)は、その最後の保持者の`release`が
その時点でタイマーをarmし、そこから`idle_grace`後に削除される。
`refcount`が0に至る経路は必ず`release`を通り、`release`は必ず
タイマーをarmする(`pool.rs:154-165`)ので、**I1が成り立つ限り
「refcount=0なのにタイマーが立っていないDead entry」という状態は
存在しない**(I1が無ければ、それがまさに直前で示した不滅化の再来になる)。
逆にtombstone化が`idle_generation`まで進めてしまっていたら、armされて
いたタイマーを無効化し、誰かが偶然再アタッチするまでDead entryが
宙に浮いたまま残ってしまう——I2はこの意味で「実装の細部」ではなく
「回収の仕組みそのもの」である。`EntryState::Dead`にはペイロードを
一切持たせないこと(`Arc<PooledSshHandle>`とその下のソケット/russh
タスクハンドルをtombstone化の瞬間に確実にdropするため)。最悪ケースで
残るのは`HashMap`のエントリ(ホスト×ユーザー×鍵×ポートの組み合わせ数で
上限がある、`SshPoolKey`を保持するだけ)であり、生きた接続ではない
——この規模感は書き留めておく価値がある。

**副次的に得られる性質**: 同じ死んだエントリに対する複数タブの並行
リトライは、追加の仕組み無しに自然に直列化される——3つのタブが同時に
`try_attach`しても、`parking_lot`のマップロックの下で最初の1つだけが
`Ready(dead) → Connecting → Establisher`を得て、残り2つは
`Connecting → Waiter`を見る。したがって再確立は1回しか起きない。
これは「削除してから再挿入する」という定式化にはない利点で、
tombstone-in-placeを選ぶ理由の1つとして記録しておく価値がある。

**`Dead`はI3(またはそれに類する経路)が無いと到達不能であることも
明記しておく**: 項目1(案B単体)だけを見ると、`try_attach`が
`is_closed()`を見て`Ready → Connecting`へ遷移させるのは全て同じ
クリティカルセクション内なので、`EntryState::Dead`という状態が
外から観測されることはない。`Dead`が実際に意味を持つのは、
`try_attach`の外からエントリを無効化する経路——`publish_failure`
(I3)や項目2の初回使用時タイムアウト、および(もし実装するなら)
案A——が存在する場合に限られる。この点をADRに明記しておかないと、
実装者が項目1の段階で使われない`Dead`変種を作ってしまうか、
逆に省略して項目2で詰まるかのどちらかになりうる。

I1〜I4を満たす実装は(4つの失敗分岐への`release`追加・2つのdocコメント
更新・新しい`EntryState`列挙子・既存テスト1本の書き換え+新規テスト
数本を要するため)もはや明らかに「最小の差分」とは言えないが、
並行性の議論が最も単純になる(`entry_id`のような新しい識別子を
一切必要としない)という点は変わらず成り立ち、実装時はこれを採用する。

### 3.6 既存テストとの整合性

`pool.rs:286-444`は`PoolMap<&'static str, u32>`という汎用型でプールの
基本プリミティブを検証しており、`RELEASE_TEST_POOL`はプロセス全体で
共有される`LazyLock`(複数の`#[tokio::test]`間で共有)。したがって:

- 生存確認は`T`へのtrait境界ではなく、呼び出し元が渡すクロージャ/
  パラメータにする必要がある(§3.2の`try_attach_with`シグネチャの理由)。
- `reattaching_during_idle_grace_cancels_the_pending_removal`
  (`pool.rs:426-444`)は、まさに今回のバグの原因である
  「世代インクリメントによる削除タイマー無効化」という挙動自体を
  固定するテストであり、修正後もこれは緑のまま維持する必要がある
  (生存確認述語に`|_| true`を渡せばそのまま緑を維持できる——この
  挙動自体、再アタッチが保留中の削除を無効化することは変更しない、
  変えるのは「死んだ値を返さない」という`try_attach`側の判断だけ)。
- **既存テスト`waiter_receives_establisher_failure_and_entry_is_removed`
  (`pool.rs:322-334`)は書き換えが必要**(round 3レビューで発見、
  当初§3.6には書かれていなかった)。この既存テストの最後のアサーション
  `assert!(pool.lock().get(&"k").is_none(), "failed entry should be
  removed")`は、まさにI3が置き換える「失敗したら即座に削除する」という
  挙動そのものを固定しており、I3実装後はこのアサーションのタイミングで
  エントリは存在するがtombstone化(`Dead`)されている状態になるため、
  そのままでは失敗する。修正はこのテストの見た目上の「退行」ではなく
  意図した挙動変化なので注意——さらにこのテストが使っている
  `PoolMap<&'static str, u32>`は**ローカル変数**であり(`RELEASE_TEST_POOL`
  のような`'static`ではない)、`release`が`pool: &'static PoolMap<..>`を
  要求する(`pool.rs:143-147`)ため、この局所プールのままでは「猶予後に
  実際に削除される」ところまでは検証できない。書き換えの際は
  (a)「tombstone化されるがまだ削除されない・エラーはブロードキャスト
  される」ことをローカルプールで検証するテストと、(b)`RELEASE_TEST_POOL`
  のような`'static`プールで「最後の`release`+猶予後に実際に削除される」
  ことを検証するテストの2本に分けるのが自然。テスト名自体も
  (`..._and_entry_is_removed`のままだと新しい契約と矛盾するので)
  合わせて変更する。
- その上で、新しい回帰テストを追加する。「失敗する再アタッチは削除を
  無期限に先送りしない」は**修正前**の不変条件の記述であり、案B適用後は
  失敗した再アタッチは削除を先送りするのではなく即座にエントリを
  置き換えるため、この文言のテストは修正後の挙動とは異なるものを
  検証してしまう。`PoolMap<&'static str, u32>`上で述語を渡すだけで
  実機なしに書ける、より直接的なテストは以下の4つ:
  - `try_attach_with`は`is_alive`が偽のとき`Ready`ではなく
    `Establisher`を返し、死んだ値を決して渡さないこと。
  - 別の保持者がまだ存在する状態で`Ready → Connecting → Ready`の
    差し替えが起きても、`refcount`が正しく保たれ、遅れて`release`した
    側が**新しい**方の値の削除を誤ってスケジュールしないこと(これが
    R1そのもの、静かに腐るタイプの回帰)。
  - out-of-band(`try_attach`の外)からのtombstone化の後、最後の
    `release`が実際にエントリを猶予後に消し去ること(§3.5の
    「Dead entryは不滅にならない」性質、I1〜I4の検証)。
  - `publish_failure`後も`refcount`が0まで到達可能であること。これは
    **現状のコードに対する「赤いテスト」ではない**(現状は
    `publish_failure`がエントリごと削除するため、観測すべき`refcount`
    自体が存在せず「テストが失敗する」のではなく「テストが書けない」)
    ——I1+I3を実装して初めて意味を持つ、新しい挙動を固定するための
    テストとして追加する。

## 4. 未決事項(rev1からの更新、rev3・rev4でさらに更新)

- ~~`PooledSshHandle`の内部構造がrussh `client::Handle`の生存確認に
  使える既存APIを持つか~~ → **解決済み**: `Handle::is_closed()`が
  存在し、同期・無料(§3.2)。
- ~~プレーンSSHプール側で同種のバグが実害を持つかどうか~~ →
  **解決済み**: コードから確認済み、同一の構造で100%再現する(§2.4)。
- 案A実装時にどの早期return分岐が「failed」でどれが「graceful」かの
  一覧化 → §3.1で完了(A1〜A5)。ただし§3.4により案Aは優先度最低のため、
  この分類の精緻化自体が今すぐ必要というわけではない。
- ~~初回使用時タイムアウト(§3.4項目2)の具体的な秒数値・既存コードとの
  整合性~~ → **解決済み**: `ssh_handler.rs:679-689`に既に
  `RUN_EXEC_TIMEOUT=10秒`が定義されており、`:695-700`で**全く同じ**
  `handle.lock().await.channel_open_session().await`パターンを
  `tokio::time::timeout`で包む既存の前例がある(QUICダイヤル/RESUME
  往復のタイムアウトである`isekai-transport`クレートの
  `TRANSPORT_STEP_TIMEOUT`は用途が異なる別クレートの値であり、
  こちらを参照先にするのは不適切——rev2はこれを誤って引用していた)。
  実装時は`RUN_EXEC_TIMEOUT`を再利用するか、その隣に姉妹定数を
  立てるのが最も筋が良い。
- ~~案A・案Bどちらの提案にも共通する未対処のレース~~ → **解決済み**:
  §3.5でI1〜I4の不変条件セットとして解決策を明記した(I4はround 3
  レビューで追加、`refcount==0`でのtombstone化を将来にわたって禁じる
  条件)。
- ~~既存テストへの影響~~ → **解決済み**: §3.6で
  `waiter_receives_establisher_failure_and_entry_is_removed`
  (`pool.rs:322-334`)の書き換えが必要であることを明記した(round 3で
  発見、当初漏れていた)。

## 5. 次のステップ

rev4はopus-adversarial-consult round 3から「収束レベル
(convergence-level feedback)、これ以上の異論無し」との回答を得た
(round 1〜3の全指摘を反映済み)。実装に着手してよい状態と判断する。
実装は§3.4の順序(案B+tombstone化とI1〜I4 → 初回使用時タイムアウト
[`RUN_EXEC_TIMEOUT`再利用、両者は不可分の1セットとして同時に実装する] →
案Aは任意)で進める。実装後は実機での再現テスト(issue #120のスクリプトを
再利用)で修正を検証する。
