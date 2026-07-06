package tools.isekai.terminal.util

import android.util.Log
import tools.isekai.terminal.BuildConfig

/**
 * android.util.Log の薄いラッパー。
 * adb logcat で拾うためのタグ付きログ出力を提供する。
 *
 * v/d/i はセッションメタデータ(host/user/鍵フィンガープリント等)を含むことがあるため、
 * デバッグビルドでのみ出力する(release ではノーオペ)。adb/物理アクセスがあれば
 * logcat は読めるため、release でこれらを出し続けるのは不要な情報露出になる。
 * w/e は障害調査のため release でも残すが、機微値を含めないこと。
 */
object RemoteLogger {
    fun init(url: String) { if (BuildConfig.DEBUG) Log.i("RemoteLogger", "init (logcat only)") }

    fun v(tag: String, msg: String) { if (BuildConfig.DEBUG) Log.v(tag, msg) }
    fun d(tag: String, msg: String) { if (BuildConfig.DEBUG) Log.d(tag, msg) }
    fun i(tag: String, msg: String) { if (BuildConfig.DEBUG) Log.i(tag, msg) }
    fun w(tag: String, msg: String, t: Throwable? = null) {
        if (t != null) Log.w(tag, msg, t) else Log.w(tag, msg)
    }
    fun e(tag: String, msg: String, t: Throwable? = null) {
        if (t != null) Log.e(tag, msg, t) else Log.e(tag, msg)
    }
}
