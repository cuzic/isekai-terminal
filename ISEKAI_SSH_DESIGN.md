# isekai-ssh 設計書（ドラフト・未実装）

## 位置づけ

本書は、`isekai-terminal`（Android アプリ）が持つ isekai-helper 経由の QUIC 接続耐性・NAT 越え能力を、
**Android アプリに依存しない単一バイナリの `ssh` ラッパー**として切り出せないか検討した設計ドラフトである。

前提として `PLAN.md`「Phase 7: 自作ヘルパー方式による QUIC 接続耐性」と `HELPER_PROTOCOL.md` を先に読むこと。
isekai-helper 自体の役割・配布方法・ワイヤープロトコルはここでは変更しない。**isekai-ssh はサーバー側
（isekai-helper）を一切変更せず、クライアント側の実装を Android アプリから素の `ssh` に差し替えるだけの
取り組みである。**

現状は設計提案のみで実装には着手していない。フェーズ番号は割り当てず、`PLAN.md` 本体に組み込むかどうかは
別途判断する。

**現在のステータス（2026-07-04 追記）**: 本書執筆後、`PLAN.md`「Phase 10」（isekai-terminal アプリ側に
STUN+SSHランデブー方式・MASQUE relay方式のP2P・ProxyJumpを実装）が並行して進み、本書が「将来やるべきこと」
として書いていたNAT越え機構（`isekai-stun`/`isekai-link-masque`/`h3-noq` クレート、`connect_via_jump_or_direct`
によるProxyJump、HELLO/proof/ACK・resumeクライアントロジックの共有関数化）の大部分が**既に実装済み**になった。
このため本書の「実装方針」「オープンな課題」節は、Phase 10の成果物と、それを踏まえた外部セカンドオピニオンでの
指摘（2026-07-04）を反映して更新済み。「ユーザー体験の流れ」節も、`ProxyCommand` の stdout 純粋性という
OpenSSH側の制約に合わせて、初回配布・信頼登録・JWT取得を `connect` から独立したサブコマンドへ切り出す方針に
改訂した（詳細は各該当節）。

## モチベーション

- isekai-terminal（Android）が持つ「NAT配下のサーバーに、isekai-link のリレー経由で hole punching しつつ
  直接SSHする」能力は、Android の対話端末アプリとしての価値（日本語IME、trzsz）とは独立した価値である。
- CLI 環境（開発機の `ssh`、他のSSHクライアント、CI等）からも同じ到達性の恩恵を受けたい場合、Android アプリ
  自体を経由する必要はない。OpenSSH の `ProxyCommand` に薄いバイナリを差し込むだけで、既存の `ssh` の資産
  （設定ファイル、鍵管理、エージェント転送、補完等）をそのまま使い続けられる。
- サーバー側（isekai-helper）は完全に共用できる。新規実装が必要なのはクライアント側の代替実装だけ。

## 全体像

```
$ ssh -o ProxyCommand="isekai-ssh connect myhost" user@myhost
        │
        ▼
┌──────────────────┐        outbound          ┌──────────────────────┐
│       ssh          │  stdin/stdout(生パイプ)  │      isekai-ssh        │
│  （素のOpenSSH）     │◄────────────────────────►│  （新規: 単一バイナリ）  │
└──────────────────┘                           └───────────┬──────────┘
                                                             │ QUIC (hole punching /
                                                             │ MASQUE fallback)
                                                             ▼
                                                  ┌──────────────────────┐
                                                  │   isekai-link relay    │
                                                  └───────────┬──────────┘
                                                              │
                                                              ▼
                                                  ┌──────────────────────┐
                                                  │     isekai-helper       │（無改造）
                                                  │  --target 127.0.0.1:22 │
                                                  └───────────┬──────────┘
                                                              ▼
                                                        sshd (myhost)
```

`ssh` から見ると、`isekai-ssh` は単なる「stdin に書けば相手に届き、stdout を読めば相手からの応答が来る」
プロセスでしかない。SSH の鍵交換・ユーザー認証・PTY・チャネル多重化は、isekai-terminal アプリのときと
同じく**すべて `ssh`⇔`sshd` 間で完結**し、isekai-ssh も isekai-helper もそこには一切関与しない。

## ユーザー体験の流れ

技術的な接続シーケンス（後述）とは別に、ユーザーが実際に何をして、何を目にするかを時系列で書き下す。

**方針転換（2026-07-04）**: `connect` は `ssh` の `ProxyCommand` から起動され、標準出力がそのまま
SSHバイト列として扱われる。ここに確認メッセージや OAuth ログイン案内を混ぜると、親の `ssh` からは
SSH banner の破損として観測される。そのため `connect` は **常に非対話・標準出力純粋** を徹底し、
認証（JWT取得）・初回配布・信頼登録は `connect` から独立したサブコマンド（`login`/`init`/`trust`）に
切り出す。旧ドラフトが想定していた「`connect` の中で確認プロンプトを挟んで自動配布する」設計は
撤回する（詳細は「CLIコマンド構成」節）。

### A. 初回セットアップ（デバイスごとに一度 / ホストごとに一度）

```
# デバイスごとに一度（JWT取得。Device Authorization Flowでブラウザ確認）
$ isekai-ssh login
Open https://.../device?user_code=ABCD-EFGH and confirm in your browser.
Waiting for confirmation... done. Logged in as tomoya@example.com

# ホストごとに一度（isekai-helper配布・起動・信頼登録）
$ isekai-ssh init myhost --via bastion.example.com
myhost に isekai-helper が見つかりません。
  bastion.example.com 経由で isekai-helper (musl, aarch64) を配布・起動します。よろしいですか？ [y/N] y
Deploying via bastion.example.com... done
myhost を信頼済みホストとして登録しました（~/.config/isekai-ssh/known_helpers.toml）
```

これは isekai-terminal アプリで言う「プロファイル作成」に相当する準備作業。日常的な接続のたびには
行わない。

### B/C. ホストへの接続（信頼済みホストへの日常の接続、常に同じコマンド）

`~/.ssh/config` に一度だけ書いておけば、あとは初回だろうと1000回目だろうと、ユーザーが打つのは
常に `ssh myhost` だけになる。

```
# ~/.ssh/config
Host myhost
    HostName 10.0.5.20
    ProxyCommand isekai-ssh connect myhost --via bastion.example.com
    ServerAliveInterval 30
    ServerAliveCountMax 6
    TCPKeepAlive no
```

`--via` は「relay経由で isekai-helper に届かなかった時だけ使うフォールバック経路」であり、relay接続が
機能している間は一度も参照されない。`isekai-ssh connect` は **`init` で既に信頼登録済みのホストに
対してのみ動作する**。内部ロジックは常に同じ:

```
1. myhost が trust store（~/.config/isekai-ssh/known_helpers.toml）に登録済みか確認
   → 未登録なら stdout には何も書かず、stderr にエラーを出して終了（下記「未信頼ホスト」参照）
2. relay経由でisekai-helperに到達を試みる
3. 届いた → そのまま接続（日常の9割9分はここで終わる）
4. 届かない（helperが再起動待ち等） → --via 経由で再配布・再起動する
   （既に信頼登録済みのホストなので対話確認はしない。identity key・署名が変わった場合の扱いは
   「オープンな課題」参照）
   → 再配布後、2に戻って再試行
```

**信頼済みホストへの接続（日常の大半）:**

```
$ ssh myhost
Last login: ...
user@myhost:~$
```

`connect` の標準出力はここで一切汚染されない——ログ・進捗はすべて stderr のみに出す。接続開始から
`Last login` が出るまでの体感速度は、hole punching やrelay接続が成立していれば通常のSSHとほぼ変わらない。

**未信頼・未配布ホストへの接続（`init` の実行を忘れている場合）:**

```
$ ssh myhost
isekai-ssh: helper is not installed or not trusted for myhost.
Run:
  isekai-ssh init myhost --via bastion.example.com
ssh: connect to host myhost port 22: Connection refused
```

`ssh` から見ると単なる `ProxyCommand` の異常終了であり、通常の SSH 接続失敗と同じ体験に落ちる。
`connect` がここで自動的に配布を代行しない（＝`init` を代行しない）のが、旧ドラフトからの明確な
方針転換。`PLAN.md` の既存方針「任意バイナリの自動転送・実行は opt-in、既定は無効」は変わらず
守られている——ただし opt-in の確認は `connect` の中の対話プロンプトではなく、ユーザーが明示的に
実行する独立コマンド `init` に置き換わった。

### D. ローミング中（ユーザー体験としては「何も起きない」）

ノートPCがWi-Fi⇔テザリングを切り替えても、QUICのconnection migrationが下のレイヤーで吸収するため、
タイプ中の文字が欠けたりカーソルが飛んだりしない。ユーザーが気づく手段は無い（ログを見ない限り）。

### E. 一時的な完全切断からの復帰（自動）

ラップトップをしばらくスリープさせた、電車がトンネルに入った、等でQUICコネクション自体が失われるケース。

```
user@myhost:~$ ls -la<フリーズ>
```

ここでユーザーが見るのは「反応が返ってこない」状態。裏では isekai-ssh が resume window（既定120秒＋
`ServerAliveInterval`の猶予）以内に relay 再接続 → hole punching/MASQUE → `RESUME` を試みている。
成功すれば、何事もなかったかのように出力の続きが流れ始める——**再ログインは発生しない**。

