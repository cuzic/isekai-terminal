package com.example.imespike.session

import com.example.imespike.TerminalUiState
import com.example.imespike.TrzszUiState
import com.example.imespike.util.RemoteLogger
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.channels.BufferOverflow
import kotlinx.coroutines.flow.MutableSharedFlow
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.update
import kotlinx.coroutines.launch
import uniffi.tssh_core.CellData
import uniffi.tssh_core.ScreenUpdate
import uniffi.tssh_core.SessionCallback
import uniffi.tssh_core.QuicConfig
import uniffi.tssh_core.SshConfig
import uniffi.tssh_core.SshException

/**
 * SSH セッションのドメインオブジェクト。Android 依存なし。
 *
 * UniFFI コールバックを [MutableSharedFlow]<[SessionEvent]> に変換し、
 * [TerminalReducer.reduce] で [TerminalUiState] に畳み込む。
 * 独立した [CoroutineScope] を内包 — [close] で破棄。
 */
class TerminalSession(
    private val gateway: SshGateway,
    private val hostKeyChecker: HostKeyChecker,
) : AutoCloseable {

    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.Default)

    private val _events = MutableSharedFlow<SessionEvent>(
        extraBufferCapacity = 128,
        onBufferOverflow = BufferOverflow.DROP_OLDEST,
    )

    private val _state = MutableStateFlow(TerminalUiState())
    val state: StateFlow<TerminalUiState> = _state.asStateFlow()

    private val _log = MutableStateFlow("")
    val log: StateFlow<String> = _log.asStateFlow()

    private var downloadBuffer: java.io.ByteArrayOutputStream? = null
    private val _pendingDownloadFile = MutableStateFlow<Pair<String, ByteArray>?>(null)
    val pendingDownloadFile: StateFlow<Pair<String, ByteArray>?> = _pendingDownloadFile.asStateFlow()

    @Volatile private var activeSession: TsshSession? = null
    @Volatile private var _isQuicSession = false
    val isQuicSession: Boolean get() = _isQuicSession

    init {
        scope.launch {
            _events.collect { event ->
                when (event) {
                    is SessionEvent.Data -> appendLog(event.bytes)
                    is SessionEvent.TrzszDownloadChunk -> accumulateDownloadChunk(event)
                    else -> {}
                }
                _state.update { TerminalReducer.reduce(it, event) }
            }
        }
    }

    // ── 接続制御 ─────────────────────────────────────────────────

    fun connect(config: SshConfig) {
        if (_state.value.connected) return
        _isQuicSession = false
        _state.update { TerminalReducer.connecting(it) }

        val s = gateway.create(config)
        activeSession = s
        try {
            s.connect(buildCallback(config.host, config.port.toInt(), s))
        } catch (e: SshException) {
            activeSession = null
            _events.tryEmit(SessionEvent.Error(e.message ?: "不明なエラー"))
        }
    }

    fun connectQuic(config: QuicConfig) {
        if (_state.value.connected) return
        _isQuicSession = true
        _state.update { TerminalReducer.connecting(it) }

        val s = gateway.createQuic(config)
        activeSession = s
        try {
            s.connect(buildCallback(config.sshHost, config.sshPort.toInt(), s))
        } catch (e: SshException) {
            activeSession = null
            _events.tryEmit(SessionEvent.Error(e.message ?: "不明なエラー"))
        }
    }

    fun notifyAuthError(message: String) {
        _state.update { TerminalReducer.authError(it, message) }
    }

    fun send(bytes: ByteArray) { activeSession?.send(bytes) }

    fun resize(cols: UInt, rows: UInt) { activeSession?.resize(cols, rows) }

    fun disconnect() {
        _isQuicSession = false
        activeSession?.disconnect()
        activeSession = null
        _state.update { it.copy(connected = false, statusMsg = "切断済み", currentHost = null) }
    }

    fun scrollbackCells(offset: Int, rows: Int): List<CellData>? =
        activeSession?.scrollbackCells(offset.toUInt(), rows.toUInt())

    // ── ホスト鍵 ─────────────────────────────────────────────────

    fun trustUpdatedHostKey() {
        val w = _state.value.hostKeyChangedWarning ?: return
        scope.launch(Dispatchers.IO) {
            hostKeyChecker.trustUpdated(w.host, w.port, w.newFingerprint)
            RemoteLogger.i("TsshSSH", "user trusted updated host key for ${w.host}")
        }
        _state.update { it.copy(hostKeyChangedWarning = null) }
    }

    fun dismissHostKeyWarning() {
        _state.update { it.copy(hostKeyChangedWarning = null) }
        disconnect()
    }

    // ── trzsz ────────────────────────────────────────────────────

    fun trzszAcceptUpload(transferId: String, fileName: String, fileSize: ULong, mode: UInt) =
        activeSession?.trzszAcceptUpload(transferId, fileName, fileSize, mode)

    fun trzszSendChunk(transferId: String, data: ByteArray, isLast: Boolean) =
        activeSession?.trzszSendChunk(transferId, data, isLast)

    fun trzszAcceptDownload(transferId: String) =
        activeSession?.trzszAcceptDownload(transferId)

    fun trzszCancel(transferId: String) =
        activeSession?.trzszCancel(transferId)

    fun trzszDismiss() { _state.update { it.copy(trzszState = null) } }

    fun consumeDownloadFile() { _pendingDownloadFile.value = null }

    // ── ログ ────────────────────────────────────────────────────

    fun clearLog() { _log.value = "" }

    // ── 内部 ─────────────────────────────────────────────────────

    private fun appendLog(bytes: ByteArray) {
        val text = bytes.toString(Charsets.UTF_8)
        _log.update { current ->
            if (current.length + text.length > 200_000)
                (current + text).takeLast(180_000)
            else
                current + text
        }
    }

    private fun accumulateDownloadChunk(event: SessionEvent.TrzszDownloadChunk) {
        val buf = downloadBuffer ?: run {
            val size = (_state.value.trzszState as? TrzszUiState.InProgress)?.total?.toInt()?.coerceAtLeast(4096) ?: 65536
            java.io.ByteArrayOutputStream(size).also { downloadBuffer = it }
        }
        buf.write(event.data)
        if (event.isLast) {
            val all = buf.toByteArray()
            downloadBuffer = null
            val fname = (_state.value.trzszState as? TrzszUiState.InProgress)?.fileName ?: "download"
            _pendingDownloadFile.value = Pair(fname, all)
        }
    }

    private fun buildCallback(host: String, port: Int, s: TsshSession) = object : SessionCallback {
        override fun onConnected() {
            RemoteLogger.i("TsshSSH", "✓ connected: $host:$port")
            _events.tryEmit(SessionEvent.Connected(host))
        }

        override fun onDisconnected(reason: String?) {
            RemoteLogger.i("TsshSSH", "✗ disconnected: reason='${reason ?: "none"}'")
            activeSession = null
            _events.tryEmit(SessionEvent.Disconnected(reason))
        }

        override fun onData(data: ByteArray) {
            if (_log.value.isEmpty()) RemoteLogger.i("TsshSSH", "first data: ${data.size}B")
            _events.tryEmit(SessionEvent.Data(data))
        }

        override fun onScreenUpdate(update: ScreenUpdate) {
            val sb = s.scrollbackLen().toInt()
            _events.tryEmit(SessionEvent.ScreenUpdated(update, sb))
        }

        override fun onHostKey(fingerprint: String): Boolean {
            RemoteLogger.i("TsshSSH", "host key fingerprint: $fingerprint")
            return try {
                when (val decision = hostKeyChecker.check(host, port, fingerprint)) {
                    is HostKeyDecision.Trust -> {
                        if (decision.isNew) {
                            RemoteLogger.i("TsshSSH", "TOFU: trusted $host")
                            _events.tryEmit(SessionEvent.HostKeyTrusted(fingerprint))
                        } else {
                            RemoteLogger.i("TsshSSH", "host key OK: $host")
                        }
                        true
                    }
                    is HostKeyDecision.Changed -> {
                        RemoteLogger.w("TsshSSH", "⚠ HOST KEY CHANGED: $host")
                        _events.tryEmit(SessionEvent.HostKeyChanged(decision.warning, fingerprint))
                        false
                    }
                }
            } catch (e: Exception) {
                RemoteLogger.e("TsshSSH", "host key check error: ${e.message}", e)
                false
            }
        }

        override fun onTrzszRequest(transferId: String, mode: String, suggestedName: String?, expectedSize: ULong?) {
            RemoteLogger.i("TsshSSH", "trzsz request: mode=$mode id=$transferId")
            _events.tryEmit(SessionEvent.TrzszRequest(transferId, mode, suggestedName, expectedSize))
        }

        override fun onTrzszDownloadChunk(transferId: String, data: ByteArray, isLast: Boolean) {
            _events.tryEmit(SessionEvent.TrzszDownloadChunk(transferId, data, isLast))
        }

        override fun onTrzszProgress(transferId: String, transferred: ULong, total: ULong?) {
            _events.tryEmit(SessionEvent.TrzszProgress(transferId, transferred, total))
        }

        override fun onTrzszFinished(transferId: String, success: Boolean, message: String?) {
            RemoteLogger.i("TsshSSH", "trzsz finished: success=$success id=$transferId")
            _events.tryEmit(SessionEvent.TrzszFinished(transferId, success, message))
        }
    }

    override fun close() {
        activeSession?.disconnect()
        activeSession = null
        scope.cancel()
    }
}
