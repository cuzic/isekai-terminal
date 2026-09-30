# 2026-09-29 コードレビュー指摘: transport 領域

対象: rust-core 配下の isekai-transport / quicmux(`src/resume*.rs` は isekai-pipe 担当のため除く)/
h3-noq / h3-qmux / isekai-link-masque / isekai-protocol / isekai-stun / isekai-auth / isekai-trust /
isekai-pipe-protocol / russh-stream-session / local-ipc-mux / isekai-fs-guard / isekai-netmon /
openssh-config / osc-color(quicsock は vendored)。

レビュー原文は古いコミットを読んでいたため、各指摘を origin/main(0ac06820)で再確認した。
行番号は再確認時点のもの。

状態: `[ ]` 未着手 / `[x]` 修正済み / `[-]` 対応不要(理由) / `[~]` 見送り(理由)

## High

- [x] **TR-H1** High `isekai-transport/src/warm_standby.rs:204-219,308-313` — warm standby 接続は実サーバー
  (`isekai-pipe/src/engine/mod.rs:927-952`)の `HELLO_TIMEOUT`(5秒)で閉じられ、promote はほぼ機能しない。
  probe は stream を開いて即 FIN するので、サーバー側でも EOF として close される。テストのモックはこれを検出できない。
  - 方針: `isekai_protocol::standby` に認証付き `STANDBY_HOLD`(0x34)/`READY`/`PING`/`PONG` を新設する。
    サーバー(`engine/standby_hold.rs`)は hold を HELLO_TIMEOUT の対象外にし、2本目の stream の RESUME を通常の RESUME として処理する。
    probe は hold stream 上の PING/PONG に変える。旧サーバー(REJECT_UNSUPPORTED)ではそのセッションの standby を無効化し、再 dial しない。
    モックを実サーバー準拠(first-frame deadline あり)に書き直し、実 `isekai-pipe serve` を相手にする e2e(`isekai-pipe/tests/warm_standby_e2e.rs`)を追加する。
    サーバーの bidi 上限は 2→3(hold+resume+control)。
- [x] **TR-H2** High `isekai-link-masque/src/relay_client.rs:358-366` — `send_datagram` のエラーは TooLarge を含めて何でも break し、`serve --relay` の送信方向が恒久停止する。
  - 方針: TooLarge はそのデータグラムだけ捨てて継続し、NotAvailable/ConnectionError のときだけ終了する。
    内側 endpoint の MTU 上限は不要と判断した。noq の PMTUD(既定で有効、上限 1452)のプローブが捨てられても、プローブ損失として処理されるだけなので正常に動く。
- [x] **TR-H3** High `isekai-netmon/src/linux.rs:91-95,143-150` / `windows.rs:81-87` — 無関係な netlink メッセージ(veth・docker・経路変更・RA)でも `InterfaceChange` を送り、再接続を引き起こす。debounce も無い。
  - 方針: nlmsghdr/ifaddrmsg/rtmsg/ifinfomsg を解析する。対象は ①ループバック以外のアドレス追加・削除(集合の差分で判定)、
    ②main テーブルの default route の追加・削除、③ループバック以外の IF で IFF_RUNNING が変化したとき。これを 300ms debounce する。
    Windows は通知を受けたら IP アドレス集合と既定経路のスナップショットを取り直し、変化した場合のみ送る。こちらも debounce する。

## Medium

- [x] **TR-M1** Medium `isekai-transport/src/relay.rs:174-230,294-313` — 初回 ATTACH(connect・open_bi・HELLO 送信・応答待ち)全体にタイムアウトが無い。
  - 方針: dial と attach handshake を `TRANSPORT_STEP_TIMEOUT` で個別に包み、`TransportError::TimedOut` を返す。
- [x] **TR-M2** Medium `warm_standby.rs:205-218,271` — `ensure_warm` がロックを保持したまま probe と(タイムアウト無しの)dial を await し、promote を待たせる。
  - 方針: スロットを Arc で clone してロック外で probe・dial する。dial は `TRANSPORT_STEP_TIMEOUT` で包む(H1 と同じコミット)。
- [x] **TR-M3** Medium `isekai-transport/src/resume/app_ack.rs:116-191` — `AppAckTasks` を捨てるとタスクがリークする(send ループは stream が死んでも終わらない)。
  - 方針: recv ループが終わったら send ループも終わる自己終了にし、`#[must_use]` を付ける。Drop(abort) は付けない。
    `src/isekai_pipe_quic_transport.rs` が戻り値を捨てているので、Drop にすると即 abort されてしまうため。
    src 側(ハンドル保持と `spawn_app_ack_bridge` の終了条件)は fix-rust-core に連絡済み。
