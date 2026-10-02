# ADR: 接続/セッションライフサイクルの可観測性(tracing化)

- **Status**: **Accepted**(2026-09-11起草・同日Accepted。opus-adversarial-
  consult 3ラウンドで収束。Round 1: logger二重初期化問題とPhase 1計装
  対象の取り違え。Round 2: (1)Subscriber設置が`init_logger()`と
  `init_diagnostics()`の2箇所に分裂し同じ「1枠しか持てない」問題を
  1階層上で再現していた点、(2)計装対象`android_quic_endpoint.rs`が
  計装しようのない純粋factoryだった点、(3)`noq`自体が`tracing`に依存し
  フィルタ設計が丸ごと抜けていた点、(4)Phase 1の機構がLinux CIから
  到達不能な点。Round 3: `MainActivity.onCreate()`選定の裏取りはOK、
  ただし`tracing-appender`にsize-basedローテーションは存在しない
  (時間基準のみ)という実装詳細の誤りを指摘され、本版で
  `DAILY`+`max_log_files`に訂正して最終Approve)
- **対象**(見込み、要精査): `rust-core/src`(`orchestrator.rs`・
  `resume_client.rs`・`isekai_pipe_quic_transport.rs`・`lib.rs`の
  `init_logger`/新設`diagnostics`モジュール)、`rust-core/isekai-pipe`
  (`main.rs`・`connect.rs`・`engine/`・`resume_loop.rs`——**Phase 2以降**)、
  `rust-core/isekai-ssh`(`log_file.rs`・`wrapper.rs`——**Phase 3、変更ほぼ
  無し**)、`android/`(`MainActivity.kt`のUniFFI初期化呼び出し1箇所)
- **入力**: ユーザーから「外出先→帰宅のローミングでisekai-ssh(比較対象の別
  クライアント`tssh`は繋がり続けた)が切断した。原因調査用に内部状態を
  ログ/tracing/metricsで残せるようにしたい」という相談(2026-09-11)。
  Round 1のopus-adversarial-consultで`resume_client.rs`/
  `android_quic_endpoint.rs`の実コードを裏取りした結果を反映
- **拘束される既存ルール**: `.claude/rules/rust-ssot.md`、
  `.claude/rules/always-connects.md`(本ADRが直接支える)、
  `.claude/rules/prefer-gh-actions-over-local-cargo.md`、
  `.claude/rules/uniffi-binding-regeneration.md`

---

## 1. 背景

### 1.1 何が起きたか

外出先(ローミング中)では接続を維持できていたが、帰宅してネットワークpathが
再び変わった(モバイル回線→自宅Wi-Fi/Tailscale)タイミングでisekai-ssh
(Androidアプリ)の接続が切れ、比較対象の別クライアント(`tssh`)は繋がり
続けた。朝になって気づいたため、発生時点の内部状態は一切追えなかった。

### 1.2 現状把握

- `log`クレート(0.4)は`rust-core`配下ほぼ全クレートで使われ、Android向けには
  `rust-core/src/lib.rs`の`init_logger()`が`android_logger::init_once(...)`を
  呼んでいる(5箇所——`lib.rs`・`orchestrator.rs`・
  `isekai_pipe_quic_transport.rs`・`isekai_link_relay_transport.rs`・
  `multipath_transport.rs`——から冪等に呼ばれる設計)。logcatへは既に
  流れているが、リングバッファのため夜間のうちに上書き・消失しうる。
- `tracing`クレートは`h3-noq`/`h3-qmux`のoptional featureとしてのみ存在し、
  `isekai-terminal-core`/`isekai-pipe`/`isekai-ssh`の実コードでは未使用。
