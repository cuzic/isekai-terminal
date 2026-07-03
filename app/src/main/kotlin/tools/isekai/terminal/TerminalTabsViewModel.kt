package tools.isekai.terminal

import android.app.Application
import android.net.Uri
import androidx.lifecycle.AndroidViewModel
import androidx.lifecycle.viewModelScope
import java.util.UUID
import java.util.concurrent.atomic.AtomicBoolean
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.combine
import kotlinx.coroutines.flow.update
import kotlinx.coroutines.launch
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
import uniffi.tssh_core.CellData
import uniffi.tssh_core.SshAuth

/**
 * 複数タブ（複数 SSH/QUIC セッション）を横断する Activity/Application スコープの状態管理。
 *
 * 「タブ横断で1回だけ登録すればよい」責務——ネットワーク監視・ForegroundService の
 * 起動/停止・ネットワーク断の全セッションへのファンアウト——をここに集約する。
 * 個々のセッションのドメインロジック（接続状態機械・trzsz 等）は既存の [TerminalSession]
 * にそのまま委譲し、[TerminalSession] 自体は無改修で複数インスタンス生成するだけに留める
 * （Rust の [uniffi.tssh_core.SessionOrchestratorInterface] もグローバル状態を持たない設計
 * のため、UniFFI 側の変更は不要）。
 */
