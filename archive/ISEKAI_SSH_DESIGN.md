# isekai-ssh 設計書（ドラフト・未実装）

> **ARCHIVED（2026-07-07）**: 本書はアーカイブ済みの歴史的記録。現在の設計・実装状況は
> `ISEKAI_PIPE_DESIGN.md` を参照すること。本書の多くの提案（`connect`/`init`/`login`/`trust`の
> 4サブコマンド構成、`--mode stun`等）はその後の実装で方針が変わっている
> （`isekai-ssh connect`サブコマンドは削除され、非サブコマンド呼び出しのwrapperモードに
> 統合された）。resumeのオフセット意味論・NAT越え調査等、変わっていない部分も多いため
> 参考資料として保持する。

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

Main の 6〜8 は、isekai-terminal アプリの `isekai_pipe_quic_transport.rs` が既にやっていることと全く同じで
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
- H2C方向も同じく lossless な bounded buffer として扱う。helper 側の output buffer が満杯になったら
  `--target` の TCP socket から読まず、`APP_ACK` で `h2c_client_delivered_offset` が進んで空きが戻るまで
  TCP backpressure をかける（2026-07-07 実装済み）。古い未確認データを eviction して
  `REJECT_OFFSET_GONE` に落とす方式は採らない。
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

**解決済み（2026-07-04 改訂）**: `--relay` モードでも `--max-idle-lifetime`（既定600秒）の自己終了
ロジックはそのまま有効で、これはisekai-sshが目指す「日常的にrelay越しに繋がり続ける」体験には
本来噛み合わない（`init`は一度きりの対話的デプロイ、`connect`は数時間〜数日空けて何度も同じ
稼働中helperにダイヤルし直すだけのモデルなので、10分放置しただけでhelperが自己終了してしまうと
毎回`init`をやり直す羽目になる）。**isekai-helper側は無改造のまま**、`isekai-ssh init`が
`isekai-bootstrap::RelayLaunchSpec::idle_lifetime_secs`（新設、必須フィールド）を経由して
`--max-idle-lifetime`を明示的に指定する形で解決した。CLIには`isekai-ssh init --idle-lifetime <SECS>`
（既定30日 = 2,592,000秒）を新設。`isekai-terminal-core`（Android、`rust-core/src/helper_bootstrap.rs`）は
接続の度に新しいhelperを再デプロイするモデルのため元々この引数を渡しておらず、isekai-helper自身の
既定値（600秒）で変わらず動作する——両者は同じ`--max-idle-lifetime`という既存フラグを使い分けて
いるだけで、isekai-helper側のコード・既定値には一切手を入れていない。
`isekai-bootstrap/tests/openssh_e2e.rs::install_and_start_passes_idle_lifetime_to_the_launched_helper`
で、実際に起動されたisekai-helperのargvに`--max-idle-lifetime 2592000`が渡ることを確認済み。

**確認状況（2026-07-07 更新）**: 既存のPhase 8 resumeプロトコル
実装（`rust-core/src/resume_client.rs`、isekai-helper側の該当ロジック）が、上記「resume を
ProxyCommand の背後に隠す」節で定めたcommit/delivered境界の意味論（helperのsshd write成功を
source of truthにする等）を満たすかを見直し、isekai-helper側は `--max-sessions` と
`--resume-buffer-size`、H2C output buffer 満杯時の TCP read pause/backpressure を実装済み。
Android 側の `rust-core/src/resume_client.rs` については別途同じ観点で確認が必要。

## 実装方針（2026-07-04 改訂: Phase 10の実装済み資産・外部セカンドオピニオンを反映）

- 単一の static バイナリ（Rust、musl）として配布する。既存の `rust-core/isekai-helper/` のビルドスクリプト
  （`rust-core/scripts/build-isekai-helper-musl.sh`）と同じ手法をそのまま転用できる。
- CLI サブコマンド構成は「CLIコマンド構成（サブコマンド分離）」節の通り（`connect`/`init`/`login`/
  `logout`/`trust`）。

### 共有ロジックの crate 分割

`rust-core/src/isekai_pipe_quic_transport.rs`・`resume_client.rs`・`helper_bootstrap.rs`・`transport.rs`
の中核ロジック（HELLO/proof/ACK・resumeクライアント・ProxyJump・SSH認証）はロジックとして100%
流用できる見込みだが、いずれも `isekai-terminal-core` 内で `pub(crate)` として書かれており、UniFFI・
`RusshEventHandler`・Android専用 `FaultyUdpSocket` 型に絡んでいるため、そのままでは別バイナリから
呼べない。`pub(crate)` を場当たり的に `pub` へ広げるのではなく、新しい facade を設計して境界を切り直す。

```text
rust-core/
  isekai-protocol/   # HELLO/proof/ACK, resumeフレーム, session_id, オフセット管理
  isekai-transport/  # QUIC接続確立, relay/STUN到達性, reconnect（isekai_pipe_quic_transport.rs相当）
  isekai-bootstrap/  # --via 経由の配布・起動確認（helper_bootstrap.rs相当）
  isekai-auth/       # JWT取得・token cache（Device Authorization Flow, PKCE）
  isekai-trust/      # helper identity・バイナリhash・trust store
  isekai-link-masque/  # 既存（Phase 10で実装済み、無改造で共用）
  isekai-stun/         # 既存（Phase 10で実装済み、無改造で共用）
  h3-noq/              # 既存（Phase 10で実装済み、isekai-link-masqueの依存としてのみ）
  src/(isekai-terminal-core)      # Android/UniFFI向けfacade。上記crateを呼ぶだけに薄くする
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
（Android用）・`FaultInjectingEndpointFactory`（`isekai-terminal-core` のデバッグ専用モジュール、Android
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
| S-0f | `isekai-terminal-core` をfacadeに整理（S-1〜S-3が動いた後の「大掃除」として実施） | UniFFI型・`FaultyUdpSocket`等のAndroid専用型が`isekai-*`crateに漏れていないこと |
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
  `--resume-buffer-size`のDoSガード、H2C buffer 満杯時の TCP backpressure）を実装
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

## 複数isekai-sshプロセスによるisekai-pipe共有（マルチプレクス設計、優先度低・2026-07-07判断、記録として保持）

### 2026-07-07: SSH接続プーリングによる代替が判明、本節の設計は優先度を下げる

本節が最初に検討していた「1つの共有isekai-pipeを複数のプロセス/タブが多重化して使う」という
目的自体は、**isekai-pipe（QUIC transport）層ではなく、その1層上のSSHプロトコル層で、より
単純に達成できる**ことが判明した。以下、経緯と結論を記録する。

**気づきの根拠**: `rust-core/src/transport.rs`の`run_ssh_channel_loop`を確認したところ、1本の
認証済み`russh::client::Handle`に対して`channel_open_session()`を呼ぶ構造になっている。SSH
プロトコル自体が、1本のトランスポート接続（1回の鍵交換・1回のユーザー認証）の上に複数の
独立したチャネル（`session`/`direct-tcpip`等）を多重化できる設計であり（RFC 4254）、これは
まさにOpenSSHの`ControlMaster`が裏で行っていることそのものである。`russh`もクライアント
ライブラリとしてこの機能をそのまま提供している。

**したがって**: Androidアプリ（`rust-core`が`russh`をin-processで直接組み込んでいる）は、
CLIの`ControlMaster`と対称的な仕組みを、**isekai-pipeのwireプロトコルを一切変更せずに**
実現できる。

```text
CLI（本物のsshバイナリ経由）:
  ControlMaster/ControlPersist が、1本のSSH接続を複数の`ssh`プロセスで共有する
  （OSプロセスをまたいだ制御ソケット経由）

Android（russh をin-processで直接組み込み）:
  orchestrator（Rust側、SSOT原則）が、ホスト/プロファイルごとに1本の認証済み
  `client::Handle`をプールし、新しいタブは`client::connect()`からではなく
  既存Handleへの`channel_open_session()`から始める
  （同一プロセス内で完結するのでIPCも不要）
