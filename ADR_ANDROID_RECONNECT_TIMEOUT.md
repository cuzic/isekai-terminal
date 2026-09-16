# ADR: 完全切断時の自動再接続タイムアウトを見直す(Android)

- **Status**: Draft(2026-09-16起草。round 1レビューで§1の現状認識(フォア
  グラウンド復帰による自動復旧が実際には機能していない)を訂正し、B案を
  「締切を外す」から「起床源をnetwork callback主導に切り替える」へ書き直した。
  round 2レビューで、B案が「起床の取りこぼしを致命化させる」欠落(§3.1)・
  「認証失敗を無限にリトライする」欠落(§3.2、新規タスク追加)・
  iOSとの矛盾(§3.4)を指摘され修正。ユーザー判断により、これ以上の設計判断は
  `ANDROID_RECONNECT_SPIKE_PLAN.md`の実機スパイク結果を得てから行う——
  本ADRは現時点でのDraftとして凍結し、スパイク結果を受けて改訂する)
- **対象**(見込み、要精査): `rust-core/src/orchestrator.rs`(`ReconnectPolicy`・
  `spawn_reconnect_loop`・`BackgroundState`)。`ConnectionPublicState::Reconnecting`の
  表現変更(§3.4、sentinel案なら型変更・UniFFI再生成は不要)を伴う見込み
- **入力**: 2026-09-16、Windows `isekai-ssh` との接続安定化ギャップ分析セッション。
  round 1・round 2レビュー
  (`/tmp/claude-1001/-home-cuzic-isekai-terminal/2285f8d6-e7bb-4a20-83c5-de317b68d9c7/scratchpad/opus-review-android-reconnect-round{1,2}.md`)
- **拘束される既存ルール**: `.claude/rules/rust-ssot.md`、
  `.claude/rules/always-connects.md`

---

## 1. 背景

Android(`isekai-terminal-core`)の完全切断からの自動再接続は
`rust-core/src/orchestrator.rs`の`SessionOrchestrator`が担う
(tssh風reconnect、`spawn_reconnect_loop`)。現状の`ReconnectPolicy::default()`:

```rust
impl Default for ReconnectPolicy {
    fn default() -> Self {
        Self {
            tick: Duration::from_secs(1),
            retry_interval: Duration::from_secs(3),
            timeout: Duration::from_secs(60),
        }
    }
}
```

`timeout`に達すると`reconnect_loop_active = false`にして
`ConnectionPublicState::Disconnected{ reason: "reconnect timed out after 60s" }`
を通知する。

### 1.1 60秒後の復旧手段はUI操作のみ(round 1レビューで訂正した現状認識)

**当初案は「timeout後はフォアグラウンド復帰(`notify_will_enter_foreground`)で
自動的に再接続される」ことを前提に、この60秒を軽微な問題と位置づけていたが、
これは誤り。Androidの実機ではこの経路自体が機能していない。**

`BackgroundState::Suspended`へ遷移する入口は`notify_background_budget_expired`
(`orchestrator.rs:1261`)と`notify_memory_warning`(`orchestrator.rs:1271`)の
2つだけだが、Android本体のproduction Kotlinコードにはこのどちらの呼び出し元も
存在しない(`android/src/main`内のヒットはUniFFI生成コードの宣言のみ、
`src/test`/`src/androidTest`の`FakeSshGateway.kt`は空実装のフェイク。
`onTrimMemory`/`onLowMemory`のオーバーライドもリポジトリ全体でゼロ)。

したがって`background_state`は実機では`Foreground`/`Quiescing`の2値しか
取らず、`notify_will_enter_foreground`の`was_suspended`は常に`false`——
`reconnect_with`は常に`None`になる。Kotlin側にも代替の自動再接続は無い。

**つまり現状、60秒のタイムアウト後にユーザーが取れる復旧手段はUIの明示的な
再接続操作のみである。** `always-connects.md`の基準では、これ自体が既に
バグに該当する。

