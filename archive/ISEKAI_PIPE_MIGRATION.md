# isekai-pipe 移行タスク

> **ARCHIVED（2026-07-07）**: 本書のタスク(P0〜P5)は全て完了した。現在の設計・実装状況は
> `ISEKAI_PIPE_DESIGN.md` を参照すること。本書は各タスクをどう完了させたかの経緯記録として
> 保持する。

**目的:** `chatgpt.md` の最終設計に合わせて、現行の `isekai-helper` / `isekai-ssh connect`
中心の構成を、`isekai-ssh` と `isekai-pipe connect/serve` の責務分離へ段階移行する。

現行実装は、bootstrap 用 SSH 宛先と QUIC dial 宛先を一部で同一視している。
これは Tailscale、LAN、既知 direct host では成立するが、ProxyJump でしか bootstrap
できない host では一般に成立しない。移行後は、bootstrap 経路、candidate endpoint、
selected path、service target を別概念として扱う。

## 用語

| 用語 | 意味 |
| --- | --- |
| logical host | ユーザーが指定する接続名。例: `production` |
| bootstrap candidate | remote に SSH で到達し、`serve` を配布・起動するための経路 |
| service target | remote `serve` から見た TCP 接続先。SSH なら通常 `127.0.0.1:22` |
| candidate endpoint | STUN、ISEKAI-link、relay 等で実測・交換される短命な到達候補 |
| selected path | 今回の接続で `connect` が選択した実経路 |
| path hint | 次回探索の補助にだけ使う短命情報。恒久的な接続先ではない |

## 最終責務

### isekai-ssh

- SSH argv と `~/.ssh/config` を解決する。
- `#@isekai` 設定を読む。
- bootstrap candidate を選ぶ。
- 必要なら ProxyJump 等で remote に SSH し、`isekai-pipe serve` を配布・起動する。
- peer identity、service、profile、ConnectionIntent を作る。
- OpenSSH を起動する。
- QUIC socket、candidate pair、SelectedPath、resume session は所有しない。

### isekai-pipe connect

- profile または ConnectionIntent を読む。
- UDP socket を作る。
- STUN / ISEKAI-link / rendezvous で local/remote candidate を収集・交換する。
- direct / relay fallback を含む SelectedPath を決める。
- QUIC 接続と logical session を確立する。
- stdio または TCP listen と logical session を中継する。
- ProxyCommand で使う場合、stdout は SSH byte stream 専用にする。

### isekai-pipe serve

- remote 側で起動し、service target を提供する。
- ISEKAI-link または rendezvous へ outbound presence/candidate を登録する。
- STUN 観測、hole punching、relay association を行う。
- ServerIdentity、session table、resume buffer を所有する。
- SSH プロトコルは解釈しない。

## 現行実装の扱い

### direct-by-bootstrap-host

現行 `HelperQuic` の

```text
QUIC dial target = ssh_host:handshake.listen_port
```

は、`direct-by-bootstrap-host` という特殊ケースとして扱う。これは bootstrap host が
client から UDP/QUIC でも直接到達可能な場合だけ正しい。

この前提を一般の ProxyJump/NAT 越えの既定にしない。

### STUN / relay

現行の STUN / relay 実装は、remote helper 起動後に `stun_observed_addr` または
`relay_public_addr` を handshake JSON で受け取るため、bootstrap SSH 宛先と QUIC
dial 宛先が分離できている。この考え方を `isekai-pipe connect/serve` の candidate
交換へ拡張する。

### 互換名

`isekai-helper`、`HelperQuic*`、`known_helpers.toml`、`isekai-helper/1` ALPN などは
外部互換性に関わる。最初の段階では互換 alias として残し、動作変更と名称変更を同じ
PR に詰め込まない。

ただし実利用者がほぼいない段階（2026-07-07 時点）と判断し、P5 では `HelperQuic*`・
ALPN・exporter label について互換 alias を作らず直接 rename した（下記 P5 参照）。
`known_helpers.toml` 自体は据え置き、新 profile schema への一方向 migration 関数
（`isekai-pipe-core::profile`）だけを追加している。

