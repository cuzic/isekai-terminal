# isekai-helper CLI / ワイヤープロトコル契約（Phase 7-0）

Phase 7（自作ヘルパー方式による QUIC 接続耐性）の実装に先立ち、`isekai-helper` の CLI インターフェースと
ワイヤー上の契約をここで固定する。Phase 7-1（最小実装）はこの契約に従う。

背景・前提は `PLAN.md` の「Phase 7: 自作ヘルパー方式による QUIC 接続耐性」節を参照。

---

## 1. CLI 契約

```
isekai-helper [OPTIONS]

OPTIONS:
  --target <ADDR:PORT>          中継先（既定: 127.0.0.1:22）
  --bind <ADDR:PORT>            QUIC バインドアドレス。ポート 0 で OS に空きポートを選ばせる（既定: 0.0.0.0:0）
  --idle-timeout <SECS>         QUIC トランスポートの max_idle_timeout（既定: 30）
  --max-idle-lifetime <SECS>    アクティブな接続が無く、かつ新規接続も来ない状態が続いたら自己終了するまでの秒数（既定: 600）
  --once                        1回の接続が終了したら常駐せず終了する（既定は常駐し、次の接続を待つ）
  --log-level <LEVEL>           error|warn|info|debug|trace（既定: info）。stderr にのみ出力する
  --version                     バージョン表示して終了
  --help
```

**設計判断**:
- `--token` のような CLI 引数は用意しない。`session_secret` は helper が起動時に自身で生成し、
  標準出力経由で呼び出し元（SSH exec 元）に返す（後述）。CLI 引数や環境変数に秘密情報を載せると、
  マルチユーザーサーバーで `ps aux` や `/proc/<pid>/environ` から読める可能性があるため避ける。
- `--once` を既定 OFF（常駐）にしたのは、QUIC connection が一時的に切れて新しい接続が来た場合に
  ヘルパーの再配布・再起動をせずに済ませるため（Phase 8 の resume ではなく、単なる「新しい接続を
  同じ helper プロセスで受け付ける」挙動）。ただし `--max-idle-lifetime` で放置されたプロセスは
  自己終了する。
- `--max-idle-lifetime`（旧称 `--max-lifetime-idle` から改称。意味を明確化）: 起動直後だけでなく
  **各接続終了後にもこの idle timer を再開する**。「1回だけ接続が来て、その後は誰も繋がずに
  永遠にプロセスが残り続ける」事故を防ぐため、常に「アクティブ接続が無い期間」を計測する。

### 終了コード

| コード | 意味 |
|---|---|
| 0 | 正常終了（SIGTERM/SIGINT、`--max-idle-lifetime` 到達、または `--once` 指定時の接続正常終了） |
| 64 | 使用方法エラー（CLI 引数不正、`sysexits.h` の `EX_USAGE` に準拠） |
| 73 | bind 失敗（ポート使用中等、`EX_CANTCREAT` に準拠） |
| 1 | その他の一般エラー |

### ログ

- ログは **stderr にのみ**出力する（`--log-level` で制御）。
- **標準出力（stdout）はハンドシェイク JSON 専用**とし、ログを一切混在させない。
  SSH ブートストラップ側は stdout の1行目だけをパースするため、ここが汚染されると壊れる。
- `session_secret` や `proof` 等の機微情報はログに出力しない。

---

## 2. 起動ハンドシェイク（stdout）

bind に成功した直後、accept ループに入る前に、**1行だけ** JSON を stdout に出力する。

```json
{"v":1,"listen_port":45231,"cert_sha256":"3a7f...（hex, 64文字）","session_secret":"base64エンコードされた32byte"}
```