```
user@myhost:~$ ls -la
total 24
drwxr-xr-x ...
```

ユーザーへの見え方は「たまに一瞬固まる、たまに数十秒固まるSSH」であり、mosh のような専用UIは無いが、
セッションが落ちて張り直しになることはない。

### F. 復帰不能な切断（従来のSSH切断と同じ体験に落ちる）

resume window を超えて切断が続いた場合（長時間の圏外、relay自体の障害等）は、isekai-sshがあきらめて
パイプを閉じ、`ssh` は通常の切断として終了する。

```
user@myhost:~$ ls -la
client_loop: send disconnect: Broken pipe
$
```

この場合ユーザーがやることは、普段の `ssh` が切れた時と全く同じ——`ssh myhost` を打ち直すだけ。
再接続時に isekai-helper がまだ生きていれば（`--max-idle-lifetime` 内）即座に relay 経由で繋がり直す。

### G. isekai-helper の再配置が必要になったとき

isekai-helper のバージョンが古い、あるいはプロセスが完全に落ちて `--relay` モードでの常駐が切れて
いた場合も、ユーザーが打つコマンドは変わらず `ssh myhost` のまま——`isekai-ssh connect` が
「isekai-helperに到達できない」ことを検知し、B/C と同じ自動フォールバック（`--via` 経由の再配布）に
入る。`connect` は非対話が原則のため、再配布そのものは対話確認なしで進める。ただし helper の
identity key（署名鍵）が変わっているなど「信頼の実体が変わった」と判断すべきケースでは、対話
プロンプトを出す代わりに B/C の「未信頼ホスト」と同じく fail-closed でエラー終了し、
`isekai-ssh init` の再実行を促す。バイナリ version のみの変更（identity key・署名は同一）を
自動更新扱いにしてよいかは要検討（下記オープンな課題）。

`--via` の経路自体に到達できない（bastion落ち、ネットワーク不通等）場合は、さすがに自動化のしようが
無いので、明確なエラーで沈黙を避ける:

```
$ ssh myhost
isekai-ssh: myhost に isekai-helper が見つからず、再配布用の経路（bastion.example.com）にも
  到達できません。ネットワークまたは ~/.ssh/config の --via 設定を確認してください。
ssh: connect to host myhost port 22: Connection refused
```

## 責務分離

| コンポーネント | 変更 | 責務 |
|---|---|---|
| `ssh`（本物のOpenSSH） | 無改造 | 鍵交換・ユーザー認証・PTY・チャネル多重化。`ProxyCommand` の stdin/stdout を「ネットワーク」として扱う |
| `sshd`（対象ホスト上） | 無改造 | 通常の SSH サーバー。isekai-helper からの TCP 接続を、いつも通り `127.0.0.1:22` で受ける |
| `isekai-helper`（対象ホスト上） | 無改造 | isekai-link relay へ outbound 接続し、hole punching / MASQUE フォールバック含む QUIC connection を確立、HELLO/proof/ACK 後は生バイトパイプとして `127.0.0.1:22` へ中継 |
| `isekai-link relay` | 無改造（既存の外部サービス） | rendezvous・観測アドレス交換・（不成立時の）MASQUE CONNECT-UDP 中継 |
| **`isekai-ssh`（新規）** | **新規実装** | `connect`（非対話・stdout純粋: isekai-link relay への outbound 接続・HELLO/proof/ACK・resume の状態管理・`ssh` との stdin/stdout パイプ管理）と、`init`/`login`/`trust`/`logout`（対話的な配布・認証・信頼管理、`connect` とは別プロセス起動）に分離（詳細は次節「CLIコマンド構成」） |

isekai-helper に手を入れないのが本設計の核。isekai-helper の視点では、接続してくるクライアントが
Android アプリ（russh経由）なのか isekai-ssh なのかを区別する必要が無い——HELLO/proof/ACK のプロトコルは
同一だから。

## CLIコマンド構成（サブコマンド分離）と配布経路

isekai-link のリレーが解決するのは「**既に起動している** isekai-helper への到達性」であって、
isekai-helper 自体を NAT 配下のホストへ配置する経路の問題ではない。したがって配布用の経路
（多段SSH）は必ず必要だが、**「初めてそのホストを信頼するかどうかの対話確認」は `connect` の外に
出す**（`connect` は `ProxyCommand` の標準出力純粋性を守るため常に非対話でなければならない、という
方針転換については「ユーザー体験の流れ」節冒頭を参照）。

### サブコマンド一覧

| コマンド | 対話性 | 役割 |
|---|---|---|
| `isekai-ssh connect <host> [--via <jumphost>]` | **非対話・stdout純粋** | `ProxyCommand` から呼ばれる。trust store に登録済みのホストにのみ接続する。標準出力はSSHバイト列専用、ログ・進捗は全てstderr |
| `isekai-ssh init <host> [--via <jumphost>]` | 対話的 | 未知のホストへの isekai-helper 初回配布・起動・trust store への登録。`opt-in` の明示確認はここで行う |
| `isekai-ssh login` | 対話的（ブラウザ） | JWT取得（Device Authorization Flow、後述「JWT発行・配布フロー」節） |
| `isekai-ssh logout` | 非対話 | ローカルのtoken cache削除 |
| `isekai-ssh trust list` / `trust remove <host>` | 非対話 | trust store の一覧・削除 |

### `--via` フォールバックの2つの用途

- **`init` の中で使う場合（初回配布）**: 未知のホストへ isekai-helper を配布・起動する唯一の経路。
  `HELPER_PROTOCOL.md` の配布方法（既存確認 → バイナリ転送 → 起動）と同じ手順を踏む。配布時に
  isekai-helper を `--relay <endpoint> --relay-sni <name> --relay-jwt <token>`（Phase 10で実装済み、
  `HELPER_PROTOCOL.md` 参照）で起動し、inbound listen ではなく outbound 接続 + 常駐に切り替える。
  未知のホストへ初めてバイナリを転送・実行する瞬間の確認は、`PLAN.md` の既存方針（bootstrap は
  opt-in）に従いここで明示的に行う。
- **`connect` の中で使う場合（再配布）**: 既に trust store に登録済みのホストで、relay越しに
  isekai-helper へ到達できなくなった場合（プロセス再起動待ち等）だけのフォールバック。この場合は
  対話確認をしない（トラストは既に確立済みのため）。identity key が変わっている等、信頼の実体が
  変わったと判断すべきケースの扱いは「オープンな課題」参照。
- どちらの場合も、`--via` 経由での実行は `isekai-ssh` 自身が `rust-core/src/helper_bootstrap.rs`
  相当のロジックを持って完結させる（ユーザーが別途 `ssh -J ...` を手打ちする必要はない）。
- 一度 `init` が完了すれば、以降は `connect` が isekai-link relay 経由で直接繋がる。`--via` の
  経路が実際に使われるのは「`init` 実行時」と「isekai-helperが死んで再配置が要るとき」だけで、
  日常的な `connect` では一度も経由しない。

### `--via` の実装方式（確定、2026-07-04 外部レビュー第2ラウンドを反映）

**CLI版の既定バックエンドは OpenSSH 子プロセス**（`connect_via_jump_or_direct` のrussh実装は
Android版に残すが、CLI既定にはしない）。理由は、ユーザーの `~/.ssh/config`（`IdentityFile`・
`IdentityAgent`・`Include`・`Match`・`ProxyJump` 等）を自前SSHクライアント実装で再現するコストが
非常に大きいため。ただし「`ssh` 子プロセスの標準出力を雑にパースする」設計は避け、次の形にする:

- **OpenSSHは「到達」だけを担当し、状態管理はしない**。`isekai-ssh` は
  `ssh -T -o BatchMode=yes -o LogLevel=ERROR -J <via> <host> '<remoteコマンド>'` のように
  非対話モード（`BatchMode=yes`、パスワード/ホスト鍵確認プロンプトが必要な場合は認証情報や
  未知ホスト鍵の対話が発生せずそのまま失敗する）で子プロセスを起動する。
- **リモートコマンドの標準出力は、`isekai-helper` が既に出している1行JSON（`HELPER_PROTOCOL.md`
  のハンドシェイクJSON）だけ**にする。バイナリ転送（stdin経由）→ 一時ファイルへ書いてatomic
  rename → 実行権限付与 → 起動、という手順そのものは既にリモート側で完結しており、新しいパース対象を
  増やす必要は無い。人間向けのログ・警告は全てstderrへ出す設計を徹底する。
- **子プロセスの標準出力は、検証が終わるまで `isekai-ssh` 自身の標準出力へ絶対に直結しない**
  （`connect` の標準出力はSSHバイト列専用、という大原則を内部の bootstrap 用 `ssh` 呼び出しにも
  適用する）。
- 実装は `BootstrapBackend` トレイトで抽象化し、`OpenSshBackend`（CLI既定）と `RusshBackend`
  （Android版、および将来のCLI向け明示オプション・テスト用）を両方持てるようにする。既存の
  `connect_via_jump_or_direct` はそのまま `RusshBackend` として温存する（無駄にしない）。

