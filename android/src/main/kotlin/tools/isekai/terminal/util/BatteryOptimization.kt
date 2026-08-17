package tools.isekai.terminal.util

import android.content.ActivityNotFoundException
import android.content.Context
import android.content.Intent
import android.net.Uri
import android.os.PowerManager
import android.provider.Settings

/**
 * 項目2(OEMバッテリー最適化への案内UI)で使う標準APIのラッパー。
 *
 * `REQUEST_IGNORE_BATTERY_OPTIMIZATIONS`権限は意図的に追加しない(Playの
 * acceptable use casesにSSHクライアントは該当せず、誤用はアプリ停止措置の対象に
 * なりうるため、`PLAN.md`参照)。この権限があれば`ACTION_REQUEST_IGNORE_BATTERY_OPTIMIZATIONS`
 * で「自アプリへの直接確認ダイアログ」を出せるが、それは使わず、
 * [openIgnoreBatteryOptimizationSettings]はOS標準の一覧設定画面を開くだけに留まる
 * (ユーザーが自分でisekai-terminalを探してトグルする)。
 */
object BatteryOptimization {

    /** `PowerManager.isIgnoringBatteryOptimizations()`。権限不要。 */
    fun isIgnoringBatteryOptimizations(context: Context): Boolean {
        val powerManager = context.getSystemService(PowerManager::class.java) ?: return false
        return powerManager.isIgnoringBatteryOptimizations(context.packageName)
    }

    /**
     * OS標準のバッテリー最適化一覧設定画面(`Settings.ACTION_IGNORE_BATTERY_OPTIMIZATION_SETTINGS`)
     * を開く。権限不要。一部OEMのカスタムROMではこのIntentが処理されないことがあるため、
     * その場合はアプリ詳細設定画面へフォールバックする。
     */
    fun openIgnoreBatteryOptimizationSettings(context: Context) {
        try {
            context.startActivity(Intent(Settings.ACTION_IGNORE_BATTERY_OPTIMIZATION_SETTINGS))
            return
        } catch (e: ActivityNotFoundException) {
            RemoteLogger.w("IsekaiTerminalBattery", "ACTION_IGNORE_BATTERY_OPTIMIZATION_SETTINGS unavailable, falling back")
        }
        try {
            context.startActivity(
                Intent(Settings.ACTION_APPLICATION_DETAILS_SETTINGS)
                    .setData(Uri.parse("package:${context.packageName}")),
            )
        } catch (e: ActivityNotFoundException) {
            RemoteLogger.w("IsekaiTerminalBattery", "no activity found to handle battery/app settings intent")
        }
    }
}