- [x] **TR-M4** Medium `relay.rs:90-92` / `resume.rs:740` / `stun_p2p.rs:267` / `quicmux/src/qmux_backend.rs:147` — 常に IPv4 で bind するため、IPv6 の宛先に繋がらない。
  - 方針: 宛先のアドレスファミリーに合わせた `BindSpec`(`BindSpec::unspecified_for(dest)`)を quicmux に追加し、全経路で使う。qmux の TCP も同様にする。
- [x] **TR-M5** Medium `relay_client.rs:330,343` — relay 受信経路が unbounded チャネルで、公開アドレス宛ての任意の UDP でメモリを枯渇させられる。
  - 方針: bounded(1024)にし、`try_send` が Full なら捨てる(UDP と同じ振る舞い)。
- [x] **TR-M6** Medium `isekai-link-masque/src/capsule.rs:92-95` / `relay_client.rs:288-291,377-410` — capsule 長を無制限に信頼しており、handshake にタイムアウトが無い。
  - 方針: payload の上限を 64KiB にし(`CapsuleDecodeError::TooLarge`)、recv_response と COMPRESSION_ACK 待ちを 15 秒で包む。
- [x] **TR-M7** Medium `quicmux/src/qmux_backend.rs:254-263,296` — QMux listener が一時的な accept エラー1回で恒久停止する。TLS accept にもタイムアウトが無い。
  - 方針: TCP accept のエラーはバックオフして継続する。TLS と QMux handshake はタスク内でタイムアウト付きにする。
- [x] **TR-M8** Medium(潜在) `h3-qmux/src/lib.rs:68-110,173,188` — `StreamIdAllocator` が、peer 起点の stream が id 昇順で届くことを前提にしている。
  - 方針: 実 id を qmux から取得できるならそれを使う。できなければ、サーバー用途(peer 起点 stream の accept)を明示的にエラーにする。
- [x] **TR-M9** Medium `h3-qmux/src/lib.rs:550-596` — `SendStream` の状態機械が、エラー後や Writing 中の finish/reset で偽の成功を返す。
  - 方針: `Failed(err)` 状態を追加する。Writing 中の finish/reset は、書き込みを完了させてから実行する。
- [x] **TR-M10** Medium `relay_client.rs:100-105` — relay 上りの QUIC に keep-alive が無い(noq の既定 idle は 30 秒、keep-alive は off)。
  - 方針: `keep_alive_interval(10s)` を設定する。
- [~] **TR-M11** Medium `isekai-protocol/src/ctl.rs:43,396-402` — ctl の行長上限が、行を読み終えた後にしか効いていない。
  - 見送り(担当境界): 実際の無制限 `read_line` は `isekai-ssh/src/ctl_forward.rs:318-325` と `src/transport/ssh_handler.rs:356-366` にあり、isekai-protocol 側には読み取りコードが無い(I/O-free crate)。fix-isekai-ssh に連絡済み。
- [x] **TR-M12** Medium `isekai-auth/src/file_provider.rs:370-402` — refresh にプロセス間ロックが無く、同じ refresh_token を再利用して token family ごと失効しうる。
  - 方針: `isekai_fs_guard::with_exclusive_lock` で load→refresh→save を囲み、ロック取得後に再読込する。
- [x] **TR-M13** Medium `isekai-auth/src/oauth.rs:57-64` — ureq 3 の既定タイムアウトはすべて None。
  - 方針: `timeout_global`(30 秒)を設定する。`isekai-ssh/src/wrapper.rs:1527` の `spawn_blocking` 化は fix-isekai-ssh の担当(連絡済み)。
- [x] **TR-M14** Medium `isekai-trust/src/host_key_verifier.rs:145-150,176-181` / `store.rs:133-135` — 既知ホストが一致しても毎回書き込み、`last_seen_at` の保存に失敗すると Rejected になる。
  - 方針: 既知一致時の更新は best-effort にし、失敗しても Accepted を返して警告ログのみ出す(always-connects.md)。
- [x] **TR-M15** Medium `isekai-protocol/src/bootstrap.rs:178-185` — helper アップロードの一時ファイル名が固定(`isekai-pipe.tmp`)で、並行 bootstrap で壊れたバイナリが mv されうる。
  - 方針: 一時ファイル名を `.tmp.$$` で一意にし、失敗時は削除する。
- [x] **TR-M16** Medium `local-ipc-mux/src/windows_named_pipe.rs:205` / `framing.rs:75-81` — accept 中の `connect()` エラー1回(open 直後に close したクライアント等)で holder が落ちる。
  - 方針: 一時的なエラーのときはそのインスタンスを捨てて作り直し、local-ipc-mux 内で再試行する(呼び出し側の owner.rs は変更不要)。
