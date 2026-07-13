package tools.isekai.terminal

import tools.isekai.terminal.session.HostKeyChecker
import tools.isekai.terminal.session.HostKeyDecision
import uniffi.isekai_terminal_core.*

/**
 * テスト用フェイク SessionOrchestrator。
 * Rust/ネイティブを一切呼ばず、コールバックを直接発火できる。
 */
class FakeOrchestrator : SessionOrchestratorInterface {
    var callback: OrchestratorCallback? = null

    var connectCalled = false
    var connectQuicCalled = false
    var connectIsekaiPipeQuicCalled = false
    var connectIsekaiPipeQuicAutoCalled = false
    var connectMultipathIsekaiPipeQuicCalled = false
    var connectIsekaiStunP2pCalled = false
    var connectIsekaiLinkRelayCalled = false
    var disconnectCalled = false
    private var quic = false

    // 実 Rust 側の ConnPhase を模した最小限の状態。notifyNetworkLost() の
    // 判断（切断する/無視する）を Rust 側の実装に合わせてここで再現する。
    private enum class Phase { IDLE, CONNECTING, CONNECTED }
    private var phase = Phase.IDLE
    val sentBytes = mutableListOf<ByteArray>()
    var lastResizeCols: UInt? = null
    var lastResizeRows: UInt? = null
    var trzszAcceptDownloadCount = 0
    var trzszAcceptUploadCount = 0
    var trzszCancelCount = 0
    var trzszDismissCalled = false
    var rebindToFdCalls = mutableListOf<Pair<Int, String>>()
    var forceReturnToWifiCallCount = 0
    var cancelReconnectCalled = false

    @Throws(SshException::class)
    override fun connect(config: SshConfig) {
        connectCalled = true
        phase = Phase.CONNECTING
        callback!!.onConnectionStateChanged(ConnectionPublicState.Connecting)
    }

    @Throws(SshException::class)
    override fun connectQuic(config: QuicConfig) {
        connectQuicCalled = true
        quic = true
        phase = Phase.CONNECTING
        callback!!.onConnectionStateChanged(ConnectionPublicState.Connecting)
    }

    @Throws(SshException::class)
    override fun connectIsekaiPipeQuic(config: IsekaiPipeQuicConfig) {
        connectIsekaiPipeQuicCalled = true
        quic = true
        phase = Phase.CONNECTING
        callback!!.onConnectionStateChanged(ConnectionPublicState.Connecting)
    }

    @Throws(SshException::class)
    override fun connectIsekaiPipeQuicAuto(config: IsekaiPipeQuicConfig) {
        connectIsekaiPipeQuicAutoCalled = true
        quic = true
        phase = Phase.CONNECTING
        callback!!.onConnectionStateChanged(ConnectionPublicState.Connecting)
    }

    @Throws(SshException::class)
    override fun connectMultipathIsekaiPipeQuic(config: MultipathIsekaiPipeQuicConfig) {
        connectMultipathIsekaiPipeQuicCalled = true
        quic = true
        phase = Phase.CONNECTING
        callback!!.onConnectionStateChanged(ConnectionPublicState.Connecting)
    }

    @Throws(SshException::class)
    override fun connectIsekaiStunP2p(config: IsekaiStunP2pConfig) {
        connectIsekaiStunP2pCalled = true
        quic = true
        phase = Phase.CONNECTING
        callback!!.onConnectionStateChanged(ConnectionPublicState.Connecting)
    }

    @Throws(SshException::class)
    override fun connectIsekaiLinkRelay(config: IsekaiLinkRelayConfig) {
        connectIsekaiLinkRelayCalled = true
        quic = true
        phase = Phase.CONNECTING
        callback!!.onConnectionStateChanged(ConnectionPublicState.Connecting)
    }

