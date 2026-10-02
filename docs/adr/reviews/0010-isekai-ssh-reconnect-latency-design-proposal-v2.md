# 設計案v2: isekai-ssh(Windows/mux)の再接続レイテンシと進捗不可視性

- **Status**: Proposal(2026-09-15。`docs/adr/0010-isekai-ssh-reconnect-latency.md`
  §3の現行案(輸送層状態の公表 + 条件付き入場ゲート + Win32進捗表示)に
  縛られず、ゼロベースで再設計したもの。現行ADRとの対応は§8にまとめる)
- **前提として読んだもの**: `CLAUDE.md`、`.claude/rules/always-connects.md`、
  `.claude/rules/rust-ssot.md`、`docs/adr/0010-isekai-ssh-reconnect-latency.md`全文
- **コード調査の範囲**: `native/mux/{owner,client,mod,protocol,ctl_forward,holder}.rs`、
  `native/{connect,child_stdio}.rs`、`isekai-ssh/src/log_file.rs`、
  `isekai-pipe/src/resume_loop.rs`、`quicmux/src/mux.rs`

---

## 0. この文書の結論(先に書く)

現行ADRは「輸送層の生死を子プロセスから親へ伝播する」ことを設計の核に
据えているが、**コードを追った結果、この核は不要**だと判断した。理由は
2つで、どちらも新しく発見した構造的事実に基づく:

1. **オーナーは自分が詰まっていることを、プロセス境界を一切またがずに
   知ることができる**(§2.1)。`relay_client`は共有`Handle`の
   `tokio::sync::Mutex`を必ず取りに行くため、先行クライアントが詰まって
   いれば後続は`handle.lock()`で**確定的に**待たされる。これは推測では
   なく待ち行列そのものであり、輸送層状態ファイルより厳密な情報である。
2. **「動いているか分からない」の真因は`--isekai-log-file`ではなく、
   mux導入によるプロセストポロジ**である(§2.3)。tsshスタイルの
   ライブ再接続表示(`print_reconnect_status`)は**すでに実装済みで
   テスト済み**だが、mux経路ではそれを出力する`isekai-pipe connect`が
   holder(stdout/stderrが`NUL`の`DETACHED_PROCESS`)の子であるため、
   **構造的に人間に届かない**。Win32コンソールタイトルという新しい表示
   機構を発明する前に、既にある表示が届いていないことを直すべきである。

提案は4段(§4)。**P1(可視性)だけで元の苦情は解消する**と考えており、
P2以降は「待ち時間を縮める」ための追加施策である。そして§4.3で正直に
書くが、**閾値を使って待ちを打ち切る施策は、どんな閾値を選んでも
必ず退行ケースを持つ**——現行ADRが§1.6で1度踏んだ罠は、センサーを
差し替えても消えない。消せるのは「フォールバックを重ねて走らせる」
設計だけであり、その代償(remote側の二重ログイン・対話プロンプトの
競合)も§4.3に明記する。

---

## 1. コード調査で新たに確定した事実

現行ADRに書かれていない、あるいは書かれているが結論に反映されていない
事実を列挙する。いずれも実際にファイルを読んで確認した。

### F-A: 共有`Handle`のMutexは、全クライアントのハンドシェイクを直列化する

- `rust-core/isekai-ssh/src/native/mux/owner.rs:39` —
  `use tokio::sync::{mpsc, Mutex, Notify};`(`handle: &Mutex<client::Handle<H>>`)
- `rust-core/isekai-ssh/src/native/mux/ctl_forward.rs:72-73` —
  ```rust
  let mut guard = handle.lock().await;
  guard.streamlocal_forward(remote_path.clone()).await
  ```
  **排他ロックを保持したまま**リモートの応答を待つ。
- `owner.rs:355`付近(`relay_client`の`open_result`ブロック) —
  `let guard = handle.lock().await;` を取ってから
  `ctl_forward::open_login_shell(...)` / `open_channel(...)` を`await`する。
- `owner.rs:251` — `handle_died`も1秒ごとに`handle.lock().await`する。

現行ADR §1.5で確定済みの「実際にハングしうるのは`channel_open_session`と
`streamlocal_forward`だけ」と合わせると、次が言える:

> **先行クライアントAが死んだリンク越しの応答待ちで詰まっている間、
> 後続クライアントBは`handle.lock().await`で確定的にブロックされる。
> Bは`ctl_forward::request`にも`open_channel`にも到達できない。**

つまり実インシデントの「2回目の30秒待ち」は、確率的な現象でも
タイミング依存でもなく、**Mutexの待ち行列という決定論的な機構**である。
`ctl-socket`がOFFの場合でも`open_result`側のロックで同じ直列化が起きる
ので、経路によらない。

**なぜこれが重要か**: 現行ADR §3.1が輸送層状態ファイルで推定しようと
している「オーナーは今クライアントを捌けるか」は、オーナー自身が
**ローカルで、より確実に**知っている。ファイル・`intent_id`・JSONL・
GC・torn read・「静かな無効化」(現行ADR §5.1が自ら挙げた検出困難な
失敗モード)は、いずれも不要になる。

### F-B: 詰まったholderは`IDLE_GRACE`でも退出できず、pipeの排他claimを握り続ける