```rust
trait BootstrapBackend {
    async fn install_and_start(
        &self,
        target: &HostSpec,
        via: Option<&JumpSpec>,
        helper: HelperArtifact,
    ) -> Result<BootstrapReport>; // HELPER_PROTOCOL.mdのハンドシェイクJSONをそのままparse
}
```

## 接続シーケンス

`init` と `connect` で完全に別フローになる（前節「CLIコマンド構成」参照）。

```
isekai-ssh init <host> --via <jumphost> の内部フロー（対話的、ホストごとに一度）

  1. --via 経由（多段SSH）で対象ホストに到達し、未知のホストなら確認プロンプトを挟む
  2. isekai-helper (musl バイナリ) を転送し、--relay <endpoint> --relay-sni <name>
     --relay-jwt <token> で起動（isekai-link relay へ outbound 接続し常駐するモード。
     HELPER_PROTOCOL.md 参照、Phase 10で実装済み）
  3. isekai-helper のハンドシェイクJSON（cert_sha256・relay_public_addr等）を確認
  4. trust store（~/.config/isekai-ssh/known_helpers.toml）にホスト・helper identity・
     バイナリhashを登録
```

```
isekai-ssh connect <host> --via <jumphost> の内部フロー（非対話・stdout純粋、日常の接続）

  0. myhost が trust store に登録済みか確認
     → 未登録なら stdout に何も書かず、stderr にエラーを出して終了（init実行を促す）

  1. relay経由でisekai-helperへの到達を試みる（isekai-terminal と共有の
     isekai-link-masque クレート、relay_public_addr へ普通にQUIC接続するだけで
     MASQUE/HTTP3/capsuleは意識しない）
     → 届けば 3 へ（日常の大半はここで完了、--via は一度も使わない）

  Fallback（0で登録済みだが、1が失敗した場合のみ＝isekai-helperの再起動待ち等）
  2. --via 経由で再配布・再起動（対話確認なし、既に信頼済みのため）
     → 1 に戻って再試行

  Main（1が成功、またはFallback後）
  3. isekai-ssh が HELLO（proof=HMAC(session_secret, exporter)）を送信
  4. isekai-helper が proof 検証 + 127.0.0.1:22 への TCP 接続確認 → ACK
  5. ACK後、isekai-ssh は QUIC stream ⇔ 自身の stdin/stdout を生パイプとして中継開始
  6. ssh（親プロセス）がこの stdin/stdout 上で通常の SSH ハンドシェイクを行う
```

**注**: 上記は既定（relay-first）の流れ。`--mode stun` 等で明示的にSTUN直結方式を選んだ場合は、
relayを経由せず `isekai_stun_p2p_transport.rs` 相当のシーケンス（STUN観測アドレス交換→simultaneous
open→QUIC）になる。STUN方式を isekai-ssh の既定にしない理由は「NAT越え方式の使い分け」節を参照。

Main の 6〜8 は、isekai-terminal アプリの `helper_quic_transport.rs` が既にやっていることと全く同じで
あり、コードの大部分は移植・共有できる（アプリ側は8の後 russh セッションを開始するが、isekai-ssh は
8の後ただの stdin/stdout パイプにするだけで、9 は `ssh` プロセス自身がやる）。

## NAT越え: public IP の伝達とシグナリングサーバーの正体

前節の「3〜5」（relayへのoutbound接続・観測アドレス交換・hole punching・MASQUEフォールバック）を、
`seera-networks/ISEKAI-link` の実装（2026-07-03 時点、`agent/src/main.rs` ・ `channel-masque/src/`）を
実際に読んで裏付けた。以下は推測ではなく、公開リポジトリのコードから直接確認した事実である
（バージョン管理されたプロトコル仕様書は無いため、実装が変わればここも追従が必要）。

### シグナリングサーバー = relay の HTTP/3 API（JWTベアラー認証）

`agent`（ISEKAI-linkの、NAT配下デバイス側プロセス。isekai-helperに相当）は、起動時に relay へ
msquic 経由の HTTP/3 接続を張り、以下の REST的なエンドポイントを順に叩く:

| エンドポイント | 役割 |
|---|---|
| `POST /create_session` | セッションを作成し `session_id` を受け取る（＝ペアリングの起点） |
| `GET /public_address` | **relay から見た自分の送信元アドレスをそのまま返す**（STUNのBinding Responseと同じ役割） |
| `GET /certificate` | relay がホスト名とTLS証明書・秘密鍵を発行する（`agent`が自前で立てるH3サーバー用） |

`/public_address` が、まさに今回訊かれた「public IP をどう伝達するか」の答え。relay は自分宛の
QUIC/HTTP3コネクションの送信元を観測し、それを聞かれたら素直に返すだけ——STUNサーバーを別に
用意する必要が無いのは、relay へのoutbound接続そのものが「自分の観測アドレスを知る手段」を
兼ねているため。

### NAT越えの実体 = `channel-masque` クレートの `MasqueClient`（WebRTC/ICEではない）

`README.md` は "Built-in WebRTC signaling" と謳っているが、実際に isekai-helper 統合の観点で重要なのは
`channel-masque::MasqueClient` が持つ2つの動作モードのうち **`MasqueClientMode::Forward(listen_addr)`**
の方であり、`MasqueClientMode::WebRTC`（実際の `webrtc` crateを使ったSDP/ICEネゴシエーション、
カメラ映像等のメディア用途）ではない。`agent/src/main.rs` の実際のシーケンス:

```
1. agent が 127.0.0.1:<ephemeral> に自前のH3/QUICサーバーを立てる
   （/certificate で受け取った証明書を使用。ここは今のisekai-helperの自己署名証明書と役割が同じ）
2. MasqueClient::start(MasqueClientMode::Forward(listen_addr)) を呼ぶ
   → 以降、この listen_addr 宛のトラフィックが、relay 経由で「外から見える形」になる
3. リモート側（isekai-sshに相当するもの）が同様に relay と通信すると、
   MasqueClientEvent::NewRemoteHost(remote_addr, mapped_remote_addr) イベントが飛んでくる
   → 「remote_addr（論理的な相手）に届けたいパケットは mapped_remote_addr 宛に送れ」という
     ローカルなマッピングが与えられる。P2Pが成立していれば直接、していなければMASQUE中継先を指す
     アドレスになる——**呼び出し側からは両者が区別されない**
```

`Forward` モードは WebRTC/ICE/SDPを一切経由しない、**素の UDP フォワーディングをNAT越え・relay
フォールバック付きで提供するだけの汎用プリミティブ**（README の「⚙️ Advanced networking (optional):
Securely access local UDP services from anywhere / Use ISEKAI Link beyond WebRTC limitations」に
対応する)。isekai-helper が乗せたい「QUICのHELLO/proof/ACK」プロトコルは、この `listen_addr` の上で
今まで通り自前でTLS終端すればよく、**channel-masqueはUDPデータグラムを右から左へ転送するだけで
中身のTLSには一切関与しない**——ここまで前節までで「blind relay」と表現していたことが、実装レベルで
裏付けられた形になる。

### isekai-helper・isekai-ssh の統合方針（更新）

前節「isekai-helper の責務」表を、この実装事実に基づいて具体化する:

- isekai-helper は、**既存の自己署名証明書によるQUICサーバーはそのまま**。追加するのは
  `channel_masque::MasqueClient::start(MasqueClientMode::Forward(listen_addr))` の呼び出しだけで、
  `listen_addr` に今の inbound listen アドレスをそのまま渡す。ICE/STUNの実装は一切書かない。
- isekai-ssh 側も対称に、relay へ接続し `NewRemoteHost` イベントで得た `mapped_remote_addr` へ
  今まで通りのQUIC接続（HELLO/proof/ACK）を張るだけ。
- **証明書について**: `agent` は relay の `/certificate` を使って自前のH3サーバー証明書を発行して
  もらっているが、isekai-helper はこれを使う必要が無い。`Forward` モードはUDPを中継するだけで
  TLSには関与しないため、isekai-helper は今まで通り**自分で生成した ephemeral な自己署名証明書**を
  使い続けられる（fingerprintをbootstrap SSH経由で渡す、というHELPER_PROTOCOL.md §2の設計は無変更）。
  relay発行の証明書を使わないことは、ゼロトラスト性（relayが鍵材料の生成に一切関与しない）の観点でも
  isekai-helper側にとって望ましい。
- **トランスポート実装の差異**: `channel-masque`／`agent` は QUICスタックとして **msquic**
  （Microsoftの実装、Rustバインディング経由）を使っている。isekai-terminal/isekai-helper は
  Phase 9-1 で `quinn` → `noq` に移行済みであり、`channel-masque` を素直に依存として取り込むと
  isekai-helper プロセス内に **msquic（relay向け）と noq（isekai-ssh向けのHELLO/proof/ACKサーバー）
  という2つのQUICスタックが同居する**ことになる。実害は無いはず（別々のUDPソケット・別目的）だが、
  バイナリサイズ・ビルド複雑度は増える。オープンな課題として記録する。

### isekai-sshでのNAT越え方式の既定（追記、2026-07-04）

Phase 10でisekai-terminal（Androidアプリ）向けに実装済みの2方式は、性質が大きく異なる:

- **STUN+SSHランデブー方式**（`isekai_stun_p2p_transport.rs`）: relay不要・低遅延だが、
  simultaneous open が不成立の場合のフォールバックが無く、**NATマッピング喪失（Wi-Fi⇔テザリング
  切替等）からの復旧も不能**という既知の制約を持つ（同ファイルのコメントに明記）。
- **MASQUE relay方式**（`isekai_link_relay_transport.rs`）: relayが常時経路に残るためNAT種別に
  依存せず動作し、resumeとの相互作用も単純（クライアント側はMASQUE/HTTP3を一切意識しない設計、
  「isekai-helper・isekai-ssh の統合方針」参照）。relayサーバー・JWTが必要。

`isekai-ssh` の主眼は「P2Pによる低遅延」よりも「`ssh` セッションをローミング・一時切断から
確実に復旧させること」（本書「モチベーション」節、`PLAN.md` の既存方針とも一致）にあるため、
**既定（`connect` が何も指定しない場合）は relay 方式（relay-first）とし、STUN方式は
`--mode stun` 等の明示オプションでのみ使う opt-in 扱いにする**。これは `CLAUDE.md` の
「実験的・opt-in の機能は既定OFFとし、使えない環境では黙ってフォールバックする日和見的設計にする」
という既存の設計原則とも整合する。

**`--mode stun` のCLI利用例（実装済み、2026-07-04 S-6）**: `isekai-ssh connect` は
`--mode <relay|stun>`（既定`relay`）と、`--mode stun`選択時に必須の`--stun-server <addr:port>`を
受け付ける。`--mode stun`を使うたびに、上記のNATマッピング喪失時の復旧不能という制約を
**`RUST_LOG`の設定に関わらず必ずstderrへ警告**する（`connect`のstdout純粋性は絶対に守る、
`log::warn!`ではなく`eprintln!`を使う理由もそこにある）:

```
$ ssh -o ProxyCommand="isekai-ssh connect myhost --mode stun --stun-server stun.example.com:3478" \
      user@myhost
isekai-ssh: --mode stun in use for 'myhost' — this session cannot recover from NAT mapping loss
(e.g. Wi-Fi<->cellular tethering roaming): unlike the default --mode relay, there is no relay
fallback path once the QUIC connection to isekai-helper is lost this way. Use the default
--mode relay if session resilience matters more than avoiding the relay hop.
Last login: ...
user@myhost:~$
```

HELLO/proof/ACKが失敗した場合（hole punching不成立、trust storeにキャッシュされた
STUN観測アドレスが陳腐化、等）のエラー文言は、relay方式（「isekai-helperが再起動した
可能性がある、`isekai-ssh init`をやり直せ」という案内）とは意図的に別文言にし、
「NAT越えが不成立だった可能性がある。`--mode relay`への切り替えを検討してください」という
案内を含める（relay方式のエラーメッセージをそのまま流用しない）:

```
isekai-ssh: HELLO/proof/ACK with isekai-helper for 'myhost' failed over STUN+SSH rendezvous
P2P (--mode stun). NAT越えが不成立だった可能性がある — this can happen when hole punching does
not succeed (e.g. symmetric NAT on either side) or the trust store's cached STUN-observed
address for isekai-helper is stale. `--mode relay`への切り替えを検討してください: re-run with
`--mode relay` (the default), which does not depend on simultaneous open succeeding. If the
cached address itself is stale, re-run `isekai-ssh init myhost` to refresh trust.
```

**trust store フィールドの読み替え**: `--mode stun`でもtrust storeのスキーマ
（`cached_relay_addr`/`cached_cert_sha256`/`cached_session_secret`、「trust store の
ファイル形式」節）は変更しない。`cached_relay_addr`は「relay-assignedアドレス」ではなく
「peer（isekai-helper）自身のSTUN観測アドレス」として読み替えて`StunP2pTarget::peer_addr`に
渡す（`cached_cert_sha256`/`cached_session_secret`はどちらの方式でも意味は同じ）。
どちらの`HelperTrust`エントリも「このisekai-helperインスタンスにどう到達し、HELLO/proof/ACKを
どう通すか」という同じ役割を担っているため、フィールド名自体を分岐させる必要はないと判断した
(`rust-core/isekai-ssh/src/connect.rs`の`resolve_stun_from_trust_store`にコメントで明記)。
このSTUN観測アドレスをどう相手に配布するか（`init`側でのアドレス交換の自動化）はS-6のスコープ外
のままで、当面は人間が把握した値を`init`実行時に手動で登録する運用を前提にする（「進め方」の
「含めないもの」参照）。

## resume を ProxyCommand の背後に隠す

これが本設計の最大の検討ポイント。`ProxyCommand` の契約は「stdin/stdoutが繋がっている間は生きている、
切れたら終わり」という単純なものであり、`ssh` 自身は QUIC の resume を知らない。しかし、resume を
**isekai-sshの内部だけで完結させれば、`ssh` からは何も切れていないように見せかけられる**。

**用語の整理（確定、2026-07-04 外部レビュー第3ラウンドを反映）**: isekai-ssh は SSH プロトコルを
一切理解しない。したがって「SSHセッションを維持する」という表現は不正確で、正しくは
**維持対象は次の3つ**である:

```text
1. ローカルの ssh プロセス ⇔ isekai-ssh の stdio（ProxyCommand の契約そのもの）
2. isekai-helper ⇔ sshd の TCP connection（isekai-helper側、無改造のまま維持）
3. isekai-ssh ⇔ isekai-helper の再接続可能な byte stream（★実際にresumeするのはここだけ）
```

resumeが保証するのは3だけであり、1と2は「3が保たれている限り結果的に切れない」という関係。

### 設計

- isekai-ssh は、QUIC connection が完全に失われても、**親プロセス（`ssh`）に対する stdin/stdout パイプを
  絶対に閉じない**。
- **オフセットは方向ごとに明示的に分ける**（旧ドラフトの4オフセットは片方向的で曖昧だったため、
  以下の命名に改める。ワイヤープロトコル自体（`session_id`+オフセット4種+リプレイバッファという
  構造）は `HELPER_PROTOCOL.md` §7 Phase 8 資産をそのまま流用でき、変更が要るのは意味づけと
  isekai-ssh側の実装だけ）:

  ```text
  C2H（client→helper、sshへの入力方向）:
    c2h_sent_offset              # isekai-ssh が送信済みの相手先端点オフセット
    c2h_helper_committed_offset  # helper が sshd への TCP write に成功した地点（source of truth）

  H2C（helper→client、sshからの出力方向）:
    h2c_sent_offset              # helper が送信済みのオフセット
    h2c_client_delivered_offset  # isekai-ssh が自身の stdout への write_all に成功した地点
                                  # （source of truth。親sshが中身をどう処理したかは関知しない）
  ```

- **C2Hのcommit境界（重要）**: helper が sshd への `write_all` に成功した時点だけを
  `c2h_helper_committed_offset` として進める（ACKをclientへ送る前に確定させる）。理由:
  「helperがclientへACKを送ってからsshdへwriteする」順序だと、ACK後にhelperが落ちた場合
  clientは送信済みと思うがsshdには届いていない、という**欠落**が起きる。逆に
  「sshdへのwrite成功をsource of truthにする」方式なら、ACKがclientに届く前に切れても、
  resume時にhelperが`c2h_helper_committed_offset`を返し、clientはそれ以前を絶対に再送しない
  ことで**二重投入**を防げる。`RESUME_ACK` には必ずこの値を含める。
- **H2Cのdelivered境界**: isekai-ssh 自身の stdout への `write_all` 成功を
  `h2c_client_delivered_offset` として進める。QUIC接続中はACK frameとして即座にhelperへ送出するが、
  **QUIC切断中はACKを送れないため、この値をローカルでpending ACKとして保持しておく**
  （確定、2026-07-04 外部レビュー第4ラウンド）。再接続時の`RESUME`に最新の
  `h2c_client_delivered_offset`を必ず含め、helperはそれ以前のH2C replayを破棄する。
- QUIC connection が切れている間、`ssh` から isekai-ssh の stdin に書き込まれるバイト（C2H方向）は
  **resume windowに余裕がある限り**読み込んでinput replay bufferに溜め続ける。isekai-helper から
  届くはずだったバイト（H2C方向）は届かないので、isekai-ssh の stdout への書き込みが単純に止まる
  （＝ `ssh` からは「応答が遅い」状態に見える）。**resume windowの上限（バイト数）に達したら
  stdinのreadを一時停止し、親sshに対してpipe backpressureをかける**（確定、2026-07-04。
  無制限に読み続けるとメモリを圧迫し、DoS/リソース枯渇の原因になるため）。
- 裏で isekai-ssh は isekai-link relay への再接続 → hole punching / MASQUE フォールバック →
  `RESUME`（`session_id` + `resume_proof` + 両方向オフセット）を試み続ける。成功したら `RESUME_ACK`
  （`c2h_helper_committed_offset` を含む）を受けて、バッファの再送を再開する。