```

**`isekai-helper`/`HELPER_PROTOCOL.md`への変更は一切不要**: `isekai-helper`は元々「1 QUIC
streamを1本のTCP接続へ生バイト列で中継するだけ」で、その中身がSSHチャネル的に何本に分かれて
いようと関知しない。SSHチャネルの多重化はTCP接続の中身（SSHバイナリプロトコル）の話であり、
isekai-pipeのトランスポート層より上のレイヤーで完結する。

**resumeとの相性も良い**: isekai-pipeのresumeは「1本の連続したバイトストリーム」を対象に
動作し、その中に複数のSSHチャネルが多重化されていることを意識しない。複数タブが1本のSSH
接続を共有していても、isekai-pipeレベルのresumeが成功すれば、多重化されていた全SSHチャネルの
状態はSSH自身の枠組みでそのまま維持される。本節が以下で定義していた
「`connection_epoch`によるfencing」「channel単位のresume」のような**isekai-pipe層での
複雑な状態管理は不要になる**。

**結論**: 本節が定義していたQUIC層でのマルチプレクス設計（識別子の4階層・`isekai-muxd`・
helperワイヤープロトコルv2等、以下に記録）は、**「多くのタブ/コマンドを同じホストへ開く」
という当初の主要ユースケースに対しては不要**と判断する。CLI側は前々節の通り
`ControlMaster`/`ControlPersist`、Android側は新設する**SSH接続プーリング**（`rust-core`内、
`SessionOrchestrator`が担う想定。プールのキー設計・アイドル時の生存期間・タブ終了時の扱い等は
未設計で、別途詳細設計が必要）で、isekai-pipeのwireプロトコル変更なしに同じ効果が得られる。

以下に記録する詳細設計（識別子分離・helperワイヤープロトコルv2・resume意味論等）は、**「意図的に
別々のSSH接続として分離したい」という狭いユースケース**（別ユーザー/別鍵での接続、意図的な
障害分離、等）が具体的に必要になった場合の設計候補として保持するが、当面は実装しない。

---

### 位置づけ・モチベーション（当初の動機、参考として保持）

現行の`HELPER_PROTOCOL.md`は「1 QUIC connection（isekai-pipeと呼ぶ） = 1 data stream = 1
TCPセッション（1 sshログイン）」という完全な1対1構造になっている。これは`PLAN.md`のPhase 7
時点で「将来ポートフォワードや複数チャネルに拡張する場合は、1 QUIC connectionに複数streamを
多重化し各streamを別宛先にマッピングする形に拡張できる」と既に見込まれていたが、Phase 7〜10
のスコープでは単一streamのまま据え置いていた。

`isekai-ssh connect <host>`は`ssh`の`ProxyCommand`から**呼び出しごとに新しいOSプロセス**として
起動されるため、同じホストに複数の`ssh`セッションを張る従来のユースケース（複数ターミナルタブ、
`scp`/`sftp`の並行実行、`git`のssh越し操作、多段ProxyJump等）では、`isekai-ssh connect`プロセスが
その数だけ独立に起動し、それぞれが独立にNAT越え（hole punching/relay）・QUIC接続確立・
resume可能セッションを持ってしまっている。本節は、**1つの共有isekai-pipe（1本のQUIC接続、1つの
resumeグループ）を複数の`isekai-ssh connect`プロセスが多重化して使う**設計を定める。

**本節が対象にする範囲の確定（2026-07-07）**: 本アプリケーションは現時点で実利用者がいないため、
`HELPER_PROTOCOL.md`が既に持っている「破壊的変更は将来ALPNを`isekai-helper/2`にし旧clientは
自然にネゴシエーション失敗させる」という後方互換の仕組みは、本設計の導入にあたっては**使わない**。
ワイヤーフォーマットを直接書き換えてよく、version negotiation・旧client向けフォールバック分岐は
実装しない（将来的に実利用者が付いた後の破壊的変更では、改めてバージョニングが必要になる）。

### 全体アーキテクチャ: リモート多重化層とローカル多重化層を分離する

次の二層に明確に分離する。**リモート多重化層はAndroid/CLI共通、ローカル多重化層はCLI固有**とする。

```text
Android
  Terminal Tab A ─┐
  Terminal Tab B ─┼─> in-process PipeSession ─> 1 QUIC connection
  File Transfer  ─┘

CLI
  ssh process A -> isekai-ssh connect A ─┐
  scp process   -> isekai-ssh connect B ─┼─ UDS ─> isekai-ssh muxd (isekai-muxd)
  git process   -> isekai-ssh connect C ─┘             │
                                                        └─> PipeSession
                                                             │
                                                             └─> 1 QUIC connection
```

- **リモート多重化層**（`isekai-pipe-core`/`isekai-protocol`/`isekai-transport`側）: 1本のQUIC接続上に
  複数の独立したTCP/SSHバイトストリーム（channel）を載せる。Android版はin-processで直接
  `PipeSession`を共有し、ローカルIPCは経由しない。
- **ローカル多重化層**（CLI固有、新規crate/バイナリ`isekai-muxd`相当）: 複数の`isekai-ssh connect`
  プロセスから1つの`PipeSession`を利用するためのIPCブローカー。Android側には対応する層が無い
  （Kotlin側は元々1プロセス内で完結しており、OSプロセスをまたぐ共有問題が発生しないため）。

この切り分けは、「実装方針」節の共有ロジックcrate分割方針（`isekai-protocol`/`isekai-transport`/
`isekai-bootstrap`をAndroid/CLI非依存のpure crateとして切り出す）とそのまま整合する。
`isekai-muxd`はこの並びに「CLI固有のローカルIPCブローカー」として追加される。

### 識別子を4階層に分ける

現状の問題の根本は、`session_secret`・resumeセッション・QUIC接続・TCPセッションがほぼ同じ単位に
なっていることである。次のように分離する。

| 識別子 | 寿命・意味 |
|---|---|
| `session_secret` | 認証・権限のスコープ。helperに接続する資格（従来通り、helper起動ごとに1つ） |
| `pipe_session_id` | 複数channelを束ねる論理的なresumeグループ（従来の`session_id`を昇格させたもの） |
| `connection_epoch` | 現在有効なQUIC接続の世代。split-brain防止用（新設） |
| `channel_id` | 1本の論理TCP接続。つまり1つの独立したSSHログイン（新設、client生成の128bitランダム値） |
| QUIC `stream_id` | 物理QUIC接続内だけで有効な一時的識別子（quinnが管理、アプリ層では意識しない） |

**重要**: `channel_id`とQUICの`stream_id`を同一視しない。QUIC接続を張り直すとQUIC streamは
すべて新しくなるが、`channel_id`は維持される。

```text
session_secret
  └── pipe_session_id A
        ├── channel_id 1 -> sshd TCP connection 1
        ├── channel_id 2 -> sshd TCP connection 2
        └── channel_id 3 -> sshd TCP connection 3
```

`HELPER_PROTOCOL.md` §7の`session_id`は、意味的にはそのまま`pipe_session_id`へ読み替えて拡張する
（`RESUME`/`RESUME_ACK`フレームの`session_id`フィールドは名前・型を変えずに済む）。

### コンポーネント構成

```text
isekai-protocol
  ワイヤー上の型、ChannelId、PipeSessionId、ConnectionEpoch、offset、codec

isekai-pipe-core / isekai-transport
  QUIC接続、NAT traversal、relay fallback、pipe-level resume、
  channel open/resume、replay buffer、channel table

isekai-muxd（新規、CLI専用）
  CLI向けローカルIPCブローカー。PipeSessionを1つ所有し、
  複数のProxyCommandプロセスを受け付ける

isekai-ssh connect
  stdin/stdoutとローカルIPCを中継する薄いshim

rust-core / Android
  isekai-pipe-core/isekai-transportを直接in-processで利用（ローカルIPCなし）
