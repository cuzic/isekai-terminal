#!/usr/bin/env bash
# isekai-ssh を x86_64/aarch64 の静的リンク Linux (musl) バイナリとしてビルドする。
#
# cargo-zigbuild（内部で zig を C クロスコンパイラ/リンカとして使う）を用いるため、
# musl-gcc 等のシステムトゥールチェーンは不要。
#
# 事前準備（初回のみ）:
#   brew install zig
#   cargo install cargo-zigbuild
#   rustup target add x86_64-unknown-linux-musl aarch64-unknown-linux-musl
#
# 重要: `dev-insecure` feature（`--dev-insecure-*` 系フラグを有効化する、開発/テスト専用の
# 信頼ストアバイパス）は配布用ビルドでは絶対に有効化しない。デフォルトfeatureのみでビルドする
# ことで、リリースバイナリに `--dev-insecure-*` フラグが一切現れないことを保証する
# （`isekai-ssh/tests/help_purity.rs` が検証している不変条件、ISEKAI_SSH_DESIGN.md 参照）。
#
# 出力: rust-core/target/<triple>/release/isekai-ssh と .sha256
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

TARGETS=(x86_64-unknown-linux-musl aarch64-unknown-linux-musl)

for target in "${TARGETS[@]}"; do
    echo "=== building isekai-ssh for $target ==="
    cargo zigbuild --release -p isekai-ssh --target "$target"

    bin_path="target/$target/release/isekai-ssh"
    sha256sum "$bin_path" | awk '{print $1}' > "$bin_path.sha256"
    echo "  -> $bin_path ($(du -h "$bin_path" | cut -f1), sha256=$(cat "$bin_path.sha256"))"
done

echo
echo "done. binaries:"
for target in "${TARGETS[@]}"; do
    echo "  target/$target/release/isekai-ssh"
done