- `ssh` は一度も EOF を受け取らないため、SSH プロトコルレベルでは何事も無かったかのように継続する。
- **`session_id` は識別子であって認証情報ではない（確定、2026-07-04）**: `RESUME` は
  `session_id`だけでなく、初回HELLO/proof確立時の秘密に紐づく`resume_proof`を必ず含める。
  helperは`session_id`と`resume_proof`の組み合わせが一致しないresumeを拒否する。これが無いと
  `session_id`の推測・漏洩によるセッション横取り（hijack）が可能になってしまう
  （`HELLO/proof/ACK`の既存の枠組みをresumeにもそのまま適用する）。
- **isekai-helper側に session table が必要**（新規要件、追記2026-07-04）: `session_id` ごとに
  sshdとのTCP connection・replay buffer・両方向オフセット・`resume_proof`・`last_seen_at` を
  保持する。`idle_timeout`・`resume_window`・`max_sessions`・`max_resume_window_bytes` 等の上限を
  設け、無制限にメモリを消費しないようにする（DoS/リソース枯渇対策、「isekai-helper 側の追加要件」
  節参照）。

### 制約: `ssh` 自身の生存確認とのレース

`ssh` は `ServerAliveInterval` / `ServerAliveCountMax` を設定していれば、自身のSSHプロトコルレベルの
keepalive（暗号化されているため isekai-ssh は中身を見られず、ただのバイト列として同じバッファに積むしか
ない）への応答が一定時間無いと、isekai-ssh 側のパイプが開いたままでも `ssh` の方から見切りをつけて
切断する。したがって：

- resume window（`isekai-helper --resume-window`、既定120秒）より、`ServerAliveInterval ×
  ServerAliveCountMax` を十分長く設定することを isekai-ssh のドキュメント・エラーメッセージで案内する。
  推奨する `~/.ssh/config` の記述例（追記、2026-07-04）:

  ```sshconfig
  Host myhost
      ProxyCommand isekai-ssh connect %h --via bastion.example.com
      ServerAliveInterval 30
      ServerAliveCountMax 6
      TCPKeepAlive no
  ```

  `ServerAliveInterval 30` × `ServerAliveCountMax 6` = 180秒の猶予。`TCPKeepAlive no` は、
  一時的な経路断でOS/中間NAT側のTCPセッションが先に死んでSSHが見切りをつける事故を避けるため
  （`ssh_config(5)` にも、TCP keepalive はネットワーク断で接続を死なせやすくなり無効化できる旨の
  記載がある）。
- `ssh_config` にデフォルト値が無い（`ServerAliveInterval 0` ＝ 無効）環境では、そもそも `ssh` 側に
  タイムアウトが存在しないため、この問題自体が起きない（＝ isekai-ssh が resume を試み続ける限り
  待ってくれる）。ただし TCP キープアライブに依存する中間NAT機器のセッションテーブルタイムアウト等、
  isekai-ssh の制御が及ばない外的要因は残る。
- isekai-ssh 自身の resume リトライにも上限を設け（`isekai-helper --resume-window` と同期させる）、
  あきらめる場合は明示的に stdin/stdout をクローズして `ssh` を正常終了させる（無限にハングさせない）。
  **実装済み（追記、2026-07-04 Phase S-4d）**: 上記の「同期させる」は `isekai-ssh connect
  --resume-window <SECS>`（既定120秒、`isekai-helper --resume-window` と同じ名前・既定値）という
  実際のCLI引数になった（従来は120秒固定のハードコード定数だった）。運用上どちらかの
  `--resume-window` を変更する場合は**両方を揃える**こと——isekai-ssh 側だけを isekai-helper 側より
  長く設定すると、isekai-helper が `sweep_expired_parked` で先にセッションを破棄した後の
  resume 試行が毎回 `REJECT_UNKNOWN_SESSION` になり、isekai-ssh 自身の「明示的にクローズして
  正常終了する」というクリーンな諦めメッセージの代わりに分かりにくい失敗になる
  （`rust-core/isekai-ssh/src/cli.rs` の `ConnectArgs::resume_window` のdocコメント参照）。
  また、`--resume-window` を既定の120秒から変更した場合は、上記の
  `ServerAliveInterval × ServerAliveCountMax` の推奨値（既定は resume window の既定120秒を
  前提にした180秒の余裕）もその新しい resume window より十分長くなるよう合わせて調整すること。

  **重要な実装上の落とし穴（追記、2026-07-04 Phase S-4d）**: 諦める際に`main`関数から
  `std::process::ExitCode`を普通に`return`するだけでは不十分だった。`connect`のC2Hポンプは
  `tokio::io::stdin()`から読み取っており、これは内部でOSのブロッキングスレッドに読み取りを
  委譲する実装になっている。このスレッドは、Tokioランタイムのシャットダウン時に「現在ブロック中の
  read呼び出しがある場合、そのスレッドはキャンセルされず、シャットダウンがそのスレッドの完了を
  待って無期限にハングしうる」（Tokio自身のドキュメントに明記）。resume windowを諦めた瞬間、
  `ssh`（ProxyCommandの親プロセス）はまだセッションが生きていると思っているため書き込み側の
  パイプを閉じておらず、`pump_c2h`の`stdin.read()`はまさにこの「ブロック中」状態にある。
  特に`ServerAliveInterval 0`（無効）の環境では`ssh`側にも自発的にパイプを閉じるタイムアウトが
  無いため、この問題を放置すると「無限にハングさせない」という本節の目的そのものが
  Tokioランタイムのシャットダウン処理によって静かに破られる。対策として、`main`関数は
  （成功・失敗を問わず）`std::process::exit()`で終了するように変更した（`rust-core/isekai-ssh/
  src/main.rs`参照）。これはOSレベルで即座にプロセスを終了させ、孤立したブロッキングスレッドの
  完了を待たない。e2eテスト（`tests/resume_window_exceeded_e2e.rs`）は、子プロセスの標準入力を
  閉じずに開いたままにする（＝実際の`ssh`が書き込み側を閉じないシナリオを模した）状態で、
  それでもハングせず正常終了することを検証している。

## isekai-helper 側の追加要件

isekai-helper の中核ロジック（HELLO/proof/ACK・`--target`中継・経路の生死判断をQUICに委ねる）は
**無改造**。isekai-ssh 対応に必要な起動オプションは **Phase 10で既に実装済み**（`HELPER_PROTOCOL.md`
参照）:

- `--relay <endpoint> --relay-sni <name> --relay-jwt <token>`: inbound listen の代わりに
  isekai-link relay（MASQUE CONNECT-UDP-bind）へ outbound 接続する起動モード。isekai-ssh は
  isekai-terminal と全く同じこのオプションをそのまま使える
- Phase 8 resume（control stream・`session_id`・`RESUME`/`RESUME_ACK`）は既に契約として存在するため
  追加実装不要。isekai-ssh 側が新しいクライアント実装としてこれを利用するだけ

**新たに残る課題（追記、2026-07-04）**: `--relay` モードでも `--max-idle-lifetime`（既定600秒）の
自己終了ロジックはそのまま有効なため、isekai-sshが目指す「日常的にrelay越しに繋がり続ける」体験には
噛み合わない場合がある。isekai-ssh の `init`/`connect` 側で長めの `--max-idle-lifetime` を明示的に
渡す運用にするか、helper側の自己終了ポリシーに手を入れるかは「オープンな課題」参照
（helper側は無改造、というスコープ上の理念とのバランス）。

**要確認（追記、2026-07-04 外部レビュー第3ラウンドを反映）**: 既存のPhase 8 resumeプロトコル
実装（`rust-core/src/resume_client.rs`、isekai-helper側の該当ロジック）が、上記「resume を
ProxyCommand の背後に隠す」節で定めたcommit/delivered境界の意味論（helperのsshd write成功を
source of truthにする等）を既に満たしているか、`session_id`ごとのsession table（TCP connection・
replay buffer・両方向オフセット・`last_seen_at`）・`max_resume_window_bytes`/`max_sessions`等の
上限が既に実装されているかは未確認。isekai-terminal（Android）は今のところ長時間の完全切断より
瞬断中心の運用だったため、isekai-sshが要求する「長時間・高頻度のresume」に対する耐性
（メモリ上限・多重セッション管理）は改めて検証・必要なら追加実装が要る。

## 実装方針（2026-07-04 改訂: Phase 10の実装済み資産・外部セカンドオピニオンを反映）

- 単一の static バイナリ（Rust、musl）として配布する。既存の `rust-core/isekai-helper/` のビルドスクリプト
  （`rust-core/scripts/build-isekai-helper-musl.sh`）と同じ手法をそのまま転用できる。
- CLI サブコマンド構成は「CLIコマンド構成（サブコマンド分離）」節の通り（`connect`/`init`/`login`/
  `logout`/`trust`）。

### 共有ロジックの crate 分割

`rust-core/src/helper_quic_transport.rs`・`resume_client.rs`・`helper_bootstrap.rs`・`transport.rs`
の中核ロジック（HELLO/proof/ACK・resumeクライアント・ProxyJump・SSH認証）はロジックとして100%
流用できる見込みだが、いずれも `tssh-core` 内で `pub(crate)` として書かれており、UniFFI・
`RusshEventHandler`・Android専用 `FaultyUdpSocket` 型に絡んでいるため、そのままでは別バイナリから
呼べない。`pub(crate)` を場当たり的に `pub` へ広げるのではなく、新しい facade を設計して境界を切り直す。