`owner.rs::serve_clients`の`select!`で、退出条件は
`wait_for_idle_exit(&active_clients, ...)`(`owner.rs:266-277`)。これは
`active_clients > 0`の間**決して解決しない**。そして`active_clients`の
デクリメントは`relay_client`が**返ってから**行われる
(`owner.rs`の`tokio::spawn`ブロック内、`relay_client(...).await`の直後)。

したがって:

> **クライアントが1つでも`handle.lock()`で詰まっている限り、
> `active_clients`は0にならず、holderは`IDLE_GRACE`(10秒)でも
> `WARMUP_GRACE`(30秒)でも退出しない。ローカルIPCチャネルの排他claimを
> 握り続けるので、`mux/mod.rs::dispatch`の`C::connect`は成功し続け、
> 新しいタブは必ずこの待ち行列に入る。**

`owner.rs:205-213`のコメント自身が、`handle_died`経路について
「every tab retrying against this holder just gets `Rejected` and falls
back to an unmultiplexed direct connect, one per tab, until this holder's
own `IDLE_GRACE` eventually elapses」と、まさにこのアンチパターンを
`always-connects.md`の名前を挙げて警告している。**「ハングしているが
まだ`is_closed()`ではない」ケースについては、その警告が実装されていない**
——これが実インシデントで観測された繰り返しの正体である。

**確認方法**: `serve_clients`の`select!`4アームと`wait_for_idle_exit`の
本体(`owner.rs:266-277`)を読み、`active_clients`のインクリメント/
デクリメント位置を`tokio::spawn`ブロック内で確認した。

### F-C: 「動いているか分からない」の真因はプロセストポロジ(既存の進捗表示が構造的に届かない)

**すでに tssh 互換のライブ再接続表示が実装されている**:

- `isekai-pipe/src/resume_loop.rs:730-745` `print_reconnect_status` —
  経過秒数とresume window残りをTTYへ`eprint!`(`\r`+`\x1b[K`で1行更新)。
- 同 `:81` `RECONNECT_NOTIFY_GRACE = 15s` — 15秒未満の断は無音
  (tsshの`kDefaultUdpReconnectTimeout`に合わせた意図的な設計)。
- 同 `print_reconnect_success` / `notify_os` — 復帰時の表示とOS通知まである。

これが mux 経路で人間に届かない理由:

- `native/mux/holder.rs:115-117` — holderは
  `DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW`で
  spawnされ、`.stdout(Stdio::null())` / `.stderr(Stdio::null())`。
- `native/child_stdio.rs::spawn_isekai_pipe_connect` — 子の
  `isekai-pipe connect`は`.stderr(Stdio::inherit())`。
- ゆえに **holderの子の stderr は `NUL`**。`print_reconnect_status`の
  `eprint!`はそこへ消える。`ISEKAI_PIPE_LOG_FILE`はholder用ログを指すが、
  これは`env_logger`の出力先であって、この生の`eprint!`とは別系統。

一方、**非mux(直接接続)経路では同じ表示がちゃんと出る**: 前景の
`isekai-ssh`プロセスは実コンソールを持ち、子は`Stdio::inherit()`で
それを継承する。

> **つまり `isekai-ssh` は、mux を導入した副作用として、それ以前から
> 持っていた再接続進捗表示を(mux経路でだけ)失っている。**

さらに前景タブ側も沈黙する:

- `native/mux/client.rs:163` — ハンドシェイクの**前に**
  `RawModeGuard::enable()`。
- `client.rs:268` — `tokio::time::timeout(HELLO_ACK_TIMEOUT, frame_rx.recv())`。
  この30秒間、クライアントは一切何も書かない。
- `log_file.rs:280-286` `dispatch` / `:293-300` `log_line!` —
  `--isekai-log-file`が有効なとき、`eprintln!`のフォールバックは
  **呼ばれない**。モジュールdocが「the terminal itself shows only that
  interactive session, nothing else」「`--isekai-log-file` remains the
  absolute override」と明記する通り、意図的な全面抑止である。

**確認方法**: `dispatch`の分岐(`is_enabled()` → `append_line`、
else → holderログ、else → fallback)を読み、`log_line!`のfallbackが
`eprintln!`であることを確認。`print_reconnect_status`が`log_line!`系を
通らない生の`eprint!`であることも確認した。

### F-D: 実インシデントの時間内訳(現行ADRが扱っている割合)

現行ADR §1.1のタイムラインを秒で分解すると:

| 区間 | 秒 | 現行ADRの扱い |
|---|---|---|
| 08:45:16→45:46 mux待ち① | 30 | §3.2ゲートの対象(発火するかは不明と自認) |
| 08:45:46→46:31 直接接続 + mux待ち② | 45 | 30秒分のみ対象 |
| 08:46:31→46:47 ダイヤル失敗→再デプロイ判定 | 16 | 対象外 |
| 08:46:47→46:52 再デプロイ | 5 | 対象外 |
| 08:46:52→48:04 **無音** | 72 | **明示的に対象外**(§3.3・§5.4) |
| 08:48:04→48:18 最終確立 | 14 | 対象外 |
| 合計 | **~191** | |

