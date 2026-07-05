package tools.isekai.terminal.util

import android.util.Log

/**
 * android.util.Log の薄いラッパー。
 * adb logcat で拾うためのタグ付きログ出力を提供する。
 */
object RemoteLogger {
    fun init(url: String) { Log.i("RemoteLogger", "init (logcat only)") }

    fun v(tag: String, msg: String) { Log.v(tag, msg) }
    fun d(tag: String, msg: String) { Log.d(tag, msg) }
    fun i(tag: String, msg: String) { Log.i(tag, msg) }
    fun w(tag: String, msg: String, t: Throwable? = null) {
        if (t != null) Log.w(tag, msg, t) else Log.w(tag, msg)
    }
    fun e(tag: String, msg: String, t: Throwable? = null) {
        if (t != null) Log.e(tag, msg, t) else Log.e(tag, msg)
    }
}