- `isekai-ssh`の診断ログ経路は実は**2系統ある**(Round 1で誤認していた点):
  1. 既定(`--isekai-log-file`未指定時): `wrapper.rs`が`ISEKAI_INTENT_ID`と
     `ISEKAI_PIPE_LOG_FILE`(自分の`init_verbose`が開いた既定verboseログの
     パス)を子プロセス`isekai-pipe connect`のenvに設定する。`connect.rs`は
     この`ISEKAI_PIPE_LOG_FILE`を見て自分の`env_logger`の出力先を同じ
     ファイルへ切り替える(`connect.rs`内`open_log_file_target`)——**env経由の
     間接連携**であり、stderrのpipe/teeではない。
  2. `--isekai-log-file <PATH>`明示時のみ: `wrapper.rs`が
     `command.stderr(Stdio::piped())`し、`isekai-pipe connect`のstderrを
     実際にteeする(`log_file::is_enabled()`分岐)。
  いずれの経路でも、**現状`isekai-pipe connect`のログ行に`SessionId`は
  含まれていない**。
- `isekai-pipe-core/src/profile.rs`に`default_log_file()`/
  `resolve_state_path()`という「envで上書き可能なXDG state dir解決」の
  既存共通実装が既にある(`ISEKAI_PIPE_LOG_FILE`が使っているのと同じ
  ヘルパー)。§3.4はこれを再利用する(Round 1では誤って「新設」としていた)。
- `isekai-pipe serve`は`2>$tmpdir/log`(`helper_bootstrap.rs`/
  `install_script.rs`)——再デプロイのたびに`$tmpdir`ごと消える一時ファイル。
- `SessionId = [u8; 16]`はクライアント側(`resume_client.rs`)・サーバー側
  (`engine/resume.rs`、`hex_lower`ヘルパー付き)双方に既にあるが、
  クライアント側の`log::info!`/`log::warn!`はこのIDを含んでいない。

### 1.3 Round 1レビューで判明した、当初案の致命的な誤り(本版での修正点)

opus-adversarial-consultの1周目で、2点の設計上の欠陥が見つかった:

**(a) 「既存loggerはそのまま、`tracing-log`のbridgeを足すだけ」は成立しない。**
`log::set_boxed_logger`はプロセス全体で1つしか勝者を持てない。
`android_logger::init_once`・`env_logger::Builder::init()`・
`tracing_log::LogTracer::init()`はすべてこの1枠を奪い合う。「bridgeを
追加する」という発想自体が、既存の`android_logger`/`env_logger`と
**排他的**であるという前提を欠いていた。正しい設計は「共存」ではなく
「global logger consumerを`LogTracer`ただ1つに統一し、logcat出力/
ファイル出力は`tracing_subscriber::Layer`として作り直す」という**置き換え**
である(§3.2で修正)。

**(b) Phase 1の計装対象が、今回のインシデントの実際の発生箇所を外していた。**
`orchestrator.rs`の`notify_network_path_changed`は、QUIC接続中
(`ConnPhase::Connected if is_quic`)の場合
`log::info!("...letting transport handle it")`と出して**何もせず終わる**
(TCP経路のみdebounceして`apply_network_lost`する分岐がある)。実際の
再接続ロジックは`resume_client.rs`の`attempt_reattach`にあり:
- `REATTACH_MAX_RETRIES = 5`、backoffは1+2+4+8秒の等比——**約15秒で
  完全に諦めて`io::ErrorKind::NotConnected`を返す**。
- QUIC自体も`android_quic_endpoint.rs`で`max_idle_timeout: Duration::
  from_secs(15)`。
- `android_quic_endpoint.rs`のコメントが明言する通り、**Androidは
  `AnyMuxEndpoint::rebinder()`を一切呼ばない**(multipathによる
  シームレスな経路差し替えは効かない構成)ため、モバイル→Wi-Fiの
  ハンドオーバーが約15秒以内に完了しなければ、この経路で確実に切断される。

