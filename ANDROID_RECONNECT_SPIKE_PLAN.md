# 実機スパイク検証計画: Android接続安定化(ローミング/長時間バックグラウンド耐性)

- **Status**: Draft(2026-09-16起草。`ADR_ANDROID_RECONNECT_TIMEOUT.md`・
  `ADR_ANDROID_MIDSESSION_RESUME_BUDGET.md`のopus-adversarial-consult
  round 1レビューで、机上の議論だけでは決められない前提(Doze下での
  ConnectivityManagerコールバック配送・WakeLockの実効性・FGSの実機生存期間等)が
  複数見つかったことを受け、**これ以上の設計判断は実機データを取ってから行う**
  という方針転換に基づく)
- **対象**: `rust-core/src/orchestrator.rs`(reconnect loop)・
  `rust-core/src/resume_client.rs`(reattach)の設計判断に必要な実機データの収集。
  本文書自体はコード変更を含まない——スパイク用の一時的な計測コード追加は
  別途、本計画の承認後に着手する
- **入力**: 2026-09-16のADR1/2レビューで判明した未検証の前提群。既存の
  Phase 7-5実機ローミングテスト資産(`rust-core/scripts/phase7-5-roaming-test.sh`、
  `android/src/debug/kotlin/.../FaultInjectionReceiver.kt`)を確認済み
- **拘束される既存ルール**: 「実際に動かして検証できることを重視する」
  (PLAN.md Phase 10のrelay版ローカルe2e検証で明言されている本プロジェクトの方針)。
  `.claude/rules/always-connects.md`

---

## 1. なぜ机上の議論で決められないか

ADR1/2のround 1レビューで、以下はいずれも**ドキュメント上は「そうなっているはず」
だが実機で確認されていない**前提だと判明した:

1. `ConnectivityManager`のNetworkCallbackがDoze中でも確実に配送されるか
   (Android公式ドキュメント上はそう説明されているが、OEM独自の電力管理
   ——本プロジェクトの実機はXperia XQ-DQ44のみで、他OEMでの挙動は未検証)。
2. `PARTIAL_WAKE_LOCK`を取得すれば、Doze中でも実際にQUICソケットの送受信が
   できるのか、それともDoze deep-idleのネットワーク制限はWakeLockの有無と
   無関係に効くのか。
3. `foregroundServiceType="specialUse"`がタイムアウト対象外という調査結果
   (`AndroidManifest.xml`のコメント)は、実際のプロセス生存期間として
   どこまで裏付けられているか(Web調査止まりで、この用途での長時間実機
   検証はまだ無い)。
4. `RebindManager`/multipath(Tailscale⇔direct)が実際にどれだけの頻度で
   ローミングをカバーし切れており、orchestratorの`Idle`再接続ループに
   落ちるのがどれだけ稀(または頻繁)なケースなのか——**この頻度次第で、
   ADR1/2への投資対効果そのものが変わる**。
5. QUIC経路・plain TCP SSH経路それぞれの1試行あたりの実際のin-flight時間
   (`TRANSPORT_STEP_TIMEOUT=15秒`が理論値通りに効くか、実セルラー網では
   もっと長い/短いか)。

これらは全て「実機の電力管理・実ネットワークの挙動」に依存し、コードを
読むだけでは決着しない。**新しい設計を書く前に、これらを計測するスパイクを
先に実行する。**

## 2. 候補設計(根本的に異なる案を含む)

決定を急がず、まず次の5つの根本的に異なる方向性を並べる。スパイク結果に
応じてどれか1つ、または複数の組み合わせを選ぶ。

### 案A: Network-callback主導の無期限リトライ(現ADR1のB案)
`reconnect_wake`(`ConnectivityManager`コールバック)を主たる起床源にし、
tickベースの定期リトライを保険として残す。WakeLock・AlarmManager不使用。
電池コストは理論上最小。**成立条件**: Doze中もコールバックが確実に届くこと
(§1.1)。