(副次的な独立修正、ただし効果範囲は限定的——round 2レビューで訂正:
`notify_memory_warning`は`background_state == Quiescing`の場合にしか
`Suspended`へ遷移させず(`orchestrator.rs:1271-1276`)、`Quiescing`になるのは
`notify_did_enter_background`時かつ`phase`が`Connected`/`Connecting`の
ときだけ(`:1253-1255`)。したがってこれを`Application.onTrimMemory
(TRIM_MEMORY_COMPLETE)`等から配線しても、**塞がるのはバックグラウンド中に
OSからメモリ逼迫を告げられた場合のみ**であり、本節の本論(60秒経過後の
復旧手段がUI操作のみであること)は変わらない。本ADR本体とは独立に、
小さく先出しできる修正ではある)

### 1.2 「時間ベースの締切を単調加算するだけ」も厳密には不正確

`spawn_reconnect_loop`の`woke_early`分岐(`orchestrator.rs:980-1010`)は
`notify_network_path_changed(true)`による早期起床時、`elapsed`/`tick_count`を
進めずに`continue`する。したがって、Wi-Fiスキャン・セルラーハンドオーバで
パス変化が連発する実機では、「60秒」は壁時計上60秒より長くなりうる
(安全側だが値は非決定的)。§3のB案が「時間ベースの締切をやめる」方向に
一致することの傍証でもある。

### 1.3 Windows(isekai-ssh)との対比

`ISEKAI_PIPE_DESIGN.md` Epic N-3で、isekai-ssh側はresume windowの既定値を
120秒→10日へ引き上げ、`wrapper.rs`の`RECONNECT_BUDGET`(24時間)という
予算軸を持つ。Android側はこの教訓が反映されないまま60秒だけが残っている。

### 1.4 マルチパス経路には本ADRの層以外に砦が無い(`ADR_ANDROID_MIDSESSION_RESUME_BUDGET.md`との関係)

`ADR_ANDROID_MIDSESSION_RESUME_BUDGET.md`(round 1レビューで判明)の通り、
`multipath_transport.rs`にはmid-sessionのreattach層が存在しない。
`RebindManager`のWi-Fi⇔セルラーフェイルオーバーが失敗した場合、即座に
本ADRが扱う60秒ループへ処理が移る。実機で最も頻繁に踏まれるのはこの経路と
見られ、**本ADRは`ADR_ANDROID_MIDSESSION_RESUME_BUDGET.md`より優先度が高く、
単独で先行して出荷できる。**

### 1.5 Foreground Serviceとの関係

`TerminalSessionService.kt`はForeground Serviceとして動作する。
`AndroidManifest.xml:34-56`に、`foregroundServiceType="specialUse"`を選んだ
経緯が記録されている——dataSync/mediaProcessing/shortServiceは
Android 15+の累積6時間クォータ・短時間強制終了で実機クラッシュループを
起こした経緯(2026-07-27)から却下、specialUseはAndroid 15/16/17いずれでも
OSのタイムアウト対象外であることを調査済み。**つまりFGSがOSのタイムアウトで
強制終了されるリスクは既に調査済みでNo。**

ただし`TerminalSession.kt:576`の`notifyDidEnterBackground()`が渡す
`budgetMs=0`は「Androidでは猶予管理不要」という設計コメント通りだが、
Rust側の`ReconnectPolicy.timeout`という**別の**時間ベースの締切が、
このFGSの生存能力を活かせていない、というのが本ADRの本題である。

## 2. 問題

- 60秒は地下・トンネル・機内モード解除待ちなど、スマホで数十秒〜数分
  オーダーの圏外が日常的に起きる状況を想定していない。
- §1.1の通り、60秒経過後はユーザーの明示操作以外に復旧経路が無い
  ——`always-connects.md`違反。
- §1.4の通り、マルチパス経路ではこの60秒が唯一の砦になっている。

## 3. 実装方針

### 3.1 B案: 時間ベースの締切から「起床源の切り替え」へ(round 1レビューで書き直し)

**当初案「Foreground Service生存中は実質無制限、締切という概念自体を外す」は
そのままでは技術的に誤り。** FGSは「プロセスがOSに殺されにくくなる」保証で
あって、「端末がサスペンドしてもCPUが動き続ける」保証ではない。リポジトリ
全体に`WakeLock`の取得箇所は存在せず(grep済み、`PhysicalPathProvider.kt:96`の
`acquire()`はfd取得で無関係)、`WAKE_LOCK`パーミッション宣言も無い。画面OFFで
端末がsuspendすると、`spawn_reconnect_loop`の`tokio::time::sleep(policy.tick)`
(`orchestrator.rs:972`)もQUICのkeepalive/idle timeoutも発火しない。つまり
「FGSが生きている間は無制限に再接続を試み続ける」は、実機では**「端末が
起きている間だけ」試み続ける**に縮退する。深夜のポケットの中ではtickは
間引かれる。