## タスク分解

### P0: 境界固定

- [ ] 現行 `HelperQuic` の direct 前提をドキュメント化する。
- [ ] `bootstrap_host`、`logical_host`、`service_target`、`candidate_endpoint`、
      `selected_path` の用語をコードコメントと設計文書に導入する。
- [ ] 既存 Android/iOS UniFFI API は壊さない。
- [ ] iOS 向け未マージ変更があれば先に main へ取り込む。

### P1: isekai-pipe skeleton

- [x] `isekai-pipe-protocol`、`isekai-pipe-core`、`isekai-pipe` の crate 境界を作る。
- [x] まずは既存 `isekai-protocol` / `isekai-transport` を再利用し、重複実装を避ける。
- [x] `isekai-pipe` binary に `connect` / `serve` / `probe` / `inspect` の CLI skeleton を置く。
- [x] 既存 `isekai-helper` binary は削除せず、互換入口として残す。
      → P5 で `isekai-pipe` crate へ完全統合し、`isekai-helper` crate/binary 自体を廃止した
      （実利用者がほぼいない段階と判断。下記 P5 参照）。

### P2: serve 移行

- [x] `isekai-helper` の機能を `isekai-pipe serve --service ssh=127.0.0.1:22` へ寄せる。
- [x] `--target` は `--service` の単一 service 互換 alias とする。
- [x] STUN / relay / resume session table / service target を serve 側責務として整理する。
- [x] handshake JSON を peer/service/candidate 表現へ拡張する。ただし旧 client が読む字段は維持する。

現時点では `isekai-pipe serve` が `isekai-helper` runtime library を同一プロセス内で起動する。
handshake JSON は旧 top-level fields に加えて `protocol` / `peer` / `services` / `candidates`
を出力する。

### P3: connect 移行

- [x] `isekai-pipe connect --profile <name> --service ssh --stdio` の入口を作る。
- [x] `ConnectionIntent` の runtime-dir 保存と atomic claim を実装する。
- [x] `connect` が profile/intent から relay/STUN transport を開始する形にする。
- [x] 現行 `isekai-ssh connect` の resume pump を `isekai-pipe connect` へ移す。
- [x] `ssh_host:listen_port` 直結は `direct-by-bootstrap-host` mode として残す。
- [x] ProxyCommand は `isekai-pipe connect --profile "%n" --service ssh --stdio` を基本形にする。

現時点では `isekai-pipe connect` が `ConnectionIntent` を claim し、`isekai_transport` を直接
起動して stdio bridge と relay resume pump を所有する。STUN path は従来どおり non-resumable。
旧 helper QUIC / multipath path0 の `ssh_host:listen_port` 直結は
`direct-by-bootstrap-host` resolver に隔離し、通常の candidate endpoint 選択とは別の互換 mode
として扱う。

### P4: isekai-ssh wrapper 化

- [x] `isekai-ssh [SSH_OPTIONS] destination [command...]` を入口にする。
- [x] `ssh -G` と `#@isekai` を使って logical host と bootstrap candidate を解決する。
- [x] 必要に応じて `isekai-helper` を配布・起動する（`wrapper.rs::bootstrap_and_register`）。
      スコープを `direct-by-bootstrap-host` モードのみに限定（relay/STUN経由はJWT取得手段が
      未整備のため対象外、引き続き `isekai-ssh init` が必要）。優先度最上位の bootstrap
      candidate へ `--isekai-helper-binary <path>` で指定したローカル helper バイナリを
      `isekai-bootstrap::OpenSshBackend`(`LaunchSpec::Direct`、relay引数なし)経由で配布し、
      `init` と同じ `[y/N]` 対話確認(TOFU)を経て trust store に登録する。`--via` は単一 hop
      のみ対応(複数 hop は明示的に `isekai-ssh init` へ誘導するエラーにする)。
      テスト: `isekai-bootstrap/tests/openssh_e2e.rs::install_and_start_direct_never_passes_relay_args`、
      `isekai-ssh/tests/wrapper_auto_bootstrap_e2e.rs`(確認/拒否双方)。
