#!/usr/bin/env bash
# cargo linker for target aarch64-linux-android. API level must match app/build.gradle.kts minSdk.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"
source ./ndk-common.sh
exec "$NDK_TOOLCHAIN_BIN/aarch64-linux-android28-clang" "$@"