したがって**本ADRの核心は「締切を外すこと」ではなく「起床源を何にするか」に
書き換える**:

1. **network callback主導にする(候補)**: 既に`reconnect_wake`
   (`orchestrator.rs:440`, `1535-1537`)という経路がある。`ConnectivityManager`の
   コールバックがDoze中でも配送されるという性質を利用し、現在「tickによる
   定期リトライ」を主、「network restored」を従、と捉えている構造を
   **逆転**させ、network restoredを主・tickを保険とする。
   **round 2レビューで、この案の成立には実測に加え次の実装課題があると
   判明した(§3.1.1)。**
2. `AlarmManager.setExactAndAllowWhileIdle`でバックオフを刻む(Doze中は
   9分以上の間隔制限がある)。
3. `PARTIAL_WAKE_LOCK`を取る(電池を食う。§3.5の既存方針と衝突しうるため
   慎重に検討)。

**実装着手前に、この分岐を決めるための実測を先に行う**(§3.6参照)。

#### 3.1.1 起床の取りこぼしが致命化する3つの要因(round 2レビューで新規発見、案1の必須対応)

現状の実装(tickが3秒ごとに必ず試す)では、以下の3点が重なっても最大数秒の
損で済んでいる。**しかしtickを「保険」に格下げ(=間隔をバックオフで伸ばす)
した瞬間、この3つが合成して「圏外から復帰したのに数分間気付かない」経路が
生まれる。**

1. **`NetworkPathMonitor`のコールバックはエッジトリガで、取りこぼしの
   再配送が無い**: `AndroidAppExecutor.kt:85-89`は「利用可能になった瞬間」
   にしか`onAvailable()`を呼ばない。1回取りこぼすと、次にネットワークが
   一度落ちて戻るまで二度と鳴らない。
2. **`reconnect_wake`のpermitはin-flight中に消費されて捨てられる**:
   `spawn_reconnect_loop`の`woke_early`分岐(`orchestrator.rs:980-1010`)は、
   `retry_attempt_in_flight`が立っている間に届いた「ネットワーク復帰」を
   何の効果も残さずに捨てる(`should_attempt == false`でも`continue`する
   だけで再arm無し)。
3. **in-flightの試行は古い(死んだ)経路の上で長時間居座り、キャンセル
   手段が無い**: `retry_attempt_in_flight`をfalseに戻せるのはセッション
   コールバック(`:774`, `:527-530`)か同期`Err`(`:1052-1058`)だけ。QUIC経路で
   15秒級、プレーンTCP SSH経路ではOSのconnect timeout(数十秒〜)。その間、
   上記2により復帰シグナルは捨てられ続ける。

**したがって案1を採る場合、次のどちらかが必須**(ADRのタスクとして追加):
- `pending_wake: bool`を`OrchestratorState`に持ち、in-flightの試行が
  解決した直後に即座に再試行する、または
- パス変化時にin-flightの試行をabortする(現状`connect_via`にキャンセル
  機構は無いので新設が必要)。

### 3.2 orchestrator層の失敗分類(round 2レビューで新規追加——`ADR_ANDROID_MIDSESSION_RESUME_BUDGET.md`タスク1と同型)

**B案(§3.1)は「無期限リトライ」または「大幅に長いリトライ」を導入するが、
現状のorchestratorは失敗理由を一切見ずに同じ手順を繰り返す。これは
`ADR_ANDROID_MIDSESSION_RESUME_BUDGET.md`が対象にしていたreattach層と
同型の欠落だが、締切を外す(=長時間化させる)のはこちらの方であり、
危険度はこちらの方が大きい。**

- `DisconnectKind::classify`(`orchestrator.rs:744-752`)が特別扱いするのは
  `"remote process exited"`と`"network lost"`の2文字列のみで、それ以外は
  全て`TransportError`=自動再接続の対象になる。
