#!/usr/bin/env bash
# tssh-core の UniFFI Swift バインディングを生成する。
#
# uniffi の --library 方式（コンパイル済みバイナリのメタデータを読むだけ）を使うため、
# ホストのデフォルトターゲット（Linux/macOS どちらでも可）でビルドした cdylib を
# 入力にできる。iOS 向けクロスコンパイル環境が無くても実行できる
# （Kotlin 向けバインディング生成が target/debug/libtssh_core.so を使っているのと同じ理由）。
#
# 出力: ../ios/Sources/TsshCore/generated/ 以下に生成物と .sha256
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

cargo build -p tssh-core

LIB=""
for cand in target/debug/libtssh_core.so target/debug/libtssh_core.dylib; do
    if [[ -f "$cand" ]]; then
        LIB="$cand"
        break
    fi
done
if [[ -z "$LIB" ]]; then
    echo "error: libtssh_core (.so/.dylib) not found under target/debug/" >&2
    exit 1
fi

OUT_DIR="../ios/Sources/TsshCore/generated"
rm -rf "$OUT_DIR"
mkdir -p "$OUT_DIR"
cargo run -p uniffi-bindgen -- generate --library "$LIB" --language swift --out-dir "$OUT_DIR"

for f in "$OUT_DIR"/*; do
    [[ -f "$f" ]] || continue
    sha256sum "$f" | awk '{print $1}' > "$f.sha256"
done

echo
echo "done. generated files:"
ls "$OUT_DIR"
