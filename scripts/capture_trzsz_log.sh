#!/usr/bin/env bash
# Phase 4D (#70): trzsz 転送ログを adb でキャプチャする
#
# Rust コア (rust-core) は android_logger のタグ "isekai-terminal-core" でログを出力する
# (rust-core/src/lib.rs: .with_tag("isekai-terminal-core"))。
# trzsz の wire プロトコルマーカーと UI 側 (IsekaiTerminal* タグ) の転送イベントを抽出する。
#
# Usage: ./scripts/capture_trzsz_log.sh [device-serial]
#   device-serial: 複数デバイス接続時に対象を指定 (省略可)

set -euo pipefail

DEVICE="${1:-}"
ADB=(adb)
[ -n "$DEVICE" ] && ADB=(adb -s "$DEVICE")

echo "=== Capturing trzsz transfer log (tag: isekai-terminal-core + IsekaiTerminal*) ==="
echo "Ctrl+C to stop"
echo ""

"${ADB[@]}" logcat -c

# rust-core の "isekai-terminal-core" と UI 側 SSH/VM タグを購読し、
# trzsz プロトコルマーカーと転送コールバックの行だけを抽出する。
# マーカーは rust-core/src/trzsz.rs に実在するもの: #ACT #NUM #NAME #SIZE #DATA #MD5 #SUCC
"${ADB[@]}" logcat -s "isekai-terminal-core:V" "IsekaiTerminalSSH:V" "IsekaiTerminalVM:V" \
  | grep --line-buffered -E \
    '::TRZSZ:TRANSFER:|#ACT|#NUM|#NAME|#SIZE|#DATA|#MD5|#SUCC|onTrzsz|TrzszTransfer|OnProgress|OnFinished'
