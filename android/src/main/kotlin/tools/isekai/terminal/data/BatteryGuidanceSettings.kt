package tools.isekai.terminal.data

import android.content.Context

/**
 * 項目2(OEMバッテリー最適化への案内UI)の永続化状態。`HostKeySettings`と同じく
 * `SharedPreferences("isekai_terminal_ui")`にグローバル設定として永続化する。
 *
 * ここに永続化される生の事実(予期しないkill回数・前回案内時刻・オプトアウトフラグ)は、
 * `rust-core/src/background_reliability_policy.rs`の`decide_battery_guidance`
 * (`BackgroundKillFacts`)へそのまま渡すためのものであり、「案内すべきか」の判断自体は
 * ここでは行わない(`.claude/rules/rust-ssot.md`準拠)。
 */
object BatteryGuidanceSettings {
    private const val PREFS_NAME = "isekai_terminal_ui"
    private const val KEY_UNEXPECTED_KILL_COUNT = "battery_guidance_unexpected_kill_count"
    private const val KEY_LAST_SHOWN_UNIX_SECS = "battery_guidance_last_shown_unix_secs"
    private const val KEY_OPTED_OUT = "battery_guidance_opted_out"

    private fun prefs(context: Context) = context.getSharedPreferences(PREFS_NAME, Context.MODE_PRIVATE)

    fun unexpectedKillCount(context: Context): Int = prefs(context).getInt(KEY_UNEXPECTED_KILL_COUNT, 0)

    /** 「新鮮なreattachレコードあり && clean-shutdownマーカー無し」で起動したときに呼ぶ。 */
    fun incrementUnexpectedKillCount(context: Context): Int {
        val next = unexpectedKillCount(context) + 1
        prefs(context).edit().putInt(KEY_UNEXPECTED_KILL_COUNT, next).apply()
        return next
    }

    /** 前回この案内ダイアログを表示した時刻(Unix epoch秒)。一度も表示したことが
     *  無ければ`null`([BackgroundKillFacts.lastShownUnixSecs]相当)。 */
    fun lastShownUnixSecs(context: Context): Long? =
        prefs(context).getLong(KEY_LAST_SHOWN_UNIX_SECS, -1L).takeIf { it >= 0L }

    fun markShownNow(context: Context, nowUnixSecs: Long) {
        prefs(context).edit().putLong(KEY_LAST_SHOWN_UNIX_SECS, nowUnixSecs).apply()
    }

    fun isOptedOut(context: Context): Boolean = prefs(context).getBoolean(KEY_OPTED_OUT, false)

    fun setOptedOut(context: Context, optedOut: Boolean) {
        prefs(context).edit().putBoolean(KEY_OPTED_OUT, optedOut).apply()
    }
}
