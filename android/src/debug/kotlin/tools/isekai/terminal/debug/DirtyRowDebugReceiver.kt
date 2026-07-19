package tools.isekai.terminal.debug

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import tools.isekai.terminal.ui.DirtyRowDebugFlags
import tools.isekai.terminal.util.RemoteLogger

/**
 * dirty-row(タスク#92-99)の部分再描画を無効化し、常に全画面再描画へ強制フォールバック
 * させる実機/CI用トグル(タスク#100)。dirty行の見落としは原因の分かりにくい表示バグに
 * なるため、`FaultInjectionReceiver`と同様に adb broadcast だけで新旧経路を素早く
 * 切り替えて比較できるようにする。`app/src/debug` ソースセット配下のため release
 * ビルドには一切含まれない。
 *
 * 例(Android 8+ の implicit broadcast 制限により `-n` でコンポーネントを明示すること):
 *   adb shell am broadcast -n tools.isekai.terminal/.debug.DirtyRowDebugReceiver -a tools.isekai.terminal.debug.DIRTY_ROW_FORCE_FULL --ez enabled true
 *   adb shell am broadcast -n tools.isekai.terminal/.debug.DirtyRowDebugReceiver -a tools.isekai.terminal.debug.DIRTY_ROW_FORCE_FULL --ez enabled false
 */
class DirtyRowDebugReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) {
        when (intent.action) {
            "tools.isekai.terminal.debug.DIRTY_ROW_FORCE_FULL" -> {
                val enabled = intent.getBooleanExtra("enabled", true)
                DirtyRowDebugFlags.forceFullRedraw = enabled
                RemoteLogger.i("DirtyRowDebug", "forceFullRedraw = $enabled")
            }
        }
    }
}
