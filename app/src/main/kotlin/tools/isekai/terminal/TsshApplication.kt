package tools.isekai.terminal

import android.app.Application
import tools.isekai.terminal.data.Repositories
import tools.isekai.terminal.util.RemoteLogger

class TsshApplication : Application() {
    override fun onCreate() {
        super.onCreate()
        Repositories.init(this)
        if (BuildConfig.DEBUG) RemoteLogger.init("http://127.0.0.1:9876")
    }
}