- 認証失敗(`rust-core/src/transport/ssh_handler.rs:594`の
  `"Authentication failed"`、`:524`の`"jump host authentication failed"`)も
  `TransportError`に落ちる。
- 一度`Connected`に到達した後にループが回り始めると、以降の試行が
  認証失敗・ホスト鍵不一致で失敗しても`handle_unexpected_disconnect`は
  `s.reconnect_loop_active`で`Action::Suppress`(`:776-777`)に倒れ、ループが
  次のtickで淡々と再試行し続ける。

現行60秒なら実質2〜3回で打ち止めだったものが、締切を伸ばす/外すと
**恒久的に失敗し続ける経路が新設される**:
- 自前サーバー(ユーザー自身のマシン)のfail2ban/sshdの`MaxAuthTries`に
  自分の端末が焼かれる——このプロジェクトのサーバーはユーザー自身が
  運用しているため、実害が直接ユーザーに返ってくる。
- ホスト鍵確認プロンプトが繰り返し出る。`retry_attempt_in_flight`の
  フィールドdoc(`orchestrator.rs:337-341`)が「ホスト鍵確認プロンプトの
  多重発生を防ぐ」ためにあると明記していること自体、作者がこの種の危険を
  既に認識している証拠。無期限ループはその防御を時間軸方向に迂回する。

**タスク**: 恒久的失敗(認証失敗・ホスト鍵不一致等)と判定できるものは
即座に諦めて`Disconnected`を出す分類を`orchestrator.rs`に追加する。
`ADR_ANDROID_MIDSESSION_RESUME_BUDGET.md`§3.1と完全に同型の作業であり、
2本のADRで同じ設計判断を共有できる。

**round 3レビューで発見された2つの追加要件(必須)**:

1. **単発判定ではなくガードを課すこと**: `ADR_ANDROID_MIDSESSION_RESUME_BUDGET.md`
   §3.1は「1回の確定的に見えるシグナルは消滅の証拠にならない」という
   Epic N-3の教訓から、`UNKNOWN_SESSION_CONFIRM_THRESHOLD`(N回連続)+
   `UNKNOWN_SESSION_MIN_ELAPSED_FLOOR`(最小経過時間)の2つ組を必須にしている。
   本タスクも同型のガードを課すこと——**単発判定にしてはいけない**。
   理由: `isekai_pipe_quic_transport.rs:304`の
   `"bootstrap SSH authentication failed"`はブートストラップSSH
   (isekai-pipeを配る側)の失敗であり、サーバー再起動中・鍵配布途中など
   一時的条件でも起こりうる。`ssh_handler.rs:524`の
   `"jump host authentication failed"`も踏み台側の一時不調で起こりうる。
   これらを単発で恒久的失敗に分類すると、一時的事象で自動再接続が
   永久に止まる——**しかもADR1の誤give-upは、ADR2の誤give-up(reattachを
   諦めてorchestratorが新規接続する=繋がる)より実害が大きい**: ADR1の
   誤give-upは自動再接続そのものを諦めることを意味し、§1.1の通り
   Androidでは以後UIの明示操作以外に復旧手段が無い状態に落ちる
   ——`always-connects.md`違反そのもの。
2. **判定方式(文字列マッチ拡張 or 型付き理由導入)を実装前に明示すること**:
   `DisconnectKind::classify`のdoc(`orchestrator.rs:717-728`)は「`SessionCallback`
   にはテスト専用実装が4箇所あり、シグネチャ変更・UniFFI経由でKotlinに
   公開される文字列の変更はそれら全ての更新を要する大きめの変更になるため
   見送っている」と明記している。本タスクを文字列マッチの拡張で実装すると
   このdocが警告している増殖をまさに引き起こす。型付き理由の導入は
   より健全だが、docが言う「大きめの変更」に該当し、ADRが当初見積もった
   規模を超える可能性がある。**実装着手時にどちらを取るか明示的に決定し、
   決定内容をこのADRに書き戻すこと(現時点では未決)。**

### 3.3 `retry_interval`の指数バックオフ化(round 1レビューでコスト評価済み)

