# isekai-ssh

`ssh(1)` のフロントエンドとして動く単体バイナリ。`isekai-terminal`(Android アプリ)が使っている
自作ヘルパー `isekai-pipe serve` 経由の QUIC 接続耐性(ローミング・瞬断からの resume・relay 経由の
NAT 越え)を、Android アプリに限らず手元の `ssh` からもそのまま使えるようにする。

設計の背景・各コマンドの詳細な契約は [`ISEKAI_PIPE_DESIGN.md`](../../ISEKAI_PIPE_DESIGN.md) を参照。
本ドキュメントは「実際に使うために何をすればいいか」だけに絞った利用者向けガイド。

## Quick Start

ビルド(下記「インストール」参照)が済んでいる前提で、経路別の最短手順。詳細は各節を参照。

**A. relay 経由(推奨・既定。NAT 越え・ローミング耐性あり)— ホストごとに `isekai-ssh <host>` 一発**

一度だけ、`isekai-ssh login` と `~/.ssh/config` の `Host *` へのデフォルト relay 設定をしておけば、
以降は**未登録のホストに対しても** `isekai-ssh <host>` を打つだけで初回 bootstrap(TOFU の
`[y/N]` 確認あり)から接続まで一気に進む。`isekai-ssh init` を個別ホストごとに手で実行する
必要はない。

```bash
# 1. relay の JWT を取得(一度だけ。ホストごとの作業ではない)
isekai-ssh login --device-auth-endpoint <URL> --token-endpoint <URL> --client-id <ID>

# 2. ~/.ssh/config に「どのホストにも使うデフォルト relay」を Host * で一度だけ書く
cat >> ~/.ssh/config <<'EOF'
Host *
    #@isekai bootstrap-relay addr=relay.example.com:4433 sni=relay.example.com
EOF
# (個別の Host ブロックに同じ #@isekai bootstrap-relay を書けば、そのホストだけ別の
#  relay を使う、という上書きもできる — 通常の ssh_config と同じ first-match-wins)

# 3. 以降、未登録のホストも登録済みのホストも isekai-ssh <host> だけでよい
isekai-ssh myhost
#  未登録なら: 実行中に自動でリモートarchを検出し、GitHub Release から isekai-pipe を
#  ダウンロード(`--isekai-helper-binary` は不要)→ relay 経由で配置 → "Trust this
#  isekai-helper...? [y/N]" で y → そのまま接続。登録済みなら直接接続するだけ。
```

**登録済みホストの`isekai-pipe serve`が再起動した場合も、`isekai-ssh <host>`を打つだけで
自動的に復旧する。** デプロイ済みの`isekai-pipe serve`はセッション鍵(`session_secret`)と
TLS証明書を起動のたびに毎回生成し直す(永続化しない)ため、リモートホストの再起動・
プロセスクラッシュ・手動再起動などで一度でもプロセスが入れ替わると、キャッシュ済みの
信頼情報は無効になる。この状態(stale trust)は接続時に自動検知され、`[y/N]`確認を
挟まずに(既に一度信頼したホストの単なる情報更新のため)自動で再bootstrap・再接続する。
手動で状態を確認したい場合は `isekai-ssh doctor <host>` を使う(下記「トラブルシューティング」
参照)。

`--isekai-helper-binary <path>` を明示的に渡した場合は今まで通りそれを最優先で使う
(アーキ検出・ダウンロードは一切行われない)。自動ダウンロードは**このプロジェクトがまだ
GitHub Release を公開していないため、今のところ実際には失敗し**、渡さなかった場合は
下記の手動 `init` か `--isekai-helper-binary` 明示指定にフォールバックする必要がある
(honest gap、下記「既知の制限」参照)。

`isekai-ssh init` は今も使える(踏み台 `--via` 経由での一回限りのデプロイなど、より
細かい制御が要る場合の代替経路として)——下記「2. ホストへ isekai-pipe を配置し、
信頼登録する」参照。

**B. direct-by-bootstrap-host(relay/JWT 不要。接続元から宛先へ UDP/QUIC 直接到達できる場合のみ)**

```bash
# 1. 初回接続時に自動配置・確認・登録まで一気に行う(--isekai-helper-binary は省略可、
#    省略時は上記と同じ自動ダウンロードを試みる)
isekai-ssh --isekai-helper-binary /path/to/isekai-pipe myhost
#  -> "Trust this isekai-helper...? [y/N]" で y、そのまま接続まで進む

# 2. 2回目以降は素の isekai-ssh だけでよい(登録済みなので)
isekai-ssh myhost
```

## 前提

- クライアント(`isekai-ssh`自体を実行する環境): Linux(x86_64 / aarch64、musl 静的バイナリ
  としてビルドするので配布物自体は特定ディストリへの依存が無い)・macOS(x86_64 / aarch64)・
  Windows(x86_64、`cmd.exe`/PowerShell/Git Bash いずれの起動元でも動作。`msvc`/`gnu` 両
  ターゲットをビルド)を公式サポートする。**接続先ホストは以下に関わらず常に Linux 固定**
  (下記参照)——Windows/macOS サーバー対応は対象外([`PLAN.md`](../../PLAN.md) の該当 Phase
  参照)。
