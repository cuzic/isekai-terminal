# rust-core/src (isekai-terminal-core) コードレビュー指摘の起票 (2026-09-29)

元レビュー: 2026-09-29の読み取り専用4並列レビュー(rust-core/src 配下)。行番号はレビュー時点のもの。
各指摘は origin/main (0ac06820) のコードで今も有効かを確認してから状態を付けた。

凡例: `[ ]`未着手 / `[x]`修正済み / `[-]`対応不要(理由) / `[~]`見送り(理由)

## High

- [x] **RC-01** High — `resume_client.rs:49-55, 305-316` — ReplayBufferが容量超過で未ACKバイトをevictし、reattach時に`replay_from`が`None`でも成功扱い → C→Sバイトが黙って欠落しSSHストリーム破損。
  - 方針: `helper_committed_offset`が`[start_offset, end_offset]`外なら回復不能エラーとしてreattachを即失敗させる(リトライ予算を消費しない)。
- [x] **RC-02** High — `resume_client.rs:349-362` — `write_all`部分成功→失敗時、chunkがreplay未登録のまま新接続へ全量再送され、helperがcommit済みの先頭部分が重複。
  - 方針: chunkを書き込み**前に**replay_bufferへ積み、reattach時の再送は`helper_committed_offset`からの差分のみにする。
- [x] **RC-03** High — `orchestrator.rs` `disconnect()` — `reconnect_epoch`を進めず、再接続ループ中のタブclose/切断でもループが最大60s継続し、成功すると閉じたタブへConnectedを通知する。
  - 方針: `disconnect()`で`reconnect_epoch`を進めループ系フラグをクリア、ループ中なら進行中attemptの世代も無効化し`Disconnected`をRust側から通知(Kotlinに`cancelReconnect`併用を要求しない)。
- [x] **RC-04** High — `orchestrator.rs` `cancel_reconnect`/ループのtimeout — 進行中attemptの`session_generation`を無効化せず、後から成功するとDisconnected通知後にConnectedへ戻る/失敗すると二重Disconnected。
  - 方針: cancel/timeout時に`session_generation`を進め(進行中attemptの遅延callbackを無視させ)、attempt sessionをdisconnect、`phase=Idle`へ。timeout側のDisconnected通知はepoch一致時のみ。