`tssh`が生き延びた(と推測される)のはリトライ回数/時間の制限がより
緩いためという仮説が立つ——が、これは`resume_client.rs`を計装しない限り
ログからは検証できない。旧版のPhase 1は`resume_loop.rs`/
`engine/attach_arbiter.rs`の計装を含んでいたが、これらは**デスクトップ
CLI(`isekai-pipe connect`)/サーバー(`serve`)側のコード**であり、Android
アプリはそもそも`isekai-pipe`バイナリを経由しない(`isekai_pipe_quic_
transport.rs`はAndroidプロセス内蔵の別実装)。Phase割り当てが逆だった
ため、本版で入れ替える。

### 1.4 Round 2レビューで判明した追加の欠陥(本版での修正点)

**(c) Subscriber設置が2箇所に分裂しており、(a)と同じ「1枠しか持てない」
問題を1階層上で再現していた。** `tracing_subscriber::Registry`の
Layer構成は`tracing::subscriber::set_global_default()`実行時に確定し、
後から`Layer`を足すことはできない。Round 2版案は「`init_logger()`
(Rust内部の複数箇所から呼ばれる)がlogcat Layerだけで先に確定させ、
`init_diagnostics(log_dir)`(Kotlin初期化)がファイルLayerを後から足す」
という構成になっており、`init_logger()`が先に走ればファイルシンクが
永久に付かない(しかも無言で失敗する)。**正しい設計は、Subscriber一式
(logcat Layer + ファイルLayer)を組み立てて`set_global_default`するのは
`init_diagnostics(log_dir)`ただ1箇所に限定する**こと。

呼び出し場所は`IsekaiTerminalApplication.onCreate()`ではなく
**`MainActivity.onCreate()`**にする——`IsekaiTerminalApplication.kt`
自身のコメントが「Application は Robolectric の JVM ユニットテストでも
必ず生成されるため、ここで uniffi 経由の native 呼び出しを行うと
ホスト JVM 用のネイティブライブラリが無く `UnsatisfiedLinkError` で
テストが軒並み落ちる」と明記しており、実際に配色テーマ復元の
`setTerminalTheme`呼び出しも`MainActivity.onCreate()`まで意図的に
遅延されている(既存の前例)。`init_diagnostics`もこの前例に倣い、
`MainActivity.onCreate()`内で他のどのnative呼び出しよりも先に呼ぶ。

残る`init_logger()`呼び出し箇所(`orchestrator.rs`等、複数箇所)は、
Subscriberを設置する責務を持たず、`tracing_log::LogTracer::init()`
(`log`facadeの転送設定、こちらは`log_dir`に依存しないので`init_
diagnostics`より先に呼ばれても問題ない)のみを`Once`で冪等に行う
役割に縮小する。ただし`MainActivity.onCreate()`より前に native 呼び出しが
一切発生しないことまでは保証できないため、`LogTracer::init()`側の
`Once`が先に閉じ、`tracing`側の`set_global_default`がまだ無い間に
発行されたイベントは、`tracing`の既定(no-opディスパッチャ)により
**破棄される**——`MainActivity.onCreate()`が実質的に唯一のアプリ
起動経路であるAndroidの現状構成では許容できる限界だが、実装時に
`Once`の呼び出し順をコードコメントで明示しておくこと(§7)。

**(d) 計装対象に挙げていた`android_quic_endpoint.rs`は、計装しようが
ない純粋factoryだった。** 実コード42行で`AnyMuxFactory::noq_with_
socket_adapter`を組み立てて返すだけ——接続確立もidle timeout発火も
行わない(`max_idle_timeout: Duration::from_secs(15)`は`MuxClientConfig`
へ渡す定数にすぎない)。実際に計装すべきなのは:
- `isekai_pipe_quic_transport.rs`の`reattach_fn`(`reconnect_and_resume`
  呼び出しを包むクロージャ)——reattach試行そのものの開始/成功/失敗。
- `resume_client.rs`の`write_with_reattach`/`run_pump`の`PumpEvent::
  Failed`分岐(既存の`log::warn!("reattach: data stream ... failed")`
  呼び出し箇所そのもの)と`attempt_reattach`本体。