```text
rust-core/
  isekai-protocol/   # HELLO/proof/ACK, resumeフレーム, session_id, オフセット管理
  isekai-transport/  # QUIC接続確立, relay/STUN到達性, reconnect（helper_quic_transport.rs相当）
  isekai-bootstrap/  # --via 経由の配布・起動確認（helper_bootstrap.rs相当）
  isekai-auth/       # JWT取得・token cache（Device Authorization Flow, PKCE）
  isekai-trust/      # helper identity・バイナリhash・trust store
  isekai-link-masque/  # 既存（Phase 10で実装済み、無改造で共用）
  isekai-stun/         # 既存（Phase 10で実装済み、無改造で共用）
  h3-noq/              # 既存（Phase 10で実装済み、isekai-link-masqueの依存としてのみ）
  src/(tssh-core)      # Android/UniFFI向けfacade。上記crateを呼ぶだけに薄くする
  isekai-helper/       # 既存、無改造
  isekai-ssh/          # 新規CLI bin。上記crateを呼び、ACK後はstdin/stdout⇔QUICのbidirectional copy
```

`isekai-link-masque`/`isekai-stun`/`h3-noq` は独立クレートとして既に完成しており、isekai-ssh からも
無改造で使える。「ACK後にrusshへhand offする」部分だけが isekai-terminal と isekai-ssh で異なり、
isekai-ssh 側は stdin→QUIC送信方向・QUIC受信方向→stdoutの2本のcopy taskに置き換える
（`tokio::io::copy_bidirectional`は1本の双方向streamを前提とするAPIで、stdin/stdoutという
別々のハンドルには正確には対応しないため、確定 2026-07-04 外部レビュー第4ラウンド）。
S-1では片方向EOF時に反対方向をshutdownする単純実装とし、S-4ではQUIC切断中もstdioを閉じない
resume-aware実装に置き換える（次節「resume を ProxyCommand の背後に隠す」参照）。

**抽出順序（確定、2026-07-04 外部レビュー第2〜3ラウンドを反映）**: `isekai-protocol` を最初に、
**I/O・tokio・quinn/noq・russh・Android型・UniFFI型に一切依存しないpure crate**として抜く
（HELLO/proof/ACK・session_id・resumeフレーム・C2H/H2Cオフセット管理・handshake JSON schemaの型と
検証関数・codecのみ、`protocol_version`/`min_supported_version`/`features`フィールドを含む
version negotiationも最初から入れる）。これが安定すれば Android・CLI・isekai-helper・e2eテストが
同じ語彙で話せるようになる。ただし **`isekai-transport` と `isekai-bootstrap` は一気に全機能を
移植せず、最小版→拡張版の2段階に分ける**（後述フェーズ分割案の S-0d-1/S-0d-2 参照。範囲が
大きすぎると失敗時に原因（transport抽象化の失敗かSTUN移植ミスかAndroid型の混入か）を切り分け
にくくなるため）。`isekai-auth` は connect の最小疎通のブロッカーにしない（後述）。
`pub(crate)` を機械的に `pub` へ広げるのではなく、**他crateが依存してよい安定した契約になった
型・関数だけを昇格させる**。

**codecの防御的実装**: `isekai-protocol`のframe/handshake JSONデコーダは、不正長・未知
frame type・巨大frame・u64 offsetのoverflowを検出して拒否する。`cargo tree -p isekai-protocol`に
`tokio`/`quinn`/`russh`/`uniffi`が一切出ないことをS-0aの受け入れ条件にする。

**`FaultyUdpSocket`（Android専用フォルト注入ソケット）の扱い（確定）**: 中核APIをジェネリック化
（`connect_resume<S: AsyncUdpSocket>(...)`）すると、public APIがジェネリックだらけになり
UniFFIから呼びにくくなる・テスト用の抽象が本番APIを汚染する等の問題がある。代わりに、
**上位APIは非ジェネリックにし、QUICエンドポイント生成だけを狭いtrait/factoryの境界に閉じ込める**。
さらに `QuicEndpoint` 自身の責務も、resumeフレーム送受信のような protocol 層のロジックと
混ぜず、接続確立・ストリーム開設だけに限定する（確定、2026-07-04 外部レビュー第3ラウンド）:

```rust
pub trait QuicEndpointFactory: Send + Sync {
    async fn create_endpoint(&self, bind: BindSpec) -> Result<Box<dyn QuicEndpoint>>;
}

pub trait QuicEndpoint {
    async fn connect(&self, remote: RemoteSpec) -> Result<Box<dyn QuicConnection>>;
}

pub trait QuicConnection {
    async fn open_bi(&self) -> Result<Box<dyn ByteStream>>;
    async fn close(&self);
}

pub trait ByteStream: Send {
    async fn read(&mut self, buf: &mut [u8]) -> Result<usize>;
    async fn write_all(&mut self, buf: &[u8]) -> Result<()>;
    async fn shutdown(&mut self) -> Result<()>;
}
```

`send_frame`・`resume`等のprotocolロジックは`isekai-transport`/`isekai-protocol`側の上位関数が
`ByteStream`の上に実装する。`isekai-transport` はこれらtraitだけを知り、`FaultyUdpSocket` を
含む具象型を一切知らない。`SystemQuicEndpointFactory`（CLI用）・`AndroidQuicEndpointFactory`
（Android用）・`FaultInjectingEndpointFactory`（`tssh-core` のデバッグ専用モジュール、Android
専用ソケット型はここに閉じ込める。本番featureからは外れることをビルド設定で保証する）を使う側が
それぞれ用意する。

### フェーズ分割案（2026-07-04 外部レビュー第3ラウンドを反映し細分化・再順序化）

`isekai-transport`/`isekai-bootstrap`の一括移植や、resumeの一括実装は範囲が大きすぎて失敗時の
原因切り分けが難しいため、最小版→拡張版の段階に分ける。また `isekai-auth` の本実装
（Device Authorization Flow・keychain連携）は `connect`/`init`/`trust` の最小疎通のブロッカーに
しない（`TokenProvider` traitを先に切り、env/fileベースの仮実装で早期にE2Eを通す）。

| # | 内容 | 検証方法 |
|---|------|----------|
| S-0a | `isekai-protocol` crate切り出し（pure crate、version negotiation含む） | `cargo tree`にtokio/quinn/russh/uniffiが出ない。frame/handshakeのfuzzテスト |
| S-0b | `isekai-trust` crate切り出し | atomic write・0600権限検査・unknown update_policyのfail closedを含む単体テスト |
| S-0c-1 | `isekai-auth`: `TokenProvider` traitのみ切り出し、`EnvTokenProvider`/`FileTokenProvider`で仮実装 | `ISEKAI_RELAY_JWT`環境変数からのトークン取得が動くこと |
| S-0d-1 | `isekai-transport` 最小版: `QuicEndpoint`/`QuicConnection`/`ByteStream` trait分離＋relay接続のみ | `SystemQuicEndpointFactory`でCLIからrelay接続がE2Eで通る |
| S-0e-1 | `isekai-bootstrap` 最小版: `OpenSshBackend`（`BatchMode=yes`、リモートstdoutは1行handshake JSONのみ、それ以外が混ざったらfail closed） | stdout/stderr分離のテスト（後述stdout cleanliness testと連動） |
| **S-1** | `isekai-ssh connect` 最小実装（`--dev-insecure-skip-trust`等の開発専用フラグで早期E2E、本番ビルドでは`#[cfg(debug_assertions)]`等で確実に無効化） | `ssh -o ProxyCommand=...` で実sshdにログインしecho往復ができ、かつstdoutが完全にクリーンであること |
| S-2 | trust store（非対話）: 未登録ホストはfail closed、exit codeを分類 | 未登録ホストで `ssh myhost` がクリーンなエラーで終わること |
| S-3 | `isekai-ssh init`（`--via` 経由の配布・起動・trust登録、確認プロンプトの表示内容を規定） | 未配置ホストに対し `init` → `connect` の一連が通ること |
| S-0f | `tssh-core` をfacadeに整理（S-1〜S-3が動いた後の「大掃除」として実施） | UniFFI型・`FaultyUdpSocket`等のAndroid専用型が`isekai-*`crateに漏れていないこと |
| S-0c-2 / S-5 | `isekai-auth`本実装（Device Authorization Flow・keychain/Secret Service保存）＋`isekai-ssh login`/`logout` | トークン取得・失効・自動リフレッシュの単体テスト |
| S-0d-2 | `isekai-transport`拡張: STUN/P2P・reconnect/backoffポリシー追加 | `isekai_stun_p2p_transport.rs`相当のロジック移植後もSTUN単体テストが通ること |
| S-4a〜S-4d | resumeの本実装（後述「resume本実装のサブフェーズ」参照） | フォルト注入でネットワーク瞬断中も `ssh` セッションが継続すること（`phase7-5-roaming-test.sh` 相当） |
| S-X | stdout cleanliness test（独立タスク、S-1完了後いつでも追加可能） | connectの正常系・trust未登録・auth未設定・relay失敗・bootstrap失敗の全ケースでSSHペイロード以外がstdoutに1バイトも出ないことを検証 |
| S-6 | STUN方式のopt-in提供（`--mode stun`、実装済み・2026-07-04） | モックSTUNサーバー+実`isekai-helper`+実`ssh(1)`によるe2eテスト（`isekai-ssh/tests/stun_mode_e2e.rs`）でHELLO/proof/ACK・relay完走とNATマッピング喪失警告・relay方式と異なる失敗時文言を確認。実機2ネットワークでのhole punching成立/不成立確認はS-7へ持ち越し |
| S-7 | musl静的バイナリ配布・実機検証 | `build-isekai-helper-musl.sh` 転用ビルド、複数ネットワークでの実機確認 |