- [-] **RC-05** High — `pool.rs:79-86, 143-166` — Ready handleの生存確認・evictなしで死んだ接続を再利用。
  - 対応不要: mainで既に修正済み(#120/#121: `try_attach_with`が`PooledSshHandle::is_alive`で生存確認し死んでいれば同じスロットを`Connecting`へ差し替え、`mark_dead_if_same`で最初のchannel open失敗/timeoutをtombstone化。#122で仮想時間テスト、#124でモデルベーステスト済み)。
- [x] **RC-06** High — `session.rs:364-367, 1317-1321` / `trzsz.rs:580-586` — trzszアップロードchunkを容量64のチャネルへ`try_send`し満杯時に黙って破棄(2層)。
  - 方針: (a) Kotlin→`SessionCmd`はruntime外スレッドからなら`blocking_send`でバックプレッシャー(orchestrator側はsessionロックを解放してから呼ぶ)、(b) event loop→transportの`SendStdin`は捨てずにローカルキューへ積み、`reserve()`アームで順序通り流す。キューが閾値を超えたら`session_cmd_rx`の受信を止めて上流へ背圧を伝える。
- [x] **RC-07** High(sec) — `transport/ssh_handler.rs:517, 249-257` / `orchestrator.rs:512-520` — ProxyJump時、jump hostのホスト鍵イベントがtargetの`host:port`で検証/pinされる。
  - 方針: `TransportEvent::HostKey`に検証対象の`(host, port)`を持たせ、`RusshEventHandler`をjump/targetそれぞれの識別子付きで構築する。
  - Kotlin側は既存のOrchestratorCallback::on_host_key(host, port, fp)が踏み台の識別子で呼ばれるだけなので変更不要(UniFFI公開シグネチャ変更なし)。
- [x] **RC-08** High — `terminal.rs:2728-2730, 2747-2749` — OSC 133;C〜;D間の出力キャプチャに上限なし。
  - 方針: 行数・バイト数の上限を設け、超えたら古い行から捨てる(リングバッファ的に直近N行を保持)。
- [x] **RC-09** High→Medium — `terminal.rs:2284-2292` — OSC 8リンクテーブルは件数上限のみでURL長無制限(最大~700MB級)、毎画面更新でKotlinへ全コピー。
  - 方針: URI長上限(2 KiB、他端末と同程度)を設け、超えるURIはリンクとして登録しない。

## Medium

- [x] **RC-10** Medium — `trzsz.rs:219-250` — `::TRZSZ:TRANSFER:`後の行がパース不能だと`tail_buf`に保持し続け、以後VTEへ一切flushされず端末フリーズ+無制限成長(二乗再走査)。
  - 方針: magic後の最初の行が改行まで揃ってなおパース不能ならVTEへflushする。改行待ちの候補にもサイズ上限を設け、超えたらflush。
- [x] **RC-11** Medium — `trzsz.rs:274-277, 496-500, 681-687` — フレーム行長・WaitingKotlin中のバッファ・zlib展開出力に上限なし(zip bomb)。
  - 方針: 行長上限・WaitingKotlin中のバッファ上限・`ZlibDecoder`に`take(limit)`で展開上限を設け、超過時は転送失敗にする。
- [x] **RC-12** Medium(sec) — `lib.rs:691-706` — bracketed paste時に本文中の`ESC[201~`を除去せずに括る → 貼り付けでコマンド注入。
  - 方針: bracketed paste時は本文中のESC(0x1B)を除去する(xterm等と同様)。
- [x] **RC-13** Medium — `orchestrator.rs:913` / `session.rs:1172-1178` — 生きている旧sessionをdisconnectせずに差し替え(旧接続・フォワード残存)、`session_cmd_rx`クローズ後に`select!`がbusy-spin。
  - 方針: `build_and_store_session`で旧sessionを取り出してdisconnectする。`session_cmd_rx`がNoneを返したらそのアームを以後無効化する。
- [x] **RC-14** Medium — `isekai_pipe_quic_transport.rs:548-567` — `spawn_app_ack_bridge`が無限ループ(resumeごとに追加、4MiB replay bufferを保持し続ける)。
  - 方針: `Weak`で`ClientResumeState`を持ち、strong参照が消えたら終了。さらにbridge世代番号を持たせ、resume後に新しいbridgeが立ったら古いbridgeは終了する。
- [x] **RC-15** Medium — `rebind_driver.rs:101-111` — ループタスクが自身の`input_tx`クローンを保持しており`input_rx.recv()`がNoneにならず終了しない → Endpoint/UDP fdリーク。
  - 方針: ループへ渡す送信端を`WeakSender`にする(必要時のみupgrade)。
- [x] **RC-16** Medium — `pool.rs:106-117` / `lib.rs:1641-1644` — 接続確立に全体タイムアウトがなく、handshake停止でエントリが`Connecting`のまま、後続タブ・再接続が永久待ち。
  - 方針: 確立処理(TCP/KEX/認証/jump)に全体タイムアウトを設け、失敗として`publish_failure`する。ホスト鍵確認のユーザー応答待ちにも上限を設ける。
- [x] **RC-17** Medium — `orchestrator.rs:524-527, 540-541→767-773` — 世代チェックと状態更新が別ロック区間で、古いcallbackが新しい接続試行を上書きしうる。
  - 方針: `on_connected`/`handle_unexpected_disconnect`で世代チェックと状態更新を同一ロック区間で行う(世代を引数で渡す)。
- [x] **RC-18** Medium — `orchestrator.rs:959-969, 1016-1029` — 再接続ループがepochチェック外で`Reconnecting`/`Disconnected`を通知し、Connected後にReconnecting/Disconnectedが届きうる。
  - 方針: timeout時のDisconnectedはepoch一致時のみ通知。Reconnecting通知後にepochを再確認し、Connectedへ遷移済みなら正しい状態を再通知する。
  - 注: Kotlin callbackをロック外で呼ぶ規約上、通知と状態確認を完全にアトミックにはできないため、通知後にepochを再確認し正しい状態(Connected/Disconnected)を再通知して上書きする方式にした(競合時のみ同じ状態が2回届きうる)。
- [x] **RC-19** Medium — `orchestrator.rs:1606, 683` — `pending_file_previews`が切断/世代変更で解放されず、Kotlinが永久待ち。
  - 方針: 切断時・新しいsessionへの差し替え時に保留中要求をすべてErrorで解決する。
- [x] **RC-20** Medium — `orchestrator.rs:298-311` — 転送中に切断してもtrzsz状態(最大2GiBのdownload_buf・transfer_id・interactive_busy)がリセットされない。`on_trzsz_finished`/`download_chunk`がtransfer_idを照合しない(low)。
  - 方針: 切断時に転送状態をクリアし`Done{success:false}`を通知。download_chunk/finishedは`current_transfer_id`と一致しない場合は無視する。
- [x] **RC-21** Medium — `ssh_handler.rs:262-270` / `agent_forward.rs:160-176` — プール共有Handleのagent-forward署名確認が確立したタブのevent loopへ流れる。確立タブが閉じると以後全拒否。
  - 方針: 共有Handleのagent確認経路を、そのHandleを現在使っているタブのうち生きているものへルーティングする(チャネルごとの送信先リストから生きているものを選ぶ)。
  - 注: どのタブのsshが署名を要求したかはSSHプロトコル上判別できないため、生きているうち最も新しく開いたタブへ送る(確立タブが閉じても全拒否にはならない)。
- [x] **RC-22** Medium — `transport/file_preview_exec.rs:46-62` — stdout無制限・タイムアウトなし・タブ終了後も継続。
  - 方針: 出力上限と全体タイムアウトを設ける(`run_exec_on_handle`と同等)。
- [x] **RC-23** Medium/Low — `ssh_handler.rs:353-364, 832, 848-850` — ctl streamlocalで認証前に無制限`read_line`・タイムアウトなし、ctlキュー無制限、`CtlVarStore`無制限。
  - 方針: 行長上限+読み取りタイムアウト、`CtlVarStore`の件数/サイズ上限。
  - 注: ctlキュー(CtlForwardMapのUnboundedSender)自体の有界化は型変更が広く及ぶため見送り。1接続1メッセージで行長・読み取り時間が有界になったため、残る増幅は接続数に比例するのみ。
- [x] **RC-24** Medium — `multipath_transport.rs:88-95, 905, 928, 792-795, 647-649, 979-982` — Kotlinから所有権移譲されたraw fdが早期return経路でcloseされずリーク。
  - 方針: FFI境界を越えた直後に`OwnedFd`でラップし、どの経路でもdropでcloseされるようにする。
  - 追加で発見: 自動再接続がlast_connect_attemptに残った同じ生fd番号を再利用して開き直していた(既にclose済み/別ファイルに再利用されたfdをcloseしうる)。再接続用に保存する設定からはwifi_fd/cellular_fdを外すようにした(再接続では物理pathを黙って使わない)。fdのclose自体の回帰テストはfd番号の再利用で並列テストと競合するため追加していない。
- [x] **RC-25** Medium — `trzsz.rs:428-436, 381, 371` — ダウンロードのMD5を検証せず成功扱い。SIZE/NUMのパース失敗を0/1へ黙って変換。
  - 方針: 受信データのMD5を計算して照合し、不一致なら失敗。SIZE/NUMのパース失敗は転送失敗にする。
- [x] **RC-26** Medium — `sixel.rs:229-231` — Sixel色指定パラメータVecが無制限に伸びる。
  - 方針: `params.len() >= 5`以降はpushしない(使うのは先頭4〜5個のみ)。
- [x] **RC-27** Medium — `terminal.rs:3352-3357` — 中間バイト付きDCS(`$q`DECRQSS / `+q`XTGETTCAP)もSixelとしてデコードし画面破損。
  - 方針: `c == 'q' && ints.is_empty()`のときだけSixelとして扱う。

## Low

- [x] **RC-28** Low — `orchestrator.rs:1524-1560` — `notify_network_path_changed`が古いphaseスナップショットで`apply_network_lost`する。
  - 方針: `apply_network_lost`をロック内で「期待するphase/epochのままか」を再確認してから実行する形にする。
- [x] **RC-29** Low(latent) — `orchestrator.rs:1153-1156, 847` — transportの`connect()`が同期`Err`を返すと`phase`が`Connecting`で固着。
  - 方針: `start_manual_connect`/`connect_via`の`Err`経路で`phase=Idle`へ戻す。
- [ ] **RC-30** Low — `TerminalSession.kt:425, 480` — Kotlinが自前状態でconnectをガードし、disconnect時に`connected=false`を自分で書いている(SSOT漏れ)。
  - 方針: Rust側でRC-03/RC-13を直し、Rustが判断・通知できるようにする。Kotlin側のミラー状態除去はandroid/の担当。
- [x] **RC-31** Low — `orchestrator.rs` `ensure_tmux_tab_window` — `TMUX_LOCATOR_REGISTRY`を3回別々にロックし、間に入った`push_ctl_socket_to_tmux`の新しいパスを古い値で上書きしうる。
  - 方針: take→register→set hooksを1回のロック区間で行う。
- [~] **RC-32** Low — `tmux_window_claim.rs:16-35` — claimが明示releaseでしか解放されずTTL/owner生存確認がない。
  - 見送り: owner_idはKotlin側が発行する識別子で、Rust側はその生存を知る手段が無い。解決するにはclaimをSessionOrchestrator等のRustオブジェクトの寿命に結び付けるUniFFI APIの再設計(とandroid/ios側の呼び出し変更)が必要で、担当境界(rust-core/src)内だけでは直せない。プロセス再起動では解消する(ファイル冒頭doc参照)。
- [ ] **RC-33** Low(sec/design) — `transport/ctl_streamlocal.rs:25-35` — `VarScope::Global`のctl変数が異なるリモートホスト間で共有される。
- [x] **RC-34** Low — `tmux_locator.rs:489-494, 549-551` — `TMUX_LOCATOR_REGISTRY`/`pending_ctl_socket_paths`のエントリが削除されない。
  - 方針: orchestrator破棄(Drop)時にそのAppPaneIdのエントリを削除する。
- [ ] **RC-35** Low — `forward.rs:69, 121, 188` / `ssh_handler.rs:295` / `socks.rs:40-148` — フォワード削除/タブ切断で受理済みrelayタスクが残る。SOCKSネゴシエーションにタイムアウトなし。
  - 方針: relayタスクをフォワードごとの`JoinSet`/abort tokenで管理し削除時にabort。SOCKSハンドシェイクにタイムアウト。
- [ ] **RC-36** Low — `file_preview.rs:127-161` — パス先頭の`-`がオプションとして解釈される。
  - 方針: パス引数の前に`--`を入れる(リモートCLIが`--`に対応しているか確認)か、`-`始まりのパスに`./`を前置する。
- [ ] **RC-37** Low — `isekai-protocol/src/bootstrap.rs:178-184` / `helper_bootstrap.rs:159-204, 469-476` — 固定`.tmp`アップロードパス・アップロード後のsha256未検証・`run_exec`にタイムアウト/出力上限なし。
- [ ] **RC-38** Low — `debug_fault.rs:23-53` / `lib.rs:43` — デバッグ用フォルト注入exportがreleaseビルドにも含まれる。
- [ ] **RC-39** Low — `isekai_stun_p2p_transport.rs:270-279` — STUN P2Pのreattachが穴あけ無しの新ソケットからdialする。
- [ ] **RC-40** Low — `quic_transport.rs:135-146, 210` — 旧tsshd QUIC transportは証明書検証有効時に空RootCertStoreで必ず失敗。handshake JSONを`format!`で組み立て`ssh_host`を未エスケープ。
- [x] **RC-41** Low-Medium — `terminal.rs:3144` — REPのclampが`cols*rows`で大きなCPU増幅が残る。
  - 方針: `cols`へclampする。
- [x] **RC-42** Low — `terminal.rs:2769` — SGRのコロン区切りサブパラメータを捨てて誤適用(`4:0`→下線ON、`38:2::R:G:B`の色消失)。
  - 方針: サブパラメータ付きの`4`/`38`/`48`/`58`を正しく解釈する。
- [x] **RC-43** Low — `terminal.rs:2574-2583` — 拡張色成分の範囲チェックなし(`300`が隣チャネルへ溢れる、`38;5;256`が0へwrap、短すぎる`38;2;1`が後続を誤解釈)。
  - 方針: 0..=255外の成分は無効として色を適用せず、パラメータは消費する。
- [x] **RC-44** Low — `terminal.rs:3218-3220` — OSC 0/2タイトルが最初の`;`で切れる。制御文字/bidiフィルタなし。
  - 方針: `params[1..]`を`;`で再結合し、制御文字・bidi制御文字を除去する。
- [x] **RC-45** Low — `terminal.rs:829-834` — urxvt(1015)マウスエンコーディングがurxvt仕様と異なる(+32オフセット無し、release時`m`)。
  - 方針: Cbに+32、releaseはボタン3・終端常に`M`。
- [x] **RC-46** Low — `terminal.rs` 各所 — `pending_terminal_responses`が1バッチ内で無制限。
  - 方針: 1バッチあたりの件数上限を設ける。
