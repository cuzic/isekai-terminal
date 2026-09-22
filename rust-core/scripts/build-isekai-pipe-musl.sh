#!/usr/bin/env bash
# isekai-pipe を x86_64/aarch64 の静的リンク Linux (musl) バイナリとしてビルドする。
# このバイナリは isekai-terminal-core（Android）に埋め込まれ、`isekai-pipe serve`
# としてリモートホストへ配布・起動される（旧 isekai-helper crate は
# ISEKAI_PIPE_MIGRATION.md P5 で isekai-pipe へ統合済み）。
#
# cargo-zigbuild（内部で zig を C クロスコンパイラ/リンカとして使う）を用いるため、
# musl-gcc 等のシステムトゥールチェーンは不要。
#
# 事前準備（初回のみ）:
#   brew install zig
#   cargo install cargo-zigbuild
#   rustup target add x86_64-unknown-linux-musl aarch64-unknown-linux-musl
#
# 出力: rust-core/target/<triple>/release/isekai-pipe と .sha256
#
# 2ターゲットは`cargo zigbuild`を並列に起動してビルドする(2026-09-22、CI高速化)。
# それぞれ`target/<triple>/`以下の独立したサブディレクトリに成果物を書くため、
# Cargoの内部ロック(target直下の`.cargo-lock`、依存クレートのダウンロード/展開等
# 共有状態のみを保護する)で安全に共存でき、通常のクロスコンパイル(異なる
# --targetを複数回呼ぶ)と同じ前提が成り立つ。CI実測で逐次実行が合計約249秒
# (x86_64約114秒+aarch64約120秒)かかっていたのに対し、並列化でmax(114,120)
# ≈120秒程度への短縮を見込む。出力は`[<target>] `を先頭に付けて区別しつつ
# 両方をリアルタイムでインターリーブ表示する(`set -o pipefail`により、
# `cargo`が失敗した場合は`sed`が成功していても失敗として伝播する)。
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

TARGETS=(x86_64-unknown-linux-musl aarch64-unknown-linux-musl)

pids=()
for target in "${TARGETS[@]}"; do
    echo "=== building isekai-pipe for $target (parallel) ==="
    (
        set -o pipefail
        cargo zigbuild --release -p isekai-pipe --target "$target" 2>&1 | sed -u "s/^/[$target] /"
    ) &
    pids+=("$!")
done

failed=0
for i in "${!pids[@]}"; do
    if ! wait "${pids[$i]}"; then
        echo "=== FAILED: building isekai-pipe for ${TARGETS[$i]} ==="
        failed=1
    fi
done
if [ "$failed" -ne 0 ]; then
    exit 1
fi

for target in "${TARGETS[@]}"; do
    bin_path="target/$target/release/isekai-pipe"
    sha256sum "$bin_path" | awk '{print $1}' > "$bin_path.sha256"
    echo "  -> $bin_path ($(du -h "$bin_path" | cut -f1), sha256=$(cat "$bin_path.sha256"))"
done

echo
echo "done. binaries:"
for target in "${TARGETS[@]}"; do
    echo "  target/$target/release/isekai-pipe"
done