### 案B: AlarmManager.setExactAndAllowWhileIdle駆動
Doze中でも(Doze deep-idleのメンテナンスウィンドウ、または
`setExactAndAllowWhileIdle`の間引かれた間隔で)定期的にアラームを立てて
再接続を試みる。Android公式ドキュメントではDoze深度に応じて数分〜十数分
間隔に間引かれるとされる——**この間隔が今回の要件(数十秒〜数分の圏外)に
対して実用的な速さかどうかは疑わしく、実測で棄却される可能性が高い候補**。

### 案C: 有界`PARTIAL_WAKE_LOCK`
「再接続試行中」の間だけWakeLockを取得し、成功/失敗が確定次第即座に解放する
(常時保持しない)。バッテリー最適化除外の要求とは別の権限だが、Doze
deep-idle下でWakeLockがネットワークアクセス制限そのものを解除するわけではない
可能性がある(§1.2)。`background_reliability_policy.rs`の既存方針
(バッテリー最適化除外は要求しない)とは矛盾しないが、電池消費への説明責任は残る。

### 案D: 「戦わない」設計 — 生存を諦め、コールドスタート再アタッチに一本化
Doze/バックグラウンド制限と戦って接続を維持しようとすること自体をやめ、
バックグラウンド中のorchestrator再接続ループは現状(または短い)ままにし、
**次にアプリがフォアグラウンドに戻った時点の`reattach_persistence.rs`
(`AUTO_REATTACH_GRACE_SECS=30分`、既存実装)に接続復旧を一本化する**。
UXとしては「バックグラウンド中は繋がったままにこだわらない、その代わり
フォアグラウンド復帰は速くて確実」という割り切り。

必要なら`WorkManager`のperiodic/expedited jobで、Doze中もOSが許可する
メンテナンスウィンドウ内に「短時間だけ起きて1回だけ再接続を試す」バーストを
挟む案もあるが、**round 3レビューで指摘の通り、WorkManagerのperiodic jobは
最小周期15分、expedited jobはクォータ制であり、案B(`AlarmManager`)と
同じ理由で今回のターゲット(数十秒〜数分の圏外)には使えない公算が高い**。
このバースト部分については専用スパイクを設けず、**スパイク3(案Bの判定)を
そのまま継承する**——スパイク3で「分オーダーなら棄却」と判定されれば、
案Dは「バーストを挟まず、バックグラウンド中は素直に諦めてフォアグラウンド
復帰を速くする」部分だけを採用する。

### 案E: 多重化(warm-standby)をデフォルト化して「落ちる前に切り替える」
`multipath_transport.rs`(Tailscale⇔direct、Phase 9-2/9-3で実機検証済み)を
opt-inではなくデフォルト経路に格上げし、可能な限りセッション断そのものを
発生させない方向に倒す。ADR1/2が対象にしている「完全に切断してからの
再接続・reattach」は、そもそも**多重化でカバーしきれなかった残りのケース**
にのみ効けばよい、という優先順位の転換。§1.4の頻度計測(スパイク4)が
「実は大半のローミングは既にmultipathでカバーできている」と示せば、
案A〜Dへの投資は縮小してよいことになる。

**案Eの評価は他の案と独立ではなく前提を変える**——スパイク4を最初に
実行し、その結果を見てから案A〜Dのどれにどれだけ投資するかを決める。

### 案A〜Eとは独立に必要な前提修正(opus-adversarial-consult round 2で判明)

`ADR_ANDROID_RECONNECT_TIMEOUT.md`/`ADR_ANDROID_MIDSESSION_RESUME_BUDGET.md`の
round 2レビューで、**案A〜Eのどれを選んでも共通して必要になる、実機データを
待たずに設計として決めておくべき修正**が3つ見つかっている。スパイクの
結果解釈にも影響するため、対象コードを触る前提として明記する:

1. **起床の取りこぼし対策**(ADR1): エッジトリガのコールバック配送・
   `reconnect_wake`のpermit破棄・in-flight試行のキャンセル不能、の3点が
   合成すると、tickを保険に格下げした瞬間に「圏外から復帰したのに数分間
   気付かない」経路が生まれる。`pending_wake`状態の保持か、in-flight試行の
   abortのいずれかが必須。
