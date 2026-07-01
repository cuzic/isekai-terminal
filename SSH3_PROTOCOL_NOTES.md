# SSH3 プロトコル観察メモ（Phase 6-0）— ⚠️ ABANDONED（2026-07-01、Phase 6 は不採用）

> **このドキュメントが記録する SSH3 / Remote Terminal over HTTP/3 対応（Phase 6）は検討の末に実施しないことを
> 決定した。** 理由: IETF draft は個人提案のまま expired・WG採用の見込みなし、実装には h3 crate への
> パッチが必要など、SSH3 互換だけのために背負うコストが見合わないと判断したため。代替候補
> （draft-bider-ssh-quic、oowl/quicssh-rs）も採用に値しないことを確認済み。本ドキュメントは調査記録として
> 保持するのみで、以後の実装作業では参照しない。現行方針は `PLAN.md` の Phase 5（自前ヘルパー再設計）を参照。

本家 Go 実装（francoismichel/ssh3 v0.1.7、`/home/cuzic/ssh3` に clone）を実際にビルド・実行し、
Wire レベルの挙動をソース調査 + 実接続ログで確認した記録。Rust 実装時の一次情報として使う。

## 0. IETF 仕様の状況（2026-07-01 時点で確認）— ⚠️ ドラフトと実装は乖離している

「SSH3 という名前は無くなり Remote Terminal over HTTP/3 になった」という認識は **ドラフト文書上は正しいが、
実際に動いているコードには反映されていない**。両者の違いを正確に把握しておく必要がある。

### ドラフトの状態

- `draft-michel-remote-terminal-http3-00`（2024-07-31 提出、2024-08-01 承認）が唯一かつ最新のバージョン。
  **-01 は一度も出ていない**。個人提案（Individual Submission）のままで、QUIC WG にも HTTPBIS WG にも
  採用されていない。**2025-02-02 に expire 済み**（datatracker: https://datatracker.ietf.org/doc/draft-michel-remote-terminal-http3/ ）。
- RFC 化はされていない。著者 François Michel の唯一の RFC は無関係の RFC 9265（FEC/輻輳制御）。
- IETF 119 ALLDISPATCH（2024-03）で "Towards SSH3" として個人発表されたが、その後の議論は
  「SSH3 固有の話」ではなく「SSH 全般の近代化」という広いテーマに拡散し、新設された `ssh@ietf.org` の
  1,828 メッセージ中 **SSH3/HTTP3/QUIC/remote-terminal-http3 に言及したスレッドは 0 件**（実質的に立ち消え）。
  SSHM（Secure Shell Maintenance）WG のチャーターは「SSH の新しいトランスポートを定義すること」を
  明示的にスコープ外としており、採用先の WG も無い。
- 別の個人ドラフト `draft-bider-ssh-quic`（Denis Bider、2020年〜）が「HTTP/3 を介さず SSH を直接 QUIC に
  乗せる」という別方針で存在するが、SSH3 とは相互参照されておらず設計思想も異なる（参考情報として記録のみ）。

### ドラフト本文 vs 実際に動いている Go 実装（v0.1.7）— 相互運用の観点で重要

| 項目 | ドラフト -00 の記述 | 実際の Go 実装（v0.1.7、今回ビルドしたバイナリ） |
|------|---------------------|------------------------------------------------|
| `:protocol` 疑似ヘッダ | `remote-terminal` | **`ssh3`**（`client/client.go:272` `req.Proto = "ssh3"`） |
| JWT 必須クレーム | `sub`（`remote-terminal-<user>`）/ `iat` / `exp`。`jti` は任意 | `iss` / `iat` / `exp` / `sub`（`"ssh3"` 固定）/ `aud`（`"unused"`）/ `client_id` / **`jti`（必須）** |
| ConversationID / TLS Exporter | **記載なし**。JWT を接続にバインドする仕組みの言及自体が無い | **サーバー側で厳格に検証**（`auth/plugins/pubkey_authentication/server/server_plugin.go:62-63`:
  `if jti, ok := claims["jti"]; !ok || jti != base64ConversationID { ... 拒否 }`） |

