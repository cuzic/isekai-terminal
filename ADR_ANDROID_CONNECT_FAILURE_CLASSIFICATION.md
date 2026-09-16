# ADR: 接続失敗の分類に基づく体系的リトライ(「常に接続できる」原則の核をAndroidへ移植)

- **Status**: Draft(2026-09-16起草。Windows `isekai-ssh` との接続安定化
  ギャップ分析セッションから派生。実装着手前にレビュー要——本ADRは
  `opus-adversarial-consult`の対象には含めていない(ユーザー指定))
- **対象**(見込み、要精査): `rust-core/src/orchestrator.rs`
  (`spawn_reconnect_loop`・`connect_via`)、`rust-core/src/helper_bootstrap.rs`、
  各Transport(`isekai_pipe_quic_transport.rs`・`multipath_transport.rs`・
  `isekai_stun_p2p_transport.rs`)のエラー型
- **入力**: `ADR_ANDROID_RECONNECT_TIMEOUT.md`と同一のセッション。
  `ISEKAI_PIPE_DESIGN.md` Epic N-2(「常に接続できる」原則への拡張)を
  直接の参照元とする
- **拘束される既存ルール**: `.claude/rules/rust-ssot.md`、
  `.claude/rules/always-connects.md`

---

## 1. 背景

Windows(isekai-ssh)はEpic N-2で「単純なQUIC idle timeoutを含む
あらゆる`run_connect`失敗」を自動復旧対象に一般化した:

- **`ConnectOutcomeClass`**(`isekai-pipe-core/src/outcome.rs`):
  `StaleTrust`(証明書pin不一致/Auth拒否)/`Unreachable`(それ以外の
  ハンドシェイク前失敗)/`MidSessionDisconnect`/`Unknown`(前方互換)。
- **`decide_connect_failure_recovery`**(`wrapper.rs:1007`、純関数):
  分類を見て`RebootstrapAndRetry`/`RetryConnectLightweight`/
  `AutoBootstrapDisabled`/`NoRecoverableSignal`を決定する。
- **`run_ssh_with_connect_failure_recovery`**(`wrapper.rs:661`):
  `MAX_LIGHTWEIGHT_RETRIES=5`・`RECONNECT_BUDGET=24h`・
  `REDEPLOY_BACKOFF`(初期60s/最大300s/jitter25%)による再デプロイ頻度
  制限付きのリトライループ。

唯一の例外(自動化しない)は、新規ホストのTOFU確認・認証情報失効など
本質的にユーザー入力が必要なケース(`always-connects.md`)。

対してAndroidの`spawn_reconnect_loop`(`orchestrator.rs`)は、
`connect_via`が返す`Err`の**中身を一切見ずに**ログ出力するだけで、
次の`retry_interval`(固定3秒)後に**全く同じ手順**
(`build_and_store_session`→各Transportの`connect()`、その内部で
`ensure_helper_running`を含む)を繰り返す。

## 2. 問題

1. **「たまたま直る」への依存**: `ensure_helper_running`
   (`helper_bootstrap.rs`)は接続の都度SHA256/バージョン一致確認を
   行うため、stale binaryが原因の失敗は次のリトライで自然に解消し
   得る。しかしこれは意図的な分類に基づく対処ではなく、たまたま
   「常に呼ばれる」実装になっているために結果的にカバーされているに
   過ぎない。
2. **再デプロイでは直らない失敗の未区別**: fencing拒否
   (`BUSY_OTHER_SESSION`)やresume失敗など、`isekai-pipe serve`側の
   状態に起因する失敗(サーバーを再起動しない限りクライアント側の
   再試行では原理的に回復不可能、`always-connects.md`が明記する
   ケース)を、Androidは他の失敗と区別せず同じ手順で
   `ADR_ANDROID_RECONNECT_TIMEOUT.md`のtimeout(現状60秒)まで
   単純リトライし続ける。
3. **本質的に自動化してはいけないケースとの未区別(要確認)**: 新規ホストの
   TOFU確認・ホスト鍵mismatch相当の失敗をAndroid側がどう扱っているかは
   本セッションでは裏取りできていない。`isekai-trust::FileBackedHostKeyVerifier`
   相当の仕組みがAndroid側(russh in-process)にもあるか、あるとしてそれが
   `spawn_reconnect_loop`から見てどう報告されるかは、実装着手前に
   必ず確認すべき最優先の未確認事項。もし区別できていない場合、
   「本来ユーザー確認が必要な失敗を延々と自動リトライし続ける」という
   `always-connects.md`が想定していない状態になっているおそれがある。

## 3. 実装方針

1. 各Transportの`connect()`エラー型を、少なくとも次の2分類を表現できる
   形へ拡張する:
   - **自動化してはいけない失敗**(証明書pin不一致・ホスト鍵mismatch・
     認証情報失効相当) — 検出したら`spawn_reconnect_loop`をその場で
     停止し、UIへ「ユーザー確認が必要」な状態を通知する。
   - **それ以外の自動復旧してよい失敗** — 現状通りリトライを継続する。
2. `rust-ssot.md`の原則通り、この判断はRust側(`orchestrator.rs`)に置き、
   Kotlin側には分岐ロジックを作らない。
3. `always-connects.md`の「唯一の例外はTOFU確認・トークン失効」という
   原則を、コード上のenum variant(例: `ConnectFailureClass::RequiresUserInput`)
   として明示的に表現する。

## 4. 非目標

- Windows側の`RedeployGate`(backoff付き再デプロイ頻度制限)をそのまま
  移植することはしない。Androidの`ensure_helper_running`は元々
  「バージョン+SHA256比較のみの軽量チェック、不一致時のみ実際の
  再インストール」であり、Windows側ほど「頻繁な再デプロイのコスト」が
  問題にならない可能性がある(実装着手時に要検証——もし同等のコスト
  問題が実際にあると分かれば、その時点で本ADRのスコープに追加する)。

## 5. Open Questions

- **最優先**: Android側で「証明書pin不一致・ホスト鍵mismatch」相当の
  失敗は現状どう扱われているか(既存実装の再確認が必要。無ければ
  「別途対処が必要なバグ」に格上げ)。
- `ADR_ANDROID_RECONNECT_TIMEOUT.md`(timeout設計)・
  `ADR_ANDROID_MIDSESSION_RESUME_BUDGET.md`(reattach予算)との実装順序
  ——本ADRの分類を先に入れておくと、他2件の「自動化してよい失敗だけを
  長時間リトライする」設計がしやすくなる可能性が高い。

## 6. 参照実装

(実装後に追記)
