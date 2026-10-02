# Architecture Decision Records

設計判断の記録(ADR)。形式は [MADR](https://adr.github.io/madr/) / Michael Nygard 流に準拠する。

## 規約

- **置き場所**: `docs/adr/`。ファイル名は `NNNN-kebab-case-title.md`(`NNNN` は4桁ゼロ埋めの連番、
  一度付けたら変更・再利用しない。起草日順に採番)。
- **見出し**: 先頭は `# ADR: <判断の要約>`、直後に `- **Status**: Proposed | Accepted | Rejected | Deprecated | Superseded by NNNN`(MADR の語彙に準拠。`Draft`/`Approved` は使わない)。
- **レビュー記録**: ADR 本体とは分け、`docs/adr/reviews/NNNN-<slug>-review-roundN.md` に置く
  (`NNNN` は対象 ADR の番号)。ADR 本体はレビューの結論を反映した最終形だけを書く。
- **番号の取り方**: 新規 ADR は下表の最大番号 + 1。並列ブランチでの番号衝突を避けるため、
  起草を始めたらすぐ表に行を足してコミットする。
- ADR を覆すときは旧 ADR を削除せず、Status を `Superseded by NNNN` に変えて新 ADR を書く。
- 実機スパイク等、判断そのものではない調査計画は `docs/spikes/` に置く。

## 索引

| # | タイトル | Status | 起草日 |
|---|---|---|---|
| [0001](0001-ios-parity-implementation.md) | Android→iOS 機能パリティの実装方針 | Accepted | 2026-08-21 |
| [0002](0002-midsession-disconnect-recovery.md) | セッション確立後のネットワーク切断からの自動リカバリ（`isekai-ssh` Epic R） | Accepted | 2026-08-31 |
| [0003](0003-param-cohesion-refactor.md) | 引数過多・凝集性不足の是正リファクタ(5件) | Accepted | 2026-09-05 |
| [0004](0004-isekai-ssh-local-scrollback.md) | isekai-ssh（Windows）にローカルscrollbackバッファを持たせる | Proposed | 2026-09-07 |
| [0005](0005-input-resume-symmetry.md) | 切断中のC→S入力をS→C出力と対称にresumeバッファする | Proposed | 2026-09-07 |
| [0006](0006-stun-reestablish-continuity.md) | STUN P2Pの真の再ランデブー時にもresume連続性を保てないか | Accepted (PR #118で実装済み) | 2026-09-07 |
| [0007](0007-connection-observability.md) | 接続/セッションライフサイクルの可観測性(tracing化) | Accepted | 2026-09-11 |
| [0008](0008-isekai-ssh-observability.md) | isekai-ssh(Windows)の接続ライフサイクル可観測性 | Accepted | 2026-09-11 |
| [0009](0009-isekai-ssh-exit-diagnostics.md) | isekai-ssh(Windows native)の異常終了理由が診断ログに一切残らない | Accepted | 2026-09-12 |
| [0010](0010-isekai-ssh-reconnect-latency.md) | isekai-ssh(Windows)の再接続レイテンシと進捗不可視性の改善 | Proposed | 2026-09-15 |
| [0011](0011-android-connect-failure-classification.md) | 接続失敗の分類に基づく体系的リトライ(「常に接続できる」原則の核をAndroidへ移植) | Proposed | 2026-09-16 |
| [0012](0012-android-midsession-resume-budget.md) | mid-session reattach(resume)の予算と失敗理由の区別をisekai-pipe相当に近づける(Android) | Proposed | 2026-09-16 |
| [0013](0013-android-reconnect-timeout.md) | 完全切断時の自動再接続タイムアウトを見直す(Android) | Proposed | 2026-09-16 |
| [0014](0014-android-stun-p2p-auto-fallback.md) | STUN P2Pに、relayへの自動フォールバックをopt-in変種として追加する(Android) | Proposed | 2026-09-16 |
| [0015](0015-android-stun-reestablish-continuity.md) | Android版STUN P2Pの真の再ランデブー時にresume連続性を保つ | Proposed | 2026-09-16 |
| [0016](0016-android-pool-stale-handle.md) | SSH接続プール(`pool.rs`)のstale handle再利用により`TransportPreference::Auto`の自動再接続が構造的に失敗する | Proposed | 2026-09-16 |
| [0017](0017-android-pool-fix-followup-gaps.md) | pool.rs修正(issue #120/PR #119)実装後にコードレビューで見つかった残存ギャップ | Proposed | 2026-09-17 |
| [0018](0018-connection-resilience-simulation.md) | 接続耐性を実機なしで検証する方法 | Proposed | 2026-09-21 |