    override fun disconnect() { disconnectCalled = true }
    override fun cancelReconnect() { cancelReconnectCalled = true }
    override fun send(data: ByteArray) { sentBytes.add(data) }
    override fun resize(cols: UInt, rows: UInt) { lastResizeCols = cols; lastResizeRows = rows }
    override fun scrollbackLen(): UInt = 0u
    override fun scrollbackCells(offset: UInt, rows: UInt): List<CellData> = emptyList()
    override fun trzszAcceptDownload() { trzszAcceptDownloadCount++ }
    override fun trzszAcceptUpload(fileName: String, fileSize: ULong, mode: UInt) { trzszAcceptUploadCount++ }
    override fun trzszSendChunk(data: ByteArray, isLast: Boolean) {}
    override fun trzszCancel() { trzszCancelCount++ }
    override fun notifyError(message: String) {}
    override fun rebindToFd(fd: Int, localIp: String) { rebindToFdCalls.add(fd to localIp) }
    override fun forceReturnToWifi() { forceReturnToWifiCallCount++ }

    override fun isQuic(): Boolean = quic

    // 実 Rust 側 (SessionOrchestrator::notify_network_path_changed) の判断を再現する:
    // ハンドシェイク中/プレーン TCP 接続中は切断、QUIC 接続中は無視。実装側はプレーン TCP
    // 接続中のみ 400ms debounce するが、この Fake が検証したいのはタブへの fanout など
    // Kotlin 側の配線であって debounce のタイミング自体(Rust 側で別途ユニットテスト済み)
    // ではないため、ここでは同期的に「最終的に切断されるかどうか」だけを再現する。
    // isSatisfied=true は切断判断には寄与しないが、呼び出し自体がこのペインまで届いたことは
    // notifyNetworkPathChangedCalls で検証できるようにする。
    val notifyNetworkPathChangedCalls = mutableListOf<Boolean>()
    override fun notifyNetworkPathChanged(isSatisfied: Boolean) {
        notifyNetworkPathChangedCalls.add(isSatisfied)
        if (isSatisfied) return
        when {
            phase == Phase.CONNECTING || (phase == Phase.CONNECTED && !quic) -> {
                disconnectCalled = true
                phase = Phase.IDLE
                callback!!.onConnectionStateChanged(ConnectionPublicState.Disconnected("network lost", null))
            }
            else -> {}
        }
    }

    val addedForwards = mutableListOf<PortForward>()
    var removedForwardId: String? = null

    override fun addLocalForward(id: String, bindAddress: String, bindPort: UShort, remoteHost: String, remotePort: UShort) {
        addedForwards.add(PortForward(ForwardType.LOCAL, bindAddress, bindPort, remoteHost, remotePort))
    }

    override fun removeForward(id: String) { removedForwardId = id }

    val setSessionThemeCalls = mutableListOf<Triple<List<UInt>, UInt, UInt>>()
    override fun setSessionTheme(ansi16: List<UInt>, defaultFg: UInt, defaultBg: UInt) {
        setSessionThemeCalls.add(Triple(ansi16, defaultFg, defaultBg))
    }


    // trzszDismiss() fires Idle synchronously, matching real Rust behavior
    override fun trzszDismiss() {
        trzszDismissCalled = true
        callback!!.onTrzszStateChanged(TrzszPublicState.Idle)
    }

    // ── Simulation helpers ───────────────────────────────────────────

    fun simulateConnected(host: String = "test.host"): Unit {
        phase = Phase.CONNECTED
        callback!!.onConnectionStateChanged(ConnectionPublicState.Connected(host))
    }

    fun simulateDisconnected(reason: String? = null): Unit {
        phase = Phase.IDLE
        callback!!.onConnectionStateChanged(ConnectionPublicState.Disconnected(reason, null))
    }

    fun simulateReconnecting(elapsedSecs: UInt = 0u, timeoutSecs: UInt = 60u, reason: String? = null): Unit {
        phase = Phase.IDLE
        callback!!.onConnectionStateChanged(ConnectionPublicState.Reconnecting(elapsedSecs, timeoutSecs, reason))
    }

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

    fun simulateAgentSignRequest(fingerprint: String = "SHA256:test-fingerprint"): Boolean =
        callback!!.onAgentSignRequest(fingerprint)
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