**(e) `noq`クレート自体が`tracing`に直接依存している**
(`Cargo.lock`で確認済み——`noq`の依存に`tracing`が並ぶ)。これは
リスクである以前に**最大の便益**でもある: グローバルSubscriberを
一度設置すれば、`noq`内部のconnection migration/path validation/idle
timeoutイベントが**無改修で**見えるようになり、今回のインシデントの
「QUIC自体がいつ・なぜidle timeoutしたか」という核心部分に、
`orchestrator.rs`側の計装より直接効く可能性が高い。ただしフィルタ
設計が伴わないと`noq`のTRACE/DEBUGだけでローテーション容量を数十秒で
使い切る(`isekai-pipe`側`connect.rs`が既定フィルタを`"warn,noq_udp=
error"`と意図的に抑制しているのも同じ理由)。§3.2にフィルタ設計を追加する。

**(f) Phase 1の機構はLinux CIから到達不能。** `init_logger()`は
`#[cfg(target_os = "android")]`とその否定形の2実装に分かれており、
非Android(CI)側は空関数。§6の「CIのユニットテストで確認する」は、
Android専用グローバルSubscriber自体の検証としては成立しない——
検証できるのはあくまで「計装コード(span/フィールドの発行)が正しい
引数で呼ばれるか」であり、テスト側が自前で`tracing::subscriber::
with_default(...)`を張って検証する形にする(§6で修正)。

## 2. 目的・非目的

**目的**: 接続/セッションのphase遷移・切断判断・reattach試行を、発生後に
本人が確実に再構成できるようにする。特に今回のインシデントの筋
(§1.3(b))を実際に再現・特定できる粒度で計装する。

**非目的**(変更なし):
- Prometheus/Grafana等の集計metricsパイプライン(§4)。
- ログの外部送信・集約基盤。
- `isekai-ssh`の`log_file.rs`の置き換え(既存の2系統(§1.2)は温存し、
  ログ内容に`SessionId`が乗るようにするだけ)。

## 3. 設計

### 3.1 相関ID

- **新しいID型は作らない**(Round 1で指摘されたYAGNI違反——独自の
  `attempt_id: u64`は不要)。既にある`SessionId`(16byte、`hex_lower`で
  小文字16進文字列)をそのまま使う。
- `SessionId`確定前(初回ハンドシェイク中の失敗等)のイベントは、
  Android側は単一プロセス・単一ライターの1本のローテーションファイルに
  時系列で書かれるため、`SessionId`が無くても直前の行との時系列関係で
  文脈が追える。新しい識別子空間を導入する必要性は薄いと判断する
  (ただし複数タブ/paneが同時に接続試行している場合、`SessionId`確定前の
  行がどのタブのものかは1本のファイルだけでは区別できない——単一
  ユーザー・少数タブの個人用途では致命的ではないと判断するが、根拠として
  過大に主張しない)。
- デスクトップ経路(`isekai-ssh`)には既に`ISEKAI_INTENT_ID`
  (`wrapper.rs`、接続試行ごとに発行)という同種の識別子が既にあるが、
  これはAndroidアプリの経路には存在しない・必要ない(Androidは
  `isekai-ssh`バイナリを経由しない)ので、混同せず別物のまま扱う。

### 3.2 tracing導入方式(Round 2指摘を受けて再修正): Subscriber設置は1箇所に一本化

**Phase 1のスコープを`rust-core/src`(cdylib、Android専用)に限定する。**

1. **Subscriber(Registry + Layer群)の組み立てと`tracing::subscriber::
   set_global_default()`は、新設`init_diagnostics(log_dir: String)`
   (UniFFI export)ただ1箇所でのみ行う**。`std::sync::Once`でガードし、
   2回目以降の呼び出しはno-op。中身は: logcat Layer(下記2) + ファイル
   Layer(§3.3) + `EnvFilter`(下記5)を`Registry`に載せて確定させる。