- **クライアント側で実 `ssh(1)` が必要かどうかはプラットフォームで異なる**:
  - Linux / macOS では従来通り、実 `ssh(1)`(OpenSSH クライアント)を子プロセスとして
    起動し `ProxyCommand` を差し込む薄いラッパーとして動く——**実 `ssh(1)` が必要**。
  - **Windows では実 `ssh(1)`(Win32-OpenSSH の `ssh.exe`)を一切必要としない**。
    Windows 上の `isekai-ssh` は `russh` ベースの SSH クライアント**本体**であり、
    外部の `ssh`/`ssh.exe` を起動することも `ProxyCommand` を差し込むこともしない。
    `ssh.exe` をインストールしなくてよい。詳細・非互換事項は下記「Windows
    (ネイティブ SSH クライアント経路)」を参照。
- 接続先ホストに `ssh` で(パスワードでも鍵でも)ログインできること。`isekai-ssh init` は
  この既存の SSH 接続を使って `isekai-pipe`(serve として起動)を配置する。
- 接続先ホストが到達可能な isekai-link relay(`isekai-pipe serve --relay` が張るトンネル先)の
  エンドポイント(`ADDR:PORT` と TLS SNI)、およびそこへ認証するための JWT。relay 自体の
  構築・運用はこのドキュメントの範囲外(`seera-networks/ISEKAI-link` 等)。relay を使わない
  `direct-by-bootstrap-host` 経由なら JWT は不要(下記「自動 bootstrap」参照)。

## インストール

まだ GitHub Release としては配布していないので、現状はソースからビルドする。

```bash
git clone https://github.com/cuzic/isekai-terminal.git
cd isekai-terminal/rust-core
cargo build --release -p isekai-ssh
# バイナリ: target/release/isekai-ssh
```

musl 静的バイナリ(配布・他マシンへのコピーに向く)が欲しい場合:

```bash
cd rust-core
# 初回のみ: brew install zig && cargo install cargo-zigbuild \
#           && rustup target add x86_64-unknown-linux-musl aarch64-unknown-linux-musl
bash scripts/build-isekai-ssh-musl.sh
# バイナリ: target/{x86_64,aarch64}-unknown-linux-musl/release/isekai-ssh (+.sha256)
```

生成した `isekai-ssh` を `$PATH` の通ったディレクトリ(例: `~/.local/bin/`)に置く。

`isekai-ssh init` が接続先ホストに配置する `isekai-pipe` 本体も、同じ手順で musl バイナリを
用意しておく必要がある(`bash scripts/build-isekai-pipe-musl.sh`)。手元のバイナリと同じ
アーキテクチャのサーバーに配る場合は、既に GitHub Release で配布済みのものを使ってもよい。

## 使い方

### 1. relay へのアクセストークンを取得する(`login`)

relay(MASQUE)へ認証するための JWT を、RFC 8628 Device Authorization Grant で取得する。
OAuth のエンドポイントは環境ごとに異なるため、relay の運用者から教えてもらった値を渡す。
relay を使わない `direct-by-bootstrap-host` 経由(下記)なら、この手順は不要。

```bash
isekai-ssh login \
  --device-auth-endpoint https://auth.example.com/oauth/device/code \
  --token-endpoint       https://auth.example.com/oauth/token \
  --client-id            <CLIENT_ID>
```

表示される検証 URL とコードをブラウザで承認すると、`~/.config/isekai-ssh/token.json` に
アクセストークン(+ あれば refresh token)が保存される。ログアウトは `isekai-ssh logout`。

保存済みトークンは `init --relay-jwt-from-login` を渡せば自動的に読まれる(`isekai-ssh
myhost` の自動 bootstrap で relay 経由が選ばれた場合も同様に自動で読まれる、Quick Start A
参照)。手動で取り出したい場合は:

```bash
RELAY_JWT=$(jq -r .access_token ~/.config/isekai-ssh/token.json)
```

すでに relay の JWT を別の方法で持っている場合は `login` 自体をスキップしてよい。

### 2. ホストへ isekai-pipe を配置し、信頼登録する(`init`)

ホストごとに一度だけ行う対話的な作業。既存の `ssh` 接続(公開鍵認証済みが望ましい)を使って
`isekai-pipe` バイナリをアップロード・`serve --relay ...`として起動し、その場で得た
ハンドシェイク(公開鍵指紋相当の `cert_sha256` など)を確認したうえで `[y/N]` の明示的な
承認をしてから信頼ストアに登録する。

```bash
isekai-ssh init myhost \
  --helper-binary   /path/to/isekai-pipe \
  --relay-addr      relay.example.com:4433 \
  --relay-sni       relay.example.com \
  --relay-jwt       "$RELAY_JWT"
```

主なオプション:

| オプション | 説明 |
|---|---|
| `<HOST>` | `myhost` / `myhost:2222` / `user@myhost` のいずれか。`~/.ssh/config` の `Host` と揃えておくと分かりやすい |
| `--via <JUMPHOST>` | この一度きりのデプロイ作業を踏み台経由で行う場合の `ssh -J` 相当。複数回指定で multi-hop チェーンにもなる |
| `--helper-binary <PATH>` | アップロードする `isekai-pipe` バイナリ。省略するとリモートの `uname -m` を検出し `--helper-release-repo`/`--helper-release-tag` から自動ダウンロードを試みる(今は実在する Release が無いため実際には失敗する、上記「既知の制限」参照) |
| `--helper-release-repo <OWNER/REPO>` / `--helper-release-tag <TAG>` | `--helper-binary` 省略時のダウンロード元。既定 `cuzic/isekai-terminal` / latest |
| `--relay-addr` / `--relay-sni` / `--relay-jwt` / `--relay-jwt-from-login` | isekai-pipe serve が relay へトンネルを張るための接続情報(`--relay-jwt`/`--relay-jwt-from-login` のどちらか一方が必須。後者は `isekai-ssh login` の保存済みトークンを使う) |
| `--idle-lifetime <SECS>` | 配置した isekai-pipe serve が無接続状態でも自己終了するまでの秒数。既定30日(2,592,000秒)。`isekai-ssh`(wrapper)は何時間・何日空けても同じ稼働中のプロセスにダイヤルし直すだけなので、Android アプリ向けの既定値(600秒)よりずっと長い値を明示的に渡している |
| `--helper-version` / `--release-channel` | 信頼ストアに記録するだけの表示用メタデータ |

成功すると `~/.local/state/isekai/profiles/<host:port>.json`(`PersistentProfile`)に host
ごとのエントリ(公開鍵指紋相当の `identity_pubkey`、バイナリの `sha256`、relay の公開
アドレス等)が書き込まれる。**このディレクトリが `isekai-ssh`(wrapper)の信頼判定の
正本**であり、未登録のホストへは(下記の自動 bootstrap 条件に当てはまらない限り)
fail closed で一切接続しない(下記「トラブルシューティング」参照)。中身を人間可読な
形で確認したいだけなら書き換えるより `isekai-pipe inspect --profile <host>` を使う方が
安全(secret を漏らさず状態だけ見られる)。

### 2'. 自動 bootstrap(`init` を個別に実行しなくても `isekai-ssh <host>` だけで済ませる)

`isekai-ssh myhost` の初回実行(= 未登録ホスト)時、`~/.ssh/config` の設定次第で以下のいずれかを
自動で行う:

- **relay 経由**: `Host myhost`(または `Host *` のデフォルト、上記 Quick Start A 参照)に
  `#@isekai bootstrap-relay addr=<ADDR:PORT> sni=<NAME>` があれば、`isekai-ssh login` の
  保存済みトークンから JWT を取得し、relay 経由で配置する。
- **direct-by-bootstrap-host**: `bootstrap-relay` が無ければ、relay も STUN も使わず
  bootstrap 用の SSH 宛先へ直接 QUIC で到達する経路にフォールバックする(接続元から
  direct host へ UDP/QUIC 直接到達できる場合のみ機能する)。

どちらの経路でも、アップロードする `isekai-pipe` バイナリは:

1. `--isekai-helper-binary <path>` を渡せばそれを最優先で使う(アーキ検出・ダウンロード無し)。
2. 渡さなければ、リモートの `uname -m` を検出し、`--isekai-helper-release-repo`(既定
   `cuzic/isekai-terminal`)/`--isekai-helper-release-tag`(既定 latest)で指定した GitHub
   Release から `isekai-pipe-<arch>-unknown-linux-musl` という名前の asset をダウンロードし、
   `$XDG_CACHE_HOME/isekai-ssh/helpers`(既定 `~/.cache/isekai-ssh/helpers`)にキャッシュして
   使う。**このプロジェクトはまだ実際の GitHub Release を公開していないため、今のところ
   2 は実際には失敗し、1 を渡す必要がある**(honest gap、下記「既知の制限」参照)。

```bash
isekai-ssh --isekai-helper-binary /path/to/isekai-pipe myhost
```

`init` と同じ `[y/N]` 確認(identity・sha256 の表示)を経て `PersistentProfile` に登録される。
複数 hop の `--via`/`bootstrap-candidate via=...`(ProxyJump チェーン)にも対応している。

### 3. 日常的に接続する(`isekai-ssh` を ssh wrapper として使う)

`init` 済み(または上記の自動 bootstrap 済み)の host へは、`ssh` の代わりに `isekai-ssh` を
入口にする。

```bash
isekai-ssh myhost
isekai-ssh -p 2222 myhost
isekai-ssh -L 5432:127.0.0.1:5432 myhost
isekai-ssh myhost 'journalctl -f'
```

**Linux / macOS では**、`isekai-ssh` は短命な `ConnectionIntent` をユーザー専用 runtime
directory に保存してから OpenSSH を起動し、`ProxyCommand isekai-pipe connect --profile
<host> --service ssh --stdio` を自動で差し込む。OpenSSH へ渡すのは `ISEKAI_INTENT_ID`
だけで、session secret は argv/env には載せない。OpenSSH のパスを変えたい場合は
`--isekai-ssh-path PATH`、`isekai-pipe` のパスを変えたい場合は `--isekai-pipe-path PATH`
を使う。**Windows では OpenSSH を起動せず**、`isekai-ssh` 自身が `isekai-pipe connect`
の出力バイトストリームの上に `russh` で直接 SSH セッションを張る(下記「Windows
(ネイティブ SSH クライアント経路)」)。この場合 `--isekai-ssh-path` は無関係で、
`--isekai-pipe-path` のみ意味を持つ。

