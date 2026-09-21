# ADR: 接続耐性を実機なしで検証する方法

- **Status**: Draft rev4(2026-09-21)。rev3までは「仮想時間 + in-memory UDPのシミュレーション基盤(L1)」を
  中心に据えていたが、調査の結果、**費用対効果の高い順に L0(安い不変条件テスト)→ L2(夜間netns)を先に
  やり、L1は保留**する構成へ組み替えた。経緯は§8。
- **対象**: `rust-core/src/{pool,orchestrator,resume_client,isekai_pipe_quic_transport,android_quic_endpoint,faulty_stream,faulty_udp_socket}.rs`、
  `rust-core/src/transport/ssh_handler.rs`、`rust-core/isekai-transport/src/system.rs`、`tests/netlab/`(リポジトリ直下)
- **入力**: 2026-09-21の完成度/残作業の棚卸しセッションと、Opusによる3ラウンドの批判レビュー
  (round 1〜3、`scratchpad/opus-review-resilience-sim-round{1,2,3}.md`)
- **拘束される既存ルール**: `.claude/rules/rust-ssot.md`、`.claude/rules/always-connects.md`、
  `prefer-gh-actions-over-local-cargo`(ローカルでbuild/testしない。検証はGitHub Actionsのみ)
- **表記**: 「確認済み」はコードを実際に読んで確認した事実。「推測」は導出のみで未実行。

## 0. 訂正(rev3、重要)

- 調査の初期(rev1〜rev2)は`origin/main`より古い作業ツリーを読んでおり、**#120のpool修正
  (`4793ebcd`、2026-09-17)が既にmainにあること**を見落としていた。`try_attach_with`+`is_alive`+
  tombstoneで、死んだ`Ready`は`Establisher`に落ちる。Issue #120はOPENのまま。
- 教訓: 調査の最初に`git fetch`して`origin/main`と比較する。`gh issue`のstateだけで
  「未解決」と判断しない。

## 1. 背景

接続耐性(ローミング・完全切断からのresume・multipath・自動再接続)は差別化ポイントだが、
検証は実機・実ネットワークに依存してきた。#120は実機のfault injectionで見つかったが、
**根本原因(`try_attach`のReady分岐に生存確認が無かった)は端末にもキャリアにも依存しない
ロジックバグ**で、発見が実機頼みになったのは「シミュレーション基盤が無かったから」ではなく
「その不変条件を誰もテストに書いていなかったから」だった。

問題は3つに分けられる:

- **(A)** 不変条件を書く習慣と足場が無い(#120型: タイマー・プール・状態機械)。
- **(B)** 人間が想定しない組み合わせを探索できない。
- **(C)** モデル化した世界が現実と乖離していないかを確かめられない。

## 2. 決定: 費用対効果の高い順に3層

### L0(最優先): プロセス内の不変条件テスト — (A)(B)に効く

QUICも仮想時間も使わず、既存の足場(`pooling_e2e_tests`のmock sshd、`FaultyStream`、`pool.rs`の
汎用プリミティブ)で書く。速く(ミリ秒〜秒)、CIも軽い。

| 種類 | 内容 | 状態 |
|---|---|---|
| 実Handleの回帰(EOF/RST) | `dead_pooled_handle_is_not_reused_after_underlying_connection_loss` | **マージ済み(#121)**。CI緑 |
| 実Handleの回帰(サイレント遮断) | `..._after_silent_blackhole`。本番のkeepalive(60秒×3)で確立後`tokio::time::pause()`し、死亡検出までの仮想時間を測る | PR #122(CI待ち) |
| 定数の複製解消+docの関係 | Android QUIC設定が`isekai_mux_config(false)`と同値のリテラル複製だった→直接呼ぶ。keepalive==idle/3、QUIC grace>plain grace | PR #123(CI待ち) |
| **状態機械のモデルベーステスト** | `pool.rs`の操作列(attach/publish/mark_dead/release)をproptestで参照モデルと各ステップ突き合わせ。refcountリーク・tombstone跨ぎの食い違いを探す | PR #124(CI待ち) |
| ロック競合(`try_lock`失敗→false-alive) | `is_alive`の`unwrap_or(true)`分岐が`mark_dead_if_same`で高々1回で収束すること | 未(L0-3) |
| orchestrator↔pool結合 | 再接続予算60秒に「stale 1回+新規確立1回」が収まること(値ではなく関係をassert) | 未(L0-4) |
| russh keepalive設定の一本化 | 同一設定が6箇所(`lib.rs`/`quic_transport.rs`/`isekai_pipe_quic_transport.rs`等)に複製。L0-2のテストも複製している | 未(#122マージ後) |

**定数関係テストの限界(正直な評価)**: #120の本質は「90秒>60秒」が意図された関係の違反ではなく
生存確認の欠落だった。コードに**意図として書かれている関係**(例: keepalive==idle/3)だけを守る。
全定数の関係を網羅する意義は薄い。

### L2(夜間): 実カーネル・実QUIC・実時間のnetns — (C)に効く

仮想時間の仕掛けを使わず、実時間で数分かかっても構わない夜間CIに寄せる。
`tests/netlab/`(現状bash 1シナリオ・`workflow_dispatch`のみ)を拡張し、iptables DROPによる
**サイレント遮断**、link down/up、NAT rebind、IP変更、片方向断を実バイナリで検証する。
- SIGSTOP/SIGCONTは「Windows holderがスリープ復帰で時間を飛び越えるプロセス凍結」の近似であり、
  Androidの`Doze`の近似ではない。Doze/OEM killは実機smokeの担当。
- 夜間実行(`schedule`)。required checkには含めない(実時間依存でflakeやすい)。
- QUIC経路(`ReattachableStream`が予算5回を使い切ってからrusshにエラーが見える)の死亡検出時間は、
  仮想時間で近似せず、ここで実測する(Q8)。

### L1(保留): 仮想時間 + in-memory UDPのシミュレーション基盤

**L0とL2で足りなくなるまで着手しない。** 保留の理由:
- 最大の作業量(`RUNTIME`注入、`SimNet`、シナリオDSL、seed付きランダム、engine lib化)に対し、
  見つかるバグの種類がL2と大きく重なる。
- 仮想時間には固有の制約がある(§5)。
- L0でpool状態機械のモデルテストが書けたことで、(B)の一部は安価に満たせる。

### 実機

廃止せず、シミュレーションで原理的に代替できないものだけに絞る: キャリアNATの実挙動、
Doze/OEMによるkill、Foreground Service、`Network.bindSocket()`のWi-Fi/セルラー切替、実機IME/UI。
リリース前に少数の定型で回す。

## 3. 検証する不変条件

- **I1(復旧)**: 復旧後、有限時間T以内に入力がエコーバックされる。
- **I2a(サーバー側リーク無し)**: どの終了経路でもfencing slotが解放される。**実engine必須**
  (`always-connects.md`が名指しで警戒するクラス)。fake serverでは原理的に検証できない。
- **I2b(クライアント側リーク無し)**: pool entryのrefcountがattach/releaseで1対1に対応する。
  → L0のモデルテストで検証。
- **I3(死んだHandleを再利用しない)**: → L0の実Handleテスト(EOF/RST版・サイレント遮断版)。
- **I4(状態の単調性)**: `ConnectionPublicState`が不正な遷移をしない。
- **I5(無音の失敗無し)**: 復旧不能なら`Disconnected`等に到達し、`Reconnecting`のまま固まらない。
- 不変条件は内部のタイマー値を直接assertせず、設定値から導出した上限で書く。

## 4. 判明した事実(コードで確認済み)

- **russh 0.48.2のkeepalive**(`client/mod.rs`、確認済み): タイマーは`keepalive_interval`ごとに発火し、
  `alive_timeouts > keepalive_max`で切断する。無応答なら60/120/180/240秒目に送信、**300秒目に切断**
  (`interval×(max+2)`)。本番設定は60秒×3。→ **下位層で検出されないプレーンSSH経路のサイレント遮断は
  検出に約300秒**。再接続ループはDisconnected通知の後に始まるので再接続予算(60秒)とは衝突しないが、
  **その間(最大300秒)、他タブが同じキーでアタッチすると`is_alive()`が真のまま死んだHandleを掴む**
  (共有接続の設計上の限界)。
- **#120修正後に残る非対称**: `ISEKAI_PIPE_QUIC_IDLE_GRACE`(90秒)> `ReconnectPolicy::default().timeout`
  (60秒)は未変更。死んだ値は返らなくなったので直接のバグではないが安全余裕は無い。`is_alive()`が
  trueを返す窓が最大`RUN_EXEC_TIMEOUT`(10秒)あり、その間`retry_attempt_in_flight`が後続を塞ぐ(推測)。
- **`publish_success`は既存エントリが無いと何もしない**(`pool.rs`)。エントリを作らずに
  `publish_success`だけ呼ぶテストは、バグがあっても偽グリーンになる。先に`try_attach_with`で
  `Establisher`としてエントリを作ること。
- **noq fork rev `db998f2d`の`TokioRuntime`はcrates.io 1.1.0と同一**(`diff`空): タイマーは
  `tokio::time::sleep_until`、`now`は`tokio::time::Instant`、`spawn`は`tokio::spawn`。
- **`isekai-pipe`は`[[bin]]`のみ**。engineをテストプロセス内から起動できない。`run_from_args`が
  `env_logger.init()`/`install_default()`を1プロセス1回しか呼べず、handshake情報をstdoutでしか返さない。
- **既存のFaultyStream::cut()はEOF/ConnectionReset(RST相当)**で、UDPの`debugCutUdpFault`
  (`FaultySender::poll_send`が破棄するサイレント遮断)とは故障の出方が違う。→ `blackhole()`を追加(PR #122)。
- `faulty_udp_socket.rs`のフォルト判定は`rand::thread_rng()`でseed再現できず、遅延は
  `std::time::Instant`を直読みする(仮想時間に載らない)。

## 5. 仮想時間(L1・L0-2で使う場合)の制約

- **グローバル`crate::RUNTIME`**(`lib.rs:64`、multi-thread)が本丸の障害: `pool.rs:189`のidle grace
  タイマーと`orchestrator.rs:969`の再接続ループはここにspawnされ、`start_paused`(current_thread必須)
  では仮想化できない。L1で`isekai-terminal-core`内のロジックを仮想時間で動かすには、`RUNTIME`を
  `cfg(test)`で明示注入可能にする必要がある(`try_current()`フォールバックは採らない。既存の
  `rt.block_on`型テスト約20本の挙動不変が受け入れ条件)。
- **`advance(d)`は使わず`sleep`+auto-advance**: tokio 1.53.1の`time/clock.rs`docが、auto-advanceを止める
  公開APIは無く、`advance`は満了した全タイマーを同時に完了させ「凍結→再開のシミュレーションには
  期待どおりにならない」と明記している。
- 実UDPソケットとの併用は、全タスクがI/O待ちのとき時計が自動で進んで偽陽性のタイマー発火が
  起こりうる(未検証)。実TCPで接続確立を済ませてから`pause()`する形(L0-2)は局所的に回避している。
- 時計の直読みは`faulty_udp_socket.rs`(207/231/259)、`multipath_transport.rs`(427/481/728)、
  `session.rs`(1192/1232/1234)、`terminal.rs:1113`に残る。

## 6. リスクと限界

- **「シミュレーションが緑」≠「実環境で動く」**。L2と実機smokeで補う。
- **ローカルbuild禁止のためCIが唯一のフィードバックで、1周10〜25分**。反復を速くするため、
  特定テストだけを回すworkflow_dispatchの新設、または既存のGCP Spot self-hosted runner
  (`cargo-ci-gcp-spot-instance`)の利用を検討する(いずれもGitHub Actions経由でルールに反しない)。
- **テスト間汚染**: `SSH_POOL`等は`LazyLock` static。Linux CIはnextest(process-per-test)で起きない。
  macOSジョブは現状`isekai-terminal-core`を除外しているので当面無関係だが、将来含めたときに該当する
  (ユニークキー+後始末の規律は今から守る)。
- **retry**: FaultyStream+russhの反映タイミングに依存するテストは`nextest.toml`のretry対象にする
  (実バグなら3回とも失敗するため見逃さない)。

## 7. Open Questions

- **Q8**: QUIC経路(`ReattachableStream`経由)のサイレント遮断で、`is_closed()`が何秒で立つか。
  → L2で実測。プレーンSSH経路は§4のとおり約300秒(確認済み)。
- **Q9**: #120をクローズしてよいか。L0-1(#121)が緑になったことで、pool修正が実russh Handleでも効く
  ことは確認できた。実機での最終確認は未実施。
- **Q10**: russh keepalive設定の6箇所複製を一本化する際の置き場(`ssh_handler.rs`の
  `production_russh_client_config()`案)。
- **Q11**: L2の夜間実行の置き場とコスト(GitHub-hosted runnerでnetns/iptablesが使えるかは
  既存`rust-core-netlab-check.yml`で実績あり)。
- Q1/Q2/Q4/Q5/Q7(engine lib化、netlab言語、noq fork、in-memoryソケットのmultipath忠実度、RUNTIME注入)は
  **L1を保留するため保留**。着手時にround 1〜3のレビュー内容から再開する。

## 8. 経緯(組み替えの理由)

1. rev1: #120は実機でしか見つからなかった→シミュレーション基盤(L1)が要る、と設計。
2. round 1レビュー: #120は既存の`pooling_e2e_tests`に数十行で足りると指摘(L0の着想)。
3. 調査の途中で、古い作業ツリーを読んでいたことが判明し、#120は既に修正済みと分かった(§0)。
   L0のP0-aを「バグを赤にする」から「修正が実Handleでも効くことの回帰網」へ再定義。
4. L0-1がCI緑、L0-2/定数複製/pool状態機械モデルテストをPR化。作業量に対する発見が小さいことが
   明らかになり、L1(大投資)を保留、安い順(L0→L2)に並べ替えた(本rev4)。

## 9. 却下/比較した代替案

- **実機テストの自動化強化(Firebase Test Lab等)**: キャリア/OEM固有挙動は拾えるが、フォルトの
  時系列制御が難しく再現性が低い。実機smokeの自動化としては別途検討の余地。
- **turmoil/madsim等の全面採用**: noqは自前の`AsyncUdpSocket`/`Runtime` traitを持つため薄い自作の方が
  依存が軽いが、比較検討は未実施(L1着手時に再評価)。
- **`noq-proto`(sans-io)を直接使う決定論テスト**: 非同期ランタイム不要で仮想時間が完全に決定論的。
  ただし本プロジェクトのresume層(`ReattachableStream`等)の上ではなく下の層のテストになる。未検討。