**結論**: 「SSH3 → Remote Terminal over HTTP/3」への改名は **仕様ドラフト文書だけで起きたもの**で、
GitHub リポジトリ名・バイナリの挙動・ワイヤーフォーマットは今も `ssh3` のまま何も変わっていない。
ドラフト自体も個人提案のまま 17 ヶ月 expire 状態で放置されており、実質的に「凍結」している。

**この Phase 6 の実装方針への影響**: 我々の目的は「実際に運用されている SSH3 サーバーにログインできること」
であり、そのようなサーバーは全て今回検証した Go 実装（またはその派生）で動いている。ドラフト文書の
簡略化された認証方式（TLS Exporter 不要）に合わせて実装しても、**実際の `ssh3-server` には接続できない**
（`jti` 検証で拒否される）。したがって、**本ドキュメントでこれまで記録してきた実装ベースの仕様
（`:protocol: ssh3`、TLS Exporter 由来の ConversationID を `jti` に埋め込む JWT）を正として実装を進める**
方針に変更はない。逆に言えば、ドラフトが凍結・不採用のおかげで「仕様が今後変わって互換性が壊れる」
リスクは低く、Go 実装自体のバージョン追従だけを気にすればよい状況だと分かった。

## 検証環境

```
git clone https://github.com/francoismichel/ssh3.git /home/cuzic/ssh3
cd /home/cuzic/ssh3 && go build -o bin/ ./cmd/...
./bin/ssh3-server -generate-selfsigned-cert -cert cert.pem -key priv.key
SSH3_LOG_FILE=./server.log ./bin/ssh3-server -bind 127.0.0.1:4433 -cert cert.pem -key priv.key \
  -url-path /ssh3-term -enable-password-login -v
./bin/ssh3 -v -insecure -privkey ./spike_key "$(whoami)@127.0.0.1:4433/ssh3-term" echo hi
```

鍵認証は `~/.ssh3/authorized_identities`（`~/.ssh/authorized_keys` と同じ形式）にテスト用 ed25519
公開鍵を登録して検証した。**サンドボックス環境では `fork/exec /bin/bash: operation not permitted`
でシェル起動自体は失敗する**が、CONNECT〜認証〜チャネルオープンの protocol handshake は最後まで
成功することを確認済み（後述ログ参照）。

## 1. トランスポート層

- QUIC + TLS 1.3（本家は quic-go 0.40 系）。ハンドシェイク完了後、`tls.ConnectionState.ExportKeyingMaterial("EXPORTER-SSH3", nil, 32)` で
  32byte の **ConversationID** を導出する（`conversation.go:44`）。これは TLS Exporter (RFC 5705) ベースで、
  接続ごとに一意になる。JWT の replay 対策として使われる（後述）。
- HTTP/3 は **Extended CONNECT**（RFC 9220）。Go 実装では `http.NewRequest("CONNECT", url, nil)` の後に
  `req.Proto = "ssh3"` をセットするだけで、quic-go の http3 RoundTripper が `:protocol: ssh3` 疑似ヘッダ付きの
  Extended CONNECT に変換する（`client/client.go:268-272`）。
- HTTP Datagram（RFC 9297）は **UDP port forwarding 専用**。通常の shell/exec セッションでは使わない
  （bidirectional QUIC stream で完結する）。

## 2. リクエスト行 / ヘッダ

実接続時に観測した実際の値：

```
CONNECT https://127.0.0.1:4433/ssh3-term?user=cuzic HTTP/3
:protocol: ssh3
user-agent: SSH 3.0 francoismichel/ssh3 0.1.7 experimental_spec_version=alpha-00
authorization: Bearer <JWT>          # 公開鍵認証時
# または
authorization: Basic <base64(user:pass)>   # パスワード認証時（-enable-password-login 必須）
```

サーバー応答: `200 OK` で認証成功、`ssh3-term` 以外の path には `404 Not Found`（secret path 機能）。

