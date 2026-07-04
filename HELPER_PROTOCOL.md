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
  --idle-timeout <SECS>         QUIC トランスポートの max_idle_timeout（既定: 15。Phase 8-4b 参照）
  --resume-window <SECS>        park された（data stream が切れて resume 待ちの）セッションを
                                 保持する時間。これを過ぎたら破棄する（既定: 120。Phase 8-4b 参照）
  --max-idle-lifetime <SECS>    アクティブな接続が無く、かつ新規接続も来ない状態が続いたら自己終了するまでの秒数（既定: 600）
  --max-sessions <N>            同時に保持できる resume 可能セッション数の上限（既定: 16。Phase S-4b 参照）。
                                 上限に達した状態で新規セッションを登録する際、parked（data streamが
                                 切れてresume待ちの）セッションのうち最も古いものを1つ立ち退かせる。
                                 立ち退けるセッションが無ければ（全セッションがアクティブ）新規登録を拒否する
  --once                        1回の接続が終了したら常駐せず終了する（既定は常駐し、次の接続を待つ）
  --stun-server <ADDR:PORT>     STUN(RFC 5389)サーバーへ問い合わせ、自分の観測アドレスを
                                ハンドシェイクJSONの `stun_observed_addr` に含める
                                （TransportPreference::IsekaiStunP2pQuic 用、既定は問い合わせない）
  --punch-peer <ADDR:PORT>      `--stun-server` 併用時のみ有効。isekai-terminal側が事前に調べた
                                自分自身のSTUN観測アドレスを渡す（simultaneous open用の
                                穴あけprobeデータグラムをこのアドレス宛に送る）
  --relay <ADDR:PORT>           MASQUE relay(isekai-link-masqueのCONNECT-UDP-bind)経由でトンネルを
                                張り、`--bind`する代わりにrelayが割り当てた公開アドレスを
                                ハンドシェイクJSONの `relay_public_addr` に含める
                                （TransportPreference::IsekaiLinkRelayQuic 用、`--stun-server`/
                                `--punch-peer`とは併用不可）。`--relay-sni`/`--relay-jwt`と併用必須
  --relay-sni <NAME>            `--relay`のTLS SNI/HTTPオーソリティ
  --relay-jwt <TOKEN>           `--relay`への認証に使うBearerトークン
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
- `--max-sessions`（Phase S-4b）: `--resume-window`（既定120秒）・`OutputBuffer` の容量上限は
  既にセッション単体のリソースを制限しているが、テーブルに登録できる**セッション数自体**には
  上限が無かった。悪意/異常な挙動のクライアントが `resume-window` 以内に大量の新規HELLOを
  送り続けると、`sweep_expired_parked` が効くまでの間にセッション数（≒メモリ使用量）が
  無制限に増え得る。この上限を超えた際の挙動は「立ち退き優先、全滅時のみ拒否」とした:
  parked（`parked_tcp` が `Some`）セッションは進行中の中継が無い＝失っても resume が
  次回 `REJECT_UNKNOWN_SESSION` になるだけで実害が小さいため、`parked_since` が最も古い
  ものから順に立ち退かせる。一方、アクティブなセッション（`parked_tcp` が `None`、＝
  今まさに中継が進行中）は、進行中の SSH セッションを強制切断することになるため
  **決して立ち退き対象にしない**。その結果、全セッションがアクティブで空きが無ければ
  新規登録そのものを拒否する（この場合も、拒否された接続自体の中継は継続する。単に
  resume 不可のまま扱われるだけで、`RESUME` を試みれば自然に `REJECT_UNKNOWN_SESSION` になる）。
- `--stun-server`/`--punch-peer`（Phase 10、STUN+SSHランデブー方式のP2P用）: `--punch-peer` の値を
  **stdin経由の対話的なやり取りで渡さない**のは、isekai-helperが`setsid`で即座にデタッチされ
  stdinが`/dev/null`にリダイレクトされる（後述のSSH起動例参照）ため、そもそも対話的なやり取りが
  できないから。isekai-terminal側はSSHブートストラップの**前に**自分自身のSTUN観測アドレスを
  ローカルで調べ終えており、それを起動コマンドラインへそのまま埋め込むだけで済む。
  STUN問い合わせ・穴あけprobeの送出は、いずれも実際にQUICが待ち受けるのと**同一のソケット**
  （`--bind`でbindしたもの）を使って行う——別ソケットでは観測されるNATマッピング（外部ポート）が
  変わってしまい、意味が無いため。
