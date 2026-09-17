#!/usr/bin/env bash
# Phase 7-5: 実機ローミング耐性検証スクリプト。
#
# 二軸のフォルトを組み合わせて isekai-helper QUIC セッションの耐性を検証する:
#   (A) rust-core/src/faulty_udp_socket.rs 経由のライブなロス/遅延/完全断注入
#       （adb shell am broadcast → debug_fault の UniFFI 関数 → 実接続の
#       UDP ソケットに実際に影響する。debug ビルドのみ有効）
#   (B) 実機の WiFi/5G 切替（adb shell svc wifi/data enable|disable）
#
# 各シナリオ関数は独立して呼べる。実機を操作する関数を呼ぶ前に、必ず
# 呼び出し側（Claude session）が一段階ずつユーザーに確認を取ること
# （もう一方のセッションと実機を共用しているため）。このスクリプト自体は
# 確認なしに全実行するものではなく、シナリオごとに手動で叩くための
# 関数集として使う。
#
# 使い方:
#   source rust-core/scripts/phase7-5-roaming-test.sh
#   list_scenarios
#   scenario_baseline
#   scenario_live_fault_no_switch
#   ...

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RUST_CORE_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
REPO_DIR="$(cd "$RUST_CORE_DIR/.." && pwd)"

PKG="tools.isekai.terminal"
LOG_TAGS="isekai-terminal-core:V IsekaiTerminalSSH:V IsekaiTerminalVM:V FaultInjection:V ActivityManager:I ReconnectSpike:V *:S"
LOG_DIR="${LOG_DIR:-/tmp/claude-1001/-home-cuzic-isekai-terminal/1366600f-e921-4fad-93ea-f62b10133c99/scratchpad/phase7-5-logs}"
mkdir -p "$LOG_DIR"

_ts() { date +%Y%m%d-%H%M%S; }

_broadcast() {
    # Android 8+ の implicit broadcast 制限により action 指定だけでは
    # manifest 登録レシーバーに届かないことがあるため、明示的に
    # コンポーネントを指定する（実機検証で判明した必須の対応）。
    #
    # さらに FaultInjectionReceiver は android:exported="false" のため、
    # adb shell（shell UID）からの am broadcast はAMSに一切配送されない
    # （2026-09-17、実機Android 15/API 35で確認——"Enqueued broadcast ...: 0"
    # は配送先0件を意味していた）。`run-as <pkg>` でアプリ自身のUIDから
    # 実行すると同一UID扱いで配送される。ただし `am broadcast` はuser指定を
    # 省略するとUSER_CURRENT(-2)を使おうとし、これはshell UID以外だと
    # INTERACT_ACROSS_USERS(_FULL) が無く弾かれるため、`--user 0` を明示する
    # 必要がある。
    local action="$1"; shift
    adb shell run-as "$PKG" am broadcast --user 0 -n "${PKG}/.debug.FaultInjectionReceiver" -a "${PKG}.debug.${action}" "$@"
}

_start_logcat() {
    local name="$1"
    adb logcat -c
    local out="${LOG_DIR}/$(_ts)-${name}.log"
    adb logcat -v time ${LOG_TAGS} > "$out" &
    echo $! > "${LOG_DIR}/.logcat.pid"
    echo "$out"
}

_stop_logcat() {
    local pid
    pid=$(cat "${LOG_DIR}/.logcat.pid" 2>/dev/null || true)
    [ -n "${pid:-}" ] && kill "$pid" 2>/dev/null
}