**mux待ちは合計60秒、全体の約31%**。最大の単一ブロックは72秒(38%)で、
現行ADRはここを「タイムアウトは実装しない、ログのみ」としている。
この配分は、「待ち時間を縮める」施策に複雑さを投じる価値を
割り引く材料になる(§4.3の判断根拠)。

### F-E: プロトコル拡張のコストは既に低い

`protocol.rs:43-57` — `MUX_PROTOCOL_VERSION`は不一致で`Rejected`。
`mod.rs::run_as_client_over`の`Rejected`アームは直接接続へフォールバック
する。よって**フレームを増やしてバージョンを上げても、旧holderが残って
いる間は各タブが直接接続へ落ちるだけ**で、`always-connects.md`に抵触
しない。旧holderは`IDLE_GRACE`で消える。現行ADR §4が(α)案を
「プロトコル拡張が要る」ことを理由に却下しているが、**この拡張は
既に安全に設計されている**——却下理由としては弱い。

### F-F: 輸送層は既に多重化可能(§5の戦略的論点の前提)

`quicmux/src/mux.rs:325` `AnyMuxConnection::open_bi()` /
`:342` `accept_bi()` — 1本のQUIC接続上に複数の双方向ストリームを
開ける。現行ADRが「SSHセッション自体を`isekai-pipe`側へ移す」を
規模を理由に却下しているが、**「輸送だけ共有し、SSHセッションは
タブごとに持つ」という中間形**は、この既存プリミティブの上に載る。
§5.2で論じる。

---

## 2. 問題の再定義

現行ADRは問題を「(1)2回の30秒待ちを消す、(2)不可視性を解消する」と
置いている。上記の事実から、次のように置き直す。

### 2.1 解くべき問題A(確実・高価値): mux経路で進捗表示が人間に届かない

これは「表示機構が無い」問題ではなく、**既にある表示の配線が
切れている**問題である(F-C)。Win32固有でもなく、
`--isekai-log-file`固有でもない。

### 2.2 解くべき問題B(確実・中価値): 詰まったholderが新規タブを吸い込み続ける

F-A + F-B により、これは決定論的な構造欠陥である。
`owner.rs:205-213`のコメント自身が`always-connects.md`を引いて警告して
いるパターンの、実装されていない半分。

### 2.3 解くべきでない(あるいは後回しでよい)問題: 「最初の1タブの待ち時間」

断が短ければ待つのが正解(現行ADR §1.6の教訓)。断が長ければ待っても
無駄だが、**それを事前に知る確実な手段は無い**(§4.3)。そして仮に
完全に消せても、実インシデントでは31%しか縮まない(F-D)。

---

## 3. 設計原則(この案が守るもの)

1. **プロセス境界を新しく越えない**。オーナーが既にローカルで知って
   いる事実で足りるなら、それを使う(F-A)。`rust-ssot.md`の精神にも
   合う: 「輸送の生死」のSSOTは子プロセスだが、「**オーナーがクライアントを
   捌けるか**」のSSOTはオーナー自身であり、後者こそが判断に必要な事実である。
2. **表示機構を新設しない**。既にある`print_reconnect_status`/
   `log_line!`を人間に届ける。
3. **閾値による打ち切りは、退行の可能性を定量して明示する**。
   「退行が無い」と書けるのは、本当に無いときだけ(§4.3)。
4. **確立済みクライアントのリレーには一切触れない**
   (`always-connects.md`、resume window既定10日)。

---

## 4. 提案する施策

### P1: 可視性を直す(最優先・最も確実)

元の苦情に直接答える。P2以降とは完全に独立で、先に単独で投入できる。

#### P1-a: クライアント側ハンドシェイク待ちのライブ表示

`client.rs:268`の`timeout(HELLO_ACK_TIMEOUT, frame_rx.recv())`を、
1秒ティック付きの`select!`に置き換える。2秒(閾値)を超えたら
自分のstderrへ1行更新表示:

```
isekai-ssh: waiting for the shared connection to <host>... (12s)
```

- raw mode下なので`\r` + `\x1b[K`で1行を書き換える
  (`resume_loop.rs::format_reconnect_status`と同じ形。実装を真似るだけで
  新しい依存も新しい定数系も要らない)。
- 決着時(`HelloAck`/`Rejected`/タイムアウト)に`\r\x1b[K`で消す。
- `frame_rx.recv()`はmpscなのでキャンセル安全。`select!`に入れても
  `read_frame`のような部分読みの問題は起きない(`client.rs:266`の
  doc comment が `spawn_frame_reader` を置いた理由そのもの)。
- **2秒という閾値はこの表示にしか影響しない**。接続の成否・待ち時間を
  一切変えないので、退行のしようがない。

**効果の追跡**: 実インシデントの2回の30秒待ちは、いずれも
`client.rs:268`のこのタイムアウトの中で起きている(`Rejected`の
理由文字列 "no response from the owner within 30s" が`client.rs:299`の
まさにこの`Err(_)`アームでしか生成されないことで確認)。したがって
**この30秒×2は、変更後は必ずカウンタ付きで可視になる**。
元の報告「ログインできない(実際はすごく遅かった)」は、少なくとも
「待っていることが分かる」状態に変わる。

