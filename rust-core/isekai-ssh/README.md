# isekai-ssh

`ssh(1)` の `ProxyCommand` に差し込んで使う単体バイナリ。`isekai-terminal`(Android アプリ)が
使っている自作ヘルパー `isekai-helper` 経由の QUIC 接続耐性(ローミング・瞬断からの resume・relay
経由の NAT 越え)を、Android アプリに限らず手元の `ssh` からもそのまま使えるようにする。

設計の背景・各コマンドの詳細な契約は [`ISEKAI_SSH_DESIGN.md`](../../ISEKAI_SSH_DESIGN.md) を、
`isekai-helper` 側のワイヤプロトコルは [`HELPER_PROTOCOL.md`](../../HELPER_PROTOCOL.md) を参照。
本ドキュメントは「実際に使うために何をすればいいか」だけに絞った利用者向けガイド。

## 前提

- Linux(x86_64 / aarch64)。musl 静的バイナリとしてビルドするので、配布物自体は特定ディストリ
  への依存が無い。
- 接続先ホストに `ssh` で(パスワードでも鍵でも)ログインできること。`isekai-ssh init` は
  この既存の SSH 接続を使って `isekai-helper` を配置する。
- 接続先ホストが到達可能な isekai-link relay(`isekai-helper --relay` が張るトンネル先)の
  エンドポイント(`ADDR:PORT` と TLS SNI)、およびそこへ認証するための JWT。relay 自体の
  構築・運用はこのドキュメントの範囲外(`seera-networks/ISEKAI-link` 等)。

## インストール

まだ GitHub Release としては配布していない(`ISEKAI_SSH_DESIGN.md` "S-7実施結果" 参照)ので、
現状はソースからビルドする。

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

`isekai-ssh init` が接続先ホストに配置する `isekai-helper` 本体も、同じ手順で musl バイナリを
用意しておく必要がある(`build-isekai-helper-musl.sh`)。手元のバイナリと同じアーキテクチャの
サーバーに配る場合は、既に GitHub Release で配布済みのもの(`HELPER_PROTOCOL.md` 参照)を
使ってもよい。

## 使い方

### 1. relay へのアクセストークンを取得する(`login`)

isekai-helper が relay(MASQUE)へ認証するための JWT を、RFC 8628 Device Authorization Grant で
取得する。OAuth のエンドポイントは環境ごとに異なるため、relay の運用者から教えてもらった値を渡す。

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

### 2. ホストへ isekai-helper を配置し、信頼登録する(`init`)

ホストごとに一度だけ行う対話的な作業。既存の `ssh` 接続(公開鍵認証済みが望ましい)を使って
`isekai-helper` バイナリをアップロード・起動し、その場で得たハンドシェイク(公開鍵指紋相当の
`cert_sha256` など)を確認したうえで `[y/N]` の明示的な承認をしてから信頼ストアに登録する。

```bash
isekai-ssh init myhost \
  --helper-binary   /path/to/isekai-helper \
  --relay-addr      relay.example.com:4433 \
  --relay-sni       relay.example.com \
  --relay-jwt       "$RELAY_JWT"
```

主なオプション:

| オプション | 説明 |
|---|---|
| `<HOST>` | `myhost` / `myhost:2222` / `user@myhost` のいずれか。`~/.ssh/config` の `Host` と揃えておくと分かりやすい |
| `--via <JUMPHOST>` | この一度きりのデプロイ作業を踏み台経由で行う場合の `ssh -J` 相当 |
| `--helper-binary <PATH>` | アップロードする `isekai-helper` バイナリ(必須) |
| `--relay-addr` / `--relay-sni` / `--relay-jwt` | isekai-helper が relay へトンネルを張るための接続情報(必須) |
| `--idle-lifetime <SECS>` | 配置した isekai-helper が無接続状態でも自己終了するまでの秒数。既定30日(2,592,000秒)。`connect` は何時間・何日空けても同じ稼働中の isekai-helper にダイヤルし直すだけなので、Android アプリ向けの既定値(600秒)よりずっと長い値を明示的に渡している |
| `--helper-version` / `--release-channel` | 信頼ストアに記録するだけの表示用メタデータ |

