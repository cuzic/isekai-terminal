#!/usr/bin/env bash
# Resolves the Android NDK toolchain root without any hardcoded, machine-specific path.
# Sourced by the android-arm64-*.sh cargo linker/ar wrappers.
set -euo pipefail

resolve_ndk_root() {
    if [ -n "${ANDROID_NDK_HOME:-}" ]; then
        echo "$ANDROID_NDK_HOME"
        return
    fi
    if [ -n "${ANDROID_NDK_ROOT:-}" ]; then
        echo "$ANDROID_NDK_ROOT"
        return
    fi
    if [ -n "${ANDROID_HOME:-}" ] && [ -d "$ANDROID_HOME/ndk" ]; then
        # Pick the highest installed NDK version under $ANDROID_HOME/ndk.
        find "$ANDROID_HOME/ndk" -mindepth 1 -maxdepth 1 -type d | sort -V | tail -n1
        return
    fi
    echo "error: Android NDK not found. Set ANDROID_NDK_HOME (or ANDROID_NDK_ROOT, or ANDROID_HOME with an ndk/ subdir)." >&2
    exit 1
}

resolve_host_tag() {
    case "$(uname -s)" in
        Linux) echo "linux-x86_64" ;;
        Darwin) echo "darwin-x86_64" ;;
        MINGW*|MSYS*|CYGWIN*) echo "windows-x86_64" ;;
        *) echo "error: unsupported host OS for Android NDK toolchain: $(uname -s)" >&2; exit 1 ;;
    esac
}

NDK_ROOT="$(resolve_ndk_root)"
HOST_TAG="$(resolve_host_tag)"
NDK_TOOLCHAIN_BIN="$NDK_ROOT/toolchains/llvm/prebuilt/$HOST_TAG/bin"