2. logcatへの出力は、`android_logger`が内部で使っているのと同じ
   `android_log-sys`(あるいは`paranoid-android`等の既存crate)を直接叩く
   `tracing_subscriber::Layer`を新規に書き、`log`facadeを経由せず直接
   tracingのeventから出力する(`android_logger::init_once`自体は
   もう呼ばない——`log::set_boxed_logger`の枠を奪い合わないため)。
3. 既存の`init_logger()`呼び出し箇所(`orchestrator.rs`等、複数箇所)は
   **Subscriber設置の責務を持たず**、`tracing_log::LogTracer::init()`
   (`log`facadeの転送設定。`log_dir`に依存しないので`init_diagnostics`
   より先に呼ばれても問題ない)のみを別の`Once`で冪等に行う役割に縮小
   する。呼び出し順の詳細は§1.4(c)。
4. これにより`log::info!`/`log::warn!`(既存呼び出し、無改修)は
   `LogTracer`経由で`init_diagnostics`が設置したSubscriber配下の
   両Layerに届く。
5. **`EnvFilter`の既定値をこのADRで決める**(§1.4(e)、Round 2指摘):
   `"info,isekai_terminal_core=debug,noq=debug,noq_udp=warn"`を初期値と
   する——`noq`(接続レベルのmigration/idle timeout等、価値が高い)は
   debugまで、`noq_udp`(パケットレベル、`isekai-pipe`側`connect.rs`が
   `"warn,noq_udp=error"`で意図的に抑制しているのと同じ理由)はwarnに
   留める。§3.3のローテーション保持日数は、この既定フィルタでの実測を
   見てから最終調整する(§7)。
6. `session_id`/`ConnPhase`等をspanフィールドに載せる`tracing::instrument`/
   `info_span!`は、次の(Round 2で訂正した)箇所に絞って新規追加する:
   - `isekai_pipe_quic_transport.rs`の`reattach_fn`(`reconnect_and_
     resume`呼び出しを包むクロージャ——reattach試行の開始/成功/失敗)。
   - `resume_client.rs`の`attempt_reattach`・`write_with_reattach`・
     `run_pump`の`PumpEvent::Failed`分岐(既存の`log::warn!("reattach:
     ...")`呼び出し箇所そのものに`session_id`/試行回数/backoff秒数を
     フィールドとして追加)。
   - `orchestrator.rs`の`notify_network_path_changed`・
     `notify_did_enter_background`・`notify_background_budget_expired`・
     `notify_will_enter_foreground`(下記7でwall-clock時刻も付与)。
   (`android_quic_endpoint.rs`は§1.4(d)の通り計装対象から除外——
   計装しようのない純粋factoryのため。)
7. **wall-clock時刻の付与**: `tracing`のspan所要時間は既定でmonotonic
   instant基準のため、夜間・バックグラウンド遷移を翌朝人間が読んで
   実時刻と突き合わせられる形でファイルLayerに出す必要がある。
   `tracing_subscriber::fmt`系Layerは既定で行頭に壁時計タイムスタンプを
   出すため、まずそれで足りるかを実装時に確認し、不足する場合のみ
   各イベントへ`SystemTime::now()`を明示フィールドとして追加する
   (Round 3レビュー指摘——両方載せると冗長になりうる)。

`isekai-pipe`(`connect`/`serve`)の`env_logger`ベースの`log`エコシステム
(§1.2)は、**Phase 1では一切触らない**。Androidアプリの経路
(`resume_client.rs`/`isekai_pipe_quic_transport.rs`)はこのバイナリを
通らないため、今回のインシデント究明に対しては不要な変更であり、
`env_logger`のTTY向け`\r\x1b[K`書式維持等の別の複雑さ(§7)を今回の
スコープに持ち込まない。

### 3.3 Androidシンク