**最初のゲートはS-0a（`isekai-protocol`のpure crate化）**。Phase 10-0が「独自プロトコル再実装」で
未知数だったのに対し、こちらは「既存資産の切り出し境界の確定」が未知数。S-0a/S-0d-1/S-0e-1が
通ればS-1の早期E2Eに進め、以降は確立パターンをなぞる作業に近い。

### resume本実装のサブフェーズ（S-4a〜S-4d、確定 2026-07-04）

一括実装ではなく、以下の順で積む:

- **S-4a**: `isekai-protocol` に C2H/H2C オフセット型・`RESUME`/`RESUME_ACK`フレームを追加
  （「resume を ProxyCommand の背後に隠す」節の命名・意味論に合わせる）
- **S-4b**: isekai-helper に session table（`session_id`ごとのsshd TCP connection・
  `c2h_helper_committed_offset`・`h2c_sent_offset`等の保持、`idle_timeout`/`max_sessions`/
  `max_resume_window_bytes`のDoSガード）を実装
- **S-4c**: isekai-ssh側にreplay bufferと`h2c_client_delivered_offset`のACK送出を実装
- **S-4d**: resume window超過時の明示的クローズ（stdin/stdout close、helper側TCP connection
  close、非ゼロexit）とフォルト注入によるe2e検証

### trust store のファイル形式

`~/.ssh/known_hosts`（SSHホスト鍵の責務）とは分離し、`~/.config/isekai-ssh/known_helpers.toml`
に構造化データ（TOML）として保存する。保存する情報は helper identity・バイナリhash・承認日時等、
SSHホスト鍵とは異なる情報のため。XDG Base Directory の慣習に合わせ、設定は `~/.config/isekai-ssh/`、
`init` で配布した helper バイナリそのもののキャッシュは `~/.cache/isekai-ssh/helpers/sha256-<hash>/`
に置く（再配布時に「以前信頼した実体そのもの」を使い回すため。次節参照）。

**helper identity key と release signing key は別概念として扱う（確定、2026-07-04）**:
- **helper identity key**: そのリモート helper インスタンスが以前と同じ相手であることを証明する
  （`session_secret`によるproof等、既存のHELLO/proof/ACKの枠組み）
- **release signing key**: そのバイナリが isekai-helper のリリースプロセスから生成されたことを
  証明する（署名検証、Phase 10時点では未実装）

**キーの正規化（確定、2026-07-04 外部レビュー第4ラウンド）**: SSHの接続先表記には
`myhost` / `myhost:22` / `user@myhost` / IPアドレス / FQDN 等の揺れがある。trust storeの
キーは **`host:port`（ポート省略時は22に正規化、ユーザー名は含めない）** に統一する。
`--via`（jumphost）は信頼対象そのもの（helper identity）とは別軸なので正規化キーには含めず、
`last_via`として参考情報のみ記録する。

trust store のスキーマ例（MVP、ホスト単位のフラットな構造）:

```toml
[helpers."myhost:22"]
identity_pubkey = "..."
trusted_helper_sha256 = "aaa..."
trusted_helper_version = "0.3.1"
update_policy = "exact-digest-only"  # 署名検証導入後は "signed-compatible" 等へ移行
release_channel = "stable"
last_via = "bastion.example.com"
trusted_at = "2026-07-04T..."
last_seen_at = "2026-07-04T..."

# S-2実装時に追加（確定、2026-07-04）: `connect`が日常的に`--via`を経由せず
# 直接relay/isekai-helperへ接続できるのは、`init`（または直近の再配布）で得た
# ハンドシェイク情報をここにキャッシュしているため。`--relay`モードのisekai-helper
# は常駐が前提（「isekai-helper 側の追加要件」節）なので、helperが再起動しない限り
# このキャッシュは有効であり続ける。helperが再起動して`session_secret`が変わった
# 場合、キャッシュを使ったconnectはHELLO/proofの検証に失敗する（isekai-helperが
# ACKで拒否する）——この失敗が「`--via`経由の再配布が必要」というシグナルになる
# （「CLIコマンド構成」節の`connect`内フォールバックの契機）。
cached_relay_addr = "203.0.113.10:45231"
cached_cert_sha256 = "3a7f..."
cached_session_secret = "MDEyMzQ1Njc4OTAxMjM0NTY3ODkwMTIzNDU2Nzg5MDE="

[release_keys]
# 署名検証導入後に使う。Phase 10時点では未設定（＝update_policyは常にexact-digest-only）
stable = "..."
```

**将来の拡張候補（MVPでは不要、記録として残す）**: 同一の helper identity が複数のホスト名・
alias・relay経路から見える可能性があるため、`host` と `helper identity` を分離した
`[hosts."myhost"] -> helper_id` / `[helpers."<helper_id>"]` という二段構成にする案がある。
最初から凝る必要は無く、MVPは上記フラット構造で十分。

### JWT発行・配布フロー

CLI専用ユーザー向けに **Device Authorization Flow**（`isekai-ssh login`、ブラウザで一度だけ認可）を
既定とする。トークンは可能な限りOSのkeychain/Secret Serviceに保存し、フォールバックとして
`0600` 権限のファイルストアを用意する。`connect` 実行中のトークン失効は裏で自動リフレッシュを試み、
リフレッシュも失敗した場合のみ stderr に `isekai-ssh login` の再実行を促して終了する
（`connect` の中でブラウザログインを開始することはしない）。

### S-7実施結果: muslビルド手順・配布方法（2026-07-04）

**ビルド手順**: `rust-core/isekai-helper/`向けの`rust-core/scripts/build-isekai-helper-musl.sh`
（Phase 7-2・`cargo-zigbuild`でzigをCクロスコンパイラ/リンカに使い、musl-gcc等のシステム
トゥールチェーンを不要にする手法）をそのまま転用し、`rust-core/scripts/build-isekai-ssh-musl.sh`
を新設した。`cargo zigbuild --release -p isekai-ssh --target <triple>`を`x86_64-unknown-linux-musl`/
`aarch64-unknown-linux-musl`の両方に対して実行するだけの構造で、helper版との唯一の違いは対象
crateだけである。**配布用ビルドでは`dev-insecure` feature（`--dev-insecure-*`系フラグを有効化する
開発専用の信頼ストアバイパス、「実装方針」節参照）を明示的に有効化しない**（デフォルトfeatureの
みでビルドする）ことをスクリプト冒頭のコメントに明記した。

**実施内容と結果（このセッションのサンドボックス環境で実際に実行・確認済み）**:

- `cargo zigbuild --release -p isekai-ssh --target x86_64-unknown-linux-musl` /
  `--target aarch64-unknown-linux-musl` の両方がビルド成功（各約2分10秒）。
- `file`コマンドで両バイナリとも `ELF 64-bit LSB executable, ..., statically linked, stripped`
  であることを確認（動的リンクライブラリへの依存が無いことの確認）。
- x86_64バイナリ（5,797,520 bytes）はこの環境で実際に実行でき、`--help`（トップレベル・
  `connect --help`・`init --help`・`login --help`・`logout --help`全て）に`--dev-insecure-*`系
  フラグが一切出現しないことを`grep`で確認した。これは既存の`isekai-ssh/tests/help_purity.rs`が
  検証している不変条件と同じであり、実際に`cargo test -p isekai-ssh --test help_purity`
  （デフォルトfeatureのみ、`--dev-insecure`無し）を実行し
  `release_build_connect_help_never_mentions_dev_insecure_flags ... ok`のpassを確認した。
- aarch64バイナリ（5,003,464 bytes）はこの環境（x86_64ホスト）では実行できないため`file`による
  静的リンク確認のみ（クロスビルド自体の成否確認）。
- 両バイナリのsha256を`sha256sum`で記録（`.sha256`ファイルとしてビルド成果物に同梱、
  helper版と同じ運用）。

**配布方法**: `isekai-ssh`はサーバー側常駐の`isekai-helper`とは異なり、**利用者本人が手元の
`~/.ssh/config`に`ProxyCommand isekai-ssh connect %h`として置く個人用CLIツール**である。
Phase 7-6の`isekai-helper`向けLinuxbrew tap（`cuzic/homebrew-isekai-terminal`）のような
配布インフラを新規に構築するほどの利用規模ではまだ無いと判断し、本タスクでは以下の
軽量な配布に留める:

- GitHub Release（`cuzic/isekai-terminal`）に`isekai-ssh-v<version>`のような形で
  x86_64/aarch64両musl静的バイナリと`.sha256`ファイルを添付する。
- ユーザーは該当アーキテクチャのバイナリを手動ダウンロード→sha256照合→`chmod +x`→
  `$PATH`の通ったディレクトリ（例: `~/.local/bin/`）に配置→`~/.ssh/config`に
  `ProxyCommand`として設定する、という手順を踏む（Homebrew Formula等の追加の抽象化層は導入しない）。
