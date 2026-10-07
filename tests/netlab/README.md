# netlab: L2 夜間シナリオ

実バイナリ(`isekai-pipe serve`/`connect` + 実sshd + 実ssh)を、veth直結の2つのnetwork
namespace(`topology.sh`)上で動かし、実カーネル・実QUIC・実時間で故障を注入する。
位置づけは `docs/adr/0018-connection-resilience-simulation.md` の L2 と
`docs/adr/0021-deterministic-network-simulation-l1.md` §5-4(Q-L1-14)。

- ワークフロー: `.github/workflows/rust-core-netlab-check.yml`
  (`schedule` 毎日 18:17 UTC + `workflow_dispatch`。PR/pushでは動かず、required checkでもない)。
- 共通部: `common.sh`(sshd/serve起動、PersistentProfile組み立て、pacedシナリオ実行器)。
- 概算実行時間: build 数分(キャッシュ命中時) + シナリオは並列で各 約 1.5〜3 分(合計の壁時計 < 20 分)。

## シナリオ

| スクリプト | 故障 | 検証 |
|---|---|---|
| `silent_blackhole_then_resume.sh` | 両nsでUDPをiptables DROP(サイレント)を25秒 | 同一sshセッションが完走、sha256一致、故障注入後にserveが新たなQUIC接続を受理(=実際にresume) |
| `link_down_then_resume.sh` | client側vethをdown→20秒後up(完全切断、エラーが即見える経路) | 同上 |
| `zombie_relay_preempt.sh` | 旧QUIC接続の4タプルだけ両方向DROP(serve側は `--idle-timeout 60` でzombie化) | 新ソケットのRESUMEがzombieをpreemptして受理され完走(857f6ae6 D-2型) |
| `direct_survives_loss_and_delay.sh` | tc netem loss 3% + delay 80ms±20ms | ベースライン(バイト列が壊れない) |

pacedシナリオは 4096B/秒 で流し続けるので、故障中のデータもreplay経由で届くことを見る。
ログに出る「ssh finished ... after Ns」が、実QUICでの検出+復旧を含む所要時間の実測値(RES ADR Q8)。

## 赤い夜間runの調査(lead向け)

1. 失敗すると `l2-nightly` ラベルのissueが自動で作られる(既にopenなら追記)。
   本文のrunリンクと「失敗したjob」(=シナリオ名)から開く。
2. 失敗jobの `Run <scenario>` ステップのログを見る。`common.sh` の cleanup が失敗時に
   `netlab_diagnostics`(ip addr / tc qdisc)、`sshd.log`、`serve.stdout`/`serve.stderr`
   (`--log-level debug`)、`ssh.log`、`dmesg` 末尾を全部ダンプする。
3. 症状の見分け方:
   - `ssh exited 124`(timeout): resumeしなかった/遅すぎた。`serve.stderr` の
     `QUIC connection established` の回数(注入後に増えたか)と、`BUSY_OTHER_SESSION`/`preempt` の有無を確認。
     zombie系で出ているなら fencing slot / preempt(`isekai-pipe/src/engine/`)の回帰を疑う。
   - `checksum mismatch`: バイト損失・重複。resume replay の回帰。**最優先で調べる**。
   - `session survived without a resume after the fault`: 故障が効いていない(シナリオ側の問題。
     iptables/ip link の失敗やタイミング)。製品バグではなくテスト基盤の修正。
   - `could not find the client's QUIC UDP socket`(zombie): `ss -uHnap` の出力形式/権限の問題。
   - build/依存取得だけが赤: インフラのflake。再実行(`workflow_dispatch`)して再現するか確認。
4. flakeかどうかの切り分け: 同じコミットで `workflow_dispatch` を2〜3回回す。常に赤なら回帰、
   時々なら実時間依存のflake(タイミング定数を広げるのはシナリオ側の `*_SECS` 環境変数)。
   回帰ならその直近のmainコミット(`git log` で serve/resume 系)を二分する。
5. 修正後は同じissueに結果を書いてcloseする(次の赤で新規issueが立つ)。
6. ローカルでは**ビルドも実行もしない**運用(GitHub Actionsのみ)。再現・検証は
   `workflow_dispatch` でブランチを指定して回す。

## 新しいシナリオを足すとき

`scenarios/<name>.sh` を作り、`common.sh` を source して `netlab_up` → `netlab_stack_up [serve引数]`
→ `netlab_run_paced_scenario <回数> <注入関数>` と書く。ワークフローの `matrix.scenario` に名前を
足す。1シナリオは `timeout-minutes: 12` に収まること。