既存の `~/.ssh/config` へ直接書く互換運用も残している。

```sshconfig
# ~/.ssh/config
Host myhost
    HostName 10.0.5.20
    ProxyCommand isekai-pipe connect --profile myhost --service ssh --stdio
    TCPKeepAlive no
```

`TCPKeepAlive no` は必須ではないが強く推奨する(このトランスポートは TCP そのものではない
ので、`ssh` 自身の TCP keepalive は無意味かつ誤解を招く)。
`isekai-pipe connect` は `ConnectionIntent` を atomic claim し、`isekai_transport` で
relay/STUN transport を直接起動して stdio bridge を所有する。relay mode では QUIC 接続が
切れても `--resume-window`(既定はtrzsz-ssh/tsshdの`UdpAliveTimeout`に倣った10日間。
`isekai-pipe serve` 側の既定と自動的に揃う、後述)の間は resume を試み続けて `ssh` 側の
stdin/stdout を閉じずに粘る。openssh は `ServerAliveInterval` を明示設定しない限り
自分から生存確認しない(既定無効)ので、通常はこのresumeの粘りとは競合しない。もし
`ServerAliveInterval`/`ServerAliveCountMax` を明示的に設定するなら、resume window より
十分短い値にすると「まだ正当にresumeで復帰できたはずの接続」を `ssh` 自身が先回りして
諦めてしまう(`ISEKAI_PIPE_DESIGN.md` §6.4 参照)——瞬断からの高速な自動復旧を諦めても
良いから短時間で確実に死活判定したい、という用途でだけ明示的に設定すること。

`isekai-pipe connect` は **信頼ストア登録済みのホストにしか接続しない**(自動 bootstrap 対象
なら wrapper がその場で登録してから接続する)。それ以外は何もしない(標準出力へは1バイトも
書かない)。ログ・進捗は全て標準エラーへ出るので、`ssh` から見える標準出力を汚さない。

### `.ssh/config` 設定例(`#@isekai` ディレクティブ)

`isekai-ssh myhost` を使う場合、上記の手書き `ProxyCommand` の代わりに `~/.ssh/config` の
`Host` ブロックに `#@isekai <directive> <arguments...>` 行を足すことで挙動を細かく設定できる
(`#` 始まりなので通常の OpenSSH からはただのコメント行として無視される)。

```sshconfig
# ~/.ssh/config
Host production
    HostName 10.20.0.15
    User deploy
    ServerAliveInterval 30
    ServerAliveCountMax 6
    TCPKeepAlive no

    #@isekai profile production-east
    #@isekai bootstrap-candidate target=192.168.10.15:22 priority=120
    #@isekai bootstrap-candidate target=10.20.0.15:22 via=corp-bastion priority=100
    #@isekai link https://link.example.com
    #@isekai rendezvous https://rendezvous.example.com
    #@isekai stun stun1.example.com:3478
    #@isekai relay masque://relay.example.com
    #@isekai service ssh=127.0.0.1:22
    #@isekai service postgres=127.0.0.1:5432
    #@isekai resume-grace 180s
    #@isekai candidate-race-delay 250ms
    #@isekai relay-delay 900ms
    #@isekai ctl-socket yes
```

これで `isekai-ssh production`(または `-L 5432:127.0.0.1:5432 production` で `service postgres`
を経由するポートフォワード)がそのまま使える。`Host` パターンは通常の OpenSSH と同じ
(完全一致 / `*` / `?` / `!` 否定 / 複数パターン)で、`Include` (絶対パス・相対パス・`~`・glob・
循環検出込み)にも対応している。`Match` ブロック内の `#@isekai` はサポート外で、見つかると
`ISEKAI_CONFIG_UNSUPPORTED_MATCH` エラーで即座に拒否される(黙って無視されることはない)。

利用可能なディレクティブ:

| ディレクティブ | 引数 | 既定値 | 説明 |
|---|---|---|---|
| `enabled` | `yes`\|`no`(`true`/`on`/`1`, `false`/`off`/`0` も可) | `yes` | `no` にすると isekai-pipe を経由せず素の `ssh` にフォールバックする |
| `bootstrap-policy` | `auto`\|`always`\|`never` | `auto` | `auto`: 未登録ホストへの自動 bootstrap を試す(`--isekai-bootstrap` 相当)。`always`: 常に試す。`never`: 試さない(`--isekai-no-bootstrap` 相当) |
| `profile` | 文字列 | 接続先(destination)そのもの | trust store / `ConnectionIntent` のキーに使うプロファイル名。`Host` 名と分けたい時に使う |
| `remote-path` | パス | (isekai-pipe 側の既定) | 自動 bootstrap 時にリモートへ配置する `isekai-pipe` の設置先パス |
| `service` | `<name>=<host:port>`(複数可) | `ssh=127.0.0.1:22` のみ | 転送先サービス定義。`-L`/`-R` 等でこのプロファイル経由の別サービスへ転送する時に追加する |
| `bootstrap-candidate` | `target=<host:port> [via=<hop[,hop...]>] [priority=<n>]`(複数可) | `ssh -G` で解決した宛先 + `ProxyJump`(priority=100) | 自動 bootstrap の配布先候補。`priority` が最大のものが選ばれる |
| `link` | URL(複数可) | なし | isekai-link エンドポイント |
| `rendezvous` | URL(複数可) | なし | rendezvous エンドポイント |
| `stun` | `addr:port`(複数可) | なし | STUN サーバー。省略すると STUN 候補収集を行わない |
| `relay` | URL(複数可) | なし | isekai-link relay エンドポイント(`masque://...`) |
| `resume-grace` | `<n>ms` / `<n>s` / `<n>`(単位省略時は秒) | `10d`(864,000秒) | QUIC 接続が切れてから resume を試み続ける猶予時間。**新規に** auto bootstrap する時だけ、この値がそのまま `isekai-pipe serve --resume-window` に渡る。既に稼働中の helper がある場合は helper 再利用の仕組み(fingerprint に `resume-window` を含めない、アクティブな接続を巻き込んで強制再起動しないため)により、その helper が寿命切れ(既定 `--max-idle-lifetime` 30日)になるか手動で再デプロイを誘発するまで、helper 起動時点の値のまま変わらない |
| `candidate-race-delay` | 同上の duration 表記 | `150ms` | 複数 candidate を同時に試す際の後発 candidate の遅延 |
| `relay-delay` | 同上の duration 表記 | `750ms` | direct 系 candidate に対して relay を遅らせて追い掛けさせる遅延 |
| `install-mode` | `user`\|`system` | `user` | `system` は sudo・所有権・rollback が未実装かつ実装予定も無いため、指定すると設定解決時点でエラーになる(fail closed)。将来必要になった場合もisekai-ssh本体には組み込まず、`curl ... \| sudo bash`的な別のインストーラースクリプト/ラッパーとして提供する想定 |
| `ctl-socket` | `yes`\|`no` | `no` | `yes` にすると、リモートの対話シェルから `isekai-pipe ctl title "<text>"` / `isekai-pipe ctl clip push --mime <mime>` を実行することで、tmux を経由せず直接ローカルのタブ/ターミナルのタイトルやクリップボードへ反映できるよう per-タブのリモートフォワードを張る(`ISEKAI_PIPE_DESIGN.md` §8 Epic M参照)。Unix は実 `ssh(1)` の `-R` + UNIX domain socket、Windows ネイティブ経路は `russh` の streamlocal forward を in-process で消費する(下記「Windows(ネイティブ SSH クライアント経路)」)。明示的なリモートコマンドを指定した呼び出し(`isekai-ssh host 'some command'`)では黙って無効化される(opportunistic fallback) |
| `remote-log-level` | `error`\|`warn`\|`info`\|`debug`\|`trace` | `info` | 自動 bootstrap で起動するリモート側 `isekai-pipe serve` の `--log-level`。接続不良の切り分け時だけ `debug`/`trace` に上げ、常用ホストでは既定(`info`)のままにしておくのが推奨(ホストごとに設定できる) |
| `remote-bind-port-range` | `<START>-<END>`(例 `40000-40100`) | なし(OSが割り当てる ephemeral port) | 自動 bootstrap で起動するリモート側 `isekai-pipe serve --bind-port-range`。この範囲だけをホスト側ファイアウォールで許可すればよくなる(既定は Linux の ephemeral port range 全体を開ける必要がある) |
| `local-bind-port-range` | `<START>-<END>`(例 `40000-40100`) | なし(OSが割り当てる ephemeral port) | `isekai-ssh`(`isekai-pipe connect`)自身がこのマシンで張るQUICソケットのbindポート範囲。手元のファイアウォール/NATが outbound UDP を特定範囲にしか通さない場合に使う。`remote-bind-port-range` とは独立した設定(片方だけ・両方同時に設定してよい) |

`bootstrap-candidate`/`link`/`rendezvous`/`stun`/`relay`/`service` は複数行書くと追記されていく。
それ以外(`enabled`/`bootstrap-policy`/`profile`/`remote-path`/`resume-grace`/
`candidate-race-delay`/`relay-delay`/`install-mode`/`ctl-socket`/`remote-log-level`/
`remote-bind-port-range`/`local-bind-port-range`)は最初に出てきた値が採用される
(OpenSSH 本体の `Host`/`Match` と同じ first-value-wins 規則)。

### オプション: STUN による低遅延 P2P(`--mode stun`)

relay を経由しない直結を試したい場合、opt-in で使える。

```
ProxyCommand isekai-pipe connect --profile myhost --service ssh --stdio --mode stun --stun-server stun.example.com:3478
```

relay モードと違い、**セッション中に NAT マッピングが失われる(Wi-Fi⇔モバイル回線のローミング等)と
resume できず、その場でセッションが終了する**。低遅延を優先する代わりにこのリスクを受け入れる
場合にのみ使う(既定は常に relay モード)。

## Windows(ネイティブ SSH クライアント経路)

**Windows 上だけ**、`isekai-ssh` は実 `ssh(1)`(Win32-OpenSSH の `ssh.exe`)を一切
起動せず、`russh` ベースのネイティブ SSH クライアントとして直接動作する
(`isekai-ssh/src/native/`、入口は `isekai-ssh/src/main.rs` の
`#[cfg(windows)] native::mux::run`)。Linux / macOS は今まで通り実 `ssh(1)` の
`ProxyCommand` 方式のままで、この節の内容は Windows にのみ当てはまる。`ssh.exe` の
インストールは不要で、`isekai-ssh doctor` を含めどのコマンドも `ssh.exe` の有無を
確認しない。