- 配布アセットのsha256は**アップロード後に実際のアセットから再計算した値**を使うこと
  （Phase 7-6で判明した「ビルド直後に記録した値の使い回しは危険（workspace内の別クレート再
  ビルドの影響で非決定的に再リンクされることがある）」という教訓をここでも踏襲する）。
- Homebrew tap等のパッケージマネージャー経由配布は、利用者数が増え「バージョン管理・
  アップデート通知」のニーズが顕在化した時点で改めて検討する（現時点で先回りして構築しない）。

**実機ネットワーク検証**: このサンドボックス環境（単一マシン、実ネットワークデバイス無し）では
実行不可能なため、`PLAN.md`側に別途記録した（「実機・実relay検証が未実施」の項も参照）。

## オープンな課題（2026-07-04 改訂）

### Phase 10の実装・外部レビューにより解決済みになった項目（記録として残す）

- ~~msquicとnoqの二重スタック問題~~: `isekai-link-masque` が `channel-masque`/msquicに依存せず、
  `h3-noq` 上に独自実装したことで解消（noq一本化を維持できている）
- ~~channel-masqueレベルの再マッピングとresumeの相互作用~~: クライアント側（isekai-terminal /
  isekai-ssh）がMASQUE/再マッピングを一切意識しない設計に落とし込んだため、構造的にほぼ解消
  （relayとのトンネル再確立は isekai-helper 側だけの問題になった）
- ~~channel-masqueのライセンス~~: 外部依存として組み込まず独自実装したため、この懸念自体が消滅
- ~~永続化ファイル形式~~: TOML構造化データ（`~/.config/isekai-ssh/known_helpers.toml`、
  `~/.ssh/known_hosts` とは分離）に決定（「実装方針」節参照）
- ~~JWT発行方式~~: Device Authorization Flowを既定に決定。ただしリフレッシュ実装の詳細は下記で継続検討
- ~~`--via` フォールバックの実装方式~~: CLI既定は OpenSSH 子プロセス（`BatchMode=yes`）に決定。
  状態管理はリモート側の `isekai-helper` が既に出すハンドシェイクJSON（`HELPER_PROTOCOL.md`）を
  そのまま使うため、新たなstdoutパース対象を増やす必要は無い。既存のrussh実装
  （`connect_via_jump_or_direct`）は `BootstrapBackend` トレイトの一実装として温存し、Android版
  および将来のCLI向け明示オプション・テストで使う（「`--via` の実装方式」節参照）
- ~~S-0の共有crate境界での `FaultyUdpSocket` の扱い~~: 中核APIをジェネリック化せず、
  `QuicEndpointFactory` という狭いtrait境界に閉じ込めることに決定（「実装方針」節参照）。
  `FaultyUdpSocket` は `tssh-core` のデバッグ専用モジュールの外に出ない
- ~~再配布時のバイナリバージョン変更の扱い~~: A/B二択ではなく三段階ポリシーに決定
  （「同一digestの再配布は自動」「署名済みdigestへの更新は自動（署名検証は未実装、将来対応）」
  「未署名の異なるdigestへの変更はfail closed」）。署名検証導入前の暫定策として、`init` で
  配布したバイナリそのものをローカルにキャッシュし（`~/.cache/isekai-ssh/helpers/sha256-<hash>/`）、
  `connect` からの自動再配布は「以前信頼した実体と同一のバイナリ」に限定する
  （`update_policy = "exact-digest-only"`、「trust store のファイル形式」節参照）

### 引き続き未決の項目

- **署名検証の導入**: リリースプロセスからの正当なバイナリであることを証明する release signing key
  の導入（manifestへのEd25519署名等）は未着手。導入後、`update_policy` を
  `"signed-compatible"` 等へ移行する設計だけは決めてある（「trust store のファイル形式」節）が、
  鍵管理・配布・ローテーションの運用は未設計。
- **`isekai-helper --relay` の常駐と `--max-idle-lifetime`（既定600秒）の整合**: isekai-sshが目指す
  「日常的にrelay越しに繋がり続ける」体験には噛み合わない場合がある。isekai-ssh側から長めの値を
  明示的に渡す運用にするか、helper側の自己終了ポリシーに手を入れるか（後者は「サーバー側無改造」の
  理念とのバランスが要る）。
- **isekai-terminal（Android）アプリと isekai-ssh（CLI）間の設定・認証情報の共有**: プロファイル
  （ホスト・relayエンドポイント）やJWT/token cacheを共有する手段が今のところ無い。両者は独立した
  trust store・token cacheを持つ前提で進めるか、将来的に統合するかは未検討。
- **リフレッシュトークンの保存場所**: OSのkeychain/Secret Service優先、フォールバックとして
  `0600` ファイルストアという方針までは決まったが、各OS（Linux/macOS/将来のWindows）でのkeychain
  連携ライブラリの選定は未着手。
- **実機・実relay検証が未実施**: STUNの実hole punching・実 `seera-networks` relay相手の疎通確認、
  および複数ネットワーク環境（宅内Wi-Fi NAT配下ホスト・モバイル回線クライアント）をまたいだ
  実機ローミング確認は引き続き未実施（モックrelayでのプロトコルレベル検証・単一サンドボックス
  環境でのmuslビルド実行確認のみ完了、S-7実施結果・`PLAN.md`参照）。Phase 10-5と同じ理由
  （実ネットワークデバイスを持つ実機がこのセッションで利用できない）で、次回実機が用意できる
  環境でのフォローアップが必要。
- **配布対象プラットフォーム**: 当面はLinux（x86_64/aarch64、musl静的バイナリ）を優先し、
  macOS/Windows対応の要否・時期は未検討。
- **`init` の `BatchMode` 方針**: `connect` はfail closed前提で常に`BatchMode=yes`（非対話）とするが、
  `init`（対象ホストへの初回SSH接続）でパスワード認証や未知ホスト鍵の確認が必要な場合、
  `BatchMode=yes`だとその対話自体が失敗してしまう。A案（`init`も「既に非対話SSHが通る状態」を
  前提にする、実装は単純）とB案（`init`だけOpenSSHの対話を許す、UX的には自然だが実装が複雑）の
  どちらにするかは未確定。MVPはA案から始める想定。
- **既存Phase 8 resume実装のcommit/DoS耐性の検証**: 「isekai-helper 側の追加要件」節の
  「要確認」参照。isekai-terminalの既存利用パターン（瞬断中心）と、isekai-sshが要求する
  「長時間・高頻度のresume」に対する耐性のギャップを検証する必要がある。

## 参照

- `PLAN.md`「Phase 7: 自作ヘルパー方式による QUIC 接続耐性」「Phase 8: resume プロトコル契約」
  「Phase 10: 多段SSH依存からのP2P移行（ProxyJump・STUN方式・relay方式）」
- `HELPER_PROTOCOL.md`（isekai-helper の CLI/ワイヤープロトコル契約。特に §7 Phase 8 resume、
  Phase 10 の `--stun-server`/`--relay` 等）
- `seera-networks/ISEKAI-link`: https://github.com/seera-networks/ISEKAI-link
  - `agent/src/main.rs`（NAT配下デバイス側プロセス。isekai-helper統合の参考実装として最も近い）
  - `channel-masque/src/lib.rs` / `channel-masque/src/masque/mod.rs`（`MasqueClient`・
    `MasqueClientMode::Forward`・`MasqueClientEvent`）
  - `webrtc-app/`（ブラウザ向けWebSocketシグナリング側。`agent`とは別経路で同じrelayに繋がる）
  - 閲覧日: 2026-07-03。バージョン管理されたプロトコル仕様書は無く、実装依存の記述である点に注意
- `seera-networks/axum-masque-rs`（`bound-udp-server`・`axum-masque`）: Phase 10-0で実際のワイヤー
  契約（capsuleプロトコル・ヘッダ）をソースから確定。閲覧日: Phase 10実施時（本書「現在のステータス」参照）
- RFC 9298（MASQUE、`CONNECT-UDP`）
- RFC 9297（HTTP/3 Datagram、MASQUEの前提）
- RFC 8628（OAuth 2.0 Device Authorization Grant。`isekai-ssh login` のDevice Authorization Flow）
- RFC 9000 §9（QUIC Connection Migration）
- OpenSSH `ssh_config(5)`: `ProxyCommand`, `ProxyJump`, `ServerAliveInterval`, `ServerAliveCountMax`,
  `TCPKeepAlive`, `StrictHostKeyChecking`, `BatchMode`, `Include`, `Match`
- Minisign（Ed25519ベースのシンプルなファイル署名・検証）、Sigstore/cosign: 将来の
  release signing key導入時の実装候補として比較検討（「オープンな課題」参照）
- XDG Base Directory Specification（設定/状態/キャッシュの配置規約、trust store・token cacheの配置に採用）
- Auth0 ドキュメント: "Secure a CLI with Auth0"（Device Authorization Flow）、
  "Authorization Code Flow with PKCE"、"Refresh Token Rotation"
- 本書「実装方針」「オープンな課題」節の2026-07-04改訂は、社内検討に加え外部セカンドオピニオン
  （ProxyCommandのstdout純粋性・trust storeの分離設計等の指摘）を反映したもの