```

`isekai-muxd`は独立バイナリにせず、配布を簡単にするため`isekai-ssh muxd --internal ...`という
非公開サブコマンドとして同一バイナリに含める（既存の`isekai-ssh`単一バイナリ配布方針を踏襲）。

### マスター選出方式

**最初の`connect`プロセス自身をマスターにしない**（`ControlMaster=auto`と似たUXにはするが、
最初のProxyCommandプロセスがQUIC接続を所有し続ける設計は避ける）。最初のプロセスは次の処理だけ
行う:

```text
1. 既存のmux daemonへの接続を試す
2. 存在しなければpipe-key用ロックを取得する
3. ロック取得後、もう一度socketへ接続する（クラッシュ後にstaleなsocketが残っている場合に対処）
4. まだ不在ならstale socketを削除し、専用mux daemonをspawnする
5. readiness pipeで起動完了を待つ
6. ロックを解放し、daemonへ接続する
7. 自分は通常のProxyCommand shimとして動く
```

これにより、最初のSSHセッションが終了しても、他のSSHセッションや共有QUIC接続の寿命に影響しない
（OpenSSHの`ControlPersist`が最初のセッション終了後もマスター接続をバックグラウンドで維持する
モデルと同じ狙い）。

ソケットは`$XDG_RUNTIME_DIR/isekai-ssh/<pipe-key>.sock`（+ 同ディレクトリの`.lock`）に置き、
ディレクトリを`0700`、ソケットを`0600`相当にする（既存のtrust store・handshakeファイルの権限
契約と同じ方針）。

**`pipe-key`に含めるもの**: 単純なホスト名だけでは不十分。helperの識別・relayポリシー・
ネットワークbind方針等が異なれば別daemonに分離しなければならない。

```text
PipeCompatibilityKey {
    helper_identity_fingerprint,
    auth_profile_id,
    helper_endpoint_set,
    relay_policy,
    network_binding_policy,
    protocol_major_version,
    trust_policy_hash,
    target_service_scope,
}
```

これをcanonical serializationしBLAKE3等でハッシュしたものをキーにする。`session_secret`自体は
ソケットパス・コマンドライン・環境変数には含めず、daemonが安全な設定ストアから読むか、spawn時に
匿名pipe/継承FD経由で渡す（既存の「`session_secret`はCLI引数や環境変数に載せない」設計原則
（`HELPER_PROTOCOL.md`）をローカルIPCにもそのまま適用する）。

### mux daemonのライフサイクル

```text
Starting → Connecting → Active ⇄ Resuming → IdlePersist → Draining → Stopped
```

- アクティブなローカルchannelが1本以上ある間は終了しない
- 最後のchannelが閉じたら`persist_timeout`を開始し、persist中に新しいchannelが来ればタイマーを
  取り消す。既定`--persist 10m`（`--persist 0`で即終了、`--persist forever`も許容）
- 管理コマンド: `isekai-ssh mux status/drain/stop [--force] <host>`。`drain`は新規channelだけ拒否し
  既存channelがゼロになるまで待つ

### マスタークラッシュ時の扱い（初版）

**初版ではプロセスフェイルオーバーを実装しない**。mux daemonがクラッシュした場合、全ローカルIPCが
切断→全`isekai-ssh connect`がエラー終了→親の`ssh`も切断→次回接続時に新しいdaemonが起動、という
扱いにする。これは「ネットワーク切断からのresume」と「プロセス障害からのresume」を意図的に
分離する判断である。QUIC接続状態・stream・再送状態はユーザー空間のオブジェクトであり、UDP
socketのFDだけを別プロセスへ渡してもQUIC接続状態そのものは引き継げない。プロセスフェイルオーバー
まで実現するには、各ProxyCommand shim側にも`channel_id`・方向別offset・未ACKバッファ・新daemonへの
再アタッチ処理を持たせる必要があり、ローカルIPC自体がresumeプロトコルになってしまう。これは
必要性が確認できてから（後述フェーズ分割のM5）検討する。

### ローカルIPCプロトコル

1ローカル接続（1本のUDS）= 1論理channelとする。ローカルIPCまで1ソケット上に多重化する必要はない
（各`isekai-ssh connect`プロセスが1本ずつUDSを開き、それが1つの`channel_id`に対応する）。

完全なraw streamにはせず、軽量なlength-prefixed framingを使う（raw byte modeにすると、SSHの
データとmux daemonのエラー通知を同じstreamに載せられなくなるため）:

```text
CLIENT_HELLO { ipc_version, pipe_key, options_hash, client_pid }
OPEN { request_id, target_service, traffic_class }
OPENED { channel_id }
DATA { bytes }        # 16〜64KiB程度が上限
FIN
RESET { code }
STATUS { state }
ERROR { code, message }
```

`ssh`のstdin EOFはTCP接続全体の即時終了とは限らないため、half-closeを維持する:
`stdin EOF → IPC FIN(C2H) → QUIC channel send FIN → helperがtarget TCPのwrite側をshutdown`
（逆方向は引き続き受信可能）。バックプレッシャーはchannelごとのbounded queue・pipe全体のbuffer
上限を設け、QUIC send windowが詰まればIPC readを止め、ローカルstdoutが詰まればQUIC receive消費を
止める形でUDSとQUICを連動させる。

### helper側ワイヤープロトコル v2

```text
1 QUIC connection
  ├── 1 control stream
  ├── bidi stream -> channel_id A -> TCP connection A -> sshd
  ├── bidi stream -> channel_id B -> TCP connection B -> sshd
  └── bidi stream -> channel_id C -> TCP connection C -> sshd
```

`max_concurrent_bidi_streams`は「control stream 1本 + channel数上限」まで引き上げる（`--max-sessions`
に相当するchannel数上限、`--max-channels-per-pipe`のような新設フラグで制御する）。control streamは
接続内で1本だけ許可し、重複したcontrol streamはprotocol errorとする。

control streamのフレーム: `PIPE_HELLO` / `PIPE_ACCEPTED` / `RESUME_PIPE` / `PIPE_RESUMED` /
`CHANNEL_ACK` / `CHANNEL_LOST` / `GOAWAY` / `PING` / `PONG`。

各data streamの先頭ヘッダー（新規channel）:

```text
CHANNEL_OPEN { channel_id, request_id, connection_epoch, target_service, traffic_class }
```

resume時:

```text
CHANNEL_RESUME { channel_id, connection_epoch,
                 client_h2c_committed_offset, client_c2h_last_known_helper_commit }
```

helperの応答:

```text
CHANNEL_ACCEPTED { channel_id, helper_c2h_committed_offset, helper_h2c_committed_offset }
```

`channel_id`はclient生成（128bitランダム値）とし、helper側は次の規則で冪等に扱う: 同じ
`channel_id`かつ同じtargetなら既存channelとして返す、同じ`channel_id`だがtargetが異なれば
protocol error、未知の`channel_id`なら新規TCP接続を作る（ネットワーク断による`CHANNEL_OPEN`の
再送に対する冪等性）。`request_id`も併せて持たせ、リクエスト単位の重複判定・ログ追跡に使う。

### helper側セッションテーブル

```rust
struct PipeSession {
    pipe_session_id: PipeSessionId,
    resume_token_hash: ResumeTokenHash,
    active_epoch: ConnectionEpoch,
    active_connection: Option<ConnectionHandle>,
    channels: HashMap<ChannelId, ChannelState>,
    expires_at: Instant,
}