2. **orchestrator層の失敗分類**(ADR1): 認証失敗・ホスト鍵不一致のような
   恒久的失敗を、無期限/長時間リトライが誤って延々と再試行し続けると、
   ユーザー自身が運用する自前サーバーのfail2ban等に自分の端末が焼かれる
   実害がある。
3. **reattachバックオフのnetwork復帰による早期打ち切り**(ADR2): 予算を
   伸ばす前に、この1点だけで「Wi-Fiに戻った瞬間に繋がる」体感が改善する
   可能性が高い(配線先は既存)。

これらはスパイク結果を待たずに実装できる(実機データが必要なのは
「締切の絶対値」や「起床源の選択」であって、これら3点の要否ではない)。
むしろ3が入っているかどうかでスパイク5(in-flight時間計測)の解釈が
変わりうるため、**スパイク5より前にADR2のこのタスクを先に実装しておくと
計測結果がクリーンになる**。

## 3. 実機スパイク計画

**前提**: 実機はXperia XQ-DQ44(1台のみ、CLAUDE.md/既存PLAN.mdの実機検証と
同一機)。他OEMでの検証は入手できる範囲でベストエフォート(無ければ
「単一OEMでの結果」であることを結論に明記する)。

**共通の計測基盤(スパイク着手前に用意する、既存資産の拡張)**:

- **Doze強制遷移の前提条件(round 3レビューで発見、最重要)**: `adb shell
  dumpsys deviceidle force-idle`は**充電中は拒否される**。adbをUSB接続した
  ままの状態(=充電状態)では端末はDozeに入らないため、このコマンド単体では
  Doze依存のスパイク(1・2・3・6・7)が全て「Dozeに入らないまま計測して
  正常に見える」という空振りになる。**必ず次の順序で行うこと**:
  ```
  adb shell dumpsys battery unplug   # 充電中と判定されるのを防ぐ
  adb shell dumpsys deviceidle force-idle
  # ここで計測
  adb shell dumpsys deviceidle unforce
  adb shell dumpsys battery reset    # 必ず元に戻す(戻し忘れ注意)
  ```
  `phase7-5-roaming-test.sh`にこの手順をヘルパー関数として組み込むこと。
- `FaultInjectionReceiver`パターンを踏襲し、debugビルド専用の新規broadcast
  受信口を追加: 上記Doze強制遷移の補助、バッテリー最適化状態の切替補助、
  **`ReconnectPolicy`(`tick`/`retry_interval`/`timeout`)を実行時に上書きする
  フック**(round 3レビューで発見: 現状`reconnect_policy`は
  `create_session_orchestrator`で`ReconnectPolicy::default()`が入るだけで、
  production/debugいずれにも上書き経路が無い。テストは`OrchestratorState`を
  直接構築しているだけなので、実機でtick/timeoutを変えて計測するには
  この経路の新設が必須)。
- `RemoteLogger`(既存、logcatのみ——ネットワーク経由ではない。round 2の
  レビューで一時「ネットワーク経由」と誤って記載されたが誤りだったので
  訂正)に以下のタイムスタンプ付きイベントを追加で流す(一時的な計測用、
  スパイク後に本実装が固まれば整理):
  - `spawn_reconnect_loop`の各tick発火
  - `retry_attempt_in_flight`のON/OFF(開始・終了・成否)
  - `reconnect_wake`(network path restored通知)の着弾
  - **`NetworkPathMonitor`内部の、集約前(per-transport)の
    `ConnectivityManager.NetworkCallback`の発火**(`PathId`ごと)。
    集約後の`onNetworkPathChanged`だけでは「Wi-Fiだけ落としてもセルラーが
    生きていれば`anyPathAvailable`が変化せず一度も鳴らない」という
    仕様通りの挙動と「Dozeがコールバックを本当に落とした」ケースを
    区別できない(round 3レビューで判明、スパイク1参照)。
  - `resume_client.rs`の`attempt_reattach`各試行の開始・終了・結果
  - `RebindManager`の状態遷移(既存ログがあれば流用)
