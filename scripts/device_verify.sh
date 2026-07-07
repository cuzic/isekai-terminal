#!/usr/bin/env bash
# 実機での動作確認(TESTING.md)のうち、adb だけで完結する範囲をスクリプト化したもの。
#
# TESTING.md の手順のうち、日本語IME変換やピンチズームなど「目視確認が前提」の項目は
# 対象外(scripts/lib/adb_ui.py の uiautomator dump ベースの座標タップで代替できないため)。
# 対象: 起動確認 / 鍵生成 / プロファイル追加 / SSH接続(鍵認証) / 画面回転 / バックグラウンド
# 移行・復帰 / 切断 / 後片付け(生成した鍵・プロファイル・authorized_keysエントリの削除)。
#
# パスワード認証(TESTING.md 5)は対象外: サーバー側 sshd_config の PasswordAuthentication を
# 一時的にでも有効化するのは、このスクリプトが動く実マシン(=デフォルトのSSHテスト対象)に
# とって不要なセキュリティリスクのため、意図的に行わない。
#
# Usage:
#   ./scripts/device_verify.sh [--device SERIAL] [--host HOST] [--port PORT] [--user USER]
#                               [--skip-install] [--keep]
#
#   --device SERIAL   adb -s に渡すデバイスシリアル(省略時: 接続中デバイスが1台ならそれを使う)
#   --host/--port/--user  接続先SSHサーバー(デフォルト: 100.100.45.36:22, 実行ユーザー名)
#   --skip-install    ./gradlew installDebug をスキップ(既にインストール済みの場合)
#   --keep            最後の後片付け(生成鍵・プロファイル・authorized_keys削除)をスキップ
#                      (失敗時の実機状態をそのまま確認したい場合)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
UI_HELPER="$SCRIPT_DIR/lib/adb_ui.py"

DEVICE=""
SSH_HOST="100.100.45.36"
SSH_PORT="22"
SSH_USER="$(whoami)"
SKIP_INSTALL=0
KEEP=0

while [ $# -gt 0 ]; do
    case "$1" in
        --device) DEVICE="$2"; shift 2 ;;
        --host) SSH_HOST="$2"; shift 2 ;;
        --port) SSH_PORT="$2"; shift 2 ;;
        --user) SSH_USER="$2"; shift 2 ;;
        --skip-install) SKIP_INSTALL=1; shift ;;
        --keep) KEEP=1; shift ;;
        -h|--help) grep '^#' "$0" | sed 's/^# \?//'; exit 0 ;;
        *) echo "unknown option: $1" >&2; exit 1 ;;
    esac
done

if [ -z "$DEVICE" ]; then
    mapfile -t devs < <(adb devices | awk 'NR>1 && $2=="device" {print $1}')
    if [ "${#devs[@]}" -ne 1 ]; then
        echo "複数/0台のデバイスが接続されています。--device SERIAL で指定してください:" >&2
        printf '  %s\n' "${devs[@]}" >&2
        exit 1
    fi
    DEVICE="${devs[0]}"
fi

echo "=== device: $DEVICE ($(adb -s "$DEVICE" shell getprop ro.product.model | tr -d '\r') / Android $(adb -s "$DEVICE" shell getprop ro.build.version.release | tr -d '\r')) ==="
echo "=== ssh target: ${SSH_USER}@${SSH_HOST}:${SSH_PORT} ==="

TS="$(date +%s)"
KEY_LABEL="isekai-e2e-${TS}"
PROFILE_LABEL="isekai-e2e-profile-${TS}"
APP_ID="tools.isekai.terminal"

ui() { python3 "$UI_HELPER" --device "$DEVICE" "$@"; }
adbd() { adb -s "$DEVICE" "$@"; }

LOGFILE="$(mktemp "${TMPDIR:-/tmp}/isekai_device_verify_XXXXXX.log")"
LOGCAT_PID=""
ORIG_ACCEL_ROTATION=""
ORIG_USER_ROTATION=""
ORIG_SCREEN_TIMEOUT=""
PASS_COUNT=0
FAIL_STEP=""

