# isekai-pipe 移行タスク

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

## タスク分解

### P0: 境界固定

- [ ] 現行 `HelperQuic` の direct 前提をドキュメント化する。
- [ ] `bootstrap_host`、`logical_host`、`service_target`、`candidate_endpoint`、
      `selected_path` の用語をコードコメントと設計文書に導入する。
- [ ] 既存 Android/iOS UniFFI API は壊さない。
- [ ] iOS 向け未マージ変更があれば先に main へ取り込む。

### P1: isekai-pipe skeleton

- [ ] `isekai-pipe-protocol`、`isekai-pipe-core`、`isekai-pipe` の crate 境界を作る。
- [ ] まずは既存 `isekai-protocol` / `isekai-transport` を再利用し、重複実装を避ける。
- [ ] `isekai-pipe` binary に `connect` / `serve` / `probe` / `inspect` の CLI skeleton を置く。
- [ ] 既存 `isekai-helper` binary は削除せず、互換入口として残す。

### P2: serve 移行

- [ ] `isekai-helper` の機能を `isekai-pipe serve --service ssh=127.0.0.1:22` へ寄せる。
- [ ] `--target` は `--service` の単一 service 互換 alias とする。
- [ ] STUN / relay / resume session table / service target を serve 側責務として整理する。
- [ ] handshake JSON を peer/service/candidate 表現へ拡張する。ただし旧 client が読む字段は維持する。

### P3: connect 移行

- [ ] 現行 `isekai-ssh connect` の stdio relay と resume pump を `isekai-pipe connect` へ移す。
- [ ] `connect` が profile/intent から candidate 収集を始める形にする。
- [ ] `ssh_host:listen_port` 直結は `direct-by-bootstrap-host` mode として残す。
- [ ] ProxyCommand は `isekai-pipe connect --profile "%n" --service ssh --stdio` を基本形にする。

### P4: isekai-ssh wrapper 化

- [ ] `isekai-ssh [SSH_OPTIONS] destination [command...]` を入口にする。
- [ ] `ssh -G` と `#@isekai` を使って logical host と bootstrap candidate を解決する。
- [ ] 必要に応じて `isekai-pipe serve` を配布・起動する。
- [ ] ConnectionIntent を作り、OpenSSH を `ProxyCommand isekai-pipe connect ...` 付きで起動する。
- [ ] stdout/stderr 契約を整理する。OpenSSH の byte stream は `isekai-pipe connect` の stdout のみ。

### P5: 旧名整理

- [ ] `HelperQuic*` 型名を段階的に新名へ移行する。
- [ ] DB カラムなど永続互換が必要な旧名は残す。
- [ ] ALPN / exporter label は version bump と互換テスト込みで変更する。
- [ ] `known_helpers.toml` は新 profile schema へ migration path を用意する。

## 最初の実装 PR の範囲

最初の PR は P0 だけに限定する。

- 振る舞い変更なし。
- UniFFI 生成物を更新しない。
- 既存 `isekai-helper` / Android / iOS API を壊さない。
- direct-by-bootstrap-host が特殊ケースであることを明記する。
- 後続 PR のための skeleton 追加可否を判断する。