## 3. JWT Bearer 認証（公開鍵）

`client_auth.go:328` `BuildJWTBearerToken()`:

```go
jwt.MapClaims{
    "iss":       username,
    "iat":       now,
    "exp":       now + 10*time.Second,   // 有効期限は10秒のみ
    "sub":       "ssh3",
    "aud":       "unused",
    "client_id": "ssh3-" + username,
    "jti":       base64(ConversationID),  // TLS Exporter 由来。接続に紐付く replay 対策
}
```

署名アルゴリズムは鍵の型から自動選択（ed25519 → EdDSA、RSA → RS256 系）。
サーバー側は `~/.ssh3/authorized_identities` → `~/.ssh/authorized_keys`（この順）を走査し、
一致する公開鍵で署名検証する。**OpenSSH の `authorized_keys` をそのまま流用できる**のが利点。

## 4. チャネル / メッセージフレーミング（`message/` パッケージ）

CONNECT が 200 OK になった時点のストリームがそのまま "conversation" になり、
以降のチャネルは新規 QUIC bidi stream として開く。メッセージは **RFC 4254 (SSH Connection Protocol) の
メッセージ番号をそのまま再利用**し、SSH バイナリパケットプロトコルではなく **varint 長プレフィックス**で
フレーミングする（`message/message.go`）。

```
SSH_MSG_CHANNEL_OPEN_CONFIRMATION = 91
SSH_MSG_CHANNEL_OPEN_FAILURE      = 92
SSH_MSG_CHANNEL_DATA              = 94
SSH_MSG_CHANNEL_EXTENDED_DATA     = 95
SSH_MSG_CHANNEL_EOF               = 96
SSH_MSG_CHANNEL_CLOSE             = 97
SSH_MSG_CHANNEL_REQUEST           = 98
SSH_MSG_CHANNEL_SUCCESS           = 99
SSH_MSG_CHANNEL_FAILURE           = 100
```

`message/channel_request.go` に定義されている ChannelRequest 実装（= RequestType 文字列）:

```
pty-req / shell / exec / subsystem / window-change / signal / exit-status / exit-signal / x11-req / forwarding-request
```

いずれも OpenSSH の RFC4254 実装とフィールド構成がほぼ同一。**SSH3 は「輸送層とハンドシェイクだけを
HTTP/3 に置き換え、Connection Protocol のセマンティクスはそのまま流用」**という設計であることが
ソースからも実接続ログからも確認できた。

## 5. 実接続ログ（抜粋、鍵認証成功 → exec 送信まで）

```
[client] send CONNECT request on URL https://127.0.0.1:4433/ssh3-term?user=cuzic, User-Agent="SSH 3.0 francoismichel/ssh3 0.1.7 experimental_spec_version=alpha-00"
[client] got response with 200 OK status code
[client] opened new session channel
[client] sent exec request for command "echo hello-from-ssh3"

[server] pubkey auth plugin: parse identity string
[server] parsing ssh authorized key / parsing ssh-ed25519 identity
[server] token method: EdDSA, pubkey = ed25519.PublicKey [...]
[server] request for user cuzic successfully verified by plugin
[server] got request: method: CONNECT, URL: https://127.0.0.1:4433/ssh3-term?user=cuzic
[server] error while processing message: ...: fork/exec /bin/bash: operation not permitted   # サンドボックス制約。プロトコル自体は成功
```

## 6. Rust 実装（Phase 6 以降）への示唆