- [x] ConnectionIntent を作り、OpenSSH を `ProxyCommand isekai-pipe connect ...` 付きで起動する。
- [x] stdout/stderr 契約を整理する。OpenSSH の byte stream は `isekai-pipe connect` の stdout のみ。
      wrapper 自身は `Stdio::inherit()` で `ssh` に丸ごと委譲するだけで stdout を触らない
      （`--isekai-explain`/`--isekai-dry-run`・エラーは全て stderr）。`isekai-pipe connect` は
      HELLO/proof/ACK 成功後の `pump_h2c`/`relay_stdio` だけが stdout に書き込み、失敗系
      （trust store 未登録・secret 不一致・relay 到達不可）は stdout に一切書かない。
      `isekai-pipe serve` の stdout は起動ハンドシェイク JSON 1行のみ
      （`isekai_helper::run_from_args` へ委譲、`HELPER_PROTOCOL.md` §2）。
      テスト: `isekai-ssh/tests/wrapper_stdout_cleanliness.rs`（wrapper の未信頼ホスト・
      dry-run 経路）、`isekai-pipe/tests/stdout_purity.rs`（`connect`/`serve` 双方）。
      wrapper/pipe connect/pipe serve の3経路をカバーする（旧 `connect` サブコマンド専用
      だった `isekai-ssh/tests/stdout_cleanliness.rs` は P5 でサブコマンドごと削除した）。

現時点では `isekai-ssh` が OpenSSH に `ProxyCommand isekai-pipe connect --profile <destination>
--service ssh --stdio` を注入し、`ISEKAI_INTENT_ID` で短命な `ConnectionIntent` を渡す。
wrapper は `ssh -G` の実効設定と `#@isekai` コメントから `profile` / `service` /
`remote-path` / `bootstrap-candidate` を解決する。trust store が無く bootstrap が必要な場合の
remote `isekai-pipe serve` 配布・起動は後続で行う。

### P5: 旧名整理

- [x] `HelperQuic*` 型名を新名（`IsekaiPipeQuic*`）へ全面 rename する（`rust-core/src`・
      UniFFI 生成物・Android Kotlin 呼び出し箇所を含む）。実利用者がほぼいない段階と
      判断し、互換 alias は作らず直接 rename した。ただし Room の
      `transport_preference` 列は enum の `name()` を文字列で永続化するため、
      `AppDatabase.kt` の migration 対象になり得る点は引き続き把握しておくこと
      （今回は「無視してよい」との判断で互換 shim を追加していない）。
- [x] ALPN / exporter label を新名へ変更する（`isekai-helper/1` → `isekai-pipe/1`、
      `isekai-helper-auth-v1` → `isekai-pipe-auth-v1`）。実利用者がほぼいない段階と
      判断し、旧値との互換テストは追加せず直接変更した。
- [x] `known_helpers.toml` は新 profile schema（`isekai-pipe-core::profile::PersistentProfile`）
      へ変換する migration 関数を用意した。ファイル自体は据え置き、`connect`/`wrapper`
      からの参照も変更していない（挙動変更は別 PR）。
- [x] `isekai-ssh` の旧 `connect` サブコマンド（独立実装の QUIC relay client、
      `--dev-insecure-*`・`--mode relay|stun`・`resume.rs`・`connect/`一式）を削除した。
      新 wrapper + `isekai-pipe connect` が同じ役割を果たすため。`init`/`login`/`logout`
      は trust store 登録手段として残す（wrapper 側にまだ代替の自動 bootstrap が無いため、
      唯一の登録経路）。旧 `connect` 専用テスト
      （`connect_e2e.rs`/`help_purity.rs`/`resume_*_e2e.rs`/`stdout_cleanliness.rs`/
      `stun_mode_e2e.rs`/`trust_store_e2e.rs`）も削除し、`init_e2e.rs` の
      「init→connect」後半は `isekai-pipe connect` を ProxyCommand として使う形に更新した。
