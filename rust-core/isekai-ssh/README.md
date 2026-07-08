# isekai-ssh

`ssh(1)` のフロントエンドとして動く単体バイナリ。`isekai-terminal`(Android アプリ)が使っている
自作ヘルパー `isekai-pipe serve` 経由の QUIC 接続耐性(ローミング・瞬断からの resume・relay 経由の
NAT 越え)を、Android アプリに限らず手元の `ssh` からもそのまま使えるようにする。

設計の背景・各コマンドの詳細な契約は [`ISEKAI_PIPE_DESIGN.md`](../../ISEKAI_PIPE_DESIGN.md) を参照。
本ドキュメントは「実際に使うために何をすればいいか」だけに絞った利用者向けガイド。

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

取得したトークンは今のところ `init` に手動で渡す必要がある(自動連携は未実装、下記「既知の制限」
参照):

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
| `--via <JUMPHOST>` | この一度きりのデプロイ作業を踏み台経由で行う場合の `ssh -J` 相当 |
| `--helper-binary <PATH>` | アップロードする `isekai-pipe` バイナリ(必須) |
| `--relay-addr` / `--relay-sni` / `--relay-jwt` | isekai-pipe serve が relay へトンネルを張るための接続情報(必須) |
| `--idle-lifetime <SECS>` | 配置した isekai-pipe serve が無接続状態でも自己終了するまでの秒数。既定30日(2,592,000秒)。`isekai-ssh`(wrapper)は何時間・何日空けても同じ稼働中のプロセスにダイヤルし直すだけなので、Android アプリ向けの既定値(600秒)よりずっと長い値を明示的に渡している |
| `--helper-version` / `--release-channel` | 信頼ストアに記録するだけの表示用メタデータ |

成功すると `~/.config/isekai-ssh/known_helpers.toml` に host ごとのエントリ(公開鍵指紋相当の
`identity_pubkey`、バイナリの `sha256`、relay の公開アドレス等)が追記される。**このファイルが
`isekai-ssh`(wrapper)の信頼判定の正本**であり、未登録のホストへは(下記の自動 bootstrap 条件に
当てはまらない限り)fail closed で一切接続しない(下記「トラブルシューティング」参照)。

### 2'. 自動 bootstrap(relay を使わない場合、`init` の代わりに)

`--isekai-helper-binary` を渡すと、`isekai-ssh myhost` の初回実行時に
`direct-by-bootstrap-host` モード(relay も STUN も使わず、bootstrap 用の SSH 宛先へ直接
QUIC で到達する経路)に限り、自動で配布・確認・登録できる。relay/JWT が不要な代わりに、
接続元から bootstrap 用 SSH 宛先へ UDP/QUIC で直接到達できる場合(Tailscale・LAN・既知の
direct host 等)にしか使えない。

```bash
isekai-ssh --isekai-helper-binary /path/to/isekai-pipe myhost
```

`init` と同じ `[y/N]` 確認(identity・sha256 の表示)を経て `known_helpers.toml` に登録される。
複数 hop の `--via`(ProxyJump チェーン)には対応していない(単一 hop のみ)。

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

### オプション: STUN による低遅延 P2P(`--mode stun`)

relay を経由しない直結を試したい場合、opt-in で使える。

```
ProxyCommand isekai-pipe connect --profile myhost --service ssh --stdio --mode stun --stun-server stun.example.com:3478
```

relay モードと違い、**セッション中に NAT マッピングが失われる(Wi-Fi⇔モバイル回線のローミング等)と
resume できず、その場でセッションが終了する**。低遅延を優先する代わりにこのリスクを受け入れる
場合にのみ使う(既定は常に relay モード)。

## 既知の制限

- `isekai-ssh login`(RFC 8628)で取得したトークンは、まだ `init --relay-jwt` に自動連携されない。
  `jq` 等で手動で取り出して渡す必要がある。
- リリース署名の検証は未実装。信頼できるバイナリかどうかは `init`/自動 bootstrap 時に
  アップロードした実体の sha256 を記憶しておくことだけで担保している
  (`update_policy = exact-digest-only`)。
- 自動 bootstrap(`--isekai-helper-binary`)は `direct-by-bootstrap-host` モードのみ対応。
  relay/STUN 経由の自動 bootstrap は未実装(`init` を使うこと)。
- `isekai-terminal`(Android アプリ)と信頼ストア・トークンを共有する仕組みは無い。両者は
  完全に独立している。
- 複数ネットワーク環境(宅内 Wi-Fi NAT 配下 ↔ モバイル回線)をまたいだ実機ローミング検証は
  未実施。

## トラブルシューティング

- `isekai-ssh myhost` が「not a trusted host yet」相当のエラーを stderr に出して即座に終了する
  → `isekai-ssh init myhost ...` をまだ実行していないホスト(または自動 bootstrap の条件を
    満たしていない)。まず `init` を実行するか、`--isekai-helper-binary` を渡して再実行する。
- `~/.config/isekai-ssh/known_helpers.toml` の中身を直接確認・編集したい
  → TOML なので普通にテキストエディタで開ける。`[helpers."host:port"]` の形で1ホスト1
    エントリ。ただし手で書き換えるくらいなら `init` を再実行する方が安全(sha256 等の
    整合性はプログラムが検証する)。
