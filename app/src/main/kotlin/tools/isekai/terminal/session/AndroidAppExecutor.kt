package tools.isekai.terminal.session

import android.app.Application
import android.content.ComponentName
import android.content.Context
import android.content.Intent
import android.content.ServiceConnection
import android.net.ConnectivityManager
import android.net.Network
import android.net.NetworkCapabilities
import android.net.NetworkRequest
import android.os.IBinder
import tools.isekai.terminal.TerminalSessionService
import tools.isekai.terminal.data.Repositories
import tools.isekai.terminal.KeystoreKek
import tools.isekai.terminal.util.RemoteLogger
import android.content.ContentValues
import android.net.Uri
import android.os.Build
import android.os.Environment
import android.provider.MediaStore
import android.provider.OpenableColumns
import java.io.File
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext

/** AppExecutor の本番実装。Android システム API を直接呼び出す。 */
class AndroidAppExecutor(private val app: Application) : AppExecutor {

    @Volatile private var isServiceBound = false
    @Volatile private var terminalService: TerminalSessionService? = null
    private var networkCallback: ConnectivityManager.NetworkCallback? = null

    private val serviceConnection = object : ServiceConnection {
        override fun onServiceConnected(name: ComponentName, binder: IBinder) {
            terminalService = (binder as TerminalSessionService.SessionBinder).getService()
            RemoteLogger.i("TsshVM", "service bound OK")
        }
        override fun onServiceDisconnected(name: ComponentName) {
            RemoteLogger.w("TsshVM", "service disconnected unexpectedly")
            terminalService = null
        }
    }

    init {
        // サービスがすでに起動済みなら初回バインドを試みる (起動はしない)
        isServiceBound = app.bindService(
            Intent(app, TerminalSessionService::class.java), serviceConnection, 0
        )
        RemoteLogger.i("TsshVM", "AndroidAppExecutor init (serviceBound=$isServiceBound)")
    }

    override fun ensureServiceRunning() {
        app.startService(Intent(app, TerminalSessionService::class.java))
        if (!isServiceBound) {
            isServiceBound = app.bindService(
                Intent(app, TerminalSessionService::class.java),
                serviceConnection,
                Context.BIND_AUTO_CREATE,
            )
        }
    }

    override fun notifyConnected(host: String) {
        terminalService?.notifyConnected(host)
    }

    override fun notifyDisconnected() {
        terminalService?.notifyDisconnected()
    }

    override fun registerNetworkCallbacks(onAvailable: () -> Unit, onLost: () -> Unit) {
        val cb = object : ConnectivityManager.NetworkCallback() {
            override fun onAvailable(network: Network) = onAvailable()
            override fun onLost(network: Network) = onLost()
        }
        networkCallback = cb
        app.getSystemService(ConnectivityManager::class.java).registerNetworkCallback(
            NetworkRequest.Builder()
                .addCapability(NetworkCapabilities.NET_CAPABILITY_INTERNET)
                .build(),
            cb,
        )
    }

    override fun unregisterNetworkCallbacks() {
        networkCallback?.let {
            try {
                app.getSystemService(ConnectivityManager::class.java).unregisterNetworkCallback(it)
            } catch (_: Exception) {}
            networkCallback = null
        }
    }

    override suspend fun loadKeyPem(keyId: Long): ByteArray {
        val keyEntry = Repositories.keys.findById(keyId)
            ?: error("鍵が見つかりません (id=$keyId)")
        RemoteLogger.i("TsshSSH", "decrypting key '${keyEntry.label}'")
        val encBytes = File(keyEntry.encryptedPrivateKeyPath).readBytes()
        return KeystoreKek.decrypt(encBytes)
    }

    override suspend fun openUploadFile(uri: Uri): UploadFile? {
        val cr = app.contentResolver
        var name = uri.lastPathSegment ?: "file"
        var size = 0L
        cr.query(uri, null, null, null, null)?.use { cursor ->
            val nameIdx = cursor.getColumnIndex(OpenableColumns.DISPLAY_NAME)
            val sizeIdx = cursor.getColumnIndex(OpenableColumns.SIZE)
            if (cursor.moveToFirst()) {
                if (nameIdx >= 0) name = cursor.getString(nameIdx) ?: name
                if (sizeIdx >= 0) size = cursor.getLong(sizeIdx)
            }
        }
        val stream = cr.openInputStream(uri) ?: return null
        return UploadFile(name, size, stream)
    }

    override fun release() {
        if (isServiceBound) {
            try { app.unbindService(serviceConnection) } catch (_: Exception) {}
            isServiceBound = false
        }
    }

    override suspend fun saveDownloadFile(fileName: String, data: ByteArray) {
        withContext(Dispatchers.IO) {
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
                val values = ContentValues().apply {
                    put(MediaStore.Downloads.DISPLAY_NAME, fileName)
                    put(MediaStore.Downloads.IS_PENDING, 1)
                }
                val resolver = app.contentResolver
                val uri = resolver.insert(MediaStore.Downloads.EXTERNAL_CONTENT_URI, values)
                uri?.let {
                    resolver.openOutputStream(it)?.use { out -> out.write(data) }
                    values.clear()
                    values.put(MediaStore.Downloads.IS_PENDING, 0)
                    resolver.update(it, values, null, null)
                }
            } else {
                val dir = Environment.getExternalStoragePublicDirectory(
                    Environment.DIRECTORY_DOWNLOADS
                )
                dir.mkdirs()
                File(dir, fileName).writeBytes(data)
            }
        }
    }
}