cleanup() {
    local rc=$?
    if [ -n "$LOGCAT_PID" ]; then
        kill "$LOGCAT_PID" 2>/dev/null || true
        wait "$LOGCAT_PID" 2>/dev/null || true
    fi
    if [ -n "$ORIG_ACCEL_ROTATION" ]; then
        adbd shell settings put system accelerometer_rotation "$ORIG_ACCEL_ROTATION" 2>/dev/null || true
    fi
    if [ -n "$ORIG_USER_ROTATION" ]; then
        adbd shell settings put system user_rotation "$ORIG_USER_ROTATION" 2>/dev/null || true
    fi
    if [ -n "$ORIG_SCREEN_TIMEOUT" ]; then
        adbd shell settings put system screen_off_timeout "$ORIG_SCREEN_TIMEOUT" 2>/dev/null || true
    fi
    echo ""
    echo "=== full log: $LOGFILE ==="
    if [ "$rc" -ne 0 ]; then
        echo "=== FAILED at: ${FAIL_STEP:-unknown} (exit $rc) ==="
    else
        echo "=== all steps passed ($PASS_COUNT assertions) ==="
    fi
}
trap cleanup EXIT

checkpoint() { wc -l < "$LOGFILE" | tr -d ' '; }

# $1=since(行数) $2=grep -E パターン $3=説明 $4=timeout秒(省略時10)
assert_since() {
    local since="$1" pattern="$2" desc="$3" timeout="${4:-10}"
    FAIL_STEP="$desc"
    local deadline=$((SECONDS + timeout))
    while [ "$SECONDS" -lt "$deadline" ]; do
        if tail -n "+$((since + 1))" "$LOGFILE" | grep -Eq "$pattern"; then
            echo "  PASS: $desc"
            PASS_COUNT=$((PASS_COUNT + 1))
            return 0
        fi
        sleep 0.3
    done
    echo "  FAIL: $desc"
    echo "  (${timeout}s 待っても logcat にパターンが出現しませんでした: $pattern)"
    echo "----- 直近ログ -----"
    tail -n 40 "$LOGFILE"
    exit 1
}

# $1=since $2=grep -E パターン(出現してはいけない) $3=説明 $4=待機秒(省略時2)
assert_absent_since() {
    local since="$1" pattern="$2" desc="$3" wait_s="${4:-2}"
    FAIL_STEP="$desc"
    sleep "$wait_s"
    if tail -n "+$((since + 1))" "$LOGFILE" | grep -Eq "$pattern"; then
        echo "  FAIL: $desc (想定外に出現: $pattern)"
        tail -n 40 "$LOGFILE"
        exit 1
    fi
    echo "  PASS: $desc"
    PASS_COUNT=$((PASS_COUNT + 1))
}

echo ""
echo "--- 0. 事前準備 ---"
if [ "$SKIP_INSTALL" -eq 0 ]; then
    echo "installDebug..."
    (cd "$REPO_ROOT" && ./gradlew installDebug -q)
else
    echo "(--skip-install: ビルド/インストールをスキップ)"
fi

ORIG_ACCEL_ROTATION="$(adbd shell settings get system accelerometer_rotation | tr -d '\r')"
ORIG_USER_ROTATION="$(adbd shell settings get system user_rotation | tr -d '\r')"
ORIG_SCREEN_TIMEOUT="$(adbd shell settings get system screen_off_timeout | tr -d '\r')"
adbd shell settings put system screen_off_timeout 1800000
adbd shell input keyevent KEYCODE_WAKEUP
# uiautomator dump の座標は縦向き前提で計算しているため、実行中に端末が横向きに
# なっていると(センサーの自動回転などで)全タップがずれる。開始時に縦固定する。
adbd shell settings put system accelerometer_rotation 0
adbd shell settings put system user_rotation 0
sleep 0.5

adbd logcat -c
adbd logcat -v raw -s IsekaiTerminalVM IsekaiTerminalTabsVM IsekaiTerminalSSH IsekaiTerminalNav \
    IsekaiTerminalProfile IsekaiTerminalKey MainActivity > "$LOGFILE" 2>&1 &
LOGCAT_PID=$!
sleep 0.5

echo ""
echo "--- 1. 起動確認 ---"
since=$(checkpoint)
adbd shell am force-stop "$APP_ID"
# ランチャーアイコンをタップした場合と同じ action/category を明示する。ここで
# component名だけの Intent(action/category なし)でタスクを起こすと、そのタスクの
# "root intent" が MAIN/LAUNCHER と一致しなくなり、後段(6.復帰)で全く同じ
# action/category 付き Intent で am start してもタスクの前面化ではなく
# 新しい MainActivity インスタンスが積み上がってしまう(実機で確認済みの罠)。
adbd shell am start -a android.intent.action.MAIN -c android.intent.category.LAUNCHER -n "$APP_ID/.MainActivity" > /dev/null
assert_since "$since" "app started" "MainActivity起動"
assert_since "$since" "TerminalTabsViewModel created" "TabsViewModel生成"
assert_since "$since" "→ ProfileList" "ProfileList画面へ遷移"
assert_since "$since" "loaded [0-9]+ profile" "プロファイル一覧読み込み"