`~/.ssh/config` の解決には、実 `ssh(1)` の `ssh -G` を呼ぶ代わりに専用の
`openssh-config` クレートを使う(実 `ssh(1)` が無い環境で `ssh -G` に頼れないため)。
`~` の展開もこのクレートが `%USERPROFILE%`/`$HOME` を基準に自分で行うので、MSYS2/Cygwin
版 `ssh(1)` に固有の `~` 展開の癖(passwd データベース経由)には影響されない。

### 対応していない・非互換の事項

Windows ネイティブ経路は `isekai-ssh <host>` の**対話的接続に特化**しており、`ssh(1)` の
完全なドロップイン代替ではない。以下は明示的にスコープ外・非対応:

- **`ssh_config(5)` の一部キーワードのみ対応**。`openssh-config` クレートが解決するのは
  `HostName` / `User` / `Port` / `IdentityFile` / `ProxyJump` / `ForwardAgent` /
  `IdentityAgent` だけで、それ以外のキーワードは黙って無視される。特に次は非対応:
  - `ProxyCommand`(`isekai-ssh` 自身が接続経路を持つため無関係)
  - `Match` ブロックの条件(`Match exec` / `Match host` / `Match user` 等)——構文としては
    認識するが条件は一切評価されず、`Match` ブロック内の設定は適用されない
  - `CertificateFile`(SSH 証明書認証)
  - `IdentitiesOnly`
  - パスフレーズ付き秘密鍵——パスフレーズ入力プロンプトは未実装。`IdentityFile` に指定
    できるのはパスフレーズ無しの OpenSSH 形式鍵か、SSH agent(named pipe / Pageant)
    経由の鍵のみ
  - `known_hosts` ファイルとの直接互換——ホスト鍵の TOFU は `isekai-trust` の専用ストア
    (`host:port` キー)に記録し、OpenSSH の `~/.ssh/known_hosts` は読み書きしない
- **X11 forwarding は対象外**。
- **他ツールからの `ssh` 呼び出し互換は対象外**。git / rsync / VS Code Remote-SSH /
  Ansible 等が内部で `ssh` を起動する用途のドロップイン代替にはならない。`isekai-ssh`
  はあくまで `isekai-ssh <host>` の対話的接続に特化しており、汎用の `ssh` コマンド
  代替ではない。
- **非対話のリモートコマンド実行**(`isekai-ssh <host> 'cmd'`)は native 経路ではまだ
  配線されておらず、末尾のコマンド引数は無視して対話シェルを開く。基盤の
  `russh-stream-session` クレート自体は `SessionKind::Exec` に対応しているが、native の
  接続 dispatch(`native/connect.rs`)は現状 `SessionKind::Shell` 固定。

### マルチプレクサ(ControlMaster 相当)

複数のタブが**同じ接続設定**(host / port / user / identity / agent forward / route
設定など、`native/mux/naming.rs` がハッシュ化する接続関連の要素すべて)で
`isekai-ssh <host>` を実行すると、最初の1プロセス(owner)だけが認証済みの `russh`
接続を保持し、以降のプロセス(client)は owner にローカルの named pipe(`local-ipc-mux`)
経由でリレーしてもらう。各 client は自分専用のリモートシェルチャネルを持ち、接続・認証を
毎回やり直さずに済む(`native/mux/`)。

**既知の制限**: 真の `ControlPersist`(共有接続を、それを作ったタブより長生きさせる)は
実装していない。owner はそれ自身の前面シェルが終了した時点で接続を畳むため、その時点で
接続中の client は「接続喪失」で終了し(専用 exit code)、`isekai-ssh <host>` で改めて
起動し直す(=新しい owner になる)必要がある。生存プロセス間の再選挙は行わない。

### ctl-socket(`#@isekai ctl-socket yes`)

`#@isekai ctl-socket yes` は Windows ネイティブ経路でも**配線済み**で機能する
(`native/connect.rs` → `native/mux/ctl_forward.rs`)。リモートの対話シェルから
`isekai-pipe ctl title "<text>"` / `isekai-pipe ctl clip push` を実行すると、ローカルの
タブのタイトル・クリップボードへ反映される(`ISEKAI_PIPE_DESIGN.md` §8 Epic M)。

ただし Windows native の実現方式は Unix とは異なる。Unix は実 `ssh(1)` の `-R` が
リモート UNIX ソケットの forward を*ローカルソケット*にしか届けられないため、`isekai-ssh`
がそのローカルソケットで待ち受ける。native 経路は `isekai-ssh` 自身が SSH クライアント
なので、**ローカルの受け口(TCP ループバックや named pipe)を一切介さない**:

- `russh` の `streamlocal_forward(remote_path)` でリモート UNIX ソケットの forward を
  直接張り、サーバー発の `forwarded-streamlocal` チャネルをハンドラ経由で**そのまま
  in-process で受け取る**。
- リモートのシェルには Unix と同じく `export ISEKAI_CTL_SOCK=...; exec "$SHELL" -i -l` を
  pty+exec で渡し、socket path を知らせる。
- **owner / 単一プロセス**のタブは、受信した ctl メッセージを自プロセスの stderr へ OSC
  (タイトル OSC 0 / クリップボード OSC 52)として直接適用する。
