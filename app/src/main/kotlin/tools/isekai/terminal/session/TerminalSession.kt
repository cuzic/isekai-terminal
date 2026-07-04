package tools.isekai.terminal.session

import tools.isekai.terminal.HostKeyChangedWarning
import tools.isekai.terminal.TerminalUiState
import tools.isekai.terminal.TrzszUiState
import tools.isekai.terminal.util.RemoteLogger
import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.TimeoutCancellationException
import kotlinx.coroutines.cancel
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.flow.MutableSharedFlow
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.SharedFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asSharedFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.update
import kotlinx.coroutines.launch
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.withTimeout
import uniffi.tssh_core.*
import java.util.concurrent.atomic.AtomicBoolean
import java.util.concurrent.atomic.AtomicReference

/**
 * SSH セッションのドメインオブジェクト。
 *
 * [SessionOrchestrator] を薄くラップし、[OrchestratorCallback] でコールバックを受け取って
 * [TerminalUiState] に反映する。セッション状態の SSOT は Rust 側に持つ。
 */
class TerminalSession(
    private val hostKeyChecker: HostKeyChecker,
    orchestratorFactory: (OrchestratorCallback) -> SessionOrchestratorInterface = { createSessionOrchestrator(it) },
) : AutoCloseable {

    companion object {
        // Rust 側（agent_forward.rs の SIGN_CONFIRM_TIMEOUT）の 30 秒より短くして、
        // 先に Kotlin 側が拒否応答を確定できるようにする。
        private const val AGENT_SIGN_CONFIRM_TIMEOUT_MS = 25_000L
    }

    private val _state = MutableStateFlow(TerminalUiState())
    val state: StateFlow<TerminalUiState> = _state.asStateFlow()

    private val _log = MutableStateFlow("")
    val log: StateFlow<String> = _log.asStateFlow()

    private val _pendingDownloadFile = MutableStateFlow<Pair<String, ByteArray>?>(null)
    val pendingDownloadFile: StateFlow<Pair<String, ByteArray>?> = _pendingDownloadFile.asStateFlow()

    // 「WiFiはあるがupstreamが死んでいる」等、マルチパスtransportがQUIC自身の視点で
    // 「応答が一切返ってこない」ことを検知した際に発火する（Rust側`PathBroker`起点）。
    private val _noViablePathEvent = MutableSharedFlow<Unit>(extraBufferCapacity = 1)
    val noViablePathEvent: SharedFlow<Unit> = _noViablePathEvent.asSharedFlow()

    private val ioScope = CoroutineScope(SupervisorJob() + Dispatchers.IO)
    private val screenUpdateChannel = Channel<ScreenUpdate>(Channel.CONFLATED)

    private val transferAccepted = AtomicBoolean(false)

    // SSH agent forwarding: 署名要求ごとにユーザー確認を待つための橋渡し。
    // Rust 側の spawn_blocking スレッドから onAgentSignRequest() が同期呼び出しされるため、
    // ここで CompletableDeferred + runBlocking を使い、UI（respondAgentSignRequest 経由）から
    // 応答が来るまでそのスレッドをブロックする（RealHostKeyChecker.check() と同じ設計）。
    private val pendingAgentSignRequest = AtomicReference<CompletableDeferred<Boolean>?>(null)

    private val callback = object : OrchestratorCallback {
        override fun onConnectionStateChanged(state: ConnectionPublicState) {
            when (state) {
                ConnectionPublicState.Connecting ->
                    _state.update { it.copy(isConnecting = true, connected = false, statusMsg = "接続中…") }
                is ConnectionPublicState.Connected -> {
                    RemoteLogger.i("TsshSSH", "✓ connected: ${state.host}")
                    _state.update { it.copy(isConnecting = false, connected = true,
                        statusMsg = "接続済み: ${state.host}", currentHost = state.host) }
                }
                is ConnectionPublicState.Disconnected -> {
                    RemoteLogger.i("TsshSSH", "✗ disconnected: reason='${state.reason ?: "none"}'")
                    _state.update { it.copy(isConnecting = false, connected = false,
                        statusMsg = state.reason?.let { r -> "切断: $r" } ?: "切断済み (不明)",
                        currentHost = null, screenUpdate = null, trzszState = null) }
                }
                is ConnectionPublicState.Error -> {
                    RemoteLogger.w("TsshSSH", "connection error: ${state.message}")
                    _state.update { it.copy(isConnecting = false, statusMsg = "エラー: ${state.message}") }
                }
            }
        }

        override fun onScreenUpdate(update: ScreenUpdate) {
            if (!_state.value.connected) return
            screenUpdateChannel.trySend(update)
        }

        override fun onHostKey(host: String, port: UShort, fingerprint: String): Boolean {
            RemoteLogger.i("TsshSSH", "host key fingerprint: $fingerprint")
            return try {
                when (val decision = hostKeyChecker.check(host, port.toInt(), fingerprint)) {
                    is HostKeyDecision.Trust -> {
                        if (decision.isNew) {
                            RemoteLogger.i("TsshSSH", "TOFU: trusted $host")
                            _state.update { it.copy(lastFingerprint = fingerprint) }
                        }
                        true
                    }
                    is HostKeyDecision.Changed -> {
                        RemoteLogger.w("TsshSSH", "⚠ HOST KEY CHANGED: $host")
                        _state.update { it.copy(hostKeyChangedWarning = decision.warning) }
                        false
                    }
                    is HostKeyDecision.Reject -> {
                        RemoteLogger.w("TsshSSH", "host key rejected: ${decision.reason}")
                        false
                    }
                }
            } catch (e: Exception) {
                RemoteLogger.e("TsshSSH", "host key check error: ${e.message}", e)
                false
            }
        }

        override fun onData(data: ByteArray) { appendLog(data) }

        override fun onTrzszStateChanged(state: TrzszPublicState) {
            when (state) {
                TrzszPublicState.Idle -> {
                    transferAccepted.set(false)
                    _state.update { it.copy(trzszState = null) }
                }
                is TrzszPublicState.WaitingUser -> {
                    transferAccepted.set(false)
                    _state.update { it.copy(trzszState = TrzszUiState.WaitingUser(
                        state.transferId, state.mode, state.suggestedName, state.expectedSize)) }
                }
                is TrzszPublicState.InProgress -> {
                    _state.update { it.copy(trzszState = TrzszUiState.InProgress(
                        state.transferId, state.mode, state.fileName, state.transferred, state.total)) }
                }
                is TrzszPublicState.Done -> {
                    transferAccepted.set(false)
                    _state.update { it.copy(trzszState = TrzszUiState.Done(
                        state.transferId, state.success, state.message)) }
                }
            }
        }

        override fun onDownloadComplete(fileName: String?, data: ByteArray) {
            _pendingDownloadFile.value = Pair(fileName ?: "download", data)
        }

        override fun onNoViablePath() {
            RemoteLogger.w("TsshSSH", "no viable path (QUIC sees no response on any path)")
            _noViablePathEvent.tryEmit(Unit)
        }

        override fun onForwardStateChanged(id: String, state: ForwardState) {
            when (state) {
                is ForwardState.Listening ->
                    RemoteLogger.i("TsshSSH", "port forward '$id': listening")
                is ForwardState.Failed ->
                    RemoteLogger.w("TsshSSH", "port forward '$id': failed: ${state.reason}")
                is ForwardState.Stopped ->
                    RemoteLogger.i("TsshSSH", "port forward '$id': stopped")
            }
        }

        // SSH agent forwarding: Rust 側の spawn_blocking スレッドから同期呼び出しされる。
        // ユーザーが respondAgentSignRequest() を呼ぶまでこのスレッドをブロックして待つ。
        // タイムアウト（Rust 側の 30 秒より短い 25 秒）した場合も拒否扱いにする。
        override fun onAgentSignRequest(keyFingerprint: String): Boolean {
            RemoteLogger.i("TsshSSH", "agent sign request: $keyFingerprint")
            val deferred = CompletableDeferred<Boolean>()
            pendingAgentSignRequest.set(deferred)
            _state.update { it.copy(agentSignRequestFingerprint = keyFingerprint) }
            return try {
                runBlocking {
                    try {
                        withTimeout(AGENT_SIGN_CONFIRM_TIMEOUT_MS) { deferred.await() }
                    } catch (e: TimeoutCancellationException) {
                        RemoteLogger.w("TsshSSH", "agent sign request timed out — denying")
                        false
                    }
                }
            } finally {
                pendingAgentSignRequest.set(null)
                _state.update { it.copy(agentSignRequestFingerprint = null) }
            }
        }
    }

    private val orchestrator: SessionOrchestratorInterface = orchestratorFactory(callback)

    init {
        ioScope.launch {
            for (update in screenUpdateChannel) {
                if (_state.value.connected) {
                    _state.update { it.copy(screenUpdate = update, scrollbackLen = orchestrator.scrollbackLen().toInt()) }
                }
            }
        }
    }

    // ── Connection ───────────────────────────────────────────────────

    fun connect(config: SshConfig) {
        if (_state.value.let { it.connected || it.isConnecting }) return
        try {
            orchestrator.connect(config)
        } catch (e: SshException) {
            _state.update { it.copy(isConnecting = false, statusMsg = "エラー: ${e.message ?: "不明なエラー"}") }
        }
    }

    fun connectQuic(config: QuicConfig) {
        if (_state.value.let { it.connected || it.isConnecting }) return
        try {
            orchestrator.connectQuic(config)
        } catch (e: SshException) {
            _state.update { it.copy(isConnecting = false, statusMsg = "エラー: ${e.message ?: "不明なエラー"}") }
        }
    }

    /** Phase 7: 自作ヘルパー経由 QUIC。フォールバック無し（明示選択時）。 */
    fun connectHelperQuic(config: HelperQuicConfig) {
        if (_state.value.let { it.connected || it.isConnecting }) return
        try {
            orchestrator.connectHelperQuic(config)
        } catch (e: SshException) {
            _state.update { it.copy(isConnecting = false, statusMsg = "エラー: ${e.message ?: "不明なエラー"}") }
        }
    }

    /** Phase 7: 自作ヘルパー経由 QUIC を試し、失敗したら通常の TCP SSH にフォールバックする。 */
    fun connectHelperQuicAuto(config: HelperQuicConfig) {
        if (_state.value.let { it.connected || it.isConnecting }) return
        try {
            orchestrator.connectHelperQuicAuto(config)
        } catch (e: SshException) {
            _state.update { it.copy(isConnecting = false, statusMsg = "エラー: ${e.message ?: "不明なエラー"}") }
        }
    }

    /** Phase 9: 自作ヘルパー経由 QUIC + Tailscale⇔直接アドレスの受動的マルチパス。フォールバック無し。 */
    fun connectMultipathHelperQuic(config: MultipathHelperQuicConfig) {
        if (_state.value.let { it.connected || it.isConnecting }) return
        try {
            orchestrator.connectMultipathHelperQuic(config)
        } catch (e: SshException) {
            _state.update { it.copy(isConnecting = false, statusMsg = "エラー: ${e.message ?: "不明なエラー"}") }
        }
    }

    /** Phase 10: STUN+SSHランデブーによる直接P2P QUIC。relay無し・フォールバック無し。 */
    fun connectIsekaiStunP2p(config: IsekaiStunP2pConfig) {
        if (_state.value.let { it.connected || it.isConnecting }) return
        try {
            orchestrator.connectIsekaiStunP2p(config)
        } catch (e: SshException) {
            _state.update { it.copy(isConnecting = false, statusMsg = "エラー: ${e.message ?: "不明なエラー"}") }
        }
    }

    fun send(bytes: ByteArray) = orchestrator.send(bytes)
    fun resize(cols: UInt, rows: UInt) = orchestrator.resize(cols, rows)

    fun disconnect() {
        _state.update { it.copy(connected = false, isConnecting = false, statusMsg = "切断済み") }
        orchestrator.disconnect()
    }

    fun scrollbackCells(offset: Int, rows: Int): List<CellData>? =
        orchestrator.scrollbackCells(offset.toUInt(), rows.toUInt())

    // ── Network ───────────────────────────────────────────────────────

    /** ネットワーク断イベントをそのまま Rust 側に転送する。
     *  切断するかどうか（ハンドシェイク中/TCP接続中は切断、QUIC接続中は無視）の
     *  判断はセッション状態の SSOT を持つ Rust 側（`SessionOrchestrator::notify_network_lost`）が行う。
     *  結果は通常の `onConnectionStateChanged` コールバック経由で [_state] に反映される。 */
    fun notifyNetworkLost() = orchestrator.notifyNetworkLost()

    /** 「WiFiは繋がっているがupstreamが死んでいる」等を検知した際に呼ぶ。
     *  マルチパス以外のtransportや未接続時は Rust 側で無視される（日和見的に呼べばよい）。 */
    fun rebindToFd(fd: Int, localIp: String) = orchestrator.rebindToFd(fd, localIp)

    // ── Host key ──────────────────────────────────────────────────────

    fun trustUpdatedHostKey() {
        val w = _state.value.hostKeyChangedWarning ?: return
        _state.update { it.copy(hostKeyChangedWarning = null) }
        ioScope.launch {
            hostKeyChecker.trustUpdated(w.host, w.port, w.newFingerprint)
        }
    }

    fun dismissHostKeyWarning() {
        _state.update { it.copy(hostKeyChangedWarning = null) }
        disconnect()
    }

    // ── SSH agent forwarding ──────────────────────────────────────────

    /** ユーザーが署名確認ダイアログで承認/拒否を選んだ時に呼ぶ。応答が無ければ拒否扱い。 */
    fun respondAgentSignRequest(approved: Boolean) {
        val deferred = pendingAgentSignRequest.getAndSet(null) ?: return
        _state.update { it.copy(agentSignRequestFingerprint = null) }
        deferred.complete(approved)
    }

    // ── trzsz ─────────────────────────────────────────────────────────

    fun trzszAcceptDownload() {
        if (_state.value.trzszState !is TrzszUiState.WaitingUser) return
        if (!transferAccepted.compareAndSet(false, true)) return
        orchestrator.trzszAcceptDownload()
    }

    fun trzszAcceptUpload(fileName: String, fileSize: ULong, mode: UInt) {
        if (_state.value.trzszState !is TrzszUiState.WaitingUser) return
        if (!transferAccepted.compareAndSet(false, true)) return
        orchestrator.trzszAcceptUpload(fileName, fileSize, mode)
    }

    fun trzszSendChunk(data: ByteArray, isLast: Boolean) {
        orchestrator.trzszSendChunk(data, isLast)
    }

    fun trzszCancel() {
        if (_state.value.trzszState == null) return
        transferAccepted.set(false)
        _state.update { it.copy(trzszState = null) }
        orchestrator.trzszCancel()
    }

    fun trzszDismiss() = orchestrator.trzszDismiss()

    fun consumeDownloadFile() { _pendingDownloadFile.value = null }

    // ── Log ───────────────────────────────────────────────────────────

    fun clearLog() { _log.value = "" }

    private fun appendLog(bytes: ByteArray) {
        val text = bytes.toString(Charsets.UTF_8)
        _log.update { current ->
            if (current.length + text.length > 200_000) (current + text).takeLast(180_000)
            else current + text
        }
    }

    override fun close() {
        orchestrator.disconnect()
        screenUpdateChannel.close()
        ioScope.cancel()
    }
}
