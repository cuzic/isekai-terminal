package tools.isekai.terminal.session

import android.app.Application
import android.content.ComponentName
import android.content.Context
import android.content.Intent
import android.content.ServiceConnection
import android.net.ConnectivityManager
import android.os.IBinder
import tools.isekai.terminal.TerminalSessionService
import tools.isekai.terminal.data.Repositories
import tools.isekai.terminal.KeystoreKek
import tools.isekai.terminal.RelayCredentialVault
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
    private var pathMonitor: NetworkPathMonitor? = null

    private val serviceConnection = object : ServiceConnection {
        override fun onServiceConnected(name: ComponentName, binder: IBinder) {
            terminalService = (binder as TerminalSessionService.SessionBinder).getService()
            RemoteLogger.i("IsekaiTerminalVM", "service bound OK")
        }
        override fun onServiceDisconnected(name: ComponentName) {
            RemoteLogger.w("IsekaiTerminalVM", "service disconnected unexpectedly")
            terminalService = null
        }
    }

    init {
        // サービスがすでに起動済みなら初回バインドを試みる (起動はしない)
        isServiceBound = app.bindService(
            Intent(app, TerminalSessionService::class.java), serviceConnection, 0
        )
        RemoteLogger.i("IsekaiTerminalVM", "AndroidAppExecutor init (serviceBound=$isServiceBound)")
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

    override fun updateSessionsSummary(connectedCount: Int, totalCount: Int) {
        terminalService?.updateSessionsSummary(connectedCount, totalCount)
    }

    override fun registerNetworkCallbacks(onAvailable: () -> Unit, onLost: () -> Unit) {
        // 単一の "internet capability があるか" ではなく、direct/Tailscale を別々に追跡し、
        // どちらか一方でも使える経路がある間は onLost を鳴らさない（PLAN.md Phase 7-7 参照）。
        val monitor = NetworkPathMonitor(app.getSystemService(ConnectivityManager::class.java))
        pathMonitor = monitor
        var wasAnyPathAvailable = false
        monitor.start { anyPathAvailable ->
            if (anyPathAvailable && !wasAnyPathAvailable) onAvailable()
            if (!anyPathAvailable && wasAnyPathAvailable) onLost()
            wasAnyPathAvailable = anyPathAvailable
        }
    }

    override fun unregisterNetworkCallbacks() {
        pathMonitor?.stop()
        pathMonitor = null
    }

    override suspend fun acquirePhysicalMultipathFds(): PhysicalMultipathAcquisition {
        val provider = PhysicalPathProvider(app)
        val fds = provider.acquire()
        return PhysicalMultipathAcquisition(fds, AutoCloseable { provider.release() })
    }

    override fun registerUpstreamFailoverMonitor(onWifiUpstreamBroken: () -> Unit): AutoCloseable {
        val monitor = UpstreamHealthMonitor(app.getSystemService(ConnectivityManager::class.java))
        monitor.start(
            onWifiUpstreamBroken = onWifiUpstreamBroken,
            onWifiUpstreamRecovered = {},
        )
        return AutoCloseable { monitor.stop() }
    }

    override fun createRebindFdSource(): RebindFdSource {
        val provider = PhysicalPathProvider(app)
        return object : RebindFdSource {
            @Volatile private var closed = false
            override suspend fun acquireWifiFd(): Pair<Int, String>? =
                if (closed) null else provider.acquireWifiOnly()
            override suspend fun acquireCellularFd(): Pair<Int, String>? =
                if (closed) null else provider.acquireCellularOnly()
            override fun close() {
                // close後にRust側のRebindManagerからの遅延到達コールバックが新規NetworkRequestを
                // 復活させないよう、release前にclosedを立てる(idempotent)。
                closed = true
                provider.release()
            }
        }
    }

    override suspend fun loadKeyPem(keyId: Long): ByteArray {
        val keyEntry = Repositories.keys.findById(keyId)
            ?: error("鍵が見つかりません (id=$keyId)")
        RemoteLogger.i("IsekaiTerminalSSH", "decrypting key '${keyEntry.label}'")
        val encBytes = File(keyEntry.encryptedPrivateKeyPath).readBytes()
        return KeystoreKek.decrypt(encBytes)
    }

    override fun decryptRelayJwt(ciphertext: String): String = RelayCredentialVault.decrypt(ciphertext)

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
        val safeName = sanitizeDownloadFileName(fileName)
        withContext(Dispatchers.IO) {
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
                val values = ContentValues().apply {
                    put(MediaStore.Downloads.DISPLAY_NAME, safeName)
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
                File(dir, safeName).writeBytes(data)
            }
        }
    }

    companion object {
        private const val MAX_FILE_NAME_LENGTH = 255

        /**
         * ダウンロード保存用のファイル名をサニタイズする。
         *
         * サーバー由来のファイル名(将来 trzsz の `suggested_name`/`file_name` を
         * 配線した場合に想定される入力)にパストラバーサル(`../..`)や絶対パスが
         * 含まれていても、ダウンロードディレクトリ外への書き込みが起きないよう
         * basename のみを抽出する。現状は呼び出し元(`TerminalSession.kt`)が常に
         * リテラル `"download"` を渡すため到達不能だが、将来配線した際の防御。
         */
        internal fun sanitizeDownloadFileName(rawName: String): String {
            // Android/Linux では `\` はパス区切りとして扱われないため、
            // File().name だけでは Windows 風のパスからの basename 抽出ができない。
            // 先に `\` を `/` に正規化してから File().name で basename を取り出す。
            val basename = File(rawName.replace('\\', '/')).name
            val sanitized = when {
                basename.isBlank() -> "download"
                basename == "." || basename == ".." -> "download"
                else -> basename
            }
            return sanitized.take(MAX_FILE_NAME_LENGTH).ifBlank { "download" }
        }
    }
}
