package tools.isekai.terminal.session

import tools.isekai.terminal.HostKeyChangedWarning
import tools.isekai.terminal.TerminalUiState
import tools.isekai.terminal.TrzszUiState
import tools.isekai.terminal.util.RemoteLogger
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.update
import kotlinx.coroutines.launch
import uniffi.tssh_core.*
import java.util.concurrent.atomic.AtomicBoolean

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

    private val _state = MutableStateFlow(TerminalUiState())
    val state: StateFlow<TerminalUiState> = _state.asStateFlow()

    private val _log = MutableStateFlow("")
    val log: StateFlow<String> = _log.asStateFlow()

    private val _pendingDownloadFile = MutableStateFlow<Pair<String, ByteArray>?>(null)
    val pendingDownloadFile: StateFlow<Pair<String, ByteArray>?> = _pendingDownloadFile.asStateFlow()

    private val ioScope = CoroutineScope(SupervisorJob() + Dispatchers.IO)
    private val screenUpdateChannel = Channel<ScreenUpdate>(Channel.CONFLATED)

    private val transferAccepted = AtomicBoolean(false)

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

    fun send(bytes: ByteArray) = orchestrator.send(bytes)
    fun resize(cols: UInt, rows: UInt) = orchestrator.resize(cols, rows)

    fun disconnect() {
        _state.update { it.copy(connected = false, isConnecting = false, statusMsg = "切断済み") }
        orchestrator.disconnect()
    }

    fun scrollbackCells(offset: Int, rows: Int): List<CellData>? =
        orchestrator.scrollbackCells(offset.toUInt(), rows.toUInt())

    // ── Network ───────────────────────────────────────────────────────

    fun notifyNetworkLost() {
        val s = _state.value
        when {
            s.isConnecting -> {
                RemoteLogger.w("TsshSSH", "network lost during handshake — aborting")
                _state.update { it.copy(connected = false, isConnecting = false, statusMsg = "切断済み") }
                orchestrator.disconnect()
            }
            s.connected && !orchestrator.isQuic() -> {
                RemoteLogger.w("TsshSSH", "network lost while connected — disconnecting TCP session")
                _state.update { it.copy(connected = false, isConnecting = false, statusMsg = "切断済み") }
                orchestrator.disconnect()
            }
            s.connected && orchestrator.isQuic() ->
                RemoteLogger.i("TsshSSH", "network lost — QUIC session, letting transport handle it")
        }
    }

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
