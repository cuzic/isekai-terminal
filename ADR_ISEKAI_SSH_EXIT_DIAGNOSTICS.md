# ADR: isekai-ssh(Windows native)の異常終了理由が診断ログに一切残らない

- **Status**: **Approved**(2026-09-12起草・同日Approved。
  opus-adversarial-consult 3ラウンドで収束。Round 1で
  §1.3の因果関係の主張の一部[「VERBOSEへの書き手はパニックフックだけ」]が
  誤りと判明し、より正確な診断[「holderでは`log_line!`が`NUL`へ消える」]
  へ差し替え。合わせて論点2・3・4の再構成、新代案[PR #116のper-holder
  ファイルへの相乗り]の追加、却下する代案[`RUST_LOG`一発トグル・
  `--isekai-log-file`デフォルト化]の明記、`ADR_ISEKAI_SSH_LOCAL_
  SCROLLBACK.md`との重複整理を反映。Round 2で2件のMUST-FIX
  [`--isekai-log-file`が実は今日のビルドで既にholder/孫プロセスまで
  伝播済みという事実の欠落、ステップ0が孫プロセスのログと自プロセスの
  終了理由を混同していた点]と5件の軽微な訂正[`log_line_verbose!`の
  正確な箇所数・holderの回復ロジックが「相当」ではなく同一関数チェーン・
  仮説の優先順位・§3.3のUnix記述誤り・§3.4の相互参照切れ]を反映。
  Round 3で新規指摘なしを確認し収束)
