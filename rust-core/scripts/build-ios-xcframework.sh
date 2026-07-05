#!/usr/bin/env bash
# tssh-core を iOS 向けにクロスコンパイルし、XCFramework としてパッケージングする。
#
# macOS + Xcode が必須（Rust側のクロスコンパイルにXcode同梱のcc/ar/ld、
# パッケージングに xcodebuild/lipo を使うため）。
#
# 事前準備（初回のみ）:
#   rustup target add aarch64-apple-ios aarch64-apple-ios-sim x86_64-apple-ios
#
# 先に rust-core/scripts/generate-swift-bindings.sh を実行し、
# ../ios/Sources/TsshCoreLogic/generated/ にバインディングが生成済みであることが前提。
#
# 出力: ../ios/Frameworks/TsshCoreFFI.xcframework
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

if [[ "$(uname)" != "Darwin" ]]; then
    echo "error: this script requires macOS (Xcode toolchain: cc/ar/lipo/xcodebuild)" >&2
    exit 1
fi

GENERATED_DIR="../ios/Sources/TsshCoreLogic/generated"
if [[ ! -f "$GENERATED_DIR/tssh_coreFFI.modulemap" ]]; then
    echo "error: $GENERATED_DIR/tssh_coreFFI.modulemap not found." >&2
    echo "       run rust-core/scripts/generate-swift-bindings.sh first." >&2
    exit 1
fi

TARGET_DEVICE=aarch64-apple-ios
TARGETS_SIM=(aarch64-apple-ios-sim x86_64-apple-ios)

for t in "$TARGET_DEVICE" "${TARGETS_SIM[@]}"; do
    echo "=== building tssh-core for $t ==="
    cargo build --release --target "$t" -p tssh-core
done

# XCFramework は1プラットフォームバリアントにつき1ライブラリしか受け付けないため、
# シミュレータ向け arm64(Apple Silicon Mac) + x86_64(Intel Mac) を事前にfat化する。
SIM_FAT_DIR="target/ios-sim-fat"
mkdir -p "$SIM_FAT_DIR"
lipo -create \
    "target/aarch64-apple-ios-sim/release/libtssh_core.a" \
    "target/x86_64-apple-ios/release/libtssh_core.a" \
    -output "$SIM_FAT_DIR/libtssh_core.a"

# xcodebuild -create-xcframework の -headers ディレクトリは `module.modulemap` という
# ファイル名を期待する。uniffi-bindgen の実際の出力ファイル名は `tssh_coreFFI.modulemap`
# （module宣言自体は `module tssh_coreFFI { ... }` のままでよく、ファイル名だけの問題）。
HEADERS_DIR="target/ios-xcframework-headers"
rm -rf "$HEADERS_DIR"
mkdir -p "$HEADERS_DIR"
cp "$GENERATED_DIR/tssh_coreFFI.h" "$HEADERS_DIR/"
cp "$GENERATED_DIR/tssh_coreFFI.modulemap" "$HEADERS_DIR/module.modulemap"

OUT_XCFRAMEWORK="../ios/Frameworks/TsshCoreFFI.xcframework"
rm -rf "$OUT_XCFRAMEWORK"
xcodebuild -create-xcframework \
    -library "target/$TARGET_DEVICE/release/libtssh_core.a" -headers "$HEADERS_DIR" \
    -library "$SIM_FAT_DIR/libtssh_core.a" -headers "$HEADERS_DIR" \
    -output "$OUT_XCFRAMEWORK"

echo
echo "done. $OUT_XCFRAMEWORK"