- **ログの永続化方法**: 長時間(スパイク6の数時間、スパイク7の一晩)の計測では
  logcatのリングバッファが回転して初期のイベントが失われる。加えて
  `phase7-5-roaming-test.sh`の`_start_logcat`はadb接続を前提としており、
  Doze検証中に意図的にadbを切る手順を挟む場合はその間のログを取りこぼす。
  **Doze/長時間計測区間のイベントは、いったんローカル(SharedPreferencesまたは
  ローカルファイル)へ永続化し、フォアグラウンド復帰後・adb再接続後に
  まとめて吸い上げる**設計にする。
- ログ収集は`phase7-5-roaming-test.sh`の`_start_logcat`/`_stop_logcat`
  パターンをそのまま流用する。

各操作ステップの前にユーザーへ確認を取ること(既存スクリプトの規約、
実機を他セッションと共用している場合があるため)。

---

### スパイク1: Doze中のConnectivityManagerコールバック配送(案Aの成立条件)

**目的**: 案Aの前提(Doze中もnetwork callbackが届く)を検証する。
**round 2レビューの指摘により、1回の成否ではなく「配送率」として計測する**
(round 1の「Doze中でも配送されるため唯一の起床源」という記述は実測ではなく
推奨案としての断定だったため)。

**手順(round 3レビューで3つの欠落を修正)**:
1. 共通計測基盤の`ReconnectPolicy`上書きフックを使い、`tick`を大きく
   (例: 300秒)するのと**同時に`timeout`も十分大きく**する(例: 3600秒)。
   **`timeout`を既定60秒のままにすると、`elapsed += policy.tick`が最初の
   1 tickで`elapsed(300s) >= timeout(60s)`となりループが即座にgive upし、
   以降`notify_network_path_changed`の`Idle`分岐が`reconnect_loop_active`を
   見て起床通知自体を出さなくなる**(round 3レビューで発見、これを
   見落とすと後半の試行が全て無反応になり誤診断する)。
2. **セルラーを無効化するか機内モード相当にし、Wi-Fiを唯一の経路にする**
   (`adb shell svc data disable`等)。セルラーが生きたままだと
   `AndroidAppExecutor.kt`の集約ロジック(`anyPathAvailable`)がWi-Fiの
   ON/OFFを吸収してしまい、Wi-Fiを何度トグルしても集約後コールバックが
   一度も鳴らない、という**仕様通りの挙動をDozeのせいと誤診断**しうる
   (round 3レビューで発見)。
3. アプリをバックグラウンドへ(画面OFF)、意図的にセッションを切断
   (`FaultInjectionReceiver`の`CUT`)。
4. `adb shell dumpsys battery unplug`→`dumpsys deviceidle force-idle`で
   強制的にDeep Doze状態にする(共通計測基盤参照、`battery unplug`が
   無いと充電中判定でDozeに入らない)。
5. その状態でWi-Fi ON/OFFの切替を**複数回(最低10回)繰り返し**、
   `NetworkPathMonitor`内部の集約前(per-transport)コールバックの発火と、
   `onAggregateChanged`→`notifyNetworkPathChanged`→`reconnect_wake`が
   実際に何回・どれだけの遅延で着弾したかを、ローカル永続化ログから回収する。
6. tickは意図的に発火しない間隔にしてあるため、再接続が起きたとすれば
   それは`reconnect_wake`経由だと確定できる。計測後は`dumpsys deviceidle
   unforce`/`dumpsys battery reset`を忘れず実行する。

**判定指標**: 「OS側のネットワーク変化回数」に対する「アプリ側コールバック
着弾回数」の比率と、着弾までの遅延分布。比率がほぼ100%かつ遅延が数秒〜
十数秒以内なら案Aの前提は成立。取りこぼしが見られる、または遅延が数分に
及ぶ場合は、案Aを単独では採用できず、案B/Cとの併用または案Dへ倒す根拠になる。
**なお、`ADR_ANDROID_RECONNECT_TIMEOUT.md`§3.1.1が指摘する「起床の取りこぼし」
対策(pending_wake等)を先に実装してからこのスパイクを行うこと**——対策無しで
計測すると、コールバック自体は届いていてもin-flight中のpermit破棄で
再接続が起きず、「コールバックが届いていない」と誤診断するおそれがある。

