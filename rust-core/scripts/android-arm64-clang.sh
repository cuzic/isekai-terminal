#!/usr/bin/env bash
# cargo linker for target aarch64-linux-android. API level must match android/build.gradle.kts minSdk.
set -euo pipefail
# cdはしない: cargo/rustcが相対パス引数を渡した場合に壊れるため、呼び出し元のcwdのままexecする。
source "$(dirname "${BASH_SOURCE[0]}")/ndk-common.sh"
exec "$NDK_TOOLCHAIN_BIN/aarch64-linux-android28-clang" "$@"