list_scenarios() {
    cat <<'EOF'
--- ステップ0: 前提 ---
step0_precheck            adb接続確認・ローカル自動テスト(faulty_udp_socket)を再実行
step0_install_launch      debug APK インストール & 起動（要ユーザー事前登録: プロファイル2件・鍵インポート）
helper_force_doze         dumpsys battery unplug → deviceidle force-idle（要ユーザー確認）
helper_unforce_doze       deviceidle unforce → dumpsys battery reset（要ユーザー確認）
debug_set_reconnect_policy tick retry timeout
debug_dump_reconnect_log
debug_clear_reconnect_log

--- グループA: ライブフォルト注入のみ（ネットワーク切替なし） ---
scenario_live_latency     接続中に遅延300msを注入 → シェルの反応が遅くなるが継続することを確認
scenario_live_loss        接続中にロス20%を注入 → 再送で継続することを確認
scenario_live_cut_restore 接続中に完全断→復旧 → QUIC idle timeout 内なら自動復旧することを確認
scenario_clear_fault      フォルトを全解除（各シナリオ後に必ず実行）

--- グループB: 実ネットワーク切替のみ（フォルト注入なし） ---
scenario_wifi_to_cell     WiFi→モバイルデータ切替
scenario_cell_to_wifi     モバイルデータ→WiFi切替

--- グループC: 組み合わせ（フォルト注入 + 切替、ユーザー要望の本命） ---
scenario_degraded_then_switch   劣化(遅延+ロス)を注入した状態でWiFi→5Gに切替 → 切替後も継続するか
scenario_switch_then_cut        切替直後に完全断を挟んで復旧するか

--- グループD: 拡充シナリオ（PLAN.md Phase7-5表の追加項目、都度手動操作込み） ---
scenario_matrix_report    ここまでのログをまとめて集計・レポート出力
EOF
}

# ── ステップ0 ──────────────────────────────────────────

step0_precheck() {
    echo "== adb devices =="
    adb devices -l
    echo "== ローカル自動テスト (faulty_udp_socket, 実機不要) =="
    (cd "$RUST_CORE_DIR" && cargo test -p isekai-terminal-core faulty_udp_socket -- --nocapture)
}

step0_install_launch() {
    echo "== debug APK インストール =="
    adb install -r "$REPO_DIR/android/build/outputs/apk/debug/android-debug.apk"
    echo "== アプリ起動 =="
    adb shell am start -n "${PKG}/.MainActivity"
    echo "この後、プロファイル一覧からテスト対象プロファイルをタップして手動接続してください。"
    echo "接続完了（シェルプロンプトが出る）を確認してから各シナリオ関数を呼んでください。"
}

helper_force_doze() {
    echo "充電中判定を外し、画面をOFFにしてから Deep Doze を強制します。実行前にユーザー確認を取ってください。"
    echo "重要: 計測が終わったら必ず helper_unforce_doze を呼んでください。"
    adb shell dumpsys battery unplug
    adb shell dumpsys deviceidle enable >/dev/null
    adb shell input keyevent KEYCODE_SLEEP
    # deviceidleは画面OFF・非充電への状態遷移を観測してからACTIVEを抜けるため、
    # keyevent直後だとレースでforce-idleが弾かれうる。遷移を待つ。
    sleep 2
    local out
    out="$(adb shell dumpsys deviceidle force-idle 2>&1 || true)"
    echo "$out"
    local deep
    # `*IDLE*`のようなグロブだとIDLE_PENDING/IDLE_MAINTENANCEも通してしまう
    # (前者はまだDeep Dozeに入っていない、後者はネットワーク制限が一時解除される
    # メンテナンスウィンドウで、どちらもDoze検証としては不合格)。CRを除去した
    # 上で厳密一致にする。
    deep="$(adb shell dumpsys deviceidle get deep 2>&1 | tr -d '\r' | tail -n1)"
    echo "deviceidle deep state: $deep"
    if [ "$deep" != "IDLE" ]; then
        echo "ERROR: deep idle state is '$deep' (expected IDLE)。この状態の計測はDoze条件を満たしていません。計測を中止してください。"
        return 1
    fi
}

helper_unforce_doze() {
    echo "Deep Doze 強制を解除し、battery 状態を必ず元に戻します。"
    adb shell dumpsys deviceidle unforce
    adb shell dumpsys battery reset
}

debug_set_reconnect_policy() {
    local tick="${1:?tick secs required}"
    local retry="${2:?retry interval secs required}"
    local timeout="${3:?timeout secs required}"
    _broadcast SET_RECONNECT_POLICY --ei tick_secs "$tick" --ei retry_interval_secs "$retry" --ei timeout_secs "$timeout"
}

debug_clear_reconnect_policy() {
    _broadcast CLEAR_RECONNECT_POLICY
}