echo ""
echo "--- 2. 鍵生成(KeyList) ---"
since=$(checkpoint)
ui tap --content-desc "メニュー"
ui tap --text "鍵管理"
assert_since "$since" "→ KeyList" "KeyList画面へ遷移"

since=$(checkpoint)
ui tap --resource-id generateKeyFab
ui type --resource-id generateKeyLabelField --value "$KEY_LABEL"
ui tap --resource-id generateKeyConfirmButton
assert_since "$since" "generated ed25519 key pair" "ed25519鍵ペア生成"
assert_since "$since" "generated key saved id=[0-9]+ '${KEY_LABEL}'" "鍵をDBに保存"

PUBKEY="$(ui get-prefix --prefix "ssh-ed25519 ")"
echo "  pubkey: $PUBKEY"
MARKER="isekai-terminal-e2e-test-${TS}"
mkdir -p ~/.ssh
echo "${PUBKEY} ${MARKER}" >> ~/.ssh/authorized_keys
chmod 600 ~/.ssh/authorized_keys
echo "  appended to ~/.ssh/authorized_keys (marker=${MARKER})"

ui tap --resource-id dismissGeneratedKeyButton
since=$(checkpoint)
ui tap --resource-id keyListBackButton
assert_since "$since" "→ ProfileList" "ProfileListへ戻る"

echo ""
echo "--- 3. プロファイル追加 ---"
since=$(checkpoint)
ui tap --resource-id addProfileFab
assert_since "$since" "→ ProfileEdit\(new\)" "ProfileEdit(new)画面へ遷移"

ui type --resource-id profileLabelField --value "$PROFILE_LABEL"
ui type --resource-id profileHostField --value "$SSH_HOST"
ui type --resource-id profilePortField --value "$SSH_PORT"
ui type --resource-id profileUsernameField --value "$SSH_USER"
# ソフトキーボードが開いたままだと下のフィールドを覆い隠しタップが誤爆するため
# (実機で確認済み)、テキスト入力後は明示的に閉じる。
adbd shell input keyevent KEYCODE_BACK
sleep 0.3
ui tap --resource-id authTypeKeyChip
# ExposedDropdownMenuBox の読み取り専用フィールドは見た目上は全体がタップ可能に
# 見えるが、実際に展開に反応する領域が末尾のドロップダウン矢印アイコン付近に
# 偏っていて中央タップでは開かないことを実機で確認した。右寄り(--x-bias)を狙う。
ui tap --resource-id profileKeyDropdownField --x-bias 0.9
ui tap --text "$KEY_LABEL"
ui scroll-to --resource-id profileSaveButton

since=$(checkpoint)
ui tap --resource-id profileSaveButton
assert_since "$since" "saving profile: label='${PROFILE_LABEL}'.*authType=key" "プロファイル保存(鍵認証)"
assert_since "$since" "→ ProfileList" "ProfileListへ戻る"
assert_since "$since" "loaded [0-9]+ profile" "プロファイル一覧再読み込み"

echo ""
echo "--- 4. SSH接続(鍵認証) ---"
since=$(checkpoint)
ui tap --text "$PROFILE_LABEL"
assert_since "$since" "tap .* key connect: '${PROFILE_LABEL}'" "鍵認証接続タップ"
assert_since "$since" "ProfileList → Terminal profile='${PROFILE_LABEL}' authType=key" "タブ接続開始"
assert_since "$since" "→ Terminal \(tabs=" "Terminal画面へ遷移"
assert_since "$since" "connectTab\[.*\]: '${PROFILE_LABEL}'" "SSH接続試行(タブ)"
assert_since "$since" "connected: .*${SSH_HOST}" "SSH接続確立" 20

echo ""
echo "--- 5. 画面回転 ---"
# 現行アーキテクチャ(タブ化後)では TerminalTabsViewModel は Activity の
# ViewModelStore にスコープされ、回転を跨いで生存する(MainActivityのonCreateのみ
# 再実行される)。TESTING.md の「ViewModel破棄→再生成」はタブ化前の実装の記述であり、
# 現行では「回転してもタブ/接続状態が壊れないこと」を確認するのが正しい期待値になる。
adbd shell settings put system accelerometer_rotation 0
since=$(checkpoint)
adbd shell settings put system user_rotation 1
assert_since "$since" "app started" "回転(横): Activity再生成" 5
assert_absent_since "$since" "TerminalTabsViewModel cleared" "回転(横): タブ状態は破棄されない" 2
assert_absent_since "$since" "disconnected" "回転(横): 接続は切れない" 1