- `v`: ハンドシェイクフォーマットのバージョン（将来の破壊的変更に備える）
- `listen_port`: 実際に bind されたポート番号（`--bind` で 0 を指定した場合、OS が選んだ実ポート）
- `cert_sha256`: 自己署名証明書（起動のたびに生成する ephemeral cert）の DER 全体の SHA-256（hex）。
  クライアントは QUIC 接続時にこの値でピン留めする（証明書 fingerprint は QUIC 接続時の素の TOFU ではなく、
  **既に認証済みの SSH チャネル経由**で受け渡す設計。PLAN.md「セキュリティ」節参照）
- `session_secret`: helper がその起動ごとにランダム生成する秘密（base64）。クライアントはこれを使って
  `proof` を計算する（後述）。

SSH 側の呼び出し例（ブートストラップスクリプトのイメージ、Phase 7-3 で実装・実機検証済み）:

```bash
mkdir -p -m 0700 ~/.cache/isekai-terminal
( setsid ~/.local/bin/isekai-helper --target 127.0.0.1:22 \
  </dev/null >~/.cache/isekai-terminal/helper.handshake 2>~/.cache/isekai-terminal/helper.log & )
chmod 0600 ~/.cache/isekai-terminal/helper.handshake
# 1行目が読めるまで短い timeout 付きでポーリングする（固定 sleep 0.2 に頼らない）
```

**`cmd & disown` ではなく `( cmd & )` というサブシェル二重 fork にすること（実機検証で判明した重要な差異）**:
実機で検証したところ、`setsid isekai-helper ... & disown`（`disown` 引数無し、`disown -a` いずれも）では、
SSH exec チャネルを実行している `bash -c "..."` 本体がスクリプト終了後も `do_wait()` に留まり、
長時間稼働する isekai-helper（デーモンなので自然終了しない）の終了を待ち続けてチャネルがハングし続ける
不具合を確認した。原因は完全には特定していないが、`setsid` の内部 fork 挙動と bash の "current job" 追跡が
噛み合わないためと推測している。`( setsid cmd & )` のようにサブシェルで一段包むと、外側シェルの直接の
子はサブシェル（即座に終了）だけになり、isekai-helper は孫プロセスとして完全に独立するため解消する。
`disown` は不要（`setsid` 自体が新しいセッションを作るため SIGHUP 到達防止の目的は既に満たされている）。

`setsid`（または同等の手段）で SSH セッション終了時の SIGHUP から切り離し、SSH exec チャネルが
閉じた後もプロセスが生き続けるようにする。

**ファイル権限契約**: `helper.handshake` には `session_secret` が平文で含まれるため、これは実質的な
bearer secret として扱う。ブートストラップ側（Phase 7-3）は以下を **必須**とする。

- 出力先ディレクトリ（`~/.cache/isekai-terminal/` 等）は `0700` で作成する
- `helper.handshake` ファイルは `0600` で作成する（`umask` に依存しない明示的な `chmod`）
- 既存ファイルを使い回す場合は所有者と permission を検査し、不適切なら失敗させる（他ユーザーが
  読めるパーミッションのファイルは信用しない）

**stdout flush 契約**: helper は起動ハンドシェイク JSON と末尾改行を書き出した後、**accept ループに
入る前に stdout を明示的に flush しなければならない**（stdout がファイルにリダイレクトされる場合、
行バッファを前提にできないため）。flush に失敗した場合は accept ループに入らず終了する
（exit code 1）。

---

## 3. QUIC / TLS 契約

- ALPN: `isekai-helper/1`（バージョン付き。将来の破壊的変更は `/2` にし、旧クライアントは
  自然にネゴシエーション失敗するようにする）
- TLS: 起動のたびに自己署名証明書を生成する（永続化しない、ephemeral）。fingerprint はハンドシェイク
  JSON で報告する。
- **0-RTT はクライアント・サーバー双方で完全に無効化する**（片側だけでは不十分なので契約として両方に明記する）:
  - クライアントは `quinn::Connecting::into_0rtt()` を使用しない（常に通常のハンドシェイク完了を待つ）
  - サーバー（helper）は TLS session resumption / early data を受け付けない設定にする
  - サーバーは 0-RTT application data を一切処理してはならない（届いても無視・破棄する）
  - `session_secret` に基づく HMAC 認証があるため実害は限定的だが、仕様として「早期データは存在しない」
    と固定する
