#!/usr/bin/env bash
# cargo ar for target aarch64-linux-android.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"
source ./ndk-common.sh
exec "$NDK_TOOLCHAIN_BIN/llvm-ar" "$@"
