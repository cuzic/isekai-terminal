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

- Linux(x86_64 / aarch64)。musl 静的バイナリとしてビルドするので、配布物自体は特定ディストリ
  への依存が無い。
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

`isekai-ssh` は短命な `ConnectionIntent` をユーザー専用 runtime directory に保存してから
OpenSSH を起動し、`ProxyCommand isekai-pipe connect --profile <host> --service ssh --stdio` を
自動で差し込む。OpenSSH へ渡すのは `ISEKAI_INTENT_ID` だけで、session secret は argv/env には
載せない。OpenSSH のパスを変えたい場合は `--isekai-ssh-path PATH`、`isekai-pipe` のパスを
変えたい場合は `--isekai-pipe-path PATH` を使う。

既存の `~/.ssh/config` へ直接書く互換運用も残している。

```sshconfig
# ~/.ssh/config
Host myhost
    HostName 10.0.5.20
    ProxyCommand isekai-pipe connect --profile myhost --service ssh --stdio
    ServerAliveInterval 30
    ServerAliveCountMax 6
    TCPKeepAlive no
```

`ServerAliveInterval`/`ServerAliveCountMax`/`TCPKeepAlive no` は必須ではないが強く推奨する。
`isekai-pipe connect` は `ConnectionIntent` を atomic claim し、`isekai_transport` で
relay/STUN transport を直接起動して stdio bridge を所有する。relay mode では QUIC 接続が
切れても `--resume-window`(既定120秒、isekai-pipe serve 側の既定と揃えてある)の間は
resume を試み続けて `ssh` 側の stdin/stdout を閉じずに粘るので、
`ssh` 自身の生存確認(`ServerAliveInterval × ServerAliveCountMax`)は resume window より
十分長く設定しておくと、瞬断のたびに `ssh` 自身が先にセッションを諦めてしまう事故を防げる
(`ISEKAI_PIPE_DESIGN.md`「`ssh`自身の生存確認とのレース」節参照)。

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
| `resume-grace` | `<n>ms` / `<n>s` / `<n>`(単位省略時は秒) | `120s` | QUIC 接続が切れてから resume を試み続ける猶予時間 |
| `candidate-race-delay` | 同上の duration 表記 | `150ms` | 複数 candidate を同時に試す際の後発 candidate の遅延 |
| `relay-delay` | 同上の duration 表記 | `750ms` | direct 系 candidate に対して relay を遅らせて追い掛けさせる遅延 |
| `install-mode` | `user`\|`system` | `user` | `system` は sudo・所有権・rollback が未実装かつ実装予定も無いため、指定すると設定解決時点でエラーになる(fail closed)。将来必要になった場合もisekai-ssh本体には組み込まず、`curl ... \| sudo bash`的な別のインストーラースクリプト/ラッパーとして提供する想定 |
| `ctl-socket` | `yes`\|`no` | `no` | `yes` にすると、リモートの対話シェルから `isekai-pipe ctl title "<text>"` / `isekai-pipe ctl clip push --mime <mime>` を実行することで、tmux を経由せず直接ローカルのタブ/ターミナルのタイトルやクリップボードへ反映できるよう per-タブの UNIX domain socket forward(`-R`)を張る(`ISEKAI_PIPE_DESIGN.md` §8 Epic M参照)。明示的なリモートコマンドを指定した呼び出し(`isekai-ssh host 'some command'`)や unix 系以外の OS では黙って無効化される(opportunistic fallback) |

`bootstrap-candidate`/`link`/`rendezvous`/`stun`/`relay`/`service` は複数行書くと追記されていく。
それ以外(`enabled`/`bootstrap-policy`/`profile`/`remote-path`/`resume-grace`/
`candidate-race-delay`/`relay-delay`/`install-mode`/`ctl-socket`)は最初に出てきた値が採用される
(OpenSSH 本体の `Host`/`Match` と同じ first-value-wins 規則)。

### オプション: STUN による低遅延 P2P(`--mode stun`)

relay を経由しない直結を試したい場合、opt-in で使える。

```
ProxyCommand isekai-pipe connect --profile myhost --service ssh --stdio --mode stun --stun-server stun.example.com:3478
```

relay モードと違い、**セッション中に NAT マッピングが失われる(Wi-Fi⇔モバイル回線のローミング等)と
resume できず、その場でセッションが終了する**。低遅延を優先する代わりにこのリスクを受け入れる
場合にのみ使う(既定は常に relay モード)。

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
