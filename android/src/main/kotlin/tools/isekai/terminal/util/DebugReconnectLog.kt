package tools.isekai.terminal.util

import android.content.Context
import android.util.Log
import tools.isekai.terminal.BuildConfig
import java.io.File

object DebugReconnectLog {
    private const val TAG = "ReconnectSpike"
    private const val FILE_NAME = "debug-reconnect-kotlin-events.log"

    @Volatile
    private var appContext: Context? = null

    fun init(context: Context) {
        if (BuildConfig.DEBUG) appContext = context.applicationContext
    }

    fun record(event: String) {
        if (!BuildConfig.DEBUG) return
        val context = appContext ?: return
        runCatching {
            file(context).appendText("${System.currentTimeMillis()} $event\n")
        }
    }

    fun dumpToLogcat(context: Context) {
        if (!BuildConfig.DEBUG) return
        init(context)
        val text = runCatching { file(context).readText() }.getOrDefault("")
        if (text.isBlank()) {
            Log.i(TAG, "kotlin log empty")
            return
        }
        text.lineSequence()
            .filter { it.isNotBlank() }
            .forEach { Log.i(TAG, it) }
    }

    fun clear(context: Context) {
        if (!BuildConfig.DEBUG) return
        init(context)
        runCatching { file(context).delete() }
    }

    private fun file(context: Context): File = File(context.filesDir, FILE_NAME)
}
