package tools.isekai.terminal

import android.app.Application
import android.net.Uri
import androidx.lifecycle.AndroidViewModel
import androidx.lifecycle.viewModelScope
import java.util.UUID
import java.util.concurrent.atomic.AtomicBoolean
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.combine
import kotlinx.coroutines.flow.update
import kotlinx.coroutines.launch
import tools.isekai.terminal.data.ConnectionProfile
import tools.isekai.terminal.data.HostKeySettings
import tools.isekai.terminal.data.Repositories
import tools.isekai.terminal.data.Snippet
import tools.isekai.terminal.data.toIsekaiPipeQuicConfig
import tools.isekai.terminal.data.toIsekaiLinkRelayConfig
import tools.isekai.terminal.data.toIsekaiStunP2pConfig
import tools.isekai.terminal.data.toMultipathIsekaiPipeQuicConfig
import tools.isekai.terminal.data.toQuicConfig
import tools.isekai.terminal.data.toSshConfig
import tools.isekai.terminal.session.AndroidAppExecutor
import tools.isekai.terminal.session.AppExecutor
import tools.isekai.terminal.session.AuthValidation
import tools.isekai.terminal.session.AuthValidator
import tools.isekai.terminal.session.PhysicalMultipathFds
import tools.isekai.terminal.session.RealHostKeyChecker
import tools.isekai.terminal.session.TerminalSession
import tools.isekai.terminal.ui.TerminalTheme
import tools.isekai.terminal.ui.TerminalThemes
import tools.isekai.terminal.ui.applyTo
import tools.isekai.terminal.util.RemoteLogger
import uniffi.isekai_terminal_core.CellData
import uniffi.isekai_terminal_core.ClipboardMimeKind
import uniffi.isekai_terminal_core.ClipboardPayload
import uniffi.isekai_terminal_core.IsekaiPipeQuicConfig
import uniffi.isekai_terminal_core.IsekaiLinkRelayConfig
import uniffi.isekai_terminal_core.IsekaiStunP2pConfig
import uniffi.isekai_terminal_core.MultipathIsekaiPipeQuicConfig
import uniffi.isekai_terminal_core.QuicConfig
import uniffi.isekai_terminal_core.SshAuth
import uniffi.isekai_terminal_core.SshConfig
import uniffi.isekai_terminal_core.TransportPreference