- `--relay`/`--relay-sni`/`--relay-jwt`（Phase 10、relay版P2P用）: isekai-helperが「agent役」として
  `isekai-link-masque`クレート経由でMASQUE relay(`seera-networks/axum-masque-rs`の`bound-udp-server`)へ
  CONNECT-UDP-bindトンネルを張り、relayが割り当てた公開アドレス(`relay_public_addr`)を
  `--bind`する代わりのQUIC待受アドレスとして使う。isekai-terminal側(`isekai_link_relay_transport.rs`)は
  MASQUE/HTTP/3/capsuleを一切意識せず、`relay_public_addr`へ普通にQUIC接続するだけでよい——
  relayから見ればisekai-helperが直接そのアドレスで listen しているのと区別が付かない。
  JWTの発行・配布フロー自体はPLAN.md Phase 10で別途設計する（このCLI契約はBearerトークンの
  文字列を受け取るだけで、取得方法には関知しない）。

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
{"v":1,"listen_port":45231,"cert_sha256":"3a7f...（hex, 64文字）","session_secret":"base64エンコードされた32byte","stun_observed_addr":"203.0.113.5:45231","relay_public_addr":null}
```

- `v`: ハンドシェイクフォーマットのバージョン（将来の破壊的変更に備える）
- `listen_port`: 実際に bind されたポート番号（`--bind` で 0 を指定した場合、OS が選んだ実ポート）
- `cert_sha256`: 自己署名証明書（起動のたびに生成する ephemeral cert）の DER 全体の SHA-256（hex）。
  クライアントは QUIC 接続時にこの値でピン留めする（証明書 fingerprint は QUIC 接続時の素の TOFU ではなく、
  **既に認証済みの SSH チャネル経由**で受け渡す設計。PLAN.md「セキュリティ」節参照）
- `session_secret`: helper がその起動ごとにランダム生成する秘密（base64）。クライアントはこれを使って
  `proof` を計算する（後述）。
- `stun_observed_addr`（Phase 10、`--stun-server` 指定時のみ存在。`null` は「未指定」または
  「STUN問い合わせが失敗した」の両方を表す——後者でもハンドシェイク自体は失敗させず継続する）:
  `--stun-server` から見た、この helper の観測アドレス（`"ip:port"` 文字列）。isekai-terminal は
  これを使って直結（hole punching）を試みる。
- `relay_public_addr`（Phase 10、`--relay` 指定時のみ存在。`null` は未指定）: relayが割り当てた
  公開アドレス（`"ip:port"` 文字列、`--relay`成功時は必ず存在する——STUNと異なり中間失敗時の
  フォールバック余地が無く、relay接続自体が失敗すればhelperの起動自体が失敗するため）。
  isekai-terminal はこのアドレスへ直接QUIC接続する（`ssh_host`とは無関係の別アドレス）。

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

---

## 7. Phase 8: resume プロトコル契約（Phase 8-0 成果物）

PLAN.md の「Phase 8: Opaque SSH byte-stream resume proxy」で定義した設計を、実装可能なワイヤー
フォーマットまで落とし込む。位置づけ・成立条件・非ゴールは PLAN.md 側の記述が正なので、ここでは
**ワイヤー上の契約**（フレーム形式・オフセット定義・ハンドシェイク手順）のみを定義する。

### 7.1 2ストリーム構成への変更

Phase 7 は「1 QUIC connection につき data stream 1本のみ」（§4）だったが、Phase 8 ではこれを
**2 stream 構成**に変更する:

| stream | 用途 | フレーミング |
|---|---|---|
| data stream | SSH の opaque バイト列を中継する（Phase 7 の HELLO/ACK 後と同じ、生の双方向パイプ） | フレーム無し（raw） |
| control stream | `APP_ACK` / `RESUME` / `RESUME_ACK` の交換専用 | 固定長フレーム（後述） |

data stream を raw pipe のまま維持するのは、ACK 用のフレーミングをホットパス（SSH の実データ）に
混ぜるとレイテンシ・実装複雑度の両面で不利なため。control stream は帯域を必要としない小さい
固定長フレームのみを扱うため、独立させても実装が単純になる。

`max_concurrent_bidi_streams` は Phase 7 の `1` から `2` に変更する（`max_concurrent_uni_streams`
は `0` のまま）。3本目以降の stream は Phase 7 と同様 reset する。

**接続確立順序**:
1. data stream を open し、Phase 7 §4 の HELLO/ACK を行う（既存のまま、無変更）。
2. ACK 受信後、client は control stream を新規に open し、`CONTROL_HELLO` フレームを送る
   （後述）。helper が `session_id` を発行して `CONTROL_ACK` を返す。
3. 以降、data stream は raw pipe、control stream は `APP_ACK` の定期交換に使う。

初回接続（resume ではない、新規セッション）でも control stream は必ず開く。session_id は
resume 時だけでなく、通常運用中もログ・診断のために存在する。

### 7.2 session_id とオフセットの定義

`session_id`: helper が `CONTROL_HELLO` 受信時に生成する 16 byte のランダム値。同一 helper
プロセス内でアクティブな resume 可能セッションを一意に識別する（`session_secret` とは別物 —
`session_secret` は起動ごとに 1 つだが `session_id` は接続ごとに発行する）。

4 つのオフセット（すべて起点 0、単位はバイト、`u64`、data stream 上での論理位置）:

| オフセット | 管理主体 | 意味 |
|---|---|---|
| `client_sent_offset` | client | data stream へ実際に書き込んだ（QUIC に渡した）累計バイト数 |
| `helper_committed_offset` | helper | data stream から読み、`--target` の TCP socket へ **書き込み成功した**累計バイト数（C→S） |
| `helper_sent_offset` | helper | data stream へ実際に書き込んだ累計バイト数 |
| `client_delivered_offset` | client | data stream から読み、russh（の下位 transport）へ **引き渡し成功した**累計バイト数（S→C） |

`committed`/`delivered` は「QUIC が ACK した」ではなく「アプリ層が実際に処理した」ことを意味する
（PLAN.md §実装上の難所の区別をそのまま踏襲）。QUIC 自体の ACK は quinn 内部で完結しており
アプリケーションはこれを意識しない。

### 7.3 control stream フレーム形式

すべて固定長。マルチバイト整数は big-endian。

**`CONTROL_HELLO`**（client → helper、control stream 先頭）:
```
byte 0:      0x10 (CONTROL_HELLO)
byte 1..33:  proof（Phase 7 §4 と同じ HELLO の proof を再利用。同一 QUIC connection の
             exporter から計算するため、data stream の HELLO と同じ値になる）
