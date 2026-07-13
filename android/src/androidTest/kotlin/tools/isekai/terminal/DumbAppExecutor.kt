package tools.isekai.terminal

import android.net.Uri
import tools.isekai.terminal.session.AppExecutor
import tools.isekai.terminal.session.PhysicalMultipathAcquisition
import tools.isekai.terminal.session.PhysicalMultipathFds
import tools.isekai.terminal.session.RebindFdSource
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
    override fun decryptRelayJwt(ciphertext: String): String = ciphertext
    override suspend fun openUploadFile(uri: Uri): UploadFile =
        UploadFile(uri.lastPathSegment ?: "fake", 0L, ByteArrayInputStream(ByteArray(0)))
    override suspend fun saveDownloadFile(fileName: String, data: ByteArray) {}
    override fun release() { released = true }

    /** [AppExecutor]が返すhandle/sourceのclose記録用フェイク。テストから`.closed`を検証する。 */
    class TestHandle(val label: String) : AutoCloseable {
        var closed = false
        override fun close() { closed = true }
    }

    /** acquirePhysicalMultipathFds() が返すfds。テストで上書きして使う。既定は全滅（未取得）。 */
    var physicalMultipathFds: PhysicalMultipathFds = PhysicalMultipathFds()
    /** acquirePhysicalMultipathFds() が発行した順の全handle。個別に`.closed`を検証できる。 */
    val physicalMultipathHandles = mutableListOf<TestHandle>()

    override suspend fun acquirePhysicalMultipathFds(): PhysicalMultipathAcquisition {
        val handle = TestHandle("physical-multipath-${physicalMultipathHandles.size}")
        physicalMultipathHandles += handle
        return PhysicalMultipathAcquisition(physicalMultipathFds, handle)
    }

    /** registerUpstreamFailoverMonitor() が発行した順の全handle。 */
    val upstreamFailoverHandles = mutableListOf<TestHandle>()
    private val onWifiUpstreamBrokenCallbacks = mutableListOf<() -> Unit>()

    override fun registerUpstreamFailoverMonitor(onWifiUpstreamBroken: () -> Unit): AutoCloseable {
        val handle = TestHandle("upstream-failover-${upstreamFailoverHandles.size}")
        upstreamFailoverHandles += handle
        onWifiUpstreamBrokenCallbacks += onWifiUpstreamBroken
        return handle
    }

    /** [RebindFdSource]のフェイク。本番実装と同じくclose後は問い合わせがnullを返す契約を再現する。 */
    class TestRebindFdSource(
        var wifiFd: Pair<Int, String>?,
        var cellularFd: Pair<Int, String>?,
    ) : RebindFdSource {
        var closed = false
        var acquireWifiFdCallCount = 0
        var acquireCellularFdCallCount = 0
        override suspend fun acquireWifiFd(): Pair<Int, String>? {
            acquireWifiFdCallCount++
            return if (closed) null else wifiFd
        }
        override suspend fun acquireCellularFd(): Pair<Int, String>? {
            acquireCellularFdCallCount++
            return if (closed) null else cellularFd
        }
        override fun close() { closed = true }
    }

    /** createRebindFdSource() が発行するsourceの既定fd値。テストで上書きして使う。既定はnull（未取得）。 */
    var wifiFdForRebind: Pair<Int, String>? = null
    var cellularFdForUpstreamFailover: Pair<Int, String>? = null
    /** createRebindFdSource() が発行した順の全source。 */
    val rebindFdSources = mutableListOf<TestRebindFdSource>()

    override fun createRebindFdSource(): RebindFdSource {
        val source = TestRebindFdSource(wifiFdForRebind, cellularFdForUpstreamFailover)
        rebindFdSources += source
        return source
    }

    /** ネットワーク切断をシミュレートする。 */
    fun simulateNetworkLost() = _onLost?.invoke()
    /** ネットワーク復帰をシミュレートする。 */
    fun simulateNetworkAvailable() = _onAvailable?.invoke()
    /** [index]番目(登録順)に登録されたupstream failover監視のコールバックを発火する。既定は先頭。 */
    fun simulateWifiUpstreamBroken(index: Int = 0) = onWifiUpstreamBrokenCallbacks.getOrNull(index)?.invoke()
}