/**
 * 複数タブ（複数 SSH/QUIC セッション）を横断する Activity/Application スコープの状態管理。
 *
 * 「タブ横断で1回だけ登録すればよい」責務——ネットワーク監視・ForegroundService の
 * 起動/停止・ネットワーク断の全セッションへのファンアウト——をここに集約する。
 * 個々のセッションのドメインロジック（接続状態機械・trzsz 等）は既存の [TerminalSession]
 * にそのまま委譲し、[TerminalSession] 自体は無改修で複数インスタンス生成するだけに留める
 * （Rust の [uniffi.isekai_terminal_core.SessionOrchestratorInterface] もグローバル状態を持たない設計
 * のため、UniFFI 側の変更は不要）。
 *
 * 単一セッション時代の [TerminalViewModel] が持っていた全トランスポート分岐・スニペット・
 * 接続後自動実行コマンド・upstream フェイルオーバー・agent forwarding 確認は、ここでは
 * タブ([TabState])単位の状態として複製する。
 *
 * 既知の制約: 物理マルチパス fd 取得(`acquirePhysicalMultipathFds`)・upstream フェイルオーバー
 * 監視(`registerUpstreamFailoverMonitor`)は [AppExecutor] 側がプロセス単位のグローバル API
 * （タブ単位に分離されていない）であるため、複数タブが同時に
 * `ISEKAI_PIPE_QUIC_MULTIPATH` + 物理マルチパス/upstream フェイルオーバーを有効にした場合は
 * 後勝ちになる。単一セッション設計時点からの既存の制約であり、このタブ機能追加で新たに
 * 生まれたものではない。
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
        {
            val clipboardPolicy = RemoteClipboardPolicy(
                isWriteAllowed = {
                    app.getSharedPreferences("isekai_terminal_ui", android.content.Context.MODE_PRIVATE)
                        .getBoolean(PREF_KEY_ALLOW_REMOTE_CLIPBOARD_WRITE, false)
                },
                isPullAllowed = {
                    app.getSharedPreferences("isekai_terminal_ui", android.content.Context.MODE_PRIVATE)
                        .getBoolean(PREF_KEY_ALLOW_REMOTE_CLIPBOARD_PULL, false)
                },
                writeToClipboard = { payload ->
                    val cm = app.getSystemService(android.content.Context.CLIPBOARD_SERVICE)
                        as android.content.ClipboardManager
                    val clip = when (payload.mime) {
                        ClipboardMimeKind.IMAGE_PNG ->
                            RemoteClipboardImagePolicy.writeImageToClipData(app, payload.data)
                        ClipboardMimeKind.TEXT_HTML -> {
                            val html = String(payload.data, Charsets.UTF_8)
                            android.content.ClipData.newHtmlText("isekai-terminal (remote)", html, html)
                        }
                        else -> android.content.ClipData.newPlainText(
                            "isekai-terminal (remote)",
                            String(payload.data, Charsets.UTF_8),
                        )
                    }
                    // 不正なPNGペイロード(署名不一致・サイズ超過)は[RemoteClipboardImagePolicy]が
                    // `null`を返して弾く。クリップボードには何も反映しない。
                    if (clip != null) cm.setPrimaryClip(clip)
                },
                readFromClipboard = {
                    val cm = app.getSystemService(android.content.Context.CLIPBOARD_SERVICE)
                        as android.content.ClipboardManager
                    val clipData = cm.primaryClip
                    val item = clipData?.takeIf { it.itemCount > 0 }?.getItemAt(0)
                    when {
                        RemoteClipboardImagePolicy.isImageClip(clipData) ->
                            RemoteClipboardImagePolicy.readImageFromClipData(app.contentResolver, clipData)
                        item?.htmlText != null ->
                            ClipboardPayload(ClipboardMimeKind.TEXT_HTML, item.htmlText.toByteArray(Charsets.UTF_8))
                        else -> item?.coerceToText(app)?.toString()
                            ?.takeIf { it.isNotEmpty() }
                            ?.let { ClipboardPayload(ClipboardMimeKind.TEXT_PLAIN, it.toByteArray(Charsets.UTF_8)) }
                    }
                },
            )
            TerminalSession(
                RealHostKeyChecker(Repositories.knownHosts) {
                    HostKeySettings.isAutoTrustNewHostKeysEnabled(app)
                },
                onClipboardWriteRequested = clipboardPolicy::onClipboardWriteRequested,
                onClipboardPullRequested = clipboardPolicy::onClipboardPullRequested,
            )
        },
    )

    companion object {
        // Connected 直後は取りこぼし防止のため少し待ってから自動実行コマンドを送る。
        private const val POST_CONNECT_DEBOUNCE_MS = 400L
    }

    /**
     * 1タブ分の状態。ドメイン状態の SSOT はあくまで [session]（ひいては Rust 側）であり、
     * ここで保持するのは接続前バリデーションエラー・スニペット一覧・接続後自動実行コマンドの
     * 送信フラグなど Kotlin ローカルの補助状態のみ。
     */
    class TabState internal constructor(
        val tabId: String,
        val session: TerminalSession,
        val profile: ConnectionProfile?,
        val label: String,
        initialTheme: TerminalTheme,
        initialThemeIsOverridden: Boolean,
    ) {
        // 接続前のバリデーションエラー。session.state (Rust 由来) には混入させず合成する。
        internal val preConnectError = MutableStateFlow<String?>(null)
        // trzsz アップロードの二重起動防止 (Bug 2 と同種のガード。タブごとに独立させる)。
        internal val uploadInProgress = AtomicBoolean(false)

        // ── 定型コマンド（スニペット）─────────────────────────────
        internal val snippets = MutableStateFlow<List<Snippet>>(emptyList())

        // ── 接続後自動実行コマンド ────────────────────────────────
        internal var pendingPostConnectBytes: ByteArray? = null
        internal val postConnectSent = AtomicBoolean(true)

        // ── upstream フェイルオーバー ────────────────────────────
        internal var upstreamFailoverEnabledForCurrentSession = false
        internal val rebindInFlight = AtomicBoolean(false)

        // ── 配色テーマ（Phase 12 P2-1: per-session/per-host theme）───────
        // Global default → Profile default → Tab/session override の3段階のうち、
        // このタブが「今」使っているテーマの解決結果。isThemeOverridden が false の間は
        // アプリ全体のテーマ変更が [TerminalTabsViewModel.applyGlobalThemeToNonOverriddenTabs]
        // 経由でここへ反映され続ける。true になった後(このタブだけ個別に変更した後)は
        // 以後グローバル変更の影響を受けない。
        internal val currentTheme = MutableStateFlow(initialTheme)
        internal var isThemeOverridden: Boolean = initialThemeIsOverridden

        /** UI が購読する合成済み状態。 */
        val uiState: Flow<TerminalUiState> = session.state.combine(preConnectError) { s, err ->
            if (err != null) s.copy(statusMsg = err) else s
        }
    }

    private val _tabs = MutableStateFlow<List<TabState>>(emptyList())
    val tabs: StateFlow<List<TabState>> = _tabs.asStateFlow()

    private val _activeTabId = MutableStateFlow<String?>(null)
    val activeTabId: StateFlow<String?> = _activeTabId.asStateFlow()

    // タブごとの監視コルーチン（通知集約の再計算・ダウンロード完了ファンアウト・接続状態遷移）。closeTab で cancel する。
    private val watchJobs = mutableMapOf<String, Job>()

    init {
        RemoteLogger.i("IsekaiTerminalTabsVM", "TerminalTabsViewModel created")
        executor.registerNetworkCallbacks(
            onAvailable = {
                RemoteLogger.i("IsekaiTerminalSSH", "network available")
                onNetworkPathChanged(isSatisfied = true)
            },
            onLost = { onNetworkPathChanged(isSatisfied = false) },
        )
    }

    // ── ネットワーク（全タブへファンアウト）───────────────────────────

    /** internal にすることでテストから直接呼べる。 */
    internal fun onNetworkPathChanged(isSatisfied: Boolean) {
        _tabs.value.forEach { it.session.notifyNetworkPathChanged(isSatisfied) }
    }

    // ── タブのライフサイクル ────────────────────────────────────────

    /**
     * アプリ全体の既定テーマ(ProfileListScreenの配色ダイアログが書き込む
     * SharedPreferences("isekai_terminal_ui"))を読む。[openTab]でプロファイルにテーマ指定が
     * 無い場合の解決や、[applyGlobalThemeToNonOverriddenTabs]の呼び出し元(MainActivity)
     * が渡してくる値の既定として使う。
     */
    private fun currentGlobalTheme(): TerminalTheme {
        val prefs = getApplication<Application>().getSharedPreferences("isekai_terminal_ui", android.content.Context.MODE_PRIVATE)
        return TerminalThemes.byName(prefs.getString(TerminalThemes.PREF_KEY, null))
    }

    /** 新しいタブを開いて接続を開始し、そのタブをアクティブにする。生成した tabId を返す。 */
    fun openTab(profile: ConnectionProfile, password: String? = null, jumpPassword: String? = null): String {
        val tabId = UUID.randomUUID().toString()
        val session = sessionFactory()
        // Phase 12 P2-1: Global default → Profile default の解決。プロファイルに明示的な
        // テーマ指定があれば、その時点で「上書き済み」タブとして扱う(以後グローバル変更に
        // 追従しない。ユーザーがそのプロファイル用に選んだ意図を尊重する)。
        val profileTheme = profile.themeName?.let { TerminalThemes.byName(it) }
        val initialTheme = profileTheme ?: currentGlobalTheme()
        val tab = TabState(tabId, session, profile, profile.label, initialTheme, initialThemeIsOverridden = profileTheme != null)

        RemoteLogger.i("IsekaiTerminalTabsVM", "openTab '${profile.label}' id=$tabId")
        _tabs.update { it + tab }
        _activeTabId.value = tabId

        // 複数セッションを1つの FGS が共有する。初回タブで起動、以後は通知内容の更新のみ。
        executor.ensureServiceRunning()
        watchTab(tab)
        connectTab(tab, profile, password, jumpPassword)
        updateSessionsSummary()
        return tabId
    }

    /** タブを切断＋破棄する。最後のタブが閉じられた場合のみ FGS を停止させる。 */
    fun closeTab(tabId: String) {
        val tab = _tabs.value.find { it.tabId == tabId } ?: return
        RemoteLogger.i("IsekaiTerminalTabsVM", "closeTab id=$tabId")
        tab.session.disconnect()
        tab.session.close()
        watchJobs.remove(tabId)?.cancel()
        if (tab.upstreamFailoverEnabledForCurrentSession) {
            executor.releasePhysicalMultipathFds()
            executor.unregisterUpstreamFailoverMonitor()
        }

        _tabs.update { list -> list.filterNot { it.tabId == tabId } }
        if (_activeTabId.value == tabId) {
            _activeTabId.value = _tabs.value.firstOrNull()?.tabId
        }
        updateSessionsSummary()
    }

    fun setActiveTab(tabId: String) {
        if (_tabs.value.any { it.tabId == tabId }) _activeTabId.value = tabId
    }

    /**
     * アクティブタブを次のタブへ切り替える（末尾なら先頭へ循環）。物理キーボードの
     * Ctrl+Tab ショートカット用（[tools.isekai.terminal.input.TerminalInputView.onNextTabRequested]
     * 経由で呼ばれる）。タブが1つ以下、またはアクティブタブが存在しない場合は何もしない。
     */
    fun nextTab() {
        val list = _tabs.value
        if (list.size < 2) return
        val idx = list.indexOfFirst { it.tabId == _activeTabId.value }
        if (idx < 0) return
        _activeTabId.value = list[(idx + 1) % list.size].tabId
    }

    /**
     * アクティブタブを前のタブへ切り替える（先頭なら末尾へ循環）。物理キーボードの
     * Ctrl+Shift+Tab ショートカット用。タブが1つ以下、またはアクティブタブが存在しない場合は
     * 何もしない。
     */
    fun previousTab() {
        val list = _tabs.value
        if (list.size < 2) return
        val idx = list.indexOfFirst { it.tabId == _activeTabId.value }
        if (idx < 0) return
        _activeTabId.value = list[(idx - 1 + list.size) % list.size].tabId
    }

    private fun tabOrNull(tabId: String): TabState? = _tabs.value.find { it.tabId == tabId }

    /**
     * タブ固有の監視: 通知集約の再計算・ダウンロード完了ファイルの保存・
     * 接続状態遷移(Connected 立ち上がりでの自動実行コマンド送信・切断時の後始末)・
     * upstream フェイルオーバーの `NoViablePath` 検知。非アクティブでも動き続ける。
     */
    private fun watchTab(tab: TabState) {
        watchJobs[tab.tabId] = viewModelScope.launch {
            launch { observeSummary(tab) }
            launch { observeDownloads(tab) }
            launch { observeFailover(tab) }
            launch { observeConnectionTransitions(tab) }
        }
    }

    private suspend fun observeSummary(tab: TabState) {
        tab.session.state.collect { updateSessionsSummary() }
    }

    private suspend fun observeDownloads(tab: TabState) {
        tab.session.pendingDownloadFile.collect { pending ->
            pending ?: return@collect
            executor.saveDownloadFile(pending.first, pending.second)
            tab.session.consumeDownloadFile()
        }
    }

    private suspend fun observeFailover(tab: TabState) {
        tab.session.noViablePathEvent.collect {
            if (tab.upstreamFailoverEnabledForCurrentSession) onWifiUpstreamBroken(tab)
        }
    }

    private suspend fun observeConnectionTransitions(tab: TabState) {
        var prevConnected = false
        tab.uiState.collect { state ->
            val connected = state.connected
            if (connected && !prevConnected) {
                executor.notifyConnected(state.currentHost ?: "")
                if (tab.upstreamFailoverEnabledForCurrentSession) {
                    executor.registerUpstreamFailoverMonitor { onWifiUpstreamBroken(tab) }
                }
                maybeSendPostConnectCommands(tab)
            } else if (!connected && prevConnected) {
                executor.notifyDisconnected()
                executor.releasePhysicalMultipathFds()
                executor.unregisterUpstreamFailoverMonitor()
                tab.upstreamFailoverEnabledForCurrentSession = false
            }
            prevConnected = connected
        }
    }

    private fun updateSessionsSummary() {
        val tabs = _tabs.value
        val connected = tabs.count { it.session.state.value.connected }
        executor.updateSessionsSummary(connected, tabs.size)
    }

    // ── upstream フェイルオーバー ────────────────────────────────────

    /**
     * 「WiFiは繋がっているがupstreamが死んでいる」を検知した際の処理。
     * セルラーへの bindSocket 済み fd を取得できたら `rebindToFd` でendpointの
     * ソケットを丸ごと差し替える。取得できなければ何もしない（日和見的ポリシー）。
     * [TabState.rebindInFlight] で多重発火（capabilities変化の連続通知等）を防ぐ。
     */
    private fun onWifiUpstreamBroken(tab: TabState) {
        if (!tab.rebindInFlight.compareAndSet(false, true)) return
        viewModelScope.launch(Dispatchers.IO) {
            try {
                val cellular = executor.acquireCellularFd()
                if (cellular == null) {
                    RemoteLogger.w("IsekaiTerminalSSH", "upstream failover: cellular fd not available, staying on current path")
                    return@launch
                }
                val (fd, localIp) = cellular
                RemoteLogger.i("IsekaiTerminalSSH", "upstream failover: rebinding to cellular (localIp=$localIp)")
                tab.session.rebindToFd(fd, localIp)
            } finally {
                tab.rebindInFlight.set(false)
            }
        }
    }

    // ── 接続 ─────────────────────────────────────────────────────────

    fun reconnect(tabId: String, password: String? = null, jumpPassword: String? = null) {
        val tab = tabOrNull(tabId) ?: return
        val profile = tab.profile ?: return
        connectTab(tab, profile, password, jumpPassword)
    }

    private fun connectTab(tab: TabState, profile: ConnectionProfile, password: String?, jumpPassword: String? = null) {
        val current = tab.session.state.value
        if (current.connected || current.isConnecting) return
        tab.preConnectError.value = null
        armPostConnectCommands(tab, profile)
        loadSnippets(tab.tabId, profile.id)
        RemoteLogger.i(
            "IsekaiTerminalSSH",
            "connectTab[${tab.tabId}]: '${profile.label}' ${profile.username}@${profile.host}:${profile.port} " +
                "transport=${profile.transportPreference}" +
                (if (profile.usesJumpHost) " via jump ${profile.jumpUsername}@${profile.jumpHost}:${profile.jumpPort}" else ""),
        )
        viewModelScope.launch(Dispatchers.IO) {
            val auth = resolveAuth(tab, profile, password) ?: return@launch
            // 踏み台(jump host)は、SSHブートストラップを伴う全トランスポートで共通に使える
            // (TSSHD_QUICのみ旧Phase 5B経路でrust-core側が未対応、Phase 10--1c参照)。
            val jumpAuth = if (profile.usesJumpHost) {
                resolveJumpAuth(tab, profile, jumpPassword) ?: return@launch
            } else {
                null
            }
            when (profile.transportPreference) {
                TransportPreference.PLAIN_SSH -> connect(tab, profile.toSshConfig(auth, jumpAuth))
                TransportPreference.TSSHD_QUIC -> connectQuic(tab, profile.toQuicConfig(auth))
                TransportPreference.ISEKAI_PIPE_QUIC -> connectIsekaiPipeQuic(tab, profile.toIsekaiPipeQuicConfig(auth, jumpAuth))
                TransportPreference.AUTO -> connectIsekaiPipeQuicAuto(tab, profile.toIsekaiPipeQuicConfig(auth, jumpAuth))
                TransportPreference.ISEKAI_PIPE_QUIC_MULTIPATH -> {
                    // Phase 9-4（実験的機能）: 有効化されていれば物理Wi-Fi/セルラーの
                    // fdも取得してから接続する。取得に失敗/未取得でも例外にはせず、
                    // path0/path1のみのマルチパスにフォールバックする（日和見的ポリシー）。
                    val physicalFds = if (profile.enablePhysicalMultipath) {
                        executor.acquirePhysicalMultipathFds()
                    } else {
                        PhysicalMultipathFds()
                    }
                    tab.upstreamFailoverEnabledForCurrentSession = profile.enableUpstreamFailover
                    connectMultipathIsekaiPipeQuic(tab, profile.toMultipathIsekaiPipeQuicConfig(auth, physicalFds, jumpAuth))
                }
                TransportPreference.ISEKAI_STUN_P2P_QUIC ->
                    connectIsekaiStunP2p(tab, profile.toIsekaiStunP2pConfig(auth, jumpAuth))
                TransportPreference.ISEKAI_LINK_RELAY_QUIC -> {
                    // relayJwt は Room に RelayCredentialVault で暗号化して保存してあるため、
                    // 実際の接続直前に復号する(toIsekaiLinkRelayConfig 自体は暗号化を意識しない
                    // 純粋なマッピング関数のまま保つ)。
                    val decrypted = profile.copy(relayJwt = profile.relayJwt?.let { executor.decryptRelayJwt(it) })
                    connectIsekaiLinkRelay(tab, decrypted.toIsekaiLinkRelayConfig(auth, jumpAuth))
                }
            }
            // タスク#65: 復号済み秘密鍵PEMのベストエフォートなメモリ消去。
            // connect_* はUniFFI越しのFFI呼び出しで、呼び出し内でByteArrayの内容を
            // 同期的にRust側へコピーしてから戻る(直上のコメント参照)ため、
            // ここで元のByteArrayをゼロ埋めしてもRust側の認証には影響しない。
            // ただしJVM上に他の参照(GCされるまでのコピー等)が残っていないことまでは
            // 保証できないベストエフォートの対策。
            wipeIfPublicKey(auth)
            wipeIfPublicKey(jumpAuth)
            // Phase 12 P2-1: このタブが解決したテーマ(Global default → Profile default)を
            // 接続直後に反映する。connect_* はRust側で同期的にActiveSessionを差し込むため、
            // このタイミングで呼べば確実にセッションへ届く。
            pushThemeToSession(tab, tab.currentTheme.value)
        }
    }

    /** [auth]が公開鍵認証なら復号済みPEMのByteArrayをその場でゼロ埋めする(タスク#65)。
     *  パスワード認証の`String`は不変かつCompose `TextField`がString前提のため、
     *  完全なゼロ化は行わない(ベストエフォート対策として本コメントで言及するに留める)。 */
    private fun wipeIfPublicKey(auth: SshAuth?) {
        if (auth is SshAuth.PublicKey) {
            java.util.Arrays.fill(auth.privateKeyPem, 0)
        }
    }

    private fun pushThemeToSession(tab: TabState, theme: TerminalTheme) {
        theme.applyTo(tab.session::setTheme)
    }

    /**
     * このタブだけの配色テーマを明示的に変更する(Tab/session override)。
     * 以後このタブは[applyGlobalThemeToNonOverriddenTabs]の影響を受けなくなる。
     */
    fun setTabTheme(tabId: String, theme: TerminalTheme) {
        val tab = tabOrNull(tabId) ?: return
        tab.isThemeOverridden = true
        tab.currentTheme.value = theme
        pushThemeToSession(tab, theme)
    }

    /**
     * アプリ全体の既定テーマが変更された時に呼ぶ。まだタブ固有の上書きをしていない
     * ([TabState.isThemeOverridden] が false の)タブにだけそのまま反映する。
     * MainActivity の ProfileListScreen 側テーマ変更コールバックから呼ばれる想定。
     */
    fun applyGlobalThemeToNonOverriddenTabs(theme: TerminalTheme) {
        _tabs.value.forEach { tab ->
            if (!tab.isThemeOverridden) {
                tab.currentTheme.value = theme
                pushThemeToSession(tab, theme)
            }
        }
    }

    private fun connect(tab: TabState, config: SshConfig) {
        executor.ensureServiceRunning()
        tab.session.connect(config)
    }

    private fun connectQuic(tab: TabState, config: QuicConfig) {
        executor.ensureServiceRunning()
        tab.session.connectQuic(config)
    }

    private fun connectIsekaiPipeQuic(tab: TabState, config: IsekaiPipeQuicConfig) {
        executor.ensureServiceRunning()
        tab.session.connectIsekaiPipeQuic(config)
    }

    private fun connectIsekaiPipeQuicAuto(tab: TabState, config: IsekaiPipeQuicConfig) {
        executor.ensureServiceRunning()
        tab.session.connectIsekaiPipeQuicAuto(config)
    }

    private fun connectMultipathIsekaiPipeQuic(tab: TabState, config: MultipathIsekaiPipeQuicConfig) {
        executor.ensureServiceRunning()
        tab.session.connectMultipathIsekaiPipeQuic(config)
    }

    private fun connectIsekaiStunP2p(tab: TabState, config: IsekaiStunP2pConfig) {
        executor.ensureServiceRunning()
        tab.session.connectIsekaiStunP2p(config)
    }

    private fun connectIsekaiLinkRelay(tab: TabState, config: IsekaiLinkRelayConfig) {
        executor.ensureServiceRunning()
        tab.session.connectIsekaiLinkRelay(config)
    }

    private suspend fun resolveAuth(tab: TabState, profile: ConnectionProfile, password: String?): SshAuth? =
        resolveAuthInternal(tab, profile.authType, password, profile.keyId, errorPrefix = "")

    /** 踏み台(jump host)側の認証情報を解決する。[resolveAuth] と同じ検証ロジックを
     *  jump_auth_type/jump_key_id に適用するだけの対の関数。 */
    private suspend fun resolveJumpAuth(tab: TabState, profile: ConnectionProfile, jumpPassword: String?): SshAuth? =
        resolveAuthInternal(tab, profile.jumpAuthType ?: "", jumpPassword, profile.jumpKeyId, errorPrefix = "踏み台: ")

    private suspend fun resolveAuthInternal(
        tab: TabState,
        authType: String,
        password: String?,
        keyId: Long?,
        errorPrefix: String,
    ): SshAuth? {
        return when (val v = AuthValidator.validate(authType, password, keyId)) {
            is AuthValidation.Error -> {
                RemoteLogger.w("IsekaiTerminalSSH", "${errorPrefix}auth error: ${v.statusMsg}")
                tab.preConnectError.value = "$errorPrefix${v.statusMsg}"
                null
            }
            is AuthValidation.Password -> SshAuth.Password(v.value)
            is AuthValidation.PublicKey -> loadPublicKeyAuth(tab, v.keyId)
        }
    }

    private suspend fun loadPublicKeyAuth(tab: TabState, keyId: Long): SshAuth? =
        runCatching { SshAuth.PublicKey(executor.loadKeyPem(keyId)) }
            .getOrElse { e ->
                RemoteLogger.e("IsekaiTerminalSSH", "key error: ${e.message}", e)
                tab.preConnectError.value = "鍵エラー: ${e.message}"
                null
            }

    // ── 定型コマンド（スニペット）─────────────────────────────────

    /** [profileId] が null なら全プロファイル共通のスニペットのみ、非nullなら共通＋専用をマージして読み込む。 */
    fun loadSnippets(tabId: String, profileId: Long?) {
        val tab = tabOrNull(tabId) ?: return
        viewModelScope.launch(Dispatchers.IO) {
            tab.snippets.value = Repositories.snippets.getForProfile(profileId)
        }
    }

    fun sendSnippet(tabId: String, snippet: Snippet) {
        RemoteLogger.i("IsekaiTerminalSnippet", "send snippet '${snippet.label}' id=${snippet.id} tab=$tabId")
        send(tabId, SnippetCommands.toBytes(snippet))
    }

    // ── 接続後自動実行コマンド ────────────────────────────────────

    /** 新しい接続試行のたびに呼び、この接続で送るべきコマンド（あれば）とフラグをリセットする。 */
    private fun armPostConnectCommands(tab: TabState, profile: ConnectionProfile) {
        val commands = profile.postConnectCommands?.takeIf { it.isNotBlank() }
        tab.pendingPostConnectBytes = commands?.let { SnippetCommands.toBytes(it, appendNewline = true) }
        tab.postConnectSent.set(tab.pendingPostConnectBytes == null)
    }

    /** Connected 立ち上がりで1回だけ呼ばれる。CAS でセッション単位の二重発火を防ぐ。 */
    private fun maybeSendPostConnectCommands(tab: TabState) {
        if (!tab.postConnectSent.compareAndSet(false, true)) return
        val bytes = tab.pendingPostConnectBytes ?: return
        viewModelScope.launch {
            delay(POST_CONNECT_DEBOUNCE_MS)
            RemoteLogger.i("IsekaiTerminalSSH", "sending post-connect commands (${bytes.size} bytes) tab=${tab.tabId}")
            send(tab.tabId, bytes)
        }
    }

    // ── セッション操作（タブ指定。すべて session への薄い委譲）──────────

    fun send(tabId: String, bytes: ByteArray) = tabOrNull(tabId)?.session?.send(bytes)

    fun resize(tabId: String, cols: UInt, rows: UInt) = tabOrNull(tabId)?.session?.resize(cols, rows)

    fun disconnect(tabId: String) = tabOrNull(tabId)?.session?.disconnect()

    fun scrollbackCells(tabId: String, offset: Int, rows: Int): List<CellData>? =
        tabOrNull(tabId)?.session?.scrollbackCells(offset, rows)

    fun trustUpdatedHostKey(tabId: String) = tabOrNull(tabId)?.session?.trustUpdatedHostKey()

    fun dismissHostKeyWarning(tabId: String) = tabOrNull(tabId)?.session?.dismissHostKeyWarning()

    fun trustNewHostKey(tabId: String) = tabOrNull(tabId)?.session?.trustNewHostKey()

    fun dismissNewHostKeyPrompt(tabId: String) = tabOrNull(tabId)?.session?.dismissNewHostKeyPrompt()

    fun respondAgentSignRequest(tabId: String, approved: Boolean) =
        tabOrNull(tabId)?.session?.respondAgentSignRequest(approved)

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
        RemoteLogger.i("IsekaiTerminalTabsVM", "TerminalTabsViewModel cleared")
        watchJobs.values.forEach { it.cancel() }
        _tabs.value.forEach { it.session.close() }
        executor.unregisterNetworkCallbacks()
        executor.release()
    }
}