since=$(checkpoint)
adbd shell settings put system user_rotation 0
assert_since "$since" "app started" "回転(縦): Activity再生成" 5
assert_absent_since "$since" "TerminalTabsViewModel cleared" "回転(縦): タブ状態は破棄されない" 2
assert_absent_since "$since" "disconnected" "回転(縦): 接続は切れない" 1

echo ""
echo "--- 6. バックグラウンド移行・復帰 ---"
# TerminalSessionService に onCreate/onDestroy のログが無くなっているため(現行実装は
# super.onCreate()呼び出しのみ)、Foreground Serviceの生存確認は dumpsys で行う。
FAIL_STEP="バックグラウンド移行前: Serviceが起動している"
if ! adbd shell dumpsys activity services "$APP_ID" | grep -q "TerminalSessionService"; then
    echo "  FAIL: $FAIL_STEP"; exit 1
fi
echo "  PASS: $FAIL_STEP"
PASS_COUNT=$((PASS_COUNT + 1))

since=$(checkpoint)
adbd shell input keyevent KEYCODE_HOME
assert_absent_since "$since" "disconnected" "バックグラウンド移行中に切断されない" 2

FAIL_STEP="バックグラウンド中: Serviceが生存し続ける"
if ! adbd shell dumpsys activity services "$APP_ID" | grep -q "TerminalSessionService"; then
    echo "  FAIL: $FAIL_STEP"; exit 1
fi
echo "  PASS: $FAIL_STEP"
PASS_COUNT=$((PASS_COUNT + 1))

since=$(checkpoint)
adbd shell am start -a android.intent.action.MAIN -c android.intent.category.LAUNCHER -n "$APP_ID/.MainActivity" > /dev/null
sleep 1
assert_absent_since "$since" "disconnected" "復帰後も切断されていない" 1
FAIL_STEP="復帰: 接続済み表示に戻る"
if ! ui exists --contains "接続済み" > /dev/null 2>&1; then
    echo "  FAIL: $FAIL_STEP"; exit 1
fi
echo "  PASS: $FAIL_STEP"
PASS_COUNT=$((PASS_COUNT + 1))

echo ""
echo "--- 7. 切断 ---"
# system BACK は「切断しますか?」ダイアログを開く(TerminalScreen.kt BackHandler)が、
# 接続中は常時表示されているステータスバーの「切断」ボタンと文言が重複し、
# uiautomator dump 上でどちらを掴むか不安定だった(実機で誤タップを確認済み:
# ステータスバー側は onDisconnect() のみ呼び closeTab/onBack しないため
# ProfileList に戻らない)。タブバーの「×」(testTag=closeTabButton)は
# tabsVm.closeTab() を直接呼び、closeTab内でdisconnect()もまとめて行うため、
# これ1タップの方が確実。
since=$(checkpoint)
ui tap --resource-id closeTabButton
assert_since "$since" "closeTab id=" "タブクローズ呼び出し"
assert_since "$since" "disconnected" "切断完了"
assert_since "$since" "→ ProfileList" "ProfileListへ戻る"

if [ "$KEEP" -eq 1 ]; then
    echo ""
    echo "--- --keep 指定: 後片付けをスキップ ---"
    echo "  生成した鍵: '$KEY_LABEL' / プロファイル: '$PROFILE_LABEL' / authorized_keys marker: '$MARKER' はそのまま残します"
    exit 0
fi

echo ""
echo "--- 8. 後片付け ---"
since=$(checkpoint)
ui tap-near --anchor "$PROFILE_LABEL" --resource-id profileDeleteButton
ui tap --resource-id deleteConfirmButton
assert_since "$since" "deleted profile id=[0-9]+ '${PROFILE_LABEL}'" "プロファイル削除"

sleep 0.5
since=$(checkpoint)
ui tap --content-desc "メニュー"
ui tap --text "鍵管理"
assert_since "$since" "→ KeyList" "KeyList画面へ遷移(後片付け)"

since=$(checkpoint)
ui tap-near --anchor "$KEY_LABEL" --resource-id keyDeleteButton
ui tap --resource-id deleteConfirmButton
assert_since "$since" "deleting key id=[0-9]+ '${KEY_LABEL}'" "鍵削除"

grep -v "$MARKER" ~/.ssh/authorized_keys > ~/.ssh/authorized_keys.isekai_e2e_tmp
mv ~/.ssh/authorized_keys.isekai_e2e_tmp ~/.ssh/authorized_keys
chmod 600 ~/.ssh/authorized_keys
echo "  authorized_keys から marker=${MARKER} の行を削除しました"

echo ""
echo "=== 全ステップ完了 ==="