#### P1-b: holderの子の進捗を、人間のいる端末へ配る

F-Cの配線切れを直す。holder経路のとき
`child_stdio.rs::spawn_isekai_pipe_connect`の`.stderr(Stdio::inherit())`を
`Stdio::piped()`に変え、holder(=owner)がその行を読んで、接続中の
全クライアントへ新フレーム`Frame::Notice(Vec<u8>)`として配る。
クライアントは自分のstderrへ書く。

- **ハンドシェイク待ち中のクライアントにも届く**ことが肝。
  `relay_client`は`handle.lock()`で詰まっていても、`select!`で
  notice購読を並走させれば書ける(ロックを要求しない経路なので
  F-Aの直列化に巻き込まれない)。
- 配布は`tokio::sync::broadcast`で十分(取りこぼしは表示の欠落
  だけで、状態には影響しない)。
- `MUX_PROTOCOL_VERSION`を3→4へ。F-Eの通り、旧holderが残っている間は
  各タブが直接接続へ落ちるだけで安全。
- **holder以外の経路(前景の直接接続)は一切変更しない**——そちらは
  今でも`Stdio::inherit()`で正しく届いている(F-C)。

**効果の追跡**: 変更後、mux経路のユーザーは、非mux経路のユーザーが
今日すでに見ているのと**同じ**tssh互換の再接続表示
(「connection lost … reconnecting (Ns / 10d)」→「reconnected.」)を
見る。実インシデントで言えば、輸送層がresumeを試みていた区間について、
その事実と経過秒数が端末に出る。**これは新しい情報を発明するのでは
なく、`resume_loop.rs:730-745`が既に生成している情報を届けるだけ**で
ある点が、Win32コンソールタイトル案(現行ADR §3.5)との決定的な違い。

#### P1-c: `--isekai-log-file`下でも進捗だけは端末へ出す

`log_file.rs`に`log_line_progress!`を追加する。`dispatch`と違い、
**ログファイルへ書いたうえで、さらにstderrへも書く**(tee)。

- 適用先は**意図的にごく少数**に絞る: 「再デプロイを始める/終えた」
  「鍵で認証を試みている」「候補をダイヤルしている」など、
  シェルが立つ前のフェーズ境界だけ。
- シェルが立った後は一切使わない(端末表示を壊さないため)。
- これで実インシデントの「再デプロイ中の5秒」「認証の72秒」
  「ダイヤルの16秒」——F-Dで現行ADRが対象外としていた**合計93秒
  (全体の49%)**が、初めて人間から見えるようになる。

**効果の追跡**: `log_line!`が`--isekai-log-file`有効時に端末へ
到達しないことを`log_file.rs:280-286`の`dispatch`で確認済み。
ユーザーのwrapperが常時`--isekai-log-file`を指定していることは
現行ADR §3.5が記録している。したがってこの変更は、その設定のまま
進捗だけを取り戻す。

**これはWin32ではない**。macOS/Linux版`isekai-ssh`や、Windowsでも
Windows Terminal以外のホストで等しく効く。現行ADR §3.5が挙げていた
「OSCタイトルの所有権競合」「`suppressApplicationTitle`で出ない環境」
「RAIIガードでpanic/Ctrl-Cもカバー」という懸念は**すべて消滅する**。

### P2: 詰まったholderが新規タブを吸い込み続けるのを止める

F-A/F-Bの構造欠陥に対応する。**現行ADR §3.2のゲートと同じ位置・同じ
形だが、センサーを輸送層状態ファイルからローカルの事実へ置き換える**。

`owner.rs`で`Mutex<Handle>`を薄く包み、ロック保持の開始時刻を記録する:

```rust
struct GatedHandle<H> {
    handle: Mutex<client::Handle<H>>,
    /// ロックを保持し始めた壁時計ms。0 は「誰も保持していない」。
    busy_since_ms: AtomicU64,
}
```

`lock_tracked()`が返すガードが、取得時に`busy_since_ms`をセットし、
`Drop`でクリアする。`relay_client`のHello検証直後(現行ADR §3.2と
同じ位置)で:

```rust
if let Some(busy_for) = gate.peer_busy_for() {
    if busy_for >= HELLO_ACK_TIMEOUT {
        write_frame(&mut writer, &Frame::Rejected { reason: format!(
            "the shared connection has had an SSH request outstanding for {}s; \
             continuing on a direct connection. multiplexing resumes automatically \
             on your next connection.") }).await;
        return Ok(());   // Err ではない。既知の縮退
    }
}
```

**現行ADR §3.2(β)に対する優位点**(いずれも§1の事実に基づく):