- [x] handshake JSON（`HELPER_PROTOCOL.md` §2）を単一スキーマへ統合した。
      `listen_port`/`cert_sha256`/`stun_observed_addr`/`relay_public_addr` という
      旧クライアント向けのフラットな重複フィールドを廃止し、`peer.server_identity.cert_sha256`
      と `candidates`（`direct-by-bootstrap-host`/`server-reflexive`/`relayed`）だけを
      正とする。`isekai_protocol::handshake::HandshakeJson` に `cert_sha256()`/
      `direct_by_bootstrap_host_port()`/`stun_observed_addr()`/`relay_public_addr()`
      アクセサを追加し、`rust-core/src`（Android本番コードの5トランスポート実装）・
      `isekai-helper`・`isekai-ssh/src/init.rs`・関連テスト全てを追従させた。
- [x] `isekai-pipe serve` を `isekai_helper::run_from_args` への委譲から、`isekai-pipe`
      crate 自身の実装(`isekai-pipe/src/engine/`)へ完全統合した。`isekai-helper` crate/
      binary は廃止し、workspace member からも削除。ただし `isekai-helper` は単なる
      「委譲先」ではなく、**Android の本番リモートブートストラップが `include_bytes!` で
      埋め込み配布するバイナリそのもの**だったため、この統合は以下も連動して変更した:
      - リモート配布・起動される実体を `isekai-pipe`（`isekai-pipe serve ...`)に変更。
        `isekai_protocol::bootstrap::HELPER_BIN_NAME` を `"isekai-helper"` → `"isekai-pipe"`
        に変更(isekai-bootstrap・Android 双方が共有する定数)。
      - `isekai-bootstrap::openssh`(isekai-ssh の init/wrapper 経由の配布)と
        `rust-core/src/helper_bootstrap.rs`(Android の配布)双方の起動コマンド構築に
        `serve` サブコマンドを挿入(`isekai-pipe serve --target ...`)。`isekai-pipe serve`
        は `--service`/`--target` のいずれかが必須で、旧 isekai-helper のような暗黙の
        既定値(127.0.0.1:22)が無いため、`isekai-bootstrap::openssh` 側は
        `--target 127.0.0.1:22` を明示的に追加した。
      - `rust-core/scripts/build-isekai-helper-musl.sh` を
        `build-isekai-pipe-musl.sh` へ改名し、`isekai-pipe` package をビルドするよう変更。
        参照している CI ワークフロー4本(`ios-*-check.yml`)も追従。
      - `rust-core/src/isekai_pipe_quic_transport.rs` の `include_bytes!` 埋め込みパスと
        `HELPER_VERSION`(バージョン一致チェック用)を更新。
      - `isekai-helper/tests/e2e.rs` を `isekai-pipe/tests/serve_e2e.rs` へ移設し、
        `isekai-pipe serve` 経由で spawn するよう更新(10テスト、全pass)。
      - **未検証**: `helper_bootstrap::tests::bootstraps_and_launches_helper_over_real_ssh`
        等の opt-in E2E(`HELPER_BOOTSTRAP_TEST_KEY` 要、実 sshd 相手)、および実機
        Android での動作確認は本セッションでは実施していない。コマンド文字列レベルの
        アサーション(`isekai-bootstrap/tests/openssh_e2e.rs`)では `serve`/`--target` の
        挿入を確認済みだが、実SSH経由の全経路確認は次回持ち越し。

## 最初の実装 PR の範囲

最初の PR は P0 だけに限定する。

- 振る舞い変更なし。
- UniFFI 生成物を更新しない。
- 既存 `isekai-helper` / Android / iOS API を壊さない。
- direct-by-bootstrap-host が特殊ケースであることを明記する。
- 後続 PR のための skeleton 追加可否を判断する。