- **対象**(見込み、要精査): `rust-core/isekai-ssh/src/log_file.rs`、
  `rust-core/isekai-ssh/src/native/mux/holder.rs`(`DetachedProcessSpawner`)、
  `rust-core/isekai-ssh/src/native/child_stdio.rs`(`resolve_pipe_log_file`、
  PR #116のper-holderログ命名を再利用する前提)、`rust-core/isekai-pipe/
  src/connect.rs`(`RotatingLogFile`、`isekai-pipe-core`への移設候補)、
  `rust-core/isekai-ssh/src/wrapper.rs`(`bootstrap_and_register`・
  `init_logging`)、`rust-core/isekai-ssh/src/native/connect.rs`
  (`drive_connect_recovery`・`rebootstrap_and_rebuild_intent`)
- **入力**: ユーザーとの相談セッション(2026-09-12)。「isekai-ssh(Windows)の
  セッションの多くが停止していてsshの再実行が必要だった、比較対象の
  `tssh`は再接続不要だった」という報告 → 直近のPR #115(常時接続原則
  違反=再デプロイ後リトライ1回きりの修正)・PR #116(Windowsネイティブ
  経路の`isekai-pipe connect`子プロセスのログ欠落修正)を確認 →
  ユーザーから「停止するときに何が起きているか、断末魔の状態をあとから
  送れるといい」という要望 → clipwire実機調査 → コード調査 →
  `opus-adversarial-consult`(Round 1、general-purpose/opus)による
  独立検証
- **拘束される既存ルール**: `.claude/rules/always-connects.md`(本ADRが
  補強する対象)、`ADR_ISEKAI_SSH_OBSERVABILITY.md`(Approved・実装済み
  [PR #116]。前例として「Windowsネイティブ経路はinit_verboseを呼ばない」
  という誤診断がRound 1レビューで訂正された経緯があり、本ADRも同じ
  誤りを一度犯した[後述§1.3]ことを踏まえる)、`ADR_ISEKAI_SSH_LOCAL_
  SCROLLBACK.md`(Draft・未実装。§3.6でスコープの切り分けを決定)

---

## 1. 背景

### 1.1 経緯

`ADR_MIDSESSION_DISCONNECT_RECOVERY.md`(Epic R)・PR #115(2026-09-07)により
「再デプロイ後の再接続を1回きりで諦めてプロセスが終了する」という
常時接続原則違反は修正済みで、ユーザーは実際に最新ビルド
(`isekai-ssh 0.1.0 (a266f1f387ae)`、バイナリの`LastWriteTime`は
2026-09-07 12:16:36 — clipwireで実機確認済み)を使用中。それにも
かかわらず「ほとんどのセッションが停止していてsshの再実行が必要だった」
と報告された。「停止」の実態はハングでも無音の新セッション化でもなく
**プロセスが終了していた**(要ssh再実行)——ユーザー自身の回答による。

**注意**: この実機確認は2026-09-12時点のもので、PR #116(`0df74233`、
同日マージ)より**前**のビルドに対するものである。§4のステップ0a/0bで、
#116込みビルドでの再取得(0a)と`--isekai-log-file`での即時取得(0b)を
最優先とする。

### 1.2 clipwireでの実機調査で判明した事実

- `%LOCALAPPDATA%\isekai\logs\isekai-ssh.log`(isekai-ssh自身の「常時オン
  診断ログ」の既定パス、`isekai_pipe_core::default_log_file()`/
  `log_file.rs`)の最終更新日時は**2026-07-20のまま、7005バイトで
  変化がない**。a266f1f3ビルド(2026-09-07〜)を5日間実運用し、その間に
  今回報告されたような異常終了が複数回起きているはずにもかかわらず、
  一切追記されていない。
- `ISEKAI_PIPE_LOG_FILE`環境変数はプロセス/ユーザー/マシンいずれの
  スコープにもオーバーライドが存在しない(`default_log_file()`は素直に
  `%LOCALAPPDATA%\isekai\logs\isekai-ssh.log`へ解決するはず)。

### 1.3 コード調査で判明した根本原因(Round 1レビューで訂正済み)

(`ADR_ISEKAI_SSH_OBSERVABILITY.md`のRound 1レビューが一度「Windows
ネイティブ経路は`init_verbose`を呼んでいない」という誤診断をした前例が
あり、当初の本ADRも形を変えて同じ轍を踏んだ。以下は`opus-adversarial-
consult`による独立検証を経て訂正済みの内容。)

**検証の結果、正しいと確認できた事実**:

- `log_file::init_verbose(default_log_file())`は、`wrapper::init_logging`
  という共有ヘルパー(`wrapper.rs:499`、定義はここのみ)経由で、client
  初回spawn(`native/connect.rs:193`の`prepare_with_tofu`)・client
  attach(`native/mux/mod.rs:277`の`connect::prepare`)・holder re-exec
  entrypoint(`native/mux/mod.rs:501`の`run_as_holder_entrypoint`)の
  **全ての**native経路で確実に呼ばれている。「一度も初期化されない」
  という仮説は誤り。
- `src/native/`配下の診断出力は実測54箇所すべて`log_file::log_line!`
  マクロ経由(`log_line_verbose!`の呼び出しはnative配下に0箇所——唯一の
  ヒットは`native/connect.rs:189`のdocコメント内の言及であり呼び出しでは
  ない)。`log_line!`は`--isekai-log-file`が明示指定されていない限り常に
  `eprintln!`へフォールバックする設計(`log_file.rs::dispatch`)——
  つまり(foregroundプロセスなら)ターミナル画面にしか出力されない。
- ログパスの変更・5MB切り詰め(`VERBOSE_LOG_MAX_BYTES`、`truncate_over`)
  という対抗仮説はいずれも棄却済み(パス解決ロジックは変わっておらず、
  7005バイトは閾値を遥かに下回る)。

**当初誤っていた主張とその訂正**:

当初「VERBOSE_LOG_FILEへの書き手はRustパニック時のパニックフック
(`main.rs::install_panic_hook`)のフォールバック分岐だけ」と主張したが、
これは**誤り**。`wrapper::bootstrap_and_register`(`wrapper.rs:1424`〜、
再デプロイ/初回デプロイ処理)本体だけで`log_line_verbose!`を**10箇所**
(`1560/1563/1603/1604/1606/1608/1609/1610/1631/1649`——`1560/1563`が
`TofuConfirmation::AlwaysPrompt`/`Silent`それぞれの開始ログ、
`1603〜1610`がhost/relay/identity/sha256の表示、ほか)で呼んでおり、
同関数からのみ呼ばれる`resolve_stun_servers`にも3箇所(`1413/1415/
1416`)ある。しかもWindows nativeパスから**2経路**で到達する:

- `native/connect.rs:212`(`prepare_with_tofu`)→
  `wrapper::build_intent_or_bootstrap` → `bootstrap_and_register`
  (初回bootstrap、新規宛先の場合)
- `native/connect.rs:620`(`NativeConnectOps::rebootstrap_and_rebuild_
  intent`)→ `bootstrap_and_register(..., TofuConfirmation::Silent)`
  (サイレント再デプロイ、`drive_connect_recovery`が`RedeployGate`を
  開けた場合)

`wrapper.rs:1560-1567`の開始ログは、実際のSSHダイヤルより前、**無条件に**
発火する。したがって「2026-07-20から`isekai-ssh.log`が更新されていない」
という事実は、「その日以来パニックが起きていない」よりずっと強いことを
証明している: **その期間中、サイレント再デプロイ(`RebootstrapAndRetry`
アーム)が一度も実行されなかった**。第一仮説は`drive_connect_recovery`
が`ConnectFailureRecoveryAction::NoRecoverableSignal`(`native/
connect.rs:437`、`return Err(first_error)` — 再デプロイ判断に到達する
前に諦めている経路)に入っていたケース。`run_with_reconnect`の
`RECONNECT_BUDGET`(`mux/mod.rs:122`、24時間)枯渇は、数時間で終了した
セッションには当てはまりにくいため優先度は低い(§4ステップ0で
`ConnectOutcome`ファイルの有無を確認すれば切り分けられる)。

いずれの場合も、**PR #115が実装したRedeployGate/retry-until-recovered
の機構そのものが発動していない**可能性が高い。§2の「常時接続できない
問題はPR #115で対処済み」という前提は、この時点で収集した証拠だけでは
支持されない——これ自体が再現・再調査すべき独立した論点である
(§4ステップ0)。

**実際のギャップの再定義**:

「パニック以外は永続化しない」という当初の診断ではなく、より正確には
**「holderプロセスでは`log_line!`のターミナルフォールバックが`NUL`へ
消える」**が実際のギャップ。`native/mux/holder.rs`の
`DetachedProcessSpawner::spawn`は`.stdout(Stdio::null()).stderr
(Stdio::null())`でholderを起動する(確認済み)。holderは`always-connects`
原則の回復ロジックを**文字通り同一の関数チェーン**で実際に走らせる当事者
——「相当」の別系統ではない: `run_as_holder_entrypoint`(`mux/mod.rs:474`)
→ `prepare_with_tofu` → `run_as_holder`(`mux/mod.rs:677`)→
`connect::run_prepared` → `run_native_connect_with_recovery`
(`native/connect.rs:308`)→ `drive_connect_recovery`。その`log_line!`
出力(=通常の異常終了理由の大半)は、foregroundの一時的なクライアントと
違って**そもそも表示される先すら無い**。PR #116が`isekai-pipe connect`
という孫プロセスに対して直した穴(`ISEKAI_PIPE_LOG_FILE`未伝播)と
同じ形の穴が、isekai-ssh自身の`log_line!`出力については依然として
holderに残っている。

### 1.4 重要な事実: `--isekai-log-file`は今日のビルドで既に全プロセスへ伝播する(Round 2で発見)

コード変更ゼロで、以下が**既に動く**:

- holder re-exec: `mux/mod.rs:302`が元のargv(`--isekai-log-file`を
  含む)を`dispatch`の`holder_args`として保持し、`holder.rs:112-114`の
  `DetachedProcessSpawner::spawn`が`command.args(args)`でそのargvを
  そのままholderへ再exec する。holder自身の`init_logging`が同じ
  `--isekai-log-file`を解決して`log_file::init`を呼ぶため、
  `is_enabled()`が真になり、holderの54箇所の`log_line!`は
  `eprintln!`(=`NUL`)ではなく指定ファイルへ書き込まれる。
- 孫プロセス(`isekai-pipe connect`): `child_stdio.rs`の
  `resolve_pipe_log_file`は明示指定を最優先で採用する。

したがって、`isekai-ssh --isekai-log-file C:\...\dbg.log <host>`と
明示指定するだけで、**client + holder + 孫プロセスの死ぬまでの記録を
今日のビルドのまま丸ごと取得できる**。本ADRの実質的なギャップは
「機構が存在しない」ではなく「**既定でオンになっていない、かつholderは
明示指定しない限り既定のシンクを一切持たない**」に縮む。§3.5で
`--isekai-log-file`のデフォルト化自体は却下しているが(端末出力の消失・
無制限成長する単一ファイルへの集約)、明示指定した場合に実際に機能する
という事実は本ADRの前提として重要——§4のステップ0bはこれを使う。

## 2. 狙い

ユーザーが後から(clipwire等で)取得して送れる、Windows上でのisekai-ssh
(特にholderプロセス)の異常終了理由の永続的な記録を用意する。ただし
§1.3の訂正により、「常時接続できない」問題自体がPR #115で本当に
対処されているかは未確認のまま残っている——本ADRの直接のゴールは
診断ログの永続化であり、常時接続そのものの再修正は§4ステップ0の
再現結果次第で別ADR/PRとして切り出す。

## 3. 検討すべき論点

### 3.1 スコープ

「終了理由」を最後の1行だけ残せば足りるか(exit直前のメッセージを
`log_line_verbose!`にも複製するのが最小差分)、それとも直前の接続試行の
遷移列(`RedeployGate`の状態・`consecutive_timeouts`・どの
`BootstrapFailure`バリアントだったか等)まで残すか。§3.4の代案
(per-holderファイルへの相乗り)を採るなら、コストが下がる分だけ
後者に寄せてよい。

### 3.2 `log_line!`複製案 — 反対根拠を訂正

当初「対話的メッセージ(パスフレーズプロンプト等)を含む全54箇所が
無差別に残る」「ログ量が肥大化する」を反対理由に挙げたが、**両方とも
Round 1レビューで事実誤認・根拠薄弱と指摘された**:

- パスフレーズの入力プロンプトは`log_line!`ではなく
  `native/connect.rs`の`prompt_passphrase`クロージャ引数
  (`connect.rs:1201`)、TOFUの`[y/N]`も`log_line!`ではなく生の`eprint!`
  (`wrapper.rs:1595`)。54箇所に対話的プロンプトは実際には含まれない。
- per-byte/per-frame規模の出力サイトは54箇所中に無く、最頻でも
  再接続ストーム(バックオフで自然に上限)・セッションteardown(数行)・
  accept ループ(接続あたり1行)程度で、量的な肥大化懸念も弱い。

**実際に注意すべきは以下の2点**:

1. **パス情報の混入**: `connect.rs:1099`(`identity_file`候補リスト+
   `home`パス)、`:1133`(`trying key <path>`)、`:1219`、
   `private_key.rs:69/129`、`handoff.rs:110/118`など、鍵の中身では
   ないがユーザー名を含みうるファイルパスが複数箇所で出力される。
   clipwireで自分宛に送る分には許容範囲と考えられるが、明記が必要。
2. **跨プロセス書き込み競合**(§3.3で詳述)。

複製の可否は「foreground/holderで挙動を分ける」ことで大部分解消できる
(foregroundは現状通りterminal-onlyのままでよく、パス情報の露出量も
増えない。増えるのはholder経由の出力のみ)。

### 3.3 跨プロセス競合 — 論点4は論点2の前提条件

`log_line!`をどこかの共有シンクへ複製する設計を採る場合、以下が
**設計の前提条件**として先に解決されていなければならない(独立した
別論点ではない):

- **複数プロセスが1ファイルにappendする**: `Sink`(`log_file.rs:61`)の
  直列化はプロセスローカルな`Mutex`のみ。`default_log_file()`は
  clientでもholderでも同じパスに解決される(`ADR_ISEKAI_SSH_
  OBSERVABILITY.md`で確認済み)ため、素朴に複製するとclient×複数タブ+
  holder×複数タブが無調整で同一ファイルにappendし、どの行をどの
  プロセスが書いたか区別できない。
- **`truncate_over`の跨プロセス破綻**: `Sink::open`(`log_file.rs:85-89`)
  はopen時に5MB超なら`remove_file`する設計。holderがファイルハンドルを
  保持したまま稼働中に、新しいforegroundクライアントが起動して閾値を
  踏むと:
  - Windows(holderがファイルハンドルを保持するケース): 開いている
    ファイルの`remove_file`は`ERROR_SHARING_VIOLATION`で失敗し、
    `let _ =`(`log_file.rs:87`)が握り潰す →
    **ファイルが無制限に成長し続ける**。
  - Unix(`native/`はWindows専用でholder自体が存在しないため、こちらは
    holderではなく**同じ宛先へ複数のターミナルから同時に`isekai-ssh`を
    起動した場合の、foreground同士の競合**): 先に起動していた
    プロセスがファイルハンドルを保持したまま、後から起動したプロセスが
    閾値を踏んでunlinkに成功すると、先行プロセスはorphan inodeに
    書き続ける → **その行が以後すべて消える**(readerからは見えなく
    なる)。

  これは「`log_line!`を複数プロセスから同一ファイルに書かせた瞬間に
  初めて到達可能になる」バグで、現状は実質的な書き手がいないため
  顕在化していないだけ。ローテーション方式の再設計(旧論点4)は、
  複製案を実装する前提条件として先に解決する。

### 3.4 代案: PR #116のper-holderファイルへの相乗り

PR #116が既に用意した資産に相乗りする方が、上記3.3の競合を構造的に
回避できる:

- `native/child_stdio.rs`(`resolve_pipe_log_file`、`:162-169`)が
  `channel_name`のdigestから`isekai-ssh-holder-<hex>.log`を決定的に
  導出済み(宛先ごとに分離、複数タブの衝突を回避する既存設計)。
- `isekai-pipe/src/connect.rs`の`RotatingLogFile`(5MB到達で`.1`へ
  rename する実行時ローテーション)。

holderの`log_line!`フォールバック先をこの系統に向ければ、§3.3
(ローテーション)が同時に解決し、宛先ごとの分離により競合も緩和
される。ただし**同一ファイルをisekai-ssh本体の`Sink`(ハンドル保持)と
`RotatingLogFile`(rename)で共有するのは不可**(孫プロセスがrename
した後、isekai-sshはrename済みの古いinode/ハンドルに書き続けてしまう)。
現実的な形は次のいずれか:

1. `isekai-ssh-holder-<hex>-ssh.log`のように**別ファイル名**にし、
   `RotatingLogFile`を`isekai-pipe`から`isekai-pipe-core`へ移設して
   共用する(`isekai-ssh`は既に`isekai-pipe-core`に依存済みなので
   追加の依存コストは無い)。
2. isekai-ssh側は毎回open→append→closeにする(この頻度・行数なら
   性能上無視できる。ローテーションは`isekai-pipe connect`側の
   `RotatingLogFile`に任せる)。

1の「`RotatingLogFile`の`isekai-pipe-core`への移設」を推奨候補とする。

**取得手段(この案を採る場合の必須の付随項目)**: `isekai-ssh-holder-
<hex>.log`は`channel_name`のdigest命名のため、ユーザーは自分のどの
タブ/宛先がどの`<hex>`に対応するか分からない(タブが複数あれば尚更)。
`isekai-ssh doctor <host>`に、解決済みの`default_log_file()`パスと、
同ディレクトリに存在する`isekai-ssh-holder-*.log`(および3.4案1の
`-ssh.log`)の一覧を表示する機能を追加することを、この案の実装と
不可分な付随項目とする——単一の`isekai-ssh.log`だった頃より取得性が
悪化しないようにするための必須条件。

### 3.5 却下する代案(検討済み・不採用として明記)

- **`RUST_LOG`一発トグル**: `main.rs:206`の
  `env_logger::Builder::from_default_env().target(Target::Stderr)`により
  **既に実装されている**が、この問題には効かない。理由は独立に2つ:
  (1) `isekai-ssh`内の`log::`マクロ呼び出しは全9箇所、うち8箇所が
  `helper_download.rs`、1箇所が`ctl_forward.rs:245`で、接続/回復パス
  (`native/connect.rs`・`mux/`)には1つも無い。
  (2) 出力先がstderr固定で、holderのstderrは`Stdio::null()`
  (`holder.rs:117`)——`RUST_LOG=trace`を付けてもholderでは全消滅する。
  新しい`ISEKAI_SSH_LOG=1`的な環境変数を追加しても、シンク(holderの
  出力先)自体を直さない限り同じ無音を再生産するだけであり、代案には
  ならない。
- **`--isekai-log-file`のデフォルト有効化**: (a) `log_line!`のterminal
  フォールバックが無くなり、対話的でない通常の端末出力(ステータス表示
  等)が全部消える(`log_file.rs`の意図的な役割分離、§3.2参照)。
  (b) `init`は`truncate_over: None`のappend-forever(`log_file.rs:182`)
  で、PR #116以降は明示指定が孫プロセスにも優先適用される
  (`child_stdio.rs`)ため、client+holder+孫が無制限成長する単一
  ファイルへ集約されてしまう。いずれも不採用。

### 3.6 `ADR_ISEKAI_SSH_LOCAL_SCROLLBACK.md`との重複整理(決定事項)

衝突点は1つ: 同ADR論点3の選択肢「ローカルファイルへ継続的にログ出力
する」が素直に実装されると、リモート端末出力(scrollback)が「clipwire
で送ってよい」と分類されたファイル(本ADRの診断ログ)に混ざり込み、
同ADR論点4が自ら懸念している機密性の問題を引き起こす。以下を決定
事項として両ADRに反映する:

- scrollbackリングはメモリのみ、または本ADRの診断ログとは明確に
  別名の別ファイルにする。`default_log_file()`/`isekai-ssh-holder-*.log`
  (系統)には流さない。
- 分類基準: **診断ログ=送ってよい(パス情報程度) / scrollback=
  送ってはいけない(端末出力そのもの)**。
- 両ADRは根本原因が同じ構造を持つ: PTYバイトを中継しているのはholder
  (`owner.rs`のrelayループ)であり、clientは`OwnerLost`
  (`mux/mod.rs:349`)を跨いで出入りする。**scrollbackもholderに
  置かないと切断を跨いで保持できない**——これは本ADR自身が診断ログに
  ついて到達した結論(§3.4、holder側で解決する)と同一の構造。
  `ADR_ISEKAI_SSH_LOCAL_SCROLLBACK.md`論点1(「Windows Terminal自身の
  scrollbackで足りるのでは」)への部分的な答えにもなる:
  PR #115が「Windows Terminalクラッシュ風終了」と呼んだ現象そのものが、
  タブ(=Windows Terminal自身のscrollback)ごと消える異常終了であり、
  Windows Terminal側では原理的に救えない。

## 4. 次のステップ(優先順位付き)

**ステップ0a(トランスポート側、要#116込みビルド)**: PR #116込みの
最新ビルドをWindowsへ配布し、再度異常終了を再現させて
`isekai-ssh-holder-<hex>.log`(§1.1で確認した`isekai-ssh.log`ではなく、
PR #116が新設したholder専用ログ)を確認する。ただし**これが書くのは
孫プロセス`isekai-pipe connect`のQUIC/STUN/resume診断であって、
isekai-ssh自身の`log_line!`出力ではない**——ユーザーが本当に知りたい
「なぜisekai-sshが終了したか」(`drive_connect_recovery`の判断・
`ConnectOutcome`の有無)はここには出ない。「holderログに特に何も無い
→ 問題は無かった」と誤結論しないよう注意する。

**ステップ0b(isekai-ssh自身の終了理由、今すぐ現行ビルドで実行可能)**:
§1.4の事実を使い、`isekai-ssh --isekai-log-file <path> <host>`で次回
以降のセッションを起動してもらい、次に異常終了した際そのファイルを
clipwire等で取得する。コード変更が一切不要で、本ADRの当面の実用上の
ゴール(「断末魔の状態を後から送る」)をこの時点で部分的に満たせる
——ただし常時オンではなく、ユーザーが毎回明示指定する必要がある点が
§2で狙う「既定で残る」状態との差分として残る。なお、このファイルには
client/holder/孫プロセスの3者の行がプロセス識別子なしで混在する
(`init`は`truncate_over: None`のため削除競合は起きず、単純に混ざる
だけ)——依頼時にその旨を伝えておくと読み違いを防げる。

§1.2のclipwire証拠はPR #116より前のビルド時点のものであり、本ADRの
設計スコープ自体がステップ0a/0bの結果次第で縮む可能性がある。

**設計**: ステップ0a/0bの結果を踏まえ、§3.4(per-holderファイルへの
相乗り、`RotatingLogFile`の`isekai-pipe-core`移設、`doctor`への
取得手段追加)を軸に設計し、`opus-adversarial-consult`のRound 3確認
(必要であれば)へ進む。

**独立した最小の保険**(上記と並行して着手可能): `main.rs:243`の
`log_file::log_line!("{err:?}")`(wrapper-modeのトップレベルエラー
ハンドラ)をVERBOSEにも複製する。`anyhow`のcontextチェーン
(`native/connect.rs:624`等)ごと残り、holder経由の失敗も同じfunnelを
通る安価な変更。ただし`Ok(nonzero)`終了やunwindingを伴わないkillは
カバーしない点は限界として明記する。