| 論点 | 現行ADR(輸送層状態ファイル) | P2(ローカル) |
|---|---|---|
| 実装規模 | 新crate module + 4ファイル変更 + GC + doctor統合 | `owner.rs`内に~30行 |
| 実インシデントで発火するか | **不明**と自認(§3.2末尾の留保: `Down`は`resume_loop.rs:1429`のデータポンプ確立後にしか書かれず、実インシデントは`failure_stage=quic-connect`の候補ダイヤル段階) | **発火する**(F-A: 先行クライアントが詰まった時点で`busy_since_ms`が立つ。詰まりの原因を問わない) |
| 「リンクは生きているがsshdが応答しない」 | §5.4が「原理的に解決できない」と明記 | **カバーされる**(SSH要求が返らないこと自体を見ている) |
| 静かな無効化の危険 | §5.1が`intent_id`取り違えによる「検出しにくい不具合」として自認 | 無い(同一プロセス内の`AtomicU64`) |
| 壁時計/monotonicの乖離 | §3.1が「suspend時にずれる」と記載 | `tokio::time::Instant`(monotonic)を使えば無い |

**効果の追跡(実インシデントへの適用)**: 08:45:16のクライアント①が
`handle.lock()`を取得 → `streamlocal_forward`で詰まる →
`busy_since_ms`が立つ。08:46:01のクライアント②がHelloを送った時点で
`busy_for ≒ 45秒 ≥ 30秒` → **即座に`Rejected`**。
実測タイムラインの2回目の30秒待ち(08:46:01→08:46:31)は消える。
これは現行ADRのゲートが「消せたか不明」としている区間である。

**残る限界(正直に)**: クライアント①の30秒は消えない。①が詰まった
時点では`busy_since_ms`は立っていない(①自身が最初の詰まり)。
つまりP2は**2番目以降のタブだけを救う**。F-Dの配分で言えば60秒のうち
30秒。

### P3: 「1番目のタブ」も救うには何が要るか(正直な結論: きれいな答えは無い)

現行ADRの(β)条件も、P2の`busy_for >= HELLO_ACK_TIMEOUT`も、
**閾値である以上、必ず退行ケースを持つ**。具体的に書く:

> 先行要求が31秒詰まっており、あと1秒で完了する状況で新規クライアントBが
> 来た場合。今日ならBは`t≒1秒`で多重化されたセッションを得る。P2は
> `t=0`でBを直接接続(実インシデントでは約46秒)へ叩き出す。

これは現行ADR §1.6「誤算2」が指摘したのと**まったく同じ形の退行**で
あり、センサーを変えても消えない。現行ADRはこれを「断が30秒以上
続いているなら待っても無駄だろう」という確率的な議論で受け入れている
——P2も同じ賭けをしている。賭けの妥当性の根拠は
`resume_loop.rs:81`の`RECONNECT_NOTIFY_GRACE = 15s`(15秒未満の断は
自己修復が当たり前、というこのコードベース自身の較正)であり、
30秒はその2倍なので「通常の瞬断ではない」と言える。**外したときの
被害は「多重化を1タブ失う」だけで、接続自体は成立する**
(`always-connects.md`が禁じているのは手動操作なしに復旧しない
接続失敗であって、多重化の機会損失ではない)。

**退行が原理的に存在しない唯一の設計**は、待ちを打ち切るのではなく
**フォールバックを重ねて走らせる**ことである(happy-eyeballs):

- `run_as_client_over`で、mux ハンドシェイクを開始してから2秒後に、
  直接接続(`connect::run_prepared`)を**並行して**開始する。
- 先に確立したほうを採用し、もう一方を中止する。
- 健全時: muxが数msで`HelloAck`を返すので投機は一度も起きない
  ——**コストゼロ、退行ゼロ**。
- 断が短い(20秒で復帰)場合: 投機が2秒で始まる。muxが20秒で勝てば
  投機を中止して多重化を得る(今日と同じ)。投機が15秒で勝てば
  15秒で繋がる(今日より速い)。**どちらでも今日以下**。
- 実インシデント: 直接接続が30秒後ではなく2秒後に始まる
  ——**1番目のタブも28秒短縮**。

**採用を推奨しない理由(コストを正直に)**:

1. **対話プロンプトの競合**。投機側の`connect::run_prepared`は
   パスフレーズ入力・ホスト鍵TOFU`[y/N]`・keyboard-interactive(2FA)を
   要求しうる。mux側が勝ったときにプロンプト表示中の投機を中止すると
   端末状態が壊れる。`always-connects.md`が「唯一の例外」として
   守っている対話経路に、新しい競合を持ち込む。
2. **リモート側の二重ログイン**。muxが勝った場合、認証まで進んだ
   SSH接続を即座に閉じることになる。現行ADR §5.3が(ゲート設計に
   ついて)既に懸念している「タブの数だけリモートへのSSHログインが
   増える」の、常態化した版。
3. **投機側が`drive_connect_recovery`のサイレント再デプロイまで
   到達しうる**。muxが勝っても再デプロイは実行済みになる。

**スコープを限れば安全にできる**: 「この宛先の資格情報がプロンプト
無しで解決でき(`handoff::resolve_handoff_credentials`は暗号化鍵と
確認できたものしかプロンプトしない、`mod.rs::dispatch`のコメント)、
かつホスト鍵が既知」のときだけ投機する。この条件は既存のコードで
判定できる。**ただし、P1+P2を実測したうえで、なお1番目のタブの
待ちが問題だと分かってから着手すべき**——F-Dの配分では、これが
救うのは全体の約16%(30秒/191秒)である。

### P4: 認証フェーズ72秒の観測(現行ADR §3.4を維持)

