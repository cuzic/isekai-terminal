#!/usr/bin/env bash
# tssh-core を Linux ネイティブ(x86_64/aarch64-unknown-linux-gnu)向けにビルドし、
# TsshCoreLogic(Swiftの純ロジック層)をLinux上で`swift build`/`swift test`できる
# ようにする。XCFramework(build-ios-xcframework.sh)はmacOS専用パッケージング
# 形式のためLinuxでは使えず、代わりにSwiftPMの`systemLibrary`ターゲット
# (`ios/Sources/TsshCoreFFILinux/`)から直接 .so をリンクする。
#
# 事前準備: rust-core/scripts/generate-swift-bindings.sh が
# ../ios/Sources/TsshCoreLogic/generated/ にSwiftバインディングを生成済みであること
# (このスクリプトが最初に呼び出すので、単独では前提を満たさなくてよい)。
#
# 出力:
#   ../ios/Frameworks/linux/libtssh_core.so (gitignore対象、CI/ローカルの一時生成物)
#   ../ios/Sources/TsshCoreFFILinux/module.modulemap (Linux向けに`use "Darwin"`を除去したもの)
#
# 使い方(このスクリプトの実行後、ios/ で):
#   export LIBRARY_PATH="$(pwd)/../ios/Frameworks/linux:$LIBRARY_PATH"
#   export LD_LIBRARY_PATH="$(pwd)/../ios/Frameworks/linux:$LD_LIBRARY_PATH"
#   swift test --filter TsshCoreLogicTests
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

if [[ "$(uname)" != "Linux" ]]; then
    echo "error: this script targets native Linux builds (uname != Linux)" >&2
    exit 1
fi

bash scripts/generate-swift-bindings.sh

GENERATED_DIR="../ios/Sources/TsshCoreLogic/generated"
LIB="target/debug/libtssh_core.so"
if [[ ! -f "$LIB" ]]; then
    echo "error: $LIB not found (generate-swift-bindings.sh should have built it)" >&2
    exit 1
fi

OUT_FRAMEWORKS_DIR="../ios/Frameworks/linux"
mkdir -p "$OUT_FRAMEWORKS_DIR"
cp "$LIB" "$OUT_FRAMEWORKS_DIR/libtssh_core.so"

# uniffi-bindgenが生成するmodulemapは常に`use "Darwin"`を含む(ホストOSに関係なく
# 固定テンプレート)。DarwinモジュールはLinuxに存在せずクラッシュするため、
# Linux向けターゲット(`TsshCoreFFILinux`)ではこの行を除いたコピーを使う。
# ヘッダー自体(`tssh_coreFFI.h`)はホストOSによらず内容が同一(既存の
# XCFramework用生成物と診断で確認済み)なのでシンボリックリンクで共有する。
FFI_LINUX_DIR="Sources/TsshCoreFFILinux"
mkdir -p "../ios/$FFI_LINUX_DIR"
ln -sf "../TsshCoreLogic/generated/tssh_coreFFI.h" "../ios/$FFI_LINUX_DIR/tssh_coreFFI.h"
sed 's/use "Darwin"//' "$GENERATED_DIR/tssh_coreFFI.modulemap" > "../ios/$FFI_LINUX_DIR/module.modulemap"

echo
echo "done."
echo "  $OUT_FRAMEWORKS_DIR/libtssh_core.so"
echo "  ../ios/$FFI_LINUX_DIR/module.modulemap"