`ReconnectPolicy`は`Copy`な小構造体でテストからの差し替え以外の用途が無く、
フィールド追加(`max_retry_interval`・`jitter`等)のコストは低い。剰余判定
(`ticks_per_retry = retry_interval / tick`)をバックオフに置き換える必要は
あるが(`next_retry_tick`+`current_interval`をループローカルに持つ形へ)、
壊れうる既存テストは2本のみで、どちらも`retry_interval`を「絶対に発火
しない大きな値」として使っているだけなので初回間隔を変えなければ影響しない
——**この変更自体は半日仕事であり、実装を止める理由にはならない。**

ただし、バックオフの議論より先に**1回の試行が何秒in-flightで居座るか**を
確認すべき。`retry_attempt_in_flight`が立っている間は次の試行が発火しない
ため、実効リトライ回数を決めているのは`retry_interval`ではなく「1試行の
所要時間」である。QUIC経路は`TRANSPORT_STEP_TIMEOUT=15秒`級、プレーンTCP
SSH経路はOSのconnect timeout(数十秒〜)まで伸びうる。現行60秒予算では、
圏外時に実際には**2〜3回しか試行できていない**可能性が高い。

**jitterは「要否検討」ではなく必須**: `onNetworkPathChanged`等は
`forEachPane`で全ペインにファンアウトされ(`TerminalTabsViewModel.kt:597,610`)、
全ペインが同一の`ReconnectPolicy::default()`を持つため、複数タブは完全に
同期して同じ瞬間に同じサーバーへdialする。現状60秒で終わるから顕在化して
いないだけで、無期限化すると同時dialが延々と重なる。

### 3.4 `ConnectionPublicState::Reconnecting`の締切無期限化の表現方法(open questionから格上げ、round 2レビューで訂正)

`Reconnecting{elapsed_secs: u32, timeout_secs: u32}`をKotlin側が両方とも
必ず表示している(`ConnectionStateMapper.kt:42`「再接続中…
(${elapsedSecs}/${timeoutSecs}秒)」)。

**round 1時点では「`Option<u32>`かsentinel値が必要で、UniFFIの公開API変更に
当たる」とだけ書いていたが、round 2レビューで、この変更が§4の非目標
(iOS側は対象外)と矛盾することが判明した**: `ConnectionPublicState::Reconnecting`は
Kotlin/Swift両方が消費する共有型であり、Swift側も`timeoutSecs: UInt32`を
直接消費している
(`ios/Sources/IsekaiTerminalCore/TerminalSessionController.swift:75,817-819`、
`TerminalView.swift:392-395`)。型を`Option<u32>`化すればSwift側はコンパイル
エラーになり、`.claude/rules/uniffi-binding-regeneration.md`が定めるSwift
バインディングのdrift-check(`ios-logic-linux-check.yml`/`ios-rust-core-check.yml`)
にも波及する。**§4の非目標は「iOSの*挙動*(締切ポリシー)は変えない」の
意味であり、「型変更に伴うSwift側の追従とバインディング再生成」まで
除外するものではないと明記し直す。**

**代替案(round 2レビューで追加、round 3レビューでsentinel値を訂正)**:
型自体は変えず、`timeout_secs`にsentinel値を割り当てて「無期限」を表す。
**round 2時点では`0`を提案し「`ReconnectPolicy.timeout`は常に正の値なので
0は発生しない」としたが、これは誤りだった**: ワイヤに乗るのは`Duration`
そのものではなく`orchestrator.rs:950`の`policy.timeout.as_secs() as u32`
という**秒への切り捨て**であり、`fast_test_policy()`
(`timeout: Duration::from_millis(200)`)や
`reconnect_loop_gives_up_after_timeout_and_notifies_disconnected`が使う
`Duration::from_millis(40)`のようなサブ秒policyでは`as_secs() == 0`が
**既に発生している**。`0`をsentinelにすると、これら約20本の既存テストが
一斉に「無期限」の意味を運び始めてしまい、テストでは無期限扱い・本番では
有限という検証不能な状態になる上、将来サブ秒policyを足すたびに黙って
「無期限」化する恒久的なfootgunが残る。**sentinelには`u32::MAX`を使う**
(`ReconnectPolicy.timeout`が136年に達することは無いため切り捨てでも
衝突しない)。この案なら**Rust/Kotlin/Swiftいずれも型変更・UniFFI再生成が
不要**という利点はそのまま維持できる——表示側の分岐
(`ConnectionStateMapper.kt:42`と`TerminalView.swift:395`に
`if timeoutSecs == UInt32.MAX`相当を足すだけ)で済む。実装コストが
一段下がるため、まずこちらを検討すること。