- `tracing-appender`のローテーションは`Rotation::{MINUTELY,HOURLY,DAILY,
  NEVER}`という**時間基準のみ**で、バイトサイズ閾値は存在しない
  (Round 3レビュー指摘——当初「size-basedで1MiB×5世代」としていたのは
  このcrateのAPIに実在しない機能だった)。`DAILY` +
  `max_log_files(3〜7)`(§3.2-5の`EnvFilter`実測後に世代数を最終調整)を
  既定にする——今回のインシデントが「夜間発生・翌朝発覚」だったことを
  踏まえ、保持期間が短くなりすぎない方(`HOURLY`ではなく`DAILY`)を選ぶ。
  Androidアプリのprivate storage(`context.filesDir`配下、Kotlinから
  渡す)に書く`Layer`にする(§3.2-1)。
- UniFFIの新規追加は**`init_diagnostics(log_dir: String)`1本のみ**に絞る
  (Round 1指摘: 当初案にあった`export_diagnostic_log() -> String`は、
  最大5MiB相当のログ本文をJNI/UniFFI境界越しにコピーする設計で、
  固定パスが分かっていれば不要。Kotlin側は`context.filesDir`から
  既知の相対パスを直接読み、`FileProvider`経由の`Intent.ACTION_SEND`で
  共有すればよい——Rust側に専用exportメソッドは要らない)。
- **呼び出し箇所は`MainActivity.onCreate()`の先頭、既存の
  `theme.applyTo(::setTerminalTheme)`より前**(§1.4(c))。
- android_logger経由のlogcat出力は、§3.2の置き換え後もLayerとして
  等価に維持する(実機デバッグ時のライブ観測が失われないようにする)。

### 3.4 `isekai-pipe serve`シンク(Phase 2)

- 永続パスは新設せず、`isekai-pipe-core::profile::resolve_state_path`/
  `default_log_file`と同じ仕組みを`serve`起動時にも適用し、
  `ISEKAI_PIPE_LOG_FILE`と対になる形の既定state dirパスへ
  `tracing-appender`のローテーションで書く(Round 1指摘の反映——
  新規パス規約を作らず既存の解決ロジックに乗せる)。
- Phase 2で`isekai-pipe`側も§3.2と同じ「`LogTracer`への一本化」を行う
  (`env_logger`の`Target::Pipe`/`Target::Stderr`分岐、TTY時の
  `\r\x1b[K`書式維持を`tracing_subscriber::Layer`側で再現する必要が
  あり、これはPhase 1のAndroid側置き換えより実装コストが高い——
  Phase 2着手時に改めて詳細設計する)。
- `SessionId`を`tracing::info_span!`のフィールドに持たせ、
  `always-connects.md`が名指しする「サーバー側状態リーク」
  (`AttachArbiter`のfencing slot未解放等)を、`relay_ended`呼び出し漏れの
  有無としてログから機械的に確認できるようにする。

### 3.5 `isekai-ssh`(Phase 3、変更ほぼ無し)

- 既存の2系統(§1.2)はどちらも温存する。`isekai-pipe connect`側が
  Phase 2でtracing化されれば、`SessionId`はどちらの経路にも自動的に
  乗る(`wrapper.rs`自体の改修は不要)。
- `wrapper.rs`自身の`log_line_verbose!`呼び出しには、既存の
  `ISEKAI_INTENT_ID`を引き続きそのまま使う(§3.1、新規統一はしない)。

### 3.6 ログに載せてよい情報の線引き

(変更なし)`isekai-pipe inspect --redact`の既存の線引きに合わせる:
常時可は`session_id`/`ConnPhase`/transport種別/ネットワークpath種別
(wifi/cellular等、SSIDや生IPは含めない)/エラー種別(enum)。既定で
隠すのはホスト名/ユーザー名/鍵fingerprint/生IPアドレス。

## 4. metricsについて

(変更なし、Round 1で妥当と確認済み)単一ユーザー・単一デバイスの
個人用途でPrometheus等の集計基盤の受益者がいないため見送り。
"give-up回数"のようなカウンタが必要になれば、当面`tracing`イベントの
grepで代替する。

