package tools.isekai.terminal.debug

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.util.Log
import tools.isekai.terminal.util.DebugReconnectLog
import tools.isekai.terminal.util.RemoteLogger
import uniffi.isekai_terminal_core.debugClearUdpFault
import uniffi.isekai_terminal_core.debugClearReconnectLog
import uniffi.isekai_terminal_core.debugClearReconnectPolicy
import uniffi.isekai_terminal_core.debugCutUdpFault
import uniffi.isekai_terminal_core.debugDumpReconnectLog
import uniffi.isekai_terminal_core.debugRestoreUdpFault
import uniffi.isekai_terminal_core.debugSetReconnectPolicy
import uniffi.isekai_terminal_core.debugSetUdpFaultLatencyMs
import uniffi.isekai_terminal_core.debugSetUdpFaultLossPermille

/**
 * Phase 7-5 実機検証専用: `adb shell am broadcast` から isekai-helper QUIC の
 * クライアントソケットに注入するフォルト（遅延・ロス・完全断）を操作する。
 * `app/src/debug` ソースセット配下のため release ビルドには一切含まれない。
 *
 * 例（Android 8+ の implicit broadcast 制限により、action 指定だけでは manifest
 * 登録レシーバーに届かないことがあるため `-n` でコンポーネントを明示すること。
 * さらにこのレシーバーは `android:exported="false"` のため、素の
 * `adb shell am broadcast`（shell UID）では**一切配送されない**
 * （2026-09-17、実機Android 15/API 35で確認——`am broadcast`自体は
 * `Broadcast completed: result=0`を返すが、実際には配送先0件で
 * `onReceive`が呼ばれない。ActivityManagerのログで
 * `Enqueued broadcast ...: 0`の"0"が配送先0件を意味している）。
 * `adb shell run-as <pkg> am broadcast --user 0 -n ...`で
 * アプリ自身のUIDから送る必要がある（`--user 0`が無いと
 * `am`既定のUSER_CURRENT(-2)解決に`INTERACT_ACROSS_USERS`権限が要ると
 * 弾かれる）。`rust-core/scripts/phase7-5-roaming-test.sh`の`_broadcast()`は
 * この形で実装済み、そちらを使うこと:
 *   adb shell run-as tools.isekai.terminal am broadcast --user 0 -n tools.isekai.terminal/.debug.FaultInjectionReceiver -a tools.isekai.terminal.debug.SET_LATENCY --ei ms 300
 *   adb shell run-as tools.isekai.terminal am broadcast --user 0 -n tools.isekai.terminal/.debug.FaultInjectionReceiver -a tools.isekai.terminal.debug.SET_LOSS --ei permille 200
 *   adb shell run-as tools.isekai.terminal am broadcast --user 0 -n tools.isekai.terminal/.debug.FaultInjectionReceiver -a tools.isekai.terminal.debug.CUT
 *   adb shell run-as tools.isekai.terminal am broadcast --user 0 -n tools.isekai.terminal/.debug.FaultInjectionReceiver -a tools.isekai.terminal.debug.RESTORE
 *   adb shell run-as tools.isekai.terminal am broadcast --user 0 -n tools.isekai.terminal/.debug.FaultInjectionReceiver -a tools.isekai.terminal.debug.CLEAR
 *   adb shell run-as tools.isekai.terminal am broadcast --user 0 -n tools.isekai.terminal/.debug.FaultInjectionReceiver -a tools.isekai.terminal.debug.SET_RECONNECT_POLICY --ei tick_secs 300 --ei retry_interval_secs 300 --ei timeout_secs 3600
 *   adb shell run-as tools.isekai.terminal am broadcast --user 0 -n tools.isekai.terminal/.debug.FaultInjectionReceiver -a tools.isekai.terminal.debug.CLEAR_RECONNECT_POLICY
 *   adb shell run-as tools.isekai.terminal am broadcast --user 0 -n tools.isekai.terminal/.debug.FaultInjectionReceiver -a tools.isekai.terminal.debug.DUMP_RECONNECT_LOG
 *   adb shell run-as tools.isekai.terminal am broadcast --user 0 -n tools.isekai.terminal/.debug.FaultInjectionReceiver -a tools.isekai.terminal.debug.CLEAR_RECONNECT_LOG
 */
/**
 * 実際の UDP フォルト注入 FFI 呼び出し先。native ライブラリ(Rust)に依存するため
 * 実機/エミュレータ上でしか動かない。テストでは [FaultInjectionReceiver.faultInjector]
 * を差し替えることで、intent の解釈ロジックだけを native 抜きで検証できる。
 */