struct ChannelState {
    target_service: ServiceName,
    tcp: TcpConnection,
    c2h_committed: u64,
    h2c_committed: u64,
    c2h_replay_state: ReplayState,
    h2c_replay_buffer: ReplayBuffer,
    local_half_closed: bool,
    remote_half_closed: bool,
    state: ChannelLifecycle,
}
```

`session_secret`のテーブルに直接TCP状態を置くのではなく、`session_secret(認証scope) → PipeSession
→ ChannelState[]`という階層にする。

### resumeの意味論: pipe単位で再接続し、channel単位で復元

「全streamを一括resume」か「個別resume」かの二者択一ではなく、二段階にする。

**第1段階（pipe resume）**: 新しいQUIC接続を確立し`RESUME_PIPE { pipe_session_id, resume_proof,
previous_epoch }`を送る。helperは`PIPE_RESUMED { new_connection_epoch, retained_channel_ids }`を
返す。この時点で旧QUIC接続はfenceされる。

**第2段階（channel resume）**: 各`channel_id`について新しいQUIC bidi streamを開き
`CHANNEL_RESUME(channel_id, offsets...)`を送る。各channelは独立に
`resumed`/`target TCP already closed`/`retention expired`/`replay buffer overflow`/
`unknown channel`のいずれかになる。**1 channelのresume失敗でpipe全体を落としてはいけない。**

### `connection_epoch`によるfencing（split-brain防止）

共有接続では、旧QUIC接続が一時的に通信不能になった後に新QUIC接続でresumeが成立し、その後で
旧接続が復活する、というケースが起こり得る。helperは`frame.connection_epoch != pipe.active_epoch`
のフレームを`reject_as_stale()`する。新しいresumeを受理した時点で旧接続をclose・旧接続からの
channel ACKを無視・旧streamを全て無効化する。QUIC connection migration（RFC 9000 §9）は同一QUIC
接続の経路変更には有効だが、QUIC接続そのものが失われた場合は新接続上でアプリケーション層が
streamを再構築する必要があり、`connection_epoch`はその再構築時の整合性を保証する。

### direction別offsetはchannelごとに管理する

各channelに`c2h_committed`（helperがtarget TCPへのwriteを完了した累計バイト数）・
`h2c_committed`（clientがローカルIPCへのwriteを完了した累計バイト数）を持たせる。ACKは
`CHANNEL_ACK { channel_id, direction, committed_offset }`をcontrol stream上で送り複数ACKを
coalesceする。resume時は「helperの`c2h_committed`以降をclientが再送」「clientの`h2c_committed`
以降をhelperが再送」という従来通りの意味論を、pipe全体ではなく必ず`channel_id`ごとに適用する。

### Androidでも共通プロトコルを使う

リモートプロトコルは最初からAndroid/CLI共通にする。Android側はローカルIPCを使わず
`let channel = pipe_pool.get_or_connect(pipe_key).await?.open_channel("ssh").await?;`のように
in-processで共有する。これによりAndroidでも、タブごとのNAT traversal・relay接続・resumeを
1本のpipeにまとめられる（NAT越え・handshakeの反復コストを削減できる）。CLI固有で共通化しない
処理は、daemon起動・UDS・lock file・peer credential確認・persist timer・管理コマンドのみ
（「ワイヤープロトコルとPipeSession実装は共通、プロセス共有だけCLI固有」という境界）。

### 共有QUIC接続の注意点

**SSH自体は多重化されない**: 各`channel_id`は独立したTCP接続なので、sshdから見ると従来通り
複数接続であり、SSH鍵交換・host key検証・ユーザー認証・SSHチャネル確立は毎回行われる。削減
できるのは主にhelper起動・発見・hole punching・relay確立・QUIC/TLS接続確立・resume transportの
確立であり、OpenSSHの`ControlMaster`（1つのSSHネットワーク接続上に複数SSHセッションを共有する）
とは共有するレイヤーが異なる（本設計は複数の独立SSH接続を1つのQUIC transportに載せる）。

**bulk転送によるfairness**: 1つのQUIC接続ではstreamごとのflow controlに加え、接続全体のflow
controlと輻輳制御が共有される。対話シェル・`scp`の大容量転送・`git clone`を同じpipeに載せると、
bulk転送が対話操作のレイテンシへ影響し得る。本プロジェクトはtrzszファイル転送が中核機能の
一つであり、この懸念は無視できない。初期実装（後述M1）から少なくとも次を入れる: channelごとの
buffer上限、全channelを公平にpollする、1タスクが大量writeを独占しない、
`TrafficClass::Interactive/Bulk/Background`をプロトコル上予約、`max_channels_per_pipe`。

### 独自セッションプロトコル案の検討結果とControlMaster活用方針（確定、2026-07-07）

前節までの検討と並行して、「ブートストラップ（`init`）は今まで通り本物のOpenSSHでよいが、2回目
以降の日常的な接続（現状はSSHバイト列のブラインドリレー、いわば『P2P SSH』）はSSHプロトコルへの
依存を捨てて独自のセッションプロトコルに置き換えてもよいのではないか」という案（QUIC自体が
TLSで暗号化・認証しているため、その上でさらにSSH自体の鍵交換・認証をやる必要はないはず、という
発想。`mosh`が初回だけSSHを使い以降は独自の低レイヤープロトコルでセッションを維持するモデルに
近い）を検討した。外部セカンドオピニオン（ChatGPT、2026-07-07）を踏まえ、**この案は採用せず、
代わりにOpenSSH自身の`ControlMaster`/`ControlPersist`（SSH接続の使い回し）を積極的に使う
オプションを提供する方針に決定した**。

**却下した理由**:
- 独自プロトコルへの置き換えは、現在`sshd`が無償で提供しているPTY割り当て・exec・ユーザー認証・
  シグナル転送・環境変数受け渡しを、サーバー側に新設する「isekai独自セッションデーモン」で
  自前実装し直すことを意味する。これはトランスポート耐性の改善という現行スコープを大きく超え、
  SSHプロトコルそのものの再実装に近い規模になる（`isekai-helper`を意図的に「賢いことをしない」
  設計にした`PLAN.md`Phase 7の原則そのものへの反例になってしまう）。
- 独自プロトコルに切り替えると、SSHエコシステムとの互換性（ssh-agent forwarding、X11
  forwarding、SFTPサブシステム、他ツール(git/rsync/ansible等)からの`ProxyJump`越しの相互運用、
  `~/.ssh/config`の豊富なオプション、PAM/2要素認証との連携）を失う。
- 当初の動機（「2回目以降、SSHの鍵交換・認証をやり直したくない」）は、**OpenSSH自体の
  `ControlMaster`/`ControlPersist`だけで既に達成できる**。最初の`ssh`（`ProxyCommand isekai-ssh
  connect`経由）が制御ソケットを持つマスターになれば、2本目以降の`ssh`/`scp`/`sftp`/`git`は
  SSHの鍵交換・認証すら行わず、SSHプロトコル層でマスターの接続に相乗りする。独自プロトコルを
  新設する主要な動機がこれで解消される。

**前節「複数isekai-sshプロセスによるisekai-pipe共有」への影響（重要）**: `ControlMaster`/
`ControlPersist`が有効な場合、同一ホストへの2本目以降の`ssh`系コマンドは`ProxyCommand`自体を
呼び出さない（マスターが既に確立した接続に相乗りするため）。つまり、CLI側で`isekai-ssh
connect`プロセスが実際に起動するのは常に**マスター1本だけ**になり、前節が解決しようとしていた
「タブ/コマンドの数だけisekai-pipeが独立に張られ、その数だけhole punching/relay接続が走る」
という問題は、**M4（`isekai-muxd`によるローカルIPC多重化）を実装しなくても`ControlMaster`だけで
実用上ほぼ解消する**。

Androidアプリは本物の`ssh`バイナリを使わず`russh`を直接組み込んでいるため、`ControlMaster`
という機構自体は存在しない。しかし前節冒頭「SSH接続プーリングによる代替」で整理した通り、
`russh`も1本のSSH接続上に複数チャネルを多重化できるため、`rust-core`内に同種のプーリング層
（ホスト/プロファイルごとに1本の`client::Handle`を保持し、新しいタブは`channel_open_session()`
から始める）を持たせるだけで同じ効果を得られる。したがって、前節が定義していたM0〜M3
（wireプロトコルの`pipe_session_id`/`channel_id`拡張・Androidのin-process共有）も、CLI側のM4と
合わせて優先度を下げる（詳細・結論は前節冒頭を参照）。

**推奨する`~/.ssh/config`（`ControlMaster`/`ControlPersist`を積極利用する場合）**:

```sshconfig
Host myhost
    HostName 10.0.5.20
    ProxyCommand isekai-ssh connect %h --via bastion.example.com
    ServerAliveInterval 30
    ServerAliveCountMax 6
    TCPKeepAlive no
    ControlMaster auto
    ControlPersist 10m
    ControlPath ~/.ssh/isekai-ssh-cm-%r@%h:%p