現行ADR §3.3(タイムアウトは実装しない)・§3.4(フェーズ境界ログ)の
結論に異論は無い。**ただしP1-cにより、これらのログは端末にも出る**
——原因究明のためにユーザーにログファイルを送ってもらう往復が
不要になる。

なお現行ADR §1.3の留保(「生ログを無フィルタで再確認すること」)は
そのまま前提条件として引き継ぐ。72秒の診断は要約されたタイムラインに
依存しており、未確認である。

---

## 5. 現行ADRが立てた問いへの回答

### 5.1 そもそも解くべき問題は何か

**不可視性の解消(§2.1)が第一**。理由は3つ:

1. ユーザーの一次報告が「ログインできない」であり、実際には接続して
   いた——**これは純粋に情報の欠落による誤報**である。
2. 効果が確実である。P1-a/P1-cは接続の挙動を一切変えないので、
   「効くかどうか」を議論する必要がない(現行ADRが施策A・Cで2度
   踏んだ罠に構造的に嵌まらない)。
3. F-Dの通り、現行ADRが対象外としている93秒(49%)も含めて全区間を
   カバーする唯一の施策である。

現行ADR §7が既に「§3.5を優先」と結論しているのは正しい。本案の違いは、
**その施策の中身を「Win32の新機構」から「既存表示の配線修復」へ
置き換える**点にある。

### 5.2 mux のアーキテクチャ自体は正しい選択か

**現行ADRの却下理由は正確だが、検討していない中間形がある**。

却下理由(「認証/TOFU/QUICハンドシェイクをタブごとに払う」)の内訳を
分解すると、コストの大半は**QUICハンドシェイク + bootstrap/再デプロイ
判定**であって、SSH認証そのものではない。確立済みの輸送上でのSSH認証は
1往復である。

そして F-F の通り、`quicmux::AnyMuxConnection::open_bi()`で1本のQUIC
接続上に複数ストリームを開ける。したがって:

> **中間形: 輸送(QUIC接続 + resume)だけを holder が共有し、
> SSHセッションはタブごとに独立して持つ。**

この形なら、本ADRが扱っている問題クラスが**丸ごと消える**:
共有`Handle`が無い → 共有Mutexが無い(F-A消滅) → 1タブの詰まりが
他タブをブロックしない(F-B消滅) → `HelloAck`待ちという概念自体が
無くなる。resume継続性は輸送層にあるので維持される。

**今回は採らない**が、却下理由は「効かないから」ではなく規模である:
`isekai-pipe connect --stdio`は1プロセス1ストリームの前提で書かれて
おり、`isekai-pipe serve`側もN本のsshd接続を受ける形に広げる必要が
ある(`ISEKAI_PIPE_DESIGN.md`のEpic規模)。ctl-socket forwardも
セッションごとになる。**現行ADRが却下した「SSHセッションを
isekai-pipe側へ移す」より現実的で、かつ同じ問題クラスを解く**ので、
将来課題としてはこちらを記録すべきである(§7)。

### 5.3 輸送層状態の伝播は本当に必要か

**不要**(§1 F-A、§4 P2の比較表)。オーナーは自分が捌けるかを
ローカルで、より確実に、原因を問わず知っている。現行ADR §3.1・
§3.2の輸送層状態ファイル(新規`transport_status.rs`、JSONL、
`intent_id`引き回しのMUST、GC方針、torn read、doctor統合)は
**丸ごと不要**になる。

現行ADRの設計が唯一カバーしてP2がカバーしないのは
「輸送が30秒以上落ちているが、まだ誰もハンドシェイクを試していない
状態で、最初のクライアントが来る」ケースである。これは§4 P3の
「1番目のタブ」問題と同じものであり、その解決策としては
happy-eyeballs のほうが上位互換(退行が無く、輸送層の状態ファイルが
`Down`を書かない失敗モードでも効く)。

なお`docs/adr/0006-stun-reestablish-continuity.md`が将来より細かい輸送層状態を
必要とするなら、そのときにそのADRの文脈で設計すればよい——**本件の
ために先行して作る理由は無い**。

### 5.4 「待ち時間そのものを縮める」ことに価値はあるか

**限定的な価値しかない**、というのが結論。根拠:

- 効果の上限は60秒/191秒 = **31%**(F-D)。P2単体では30秒 = 16%。
- 短縮した先で待つのは「より遅い直接接続」である(実インシデントでは
  約46秒)。「30秒待って45秒」が「0秒で45秒」になるだけで、体感は
  2.5分→2分にしかならない。
- 一方P1は、残る169秒(88%)についても「何が起きているか」を見せる。

したがって**P2は「安いから入れる」**(~30行、既存定数のみ、
新しい失敗モード無し)という位置づけが正しく、**P3(happy-eyeballs)は
P1+P2の実測を見てから判断する**。現行ADRが輸送層状態ファイルという
新しいプロセス間プロトコルを、この31%(かつ発火するか不明)のために
導入しようとしているのは、費用対効果が合っていない。

---

## 6. 批判的検討

### 6.1 `always-connects.md`との整合

**不変条件**: P2のゲートは`HelloAck`をまだ書いていないクライアントに
しか作用しない。`relay_loop`は無改変。確立済みタブのシェルは
resume window(既定10日)の間、失われない。

