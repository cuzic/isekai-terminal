# レビュー指摘タスク一覧

コードレビューの指摘を1件1項目のチェックリストとして起票し、修正の進捗を追跡するための文書置き場。

## 2026-09-29 領域別コードレビュー

| 領域 | タスク文書 | 主な対象 |
|---|---|---|
| rust-core | [2026-09-29-review-rust-core.md](2026-09-29-review-rust-core.md) | `rust-core/src/`(`isekai-terminal-core`: SSH/VTE/trzsz/orchestrator/resume) |
| isekai-pipe | [2026-09-29-review-isekai-pipe.md](2026-09-29-review-isekai-pipe.md) | `rust-core/isekai-pipe/`(connect/serve、`src/engine/`) |
| isekai-ssh | [2026-09-29-review-isekai-ssh.md](2026-09-29-review-isekai-ssh.md) | `rust-core/isekai-ssh/` と bootstrap/trust/auth 系クレート |
| transport | [2026-09-29-review-transport.md](2026-09-29-review-transport.md) | `isekai-transport`/`quicmux`/`quicsock` 等の QUIC transport 層 |
| android | [2026-09-29-review-android.md](2026-09-29-review-android.md) | `android/`(Kotlin/Compose UI 層) |
| ci-ios | [2026-09-29-review-ci-ios.md](2026-09-29-review-ci-ios.md) | `.github/`、`scripts/`、`rust-core/scripts/`、`.claude/hooks/`、`ios/` |

## 凡例

### レビュー実施日 / 方法

- **実施日**: 各タスク文書の冒頭に記載する(2026-09-29 のレビューはすべて同日に実施)。
- **方法**: 領域ごとに読み取り専用の静的レビューを行った(レビュー時点ではビルド・テストを実行していない)。
  レビューは起票時点より古いコミットを読んでいることがあるため、起票時に `origin/main` のコードで各指摘が
  今も有効かを確認し、既に直っているものや誤検知は「対応不要」として根拠を書く。
- 修正は領域ごとのブランチ `fix/review-2026-09-<領域slug>` で行い、PR の GitHub Actions で検証する
  (ローカルでのビルド・テストは行わない。`prefer-gh-actions-over-local-cargo` 方針)。

### 優先度(重要度)

| 重要度 | 意味 | 対応順 |
|---|---|---|
| **High** | 実害(セキュリティ・データ消失・CI ゲートの無効化・接続不能など)が現実的に起こりうる | 最優先 |
| **Medium** | 条件付きで実害がある、またはサプライチェーン・再現性・運用上の明確なリスク | High の次 |
| **Low-Medium** | Low と Medium の中間(発生条件が限られるが、起きると影響が大きい) | Medium と同列 |
| **Low** | 影響が小さい・限定的な環境でのみ起きる・テストツールのみの問題 | 最後 |
| **Info** | 方針との矛盾や残骸など、バグではない所見 | 判断が要るものは見送り可 |

### 状態

| 記号 | 意味 |
|---|---|
| `[ ]` | 未着手 |
| `[x]` | 修正済み(修正コミットで状態も更新する) |
| `[-]` | 対応不要(既に修正済み・誤検知など。理由を併記) |
| `[~]` | 見送り(設計判断が要る・担当領域外・リスクが大きすぎる等。理由を併記) |