- **`preferred_address` は使用しない**（QUIC-Exfil 対策、PLAN.md 参照）。
- **quinn の `TransportConfig` で stream 数を明示的に縛る**（アプリ層での reset だけに頼らない）:
  - `max_concurrent_bidi_streams = 1`
  - `max_concurrent_uni_streams = 0`
  - `datagram_receive_buffer_size = None`（datagram は使わないため無効化する）
- **keep-alive の責務は helper 側に一本化する**: `keep_alive_interval` を `idle_timeout / 3` 程度に
  helper 側で設定する。Android クライアント（バックグラウンド時に OS のスケジューリング制約を受けやすい）
  に keep-alive の責務を持たせず、常時起動している helper 側で確実に送出する。
- PATH_CHALLENGE / NEW_CONNECTION_ID のデフォルトのキュー上限を確認し、無ければレート制限を追加する
  （Phase 7-1 で quinn のデフォルト挙動を確認）。
- **依存バージョン確認済み**: `quinn-proto` の out-of-order stream reassembly による remote memory
  exhaustion（[GHSA-4w2j-m93h-cj5j](https://github.com/quinn-rs/quinn/security/advisories/GHSA-4w2j-m93h-cj5j)、
  patched: `>= 0.11.15`）について、`rust-core/Cargo.lock` と `rust-quinn-spike/Cargo.lock` の双方で
  既に `quinn-proto 0.11.15` がロックされていることを確認済み（2026-07-01）。isekai-helper の
  Cargo.lock でも同様に `>= 0.11.15` を維持することを CI/レビューで確認する。

---

## 4. Stream / フレーム契約

**Phase 7 のスコープでは、1 QUIC connection につき 1 つの client-initiated bidirectional stream のみを
使う。** それ以外の stream が開かれた場合、helper は reset する（将来のポートフォワード拡張用に予約）。

### ハンドシェイクフレーム（stream の先頭のみ）

クライアント → helper（`HELLO`）:

```
byte 0:      0x01 (HELLO)
byte 1..33:  proof（32 byte）
```

`proof = HMAC-SHA256(session_secret, exporter)`
`exporter = quic_connection.export_keying_material(label = b"isekai-helper-auth-v1", context = b"", length = 32)`

**エンコーディングの厳密な規定**（実装差異を避けるため固定する）:
- `session_secret`: RFC 4648 **standard** alphabet、**padding あり**の base64。decode 後ちょうど 32 byte。
- `cert_sha256`: leaf certificate の DER 全体の SHA-256、**lowercase hex 64文字**、区切り文字なし。
- `proof` の比較は **constant-time equality** で行う（タイミング攻撃対策）。
- exporter の `label` は ASCII byte string `"isekai-helper-auth-v1"`、`context` は zero-length byte string。

helper → クライアント（応答、1 byte）:

| 値 | 意味 |
|---|---|
| `0x02` (ACK) | proof 検証成功・`--target` への TCP 接続にも成功。この直後からストリームは生の双方向パイプになる |
| `0xFE` (REJECT_DUPLICATE) | proof は正しいが、同じ `session_secret` に紐づく接続が既にアクティブ（同時アクティブ接続は1本まで） |
| `0xFF` (REJECT_AUTH) | proof が不正 |
| `0xFC` (REJECT_TARGET) | proof は正しいが `--target` への TCP 接続に失敗した |
| `0xFD` (REJECT_UNSUPPORTED) | 未知/未対応のフレームタイプを受信した（Phase 8 の `0x03 RESUME` 等、将来の拡張を旧 helper が安全に拒否するための予約） |

`0x03`（`RESUME`）は **Phase 8 用に予約**し、Phase 7 の helper では未実装（受信したら `0xFD` を返す）。

**ハンドシェイクの処理順序（固定）**: `0x02 ACK` を送出した後は生のバイトパイプになり、そこから
エラーを表現する手段が無くなる。したがって、ACK を返す前に可能な限りのエラーを検出しておく。

```
1. HELLO の proof を検証する（不正なら 0xFF）
2. 同一 session_secret の active slot が既に使用中でないか確認する（使用中なら 0xFE）
3. --target への TCP 接続を試行する（失敗なら 0xFC）
4. すべて成功したら 0x02 ACK を返す
5. 以降は生の双方向パイプ
```

**active slot を占有するタイミング**: active slot（同時アクティブ接続数のカウント）は、**上記 3
（`--target` への TCP 接続成功）の直後**に確保する。未認証の QUIC connection、HELLO 未送信の
connection、`--target` 接続に失敗した connection は active slot を占有しない。

**HELLO 未送信 connection のタイムアウト**: QUIC connection は確立したが一定時間（実装値の目安:
5秒）以内に HELLO を送ってこない connection は、helper 側から close する。これにより、悪意ある
client が QUIC connection だけを張って stream を開かずに正規 client の接続機会を妨害することを防ぐ。

### ACK 後（生のバイトパイプ）

`0x02` (ACK) を送出した後は、そのストリームの以降のバイト列はフレーミング無しで `--target`（既定
`127.0.0.1:22`）への TCP コネクションへ双方向にそのまま中継する。

- **half-close**: クライアント側が stream の送信を FIN した場合、helper は TCP ソケットに対して
  shutdown(Write) するが、TCP 側からの残りのデータ（TCP→QUIC 方向）は読み切ってから stream を
  finish する。TCP 側（`sshd`）が先に閉じた場合も対称に扱う。
- **backpressure**: TCP 側の書き込みが詰まったら QUIC stream 側の読み込みを止める（Phase 7-1 の
  実装で対応。PLAN.md「実装上の難所」相当の考慮）。
- **graceful close の方針**: quinn の `Connection::close()` は即時 close であり、未配送データが
  破棄される可能性がある。通常のセッション終了時は `Connection::close()` を安易に呼ばず、
  **stream の finish / half-close を優先**する。`Connection::close()`（application close や reset）
  は異常系・認証失敗・protocol violation の場合のみ使用する。

---

## 5. 非ゴール（Non-goal）— 重要な期待値の明確化

**この Phase 7 の契約が保証するのは、同一 QUIC connection が migration / NAT rebinding（送信元 IP/port
の変化）に耐えることだけである。** QUIC は Connection ID によって経路変更後も同一 connection を
維持できる設計だが、これは「connection が生きている間」に限られる。

QUIC connection が完全に失われた後、**新しい QUIC connection を同じ helper に張り直すことはできる**
（`--once` が既定 OFF なのはこのため）が、その場合 helper は ACK 後に `--target` への**新しい** TCP
接続を作成する。したがって：

- 「helper が新しい接続を受け付けられること」と「既存の SSH セッションが維持されること」は **別の話**である
- `--max-idle-lifetime` の目的は「helper バイナリの再配布・再起動を避ける」ことであり、
  「SSH セッションを維持する」ことではない
- 既存 SSH セッション（byte stream）を、QUIC connection の完全な喪失後も再接続する仕組みは
  **Phase 8（opaque SSH byte-stream resume proxy）の範囲**であり、Phase 7 では実装しない

---

## 6. Phase 8 との関係（前方互換のための予約）

Phase 8（opaque SSH byte-stream resume proxy）は、この契約の `0x03 RESUME` フレームタイプと
`session_id` の概念を新設して拡張する形になる見込み。Phase 7 の helper は `0x03` を安全に拒否
（`0xFD`）するため、Phase 8 未対応の helper に Phase 8 対応クライアントが誤って resume を試みても、
クラッシュやハングではなく明確な拒否応答が返る。
