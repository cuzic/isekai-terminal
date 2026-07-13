package tools.isekai.terminal

import android.app.Application
import android.net.Uri
import androidx.lifecycle.AndroidViewModel
import androidx.lifecycle.viewModelScope
import java.util.UUID
import java.util.concurrent.atomic.AtomicBoolean
import kotlinx.coroutines.CoroutineDispatcher
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
import kotlinx.coroutines.runBlocking
import tools.isekai.terminal.data.ConnectionProfile
import tools.isekai.terminal.data.HostKeySettings
import tools.isekai.terminal.data.Repositories
import tools.isekai.terminal.data.Snippet
import tools.isekai.terminal.session.AndroidAppExecutor
import tools.isekai.terminal.session.AppExecutor
import tools.isekai.terminal.session.RealHostKeyChecker
import tools.isekai.terminal.session.TerminalSession
import tools.isekai.terminal.ui.TerminalTheme
import tools.isekai.terminal.ui.TerminalThemes
import tools.isekai.terminal.ui.applyTo
import tools.isekai.terminal.util.RemoteLogger
import uniffi.isekai_terminal_core.CellData
import uniffi.isekai_terminal_core.ClipboardMimeKind
import uniffi.isekai_terminal_core.ClipboardPayload
import uniffi.isekai_terminal_core.PlatformFd

/**
 * 複数タブ（複数 SSH/QUIC セッション）を横断する Application スコープの状態管理。
 *
 * [MainActivity.AppRoot]は`viewModel(viewModelStoreOwner = application, ...)`で生成する
 * ([IsekaiTerminalApplication]の[androidx.lifecycle.ViewModelStore]を使う)。Activityスコープに
 * していた旧実装では、Activityが(バックグラウンド中のタスク破棄等で)正規のfinish経路を通らず
 * 再生成されると[onCleared]が呼ばれずに古いインスタンスが破棄され、`session.close()`が
 * 一度も実行されないままRust側のSSH接続だけがプロセス内に孤立し、新しいインスタンスからは
 * それを発見・再アタッチする手段が無いというバグがあった(実機検証で発見、2026-07-12)。
 * Applicationスコープならプロセスが生きている限り同一インスタンスが使われ続けるため、
 * このクラスがそもそも「破棄されて再生成される」状況自体が起こらなくなる。
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
/**
 * タブ内の2分割方向。[HORIZONTAL] は左右に並べる(縦の仕切り線)、[VERTICAL] は上下に並べる
 * (横の仕切り線)。画面分割(split pane)機能はまず2分割のみをサポートする(バイナリツリー式の
 * 多段分割は将来の拡張余地としてスコープ外にする)。
 */
enum class SplitDirection { HORIZONTAL, VERTICAL }

/**
 * 1ペイン分の状態。画面分割(split pane)機能により、1タブの中に複数ペイン(まずは最大2つ)を
 * 持てるようにするための単位。各ペインは完全に独立した [TerminalSession](ひいては独立した
 * Rust側接続)を持つ(同一セッションを複数ペインで共有する設計はスコープ外、
 * `.claude/rules/rust-ssot.md` の「UI表示だけに閉じた状態」の例外としてペインの存在自体・
 * レイアウト・フォーカスはこの Kotlin 側の状態で管理する)。
 *
 * かつて [TerminalTabsViewModel.TabState] が直接持っていた「1タブ=1セッション」時代の
 * 補助状態(接続前バリデーションエラー・アップロード中フラグ・スニペット一覧・接続後自動実行
 * コマンド・upstreamフェイルオーバー)を、ペイン単位に切り出したもの。
 */
class PaneState internal constructor(
    val paneId: String,
    val session: TerminalSession,
) {
    // 接続前のバリデーションエラー。session.state (Rust 由来) には混入させず合成する。
    internal val preConnectError = MutableStateFlow<String?>(null)
    // trzsz アップロードの二重起動防止 (Bug 2 と同種のガード。ペインごとに独立させる)。
    internal val uploadInProgress = AtomicBoolean(false)

    // ── 定型コマンド（スニペット）─────────────────────────────
    internal val snippets = MutableStateFlow<List<Snippet>>(emptyList())

    // ── 接続後自動実行コマンド ────────────────────────────────
    internal var pendingPostConnectBytes: ByteArray? = null
    internal val postConnectSent = AtomicBoolean(true)

    // ── upstream フェイルオーバー ────────────────────────────
    internal var upstreamFailoverEnabledForCurrentSession = false
    internal val rebindInFlight = AtomicBoolean(false)

    /** UI が購読する合成済み状態。 */
    val uiState: Flow<TerminalUiState> = session.state.combine(preConnectError) { s, err ->
        if (err != null) s.copy(statusMsg = err) else s
    }
}

