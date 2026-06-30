#!/bin/bash
# Android ログをリアルタイムでストリーミングする。
# adb が接続済みの場合に使う。
#
# 使い方:
#   ./tools/watch-logs.sh                  # 全スパイクタグ
#   ./tools/watch-logs.sh CanvasSpike      # 特定タグのみ
#   DEVICE=100.102.163.14:39967 ./tools/watch-logs.sh

DEVICE="${DEVICE:-100.102.163.14:39967}"
FILTER="${1:-}"

ADB="adb -s $DEVICE"

# 接続確認
if ! $ADB get-state &>/dev/null; then
    echo "❌ ADB 未接続: $DEVICE"
    echo "   adb connect $DEVICE を実行してください"
    exit 1
fi

echo "📱 Android ログ監視開始 (device=$DEVICE)"
echo "   Ctrl+C で終了"
echo ""

# アプリタグのフィルタ
if [ -n "$FILTER" ]; then
    TAG_FILTER="$FILTER:D *:S"
else
    TAG_FILTER="CanvasSpike:D KeystoreSpike:D FgsSpike:D MainActivity:I RemoteLogger:W AndroidRuntime:E *:S"
fi

$ADB logcat -v time $TAG_FILTER
