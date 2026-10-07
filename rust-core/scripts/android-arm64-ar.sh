#!/usr/bin/env bash
# cargo ar for target aarch64-linux-android.
set -euo pipefail
# cdはしない: cargo/rustcが相対パス引数を渡した場合に壊れるため、呼び出し元のcwdのままexecする。
source "$(dirname "${BASH_SOURCE[0]}")/ndk-common.sh"
exec "$NDK_TOOLCHAIN_BIN/llvm-ar" "$@"