**P1-bのプロトコルバージョンbump**: 旧holder + 新クライアントは
`Rejected`→直接接続へフォールバックする(F-E)。接続失敗にはならない。
旧holderは`IDLE_GRACE`で消える。

**P1-cのtee**: 表示が増えるだけで、接続の成否を一切変えない。

**F-Bへの根本対処をしていない点**: 詰まったholderがpipeのclaimを
握り続ける構造自体はP2でも残る(P2はclaimを握ったまま`Rejected`を
返すだけ)。`owner.rs:205-213`が`always-connects.md`を引いて警告した
「サーバー側の状態リーク」に厳密に対処するなら、詰まったholderは
claimを解放して次のタブに新しいholderを立てさせるべきである。
**採らない理由**: 確立済みリレーを生かしたままclaimだけ解放するには
`serve_clients`/`run_as_holder`のライフサイクル変更が要り、
「holderを2つ同時に走らせる」という新しい状態を作る。P2の`Rejected`は
同じ効果(新規タブが待たされない)を、既存のフォールバック経路を
そのまま使って得る。§7に将来課題として記録する。

### 6.2 新しい失敗モードを持ち込まないか

| 施策 | 新しい失敗モード | 評価 |
|---|---|---|
| P1-a | 表示が端末を汚す可能性 | raw mode下で`\r`+`\x1b[K`の1行更新、決着時に消去。`resume_loop.rs`が同じことを既にしている |
| P1-b | holderの子のstderrをpipeにすると、誰も読まなければ子がブロックしうる | ownerが常時drainするタスクを持つ。ownerが死ぬ=holderが死ぬなので、読み手不在の期間は生じない。broadcastのlagは表示欠落のみ |
| P1-b | `Frame::Notice`が未知のクライアントに届く | バージョンbumpで防ぐ(F-E)。`client.rs:278`の「expected HelloAck, got other」アームに落ちないよう、ハンドシェイク待ちループでNoticeを明示的に消費すること(**MUST**) |
| P1-c | 対話セッション中に進捗が混ざる | シェルが立つ前のフェーズ境界のみに限定(適用箇所をレビューで固定する) |
| P2 | `busy_since_ms`のクリア漏れ | `Drop`でクリアするRAIIガード。`Drop`は panic/cancel でも走る |
| P2 | §4 P3の退行(閾値を外す) | 定量済み。被害は「1タブが多重化を失う」のみ |

### 6.3 「もっともらしいが効果ゼロ」になっていないか(現行ADR施策Aの教訓)

各施策について、**効果が観測可能であることの確認方法**を明示する:

- **P1-a**: 実インシデントの30秒×2が`client.rs:299`の`Err(_)`アームで
  しか生成されない理由文字列を持つことから、その区間が必ずこの
  `timeout`の中であると確定できる。よって表示は必ず出る。
  検証: モックownerを無応答にした既存テスト(`client.rs:593`の
  `tokio::time::advance(HELLO_ACK_TIMEOUT + 1s)`)にstderrの
  アサーションを足すだけで直接確認できる。
- **P1-b**: holderの子のstderrが`NUL`であることは
  `holder.rs:116-117`(`Stdio::null()`)と
  `child_stdio.rs`(`Stdio::inherit()`)の組み合わせから確定。
  変更後、`Stdio::piped()`にした子のstderrがownerに届くことは
  in-processテストで直接確認できる。
- **P1-c**: `log_file.rs:280-286`の`dispatch`が
  `is_enabled()`時にfallbackを呼ばないことをコードで確認済み。
  teeを足せば端末に出る。ユニットテスト可能。
- **P2**: `busy_since_ms`が立つのは`lock_tracked()`が成功した瞬間で
  あり、`ctl_forward::request`/`open_channel`の`await`が返るまで
  クリアされない。先行クライアントが詰まる=保持され続ける、は
  **同一プロセス内の観測**なので推測が挟まらない。
  検証: モックsshdが`streamlocal_forward`に応答しない状態で
  クライアント2本を順に接続し、2本目が`channel_open_session`を
  **一度も呼ばずに**`Rejected`を受けることを直接assertできる。

### 6.4 本案が解決しないもの

- **実インシデントの72秒の無音の原因**: 特定しない(P1-cで可視化と
  記録はする)。現行ADR §1.3の留保(生ログ再確認)を引き継ぐ。
- **1番目のタブの30秒**: P2では消えない。P3を採るなら消える。
- **russhの回収不能チャネル問題**: 残る。ただしP2のゲートに
  引っかかったクライアントは`channel_open_session`を呼ばないので
  踏まない(現行ADR §3.2と同じ)。
- **Unix版との非対称性**: 変わらない。ただしP1-cは全プラットフォーム
  共通なので、可視性については非対称性が**縮まる**。

---

## 7. 実装順序

1. **P1-a**(クライアント待ちのライブ表示)。~20行、`client.rs`のみ、
   プロトコル変更なし、既存テストに追記で検証可能。
2. **P1-c**(`log_line_progress!`のtee)。~20行、`log_file.rs` +
   適用箇所の選定。直接接続経路の93秒を可視化する。