class TerminalTabsViewModel(
    app: Application,
    private val executor: AppExecutor,
    private val sessionFactory: (AppExecutor) -> TerminalSession,
    // テストがtestScheduler駆動のディスパッチャーを注入できるようにする(既定は本番同様
    // Dispatchers.IO)。ハードコードしていた頃はテストの仮想時間(TestCoroutineScheduler)と
    // ここで起動される実スレッドの完了タイミングが競合し、withTimeout()ポーリングが
    // 断続的にタイムアウトする原因になっていた。
    private val ioDispatcher: CoroutineDispatcher = Dispatchers.IO,
) : AndroidViewModel(app) {

    /** 本番用コンストラクタ。Compose の viewModel() から呼ばれる。
     *  [sessionFactory] は`executor`を引数で受け取る形にしている
     *  ([acquireWifiFd]/[acquireCellularFd]で同じ`executor`インスタンスを再利用するため
     *  ——セカンダリコンストラクタの`this(...)`委譲の中では`this.executor`(未初期化)を
     *  参照できないので、`AndroidAppExecutor(app)`を二重生成せずに済むようにする)。 */
    constructor(app: Application) : this(
        app,
        AndroidAppExecutor(app),
        { executor ->
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
                // #10/#22: RebindManager(Rust側)がWiFi/セルラーのfdを要求してきたら、
                // AppExecutor経由で取得して返すだけ(判断はしない、rust-ssot.md準拠)。
                // Rust側のspawn_blockingスレッドから同期呼び出しされるためrunBlockingで
                // suspend関数をブリッジする(onAgentSignRequest等と同じ方式)。
                acquireWifiFd = {
                    runBlocking { executor.acquireWifiFd() }?.let { (fd, ip) -> PlatformFd(fd, ip) }
                },
                acquireCellularFd = {
                    runBlocking { executor.acquireCellularFd() }?.let { (fd, ip) -> PlatformFd(fd, ip) }
                },
            )
        },
    )

    companion object {
        // Connected 直後は取りこぼし防止のため少し待ってから自動実行コマンドを送る。
        private const val POST_CONNECT_DEBOUNCE_MS = 400L
    }

    /**
     * 1タブ分の状態。ドメイン状態の SSOT はあくまで各ペインの [TerminalSession]（ひいては
     * Rust 側）であり、ここで保持するのはペイン構成(画面分割)・フォーカス・配色テーマなど
     * Kotlin ローカルの補助状態のみ。
     *
     * 画面分割(split pane)導入前は「1タブ=1セッション」だったため、[session] 等の
     * 旧APIプロパティは引き続き [primaryPane] への薄い委譲として残してある
     * (未分割のタブでは [primaryPane] が唯一のペインであり、[focusedPane] も常にそれを指す
     * ため、既存の呼び出し元・テストの挙動は変わらない)。
     */
    class TabState internal constructor(
        val tabId: String,
        internal val primaryPane: PaneState,
        val profile: ConnectionProfile?,
        val label: String,
        initialTheme: TerminalTheme,
        initialThemeIsOverridden: Boolean,
    ) {
        // ── 後方互換プロパティ(1タブ=1セッション時代のAPI表面。primaryPaneへの委譲)──
        val session: TerminalSession get() = primaryPane.session
        internal val preConnectError get() = primaryPane.preConnectError
        internal val uploadInProgress get() = primaryPane.uploadInProgress
        internal val snippets get() = primaryPane.snippets
        internal var pendingPostConnectBytes: ByteArray?
            get() = primaryPane.pendingPostConnectBytes
            set(value) { primaryPane.pendingPostConnectBytes = value }
        internal val postConnectSent get() = primaryPane.postConnectSent
        internal var upstreamFailoverEnabledForCurrentSession: Boolean
            get() = primaryPane.upstreamFailoverEnabledForCurrentSession
            set(value) { primaryPane.upstreamFailoverEnabledForCurrentSession = value }
        internal val rebindInFlight get() = primaryPane.rebindInFlight

        /** UI が購読する合成済み状態(主ペインのもの)。 */
        val uiState: Flow<TerminalUiState> get() = primaryPane.uiState

        // ── 配色テーマ（Phase 12 P2-1: per-session/per-host theme）───────
        // Global default → Profile default → Tab/session override の3段階のうち、
        // このタブが「今」使っているテーマの解決結果。isThemeOverridden が false の間は
        // アプリ全体のテーマ変更が [TerminalTabsViewModel.applyGlobalThemeToNonOverriddenTabs]
        // 経由でここへ反映され続ける。true になった後(このタブだけ個別に変更した後)は
        // 以後グローバル変更の影響を受けない。分割時は全ペインに同じテーマを適用する
        // (ペインごとの配色分岐はスコープ外)。
        internal val currentTheme = MutableStateFlow(initialTheme)
        internal var isThemeOverridden: Boolean = initialThemeIsOverridden

        // ── 画面分割(split pane) ────────────────────────────────
        // まずは水平/垂直の2分割のみをサポートする(バイナリツリー式の多段分割はスコープ外)。
        private val _splitPane = MutableStateFlow<PaneState?>(null)
        val splitPane: StateFlow<PaneState?> = _splitPane.asStateFlow()
        private val _splitDirection = MutableStateFlow<SplitDirection?>(null)
        val splitDirection: StateFlow<SplitDirection?> = _splitDirection.asStateFlow()
        private val _focusedPaneId = MutableStateFlow(primaryPane.paneId)
        val focusedPaneId: StateFlow<String> = _focusedPaneId.asStateFlow()

        /** 現在表示すべきペイン一覧。未分割なら [primaryPane] の1つだけ、分割時は2つ。 */
        val panes: List<PaneState> get() = listOfNotNull(primaryPane, _splitPane.value)

        fun paneOrNull(paneId: String): PaneState? = panes.find { it.paneId == paneId }

        /** キーボード入力・trzsz/host key等のモーダルUIが紐づく「今アクティブな」ペイン。 */
        internal val focusedPane: PaneState get() = paneOrNull(_focusedPaneId.value) ?: primaryPane

        internal fun openSplit(pane: PaneState, direction: SplitDirection) {
            _splitPane.value = pane
            _splitDirection.value = direction
            _focusedPaneId.value = pane.paneId
        }

        /** 分割ペインを閉じる。閉じた側の [PaneState] を返す(session の disconnect/close は
         *  呼び出し元 [TerminalTabsViewModel] の責務)。分割していなければ null。 */
        internal fun closeSplit(): PaneState? {
            val closed = _splitPane.value ?: return null
            _splitPane.value = null
            _splitDirection.value = null
            _focusedPaneId.value = primaryPane.paneId
            return closed
        }

        internal fun setFocusedPane(paneId: String) {
            if (panes.any { it.paneId == paneId }) _focusedPaneId.value = paneId
        }
    }

    private val _tabs = MutableStateFlow<List<TabState>>(emptyList())
    val tabs: StateFlow<List<TabState>> = _tabs.asStateFlow()

    private val _activeTabId = MutableStateFlow<String?>(null)
    val activeTabId: StateFlow<String?> = _activeTabId.asStateFlow()

    // タブごとの監視コルーチン（通知集約の再計算・ダウンロード完了ファンアウト・接続状態遷移）。closeTab で cancel する。
    private val watchJobs = mutableMapOf<String, Job>()

    // トランスポート別connect_*呼び出しへの分岐・認証解決(Task #8 段階1でTerminalTabsViewModel
    // から切り出した)。テーマ反映・スニペット読み込みはこのViewModel側の責務のままコールバックで渡す。
    private val connectionCoordinator = ConnectionCoordinator(
        executor = executor,
        scope = viewModelScope,
        ioDispatcher = ioDispatcher,
        pushTheme = ::pushThemeToSession,
        loadSnippets = ::loadSnippetsForPane,
    )

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

    /** internal にすることでテストから直接呼べる。split pane側にも同じ生イベントを転送する。 */
    internal fun onNetworkPathChanged(isSatisfied: Boolean) {
        _tabs.value.flatMap { it.panes }.forEach { it.session.notifyNetworkPathChanged(isSatisfied) }
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
        val primaryPane = PaneState(UUID.randomUUID().toString(), sessionFactory(executor))
        // Phase 12 P2-1: Global default → Profile default の解決。プロファイルに明示的な
        // テーマ指定があれば、その時点で「上書き済み」タブとして扱う(以後グローバル変更に
        // 追従しない。ユーザーがそのプロファイル用に選んだ意図を尊重する)。
        val profileTheme = profile.themeName?.let { TerminalThemes.byName(it) }
        val initialTheme = profileTheme ?: currentGlobalTheme()
        val tab = TabState(tabId, primaryPane, profile, profile.label, initialTheme, initialThemeIsOverridden = profileTheme != null)

        RemoteLogger.i("IsekaiTerminalTabsVM", "openTab '${profile.label}' id=$tabId")
        _tabs.update { it + tab }
        _activeTabId.value = tabId

        // 複数セッションを1つの FGS が共有する。初回タブで起動、以後は通知内容の更新のみ。
        executor.ensureServiceRunning()
        watchPane(tab, primaryPane)
        connectionCoordinator.connectPane(tab.tabId, tab.currentTheme.value, primaryPane, profile, password, jumpPassword)
        updateSessionsSummary()
        return tabId
    }

    /** タブを切断＋破棄する。分割中なら全ペインを破棄する。最後のタブが閉じられた場合のみ FGS を停止させる。 */
    fun closeTab(tabId: String) {
        val tab = _tabs.value.find { it.tabId == tabId } ?: return
        RemoteLogger.i("IsekaiTerminalTabsVM", "closeTab id=$tabId")
        tab.panes.forEach { pane -> closePaneSession(pane) }

        _tabs.update { list -> list.filterNot { it.tabId == tabId } }
        if (_activeTabId.value == tabId) {
            _activeTabId.value = _tabs.value.firstOrNull()?.tabId
        }
        updateSessionsSummary()
    }

    /** [pane] の監視コルーチンを止め、セッションを切断・破棄する（[closeTab]・[closeSplitPane] 共通）。 */
    private fun closePaneSession(pane: PaneState) {
        pane.session.disconnect()
        pane.session.close()
        watchJobs.remove(pane.paneId)?.cancel()
        if (pane.upstreamFailoverEnabledForCurrentSession) {
            executor.releasePhysicalMultipathFds()
            executor.unregisterUpstreamFailoverMonitor()
        }
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

    // ── 画面分割(split pane) ─────────────────────────────────────────

    /**
     * タブを2分割し、[tab.profile] と同じ接続プロファイルで新規に接続した独立セッションを
     * 新しいペインとして追加する（「同じ接続プロファイルで新規接続する」側の選択肢）。
     * 既に分割済み、またはプロファイルを持たないタブ（現状は必ずプロファイル付きだが将来の
     * 保険）では何もしない。新しく作られたペインの paneId を返す（失敗時は null）。
     */
    fun splitPane(tabId: String, direction: SplitDirection, password: String? = null, jumpPassword: String? = null): String? {
        val tab = tabOrNull(tabId) ?: return null
        if (tab.splitPane.value != null) return null
        val profile = tab.profile ?: return null
        val pane = PaneState(UUID.randomUUID().toString(), sessionFactory(executor))
        RemoteLogger.i("IsekaiTerminalTabsVM", "splitPane[$tabId] new pane=${pane.paneId} direction=$direction")
        tab.openSplit(pane, direction)
        watchPane(tab, pane)
        connectionCoordinator.connectPane(tab.tabId, tab.currentTheme.value, pane, profile, password, jumpPassword)
        updateSessionsSummary()
        return pane.paneId
    }

    /**
     * 既存タブ [sourceTabId] のセッションを、[targetTabId] の分割ペインとして付け替える
     * （「既存タブのセッションを付け替える」側の選択肢）。[sourceTabId] はタブ一覧から消える
     * (セッション自体はdisconnectせず、新しい親タブの下で監視を再開する)。[targetTabId] が
     * 既に分割済み、または [sourceTabId] 自体が既に分割済み（複数ペインの一括付け替えは
     * スコープ外）の場合は何もせず false を返す。
     */
    fun splitPaneWithExistingTab(targetTabId: String, direction: SplitDirection, sourceTabId: String): Boolean {
        if (targetTabId == sourceTabId) return false
        val target = tabOrNull(targetTabId) ?: return false
        if (target.splitPane.value != null) return false
        val source = tabOrNull(sourceTabId) ?: return false
        if (source.splitPane.value != null) return false

        val pane = source.primaryPane
        RemoteLogger.i(
            "IsekaiTerminalTabsVM",
            "splitPaneWithExistingTab: moving pane=${pane.paneId} from tab=$sourceTabId to tab=$targetTabId",
        )
        watchJobs.remove(pane.paneId)?.cancel()
        _tabs.update { list -> list.filterNot { it.tabId == sourceTabId } }
        if (_activeTabId.value == sourceTabId) _activeTabId.value = targetTabId

        target.openSplit(pane, direction)
        watchPane(target, pane)
        // 「分割時は全ペインに同じテーマを適用する」原則(TabState.currentThemeのコメント参照)
        // に合わせ、移動してきたペインにも移動先タブのテーマを揃える。
        pushThemeToSession(pane, target.currentTheme.value)
        updateSessionsSummary()
        return true
    }

    /** 分割ペインを閉じる（未分割なら no-op）。閉じた後は主ペインのみの1ペイン表示に戻る。 */
    fun closeSplitPane(tabId: String) {
        val tab = tabOrNull(tabId) ?: return
        val pane = tab.closeSplit() ?: return
        closePaneSession(pane)
        updateSessionsSummary()
    }

    /** タップ操作等でペインのフォーカス（キーボード入力・モーダルUIの宛先）を切り替える。 */
    fun setFocusedPane(tabId: String, paneId: String) {
        tabOrNull(tabId)?.setFocusedPane(paneId)
    }

    /**
     * ペイン固有の監視: 通知集約の再計算・ダウンロード完了ファイルの保存・
     * 接続状態遷移(Connected 立ち上がりでの自動実行コマンド送信・切断時の後始末)・
     * upstream フェイルオーバーの `NoViablePath` 検知。非アクティブでも動き続ける。
     * [watchJobs] は paneId(タブをまたいで一意)をキーにする — 分割ペインを付け替えても
     * ジョブの追跡が壊れないようにするため。
     */
    private fun watchPane(tab: TabState, pane: PaneState) {
        watchJobs[pane.paneId] = viewModelScope.launch {
            launch { observeSummary(pane) }
            launch { observeDownloads(pane) }
            launch { observeFailover(pane) }
            launch { observeConnectionTransitions(pane) }
        }
    }

    private suspend fun observeSummary(pane: PaneState) {
        pane.session.state.collect { updateSessionsSummary() }
    }

    private suspend fun observeDownloads(pane: PaneState) {
        pane.session.pendingDownloadFile.collect { pending ->
            pending ?: return@collect
            executor.saveDownloadFile(pending.first, pending.second)
            pane.session.consumeDownloadFile()
        }
    }

    private suspend fun observeFailover(pane: PaneState) {
        pane.session.noViablePathEvent.collect {
            if (pane.upstreamFailoverEnabledForCurrentSession) onWifiUpstreamBroken(pane)
        }
    }

    private suspend fun observeConnectionTransitions(pane: PaneState) {
        var prevConnected = false
        pane.uiState.collect { state ->
            val connected = state.connected
            if (connected && !prevConnected) {
                executor.notifyConnected(state.currentHost ?: "")
                if (pane.upstreamFailoverEnabledForCurrentSession) {
                    executor.registerUpstreamFailoverMonitor { onWifiUpstreamBroken(pane) }
                }
                maybeSendPostConnectCommands(pane)
            } else if (!connected && prevConnected) {
                executor.notifyDisconnected()
                executor.releasePhysicalMultipathFds()
                executor.unregisterUpstreamFailoverMonitor()
                pane.upstreamFailoverEnabledForCurrentSession = false
            }
            prevConnected = connected
        }
    }

    private fun updateSessionsSummary() {
        val panes = _tabs.value.flatMap { it.panes }
        val connected = panes.count { it.session.state.value.connected }
        executor.updateSessionsSummary(connected, panes.size)
    }

    // ── upstream フェイルオーバー ────────────────────────────────────

    /**
     * 「WiFiは繋がっているがupstreamが死んでいる」を検知した際の処理。
     * セルラーへの bindSocket 済み fd を取得できたら `rebindToFd` でendpointの
     * ソケットを丸ごと差し替える。取得できなければ何もしない（日和見的ポリシー）。
     * [PaneState.rebindInFlight] で多重発火（capabilities変化の連続通知等）を防ぐ。
     */
    private fun onWifiUpstreamBroken(pane: PaneState) {
        if (!pane.rebindInFlight.compareAndSet(false, true)) return
        viewModelScope.launch(ioDispatcher) {
            try {
                val cellular = executor.acquireCellularFd()
                if (cellular == null) {
                    RemoteLogger.w("IsekaiTerminalSSH", "upstream failover: cellular fd not available, staying on current path")
                    return@launch
                }
                val (fd, localIp) = cellular
                RemoteLogger.i("IsekaiTerminalSSH", "upstream failover: rebinding to cellular (localIp=$localIp)")
                pane.session.rebindToFd(fd, localIp)
            } finally {
                pane.rebindInFlight.set(false)
            }
        }
    }

    // ── 接続 ─────────────────────────────────────────────────────────

    /** 未分割時は主ペイン、分割時はフォーカス中のペインを再接続する(後方互換。実体は[reconnectPane])。 */
    fun reconnect(tabId: String, password: String? = null, jumpPassword: String? = null) {
        val tab = tabOrNull(tabId) ?: return
        reconnectPane(tabId, tab.focusedPane.paneId, password, jumpPassword)
    }

    /** ペインを明示指定して再接続する。画面分割時、各ペインは自分自身の「再接続」ボタンを
     *  持つため(フォーカスに関わらず両ペインとも常に表示される)、こちらが実体。 */
    fun reconnectPane(tabId: String, paneId: String, password: String? = null, jumpPassword: String? = null) {
        val tab = tabOrNull(tabId) ?: return
        val pane = tab.paneOrNull(paneId) ?: return
        val profile = tab.profile ?: return
        connectionCoordinator.connectPane(tab.tabId, tab.currentTheme.value, pane, profile, password, jumpPassword)
    }

    private fun pushThemeToSession(pane: PaneState, theme: TerminalTheme) {
        theme.applyTo(pane.session::setTheme)
    }

    /**
     * このタブだけの配色テーマを明示的に変更する(Tab/session override)。分割中なら全ペインに
     * 反映する。以後このタブは[applyGlobalThemeToNonOverriddenTabs]の影響を受けなくなる。
     */
    fun setTabTheme(tabId: String, theme: TerminalTheme) {
        val tab = tabOrNull(tabId) ?: return
        tab.isThemeOverridden = true
        tab.currentTheme.value = theme
        tab.panes.forEach { pushThemeToSession(it, theme) }
    }

    /**
     * アプリ全体の既定テーマが変更された時に呼ぶ。まだタブ固有の上書きをしていない
     * ([TabState.isThemeOverridden] が false の)タブにだけそのまま反映する(分割中なら全ペインへ)。
     * MainActivity の ProfileListScreen 側テーマ変更コールバックから呼ばれる想定。
     */
    fun applyGlobalThemeToNonOverriddenTabs(theme: TerminalTheme) {
        _tabs.value.forEach { tab ->
            if (!tab.isThemeOverridden) {
                tab.currentTheme.value = theme
                tab.panes.forEach { pushThemeToSession(it, theme) }
            }
        }
    }

    // ── 定型コマンド（スニペット）─────────────────────────────────

    /** [profileId] が null なら全プロファイル共通のスニペットのみ、非nullなら共通＋専用をマージして読み込む。
     *  未分割時は主ペイン、分割時はフォーカス中のペインのスニペット一覧を差し替える。 */
    fun loadSnippets(tabId: String, profileId: Long?) {
        val tab = tabOrNull(tabId) ?: return
        loadSnippetsForPane(tab.focusedPane, profileId)
    }

    private fun loadSnippetsForPane(pane: PaneState, profileId: Long?) {
        viewModelScope.launch(ioDispatcher) {
            pane.snippets.value = Repositories.snippets.getForProfile(profileId)
        }
    }

    fun sendSnippetToPane(tabId: String, paneId: String, snippet: Snippet) {
        RemoteLogger.i("IsekaiTerminalSnippet", "send snippet '${snippet.label}' id=${snippet.id} tab=$tabId pane=$paneId")
        sendToPane(tabId, paneId, SnippetCommands.toBytes(snippet))
    }

    fun sendSnippet(tabId: String, snippet: Snippet) {
        RemoteLogger.i("IsekaiTerminalSnippet", "send snippet '${snippet.label}' id=${snippet.id} tab=$tabId")
        send(tabId, SnippetCommands.toBytes(snippet))
    }

    // ── 接続後自動実行コマンド ────────────────────────────────────
    // 発火(arm)は[ConnectionCoordinator.connectPane]側に移した(新しい接続試行のたびに
    // 呼ぶ必要があり、connect_*呼び出しと不可分なため)。ここに残る送信(fire)は
    // Connected遷移を監視する[observeConnectionTransitions]から呼ばれる別の関心事。

    /** Connected 立ち上がりで1回だけ呼ばれる。CAS でセッション単位の二重発火を防ぐ。
     *  常にこの[pane]自身のsessionへ直接送る(フォーカス中のペインへルーティングする[send]は
     *  使わない — 分割ペインが接続完了した時にフォーカスが主ペイン側にあると誤配送するため)。 */
    private fun maybeSendPostConnectCommands(pane: PaneState) {
        if (!pane.postConnectSent.compareAndSet(false, true)) return
        val bytes = pane.pendingPostConnectBytes ?: return
        viewModelScope.launch {
            delay(POST_CONNECT_DEBOUNCE_MS)
            RemoteLogger.i("IsekaiTerminalSSH", "sending post-connect commands (${bytes.size} bytes) pane=${pane.paneId}")
            pane.session.send(bytes)
        }
    }

    private fun paneOrNull(tabId: String, paneId: String): PaneState? = tabOrNull(tabId)?.paneOrNull(paneId)

    // ── セッション操作（ペイン指定が実体。タブ指定のものはフォーカス中のペインへの
    //    薄い委譲で、後方互換のため残してある）─────────────────────────
    // 画面分割時、両ペインは同時に見えるため「タブ指定」APIだけでは片方のペインの操作を
    // 表現できない(ステータスバーの再接続/切断/ログボタン・リサイズ・scrollback・キャンバスの
    // タップは常にそのペイン自身に向く)。そのためUI([TerminalHostScreen])は常にペイン指定
    // APIを使う。一方 trzsz転送シート・host key確認ダイアログ等の「フォーカス中のペインに
    // だけ表示する」モーダルUIも、実際にはフォーカスを持つペイン自身が自分のpaneIdを使って
    // これらのペイン指定APIを直接呼ぶ(hasFocus=falseの間はそもそも描画されないため、
    // 結果的にフォーカス中のペインだけが呼べる)。
    // 未分割タブでは常に主ペイン = フォーカス中のペインなので、タブ指定APIは既存の
    // 呼び出し元・テストと完全に同じ挙動になる。

    fun sendToPane(tabId: String, paneId: String, bytes: ByteArray) = paneOrNull(tabId, paneId)?.session?.send(bytes)

    fun send(tabId: String, bytes: ByteArray) = tabOrNull(tabId)?.let { sendToPane(tabId, it.focusedPane.paneId, bytes) }

    fun disconnectPane(tabId: String, paneId: String) = paneOrNull(tabId, paneId)?.session?.disconnect()

    /** 自動再接続ループ(isReconnecting中)を中止する。フォーカス中のペインに対して行う。 */
    fun cancelReconnectPane(tabId: String, paneId: String) = paneOrNull(tabId, paneId)?.session?.cancelReconnect()

    fun cancelReconnect(tabId: String) = tabOrNull(tabId)?.let { cancelReconnectPane(tabId, it.focusedPane.paneId) }

    /** #14: ユーザーが「今すぐWiFiに戻す」を要求した。判断はRust側(RebindManager)が行う。フォーカス中のペインに対して行う。 */
    fun forceReturnToWifi(tabId: String) =
        tabOrNull(tabId)?.let { paneOrNull(tabId, it.focusedPane.paneId)?.session?.forceReturnToWifi() }

    fun scrollbackCells(tabId: String, offset: Int, rows: Int): List<CellData>? =
        tabOrNull(tabId)?.let { scrollbackCellsForPane(tabId, it.focusedPane.paneId, offset, rows) }

    fun disconnect(tabId: String) = tabOrNull(tabId)?.let { disconnectPane(tabId, it.focusedPane.paneId) }

    // ── リサイズ・scrollback(フォーカスに関わらずペインごとに独立して呼ぶ必要がある)──

    fun resizePane(tabId: String, paneId: String, cols: UInt, rows: UInt) =
        paneOrNull(tabId, paneId)?.session?.resize(cols, rows)

    fun scrollbackCellsForPane(tabId: String, paneId: String, offset: Int, rows: Int): List<CellData>? =
        paneOrNull(tabId, paneId)?.session?.scrollbackCells(offset, rows)

    fun trustUpdatedHostKeyForPane(tabId: String, paneId: String) = paneOrNull(tabId, paneId)?.session?.trustUpdatedHostKey()

    fun trustUpdatedHostKey(tabId: String) = tabOrNull(tabId)?.let { trustUpdatedHostKeyForPane(tabId, it.focusedPane.paneId) }

    fun dismissHostKeyWarningForPane(tabId: String, paneId: String) = paneOrNull(tabId, paneId)?.session?.dismissHostKeyWarning()

    fun dismissHostKeyWarning(tabId: String) = tabOrNull(tabId)?.let { dismissHostKeyWarningForPane(tabId, it.focusedPane.paneId) }

    fun trustNewHostKeyForPane(tabId: String, paneId: String) = paneOrNull(tabId, paneId)?.session?.trustNewHostKey()

    fun trustNewHostKey(tabId: String) = tabOrNull(tabId)?.let { trustNewHostKeyForPane(tabId, it.focusedPane.paneId) }

    fun dismissNewHostKeyPromptForPane(tabId: String, paneId: String) =
        paneOrNull(tabId, paneId)?.session?.dismissNewHostKeyPrompt()

    fun dismissNewHostKeyPrompt(tabId: String) =
        tabOrNull(tabId)?.let { dismissNewHostKeyPromptForPane(tabId, it.focusedPane.paneId) }

    fun respondAgentSignRequestForPane(tabId: String, paneId: String, approved: Boolean) =
        paneOrNull(tabId, paneId)?.session?.respondAgentSignRequest(approved)

    fun respondAgentSignRequest(tabId: String, approved: Boolean) =
        tabOrNull(tabId)?.let { respondAgentSignRequestForPane(tabId, it.focusedPane.paneId, approved) }

    fun getSessionLogForPane(tabId: String, paneId: String): String = paneOrNull(tabId, paneId)?.session?.log?.value ?: ""

    fun getSessionLog(tabId: String): String =
        tabOrNull(tabId)?.let { getSessionLogForPane(tabId, it.focusedPane.paneId) } ?: ""

    fun clearSessionLog(tabId: String) = tabOrNull(tabId)?.focusedPane?.session?.clearLog()

    // ── trzsz（Android ファイル I/O は executor 経由。ペインごとに二重起動防止）───

    fun trzszStartUploadForPane(tabId: String, paneId: String, uri: Uri) {
        val pane = paneOrNull(tabId, paneId) ?: return
        if (pane.session.state.value.trzszState !is TrzszUiState.WaitingUser) return
        if (!pane.uploadInProgress.compareAndSet(false, true)) return
        viewModelScope.launch(ioDispatcher) {
            try {
                val file = executor.openUploadFile(uri) ?: return@launch
                pane.session.trzszAcceptUpload(file.name, file.size.toULong(), 0u)
                file.stream.use { inp ->
                    val buf = ByteArray(64 * 1024)
                    var pending: ByteArray? = null
                    while (true) {
                        val n = inp.read(buf)
                        if (n == -1) {
                            pane.session.trzszSendChunk(pending ?: ByteArray(0), true)
                            break
                        }
                        pending?.let { pane.session.trzszSendChunk(it, false) }
                        pending = buf.copyOf(n)
                    }
                }
            } catch (e: Exception) {
                RemoteLogger.e("TrzszUpload", "exception: $e")
            } finally {
                pane.uploadInProgress.set(false)
            }
        }
    }

    fun trzszStartUpload(tabId: String, uri: Uri) {
        val tab = tabOrNull(tabId) ?: return
        trzszStartUploadForPane(tabId, tab.focusedPane.paneId, uri)
    }

    fun trzszStartDownloadForPane(tabId: String, paneId: String) {
        val pane = paneOrNull(tabId, paneId) ?: return
        if (pane.session.state.value.trzszState !is TrzszUiState.WaitingUser) return
        pane.session.trzszAcceptDownload()
    }

    fun trzszStartDownload(tabId: String) {
        val tab = tabOrNull(tabId) ?: return
        trzszStartDownloadForPane(tabId, tab.focusedPane.paneId)
    }

    fun trzszCancelForPane(tabId: String, paneId: String) = paneOrNull(tabId, paneId)?.session?.trzszCancel()

    fun trzszCancel(tabId: String) = tabOrNull(tabId)?.let { trzszCancelForPane(tabId, it.focusedPane.paneId) }

    fun trzszDismissForPane(tabId: String, paneId: String) = paneOrNull(tabId, paneId)?.session?.trzszDismiss()

    fun trzszDismiss(tabId: String) = tabOrNull(tabId)?.let { trzszDismissForPane(tabId, it.focusedPane.paneId) }

    // ── ライフサイクル ──────────────────────────────────────────────

    override fun onCleared() {
        super.onCleared()
        RemoteLogger.i("IsekaiTerminalTabsVM", "TerminalTabsViewModel cleared")
        watchJobs.values.forEach { it.cancel() }
        _tabs.value.forEach { tab -> tab.panes.forEach { it.session.close() } }
        executor.unregisterNetworkCallbacks()
        executor.release()
    }
}
