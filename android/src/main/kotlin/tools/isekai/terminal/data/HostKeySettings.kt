package tools.isekai.terminal.data

import android.content.Context

/**
 * 初回接続(Unknown host key)時の挙動設定。`ProfileListScreen`の配色テーマ等と同じ
 * `SharedPreferences("isekai_terminal_ui")`にグローバル設定として永続化する。
 *
 * 既定は確認あり(=自動信頼しない)。ユーザーが明示的にオプトインした場合のみ、
 * 初回接続の鍵を確認ダイアログ無しで信頼する(`ssh -o StrictHostKeyChecking=accept-new`相当)。
 */
object HostKeySettings {
    private const val PREFS_NAME = "isekai_terminal_ui"
    private const val KEY_AUTO_TRUST_NEW = "auto_trust_new_host_keys"

    fun isAutoTrustNewHostKeysEnabled(context: Context): Boolean =
        context.getSharedPreferences(PREFS_NAME, Context.MODE_PRIVATE)
            .getBoolean(KEY_AUTO_TRUST_NEW, false)

    fun setAutoTrustNewHostKeysEnabled(context: Context, enabled: Boolean) {
        context.getSharedPreferences(PREFS_NAME, Context.MODE_PRIVATE)
            .edit()
            .putBoolean(KEY_AUTO_TRUST_NEW, enabled)
            .apply()
    }
}
