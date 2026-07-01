package tools.isekai.terminal

import android.app.Application
import android.net.Uri
import androidx.lifecycle.AndroidViewModel
import androidx.lifecycle.viewModelScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.SharingStarted
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.combine
import kotlinx.coroutines.flow.stateIn
import kotlinx.coroutines.launch
import java.util.concurrent.atomic.AtomicBoolean
import tools.isekai.terminal.data.ConnectionProfile
import tools.isekai.terminal.data.Repositories
import tools.isekai.terminal.data.toQuicConfig
import tools.isekai.terminal.data.toSshConfig
import tools.isekai.terminal.session.AndroidAppExecutor
import tools.isekai.terminal.session.AppExecutor
import tools.isekai.terminal.session.AuthValidation
import tools.isekai.terminal.session.AuthValidator
import tools.isekai.terminal.session.RealHostKeyChecker
import tools.isekai.terminal.session.TerminalSession
import tools.isekai.terminal.util.RemoteLogger
import uniffi.tssh_core.QuicConfig
import uniffi.tssh_core.SshAuth
import uniffi.tssh_core.SshConfig


/**
 * Android ライフサイクル専任の薄いラッパー。
 * ドメインロジックはすべて [TerminalSession] に委譲。
 *
 * Android 側の副作用 (サービス管理・ネットワーク・Keystore) は [AppExecutor] に委譲することで
 * テスト時に [DumbAppExecutor] へ差し替え可能にする。
 */