- [x] **TR-M17** Medium `local-ipc-mux/src/windows_named_pipe.rs:218-241`(+ `isekai-ssh/src/native/mux/naming.rs`) — 接続先 named pipe サーバーの所有者を検証しないため、別ユーザーに成りすまされる(isekai-ssh レビューの C4 と重複)。
  - 方針: `connect()` で `GetNamedPipeServerProcessId`→`OpenProcessToken`→`TokenUser` の SID が自プロセスの SID と一致することを検証し、不一致なら拒否する。
    名前への SID 付与(naming.rs)は fix-isekai-ssh の担当(連絡済み)。
- [ ] **TR-M18** Medium `openssh-config/src/lib.rs:428-437` — `Match host` を元の destination で判定している(OpenSSH はそれまでに設定された HostName で判定する)。`Match user` は User 未設定時に false になる(OpenSSH はローカルユーザー名で判定する)。
  - 方針: 評価時点の HostName(未設定なら destination)で判定する。`originalhost` を追加する。User 未設定時はローカルユーザー名を使う。

## Low

- [x] **TR-L1** `warm_standby.rs:258-263` — `promote` がキャンセルされると `promoting` が true のまま残る → RAII ガードで戻す(H1 と同じコミット)。
- [x] **TR-L2** `isekai-transport/src/multipath.rs:189-191,226-231` — 同じ PathId にヘルスモニタが二重に起動しうる → 起動済みの PathId を集合で管理して重複を防ぐ。
- [x] **TR-L3** `path_health_fsm.rs:111-116` / `path_health.rs:204-208` — NoViablePath が毎チェック繰り返し通知される → エッジトリガー(状態が変化したときだけ通知)にする。
- [x] **TR-L4** `path_health.rs:210-217` — RTT/ロス劣化で Degraded になっても NoViablePath を通知しない(doc と矛盾) → 通知する。
- [x] **TR-L5** `resume.rs:142,214-220,582-589` — CONTROL_ACK の session_id を、自分が送った値と照合していない → 照合し、不一致なら ControlHandshake エラーにする。
- [x] **TR-L6** `resume.rs:497-503,526-532` — GaveUpAfterGenerationRetries に原因の失敗が残らない → 最後の失敗を保持する。
- [x] **TR-L7** `race.rs:53` — Happy Eyeballs の遅延 250ms が punch の最低所要 ~750ms より短い → production 既定(`isekai-pipe-core` の 750ms)に揃える。
- [x] **TR-L8** `resume/app_ack.rs:169-170` — APP_ACK の offset の単調性を検証していない → 後退する値は無視する(送信済み範囲の検証は counters が送信量を知らないため、単調性の検証のみ)。
- [x] **TR-L9** 秘密値を含む型の `derive(Debug)`: relay.rs `RelayTarget` / stun_p2p.rs / race.rs / resume.rs / isekai-protocol handshake.rs / isekai-auth file_provider.rs・oauth.rs・device_flow.rs → 秘密フィールドを伏せた手書き Debug にする。
  isekai-trust schema.rs は公開鍵のみなので誤検知([-])。
- [x] **TR-L10a** `quicmux/src/qmux_backend.rs:181,308` — MuxClientConfig/MuxServerConfig の idle・keepalive・max_streams を無視している → qmux::Config に反映できるものは反映する。
- [x] **TR-L10b** `quicmux/src/noq_backend.rs:400-402` — `NoqListener::bind` が port_range を無視している → `bind_with_port_range` を使う。
- [x] **TR-L11** `qmux_backend.rs:248-252` — `close()` の起床通知を取りこぼす → notified を enable してからフラグを確認する。
- [x] **TR-L12** `relay_client.rs:270-272,281` — IPv6 SNI や JWT 中の不正なヘッダバイトで expect が panic する → エラーを返す。IPv6 はブラケットで囲む(H2 と同じコミット)。
- [x] **TR-L13a** `noq_backend.rs:32` — `candidate_ports` が start>end で underflow する → 空集合を返す。
- [x] **TR-L13b** `noq_backend.rs:52-66` — ポート範囲の bind を最大 65536 回同期的に試行する → 試行回数に上限を設ける。
- [x] **TR-L14** `relay_client.rs:321-325` — RelayUdpSocket を drop しても外側の接続を閉じない → socket の drop を検知して driver タスクを終了させる(H2 と同じコミット)。
- [ ] **TR-L15** `quicmux/src/resume.rs:479-484` — ReplayBuffer の advance_start が1バイトずつ remove している → drain を使う。
  注: 本項目は quicmux/src/resume.rs のため、「quicmux の resume*.rs は isekai-pipe 担当」の境界に該当する。isekai-pipe 側が触らない場合のみ対応する。
