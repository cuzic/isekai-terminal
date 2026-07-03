package tools.isekai.terminal

import android.net.Uri
import tools.isekai.terminal.session.AppExecutor
import tools.isekai.terminal.session.PhysicalMultipathFds
import tools.isekai.terminal.session.UploadFile
import java.io.ByteArrayInputStream

class DumbAppExecutor : AppExecutor {
    var serviceRunCount = 0
    val connectedHosts = mutableListOf<String>()
    var disconnectedCount = 0
    var released = false
    var keyPem: ByteArray = ByteArray(0)
    var keyPemError: Throwable? = null

    /** updateSessionsSummary() に渡された最後の (connectedCount, totalCount)。 */
    var lastSessionsSummary: Pair<Int, Int>? = null
    /** updateSessionsSummary(0, 0)（＝FGS が止まってよいタイミング）が呼ばれた回数。 */
    var serviceStoppedCount = 0

    private var _onAvailable: (() -> Unit)? = null
    private var _onLost: (() -> Unit)? = null

    override fun ensureServiceRunning() { serviceRunCount++ }
    override fun notifyConnected(host: String) { connectedHosts.add(host) }
    override fun notifyDisconnected() { disconnectedCount++ }

    override fun updateSessionsSummary(connectedCount: Int, totalCount: Int) {
        lastSessionsSummary = connectedCount to totalCount
        if (totalCount <= 0) serviceStoppedCount++
    }

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

    /** acquirePhysicalMultipathFds() が返す値。テストで上書きして使う。既定は全滅（未取得）。 */
    var physicalMultipathFds: PhysicalMultipathFds = PhysicalMultipathFds()
    var acquirePhysicalMultipathFdsCallCount = 0
    var releasePhysicalMultipathFdsCalled = false

    override suspend fun acquirePhysicalMultipathFds(): PhysicalMultipathFds {
        acquirePhysicalMultipathFdsCallCount++
        return physicalMultipathFds
    }
    override fun releasePhysicalMultipathFds() { releasePhysicalMultipathFdsCalled = true }

    /** cellularFdForUpstreamFailover が返す値。テストで上書きして使う。既定はnull（未取得）。 */
    var cellularFdForUpstreamFailover: Pair<Int, String>? = null
    var registerUpstreamFailoverMonitorCallCount = 0
    var unregisterUpstreamFailoverMonitorCalled = false
    private var _onWifiUpstreamBroken: (() -> Unit)? = null

    override fun registerUpstreamFailoverMonitor(onWifiUpstreamBroken: () -> Unit) {
        registerUpstreamFailoverMonitorCallCount++
        _onWifiUpstreamBroken = onWifiUpstreamBroken
    }
    override fun unregisterUpstreamFailoverMonitor() {
        unregisterUpstreamFailoverMonitorCalled = true
        _onWifiUpstreamBroken = null
    }
    override suspend fun acquireCellularFd(): Pair<Int, String>? = cellularFdForUpstreamFailover

    fun simulateNetworkLost() = _onLost?.invoke()
    fun simulateNetworkAvailable() = _onAvailable?.invoke()
    fun simulateWifiUpstreamBroken() = _onWifiUpstreamBroken?.invoke()
}
