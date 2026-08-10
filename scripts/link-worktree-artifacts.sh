#!/usr/bin/env bash
# 新しく作った git worktree へ、gitignore対象のビルド成果物のうち
# worktree間で共有して問題ないもの(現状: isekai-pipe のmuslクロスビルド済み
# バイナリ。rust-core/src/isekai_pipe_quic_transport.rs が include_bytes! で
# 埋め込むため、worktree ごとに毎回 build-isekai-pipe-musl.sh を再実行しないと
# cargo check/build がそもそも通らない)をメインworktreeからシンボリックリンクする。
#
# git 自体には「特定のignore対象ファイルだけをworktree間で共有する」ネイティブ機能は
# 無い(git-worktree-share/worktree-link 等のサードパーティツール相当を自前実装したもの)。
#
# 使い方:
#   scripts/link-worktree-artifacts.sh <worktree-path>   # 明示指定
#   scripts/link-worktree-artifacts.sh                   # 現在のworktreeに対して実行
set -euo pipefail

TARGET_WORKTREE="${1:-$(pwd)}"
TARGET_WORKTREE="$(cd "$TARGET_WORKTREE" && pwd)"

if [ ! -d "$TARGET_WORKTREE/.git" ] && [ ! -f "$TARGET_WORKTREE/.git" ]; then
    echo "error: $TARGET_WORKTREE does not look like a git worktree (no .git)" >&2
    exit 1
fi

# メインworktreeは `git worktree list` の先頭行(常にメインworktree)から取る。
MAIN_WORKTREE="$(cd "$TARGET_WORKTREE" && git worktree list | head -1 | awk '{print $1}')"

if [ "$MAIN_WORKTREE" = "$TARGET_WORKTREE" ]; then
    echo "note: $TARGET_WORKTREE is the main worktree, nothing to link" >&2
    exit 0
fi

# リンク対象: パスはメインworktree相対。ディレクトリ単位でシンボリックリンクする
# (ファイル単位でリンクすると、cargo が同じディレクトリに他の成果物を書き込もうと
# した際にリンク元を汚染しうるため、意図的にディレクトリ丸ごとリンクする)。
LINK_TARGETS=(
    "rust-core/target/x86_64-unknown-linux-musl"
    "rust-core/target/aarch64-unknown-linux-musl"
)

linked=0
for rel in "${LINK_TARGETS[@]}"; do
    src="$MAIN_WORKTREE/$rel"
    dst="$TARGET_WORKTREE/$rel"

    if [ -e "$dst" ] && [ ! -L "$dst" ]; then
        echo "skip: $dst already exists and is not a symlink (leaving as-is)" >&2
        continue
    fi
    if [ -L "$dst" ]; then
        # 既存のリンクは張り直す(メインworktree側のパスが変わっていた場合に追従)
        rm -f "$dst"
    fi
    if [ ! -d "$src" ]; then
        echo "skip: $src does not exist in main worktree yet (nothing to link)" >&2
        continue
    fi

    mkdir -p "$(dirname "$dst")"
    ln -s "$src" "$dst"
    echo "linked $dst -> $src"
    linked=$((linked + 1))
done

if [ "$linked" -eq 0 ]; then
    echo "note: nothing linked (see skip/note messages above)" >&2
fi