1. **quiche には TLS Exporter API が存在しないことを確認済み（結論: quinn に方針変更）**。
   quiche の Rust 公開 API（`quiche::Connection`）を確認したが `export_keying_material` 相当のメソッドは無く、
   quiche リポジトリ全体（219 `.rs` ファイル）を grep しても exporter 関連コードは 0 件。内部で使う BoringSSL
   バインディング（`boring` crate）自体は `SSL_export_keying_material` を持つが、quiche はその `SslRef` を
   一切外部に公開していないため、フォークしない限り到達できない。
   一方 **quinn（rustls ベース）は `Connection::export_keying_material()` を安定 API として提供**しており、
   [quinn-rs/quinn#834](https://github.com/quinn-rs/quinn/issues/834) はまさに「アプリ層認証のバインディング」
   目的で要望・実装された経緯があり、SSH3 の ConversationID ユースケースと直接一致する。
   このプロジェクトは UniFFI 経由で Rust コアを Kotlin に渡す構成であり、Phase 5（tsshd）で既に quinn 0.11 を
   統合済みなので、quiche を選ぶ根拠だった「公式 C FFI・AOSP 実績」はこのアーキテクチャでは意味を持たない。
   **→ Phase 6 の QUIC/HTTP3 スタックは quinn に決定。**
2. Extended CONNECT は `:protocol` 疑似ヘッダに `ssh3` をセットするだけで良く（Go 実装は `req.Proto = "ssh3"` を
   セットするのみ）、quinn + hyperium/h3（or 自前の薄い Extended CONNECT 実装）でも同程度の実装難度になる見込み。
   hyperium/h3 が experimental である点は引き続きリスクとして 6-A で評価する。
3. チャネルメッセージ層は RFC4254 のメッセージ構造を varint フレーミングで包むだけなので、
   **russh（既存依存）の内部型定義やパーサーロジックを大部分流用できる可能性が高い**。ゼロから再設計する
   必要はなく、「russh の binary packet framing を varint framing に差し替える」という捉え方の方が近い。
4. 認証は JWT Bearer（EdDSA）を最優先実装する。`~/.ssh3/authorized_identities` 形式はサーバー実装時の
   参考にする（今回は Android クライアントのみ実装するため直接関係しないが、将来の自前サーバー実装時に有用）。
5. **PTY/shell の実際の入出力挙動はこの開発サンドボックスでは検証不能**。`dangerouslyDisableSandbox` を
   使っても ssh3-server プロセスの `fork/exec /bin/bash: operation not permitted` は解消しなかった
   （コンテナ全体に適用された seccomp/権限制約と見られ、単一ツール呼び出し単位のサンドボックス解除では
   突破できない）。**別途 Linux 実機 or 権限制限のないコンテナ環境で pty-req → shell → data の実データフローを
   fixture 化する必要がある**（Phase 6-0 の残タスク、ユーザー自身の環境での実施が必要）。

## 7. Phase 6-A スパイク結果（quinn + h3、デスクトップ実行）

コード: `/home/cuzic/ssh3/rust-quinn-spike`（`cargo run` で再現可能。quinn 0.11 / rustls 0.23 /
h3 0.0.8 / h3-quinn 0.0.10、rust-core と同系統バージョン）。

### 7-1. TLS Exporter（RFC 5705） — ✅ 成功

client/server 双方で `quinn::Connection::export_keying_material(&mut buf, b"EXPORTER-SSH3", b"")` を
呼び出し、**完全に一致する 32byte を取得**できた。

```
[client] TLS-exported keying material: e1c00633504f46ba93c72b8ac07de70c845d6cf73f4c3f6613bf9e79abee9bf5
[server] TLS-exported keying material: e1c00633504f46ba93c72b8ac07de70c845d6cf73f4c3f6613bf9e79abee9bf5
```

quinn を採用する最大の動機（quiche には無い機能）が実地で確認できた。SSH3 の ConversationID 導出に
そのまま使える。

### 7-2. Extended CONNECT（RFC 9220） — ✅ 配線は成功、⚠️ カスタム `:protocol` 値に制約あり

h3 (`h3::client::builder().enable_extended_connect(true)`) + h3-quinn で CONNECT リクエストを送信し、
サーバー側で `method=CONNECT` と `:protocol` 疑似ヘッダの両方を正しく受信・パースできることを確認した。

```
[server] received request: method=CONNECT uri=https://localhost/ssh3-term?user=cuzic protocol_ext=Some(Protocol(WebTransport))
[client] got response status: 200 OK
```

ただし、**`h3::ext::Protocol` は `Protocol::WEB_TRANSPORT` / `Protocol::CONNECT_UDP` の2つの定数しか
公開されておらず、内部の `ProtocolInner` enum は非公開（private）**。そのため SSH3 が使う
`:protocol: ssh3` という任意文字列を外部クレートから構築する手段が無い（`ProtocolInner` に
`Custom(String)` 相当のバリアントが無く、`FromStr` も `"webtransport"` / `"connect-udp"` の2値しか
受け付けない）。今回のスパイクではこの制約を明示するため、意図的に `Protocol::WEB_TRANSPORT` を使って
「配線自体は通る」ことだけを証明した。

**対応方針（Phase 6-B で着手）**: GitHub 調査の結果、既存のフォーク・パッチ・代替 crate は存在しないが、
明確な前例が見つかった。

- `hyperium/h3` の最新 main ブランチでは `ProtocolInner` は `WebTransport` / `ConnectUdp` / `ConnectIp`
  （RFC 9484、[PR #273](https://github.com/hyperium/h3/pull/273)）/ `WebSocket`（RFC 9220、
  [PR #236](https://github.com/hyperium/h3/pull/236)）の4つに増えているが、依然として **closed enum のまま**
  （任意文字列は不可）。
- 重要なのは、**この4つはいずれも「`ProtocolInner` に1バリアント追加 + `Protocol::CONST` + `as_str()`/`from_str`
  の対応を足すだけ」という最小差分パターンで実装され、メンテナに2回マージされている実績がある**こと
  （メンテナ自身も [issue #293](https://github.com/hyperium/h3/issues/293) で汎用化の必要性を認識しているが
  未実装・timeline 無し）。
- **→ 同じパターンで `Protocol::SSH3`（`"ssh3"`）をローカルにパッチする**（`ext.rs` に数行〜十数行程度）。
  将来的に本家へ同型の PR を出す選択肢も残しておく（過去に類似 PR が2回受理されている）。
- h3 の `proto::headers::Header::request()` は `pub` なので、`Protocol` の構築さえ回避できれば
  QPACK 層を直接叩いて `:protocol` を組み立てる迂回路もあるが、上記のローカルパッチの方が保守しやすい。
- SSH3 の Rust 実装・h3 のフォーク・crates.io 上の代替パッケージは存在しない（GitHub 調査で確認済み）。
  見つかったのは無関係な「SSH over QUIC」トンネリングツール（`imsk17/quicssh-rs` 等、Extended CONNECT 自体を
  使わない生パケットトンネル方式）のみ。

## 未検証・持ち越し事項

- [ ] PTY 確立後の実データフロー（stdin/stdout のバイト列、window-change のタイミング）※ サンドボックス環境では fork/exec 制限のため検証不可、実機/別環境が必要
- [x] quiche の TLS Exporter API 有無 → **無し**。quinn の `export_keying_material()` を採用する方針に変更（§6-1参照）
- [x] quinn の TLS Exporter がデスクトップ Rust で動くか → **動く、client/server 一致確認済み**（§7-1）
- [x] h3 + quinn で Extended CONNECT が成立するか → **成立する**が `:protocol` に任意文字列を送れず要パッチ（§7-2）
- [ ] quinn + h3（パッチ版）が cargo-ndk で Android 向けにクロスコンパイルできるか（6-A の残タスク）
- [ ] h3 の `Protocol` に任意文字列コンストラクタを追加するフォークパッチの実装・動作確認（6-B）
- [ ] `-insecure` を外した状態（正式な X.509 証明書検証）での接続
- [ ] UDP port forwarding（HTTP Datagram 経路）の実挙動 ※ Phase 6 では対象外だが将来のため
- [ ] OIDC 認証フローの実際のリダイレクト・トークン交換

## 参照

- clone 先: `/home/cuzic/ssh3`（このリポジトリの外、参照用）
- ビルド済みバイナリ: `/home/cuzic/ssh3/bin/{ssh3,ssh3-server}`
- 実行ログ: `/home/cuzic/ssh3/spike/{server.stdout,client_run2.log}`