class TerminalTabsViewModel(
    app: Application,
    private val executor: AppExecutor,
    private val sessionFactory: () -> TerminalSession,
) : AndroidViewModel(app) {

    /** 本番用コンストラクタ。Compose の viewModel() から呼ばれる。 */
    constructor(app: Application) : this(
        app,
        AndroidAppExecutor(app),
        { TerminalSession(RealHostKeyChecker(Repositories.knownHosts)) },
    )

    /**
     * 1タブ分の状態。ドメイン状態の SSOT はあくまで [session]（ひいては Rust 側）であり、
     * ここで保持するのは接続前バリデーションエラーなど Kotlin ローカルの補助状態のみ。
     */
    class TabState internal constructor(
        val tabId: String,
        val session: TerminalSession,
        val profile: ConnectionProfile?,
        val label: String,
    ) {
        // 接続前のバリデーションエラー。session.state (Rust 由来) には混入させず合成する。
        internal val preConnectError = MutableStateFlow<String?>(null)
        // trzsz アップロードの二重起動防止 (Bug 2 と同種のガード。タブごとに独立させる)。
        internal val uploadInProgress = AtomicBoolean(false)

        /** UI が購読する合成済み状態。 */
        val uiState: Flow<TerminalUiState> = session.state.combine(preConnectError) { s, err ->
            if (err != null) s.copy(statusMsg = err) else s
        }
    }

    private val _tabs = MutableStateFlow<List<TabState>>(emptyList())
    val tabs: StateFlow<List<TabState>> = _tabs.asStateFlow()

    private val _activeTabId = MutableStateFlow<String?>(null)
    val activeTabId: StateFlow<String?> = _activeTabId.asStateFlow()

    // タブごとの監視コルーチン（通知集約の再計算・ダウンロード完了ファンアウト）。closeTab で cancel する。
    private val watchJobs = mutableMapOf<String, Job>()

    init {
        RemoteLogger.i("TsshTabsVM", "TerminalTabsViewModel created")
        executor.registerNetworkCallbacks(
            onAvailable = { RemoteLogger.i("TsshSSH", "network available") },
            onLost = { onNetworkLost() },
        )
    }

    // ── ネットワーク（全タブへファンアウト）───────────────────────────

    /** internal にすることでテストから直接呼べる。 */
    internal fun onNetworkLost() {
        _tabs.value.forEach { it.session.notifyNetworkLost() }
    }

    // ── タブのライフサイクル ────────────────────────────────────────

    /** 新しいタブを開いて接続を開始し、そのタブをアクティブにする。生成した tabId を返す。 */
    fun openTab(profile: ConnectionProfile, password: String? = null): String {
        val tabId = UUID.randomUUID().toString()
        val session = sessionFactory()
        val tab = TabState(tabId, session, profile, profile.label)

        RemoteLogger.i("TsshTabsVM", "openTab '${profile.label}' id=$tabId")
        _tabs.update { it + tab }
        _activeTabId.value = tabId

        // 複数セッションを1つの FGS が共有する。初回タブで起動、以後は通知内容の更新のみ。
        executor.ensureServiceRunning()
        watchTab(tab)
        connectTab(tab, profile, password)
        updateSessionsSummary()
        return tabId
    }

    /** タブを切断＋破棄する。最後のタブが閉じられた場合のみ FGS を停止させる。 */
    fun closeTab(tabId: String) {
        val tab = _tabs.value.find { it.tabId == tabId } ?: return
        RemoteLogger.i("TsshTabsVM", "closeTab id=$tabId")
        tab.session.disconnect()
        tab.session.close()
        watchJobs.remove(tabId)?.cancel()

        _tabs.update { list -> list.filterNot { it.tabId == tabId } }
        if (_activeTabId.value == tabId) {
            _activeTabId.value = _tabs.value.firstOrNull()?.tabId
        }
        updateSessionsSummary()
    }

    fun setActiveTab(tabId: String) {
        if (_tabs.value.any { it.tabId == tabId }) _activeTabId.value = tabId
    }

    private fun tabOrNull(tabId: String): TabState? = _tabs.value.find { it.tabId == tabId }

    /** タブ固有の監視: 通知集約の再計算と、ダウンロード完了ファイルの保存。非アクティブでも動き続ける。 */
    private fun watchTab(tab: TabState) {
        watchJobs[tab.tabId] = viewModelScope.launch {
            launch { tab.session.state.collect { updateSessionsSummary() } }
            launch {
                tab.session.pendingDownloadFile.collect { pending ->
                    pending ?: return@collect
                    executor.saveDownloadFile(pending.first, pending.second)
                    tab.session.consumeDownloadFile()
                }
            }
        }
    }

    private fun updateSessionsSummary() {
        val tabs = _tabs.value
        val connected = tabs.count { it.session.state.value.connected }
        executor.updateSessionsSummary(connected, tabs.size)
    }

    // ── 接続 ─────────────────────────────────────────────────────────

    fun reconnect(tabId: String, password: String? = null) {
        val tab = tabOrNull(tabId) ?: return
        val profile = tab.profile ?: return
        connectTab(tab, profile, password)
    }

    private fun connectTab(tab: TabState, profile: ConnectionProfile, password: String?) {
        val current = tab.session.state.value
        if (current.connected || current.isConnecting) return
        tab.preConnectError.value = null
        RemoteLogger.i(
            "TsshSSH",
            "connectTab[${tab.tabId}]: '${profile.label}' ${profile.username}@${profile.host}:${profile.port} quic=${profile.useTsshd}",
        )
        viewModelScope.launch(Dispatchers.IO) {
            val auth = resolveAuth(tab, profile, password) ?: return@launch
            if (profile.useTsshd) {
                tab.session.connectQuic(profile.toQuicConfig(auth))
            } else {
                tab.session.connect(profile.toSshConfig(auth))
            }
        }
    }

    private suspend fun resolveAuth(tab: TabState, profile: ConnectionProfile, password: String?): SshAuth? {
        return when (val v = AuthValidator.validate(profile.authType, password, profile.keyId)) {
            is AuthValidation.Error -> {
                RemoteLogger.w("TsshSSH", "auth error: ${v.statusMsg}")
                tab.preConnectError.value = v.statusMsg
                null
            }
            is AuthValidation.Password -> SshAuth.Password(v.value)
            is AuthValidation.PublicKey -> loadPublicKeyAuth(tab, v.keyId)
        }
    }

    private suspend fun loadPublicKeyAuth(tab: TabState, keyId: Long): SshAuth? =
        runCatching { SshAuth.PublicKey(executor.loadKeyPem(keyId)) }
            .getOrElse { e ->
                RemoteLogger.e("TsshSSH", "key error: ${e.message}", e)
                tab.preConnectError.value = "鍵エラー: ${e.message}"
                null
            }

    // ── セッション操作（タブ指定。すべて session への薄い委譲）──────────

    fun send(tabId: String, bytes: ByteArray) = tabOrNull(tabId)?.session?.send(bytes)

    fun resize(tabId: String, cols: UInt, rows: UInt) = tabOrNull(tabId)?.session?.resize(cols, rows)

    fun disconnect(tabId: String) = tabOrNull(tabId)?.session?.disconnect()

    fun scrollbackCells(tabId: String, offset: Int, rows: Int): List<CellData>? =
        tabOrNull(tabId)?.session?.scrollbackCells(offset, rows)

    fun trustUpdatedHostKey(tabId: String) = tabOrNull(tabId)?.session?.trustUpdatedHostKey()

    fun dismissHostKeyWarning(tabId: String) = tabOrNull(tabId)?.session?.dismissHostKeyWarning()

    fun getSessionLog(tabId: String): String = tabOrNull(tabId)?.session?.log?.value ?: ""

    fun clearSessionLog(tabId: String) = tabOrNull(tabId)?.session?.clearLog()

    // ── trzsz（Android ファイル I/O は executor 経由。タブごとに二重起動防止）───

    fun trzszStartUpload(tabId: String, uri: Uri) {
        val tab = tabOrNull(tabId) ?: return
        if (tab.session.state.value.trzszState !is TrzszUiState.WaitingUser) return
        if (!tab.uploadInProgress.compareAndSet(false, true)) return
        viewModelScope.launch(Dispatchers.IO) {
            try {
                val file = executor.openUploadFile(uri) ?: return@launch
                tab.session.trzszAcceptUpload(file.name, file.size.toULong(), 0u)
                file.stream.use { inp ->
                    val buf = ByteArray(64 * 1024)
                    var pending: ByteArray? = null
                    while (true) {
                        val n = inp.read(buf)
                        if (n == -1) {
                            tab.session.trzszSendChunk(pending ?: ByteArray(0), true)
                            break
                        }
                        pending?.let { tab.session.trzszSendChunk(it, false) }
                        pending = buf.copyOf(n)
                    }
                }
            } catch (e: Exception) {
                RemoteLogger.e("TrzszUpload", "exception: $e")
            } finally {
                tab.uploadInProgress.set(false)
            }
        }
    }

    fun trzszStartDownload(tabId: String) {
        val tab = tabOrNull(tabId) ?: return
        if (tab.session.state.value.trzszState !is TrzszUiState.WaitingUser) return
        tab.session.trzszAcceptDownload()
    }

    fun trzszCancel(tabId: String) = tabOrNull(tabId)?.session?.trzszCancel()

    fun trzszDismiss(tabId: String) = tabOrNull(tabId)?.session?.trzszDismiss()

    // ── ライフサイクル ──────────────────────────────────────────────

    override fun onCleared() {
        super.onCleared()
        RemoteLogger.i("TsshTabsVM", "TerminalTabsViewModel cleared")
        watchJobs.values.forEach { it.cancel() }
        _tabs.value.forEach { it.session.close() }
        executor.unregisterNetworkCallbacks()
        executor.release()
    }
}
