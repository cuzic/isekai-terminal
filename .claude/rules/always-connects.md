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
  ・`isekai-pipe/src/connect.rs::write_connect_outcome_for_wrapper`・
  `isekai-ssh/src/wrapper.rs::run_ssh_with_connect_failure_recovery`が実装本体。
  `run_connect`が失敗する経路である限り(=SSHバイトが一度も流れる前の失敗である限り)、
  新しい失敗理由を追加しても**書き込み自体は自動的にカバーされる**(`write_connect_outcome_for_wrapper`
  は`run_connect`のあらゆる`Err`に対して無条件で呼ばれる)——ただし新しい`ConnectOutcomeClass`
  を追加する場合は`wrapper.rs::outcome_summary`にもメッセージを足すこと。
- サーバー側(`isekai-pipe serve`)の状態リーク(例: `AttachArbiter`のfencing slotが
  解放されないまま残る)は、クライアント側の再試行では原理的に回復できない。新しい
  session/lease/park状態を`isekai-pipe/src/engine/`に追加するときは、そのsessionが
  どんな経路で破棄・立ち退き・タイムアウトしても、fencing slotが必ず解放されることを
  確認する。Step 2a(PR #146)以降、session indexとfencing(`AttachArbiter`)は
  単一集約`serve_fsm::ServeAggregate`の1つの値になっており、**indexからの除去は
  `ServeAggregate::discard`だけ**が行い、同じ遷移でarbiterへ`RelayEnded`を送って
  slotを解放する(`ServeEffect::Discard`を返し、shell側の唯一の解釈は
  `attach_runtime.rs::interpret_in_lock`→`discard_io`)。新しい破棄理由は、shellで
  `relay_ended`を呼び足すのではなく、新しい`DiscardCause`(+それを起こすEvent)として
  reducerに追加し、`serve_fsm.rs`のproptest(I-a〜I-j・Sweep完全性・到達カバレッジ)で
  検証すること。旧`SessionTable::sweep_expired_parked`と`insert_existing`は、shell側で
  `relay_ended`を呼ぶ方式だったため一度ずつこれを怠っていた——その反省からの構造。
  - shell側のタスクがRAIIガードを持たない区間(panic・早期returnでDropだけが走る区間)
    を作らないこと: slotを`Established`にした/維持した瞬間から後始末まで、
    `EstablishedLease`か`SessionTableEntryGuard`(Dropで`RelayTerminated{GuardDropped}`)
    のどちらかが必ず生きていること(`handle_resume_stream`はRESUME許可直後から
    `SessionTableEntryGuard`を張る、Step 2a follow-up)。
- 唯一の例外: 本質的に自動化できないケース。新規(未登録)ホストの初回TOFU確認、
  `isekai-ssh login`のトークン失効、既知ホストのSSHホスト鍵ローテーション/再生成後の
  pinning mismatchは対象外。ホスト鍵mismatchはMITMと正当な再デプロイを機械的に区別
  できないため、自動上書きしてはならない(`isekai-trust::FileBackedHostKeyVerifier`
  が既知・不一致を無条件でサイレント拒否するのは意図的な設計、`ssh(1)`の
  `known_hosts`と同じセキュリティ姿勢)。正当な変更だとユーザーが確認できた場合のみ、
  `~/.config/isekai-ssh/known_ssh_hosts.toml`の該当`"host:port"`エントリを手動削除
  して再接続し、初回TOFU確認をやり直す。
- 上記の「初回TOFU確認は例外」は、`TofuConfirmation::AlwaysPrompt`経路(`isekai-ssh
  init`・未登録ホストへのinline auto-bootstrap)にのみ適用される——`TofuConfirmation::
  Silent`(`doctor --fix`/stale-trust自動復旧)経路では、SSHホスト鍵レベルのTOFU確認
  (`isekai-bootstrap::russh_backend::prompt_new_host_confirmation`)も含めて一切
  対話プロンプトを出してはならない。
  - **`std::io::IsTerminal`でこれを判定してはいけない**(2026-07-19、実機Windows CIで
    実際に踏んだ失敗): 非tty stdinを一律拒否するガードは、e2eテストや実運用の
    自動化スクリプトがpipe経由で正当な回答("yes\ny\n"等)を流し込む
    `TofuConfirmation::AlwaysPrompt`の対話フローまで巻き添えで拒否してしまう
    (pipeは「回答が来る」場合も「何も来ない」場合も等しく非terminalなので、
    isattyでは区別できない)。
  - 正しい実装は、呼び出し元が`TofuConfirmation`から明示的に「対話してよいか」を
    判定してから、Silent時だけ非対話ポリシーを注入すること
    (`isekai_bootstrap::RusshBackend::with_unattended_new_host_policy`を
    `native::bootstrap_backend::default_bootstrap_backend`の`silent`引数経由で
    インストールする、が参照実装)。stdinの実体を覗いて判定するのではなく、
    呼び出し元の意図(`TofuConfirmation`)を素通しする——`rust-ssot.md`と同じ
    「判定ロジックを一箇所に集約し、下流でstateを覗き見しない」原則がここにも
    当てはまる。

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
- `isekai-pipe/src/connect.rs`: `write_connect_outcome_for_wrapper`
- `isekai-ssh/src/wrapper.rs`: `run_ssh_with_connect_failure_recovery`,
  `decide_connect_failure_recovery`, `outcome_summary`
- `isekai-pipe/src/engine/serve_fsm.rs`: `ServeAggregate::discard`(唯一の除去経路、
  slot解放と同一遷移)、`DiscardCause`(`Expired`/`Evicted`/`TcpDied`/`GuardDropped`/
  `Unresumable`)、`ServeEvent::Sweep`/`EvictOldestParked`/`Activated`(LRU立ち退き)
- `isekai-pipe/src/engine/attach_runtime.rs`: `interpret_in_lock`(`ServeEffect::Discard`
  の唯一の解釈→`discard_io`)、`sweep_expired_parked`/`evict_oldest_parked`/`park`/
  `resume_request`(孤児は`RelayTerminated{GuardDropped}`で後続apply破棄)、
  `EstablishedLease`(DropでRelayEndedを送るRAIIバックストップ)
- `isekai-pipe/src/engine/mod.rs`: `SessionTableEntryGuard`(Dropで
  `RelayTerminated{GuardDropped}`を送るRAIIバックストップ)、`handle_resume_stream`/
  `finish_or_park_session`
- `ISEKAI_PIPE_DESIGN.md` §8 Epic N / Epic N-2
- `isekai-trust/src/host_key_verifier.rs`: `FileBackedHostKeyVerifier`(既知一致は
  サイレント通過、mismatchはサイレント拒否、未知のみ`confirm_new_host`を呼ぶ)
- `isekai-ssh/src/native/connect.rs` / `isekai-bootstrap/src/russh_backend.rs`:
  `prompt_new_host_confirmation`(native/Windows経路のSSHホスト鍵TOFU、常に
  対話的 — Silent文脈からは呼ばれない設計)
- `isekai-bootstrap/src/russh_backend.rs`: `RusshBackend::with_unattended_new_host_policy`
  (production向けの非対話ポリシー、`TofuConfirmation::Silent`時のみ注入)
- `isekai-ssh/src/native/bootstrap_backend.rs`: `default_bootstrap_backend`の
  `silent`引数(`TofuConfirmation`から非対話ポリシーの要否を判定する唯一の場所)
- `isekai-bootstrap/src/openssh.rs` / `russh_backend.rs`: サイレント再デプロイの
  ヘルパー再利用判定(pid生存 + `/proc/<pid>/exe`パス一致 + fingerprint一致)に
  稼働中バイナリのsha256一致チェックを追加(2026-07-19)。それまでは中身(ビルドの
  新旧)を一切見ておらず、`isekai-pipe serve`側のバグ修正(Epic N-4/N-5のBUSY_OTHER_SESSION
  永続化修正含む)がいくら入っても生きている古いプロセスがある限り再デプロイされず、
  「`isekai-ssh`のリトライでは回復できないサーバー側の古さ」という、このファイルが
  警告している問題そのものの新しい一形態を実際に踏んだ(このサンドボックス自身が
  relayサーバーであるため、動いているヘルパーのビルド時刻とgit log を突き合わせて
  確認できた)。古いプロセスはkillせず「再利用しない」判定にして新規ヘルパーを
  並行デプロイするだけに留める(フィンガープリント不一致の別トポロジをkillしない
  既存方針と同じ理由: 他のクライアントがそのヘルパーでセッション中の可能性がある)。