```

- `ControlPath`は他のホスト向け設定と衝突しないよう専用のテンプレートを使う（isekai-ssh経由の
  接続だけが対象であることを明示するため、既存の`ssh_config`のControlPath運用と混ぜない）。
- `ControlPersist`の値は、前述の`ServerAliveInterval × ServerAliveCountMax`やisekai-helper側の
  `--resume-window`と独立した設定である点に注意（マスター接続自体がresumeで維持され続ける限り、
  `ControlPersist`の期限が来てもマスターは生きているセッションがあれば終了しない——OpenSSHの
  仕様通り、`ControlPersist`はアイドル時間の上限であり、アクティブなチャネルがある間は無視される）。
- `isekai-ssh`側の実装変更は不要（`ControlMaster`はOpenSSH側だけで完結する機能のため）。
  `isekai-ssh init`が生成する設定テンプレート例に上記3行を含めるかどうかは、UXの検討課題として
  残す（既定でONにするか、opt-inのドキュメント案内に留めるか）。

### 実装フェーズ（2026-07-07: 全体を低優先度に格下げ、本節冒頭参照）

破壊的変更の後方互換を考慮しなくてよい（本節冒頭「対象範囲の確定」）ため、Phase分割は
「一気に検証しきれない範囲を段階的に検証する」ための開発上の都合であり、旧clientとの互換維持は
目的に含めない。

**2026-07-07時点でM0〜M5は全て低優先度**（本節冒頭「SSH接続プーリングによる代替」参照）。
「多くのタブ/コマンドを同じホストへ開く」という主要ユースケースは、CLI側の`ControlMaster`/
`ControlPersist`とAndroid側のSSH接続プーリング（未設計、`SessionOrchestrator`に持たせる想定）で
isekai-pipeのwireプロトコル変更なしに解決できるため。以下は「意図的に別々のSSH接続として
分離したい」狭いユースケースが具体化した場合の設計候補として残す。

| # | 内容 | 受け入れ条件 |
|---|------|------|
| M0 | 識別子分離: `session_id`を`pipe_session_id`として扱う、`channel_id`追加、offset tableをchannel単位に、`connection_epoch`追加。動作は1接続1channelのまま | `max_channels_per_pipe=1`で既存テストが全て通る |
| M1 | helperの複数channel対応: 1 control stream + N bidi data stream、channel table、channel単位OPEN/CLOSE/ACK、channelごとにTCP接続生成（resumeなしでよい）。traffic class予約とbuffer上限もここで入れる | 複数channelを同時に開き、片方の切断が他方に影響しないことをe2eで確認 |
| M2 | pipe resume + channel resume: pipe-level fencing、connection epoch、channel別offset reconciliation、channel別replay、1channel失敗の隔離 | フォルト注入で1channelのresume失敗が他channelのセッション継続に影響しないことを確認 |
| M3 | Androidのin-process共有: `PipePool`、タブから`open_channel`、接続共有ポリシー、max channel/buffer/fairness | 複数タブでのNAT traversal・relay確立が1回に減ることを確認 |
| M4 | CLI mux daemon: local IPC、daemon election、persist、status/drain/stop、stale socket recovery | 複数`ssh`プロセスが同一daemon経由で同時にログインできることをe2eで確認 |
| M5 | 耐障害性強化（必要性が確認できた場合のみ）: mux daemon再起動後のshim再アタッチ、offset付きローカルIPC、resume capabilityの安全な引継ぎ、daemon state journal | （未定、M0〜M4完了後に要否を判断） |

### 障害時の挙動一覧

| 障害 | 挙動 |
|---|---|
| Wi-Fi→5G切替 | QUIC migrationで継続 |
| QUIC接続完全消失 | pipe resume後、各channelを再アタッチ |
| 1本のtarget TCP終了 | そのchannelだけEOF |
| 1本のbuffer overflow | そのchannelだけ`CHANNEL_LOST` |
| helper再起動 | 全pipe/channel失効 |
| mux daemonクラッシュ | 初版では全CLIセッション切断（前述「マスタークラッシュ時の扱い」） |
| 1つのProxyCommand shimクラッシュ | 対応channelだけreset |
| 最後のchannel終了 | persist timer開始 |
| resume中に新規OPEN | resume完了まで待機、またはbounded queue |

### オープンな課題（本節固有）

- traffic class（interactive/bulk/background）の具体的なスケジューリングアルゴリズム未設計
  （M1着手時に決める。ただしM1自体が低優先度、上記参照）
- `--max-channels-per-pipe`の既定値・QUIC transport側の`max_concurrent_bidi_streams`上限との整合
  未検討
- mux daemonのUDS peer credential確認（同一UID以外からの接続拒否）の具体的な実装（`SO_PEERCRED`
  相当）未着手
- M5（mux daemon耐障害性）の要否は、M0〜M4の実運用で「mux daemonクラッシュによる全滅」が
  実際にどの程度の頻度で問題になるかを見てから判断する

### 新しいオープンな課題: Android向けSSH接続プーリングの詳細設計（2026-07-07、未着手）

本節冒頭の判断により、当面はこちらが実際に必要になる設計。まだ何も決まっていない:

- プールのキー設計: ホスト名だけでなく、`username`・認証方式（鍵/パスワード）・
  `ConnectionProfile`のどの範囲まで一致すれば同一プールとみなすか（`isekai-ssh`側の
  `PipeCompatibilityKey`と同様の考慮が必要）
- ライフサイクル: 最後のタブ（channel）が閉じた後、`client::Handle`を即座に閉じるか、
  `ControlPersist`相当のアイドル時間だけ保持するか。保持する場合、`SessionOrchestrator`
  （`ConnPhase`等、`.claude/rules/rust-ssot.md`参照）にこの状態をどう持たせるか
  （Kotlin側にミラー状態を作らない、という既存原則をそのまま適用する）
  - 接続プロファイル削除・変更時にプール中のHandleをどう扱うか
- 障害時の挙動: プールされた1本のSSH接続を複数タブが共有している状態で、1タブのchannelが
  異常終了した場合に他タブへ影響しないことの確認（SSHプロトコル上はチャネルごとに独立した
  flow controlを持つはずだが、`russh`の実装レベルでの確認が必要）
- 既存の`TerminalTabsViewModel`/`TerminalSession`（1タブ=1接続を前提にしている可能性がある）
  との統合方法。「Rust側が意思決定ロジックを持ち、Kotlin側はイベント転送のみ」という
  `.claude/rules/rust-ssot.md`の原則に従い、プーリングの可否判断も`SessionOrchestrator`に
  持たせる想定だが、既存コードの実態を確認してから設計する必要がある

### 2026-07-07: 上記オープンな課題の調査・設計確定

前節の4項目について、現行コードの調査（タブ⇔接続の対応関係の実地確認）を踏まえて設計を確定する。
実装はまだ行わない（実装は本ドキュメント外の別タスクで行う）。

#### 前提調査で判明した事実

- `TerminalTabsViewModel.openTab()`はタブ生成のたびに無条件で新規`TerminalSession`
  （ひいては新規`SessionOrchestrator`・新規`ActiveSession`）を生成しており、「1タブ=1回の
  `client::connect()`+新規transport」という前提は現状**完全に成り立っている**。複数タブが
  接続を共有する仕組みはどこにも無い。
- `run_ssh_channel_loop`（`transport.rs`）は認証成功後に`channel_open_session()`を**1回だけ**
  呼ぶ。同一`client::Handle`に対する複数回の`channel_open_session()`呼び出しはプロダクション
  コードに存在しない。
- **重要な追加の事実（本節冒頭の判断時点では未確認だった）**: `TransportPreference::PLAIN_SSH`
  以外の全トランスポート（`TSSHD_QUIC`/`ISEKAI_PIPE_QUIC`/`AUTO`/`ISEKAI_PIPE_QUIC_MULTIPATH`/
  `ISEKAI_STUN_P2P_QUIC`/`ISEKAI_LINK_RELAY_QUIC`）では、実際のシェル用`client::Handle`は
  生TCP上ではなく、**タブごとに個別確立されるisekai-pipe QUIC data streamの上**に
  `client::connect_stream()`で張られる(`isekai_pipe_quic_transport.rs::run_over_stream`)。
  そのQUIC接続自体もタブごとに独立しており（`bootstrap_helper_via_ssh`によるヘルパー起動用
  SSHログイン→ヘルパー起動→QUIC接続確立→QUIC上でのSSHネスト認証、という一連の手順をタブごとに
  フルで実行する）、resume/multipath状態（`IsekaiPipeQuicSession`/
  `MultipathIsekaiPipeQuicSession`等、`PathBroker`を含む）もタブごとに個別に保持している。

  つまり、SSHチャネル多重化（`channel_open_session()`の使い回し）だけでは、QUIC系トランスポート
  について「ヘルパー起動SSH」「ヘルパー起動そのもの」「QUIC接続確立」「QUIC接続のresume/
  multipath状態機械」という**より高コストな部分**の重複は解消されない。これらを共有するには、
  SSH接続プーリングと同じ考え方（＝認証済みの状態を複数タブで使い回す）を、QUIC接続確立・
  そのQUIC上でのネストしたSSH認証にも適用する必要がある。単純に見えて実は難しくない理由は
  後述（「QUIC系トランスポート(isekai-pipeファミリー)への拡張」節）。

#### スコープの決定

**v1のSSH接続プーリングは`TransportPreference::PLAIN_SSH`と、isekai-pipe QUICファミリー
（`ISEKAI_PIPE_QUIC`/`AUTO`/`ISEKAI_PIPE_QUIC_MULTIPATH`/`ISEKAI_STUN_P2P_QUIC`/
`ISEKAI_LINK_RELAY_QUIC`）の両方を対象とする。** 当初（本節初版）はQUIC系を「resume/multipath
状態機械に踏み込む大きい変更」として除外していたが、以下の理由で覆した:

- isekai-pipeのwireプロトコルは現状「1 QUIC connection = 1 data stream = 1 TCPセッション」
  という1対1構造しか持たない（`ISEKAI_PIPE_DESIGN.md` §6.3）。この制約により、QUIC接続を
  複数タブで共有する場合、必然的に「その1本のdata streamの上のネストしたSSH `client::Handle`」
  も一緒に共有することになる——QUIC層だけを共有してタブごとに別々のネストしたSSH認証を
  やり直す、という中間的な設計（複数data streamを1 QUIC接続に多重化する新しいwire
  プロトコルv2が必要になる）は選択肢にならない。
- 結果として、「QUIC接続確立＋ネストしたSSH認証」という一連の処理全体を、プレーンSSHの
  `client::Handle`プーリングと**全く同じパターン**（1個のプールエントリに対して複数タブが
  `channel_open_session()`するだけ）でまとめて扱える。resume/multipath状態機械そのものを
  分割・複製する必要は無く、「これまでタブ単位だった所有権を、プールエントリ単位の所有権に
  移す」だけで済む。wireプロトコルの変更も不要（本節冒頭の2026-07-07判断の方針と一致）。
- QUIC接続確立（ヘルパー起動SSH＋ヘルパー起動確認＋QUICハンドシェイク＋ネストしたSSH認証）は
  プレーンSSHのTCP接続よりも明らかにコストが高く、複数タブで共有できた場合の効果はむしろ
  プレーンSSHより大きい。

**`TSSHD_QUIC`（Phase 5、isekai-pipeとは別の外部`tsshd`デーモン経由）は本節では詳細設計しない。**
構造的には同じパターン（タブごとに個別のQUICエンドポイントへ接続し、その上にネストした
`client::Handle`を張る、`quic_transport.rs`）が使えるはずだが、Phase 7で「tsshd非依存」の
isekai-pipe方式に主軸が移った経緯（`PLAN.md`参照）を踏まえ、優先度を下げて「今後の課題」に
記録するに留める。

#### プールのキー設計（プレーンSSH／共通コア）

対象がプレーンSSHのみになったことで、キーは「同一の認証済み`client::Handle`が得られるか」を
決めるSSH固有の識別子だけで構成できる。`ConnectionProfile`のIDのような Kotlin/Room 側の
不透明な外部キーには依存しない（Rust側は`SshConfig`に既に流れてくるSSH関連フィールドだけで
判断でき、Rust SSOT原則とも整合する）。

```rust
struct SshPoolKey {
    host: String,
    port: u16,
    username: String,
    /// 認証方式ごとの識別子。パスワード認証は常に `None`（後述、プール対象外）。
    /// 公開鍵認証は鍵のフィンガープリント（`PrivateKey::public_key().fingerprint(HashAlg::Sha256)`、
    /// `agent_forward.rs:143`で既に使っているAPIと同じもの）。
    auth_identity: Option<String>,
    /// SSH agent forwarding の有無。プールキーに含める理由は後述。
    agent_forward: bool,
    /// 踏み台がある場合、踏み台側も同じ形で再帰的にキーへ含める
    /// （踏み台Handleの共有まではせず、あくまで対象ホストへの経路が同一になるかの判定に使う）。
    jump: Option<Box<SshPoolKey>>,
}
```

各フィールドの根拠:

- **`host`/`port`/`username`**: 自明。OpenSSHの`ControlPath`既定値(`%C` = host/port/userのハッシュ)
  と同じ考え方。
- **`auth_identity`**:
  - パスワード認証（`SshAuth::Password`）は**常にプール対象外**とする（`auth_identity`が
    無い＝新規プロファイルキーが作れない、という形で表現するのではなく、プール検索自体を
    スキップする実装にする）。パスワードは実行時に手入力される値であり、平文比較のための
    保持期間を延ばしたくない（タスク#65のゼロ化方針と相性が悪い）。加えて「2回入力した
    パスワードが同じかどうか」は接続の同一性の判断根拠として弱く、無理に対応する価値が薄い。
  - 公開鍵認証（`SshAuth::PublicKey`）は鍵のSHA256フィンガープリントを使う。同じ`keyId`
    （Room側で選ばれた鍵）を指していれば当然一致するが、フィンガープリントそのものを比較
    することで「Rust側は鍵のバイト列だけを見て判断する」という閉じた設計にできる
    （`ConnectionProfile.keyId`というKotlin/Room由来の値をRust側の判断に持ち込まない）。
  - **異なる鍵・異なるユーザーで同じホストへ接続する2タブは、意図的に別Handleのままにする**
    （本節冒頭が「意図的に別々のSSH接続として分離したいユースケース」として当面スコープ外に
    した領域と同じ理由: 別ID/別鍵での接続は明示的に分離されているべきで、黙って共有すると
    権限混同の驚きになる）。
- **`agent_forward`**: SSH agent forwardingの転送先鍵(`RusshEventHandler.agent_key`)は
  `client::Handle`単位（`run_ssh_channel_loop`が認証成功後に一度だけセットする）であり、
  チャネル単位ではない。もしプールキーに含めず、`agent_forward=true`のタブが先に
  Handleを確立した後で`agent_forward=false`の別タブが同じHandleにアタッチした場合、
  Handle自体は「agent forwarding可能な状態」のまま残る（後者のタブ自身は
  `channel.agent_forward(true)`を送らないため実害は無いはずだが、この「はず」を実装時に
  検証するコストより、キーに含めて完全に分離してしまう方が単純で安全）。よってキーに含める。
- **`jump`**: 踏み台経由の接続は踏み台側の認証情報も一致して初めて「同じ経路」と言えるため、
  同じ構造を再帰的に適用する。踏み台Handle自体のプーリング（`jump_handle`の共有）は本節の
  範囲外とする（対象ホストへの接続確立が完了すれば`_jump_handle`は保持されたままトンネルを
  維持するだけの役目になり、それ自体を複数の対象ホスト接続で使い回す設計は追加の複雑さの
  割に効果が薄い。踏み台1本につき対象ホストへの接続が1つだけのケースが大半と想定される）。
- キーに**含めない**もの: `cols`/`rows`（チャネルごとのPTYサイズ、SSHプロトコル上チャネル
  ごとに独立して指定できる）・`forwards`（初期ポートフォワード、チャネル確立後に
  コマンドチャネル経由で投入するだけなので後から追加しても問題ない）・
  `allow_non_loopback_forward_bind`（forward要求ごとのポリシーチェックであり接続確立とは
  無関係）。

#### QUIC系トランスポート(isekai-pipeファミリー)への拡張

##### Layer 1とLayer 2は実質1つに収束する

`ISEKAI_PIPE_QUIC`系の接続は概念上2層に分けられる:

1. **Layer 1（QUIC接続そのもの）**: ヘルパー起動用の踏み台SSHログイン→ヘルパー起動確認→
   QUICハンドシェイク→data stream 1本の確立。resume状態（`ClientResumeState`）・
   マルチパス時の`PathBroker`・APP_ACKタスクはここに属する。
2. **Layer 2（ネストしたSSH `client::Handle`）**: Layer 1のdata stream上に
   `client::connect_stream()`で張られる、実際のシェル通信を担うSSHセッション
   (`isekai_pipe_quic_transport.rs::run_over_stream`)。

一見この2層をそれぞれ独立にプールする設計（Layer 1は共有するがLayer 2はタブごとに別々に
ネスト認証する等）もあり得そうに見えるが、**現状のwireプロトコルがdata streamを1本しか
許さない**ため、Layer 1を共有した時点で必然的にLayer 2も1個しか存在できない。つまり
実装上は2層のプールを別々に持つ必要は無く、**「1プールキーにつき、Layer 1+Layer 2をまとめて
所有する1個のプールエントリ」**という、プレーンSSHと同じ形に帰着する。プールエントリが
内部に「QUIC接続実体（resume/multipath状態機械込み）」と「その上のネストした
`client::Handle`」の両方を持つ、という点だけがプレーンSSH版との違いになる。

##### プールキーの拡張

```rust
enum TransportPoolKey {
    PlainSsh(SshPoolKey),
    IsekaiPipeQuic {
        ssh_host: String,
        ssh_port: u16,
        username: String,
        auth_identity: Option<String>,   // SshPoolKeyと同じ規則(パスワードは対象外)
        agent_forward: bool,
        jump: Option<SshPoolKey>,
        /// `IsekaiPipeQuicConfig.bind_port`。ヘルパーの固定待受ポート指定が
        /// タブによって食い違うと同一ヘルパーインスタンスに繋がる保証が無いため
        /// キーに含める(通常は同一プロファイルの全タブで同じ値になっているはず)。
        bind_port: Option<u16>,
        /// マルチパス/STUN P2P/link relay等、経路確立方式が異なれば別エントリにする
        /// (後述)。
        dial_strategy: DialStrategyKey,
    },
    // TsshdQuic は今回未設計（前述のスコープ節参照）。将来必要になれば同じ形で追加する。
}
```

- `TransportPreference`ごとに実装型（`IsekaiPipeQuicSession`/`MultipathIsekaiPipeQuicSession`/
  `IsekaiStunP2pSession`/`IsekaiLinkRelaySession`）が異なるため、`dial_strategy`は
  「どの実装型で確立された接続か＋その確立方式固有のパラメータ（マルチパスの`direct_host`、
  STUN/relayのエンドポイント設定等）」を表す。**異なる確立方式は異なるプールエントリになる**
  （同じホストでも、通常のisekai-pipe QUICとSTUN P2Pは別々の経路なので共有しない）。
- `TransportPreference::AUTO`は「実際に確立できた経路の名前空間にそのまま登録する」。
  isekai-pipe QUICが成功すれば`IsekaiPipeQuic`名前空間、内部フォールバックでプレーンSSHに
  落ちれば`PlainSsh`名前空間——`Auto`という第三の名前空間は作らない。これにより、
  「プロファイルAが`AUTO`、プロファイルBが`ISEKAI_PIPE_QUIC`で、どちらも同じホストへの
  isekai-pipe QUIC接続が成立した」場合、両者は同じプールエントリを共有できる（要求時の
  `TransportPreference`の見た目ではなく、実際に確立された接続の実体で同一性を判断する）。

##### 接続レベルイベントのfan-out（新規の実装ポイント）

プレーンSSHのプーリングでは「チャネル単位のイベント」と「接続単位のイベント」の区別が
`run_ssh_channel_loop`のスコープにほぼ閉じていた。QUIC系では**新たに`NoViablePath`
（`PathBroker`起点、`TransportEvent::NoViablePath`）という接続単位のイベントが増える**。
これは特定のチャネル/タブの問題ではなく「QUIC接続そのものが全経路で応答無し」を意味するため、
そのプールエントリを共有している**全タブ**の`OrchestratorCallback::on_no_viable_path()`へ
届ける必要がある。

現状は1タブ＝1`event_tx`（`SessionCore::start`が作る`TransportEvent`チャネル）という設計
だが、プーリング後は「接続単位のイベント発生源（QUIC接続の実体を保持するタスク）」対
「タブの数だけある`event_tx`」というfan-outが新たに必要になる。プールエントリが
`Vec<mpsc::Sender<TransportEvent>>`（アタッチ中の全タブの送信端）を保持し、接続単位の
イベント（`NoViablePath`・基盤接続そのものの`Disconnected`）はこのリストの全員に配る。
一方チャネル単位のイベント（`Stdout`・`Resized`・個別チャネルの`ExitStatus`由来の
`Disconnected`）は、各タブが自分で開いたチャネルのI/Oループが自分の`event_tx`にだけ送る
既存の形をそのまま使えるため変更不要。

##### マルチパス/物理経路rebindの意味論

`rebind_to_fd`（「WiFiは繋がっているがupstreamが死んでいる」時の物理経路切り替え）は
`ActiveSession::MultipathIsekaiPipeQuic`経由で呼ばれるが、プーリング後は対象が
「そのタブ専用の接続」ではなく「複数タブが共有する1本のQUIC接続」になる。**これは実は
自然な変化であり、新しい問題ではない**: 物理的に存在するQUIC接続（＝UDPソケット）は
元々1つしか無く、複数タブが同じ接続を共有している以上、どのタブから`rebindToFd`を
呼んでも「その1つの接続」を動かすのが正しい（他のタブだけ古い経路のまま、という状態は
そもそも物理的にあり得ない）。

なお`TerminalTabsViewModel`の既存docstring（`onWifiUpstreamBroken`まわり）が既に
「物理マルチパスfd取得・upstreamフェイルオーバー監視はプロセス単位のグローバルAPIで
タブ単位に分離されていないため、複数タブが同時に有効化した場合は後勝ちになる」という
制約を明記している。プーリング後にQUIC接続自体を共有するようになると、この制約は
「実装上の妥協」ではなく「物理的に正しい挙動」に格上げされる形になる。

##### ライフサイクルの差分

状態遷移そのもの（`Connecting`→`Active`→`IdlePersist`→削除、後述）はプレーンSSHと同じ
パターンを流用できる。差分は次の2点:

- `IdlePersist`の猶予時間: プレーンSSHは目安30秒としたが、QUIC系は接続確立コスト
  （ヘルパー起動＋QUICハンドシェイク＋ネスト認証）がプレーンSSHのTCP接続より明らかに
  高いため、より長い猶予時間（例: 60〜120秒程度）を検討する価値がある。ただし本節では
  値を確定しない（実装時に#3で決める、もしくは実測してから調整する）。
- `PoolEntry`が保持するものが増える: `handle`/`agent_key`/`remote_forwards`に加えて、
  QUIC接続の実体（resume状態・`PathBroker`・APP_ACKタスクのハンドル）と、前述の
  fan-out用`event_tx`リストを持つ。

#### ライフサイクルと保持場所

以下は`PlainSsh`/`IsekaiPipeQuic`両名前空間に共通する骨格（`PoolEntry`が保持する中身の差分は
前項「QUIC系トランスポートへの拡張」参照）。プール自体は個々の`SessionOrchestrator`
（＝個々のタブ）よりも寿命が長い必要があるため、
`SessionOrchestrator`のフィールドではなく、`RUNTIME`（`lib.rs`、`Lazy<tokio::runtime::Runtime>`の
プロセス全体シングルトン）と同じパターンで、新規モジュール（例: `pool.rs`）に
プロセス全体で1つの`static`として持たせる:

```rust
static SSH_POOL: Lazy<Mutex<HashMap<SshPoolKey, Arc<Mutex<PoolEntry>>>>> = Lazy::new(...);

