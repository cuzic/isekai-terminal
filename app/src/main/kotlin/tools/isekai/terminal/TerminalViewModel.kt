package tools.isekai.terminal

import android.app.Application
import android.net.Uri
import androidx.lifecycle.AndroidViewModel
import androidx.lifecycle.viewModelScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.SharingStarted
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.combine
import kotlinx.coroutines.flow.stateIn
import kotlinx.coroutines.launch
import java.util.concurrent.atomic.AtomicBoolean
import tools.isekai.terminal.data.ConnectionProfile
import tools.isekai.terminal.data.Repositories
import tools.isekai.terminal.data.Snippet
import tools.isekai.terminal.data.toHelperQuicConfig
import tools.isekai.terminal.data.toMultipathHelperQuicConfig
import tools.isekai.terminal.data.toQuicConfig
import tools.isekai.terminal.data.toSshConfig
import tools.isekai.terminal.session.AndroidAppExecutor
import tools.isekai.terminal.session.AppExecutor
import tools.isekai.terminal.session.AuthValidation
import tools.isekai.terminal.session.AuthValidator
import tools.isekai.terminal.session.PhysicalMultipathFds
import tools.isekai.terminal.session.RealHostKeyChecker
import tools.isekai.terminal.session.TerminalSession
import tools.isekai.terminal.util.RemoteLogger
import uniffi.tssh_core.HelperQuicConfig
import uniffi.tssh_core.MultipathHelperQuicConfig
import uniffi.tssh_core.QuicConfig
import uniffi.tssh_core.SshAuth
import uniffi.tssh_core.SshConfig
import uniffi.tssh_core.TransportPreference


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

        // Connected 直後は取りこぼし防止のため少し待ってから自動実行コマンドを送る。
        private const val POST_CONNECT_DEBOUNCE_MS = 400L
    }

    // 接続前のバリデーションエラーメッセージ。接続試行開始時にクリアされる。
    // Rust コールバック由来ではない Kotlin ローカルのエラーを session.state に混入させないための分離。
    private val _preConnectError = MutableStateFlow<String?>(null)

    // 「WiFiのupstream断検知→セルラーへrebind」機能が今のセッションで有効かどうか。
    // connectProfile 時点のプロファイル設定を接続完了まで覚えておくためのフィールド。
    private var upstreamFailoverEnabledForCurrentSession = false
    private val rebindInFlight = AtomicBoolean(false)

    val uiState: StateFlow<TerminalUiState> = session.state
        .combine(_preConnectError) { sessionState, errorMsg ->
            if (errorMsg != null) sessionState.copy(statusMsg = errorMsg) else sessionState
        }
        .stateIn(viewModelScope, SharingStarted.Eagerly, TerminalUiState())
    val pendingDownloadFile: StateFlow<Pair<String, ByteArray>?> = session.pendingDownloadFile

    // ── 定型コマンド（スニペット）─────────────────────────────────
    private val _snippets = MutableStateFlow<List<Snippet>>(emptyList())
    val snippets: StateFlow<List<Snippet>> = _snippets.asStateFlow()

    // ── 接続後自動実行コマンド ────────────────────────────────────
    // セッション（＝1回の connectProfile 呼び出し）単位で1回だけ送るためのバイト列とフラグ。
    // 再接続のたびに connectProfile() 内でリセットされる。
    private var pendingPostConnectBytes: ByteArray? = null
    private val postConnectSent = AtomicBoolean(true)

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

        // Rust側（PathBroker）がQUIC自身の視点で「どのpathからも応答が無い」ことを
        // 検知した際の通知。Android OSのNET_CAPABILITY_VALIDATEDより先に、かつ
        // キャプティブポータルに限らずどんな理由の無応答でも検知できる
        // （実機に依存せずdebug_faultのCUTだけでも再現・検証済み）。
        viewModelScope.launch {
            session.noViablePathEvent.collect {
                if (upstreamFailoverEnabledForCurrentSession) onWifiUpstreamBroken()
            }
        }

        // 接続状態の変化をシステムサービス通知に反映
        viewModelScope.launch {
            var prevConnected = false
            uiState.collect { state ->
                val connected = state.connected
                if (connected && !prevConnected) {
                    executor.notifyConnected(state.currentHost ?: "")
                    if (upstreamFailoverEnabledForCurrentSession) {
                        executor.registerUpstreamFailoverMonitor { onWifiUpstreamBroken() }
                    }
                    maybeSendPostConnectCommands()
                } else if (!connected && prevConnected) {
                    executor.notifyDisconnected()
                    // Phase 9-4: 物理Wi-Fi/セルラーのネットワークリクエストを解放する
                    // （そもそも取得していなければ no-op）。無線を握りっぱなしにして
                    // バッテリーを消費しないようにする。
                    executor.releasePhysicalMultipathFds()
                    executor.unregisterUpstreamFailoverMonitor()
                    upstreamFailoverEnabledForCurrentSession = false
                }
                prevConnected = connected
            }
        }
    }

    // ── ネットワーク ─────────────────────────────────────────────

    /**
     * 「WiFiは繋がっているがupstreamが死んでいる」を検知した際の処理。
     * セルラーへの bindSocket 済み fd を取得できたら `rebindToFd` でendpointの
     * ソケットを丸ごと差し替える。取得できなければ何もしない（日和見的ポリシー）。
     * [rebindInFlight] で多重発火（capabilities変化の連続通知等）を防ぐ。
     */
    private fun onWifiUpstreamBroken() {
        if (!rebindInFlight.compareAndSet(false, true)) return
        viewModelScope.launch(Dispatchers.IO) {
            try {
                val cellular = executor.acquireCellularFd()
                if (cellular == null) {
                    RemoteLogger.w("TsshSSH", "upstream failover: cellular fd not available, staying on current path")
                    return@launch
                }
                val (fd, localIp) = cellular
                RemoteLogger.i("TsshSSH", "upstream failover: rebinding to cellular (localIp=$localIp)")
                session.rebindToFd(fd, localIp)
            } finally {
                rebindInFlight.set(false)
            }
        }
    }

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
        armPostConnectCommands(profile)
        loadSnippets(profile.id)
        RemoteLogger.i("TsshSSH", "connectProfile: '${profile.label}' ${profile.username}@${profile.host}:${profile.port} " +
            "transport=${profile.transportPreference}")
        viewModelScope.launch(Dispatchers.IO) {
            val auth = resolveAuth(profile, password) ?: return@launch
            when (profile.transportPreference) {
                TransportPreference.PLAIN_SSH -> connect(profile.toSshConfig(auth))
                TransportPreference.TSSHD_QUIC -> connectQuic(profile.toQuicConfig(auth))
                TransportPreference.ISEKAI_HELPER_QUIC -> connectHelperQuic(profile.toHelperQuicConfig(auth))
                TransportPreference.AUTO -> connectHelperQuicAuto(profile.toHelperQuicConfig(auth))
                TransportPreference.ISEKAI_HELPER_QUIC_MULTIPATH -> {
                    // Phase 9-4（実験的機能）: 有効化されていれば物理Wi-Fi/セルラーの
                    // fdも取得してから接続する。取得に失敗/未取得でも例外にはせず、
                    // path0/path1のみのマルチパスにフォールバックする（日和見的ポリシー）。
                    val physicalFds = if (profile.enablePhysicalMultipath) {
                        executor.acquirePhysicalMultipathFds()
                    } else {
                        PhysicalMultipathFds()
                    }
                    upstreamFailoverEnabledForCurrentSession = profile.enableUpstreamFailover
                    connectMultipathHelperQuic(profile.toMultipathHelperQuicConfig(auth, physicalFds))
                }
            }
        }
    }

    // ── 定型コマンド（スニペット）─────────────────────────────────

    /** [profileId] が null なら全プロファイル共通のスニペットのみ、非nullなら共通＋専用をマージして読み込む。 */
    fun loadSnippets(profileId: Long?) {
        viewModelScope.launch(Dispatchers.IO) {
            _snippets.value = Repositories.snippets.getForProfile(profileId)
        }
    }

    fun sendSnippet(snippet: Snippet) {
        RemoteLogger.i("TsshSnippet", "send snippet '${snippet.label}' id=${snippet.id}")
        send(SnippetCommands.toBytes(snippet))
    }

    // ── 接続後自動実行コマンド ────────────────────────────────────

    /** 新しい接続試行のたびに呼び、この接続で送るべきコマンド（あれば）とフラグをリセットする。 */
    private fun armPostConnectCommands(profile: ConnectionProfile) {
        val commands = profile.postConnectCommands?.takeIf { it.isNotBlank() }
        pendingPostConnectBytes = commands?.let { SnippetCommands.toBytes(it, appendNewline = true) }
        postConnectSent.set(pendingPostConnectBytes == null)
    }

    /** Connected 立ち上がりで1回だけ呼ばれる。CAS でセッション単位の二重発火を防ぐ。 */
    private fun maybeSendPostConnectCommands() {
        if (!postConnectSent.compareAndSet(false, true)) return
        val bytes = pendingPostConnectBytes ?: return
        viewModelScope.launch {
            delay(POST_CONNECT_DEBOUNCE_MS)
            RemoteLogger.i("TsshSSH", "sending post-connect commands (${bytes.size} bytes)")
            send(bytes)
        }
    }

    private fun connectQuic(config: QuicConfig) {
        executor.ensureServiceRunning()
        session.connectQuic(config)
    }

    private fun connectHelperQuic(config: HelperQuicConfig) {
        executor.ensureServiceRunning()
        session.connectHelperQuic(config)
    }

    private fun connectHelperQuicAuto(config: HelperQuicConfig) {
        executor.ensureServiceRunning()
        session.connectHelperQuicAuto(config)
    }

    private fun connectMultipathHelperQuic(config: MultipathHelperQuicConfig) {
        executor.ensureServiceRunning()
        session.connectMultipathHelperQuic(config)
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