## 5. フェーズ分割(Round 1指摘を受けて入れ替え)

1. **Phase 1(今回のインシデントに直接効く、最優先)**: `rust-core/src`
   ——`init_diagnostics`への一本化Subscriber設置(logcat/ファイルの
   2 Layer + `EnvFilter`)、`isekai_pipe_quic_transport.rs`の
   `reattach_fn`・`resume_client.rs`(`attempt_reattach`/
   `write_with_reattach`/`run_pump`)・`orchestrator.rs`の`notify_*`群への
   計装(wall-clock時刻付き)、UniFFI `init_diagnostics`、
   `MainActivity.onCreate()`での起動時配線。
2. **Phase 2**: `isekai-pipe`(`connect`/`serve`)——同様の`LogTracer`一本化
   (`env_logger`のTTY書式維持を含む)、永続シンク(既存`resolve_state_
   path`流用)、`AttachArbiter`/session table計装。
3. **Phase 3(優先度低)**: `isekai-ssh`側は基本無改修。相関IDが自動的に
   乗ることの確認のみ。

## 6. 検証方法

`prefer-gh-actions-over-local-cargo`によりローカルbuild/testは行わない。

- **Rust側(GitHub Actions経由の`cargo test -p isekai-terminal-core
  --lib`)**: Phase 1の`init_diagnostics`/Android専用Subscriber設置自体は
  `#[cfg(target_os = "android")]`でLinux CIから到達不能(§1.4(f))なので、
  CIで検証できるのは**計装コード(span/フィールドの発行)そのもの**に
  限る——テスト側が自前で`tracing::subscriber::with_default(...)`
  (テスト用の`Collect`/`Layer`でイベントを`Vec`に集める)を張り、
  「`attempt_reattach`が`REATTACH_MAX_RETRIES`に達して`NotConnected`を
  返すまでの一連のイベントが、同一`session_id`のspan配下に記録される」
  ことをこのテスト内Subscriberに対して検証する。Android専用の
  Subscriber設置・ローテーション自体の検証ではないことを明記しておく。
- **実機検証**: `android-ci-deploy`スキルでビルド・実機インストール後、
  Wi-Fi⇔モバイル回線の手動切り替えでローミングを再現し、Androidの
  ローテーションファイルに一連のphase遷移(特に`attempt_reattach`の
  試行回数とtimeoutまでの秒数)が残ることと、フィルタ設定
  (§3.2-5)がログ量を実用的な範囲に抑えていることの両方を目視確認する。

## 7. Open Questions

- Androidのローテーション世代数(`DAILY` + `max_log_files`、暫定3〜7、
  §3.3)は、§3.2-5の既定`EnvFilter`(`noq=debug`含む)での実測を経て
  最終決定する。
- `tracing-appender`の`non_blocking`書き込みを使うか、同期書き込みで
  十分か。
- logcat向け`Layer`の実装(§3.2-2、`android_log-sys`直叩き vs 既存crate
  流用)の選定。
- `MainActivity.onCreate()`より前に(将来的な変更で)native呼び出しが
  発生するようになった場合、`LogTracer`の`Once`は先に閉じるが
  `tracing`側の`set_global_default`はまだ無く、その間のイベントは
  破棄される(§1.4(c))。実装時にこの順序依存をコードコメントで明示し、
  将来の変更でこの前提が壊れないようにする。
- `noq`自体が`tracing`に依存している(§1.4(e))ことによる
  `tracing_log`との相互作用(再帰防止ガードが実際に効くか)を、
  実装後にログ出力を目視して確認する。
- Phase 2で`env_logger`のTTY向け`\r\x1b[K`書式(進行中の再描画を壊さない
  ための挙動)を`tracing_subscriber::Layer`側でどう再現するか——
  Phase 2着手時に別途設計する。
