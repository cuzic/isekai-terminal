# 2026-09-29 コードレビュー指摘: isekai-pipe (connect/serve, engine) / quicmux resume

レビュー全文(読み取り専用・静的読解)の全指摘を1件1項目で起票する。行番号はレビュー時点の
作業ツリーのもので、origin/main(`0ac06820`)上で各指摘が今も有効かをコードで再確認した。

状態: `[ ]`未着手 / `[x]`修正済み / `[-]`対応不要(理由) / `[~]`見送り(理由)

担当範囲: `rust-core/isekai-pipe/**`, `rust-core/isekai-pipe-core/**`, `rust-core/quicmux/src/resume.rs`。

## High

- [x] **PIPE-01** (High) `isekai-pipe/src/main.rs::parse_serve`
  - 要約: `parse_serve` の許可リストに `--bind-port-range` / `--relay-transport` が無く、
    bootstrap(`isekai-bootstrap/src/install_script.rs`)が生成する起動引数を `EX_USAGE` で拒否する。
    サイレント再デプロイでも同じ引数で同じ失敗を繰り返す(always-connects違反)。mainでも有効。
  - 方針: engine側に「serveがそのまま転送する値付きオプション」の単一リストを置き、`parse_serve` はそれを参照する
    (二重管理を構造的に解消)。bootstrap生成と同形のargvを `parse_serve`→`engine::parse_args_from` に通す
    単体テスト、リスト全要素がengineで既知であることのテスト、実バイナリで `--bind-port-range` を渡すe2eを追加。
    install_script.rs(生成側)は触らない。
- [x] **PIPE-02** (High) `engine/mod.rs::handle_attach_stream` / `finish_or_park_session`
  - 要約: `insert_existing` が `Rejected` のとき `table_guard=None` のまま中継し、DataStreamDied/Preempted で
    `lease.keep()`+テーブル外のhandleにpark → 誰も `relay_ended` を呼ばずfencing slotが永久リーク。mainでも有効。
  - 方針: テーブルに載っていない(=resume不能な)セッションは DataStreamDied/Preempted でも `lease.release()` して
    TCPを破棄する。根本の引き金(PIPE-11)も同時に塞ぐ。
- [ ] **PIPE-03** (High) `engine/mod.rs::relay_buffered`(S→C) / `resume_loop.rs::pump_c2h`(C→S)
  - 要約: 「送信→replayバッファへappend」の順で、送信失敗/キャンセル時にバイトがreplayから欠落する。mainでも有効。
  - 方針: 読み取り量は既に `remaining_capacity()` で頭打ちなので、appendを先に行ってから送信する(両側)。
    `quicmux::ReplayBuffer::advance_start` のdocs(send-then-append前提の記述)も更新。
- [x] **PIPE-04** (High) `engine/attach_runtime.rs::start_connect` / `activate`
  - 要約: spawn後に `Connecting{task}` を登録するため、spawn先が先に `PendingTarget{tcp}` を入れた後に上書きされ得る
    → `activate()` がリソースを見つけられず `EstablishedLease` 未発行のまま slot が `Established` で孤児化。
    失敗パスでは `Connecting` エントリが残る。mainでも有効。
  - 方針: leasesロックを保持したままspawn+登録する(spawn先のPendingTarget挿入は必ず後になる)。spawn先は
    `Connecting` の場合のみ `PendingTarget` に置き換え、失敗時はエントリを除去。`activate()` はStartRelayを
    得たのに資源が無い場合、その場で `RelayEnded` を適用して slot を返す。

## Medium

- [x] **PIPE-05** (Medium) `engine/mod.rs::relay_buffered` プリエンプション
  - 要約: `preempt.notified()` が select の各周回で作り直されるため、arm内の write で詰まっている間の
    `notify_waiters()` を取りこぼす(ゾンビ接続でこそ効かない)。mainでも有効。
  - 方針: `Notified` をループ外で1つだけ作って pin+enable して保持し、S→C の `send.write_all` と C→S の
    TCP書き込み(cancel-safeな `write` を1回ずつ、進んだ分だけ `helper_committed_offset` を進める)も preempt と select する。
- [x] **PIPE-06** (Medium) `engine/attach_runtime.rs::hello` / `attach_arbiter.rs`
  - 要約: `rx.await` に上限が無く、supersede/next差し替え/CANCELで旧keyのwaiterが解決されずタスク/waiterがリーク
    (`--once`はハング)。同一keyの再送HELLOは前の呼び出し元を `Unsupported` で落とす。mainでも有効。
  - 方針: arbiterが旧keyへ `SendReject(StaleGeneration)` を出す(supersede時・next差し替え時)、CANCEL時も
    該当keyへ `SendReject` を出す。waiterは同一keyで複数保持し全員に配送。`hello()` はタイムアウトで包み、
    タイムアウト時にwaiterを除去。