- **mux クライアント**のタブは、owner がそのクライアント専用の forward を張り、受信 ctl を
  M4 の既存 `local-ipc-mux` 接続上で `Frame::Ctl` として当該クライアントへ中継する
  (クライアント同士で混ざらない)。
- アクセス制御: `forwarded-streamlocal` チャネルは SSH プロトコル層の in-process オブジェクト
  で他のローカルプロセスが接続できないため、Unix の TCP 用に検討された「TCP ポート +
  128bit トークン」は native では不要(廃止)。

**検証レベル(正直な現状)**: `x86_64-pc-windows-gnu` でのコンパイルと、mock sshd を使った
unit/統合テスト(`streamlocal_forward` の往復・`Frame::Ctl` 中継・OSC 適用)で検証済み
(Linux CI)。**実 Windows 機での実接続 e2e はまだ未確認**。

Unix(実 `ssh(1)` の `ProxyCommand` 経路)の ctl-socket は従来どおり `-R` + UNIX ドメイン
ソケットで変わらない。

### リモートビルドトリガー(`build-profile`、Unix/macOS クライアントのみ)

`#@isekai ctl-socket yes` の上に乗る形で、リモートの対話シェルから**このマシン(クライアント)
側**でビルドコマンドを実行させ、ログをリアルタイムでリモートの端末に流し、成果物ファイルを
リモートへ送り返せる(`ISEKAI_PIPE_DESIGN.md` §8 Epic P)。Windows でしかコンパイルできない
アプリを Linux 側の作業セッションから起動して結果を確認する、といった用途を想定している。

まずクライアント側でプロファイルを登録する(`<HOST>` は `~/.ssh/config` の `Host` エイリアス
と一致させる):

```bash
isekai-ssh build-profile add myhost win \
  --dir     /path/to/repo \
  --command "cargo build --release --target x86_64-pc-windows-msvc" \
  --result-glob "target/x86_64-pc-windows-msvc/release/*.exe" \
  --dest-dir    "~/isekai-build-results/win"
```

`--result-glob`/`--dest-dir` は成果物を送り返さないプロファイルなら両方省略してよい(片方
だけの指定はエラーになる)。登録前に `isekai-ssh build-profile test myhost win` で、ctl-socket
を一切使わずローカルでコマンドを試し実行できる。`isekai-ssh build-profile list`/`remove` で
一覧・削除。

登録後、`myhost` へ `isekai-ssh myhost` で接続した対話シェルの中から:

```bash
isekai-pipe ctl build win
```

を叩くと、クライアント側で `cargo build ...` が実行され、その stdout/stderr がそのまま
このコマンドの stdout/stderr として流れる(=リモートの端末にそのまま表示される)。終了後は
ビルドの exit code がこのコマンド自身の終了コードになるので、`isekai-pipe ctl build win &&
scp ...` のようなシェルチェインもそのまま効く。`--result-glob`/`--dest-dir` を設定していれば、
成功後にマッチしたファイルがバックグラウンドで `dest-dir` へ送られる(失敗時はクライアント
側のログにのみ記録され、リモートへの通知はない——既知の制限)。

**セキュリティ上の要点**: リモートが送れるのはプロファイル**名**だけで、実行される中身
(`--dir`/`--command`)は一切 wire に乗らない。プロファイル自体が空(既定)なら、リモートから
何を送っても何も実行されない。設定はプロファイル追加時に人間が明示的に行う。

**Windows クライアント**: `native/mux/`(owner/client mux)経路にも同じ機能を実装済み
(`ISEKAI_PIPE_DESIGN.md` §8 Epic P Phase 2)。このプロジェクトの開発環境には実機
Windows が無いため、この機能自体はモックSSHサーバーでのユニットテストと
`x86_64-pc-windows-gnu` へのクロスコンパイル確認までしか検証できていない——named pipe
実体・`cmd.exe` 固有の挙動・実 `isekai-ssh.exe` での実際のビルドは未検証(既知の制限)。

### 新規クレートの位置づけ

Windows ネイティブ経路の中核は、`isekai-` 固有の型に依存しない汎用クレートとして切り出して
ある(将来 isekai-terminal の外へ公開しやすい形):

- **`russh-stream-session`** — 任意の `AsyncRead + AsyncWrite` バイトストリームの上に
  `russh` で SSH クライアントセッションを張り認証する。ホスト鍵確認は差し替え可能で、
  単一踏み台(jump host)トンネルにも対応。
- **`openssh-config`** — `HostName` / `User` / `Port` / `IdentityFile` / `ProxyJump` /
  `ForwardAgent` / `IdentityAgent` という `~/.ssh/config` の限定サブセットを、`ssh(1)` と
  同じ `Host` / `Include` セマンティクスで解決する(`Match` ブロックは構文認識のみで
  評価しない)。
- **`local-ipc-mux`** — 同一マシン上の兄弟プロセス群のうち1つだけが排他的に named channel
  を保持し(owner)、他はその channel に client として接続する、という SSH 非依存の汎用
  パターン。上記マルチプレクサはこの上に SSH 用のフレームプロトコルを乗せている。
  **Windows(named pipe)実装のみ提供**し、Unix 実装は trait 境界(`ExclusiveChannel`)
  だけ用意して未実装(Unix は実 `ssh(1)` の ControlMaster で同じ役割を既に得ているため)。

