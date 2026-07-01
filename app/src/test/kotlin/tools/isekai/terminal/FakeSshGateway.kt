package tools.isekai.terminal

import tools.isekai.terminal.session.HostKeyChecker
import tools.isekai.terminal.session.HostKeyDecision
import uniffi.tssh_core.*

/**
 * テスト用フェイク SessionOrchestrator。
 * Rust/ネイティブを一切呼ばず、コールバックを直接発火できる。
 */
class FakeOrchestrator : SessionOrchestratorInterface {
    var callback: OrchestratorCallback? = null

    var connectCalled = false
    var connectQuicCalled = false
    var disconnectCalled = false
    private var quic = false
    val sentBytes = mutableListOf<ByteArray>()
    var lastResizeCols: UInt? = null
    var lastResizeRows: UInt? = null
    var trzszAcceptDownloadCount = 0
    var trzszAcceptUploadCount = 0
    var trzszCancelCount = 0
    var trzszDismissCalled = false

    @Throws(SshException::class)
    override fun connect(config: SshConfig) {
        connectCalled = true
        callback!!.onConnectionStateChanged(ConnectionPublicState.Connecting)
    }

    @Throws(SshException::class)
    override fun connectQuic(config: QuicConfig) {
        connectQuicCalled = true
        quic = true
        callback!!.onConnectionStateChanged(ConnectionPublicState.Connecting)
    }

    override fun disconnect() { disconnectCalled = true }
    override fun send(data: ByteArray) { sentBytes.add(data) }
    override fun resize(cols: UInt, rows: UInt) { lastResizeCols = cols; lastResizeRows = rows }
    override fun scrollbackLen(): UInt = 0u
    override fun scrollbackCells(offset: UInt, rows: UInt): List<CellData> = emptyList()
    override fun trzszAcceptDownload() { trzszAcceptDownloadCount++ }
    override fun trzszAcceptUpload(fileName: String, fileSize: ULong, mode: UInt) { trzszAcceptUploadCount++ }
    override fun trzszSendChunk(data: ByteArray, isLast: Boolean) {}
    override fun trzszCancel() { trzszCancelCount++ }
    override fun notifyError(message: String) {}

    override fun isQuic(): Boolean = quic

    // trzszDismiss() fires Idle synchronously, matching real Rust behavior
    override fun trzszDismiss() {
        trzszDismissCalled = true
        callback!!.onTrzszStateChanged(TrzszPublicState.Idle)
    }

    // ── Simulation helpers ───────────────────────────────────────────

    fun simulateConnected(host: String = "test.host") =
        callback!!.onConnectionStateChanged(ConnectionPublicState.Connected(host))

    fun simulateDisconnected(reason: String? = null) =
        callback!!.onConnectionStateChanged(ConnectionPublicState.Disconnected(reason))

    fun simulateError(message: String) =
        callback!!.onConnectionStateChanged(ConnectionPublicState.Error(message))

    fun simulateData(data: ByteArray) = callback!!.onData(data)

    fun simulateHostKey(host: String = "test.host", port: UShort = 22u, fingerprint: String): Boolean =
        callback!!.onHostKey(host, port, fingerprint)

    fun simulateScreenUpdate(update: ScreenUpdate) = callback!!.onScreenUpdate(update)

    fun simulateTrzszRequest(transferId: String, mode: String, suggestedName: String?, expectedSize: ULong?) =
        callback!!.onTrzszStateChanged(TrzszPublicState.WaitingUser(transferId, mode, suggestedName, expectedSize))

    fun simulateTrzszProgress(transferId: String, transferred: ULong, total: ULong?, mode: String = "download") =
        callback!!.onTrzszStateChanged(TrzszPublicState.InProgress(transferId, mode, null, transferred, total))

    fun simulateTrzszFinished(transferId: String, success: Boolean, message: String? = null) =
        callback!!.onTrzszStateChanged(TrzszPublicState.Done(transferId, success, message))

    fun simulateDownloadComplete(fileName: String?, data: ByteArray) =
        callback!!.onDownloadComplete(fileName, data)
}

/** テスト用フェイク HostKeyChecker。デフォルトは常に信頼。 */
class FakeHostKeyChecker(
    private val decision: HostKeyDecision = HostKeyDecision.Trust(isNew = false),
) : HostKeyChecker {
    val checked = mutableListOf<Triple<String, Int, String>>()
    val trusted = mutableListOf<Triple<String, Int, String>>()

    override fun check(host: String, port: Int, fingerprint: String): HostKeyDecision {
        checked.add(Triple(host, port, fingerprint))
        return decision
    }

    override fun trustUpdated(host: String, port: Int, fingerprint: String) {
        trusted.add(Triple(host, port, fingerprint))
    }
}