成功すると `~/.config/isekai-ssh/known_helpers.toml` に host ごとのエントリ(公開鍵指紋相当の
`identity_pubkey`、バイナリの `sha256`、relay の公開アドレス等)が追記される。**このファイルが
`connect` の信頼判定の正本**であり、未登録のホストへは `connect` は fail closed で一切接続しない
(下記「トラブルシューティング」参照)。

### 3. 日常的に接続する(`connect` を `ProxyCommand` に登録する)

`~/.ssh/config` に一度書いておけば、以後は普段どおり `ssh myhost` を打つだけでよい。

```
# ~/.ssh/config
Host myhost
    HostName 10.0.5.20
    ProxyCommand isekai-ssh connect myhost
    ServerAliveInterval 30
    ServerAliveCountMax 6
    TCPKeepAlive no
```

`ServerAliveInterval`/`ServerAliveCountMax`/`TCPKeepAlive no` は必須ではないが強く推奨する。
`isekai-ssh connect` は QUIC 接続が切れても `--resume-window`(既定120秒、isekai-helper 側の
既定と揃えてある)の間は resume を試み続けて `ssh` 側の stdin/stdout を閉じずに粘るので、
`ssh` 自身の生存確認(`ServerAliveInterval × ServerAliveCountMax`)は resume window より
十分長く設定しておくと、瞬断のたびに `ssh` 自身が先にセッションを諦めてしまう事故を防げる
(`ISEKAI_SSH_DESIGN.md`「`ssh` 自身の生存確認とのレース」参照)。

`connect` は **`init` で登録済みのホストにしか接続しない**。それ以外は何もしない(標準出力へは
1バイトも書かない)。ログ・進捗は全て標準エラーへ出るので、`ssh` から見える標準出力を汚さない。

### オプション: STUN による低遅延 P2P(`--mode stun`)

relay を経由しない直結を試したい場合、opt-in で使える。

```
ProxyCommand isekai-ssh connect myhost --mode stun --stun-server stun.example.com:3478
```

relay モードと違い、**セッション中に NAT マッピングが失われる(Wi-Fi⇔モバイル回線のローミング等)と
resume できず、その場でセッションが終了する**。低遅延を優先する代わりにこのリスクを受け入れる
場合にのみ使う(既定は常に relay モード)。

## 既知の制限

- `isekai-ssh login`(RFC 8628)で取得したトークンは、まだ `init --relay-jwt` に自動連携されない。
  `jq` 等で手動で取り出して渡す必要がある。
- リリース署名の検証は未実装。信頼できるバイナリかどうかは `init` 時にアップロードした実体の
  sha256 を記憶しておくことだけで担保している(`update_policy = exact-digest-only`)。
- `isekai-terminal`(Android アプリ)と信頼ストア・トークンを共有する仕組みは無い。両者は
  完全に独立している。
- 複数ネットワーク環境(宅内 Wi-Fi NAT 配下 ↔ モバイル回線)をまたいだ実機ローミング検証は
  未実施(`PLAN.md` "Phase S-7" 参照)。

## トラブルシューティング

- `connect` が `stderr` に "not registered" 相当のエラーを出して即座に終了する
  → `isekai-ssh init` をまだ実行していないホスト。まず `init` を実行する。
- `~/.config/isekai-ssh/known_helpers.toml` の中身を直接確認・編集したい
  → TOML なので普通にテキストエディタで開ける。`[helpers."host:port"]` の形で1ホスト1
    エントリ。ただし手で書き換えるくらいなら `init` を再実行する方が安全(sha256 等の
    整合性はプログラムが検証する)。
- `isekai-ssh connect` の終了コードが `10` → 信頼ストア未登録(上記参照)。それ以外の
  非ゼロ終了は `1`(その他のエラー、詳細は stderr)。
- `--dev-insecure-*` という名前のフラグを見かけても本番ビルドの `--help` には出てこない
  (`cfg(all(debug_assertions, feature = "dev-insecure"))` でリリースビルドには一切コンパイル
  されない)。存在しても使わないこと——信頼ストアのチェックを迂回する開発/テスト専用の抜け穴。