これらの汎用クレートに対し、プロファイル解決・`#@isekai` ディレクティブ・trust store の
ファイル形式など isekai 固有のグルーは従来通り `isekai-` 接頭辞のクレート側に残り、上記に
依存する側に回る。

## 既知の制限

- **`--isekai-helper-binary`/`--helper-binary` を省略した際の自動ダウンロードは、実際には
  まだ機能しない。** このプロジェクトはまだ GitHub Release を一切公開していない。
  ダウンロード先の URL 構築・アーキ検出・キャッシュの仕組み自体は実装済みだが、実在する
  release asset が無いので 404 になり、既存の「`--helper-binary` を渡すか `init` を実行
  してください」というエラーにフォールバックする。実際にリリースを公開する際は
  `isekai-pipe-<arch>-unknown-linux-musl`(+任意で `.sha256` サイドカー)という asset 名の
  規約に従う必要がある。
- リリース成果物の署名検証は行っていない(恒久方針)。GitHub 自体の HTTPS/インフラが実質的な
  保護を提供しており、ed25519 署名を追加しても守れるのは「GitHub 自体が侵害された」という
  非現実的な脅威モデルだけなので、この規模のプロジェクトには過剰と判断した。信頼できる
  バイナリかどうかは、ダウンロード時の `.sha256` サイドカー照合(存在する場合)と、
  `init`/自動 bootstrap 時にアップロードした実体の sha256 を記憶しておくことで担保している
  (`update_policy = exact-digest-only`)。
- `isekai-terminal`(Android アプリ)と信頼ストア・トークンを共有する仕組みは無い。両者は
  完全に独立している。
- 複数ネットワーク環境(宅内 Wi-Fi NAT 配下 ↔ モバイル回線)をまたいだ実機ローミング検証は
  未実施。
- Windows は上記「前提」「Windows(ネイティブ SSH クライアント経路)」の通り、実 `ssh(1)`
  に依存しないネイティブ経路で公式サポートする(`ssh.exe` 不要)。ただし
  trust store・token ファイル・helper cache の保存先パスは `%LOCALAPPDATA%` ではなく
  `resolve_home_dir()`(`$HOME` → `%USERPROFILE%`)ベースの XDG 風パス
  (`%USERPROFILE%\.config\isekai-ssh` 等)のままで、Windows 流(`%LOCALAPPDATA%`)には
  なっていない(動作はするが非イディオマティック、今のところ意図的に未対応)。native 経路の
  非対応・非互換事項は「Windows(ネイティブ SSH クライアント経路)」節を参照。

## トラブルシューティング

- `isekai-ssh myhost` が「not a trusted host yet」相当のエラーを stderr に出して即座に終了する
  → `isekai-ssh init myhost ...` をまだ実行していないホスト(または自動 bootstrap の条件を
    満たしていない)。自動 bootstrap は既定で試みられる(`bootstrap-policy auto`)ので、
    このエラーが出た場合は大抵「`--isekai-helper-binary` も自動ダウンロードも両方失敗した」
    ケース——エラーメッセージ自体に次にすべきこと(`--isekai-helper-binary` を渡すか
    `init` を実行するか)が添えられているので、まずそれに従う。
- `~/.local/state/isekai/profiles/<host:port>.json` の中身を確認したい
  → 直接編集するより `isekai-pipe inspect --profile <host>`(`--json`/`--verbose` オプション
    あり)を使う方が安全。sha256 等の整合性は `init`/自動 bootstrap 側がプログラムで検証する
    ため、手で書き換えると整合性が壊れる。
- 登録済みのはずのホストに繋がらなくなった(リモートの `isekai-pipe serve` を再起動した
  覚えがある、等) → 通常は `isekai-ssh <host>` を打つだけで自動検知・自動復旧する(上記
  Quick Start 参照)。今どういう状態かを事前に確認したい・復旧を明示的に走らせたい場合は
  `isekai-ssh doctor <host>` を使う:

  ```bash
  isekai-ssh doctor myhost           # 診断のみ(接続を試み、段階別に結果を表示)
  isekai-ssh doctor myhost --fix     # stale trust を検知したらその場で再bootstrap(確認なし)
  ```

  `doctor`/`init`/`login`/`logout` は `isekai-ssh <host>` のホスト名解決より前に予約語として
  扱われるため、これらと同名のホスト(`doctor`/`init`/`login`/`logout` という名前のホスト)は
  `isekai-ssh <host>` の1コマンドでは接続できない(`~/.ssh/config` で別名を付けるか、
  `ssh <host>` を直接使うこと)。
- (Windows)`~/.ssh/config` の `IdentityFile ~/.ssh/...` の `~` は、native 経路では
  `openssh-config` クレートが `%USERPROFILE%`/`$HOME` を基準に展開する。以前の実 `ssh(1)`
  (MSYS2/Cygwin 版)経由で起きていた passwd データベース由来の `~` 展開の食い違いは、
  native 経路では発生しない(実 `ssh(1)` を起動しないため)。それでも鍵が見つからない
  場合は、`~/.ssh/config` に `IdentityFile` のフルパスを直接書けば確実。
