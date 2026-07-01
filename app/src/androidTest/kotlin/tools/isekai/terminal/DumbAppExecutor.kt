package tools.isekai.terminal

import android.net.Uri
import tools.isekai.terminal.session.AppExecutor
import tools.isekai.terminal.session.UploadFile
import java.io.ByteArrayInputStream

/**
 * AppExecutor のテスト代替。
 * Android フレームワーク・実機・ネットワーク不要でロジックを検証できる。
 *
 * simulateNetworkLost() / simulateNetworkAvailable() でネットワーク変化を任意に発火できる。
 */
class DumbAppExecutor : AppExecutor {
    var serviceRunCount = 0
    val connectedHosts = mutableListOf<String>()
    var disconnectedCount = 0
    var released = false
    /** loadKeyPem() が返す値。テストで上書きして使う。 */
    var keyPem: ByteArray = ByteArray(0)
    /** null 以外をセットすると loadKeyPem() がその例外を投げる。 */
    var keyPemError: Throwable? = null

    private var _onAvailable: (() -> Unit)? = null
    private var _onLost: (() -> Unit)? = null

    override fun ensureServiceRunning() { serviceRunCount++ }
    override fun notifyConnected(host: String) { connectedHosts.add(host) }
    override fun notifyDisconnected() { disconnectedCount++ }

    override fun registerNetworkCallbacks(onAvailable: () -> Unit, onLost: () -> Unit) {
        _onAvailable = onAvailable
        _onLost = onLost
    }
    override fun unregisterNetworkCallbacks() {
        _onAvailable = null
        _onLost = null
    }

    override suspend fun loadKeyPem(keyId: Long): ByteArray {
        keyPemError?.let { throw it }
        return keyPem
    }
    override suspend fun openUploadFile(uri: Uri): UploadFile =
        UploadFile(uri.lastPathSegment ?: "fake", 0L, ByteArrayInputStream(ByteArray(0)))
    override suspend fun saveDownloadFile(fileName: String, data: ByteArray) {}
    override fun release() { released = true }

    /** ネットワーク切断をシミュレートする。 */
    fun simulateNetworkLost() = _onLost?.invoke()
    /** ネットワーク復帰をシミュレートする。 */
    fun simulateNetworkAvailable() = _onAvailable?.invoke()
}