なお「キャンセル導線が十分か」という懸念は**既に解決済み**: 
`TerminalHostScreen.kt:533`の`onCancelReconnect`→`TerminalSession.kt:487`→
`SessionOrchestrator::cancel_reconnect`(`orchestrator.rs:1213-1230`)が
`isReconnecting`の間だけ表示される形で既に実装されている。

**無期限化はJNIコールバック頻度にも影響する**: `spawn_reconnect_loop`は
毎tickで`on_connection_state_changed(Reconnecting{...})`を発火する
(`orchestrator.rs:1033-1037`)。現状は最大60回で終わるが、無期限化すると
数時間で数千〜数万回、そのたびにKotlin側のStateFlow更新→Compose
recompositionが走り、`forEachPane`でタブ/ペインごとに独立したループが
回る(`TerminalTabsViewModel.kt:597`)。**UI通知の頻度とリトライの間隔を
分離する**必要がある(例: バックグラウンド時はtick通知を止める、または
通知間隔をリトライ間隔に合わせて伸ばす)。判断材料は既に`OrchestratorState::
app_foreground`(`orchestrator.rs:379`)にある。

**出荷ゲート(round 2レビューで追加)**: `ADR_ANDROID_MIDSESSION_RESUME_BUDGET.md`と
同様、再接続中の状態(無期限リトライ中であること・キャンセル導線)がUIに
正しく表現されるようになるまで、締切の延長・無期限化のいずれも出荷しない。

### 3.5 バッテリー最適化除外は要求しない(既存方針との整合、open questionから格上げ)

`rust-core/src/background_reliability_policy.rs`のモジュールdocに、
「`REQUEST_IGNORE_BATTERY_OPTIMIZATIONS`権限は追加しない(Playの
acceptable use casesにSSHクライアントは該当せず、誤用はアプリ停止措置の
対象になりうる)。標準API(`PowerManager.isIgnoringBatteryOptimizations`/
`Settings.ACTION_IGNORE_BATTERY_OPTIMIZATION_SETTINGS`)のみを使う前提」と
既に明記されている。**本ADRの実装もこの制約に従う**——B案がDoze環境で
実際には無制限に粘れない(§3.1)ことへの対処として、バッテリー最適化除外を
要求する方向には進まない。

### 3.6 実装着手前に実測すべき3指標(round 2レビューで3つ目を追加・測定手段を修正)

無期限リトライを入れる前に、次の3つを実測してから§3.1の起床源をどれに
するか決定する:

1. `retry_attempt_in_flight`が立っていた実時間(1試行あたりの所要時間、
   §3.3の裏取り)。
2. 画面OFF中に実際に発火したtick数 vs 経過壁時計秒(Dozeでどれだけ
   間引かれたかが直接分かる)。
3. **(round 2レビューで追加、最重要)** 画面OFF/Doze中に
   `onNetworkPathChanged(isSatisfied=true)`が実際に何回届いたか vs
   同区間に実際に起きたネットワーク変化の回数。§3.1の案1(network callback
   主導)の妥当性はこの1点に全面的にかかっており、round 1レビューが
   「Doze中でも配送されるため唯一の起床源」と書いたのは実測に基づく
   事実ではなく推奨案としての記述だった——**断定を外し、この実測で決める。**
   (Dozeは端末静止を条件に入り、「圏外から復帰する」シナリオは通常ユーザーの
   移動を伴う=Dozeを抜ける、という傍証はあるが、これは推論であって測定
   ではない)