```
data stream の HELLO で既に認証済みの QUIC connection 上の control stream なので、
再認証というより「この control stream が正しい connection に属することの確認」目的。

**`CONTROL_ACK`**（helper → client、応答）:
```
byte 0:      0x11 (CONTROL_ACK)
byte 1..17:  session_id（16 byte）
```

**`APP_ACK`**（双方向、いつでも送信可）:
```
byte 0:      0x12 (APP_ACK)
byte 1..9:   自分が確認した相手方向のオフセット（u64）
             client → helper の場合: client_delivered_offset（S→C の受信確認）
             helper → client の場合: helper_committed_offset（C→S の受信確認）
```
送信タイミングは実装判断（例: 64KiB 受信ごと、または 200ms ごとのどちらか早い方）。
`APP_ACK` はベストエフォートであり、紛失しても次の `APP_ACK` が新しい（より進んだ）オフセットを
運ぶため実害はない（累積値であり差分ではないため）。

**`RESUME`**（client → helper、新しい QUIC connection の control stream 先頭）:
```
byte 0:      0x03 (RESUME)  ※ HELPER_PROTOCOL.md §4 で Phase 8 用に予約済みの値
byte 1..17:  session_id（16 byte、resume 対象）
byte 17..49: resume_proof（32 byte）
byte 49..57: client_sent_offset（u64）
byte 57..65: client_delivered_offset（u64）
```
`resume_proof = HMAC-SHA256(session_secret, exporter || session_id)`
（`exporter` は **新しい** QUIC connection の `export_keying_material(label = b"isekai-helper-resume-v1", context = "", length = 32)`。
`session_id` を HMAC 対象に含めることで、同じ `session_secret` を使い回す複数セッションが
互いの resume トークンを流用できないようにする）

**`RESUME_ACK`**（helper → client、応答）:
```
byte 0:      0x13 (RESUME_ACK)
byte 1..9:   helper_committed_offset（u64） — client はこれ以降を input replay buffer から再送する
byte 9..17:  helper_sent_offset（u64） — 参考値（client 側の整合性チェック用）
```
この直後、helper は `[client_delivered_offset, helper_sent_offset)` の範囲を output buffer から
data stream に再送する。client は `RESUME_ACK` 受信後、`[helper_committed_offset, client_sent_offset)`
の範囲を input replay buffer から data stream に再送する。両者は独立して並行に進めてよい
（どちらかの再送が先に終わるのを待つ必要はない）。

**`RESUME` の拒否応答**（helper → client、`RESUME` に対して。既存の `0xFC`/`0xFD`/`0xFF` を再利用し、
Phase 8 固有の意味を追加する）:

| 値 | 意味 |
|---|---|
| `0xFF` (REJECT_AUTH) | `resume_proof` が不正 |
| `0xF9` (REJECT_UNKNOWN_SESSION) | `session_id` が存在しない（helper 再起動・タイムアウト等で
  セッション情報が失われた）。client は Phase 7 の通常ブートストラップからやり直す以外に手段がない |
| `0xF8` (REJECT_OFFSET_GONE) | 要求された offset がすでに helper 側バッファの範囲外（バッファ
  上限超過で古いデータを破棄済み）。`REJECT_UNKNOWN_SESSION` と同様、再送不能なので resume を諦める |

### 7.4 バッファ契約（Phase 8-1 / 8-2 の前提）

- helper 側 output buffer（S→C 再送用）: 上限サイズを設ける（既定案 4MiB、`--resume-buffer-size`
  で変更可能とする）。上限到達時は `--target` からの読み込みを止める（TCP backpressure、PLAN.md
  「実装上の難所」通り）。バッファから追い出した範囲を resume 要求された場合は `REJECT_OFFSET_GONE`。
- client 側 input replay buffer（C→S 再送用）: 同様に上限を設ける。russh が生成する送信バイトは
  ほぼ全てユーザー入力起因で小さいため、helper 側ほど肥大化しにくいが、trzsz アップロード中は
  大きくなり得るため上限は必須。
- 両バッファとも `helper_committed_offset` / `client_delivered_offset` より前のデータは
  安全に破棄してよい（相手が受け取り確認済みのため）。`APP_ACK` を受信するたびに破棄範囲を進める。

### 7.5 session_id のライフサイクル

- helper プロセスが再起動すれば全 `session_id` は失われる（`REJECT_UNKNOWN_SESSION` で表現）。
- helper 側は `session_id` ごとに「data stream が閉じてから resume 待ちを続ける時間」の上限を
  `--resume-window`（既定120秒）で持つ。この時間を過ぎたら output buffer を破棄し
  `session_id` を無効化する。
- 1 つの `session_id` に対して同時に有効な data stream は 1 本のみ（Phase 7 の active slot 概念を
  session 単位に引き継ぐ）。
- helper プロセス全体で同時に保持できる `session_id` の数自体にも `--max-sessions`（既定16、
  Phase S-4b）で上限がある。詳細・立ち退き/拒否の設計判断は「1. CLI 契約」節の該当箇所を参照。
- **`--idle-timeout`（QUIC transport の生存確認）と `--resume-window`（park セッションの保持時間）は
  意図的に別の値にしてある**（Phase 8-4b、実機検証で発見）。当初はこの2つを同じ値で共用していたが、
  client が QUIC connection の喪失を検知するまでの時間（`--idle-timeout` 待ち + PTO 再送、実測で
  40秒前後かかることを確認）が、helper 側の park 保持時間（当時30秒）を上回ってしまい、
  「client が reattach を試みる頃には既に helper が session を破棄済み」という理由で
  **reattach が必ず `REJECT_UNKNOWN_SESSION` になる**致命的なタイミング不整合が起きていた。
  client 側（`helper_quic_transport.rs`）にも `keep_alive_interval`（NAT UDP マッピング維持、
  5秒間隔）と短い `max_idle_timeout`（15秒）を設定し検知を高速化した上で、`--resume-window`
  を検知時間 + reattach のリトライ予算より十分長い既定値にしてこの2つを分離した。
  なお実機での追加検証（大量出力中の切断）で、reattach が失敗する各試行自体も
  `--idle-timeout` と同じ長さ（quinn が handshake タイムアウトとして内部的に流用する
  ため）だけブロックすることが判明し、5回全滅する最悪ケースの合計時間は
  「指数バックオフの15秒」ではなく実測で約90秒（15秒×4回失敗 + バックオフ計15秒）
  かかることを確認した。ちょうど90秒だとマージンが薄いため、`--resume-window` の
  既定値は120秒にしてある。

### 7.6 Phase 7 との互換性

Phase 7 のみ対応の helper（`0x03` を `0xFD` で拒否する版）に対して Phase 8 対応 client が接続した
場合: client はまず通常の HELLO/ACK（data stream）を試み、成功後に control stream を開こうとするが
`max_concurrent_bidi_streams=1` の制限で reset される。client はこれを「resume 非対応の helper」と
解釈し、control stream 無しの Phase 7 動作（resume 機能無効）にフォールバックする。
