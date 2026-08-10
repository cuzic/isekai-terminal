# worktree間でmuslビルド成果物を共有する

`rust-core/src/isekai_pipe_quic_transport.rs`は`isekai-pipe`のmuslクロスビルド済み
バイナリ(`rust-core/scripts/build-isekai-pipe-musl.sh`が生成する
`target/{x86_64,aarch64}-unknown-linux-musl/release/isekai-pipe`)を`include_bytes!`で
無条件に埋め込む。この成果物はgit管理外(`target/`はgitignore対象)なので、
`git worktree add`で新しいworktreeを作るたびに空の状態からになり、そのままでは
`cargo build`/`cargo check`が

```
error: couldn't read .../target/x86_64-unknown-linux-musl/release/isekai-pipe: No such file or directory
```

で必ず失敗する。

## ルール

- 新しいworktreeを作ったら、`scripts/link-worktree-artifacts.sh <worktree-path>`で
  メインworktreeのmuslビルド成果物ディレクトリをシンボリックリンクする。
  `.githooks/post-checkout`(`git config core.hooksPath .githooks`で有効化済み)が
  `git worktree add`実行時にこれを自動で呼ぶよう配線してある。
- **ただしAgent toolの`isolation: worktree`経由で作られたworktreeでは、この
  post-checkout hookが必ずしも発火しない**(2026-08-09、Phase 2の複数worktreeで
  未発火を確認。素の`git worktree add`を経由していない可能性がある。原因未特定)。
  そのため、多数のworktreeを一括起動した直後は、以下のように全worktreeへ
  明示的に一括適用するのが確実:
  ```bash
  for wt in .claude/worktrees/agent-*; do
      scripts/link-worktree-artifacts.sh "$wt"
  done
  ```
- worktree作成後にビルドチェックフックがこのエラーを報告してきても、慌てて
  ワークツリー内のコードを疑う前に、まずこのリンクが張られているか確認する。

## 理由

このエラーは該当worktreeでのコード変更とは無関係に発生する環境起因のノイズであり、
2026-08-09のセッションでは並列worktreeエージェント運用中に何十回もこの誤解を招く
エラー通知が発生し、注意を逸らされた。post-checkout hookだけに頼らず能動的に
リンクを張ることで、この種のノイズを事前に潰せる。

## 大量worktree運用時のディスク容量にも注意する

muslビルド成果物と違い、各worktree自身の`rust-core/target/debug/`はシンボリック
リンクの対象外で、worktreeごとに独立して肥大化する。多数のworktreeが並列で
`cargo build`すると、サンドボックスのディスクが急速に埋まる。

- **実例**: 2026-08-09、11個のworktreeを並列運用した際、ディスク使用率が2度
  97%(空き7.5GB)まで逼迫し、`Write`/`Bash`ツールが`ENOSPC`で失敗した
  (単体worktreeの肥大化については既存メモリ`rust-core-target-debug-fills-disk.md`
  参照——単体でも37GBまで育った実績があり、これが並列worktree分だけ重なる)。
- **対処**: 作業が完了し、マージ済みで以後使わないworktreeの`target/debug`は
  都度`rm -rf`で削除する。多数のworktreeを立ち上げて並走させる作業では、
  各エージェントの完了報告を受けるたびにこれを習慣化する:
  ```bash
  rm -rf .claude/worktrees/agent-<id>/rust-core/target/debug
  ```
- **やってはいけないこと**: まだ作業中のworktreeの`target/debug`を消さない
  (ビルドキャッシュが失われて次のビルドが遅くなるだけでなく、進行中のcargo
  プロセスと競合しうる)。muslビルド成果物ディレクトリ(シンボリックリンク先の
  メインworktree)自体も消さない——他worktreeから共有参照されている。

## 参照実装

- `scripts/link-worktree-artifacts.sh`
- `.githooks/post-checkout`
- `git config core.hooksPath .githooks`(リポジトリ設定)
