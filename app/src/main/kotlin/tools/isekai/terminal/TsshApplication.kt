package tools.isekai.terminal

import android.app.Application
import tools.isekai.terminal.data.Repositories
import tools.isekai.terminal.util.RemoteLogger

class TsshApplication : Application() {
    override fun onCreate() {
        super.onCreate()
        Repositories.init(this)
        if (BuildConfig.DEBUG) RemoteLogger.init("http://127.0.0.1:9876")
        // 配色テーマ(Rust側グローバル状態)の起動時復元は MainActivity.onCreate() で行う。
        // Application は Robolectric の JVM ユニットテストでも必ず生成されるため、ここで
        // uniffi 経由の native 呼び出し(setTerminalTheme)を行うとホスト JVM 用の
        // ネイティブライブラリが無く UnsatisfiedLinkError でテストが軒並み落ちてしまう。
    }
}