**実施結果(2026-09-17、実機Sony XQ-DQ44、`feat/android-reconnect-spike-infra`の
debug APK、`pending_wake`実装済み)**: 案Aの前提は**成立**。

- 手順通りセルラー無効化(`svc data disable`、IMS専用ネットワークのみ残存し
  INTERNET capability無し)・`ReconnectPolicy`をtick=300s/timeout=3600sへ上書き・
  バックグラウンド化・`CUT`フォルト注入・`battery unplug`→`force-idle`
  (`mState=IDLE mLightState=OVERRIDE`確認済み)の順で実施。
- Deep Doze下でWi-Fiを**10回**ON/OFFトグル(各サイクル約13秒)。
  `ローカル永続化ログ`(`debug_dump_reconnect_log`)を回収したところ、
  `network_callback path=DIRECT callback=onLost/onAvailable`が**10往復(20件)
  すべて1:1で記録**されており、OS側のトグル回数に対する取りこぼしは
  **0件(配送率100%)**だった。
- reattach層(5回の試行、`REATTACH_MAX_RETRIES`)を使い切りorchestratorの
  reconnectループが`ConnPhase::Idle`のtick_wait状態(tick=300s)に入った後、
  さらに3回Wi-Fiをトグルしたところ、**3回とも**
  `notify_network_path_changed satisfied phase=Idle action=reconnect_wake`→
  `loop_woke_early`→`retry_attempt_in_flight on epoch=3 source=network_wake`
  という経路でループが即座に起床した。OS側の`onAvailable`記録時刻と
  `loop_woke_early`記録時刻の差は**1〜14ミリ秒**(実質ゼロ、Rust内部の
  伝播コスト以外の遅延は観測されず)。取りこぼし0/3、遅延は判定指標の
  「数秒〜十数秒以内」を大幅に下回る。
