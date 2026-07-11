# 「常に接続できる」原則

`isekai-ssh <hostname>` は、ローカルにキャッシュされたデプロイ情報(`PersistentProfile`)
が**どんな状態であっても**——古い・死んでいる・fingerprint不一致・`isekai-pipe serve`
プロセスがkillされている等——実際のネットワーク経路とSSHアクセス自体が生きている限り、
**自動的に(サイレント再デプロイを挟んで)接続できなければならない**。

## ルール

- ユーザーが `isekai-ssh doctor --fix` や `isekai-ssh init` を**手動で**実行しない限り
  復旧しない接続失敗は、原則として**バグ**として扱う。
- `isekai-pipe connect`(`ssh`のProxyCommand)側で新しい失敗系統を追加・変更するときは、
  「この失敗は`isekai-ssh`のwrapperが自動的にサイレント再bootstrap+再試行できるように
  `ConnectOutcome`を書いているか」を必ず確認する。`isekai-pipe-core::ConnectOutcomeClass`
  ・`isekai-pipe/src/main.rs::write_connect_outcome_for_wrapper`・
  `isekai-ssh/src/wrapper.rs::run_ssh_with_connect_failure_recovery`が実装本体。
  `run_connect`が失敗する経路である限り(=SSHバイトが一度も流れる前の失敗である限り)、
  新しい失敗理由を追加しても**書き込み自体は自動的にカバーされる**(`write_connect_outcome_for_wrapper`
  は`run_connect`のあらゆる`Err`に対して無条件で呼ばれる)——ただし新しい`ConnectOutcomeClass`
  を追加する場合は`wrapper.rs::outcome_summary`にもメッセージを足すこと。
- サーバー側(`isekai-pipe serve`)の状態リーク(例: `AttachArbiter`のfencing slotが
  解放されないまま残る)は、クライアント側の再試行では原理的に回復できない。新しい
  session/lease/park状態を`isekai-pipe/src/engine/`に追加するときは、そのsessionが
  どんな経路で破棄・立ち退き・タイムアウトしても、対応する`AttachRuntime::relay_ended`
  が必ず呼ばれることを確認する(`SessionTable::sweep_expired_parked`と`insert_existing`
  の両方が過去にこれを一度ずつ怠っていた——同じ見落としを繰り返さないこと)。
- 唯一の例外: 本質的に自動化できないケース。新規(未登録)ホストの初回TOFU確認、
  `isekai-ssh login`のトークン失効など、ユーザー入力が本質的に必要な場合は対象外。

## 理由

2026-07-11、無線LAN切断からの再接続が「サーバー側で古いsessionのfencing slotが永久に
解放されない」バグと「単純なQUIC idle timeoutはstale-trust扱いされず自動復旧しない」
バグの2つが重なって、ユーザーが`isekai-ssh <hostname>`を何度実行しても復旧しない状態に
実際に陥った。この経緯から、「一部の失敗理由だけ自動復旧する」という当初の
`ISEKAI_PIPE_DESIGN.md` §8 Epic Nの設計判断(無駄な再デプロイを避けるため接続タイムアウト
は対象外、という判断)は明示的に覆され、「`isekai-ssh <hostname>`は常に接続できる」が
最優先の設計原則として確定した(`ISEKAI_PIPE_DESIGN.md` §8 Epic N-2)。

## 参照実装

- `isekai-pipe-core/src/outcome.rs`: `ConnectOutcomeClass`(`StaleTrust`/`Unreachable`)
- `isekai-pipe/src/main.rs`: `write_connect_outcome_for_wrapper`
- `isekai-ssh/src/wrapper.rs`: `run_ssh_with_connect_failure_recovery`,
  `decide_connect_failure_recovery`, `outcome_summary`
- `isekai-pipe/src/engine/resume.rs`: `SessionTable::sweep_expired_parked`,
  `SessionTable::insert_existing`(どちらも`InsertOutcome`/破棄した`SessionId`一覧を
  返し、呼び出し元[`isekai-pipe/src/engine/mod.rs`]が`AttachRuntime::relay_ended`で
  fencing slotを解放する)
- `ISEKAI_PIPE_DESIGN.md` §8 Epic N / Epic N-2