- [x] **TR-L16a** `h3-qmux/src/lib.rs:247,326,480,593` — エラーコードを `as u32` で切り詰めている → u32 に収まらない値は H3_INTERNAL_ERROR に写像する。
- [x] **TR-L16b** `h3-qmux/src/lib.rs:253` — peer の close code を捨てている → code を保持する。
- [x] **TR-L17a** `isekai-auth/src/device_flow.rs:90` — 巨大な expires_in で Instant の加算が panic する → checked_add を使い、上限にクランプする。
- [x] **TR-L17b** `isekai-auth/src/file_provider.rs:101,140` — 巨大な expires_in で i64 がラップする → saturating/try_from にする。
- [x] **TR-L18** `isekai-auth/src/refresh.rs:22` / `oauth.rs:57` — token endpoint が http:// でも許容している → https を強制する(ループバックのみ例外)。
- [x] **TR-L19** `isekai-protocol/src/ctl_vars.rs:49-52` — CtlVarStore のキー数に上限が無い → 上限を設ける(超過時は set を拒否する)。
- [~] **TR-L20a** ctl preamble の比較が非定数時間(`isekai-ssh/src/ctl_forward.rs:319`, `src/transport/ssh_handler.rs:360`) — 担当境界外。fix-isekai-ssh / fix-rust-core の担当。
- [x] **TR-L20b** `isekai-protocol` の自前 ct_eq(hello.rs:49, attach.rs:185,221) → `subtle` に統一する。
- [x] **TR-L21a** `isekai-stun/src/lib.rs:103-105` — XOR-MAPPED のデコード失敗時に MAPPED-ADDRESS へフォールバックしない → フォールバックする。
- [x] **TR-L21b** `isekai-stun/src/lib.rs:211` — 無関係なデータグラムで試行回数を消費する → transaction id が一致しない応答は読み捨て、同じ試行内で待ち続ける。
- [x] **TR-L21c** `isekai-stun/src/lib.rs:205` — 送信元の比較が v4-mapped v6 を考慮していない → to_canonical で比較する。
- [x] **TR-L22** `isekai-trust/src/normalize.rs:48` — 裸の IPv6 を誤って分割する。大文字小文字も正規化していない → 修正する。
- [x] **TR-L23** `isekai-protocol/src/bootstrap.rs:113` — validate_remote_path が先頭 `-` を許している(オプション注入) → 拒否する。
- [~] **TR-L24** `isekai-trust/src/host_key_verifier.rs:97` — TOFU プロンプトの spawn_blocking がキャンセル後も残る。
  見送り: std の stdin 読み取りはキャンセルできない。現状キャンセルする呼び出し元も無い(latent)。回避には stdin 読み取りスレッドの共有化が必要で、isekai-ssh 側の設計変更になる。
- [x] **TR-L25a** `isekai-fs-guard/src/lib.rs:78-86,268-282` — Unix で所有者 uid を見ず、symlink を辿り、world-readable を許容している → symlink_metadata+uid 検証を行い、秘密ファイルの group/world 読み取りを拒否する。
- [x] **TR-L25b** `isekai-fs-guard/src/lib.rs:298-302` — persist の前に fsync していない → sync_all する。
- [x] **TR-L25c** `isekai-fs-guard/src/windows_acl.rs:234-263` — inherit-only ACE を誤検知して fail-closed になる → INHERIT_ONLY_ACE を無視する。
- [x] **TR-L26a** `isekai-netmon/src/macos.rs:80-90,109-114` — run 開始前の stop が失われ、join が永久に待つ → 停止フラグと run ループの再確認で防ぐ。
- [x] **TR-L26b** `isekai-netmon/src/linux.rs:186-192` — Drop で同期 join する(最大 250ms) → 維持するか、detach にするかを検討する。
- [x] **TR-L26c** `isekai-netmon/src/linux.rs:86` — socket に CLOEXEC が無い → SOCK_CLOEXEC を付ける。
- [x] **TR-L26d** `isekai-netmon/src/linux.rs:141-154` — EAGAIN 以外のエラーでも即座にリトライし、busy loop になる → バックオフする。致命的なエラーでは終了する。
- [ ] **TR-L27a** `openssh-config/src/lib.rs:190-241` — `%` トークンを展開しない → HostName 等の基本トークン(%h %p %r %u %n %%)を展開する。
- [ ] **TR-L27b** `openssh-config/src/lib.rs:492-548` — Host 照合が大文字小文字を区別する → 区別しないようにする。
- [x] **TR-L28** `local-ipc-mux/src/framing.rs:35-58` — read_frame がキャンセル非安全であることが doc に無い → 追記する。
- [ ] **TR-L29a** `russh-stream-session/src/lib.rs:386,393,402-410` — connect/handshake にタイムアウトが無い → タイムアウトを付ける。
- [ ] **TR-L29b** `russh-stream-session/src/lib.rs:572-576` — keyboard-interactive の回答をゼロ化しない → zeroize する。
- [ ] **TR-L29c** `russh-stream-session/src/lib.rs:214-253` — ForwardRoutes のキューが無制限 → bounded にする。