**測定手段の注意(round 3レビューでround 2の誤りを訂正)**: `RemoteLogger`は
`android.util.Log`の薄いラッパー(logcatのみ)であり、ネットワーク経由では
**ない**(round 2レビューは誤り)。ローカル永続化が必要な本当の理由は、
(a) logcatのリングバッファは長時間計測(スパイク6・7)で回転し初期の
イベントを失う、(b) `phase7-5-roaming-test.sh`のログ収集はadb接続を前提と
しており、Doze検証中にadbを意図的に切る手順を挟む場合はその間を
取りこぼす、の2点。指標2・3は、Doze/長時間計測中はローカルへ永続化
(既存のSharedPreferencesかファイル)してから後で吸い上げる形にする。
詳細と具体的な計測手順は`ANDROID_RECONNECT_SPIKE_PLAN.md`参照(Doze強制
遷移には`adb shell dumpsys battery unplug`が前提条件として必須——
充電中はDozeに入らないため、これが無いと指標2・3が全て空振りする)。

### 3.7 `AUTO_REATTACH_GRACE_SECS`(既存の第3層)との整合(round 2レビューで比較の枠組みを訂正)

`rust-core/src/reattach_persistence.rs`は、プロセスkillからの黙示的
セッション再アタッチを`AUTO_REATTACH_GRACE_SECS = 30分`という既存の
数値で管理している(ワイヤレベルRESUMEはプロセス再起動後には原理的に
使えないため——russhの暗号状態と`ReplayBuffer`がメモリ上にしかない)。
`rust-core/src/background_reliability_policy.rs`の
`UNEXPECTED_KILL_THRESHOLD=2`/`GUIDANCE_COOLDOWN_SECS=14日`も同様に
既存の数値。

**実際の姿は「orchestratorの再接続ループ」「reattach層」の2層構造ではなく、
FGSごとプロセスが死んだ場合に効く3層目が既に存在する**: FGSが死ぬと
プロセスごと死ぬためRust側の締切を論じる余地が無くなり、次回コールド
スタート時に`reattach_persistence`の30分ポリシーが拾う。

**round 2レビューで訂正: `AUTO_REATTACH_GRACE_SECS`とorchestratorの締切は
競合する予算ではなく直交する。** 前者は「プロセスがkillされた後、次回
コールドスタート時にどれだけ古いタブ記録まで自動復元してよいか」の
ポリシーで、プロセスが生きている限り一切発動しない。後者は「プロセスが
生きている間、どれだけ再接続を試み続けるか」。**「30分より長く粘る意味が
あるのか」という問いは「じゃあreconnectループも30分で打ち切ろう」という
根拠のない結論に実装者を誘導しうるため避ける。正しい問いは「プロセスが
生き残って(例えば)30分以上粘った場合と、killされて30分以内にコールド
スタート復元される場合とで、ユーザーから見た復旧体験が食い違わないか」
である。**

## 4. 非目標

- iOS側の**挙動**(`BackgroundState`の`budget_ms`が有限に効く可能性がある
  ため、締切ポリシー自体はAndroidと同じにしない)は本ADRの範囲外。
  **ただしround 2レビューで訂正した通り、§3.4が扱う型変更に伴うSwift側の
  追従・UniFFIバインディング再生成は本ADRの範囲に含む**(型はKotlin/Swift
  共有であり、変更すれば両方に波及するため)。§3.4のsentinel案(型を
  変えない)を採用できれば、この非目標との緊張自体が解消される。
- `resume_client.rs`側のmid-session reattach予算は
  `ADR_ANDROID_MIDSESSION_RESUME_BUDGET.md`で扱う別レイヤの問題。

## 5. Open Questions

- §3.1の起床源(network callback主導/AlarmManager/WakeLock)のうち
  どれを採るか——§3.6の実測結果を見てから決定する。
- **(round 3レビューで§3.7の訂正に合わせて問いを立て直し)** プロセスが
  生き残ってorchestratorの締切いっぱいまで粘った場合と、プロセスがkillされ
  `AUTO_REATTACH_GRACE_SECS=30分`(§3.7)以内にコールドスタート復元される
  場合とで、ユーザーから見た復旧体験(復旧までの時間・見え方)が食い違わ
  ないか。
- C案(`ConnectionProfile`単位での設定可能化)に後から進む場合、
  `scripts/reserve-room-migration.sh`によるRoom migration番号の予約が
  必要(`CLAUDE.md`のルール)。

## 6. 参照実装

(実装後に追記)