class TerminalViewModel(
    app: Application,
    internal val session: TerminalSession,
    private val executor: AppExecutor,
) : AndroidViewModel(app) {

    /** 本番用コンストラクタ。Compose の viewModel() から呼ばれる。 */
    constructor(app: Application) : this(
        app,
        createSession(app),
        AndroidAppExecutor(app),
    )

    companion object {
        private fun createSession(app: Application): TerminalSession {
            return TerminalSession(RealHostKeyChecker(Repositories.knownHosts))
        }
    }

    // 接続前のバリデーションエラーメッセージ。接続試行開始時にクリアされる。
    // Rust コールバック由来ではない Kotlin ローカルのエラーを session.state に混入させないための分離。
    private val _preConnectError = MutableStateFlow<String?>(null)

    val uiState: StateFlow<TerminalUiState> = session.state
        .combine(_preConnectError) { sessionState, errorMsg ->
            if (errorMsg != null) sessionState.copy(statusMsg = errorMsg) else sessionState
        }
        .stateIn(viewModelScope, SharingStarted.Eagerly, TerminalUiState())
    val pendingDownloadFile: StateFlow<Pair<String, ByteArray>?> = session.pendingDownloadFile

    init {
        RemoteLogger.i("TsshVM", "TerminalViewModel created")

        executor.registerNetworkCallbacks(
            onAvailable = { RemoteLogger.i("TsshSSH", "network available") },
            onLost = { onNetworkLost() },
        )

        viewModelScope.launch {
            session.pendingDownloadFile.collect { pending ->
                pending ?: return@collect
                executor.saveDownloadFile(pending.first, pending.second)
                session.consumeDownloadFile()
            }
        }

        // 接続状態の変化をシステムサービス通知に反映
        viewModelScope.launch {
            var prevConnected = false
            uiState.collect { state ->
                val connected = state.connected
                if (connected && !prevConnected) {
                    executor.notifyConnected(state.currentHost ?: "")
                } else if (!connected && prevConnected) {
                    executor.notifyDisconnected()
                }
                prevConnected = connected
            }
        }
    }

    // ── ネットワーク ─────────────────────────────────────────────

    /**
     * ネットワーク切断時の処理。Session 内部の domainState (TCP/QUIC) で判断する。
     * internal にすることでテストから直接呼べる。
     */
    internal fun onNetworkLost() {
        session.notifyNetworkLost()
    }

    // ── 接続 ─────────────────────────────────────────────────────

    fun connect(config: SshConfig) {
        RemoteLogger.i("TsshSSH", "connect: ${config.username}@${config.host}:${config.port}")
        executor.ensureServiceRunning()
        session.connect(config)
    }

    fun connectProfile(profile: ConnectionProfile, password: String? = null) {
        if (uiState.value.connected || uiState.value.isConnecting) return
        _preConnectError.value = null
        RemoteLogger.i("TsshSSH", "connectProfile: '${profile.label}' ${profile.username}@${profile.host}:${profile.port} quic=${profile.useTsshd}")
        viewModelScope.launch(Dispatchers.IO) {
            val auth = resolveAuth(profile, password) ?: return@launch
            if (profile.useTsshd) {
                connectQuic(profile.toQuicConfig(auth))
            } else {
                connect(profile.toSshConfig(auth))
            }
        }
    }

    private fun connectQuic(config: QuicConfig) {
        executor.ensureServiceRunning()
        session.connectQuic(config)
    }

    private suspend fun resolveAuth(profile: ConnectionProfile, password: String?): SshAuth? {
        return when (val v = AuthValidator.validate(profile.authType, password, profile.keyId)) {
            is AuthValidation.Error -> {
                RemoteLogger.w("TsshSSH", "auth error: ${v.statusMsg}")
                _preConnectError.value = v.statusMsg
                null
            }
            is AuthValidation.Password -> {
                RemoteLogger.i("TsshSSH", "auth: password")
                SshAuth.Password(v.value)
            }
            is AuthValidation.PublicKey -> {
                RemoteLogger.i("TsshSSH", "auth: public key id=${v.keyId}")
                loadPublicKeyAuth(v.keyId)
            }
        }
    }

    private suspend fun loadPublicKeyAuth(keyId: Long): SshAuth? =
        runCatching {
            RemoteLogger.i("TsshSSH", "decrypting key id=$keyId")
            val pem = executor.loadKeyPem(keyId)
            RemoteLogger.i("TsshSSH", "key decrypted OK (${pem.size} bytes)")
            SshAuth.PublicKey(pem)
        }.getOrElse { e ->
            RemoteLogger.e("TsshSSH", "key error: ${e.message}", e)
            _preConnectError.value = "鍵エラー: ${e.message}"
            null
        }

    // ── セッション操作（すべて session 委譲）──────────────────────

    fun send(bytes: ByteArray) = session.send(bytes)
    fun resize(cols: UInt, rows: UInt) {
        RemoteLogger.d("TsshSSH", "resize → ${cols}×${rows}")
        session.resize(cols, rows)
    }
    fun disconnect() = session.disconnect()
    fun scrollbackCells(offset: Int, rows: Int) = session.scrollbackCells(offset, rows)

    fun trustUpdatedHostKey() = session.trustUpdatedHostKey()
    fun dismissHostKeyWarning() = session.dismissHostKeyWarning()
    fun consumeDownloadFile() = session.consumeDownloadFile()

    fun getSessionLog(): String = session.log.value
    fun clearSessionLog() = session.clearLog()

    // ── trzsz（Android ファイル I/O はここで処理）────────────────

    // UI 側のガードと IO コルーチン起動の間は非アトミックなため、ダブルタップで二重起動しないよう CAS で保護（Bug 2 fix）
    private val uploadInProgress = AtomicBoolean(false)

    fun trzszStartUpload(uri: Uri) {
        if (uiState.value.trzszState !is TrzszUiState.WaitingUser) return
        if (!uploadInProgress.compareAndSet(false, true)) return
        viewModelScope.launch(Dispatchers.IO) {
            try {
                val file = executor.openUploadFile(uri) ?: return@launch
                RemoteLogger.i("TrzszUpload", "start fileName=${file.name} fileSize=${file.size}")
                session.trzszAcceptUpload(file.name, file.size.toULong(), 0u)
                file.stream.use { inp ->
                    val buf = ByteArray(64 * 1024)
                    var pending: ByteArray? = null
                    var chunkCount = 0
                    while (true) {
                        val n = inp.read(buf)
                        if (n == -1) {
                            RemoteLogger.i("TrzszUpload", "last chunk, total=$chunkCount")
                            session.trzszSendChunk(pending ?: ByteArray(0), true)
                            break
                        }
                        pending?.let { session.trzszSendChunk(it, false) }
                        pending = buf.copyOf(n)
                        chunkCount++
                    }
                }
            } catch (e: Exception) {
                RemoteLogger.e("TrzszUpload", "exception: $e")
            } finally {
                uploadInProgress.set(false)
            }
        }
    }

    fun trzszStartDownload() {
        if (uiState.value.trzszState !is TrzszUiState.WaitingUser) return
        session.trzszAcceptDownload()
    }

    fun trzszCancel() {
        session.trzszCancel()
    }

    fun trzszDismiss() = session.trzszDismiss()

    override fun onCleared() {
        super.onCleared()
        RemoteLogger.i("TsshVM", "TerminalViewModel cleared")
        session.close()
        executor.unregisterNetworkCallbacks()
        executor.release()
    }
}