- 副産物: 起床自体は3回とも成功したものの、起床直後の再接続試行はいずれも
  即座に`reason=Channel send error`で失敗した——これは
  `ADR_ANDROID_POOL_STALE_HANDLE.md`(issue #120)で報告済みのSSHプール
  stale handle再利用バグが同一試行内で再現したもの(90秒のidle graceが
  経過する前だったため)。**案Aの「起床」自体は完全に機能しているが、
  起床後に実際に再接続が成功するには別途この pool バグの修正が必要**
  ——両ADRは独立ではなく直列に効く関係にあることが実機で確認できた。
- 後片付け: `dumpsys deviceidle unforce`・`dumpsys battery reset`・
  `svc data enable`・`CLEAR_RECONNECT_POLICY`・`RESTORE`+`CLEAR`
  (フォルト解除)を実施済み。

### スパイク2: WakeLockはDoze中のネットワークアクセスを実際に解除するか(案Cの評価)

**目的**: `PARTIAL_WAKE_LOCK`保持がDoze制限下のQUIC送受信に効くかを確認する。

**手順**: スパイク1と同条件(`battery unplug`→`force-idle`を忘れず)を
再現しつつ、今度は一時的な計測用コードで`PARTIAL_WAKE_LOCK`を明示的に
取得した状態で、tick駆動の再接続試行がDeep Doze下で実際に完了する
(QUICソケットが送受信できる)かを確認する。WakeLock無しの場合
(スパイク1相当)と比較する。

**判定**: WakeLock保持で改善が見られなければ、案Cは電池コストに見合わず
棄却できる。改善が見られれば、電池消費の実測(後述スパイク7)と合わせて
案Cの採否を判断する。

### スパイク3: AlarmManager.setExactAndAllowWhileIdleの実効間隔(案Bの評価)

**目的**: Deep Doze下でこのAPIが実際にどの間隔で発火するかを実測する
(公式ドキュメントの記述を鵜呑みにせず実機で確認する、というプロジェクトの
既存方針に従う)。

**手順**: 15秒間隔でアラームをスケジュールし続け、`battery unplug`→
`force-idle`でDeep Doze強制後に実際の発火間隔をログで記録する。

**判定**: 実効間隔が今回のターゲット(数十秒〜数分の圏外からの復旧)より
大幅に長ければ(公式ドキュメント通りなら分オーダー)、案Bはこの用途には
使えないと結論して除外する。

### スパイク4: 実際にorchestratorの`Idle`再接続ループへ落ちる頻度(案Eの評価、最優先)

**目的**: ADR1/2への投資対効果を左右する、最も重要な計測。「日常的な
ローミングのうちどれだけがmultipath/RebindManagerで無停止のまま吸収され、
どれだけが完全切断(`ConnPhase::Idle`)まで落ちるか」を実測する。

**手順(round 3レビューで方法論の誤りを修正)**: **`TransportPreference::Auto`は
multipathではない**(「ヘルパー経由QUICを試し、失敗したら通常のTCP SSHへ
フォールバックする」経路であり、`RebindManager`の参照は
`multipath_transport.rs`にしか存在しない——`Auto`で計測すると
`RebindManager`は一度も動かず、案Eを構造的に評価できない)。**最低2条件で
回すこと**:

1. `TransportPreference::IsekaiPipeQuicMultipath`(+`direct_host`設定、
   Tailscale⇔direct両対応)で1〜2日の実際の日常利用(通勤・自宅Wi-Fi⇔
   屋外セルラーの往復)——**案Eの評価用、これが主目的**。
2. `TransportPreference::Auto`で同様に1〜2日——**既定経路(多くのユーザーが
   実際に使う設定)の実態把握用**。

両条件で`RebindManager`の状態遷移ログ(条件1のみ発生)、reattach試行の
発生回数・成否(スパイク5の計装を流用、条件1・条件2両方の対象3経路
——isekai_pipe_quic/stun_p2p/link_relayで発生しうる)、
orchestratorが`Idle`→`Connecting`の再接続ループに入った回数・原因を
カウントする。

**判定指標を「断の総数/うちreattachが吸収/うち`Idle`まで落ちた」の3段で
集計すること**(round 3レビューで追加): reattach層(ADR2)を持つ経路では
reattachが成功した断は`phase`が`Connected`のまま推移し`Idle`ログに一切
現れないため、「`Idle`到達回数」だけを見ると実際に起きた切断の総数を
系統的に過小評価する。

**判定**: 条件1で`Idle`再接続がほぼ発生しない(大半がmultipath/
RebindManagerで吸収される)なら、ADR1/2の優先度を下げ、案Eの方向
(multipathのデフォルト化・適用範囲拡大)に投資を振り向ける。頻繁に`Idle`へ
落ちるなら、ADR1/2の価値が裏付けられ、案A〜Dのどれを選ぶかの検証
(スパイク1〜3)が引き続き重要になる。条件2との比較で、multipathを
デフォルト化した場合の実際の改善幅も見積もれる。

### スパイク5: 各トランスポートの1試行あたりの実in-flight時間(ADR1・ADR2共通、両層とも対象)

**目的**: round 1レビューの「現行60秒予算では実際には2〜3回しか試行できて
いない可能性が高い」という推測、およびround 2レビューの「オフラインでは
`resume_client.rs`の`attempt_reattach`が15秒のタイムアウトに毎回張り付き、
reattach層の実効予算は約90秒(レンジの上端)になる公算が高い」という
再訂正された推測の両方を実測で検証する。**ADR2にはround 1時点で対応する
実測タスクが無かった(round 2で指摘)ため、本スパイクをADR1・ADR2共通の
ものとして扱う。**

**手順**: `FaultInjectionReceiver`の`CUT`で切断後、次の2つを実セルラー網
(電波が弱い場所を含む)・実Wi-Fi網それぞれで記録する。QUIC経路とplain TCP
SSH経路の両方で計測する:
1. orchestrator層: `retry_attempt_in_flight`のON/OFF間隔(ADR1)。
2. reattach層: `resume_client.rs`の`attempt_reattach`内、
   `reconnect_and_resume`(dial・RESUMEそれぞれ`TRANSPORT_STEP_TIMEOUT=15秒`
   でバウンド)の実際の所要時間(ADR2)。特に「ワイルドカードbindは成功するが
   その後のconnectがタイムアウトに張り付く」という経路を実測で確認する。

**判定**: 実測値をADR1の`retry_interval`/バックオフ設計、ADR2の
reattach予算(タスク3、実測結果が出るまで保留中——本スパイクの結果で
保留を解除する)の妥当性検証に直接反映する。

### スパイク6: FGS(`specialUse`)の実機生存期間

**目的**: `AndroidManifest.xml`のコメントにあるWeb調査結果
(`specialUse`はAndroid 15/16/17でタイムアウト対象外)を、実際の長時間
バックグラウンド(30分〜数時間、画面OFF、アプリをタスクから明示的に
スワイプで消していない状態)で裏付ける。

**手順**: `battery unplug`→`force-idle`(共通計測基盤参照)でDeep Dozeに
した状態で、セッション接続状態のままアプリをバックグラウンドへ送り、
`adb shell dumpsys activity services`でプロセス・サービスの生存を
定期的に確認する。バッテリー最適化がデフォルト(除外なし)の状態で行う
(ADR1 §3.5の既存方針=除外を要求しないため、この条件が実運用の実態)。
長時間計測になるため、ログはローカル永続化(共通計測基盤参照)を使う。

**判定**: 数時間生存が確認できれば案A/D双方の前提として十分。数十分で
OSに終了させられるなら、`AUTO_REATTACH_GRACE_SECS=30分`という既存値との
整合(ADR1 §3.7)を見直す必要がある。

### スパイク7: 各案の電池消費の相対比較

**目的**: 案A(コールバックのみ)・案C(有界WakeLock)それぞれを一晩(画面OFF、
`battery unplug`→`force-idle`済み、接続維持)動作させ、
`adb shell dumpsys batterystats`でバッテリー消費を比較する。

**判定**: 案Cが案Aに対して有意に電池を食うなら、スパイク2で効果が
確認できたとしても採用を見送る根拠になる。

---

## 4. スパイク結果からの意思決定の流れ

1. **まずスパイク4を実行**: `Idle`再接続の実発生頻度を見る。稀であれば
   ADR1/2は「稀に起きる最後の砦」としての設計に留め、multipath拡大
   (案E)を優先課題に格上げする、という方針転換をこの時点で検討する。
2. 頻度に関わらず最低限必要な改善として、スパイク1・3を実行し、
   案A/Bの実効性を判定する。案Aが機能するなら最有力候補として残す。
3. スパイク2・7で案Cの要否を判定する(案Aで足りるなら不要)。
4. スパイク5でADR1/2の具体的な数値(retry_interval、reattach deadline)を
   実測値ベースに更新する。
5. スパイク6でFGS生存期間の前提を確認し、`AUTO_REATTACH_GRACE_SECS`との
   整合を取る。
6. 以上を踏まえてADR1/2を改訂する(必要なら案Dの要素——WorkManagerによる
   バックグラウンド定期バーストや、コールドスタート再アタッチへの一本化——を
   部分的に取り込む)。改訂後、再度`opus-adversarial-consult`へ回す。

## 5. 非目標

- 本計画自体はコード実装を含まない。スパイク用の一時計測コードは
  本計画の承認後、`android/src/debug`配下に既存の`FaultInjectionReceiver`と
  同じ位置づけ(debugビルド専用、releaseに含まれない)で追加する。
- 他OEM端末での網羅的な検証は、実機が入手できる範囲でのベストエフォートに
  留める(単一OEMでの結果である旨を結論に明記する)。

## 6. 参照実装

- `rust-core/scripts/phase7-5-roaming-test.sh`(既存ローミングテスト、
  フォルト注入+ネットワーク切替の型を流用)
- `android/src/debug/kotlin/tools/isekai/terminal/debug/FaultInjectionReceiver.kt`
  (既存debug用broadcast受信口、新規計測トリガーもこのパターンで追加)
- `rust-core/src/faulty_udp_socket.rs` / `rust-core/src/debug_fault.rs`