- [x] **PIPE-07** (Medium) `engine/mod.rs::finish_or_park_session`(TcpDied) / `SessionTableEntryGuard::drop`
  - 要約: `lease.release()` → `sessions.remove(id)` の順で、解放直後に同session_idで登録された新エントリを消し得る。
    Dropフォールバックもidだけで無条件remove。mainでも有効。
  - 方針: `SessionTable::remove_if_same(id, &handle)`(`Arc::ptr_eq`)を追加し、先にremoveしてからrelease。
    ガードもhandleを保持して `remove_if_same` を使う。
- [x] **PIPE-08** (Medium) `engine/mod.rs::handle_resume_stream`(arbiter slot無しでrepark)
  - 要約: `established_lease_for` が `None` なのに repark してテーブルに残し、以後毎回 UnknownToken、
    sshd接続を最大 `--resume-window` 抱え続ける。mainでも有効。
  - 方針: この分岐では repark せず `remove_if_same` してTCPを破棄する。
- [x] **PIPE-09** (Medium) `engine/mod.rs`(control stream無しのS→C 4MiB停止)
  - 要約: control streamが確立しないとAPP_ACKが来ず、replay満杯でS→Cが永久停止。docstringも事実と異なる。mainでも有効。
  - 方針: `Session::resume_disabled` フラグを追加。control stream確立失敗/タイムアウトでフラグを立て、replayをクリアし
    tee停止(`output_space_available`で中継ループを起こす)。この状態のセッションはDataStreamDied/Preemptedでもparkせず破棄。
- [ ] **PIPE-10** (Medium) `resume_loop.rs::resume_with_backoff_until_deadline`
  - 要約: `OffsetGone` とreplay範囲外(committed_offsetがローカルreplay範囲外)は決定的・恒久的失敗なのに
    deadline(既定10日)まで再試行し、ConnectOutcomeも書かれない。mainでも有効。
  - 方針: 両者を即give-up(`Err`)にし、既存のgive-up経路(→`write_connect_outcome_for_wrapper`)に渡す。
    replay書き込み失敗/タイムアウトは一過性として従来通り再試行。
- [x] **PIPE-11** (Medium) `engine/mod.rs::admit_new_session`
  - 要約: 容量判定が check-then-act で非原子的で `--max-sessions` を超過し、PIPE-02の `Rejected` を引き起こす。
    evict対象のslotが既に無い場合も容量が空かないまま admit する。mainでも有効。
  - 方針: `AttachRuntime` にadmission用ロックを置き、「数える→evict→arbiterへHelloReceived適用(slot予約)」を
    そのロック内で原子的に行う(`hello` を登録と待機に分割)。evict後に再度数え直し、空くまで繰り返す。

## Low

- [x] **PIPE-12** (Low) `engine/mod.rs::handle_resume_stream` RESUME_ACK+replay書き込みにタイムアウト無し
  - 方針: `respond_resume_accepted` をタイムアウトで包み、超過時はreparkして終了。
- [ ] **PIPE-13** (Low) `quicmux/src/resume.rs::decode_resume_request` 未認証で最大128KiBアロケーション/無期限滞留
  - 方針: token/auth_blob長に上限を設けて早期拒否。呼び出し側(`handle_resume_stream`)でdecodeを `HELLO_TIMEOUT` で包む。
- [x] **PIPE-14** (Low) `engine/mod.rs::release_slot_for` が「その時点の」leaseを解放する
  - 方針: `Session` にlease IDを刻み、evict/sweepはevictしたエントリのlease IDを返す。呼び出し側はそのleaseで
    `relay_ended`(lease一致検査あり)を呼ぶ。
- [ ] **PIPE-15** (Low) `quicmux/src/resume.rs::ReplayBuffer::advance_start` が1バイトずつpop
  - 方針: `drain(..k)` に置き換える。
- [ ] **PIPE-16** (Low) `engine/mod.rs::resolve_relay_jwt` のゼロクリア不完全 / `--relay-jwt` 継続受理
  - 方針: `trimmed` の別Stringを作らず in-place で truncate して返す。`--relay-jwt`(argv露出)は後方互換のため
    受理は続けるが、使われた場合は警告ログを出す。
- [ ] **PIPE-17** (Low) `connect.rs` `run_connect` が非Send(既知)

## 他領域からの連携依頼

- [ ] **PIPE-18** (連携: isekai-ssh A2) `isekai-pipe-core/src/outcome.rs::ConnectOutcome`
  - 要約: relay経路のresume window枯渇・cross-family fallback・panicはSSHバイトが流れた後でも `Unreachable` を書くため、
    wrapperがpre-handshake失敗とmid-session失敗を区別できず `isekai-ssh host -- cmd` のリモートコマンドを再実行し得る。
  - 方針: `#[serde(default)] session_established: bool` を追加(後方互換)。`run_resume_loop`(SSHバイトが流れる唯一の経路)
    に入った時点でプロセスグローバルなフラグを立て、`write_connect_outcome_for_wrapper` がそれを刻む。
    class自体(`Unreachable`→`RebootstrapAndRetry`)は変えない。isekai-ssh側の読み取りはfix-isekai-ssh担当。
