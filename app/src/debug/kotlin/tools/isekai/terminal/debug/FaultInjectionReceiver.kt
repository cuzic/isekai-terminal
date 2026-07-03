package tools.isekai.terminal.debug

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import tools.isekai.terminal.util.RemoteLogger
import uniffi.tssh_core.debugClearUdpFault
import uniffi.tssh_core.debugCutUdpFault
import uniffi.tssh_core.debugRestoreUdpFault
import uniffi.tssh_core.debugSetUdpFaultLatencyMs
import uniffi.tssh_core.debugSetUdpFaultLossPermille

/**
 * Phase 7-5 実機検証専用: `adb shell am broadcast` から isekai-helper QUIC の
 * クライアントソケットに注入するフォルト（遅延・ロス・完全断）を操作する。
 * `app/src/debug` ソースセット配下のため release ビルドには一切含まれない。
 *
 * 例（Android 8+ の implicit broadcast 制限により、action 指定だけでは manifest
 * 登録レシーバーに届かないことがあるため `-n` でコンポーネントを明示すること。
 * 実機検証で action のみでは届かないことを確認済み）:
 *   adb shell am broadcast -n tools.isekai.terminal/.debug.FaultInjectionReceiver -a tools.isekai.terminal.debug.SET_LATENCY --ei ms 300
 *   adb shell am broadcast -n tools.isekai.terminal/.debug.FaultInjectionReceiver -a tools.isekai.terminal.debug.SET_LOSS --ei permille 200
 *   adb shell am broadcast -n tools.isekai.terminal/.debug.FaultInjectionReceiver -a tools.isekai.terminal.debug.CUT
 *   adb shell am broadcast -n tools.isekai.terminal/.debug.FaultInjectionReceiver -a tools.isekai.terminal.debug.RESTORE
 *   adb shell am broadcast -n tools.isekai.terminal/.debug.FaultInjectionReceiver -a tools.isekai.terminal.debug.CLEAR
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
}

object RealFaultInjectorApi : FaultInjectorApi {
    override fun setLatencyMs(ms: UInt) = debugSetUdpFaultLatencyMs(ms)
    override fun setLossPermille(permille: UInt) = debugSetUdpFaultLossPermille(permille)
    override fun cut() = debugCutUdpFault()
    override fun restore() = debugRestoreUdpFault()
    override fun clear() = debugClearUdpFault()
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
        }
    }
}