interface FaultInjectorApi {
    fun setLatencyMs(ms: UInt)
    fun setLossPermille(permille: UInt)
    fun cut()
    fun restore()
    fun clear()
    fun setReconnectPolicy(tickSecs: UInt, retryIntervalSecs: UInt, timeoutSecs: UInt)
    fun clearReconnectPolicy()
    fun dumpReconnectLog(): String
    fun clearReconnectLog()
}

object RealFaultInjectorApi : FaultInjectorApi {
    override fun setLatencyMs(ms: UInt) = debugSetUdpFaultLatencyMs(ms)
    override fun setLossPermille(permille: UInt) = debugSetUdpFaultLossPermille(permille)
    override fun cut() = debugCutUdpFault()
    override fun restore() = debugRestoreUdpFault()
    override fun clear() = debugClearUdpFault()
    override fun setReconnectPolicy(tickSecs: UInt, retryIntervalSecs: UInt, timeoutSecs: UInt) =
        debugSetReconnectPolicy(tickSecs, retryIntervalSecs, timeoutSecs)
    override fun clearReconnectPolicy() = debugClearReconnectPolicy()
    override fun dumpReconnectLog(): String = debugDumpReconnectLog()
    override fun clearReconnectLog() = debugClearReconnectLog()
}

class FaultInjectionReceiver : BroadcastReceiver() {
    var faultInjector: FaultInjectorApi = RealFaultInjectorApi

    override fun onReceive(context: Context, intent: Intent) {
        when (intent.action) {
            "tools.isekai.terminal.debug.SET_LATENCY" -> {
                val ms = intent.getIntExtra("ms", 0)
                faultInjector.setLatencyMs(ms.toUInt())
                RemoteLogger.i("FaultInjection", "latency = ${ms}ms")
            }
            "tools.isekai.terminal.debug.SET_LOSS" -> {
                val permille = intent.getIntExtra("permille", 0)
                faultInjector.setLossPermille(permille.toUInt())
                RemoteLogger.i("FaultInjection", "loss = $permille‰")
            }
            "tools.isekai.terminal.debug.CUT" -> {
                faultInjector.cut()
                RemoteLogger.i("FaultInjection", "cut")
            }
            "tools.isekai.terminal.debug.RESTORE" -> {
                faultInjector.restore()
                RemoteLogger.i("FaultInjection", "restore")
            }
            "tools.isekai.terminal.debug.CLEAR" -> {
                faultInjector.clear()
                RemoteLogger.i("FaultInjection", "clear")
            }
            "tools.isekai.terminal.debug.SET_RECONNECT_POLICY" -> {
                val tickSecs = intent.getIntExtra("tick_secs", 1).coerceAtLeast(1)
                val retryIntervalSecs = intent.getIntExtra("retry_interval_secs", 1).coerceAtLeast(1)
                val timeoutSecs = intent.getIntExtra("timeout_secs", 1).coerceAtLeast(1)
                // 生きている全orchestratorへの即時反映はRust側のレジストリ
                // (debug_reconnect.rs::for_each_live_orchestrator)が行うため、
                // Kotlin側から個別のセッションを叩く必要は無い。
                faultInjector.setReconnectPolicy(tickSecs.toUInt(), retryIntervalSecs.toUInt(), timeoutSecs.toUInt())
                RemoteLogger.i(
                    "FaultInjection",
                    "reconnect policy tick=${tickSecs}s retry=${retryIntervalSecs}s timeout=${timeoutSecs}s",
                )
            }
            "tools.isekai.terminal.debug.CLEAR_RECONNECT_POLICY" -> {
                faultInjector.clearReconnectPolicy()
                RemoteLogger.i("FaultInjection", "reconnect policy cleared")
            }
            // 注: DUMP_RECONNECT_LOG/CLEAR_RECONNECT_LOGのlogcat経由の経路は手動デバッグ用の
            // 補助手段。実機スパイクでの正規の取得手順は
            // `rust-core/scripts/phase7-5-roaming-test.sh`の`debug_dump_reconnect_log`
            // (`adb exec-out run-as cat`経由、ファイルを直接取得しlogcatのリングバッファ
            // 回転の影響を受けない)を使うこと。
            "tools.isekai.terminal.debug.DUMP_RECONNECT_LOG" -> {
                DebugReconnectLog.dumpToLogcat(context)
                faultInjector.dumpReconnectLog()
                    .lineSequence()
                    .filter { it.isNotBlank() }
                    .forEach { Log.i("ReconnectSpike", it) }
                RemoteLogger.i("FaultInjection", "reconnect log dumped")
            }
            "tools.isekai.terminal.debug.CLEAR_RECONNECT_LOG" -> {
                DebugReconnectLog.clear(context)
                faultInjector.clearReconnectLog()
                RemoteLogger.i("FaultInjection", "reconnect log cleared")
            }
        }
    }
}