struct PoolEntry {
    phase: PoolPhase,
    handle: Option<Arc<tokio::sync::Mutex<client::Handle<RusshEventHandler>>>>,
    agent_key: Arc<Mutex<Option<Arc<PrivateKey>>>>,
    remote_forwards: Arc<Mutex<HashMap<u16, (String, u16)>>>,
    refcount: u32,
    idle_timer: Option<tokio::task::JoinHandle<()>>,
}

enum PoolPhase { Connecting, Active, IdlePersist }
```

これは`.claude/rules/rust-ssot.md`の原則（セッション/接続状態はRust側SSOT、Kotlin側に
ミラー状態を作らない）に完全に従う。`SessionOrchestrator::connect()`のUniFFIシグネチャは
変更不要——内部で「新規接続」だった処理が「プールへのアタッチ or 新規接続」に変わるだけで、
Kotlin側（`TerminalTabsViewModel`/`TerminalSession`）は一切関知しない。既存の
`OrchestratorShared.session: Mutex<Option<ActiveSession>>`（タブごとの状態、変更不要）とは
別レイヤーとして共存する。

状態遷移:

1. **該当キーのエントリが無い（最初のタブ）**: `PoolPhase::Connecting`でエントリを作成し
   `connect_via_jump_or_direct()` + `authenticate_session()`を一度だけ実行。成功したら
   `Active`・`refcount=1`にし、最初のチャネル（`channel_open_session()`+ PTY + shell）を
   このタブ用に開く。失敗したらエントリを削除し、通常の接続失敗として扱う（今と同じ
   エラー経路）。
2. **該当キーのエントリが`Connecting`中に別タブが同じキーで接続要求（同じホストへ複数タブを
   間髪入れずに開いた場合）**: 新規に`client::connect()`を投げるのではなく、
   進行中の接続完了を`tokio::sync::watch`等で待ってから3.に合流する（成功時はそのまま
   チャネルを開く、失敗時はそのタブも同じエラーで失敗させる）。これにより同時多重dialを防ぐ。
3. **該当キーのエントリが`Active`/`IdlePersist`（2本目以降のタブ）**: 接続確立処理を丸ごと
   スキップし、共有`Handle`に対して`channel_open_session()`+ PTY + shellだけを行う。
   `refcount += 1`（`IdlePersist`中ならタイマーを取り消して`Active`へ戻す）。
4. **あるタブのチャネルが終了（正常切断/エラー/ユーザーによる切断）**: `refcount -= 1`。
   `refcount > 0`ならHandleには一切触れず終了（他タブは無影響——これが#4で検証する内容）。
   `refcount == 0`になったら即座にHandleを閉じず、`IdlePersist`へ遷移し猶予タイマーを開始する。
5. **`IdlePersist`中に猶予タイマーが満了**（新規アタッチが無いまま）: Handleを閉じてエントリを
   プールから削除する。
6. **`IdlePersist`中に新規タブがアタッチ**: タイマーを取り消し3.と同じ経路で`Active`に戻る。

猶予時間は固定・短時間（目安30秒程度）とし、ユーザー向け設定は設けない。CLI側の
`ControlPersist 10m`（他プロセスからの独立した将来の接続のために長時間デーモンを維持する）
とは目的が異なり、Android側で解決したいのは主に「タブを閉じてすぐ開き直す」程度のケース
（ホスト鍵確認後の再接続、タブの素早い開閉等）である。本アプリの既存方針
（`PLAN.md` Phase 7-7/9、実験的機能は既定OFF・日和見的フォールバック）と同様、複雑な
ユーザー向けオプションを増やさない方向で固定値にする。

#### 接続プロファイルの削除・変更時の扱い

特別なコードパスは不要という結論。理由:

- プールキーはRust側で接続要求のたびにSSH関連フィールドから都度計算するものであり、
  Room上の`ConnectionProfile`行への生きた参照ではない。プロファイルを編集（ホスト/
  ユーザー名/鍵を変更）すると、次回`connectTab`が計算するキーが変わるため、自然に
  別のプールエントリになる（旧エントリはどのタブからも参照されなくなり、通常のアイドル
  タイムアウト経路でいずれ閉じる）。
- プロファイル削除時も同様。`TabState.profile`は`openTab`時点のスナップショットであり
  Room行への継続的参照ではないため、既に開いているタブの接続はプロファイル削除の影響を
  受けない（プーリング導入前の既存動作と同じ）。プール導入によってこの挙動が変わることはない。

#### 障害時の設計方針（実機確認は#4で行う）

設計上の前提（実装後に#4で検証する）:

- SSHプロトコル自体（RFC 4254）はチャネルごとに独立したフロー制御・`CHANNEL_CLOSE`/
  `CHANNEL_EOF`を持ち、1チャネルの終了は他チャネルにも`client::Handle`自体にも影響しない
  設計になっている。
- 現状の`run_ssh_channel_loop`の終了経路（`channel.wait()`が`None`/`ExitStatus`を返す、
  `TransportCommand::Disconnect`受信）は、いずれも`channel.eof()`というチャネル単位の
  操作で終わっており、`session`（`client::Handle`）そのものへの操作は行っていない。
  今日Handleが実際に閉じるのは、そのタブ用の非同期タスクが終了し、（今はそのタスクだけが
  唯一の所有者である）Handleがdropされるからに過ぎない。プーリング後は「Handleの所有権」を
  タブ用タスクからプールエントリ側へ移すだけで、チャネル終了時の既存コード自体はほぼ
  そのまま使い回せる見込み。
- タブ間で影響が伝播「してはいけない」ケース: 片方のタブのリモートシェルプロセスが`exit`する・
  片方のタブでユーザーが切断する・片方のタブのchannelでプロトコルエラーが起きる。
- タブ間で影響が伝播「すべき」ケース（正しい"fate sharing"）: 基盤のTCP/QUIC接続そのものが
  切れた場合は、そのHandleを共有する全タブが`Disconnected`になるべき（個別チャネルの問題では
  なく接続そのものの問題であるため）。
- `agent_forward`はプールキーに含める設計にしたため、「異なる`agent_forward`設定のタブが
  同一Handleを共有する」状況自体が発生しない（キー設計で解消済み。#4では「発生しないこと」の
  確認で足りる）。
- QUIC系プールエントリ固有: `NoViablePath`・基盤QUIC接続そのものの切断は前述の
  fan-out機構で共有中の全タブに伝播する「べき」イベント（正しい"fate sharing"）。逆に
  個別チャネル（タブ）のシェルプロセス終了やユーザーによる切断は、プレーンSSHと同様
  他タブへ伝播「してはいけない」。#4のテスト観点にQUIC系プールエントリでの
  `NoViablePath`一斉配信の確認を追加する。

#### 今後の課題

- **`TSSHD_QUIC`（Phase 5、外部`tsshd`デーモン経由）のプーリング**: 構造的には
  `ISEKAI_PIPE_QUIC`ファミリーと同じパターン（`quic_transport.rs`もタブごとに個別のQUIC
  エンドポイントへ接続しその上にネストした`client::Handle`を張る）が使えるはずだが、
  Phase 7以降優先度が下がっている経路のため今回は詳細キー設計を行わない。必要になれば
  `TransportPoolKey`に`TsshdQuic`variantを追加する形で同じ枠組みに載せられる。
  isekai-pipe QUICファミリー（`ISEKAI_PIPE_QUIC`/`AUTO`/`ISEKAI_PIPE_QUIC_MULTIPATH`/
  `ISEKAI_STUN_P2P_QUIC`/`ISEKAI_LINK_RELAY_QUIC`）は本節で設計済み（上記「QUIC系
  トランスポート(isekai-pipeファミリー)への拡張」参照）。
- 踏み台（jump host）側Handleそのもののプーリング（同じ踏み台を経由する複数の異なる対象ホストで
  踏み台への認証を使い回す）は本節では扱わない。
- QUIC系の`IdlePersist`猶予時間の具体値（プレーンSSHの30秒と揃えるか、接続確立コストの
  高さを踏まえて伸ばすか）は#3実装時に決定する。
- 接続レベルイベントfan-out機構（`Vec<mpsc::Sender<TransportEvent>>`）の具体的な実装
  （タブのdetach時にリストから確実に取り除く、Sender送信失敗時の扱い等）は#3のスコープ。

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
  `FaultyUdpSocket` は `isekai-terminal-core` のデバッグ専用モジュールの外に出ない
- ~~再配布時のバイナリバージョン変更の扱い~~: A/B二択ではなく三段階ポリシーに決定
  （「同一digestの再配布は自動」「署名済みdigestへの更新は自動（署名検証は未実装、将来対応）」
  「未署名の異なるdigestへの変更はfail closed」）。署名検証導入前の暫定策として、`init` で
  配布したバイナリそのものをローカルにキャッシュし（`~/.cache/isekai-ssh/helpers/sha256-<hash>/`）、
  `connect` からの自動再配布は「以前信頼した実体と同一のバイナリ」に限定する
  （`update_policy = "exact-digest-only"`、「trust store のファイル形式」節参照）
- ~~`isekai-helper --relay` の常駐と `--max-idle-lifetime`（既定600秒）の整合~~: `isekai-ssh init`
  が `RelayLaunchSpec::idle_lifetime_secs`（既定30日）経由で `--max-idle-lifetime` を明示的に渡す
  ことで解決。isekai-helper自身・`isekai-terminal-core`側の既定値は無改造（「isekai-helper 側の追加要件」節参照）

### 引き続き未決の項目

- **署名検証の導入**: リリースプロセスからの正当なバイナリであることを証明する release signing key
  の導入（manifestへのEd25519署名等）は未着手。導入後、`update_policy` を
  `"signed-compatible"` 等へ移行する設計だけは決めてある（「trust store のファイル形式」節）が、
  鍵管理・配布・ローテーションの運用は未設計。
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