3. **P4**(認証フェーズのフェーズ境界ログ、現行ADR §3.4のまま)。
   2の適用箇所と一体で入れる。ここまでで**72秒の原因を実測する材料が
   揃う**。
4. **P2**(ローカルセンサーの入場ゲート)。~30行、`owner.rs`のみ。
5. **P1-b**(holderの子の進捗をクライアントへ配る)。プロトコル
   バージョンbumpを伴うので、1〜4が安定してから。
6. 実測後に **P3** の要否を判断する。

各段が独立に価値を持ち、1〜3は接続の挙動を一切変えないので、
「効かなかった」というリスクが構造的に存在しない。

## 8. 現行ADRとの対応

| 現行ADRの節 | 本案での扱い |
|---|---|
| §3.1 輸送層状態の公表(JSONL/`intent_id`/GC/doctor統合) | **削除**。P2がローカルで上位互換の情報を持つ(§5.3) |
| §3.2 入場ゲート((β)条件) | **位置と形は維持、センサーを差し替え**(P2)。効果は「不明」から「実インシデントの2回目を確実に消す」へ |
| §3.3 認証フェーズのタイムアウト見送り | **維持**(異論なし) |
| §3.4 フェーズ境界ログ | **維持**(P4)。P1-cにより端末にも出る |
| §3.5 Win32ネイティブ進捗表示(`SetConsoleTitleW`) | **置換**。P1-a/b/cで、既存の表示を届ける形にする。OSCタイトル所有権競合・`suppressApplicationTitle`・RAIIガードの懸念がすべて消える。クロスプラットフォームになる |
| §4 「入場ゲートの情報フレーム化((α))」の却下 | **却下理由を弱いと判断**(F-E)。ただし(α)そのものは採らず、P1-bという別形の情報フレームを採る |
| §4 「Windowsでもmuxをやめる」の却下 | **維持**。ただし中間形(輸送だけ共有)は未検討だったので§5.2に追加 |
| §5.1「情報が古い」問題の表 | **`intent_id`取り違えによる静かな無効化の行が消滅**(プロセス内なので) |
| §5.4「リンクは生きているがsshdが応答しない」を解決できない | **P2は解決する**(SSH要求が返らないこと自体を見るため) |
| §8 `Mutex`→`RwLock`化の将来課題 | **維持(実装しない)**。加えて、P2は`busy_since_ms`という形でこのMutexの保持状況を明示的な状態にするので、将来`RwLock`化を再評価する際の観測データが自然に得られる |
| §8 将来課題 | **追加**: (a) 詰まったholderがpipeのclaimを解放して新holderに譲る設計(§6.1)、(b) 輸送だけ共有しSSHセッションはタブごとに持つ中間形(§5.2、`quicmux::AnyMuxConnection::open_bi`が前提プリミティブ) |

---

## 9. 検証計画

**P1(挙動不変)**:
- (a) モックownerが無応答のとき、クライアントが2秒後から経過秒数を
  stderrへ書き、決着時に消すこと(`client.rs:593`の既存テストを拡張)。
- (b) holderの子のstderr行がownerのbroadcastに届き、
  ハンドシェイク待ち中のクライアントにも`Frame::Notice`として
  配られること(in-processで検証可能)。
- (c) `--isekai-log-file`有効時に`log_line_progress!`がファイルと
  stderrの**両方**へ書き、`log_line!`は従来通りファイルのみへ書くこと。
- (d) 旧バージョンのHelloを送るクライアントが`Rejected`を受け、
  直接接続へフォールバックすること(バージョンbumpの回帰防止)。

**P2(挙動変更)**:
- (e、MUST) 先行クライアントが`streamlocal_forward`で詰まっている間、
  経過が`HELLO_ACK_TIMEOUT`**未満**なら後続クライアントは素通しされ、
  今日通り`handle.lock()`へ進むこと(**短い断を悪化させないことの
  直接検証**——現行ADR §1.6の教訓)。
- (f、MUST) 同じく**以上**なら、後続クライアントが
  `channel_open_session`を**一度も呼ばずに**`Rejected`を受け、
  理由文字列に「多重化は次回の接続で自動的に戻る」旨が含まれ、
  かつ**確立済みクライアントのリレーが継続する**こと。
- (g) `busy_since_ms`が、ロックを保持していたタスクがpanic/cancel
  された場合も確実にクリアされること(RAIIガードの回帰防止)。
- (h) 真のチャネルオープン失敗では従来通りholderが即座に終了すること。

**反証テスト(現行ADR §6から継承)**:
- 輸送層を60秒切断→復帰させ、既存シェルが生き残ること。
- スリープ→復帰でタブが生き残ること。

**実現性**: すべて`owner.rs:1012,1024`の既存in-processモックsshd枠組み +
`tokio::time`のテストクロックで書ける。実Windows名前付きパイプも
実ネットワークも不要——`prefer-gh-actions-over-local-cargo`の制約下で
Linux CIで全て検証できる。

**実機確認**: P1投入後、まず「同じ症状が再発したとき、ユーザーが
何を見るか」を実機で確認する。P2の効果(2番目以降のタブが即座に
フォールバックする)は、意図的に輸送を落としてタブを2つ開くことで
再現できる。