_pull_reconnect_log() {
    local remote="$1" local_out="$2"
    if adb exec-out run-as "$PKG" cat "files/$remote" > "$local_out" 2>"${local_out}.err"; then
        echo "  ${local_out} ($(wc -l < "$local_out") 行)"
    else
        echo "  ERROR: ${remote} を取得できませんでした: $(cat "${local_out}.err")"
    fi
}

debug_dump_reconnect_log() {
    local out_dir="${LOG_DIR}"
    echo "書き出し結果:"
    _pull_reconnect_log "debug-reconnect-events.log" "${out_dir}/rust-reconnect-events.log"
    _pull_reconnect_log "debug-reconnect-kotlin-events.log" "${out_dir}/kotlin-reconnect-events.log"
}

debug_clear_reconnect_log() {
    _broadcast CLEAR_RECONNECT_LOG
}

# ── グループA: ライブフォルト注入のみ ──────────────────

scenario_live_latency() {
    local log; log=$(_start_logcat "live_latency")
    echo "logcat -> $log"
    _broadcast SET_LATENCY --ei ms 300
    echo "遅延300ms注入。ターミナルで適当にコマンドを打って応答が遅いが継続することを確認してください。"
}

scenario_live_loss() {
    local log; log=$(_start_logcat "live_loss")
    echo "logcat -> $log"
    _broadcast SET_LOSS --ei permille 200
    echo "ロス20%注入。ターミナルで yes や find / 等を実行し、出力が継続することを確認してください。"
}

scenario_live_cut_restore() {
    local log; log=$(_start_logcat "live_cut_restore")
    echo "logcat -> $log"
    _broadcast CUT
    echo "完全断。$1 秒待ってから restore します（引数省略時 10 秒）"
    sleep "${1:-10}"
    _broadcast RESTORE
    echo "restore しました。復旧するか確認してください。"
}

scenario_clear_fault() {
    _broadcast CLEAR
    _stop_logcat
    echo "フォルトを解除しました。"
}

# ── グループB: 実ネットワーク切替のみ ──────────────────

scenario_wifi_to_cell() {
    local log; log=$(_start_logcat "wifi_to_cell")
    echo "logcat -> $log"
    echo "WiFi を無効化します（モバイルデータは有効のままにしてください）"
    adb shell svc wifi disable
    echo "数秒〜十数秒待ってシェルの応答が復旧するか確認してください。"
}

scenario_cell_to_wifi() {
    local log; log=$(_start_logcat "cell_to_wifi")
    echo "logcat -> $log"
    echo "WiFi を再度有効化します"
    adb shell svc wifi enable
    echo "数秒〜十数秒待ってシェルの応答が復旧するか確認してください。"
}

# ── グループC: 組み合わせ ───────────────────────────────

scenario_degraded_then_switch() {
    local log; log=$(_start_logcat "degraded_then_switch")
    echo "logcat -> $log"
    _broadcast SET_LATENCY --ei ms 300
    _broadcast SET_LOSS --ei permille 100
    echo "遅延300ms + ロス10%を注入した状態で5秒待ちます"
    sleep 5
    echo "WiFi を無効化（モバイルデータへ切替）します"
    adb shell svc wifi disable
    echo "切替後も継続するか確認してください。確認後 scenario_clear_fault と"
    echo "'adb shell svc wifi enable' を忘れずに。"
}

scenario_switch_then_cut() {
    local log; log=$(_start_logcat "switch_then_cut")
    echo "logcat -> $log"
    echo "WiFi を無効化します"
    adb shell svc wifi disable
    sleep 3
    _broadcast CUT
    echo "切替直後に完全断。$1 秒待ってから restore します（引数省略時 8 秒）"
    sleep "${1:-8}"
    _broadcast RESTORE
    echo "restore しました。復旧するか確認してください。'adb shell svc wifi enable' も忘れずに。"
}

# ── グループD: 集計 ──────────────────────────────────────

scenario_matrix_report() {
    echo "== ログ一覧 =="
    ls -la "$LOG_DIR"
    echo "各ログの Disconnected/Connected/Connecting 遷移:"
    grep -H "Disconnected\|Connected\|Connecting\|helper_quic\|FaultInjection" "$LOG_DIR"/*.log 2>/dev/null
}
