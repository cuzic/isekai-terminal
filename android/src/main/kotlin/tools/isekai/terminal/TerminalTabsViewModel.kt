package tools.isekai.terminal

import android.app.Application
import android.net.Uri
import android.os.VibrationEffect
import android.os.Vibrator
import androidx.lifecycle.AndroidViewModel
import androidx.lifecycle.viewModelScope
import java.io.File
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
import tools.isekai.terminal.data.AuthType
import tools.isekai.terminal.data.ConnectionProfile
import tools.isekai.terminal.data.HostKeySettings
import tools.isekai.terminal.data.KeySequence
import tools.isekai.terminal.data.KeySequencePackInstallation
import tools.isekai.terminal.data.Repositories
import tools.isekai.terminal.data.Snippet
import tools.isekai.terminal.input.KeyStep
import tools.isekai.terminal.session.AndroidAppExecutor
import tools.isekai.terminal.session.AppExecutor
import tools.isekai.terminal.session.ReattachRecord
import tools.isekai.terminal.session.ReattachStateStore
import tools.isekai.terminal.session.RealHostKeyChecker
import tools.isekai.terminal.session.RebindFdSource
import tools.isekai.terminal.session.TerminalSession
import tools.isekai.terminal.ui.TerminalTheme
import tools.isekai.terminal.ui.TerminalThemes
import tools.isekai.terminal.ui.applyTo
import tools.isekai.terminal.util.RemoteLogger
import uniffi.isekai_terminal_core.CellData
import uniffi.isekai_terminal_core.ClipboardMimeKind
import uniffi.isekai_terminal_core.ClipboardPayload
import uniffi.isekai_terminal_core.PlatformFd
import uniffi.isekai_terminal_core.ScrollbackSearchMatch
import uniffi.isekai_terminal_core.reattachRecordIsFresh

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
 * 物理マルチパス fd 取得・upstream フェイルオーバー監視・WiFi/セルラー rebind fd 取得は、
 * いずれも [AppExecutor] が返す [AutoCloseable] ハンドル/[tools.isekai.terminal.session.RebindFdSource]
 * を [PaneState] が所有する設計にしており(Task #10)、複数タブ/split pane が同時に使っても
 * 互いを上書き・誤解放しない。
 */
/**
 * タブ内の2分割方向。[HORIZONTAL] は左右に並べる(縦の仕切り線)、[VERTICAL] は上下に並べる
 * (横の仕切り線)。画面分割(split pane)機能はまず2分割のみをサポートする(バイナリツリー式の
 * 多段分割は将来の拡張余地としてスコープ外にする)。
 */
enum class SplitDirection { HORIZONTAL, VERTICAL }

/** タブ横断で1つのペインを一意に指す座標(Task #13: tab-level/pane-level二重APIの統一)。 */
data class PaneAddress(val tabId: String, val paneId: String)

/**
 * タスク#14: 永続化された[ReattachRecord]が黙示的な自動再接続を試みるにあたってまだ新鮮か
 * どうかを判定するポリシー。本番実装は`reattach_persistence.rs`の`reattach_record_is_fresh`
 * (Rust側、rust-ssot準拠のポリシー判断)へそのまま委譲するだけの薄いラッパーであり、この
 * インターフェース自体はテストがネイティブ呼び出し無しに差し替えるためだけに存在する
 * ([TerminalTabsViewModel]のコンストラクタdoc参照)。
 */
fun interface ReattachFreshnessPolicy {
    fun isFresh(savedAtUnixSecs: Long, nowUnixSecs: Long): Boolean
}

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
    /** このpaneのセッションと同じ寿命を持つWiFi/セルラーfd取得元。session終了時に`close()`する。 */
    internal val rebindFdSource: RebindFdSource,
) {
    // 接続前のバリデーションエラー。session.state (Rust 由来) には混入させず合成する。
    internal val preConnectError = MutableStateFlow<String?>(null)
    // trzsz アップロードの二重起動防止 (Bug 2 と同種のガード。ペインごとに独立させる)。
    internal val uploadInProgress = AtomicBoolean(false)

    // ── 定型コマンド（スニペット）─────────────────────────────
    internal val snippets = MutableStateFlow<List<Snippet>>(emptyList())

    // ── 打鍵列（KeySequence）───────────────────────────────
    internal val keySequences = MutableStateFlow<List<KeySequence>>(emptyList())

    // ── 打鍵列セット(パック) ──────────────────────────────
    // 有効化されているパックのみ(pack定義, 解決済みinstallation)のペアで保持する。
    internal val installedPacks =
        MutableStateFlow<List<Pair<tools.isekai.terminal.pack.KeySequencePack, KeySequencePackInstallation>>>(emptyList())

    // ── 接続後自動実行コマンド ────────────────────────────────
    internal var pendingPostConnectBytes: ByteArray? = null
    internal val postConnectSent = AtomicBoolean(true)

    // ── upstream フェイルオーバー ────────────────────────────
    internal var upstreamFailoverEnabledForCurrentSession = false
    internal val rebindInFlight = AtomicBoolean(false)

    // ── Task #10: per-pane handle所有権(後勝ちバグ修正) ─────────
    /** 物理マルチパスfd取得のhandle。接続試行のたびに古いhandleを閉じてから発行し直す。 */
    internal var physicalMultipathHandle: AutoCloseable? = null
    /** upstream failover監視のhandle。 */
    internal var upstreamFailoverMonitorHandle: AutoCloseable? = null

    /** UI が購読する合成済み状態。 */
    val uiState: Flow<TerminalUiState> = session.state.combine(preConnectError) { s, err ->
        if (err != null) s.copy(statusMsg = err) else s
    }
}

class TerminalTabsViewModel(
    app: Application,
    private val executor: AppExecutor,
    private val sessionFactory: (AppExecutor, RebindFdSource) -> TerminalSession,
    // テストがtestScheduler駆動のディスパッチャーを注入できるようにする(既定は本番同様
    // Dispatchers.IO)。ハードコードしていた頃はテストの仮想時間(TestCoroutineScheduler)と
    // ここで起動される実スレッドの完了タイミングが競合し、withTimeout()ポーリングが
    // 断続的にタイムアウトする原因になっていた。
    private val ioDispatcher: CoroutineDispatcher = Dispatchers.IO,
    // タスク#14: プロセスkillからの黙示的セッション再アタッチ用の永続化ストア(ファイルベース、
    // 設計判断は[ReattachStateStore]のdoc参照)。テストは専用の一時ファイルを指す
    // インスタンスを注入できる。
    private val reattachStore: ReattachStateStore = ReattachStateStore(File(app.filesDir, REATTACH_STATE_FILE_NAME)),
    // タスク#14: 「新鮮さ」の判定(既定はRust側`reattach_record_is_fresh`への委譲、
    // rust-ssot準拠)。JVM単体テスト(Robolectric)はAndroid NDK向けにビルドされたネイティブ
    // ライブラリをロードできない(UnsatisfiedLinkError)ため、UniFFI free functionを
    // 直接呼ぶ本番実装をテストから差し替え可能にしておく必要がある——`FakeOrchestrator`が
    // `SessionOrchestratorInterface`を経由して同じ問題を解決しているのと同じ構成
    // (`FakeSshGateway.kt`のdocコメント「実Rust側のConnPhaseを模した最小限の状態」参照)。
    private val reattachFreshnessPolicy: ReattachFreshnessPolicy = ReattachFreshnessPolicy { savedAtUnixSecs, nowUnixSecs ->
        reattachRecordIsFresh(savedAtUnixSecs.toULong(), nowUnixSecs.toULong())
    },
) : AndroidViewModel(app) {

    /** 本番用コンストラクタ。Compose の viewModel() から呼ばれる。
     *  [sessionFactory] は`executor`を引数で受け取る形にしている
     *  ([acquireWifiFd]/[acquireCellularFd]で同じ`executor`インスタンスを再利用するため
     *  ——セカンダリコンストラクタの`this(...)`委譲の中では`this.executor`(未初期化)を
     *  参照できないので、`AndroidAppExecutor(app)`を二重生成せずに済むようにする)。 */
    constructor(app: Application) : this(
        app,
        AndroidAppExecutor(app),
        { executor, rebindFdSource ->
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
                // このpane用のRebindFdSource経由で取得して返すだけ(判断はしない、rust-ssot.md準拠)。
                // Rust側のspawn_blockingスレッドから同期呼び出しされるためrunBlockingで
                // suspend関数をブリッジする(onAgentSignRequest等と同じ方式)。
                acquireWifiFd = {
                    runBlocking { rebindFdSource.acquireWifiFd() }?.let { (fd, ip) -> PlatformFd(fd, ip) }
                },
                acquireCellularFd = {
                    runBlocking { rebindFdSource.acquireCellularFd() }?.let { (fd, ip) -> PlatformFd(fd, ip) }
                },
                // #25: 端末ベル(BEL)受信時の触覚フィードバック。判断(取りこぼし無く1回だけ
                // 発火させる`bell_generation`の単調増加チェック)は[TerminalSession]側で
                // 完結しており、ここでは実際にバイブレーションを鳴らすだけ(rust-ssot.md、
                // `onClipboardWriteRequested`と同じ構成)。振動できないデバイス/権限が
                // 無い場合は`vibrator`が非nullでも`hasVibrator()`がfalseになりうるが、
                // `vibrate()`自体は黙って無視されるだけなので個別ハンドリング不要。
                onBell = {
                    val vibrator = app.getSystemService(Vibrator::class.java)
                    vibrator?.vibrate(VibrationEffect.createOneShot(150, VibrationEffect.DEFAULT_AMPLITUDE))
                },
            )
        },
    )

    companion object {
        // Connected 直後は取りこぼし防止のため少し待ってから自動実行コマンドを送る。
        private const val POST_CONNECT_DEBOUNCE_MS = 400L

        // タスク#14: [ReattachStateStore]の既定の永続化先ファイル名(`context.filesDir`直下)。
        private const val REATTACH_STATE_FILE_NAME = "reattach_state.json"
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
        internal val keySequences get() = primaryPane.keySequences
        internal val installedPacks get() = primaryPane.installedPacks
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
        loadPaneContent = { pane, profileId ->
            loadSnippetsForPane(pane, profileId)
            loadKeySequencesForPane(pane, profileId)
            loadInstalledPacksForPane(pane, profileId)
        },
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
        // タスク#14: このViewModelはプロセス寿命にスコープされた(Applicationスコープの)
        // シングルトンなので(クラスdoc参照)、このinitブロックはプロセスが新規に起動した
        // 時にちょうど1回だけ走る——「前回のプロセスがkillされる直前に開いていたタブを
        // 黙示的に復元する」タイミングとして自然に一致する。
        restorePersistedReattachTabs()
    }

    /**
     * タスク#14: 前回のプロセスで開かれていたタブを、[ReattachStateStore]に永続化された
     * 記録から黙示的に(ユーザー操作無しで)復元する。「新鮮さ」の判定は
     * `reattach_persistence.rs`(Rust側、`.claude/rules/rust-ssot.md`に従いポリシー判断を
     * 一元化)に委譲する。復元は常に**通常の新規接続**([openTab]、新しいSessionIdでの
     * 通常ATTACH)であり、isekai-pipeのワイヤーレベルRESUMEを再利用するものではない
     * (`ReattachStateStore`のdoc・`reattach_persistence.rs`のモジュールdoc参照: プロセス
     * kill後はSSHクライアントの暗号状態が失われているため、ワイヤーレベルRESUMEの再利用は
     * 原理的に成立しない)。
     *
     * パスワード認証のプロファイルは対話プロンプト無しでは復元できないため対象外にする
     * (`.claude/rules/always-connects.md`が認める「本質的に自動化できないケース」の一種)。
     */
    private fun restorePersistedReattachTabs() {
        viewModelScope.launch(ioDispatcher) {
            val records = reattachStore.load()
            if (records.isEmpty()) return@launch
            // 復元後は全レコードを新しいタブIDで作り直す([openTab]が[persistReattachRecord]
            // 経由で新しいレコードを書き戻す)ため、古いレコードは先にまとめて捨てる。
            reattachStore.clear()
            val nowUnixSecs = System.currentTimeMillis() / 1000L
            for (record in records) {
                if (!reattachFreshnessPolicy.isFresh(record.savedAtUnixSecs, nowUnixSecs)) {
                    RemoteLogger.i(
                        "IsekaiTerminalReattach",
                        "discarding stale reattach record for '${record.label}' " +
                            "(savedAt=${record.savedAtUnixSecs}, now=$nowUnixSecs)",
                    )
                    continue
                }
                val profile = Repositories.profiles.findById(record.profileId)
                if (profile == null) {
                    RemoteLogger.i(
                        "IsekaiTerminalReattach",
                        "reattach record '${record.label}' refers to a deleted profile, skipping",
                    )
                    continue
                }
                if (profile.authTypeEnum != AuthType.KEY) {
                    RemoteLogger.i(
                        "IsekaiTerminalReattach",
                        "'${profile.label}' uses password auth, skipping implicit reattach",
                    )
                    continue
                }
                RemoteLogger.i("IsekaiTerminalReattach", "implicitly reattaching '${profile.label}' after process restart")
                openTab(profile)
            }
        }
    }

    /** [profile]がKEY認証の場合のみ、[tabId]の[ReattachRecord]を永続化(更新)する。
     *  パスワード認証は黙示的復元の対象外なので([restorePersistedReattachTabs]参照)、
     *  そもそも記録する意味が無く保存しない。 */
    private fun persistReattachRecord(tabId: String, profile: ConnectionProfile) {
        if (profile.authTypeEnum != AuthType.KEY) return
        viewModelScope.launch(ioDispatcher) {
            reattachStore.upsert(
                ReattachRecord(
                    tabId = tabId,
                    profileId = profile.id,
                    label = profile.label,
                    reattachToken = UUID.randomUUID().toString(),
                    savedAtUnixSecs = System.currentTimeMillis() / 1000L,
                ),
            )
        }
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
        val rebindFdSource = executor.createRebindFdSource()
        val primaryPane = PaneState(UUID.randomUUID().toString(), sessionFactory(executor, rebindFdSource), rebindFdSource)
        // Phase 12 P2-1: Global default → Profile default の解決。プロファイルに明示的な
        // テーマ指定があれば、その時点で「上書き済み」タブとして扱う(以後グローバル変更に
        // 追従しない。ユーザーがそのプロファイル用に選んだ意図を尊重する)。
        val profileTheme = profile.themeName?.let { TerminalThemes.byName(it) }
        val initialTheme = profileTheme ?: currentGlobalTheme()
        val tab = TabState(tabId, primaryPane, profile, profile.label, initialTheme, initialThemeIsOverridden = profileTheme != null)

        RemoteLogger.i("IsekaiTerminalTabsVM", "openTab '${profile.label}' id=$tabId")
        _tabs.update { it + tab }
        _activeTabId.value = tabId
        persistReattachRecord(tabId, profile)

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
        // タスク#14: ユーザーが明示的に閉じたタブは、次回プロセス起動時に黙示的復元の
        // 対象にしない(再オープンを望んでいないはずのため)。
        viewModelScope.launch(ioDispatcher) { reattachStore.remove(tabId) }
        updateSessionsSummary()
    }

    /** [pane] の監視コルーチンを止め、セッションを切断・破棄し、保有する全handleを解放する
     *  （[closeTab]・[closeSplitPane]・[onCleared] 共通）。 */
    private fun closePaneSession(pane: PaneState) {
        pane.session.disconnect()
        pane.session.close()
        watchJobs.remove(pane.paneId)?.cancel()
        pane.physicalMultipathHandle?.close()
        pane.physicalMultipathHandle = null
        pane.upstreamFailoverMonitorHandle?.close()
        pane.upstreamFailoverMonitorHandle = null
        pane.rebindFdSource.close()
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
        val rebindFdSource = executor.createRebindFdSource()
        val pane = PaneState(UUID.randomUUID().toString(), sessionFactory(executor, rebindFdSource), rebindFdSource)
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
    fun setFocusedPane(address: PaneAddress) {
        tabOrNull(address.tabId)?.setFocusedPane(address.paneId)
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
            launch { observeConnectionTransitions(tab, pane) }
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

    private suspend fun observeConnectionTransitions(tab: TabState, pane: PaneState) {
        var prevConnected = false
        pane.uiState.collect { state ->
            val connected = state.connected
            if (connected && !prevConnected) {
                executor.notifyConnected(state.currentHost ?: "")
                if (pane.upstreamFailoverEnabledForCurrentSession) {
                    pane.upstreamFailoverMonitorHandle = executor.registerUpstreamFailoverMonitor { onWifiUpstreamBroken(pane) }
                }
                maybeSendPostConnectCommands(pane)
                // タスク#14: 「直近まで生きていたセッション」の記録を、Connectedへ
                // 遷移するたびに新しい保存時刻で更新する。タブを開いた瞬間の時刻だけを
                // 使うと、長時間接続し続けたセッションが(一度もネットワーク瞬断による
                // 再接続を経験しないまま)猶予期間を過ぎて「古い」と誤判定されうるため
                // (`reattach_persistence.rs`の`AUTO_REATTACH_GRACE_SECS`参照)。
                tab.profile?.let { persistReattachRecord(tab.tabId, it) }
            } else if (!connected && prevConnected) {
                executor.notifyDisconnected()
                pane.physicalMultipathHandle?.close()
                pane.physicalMultipathHandle = null
                pane.upstreamFailoverMonitorHandle?.close()
                pane.upstreamFailoverMonitorHandle = null
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
                val cellular = pane.rebindFdSource.acquireCellularFd()
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

    /** ペインを明示指定して再接続する。画面分割時、各ペインは自分自身の「再接続」ボタンを
     *  持つため(フォーカスに関わらず両ペインとも常に表示される)。 */
    fun reconnectPane(address: PaneAddress, password: String? = null, jumpPassword: String? = null) {
        val tab = tabOrNull(address.tabId) ?: return
        val pane = tab.paneOrNull(address.paneId) ?: return
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

    private fun loadSnippetsForPane(pane: PaneState, profileId: Long?) {
        viewModelScope.launch(ioDispatcher) {
            pane.snippets.value = Repositories.snippets.getForProfile(profileId)
        }
    }

    // ── 打鍵列（KeySequence）─────────────────────────────────────

    private fun loadKeySequencesForPane(pane: PaneState, profileId: Long?) {
        viewModelScope.launch(ioDispatcher) {
            pane.keySequences.value = Repositories.keySequences.getForProfile(profileId)
        }
    }

    // ── 打鍵列セット(パック) ──────────────────────────────

    private fun loadInstalledPacksForPane(pane: PaneState, profileId: Long?) {
        viewModelScope.launch(ioDispatcher) {
            pane.installedPacks.value = tools.isekai.terminal.pack.KeySequencePacks.ALL.mapNotNull { pack ->
                Repositories.keySequencePackInstallations.resolveInstallation(pack.id, profileId)?.let { pack to it }
            }
        }
    }

    fun sendSnippetToPane(address: PaneAddress, snippet: Snippet) {
        RemoteLogger.i("IsekaiTerminalSnippet", "send snippet '${snippet.label}' id=${snippet.id} tab=${address.tabId} pane=${address.paneId}")
        sendToPane(address, SnippetCommands.toBytes(snippet))
    }

    // ── 打鍵列(KeySequence) ────────────────────────────────────
    // applicationCursorMode は新しいミラー状態を作らず、既存の Rust 由来の状態
    // (pane.session.state.value.screenUpdate、TerminalScreen が矢印キー描画等で参照している
    // のと同じ値)をそのまま読む。

    fun sendKeySequenceToPane(address: PaneAddress, steps: List<KeyStep>) {
        val pane = paneOrNull(address) ?: return
        val screenUpdate = pane.session.state.value.screenUpdate
        val applicationCursorMode = screenUpdate?.applicationCursorMode ?: false
        // DECKPAM/DECKPNM(タスク#43)。テンキーのKeyStep.Specialを含む打鍵列でも、物理
        // キーボード経由と同じくRust由来の現在のkeypad modeに従わせる(codexレビュー指摘:
        // 未伝播だとテンキーを含む打鍵列が常にnumeric modeとして送信されてしまっていた)。
        val applicationKeypadMode = screenUpdate?.applicationKeypadMode ?: false
        val kittyKeyboardFlags = screenUpdate?.kittyKeyboardFlags ?: 0u
        RemoteLogger.i("IsekaiTerminalKeySequence", "send key sequence (${steps.size} steps) tab=${address.tabId} pane=${address.paneId}")
        pane.session.send(KeySequenceCommands.toBytes(steps, applicationCursorMode, applicationKeypadMode, kittyKeyboardFlags))
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

    private fun paneOrNull(address: PaneAddress): PaneState? = tabOrNull(address.tabId)?.paneOrNull(address.paneId)

    /** [address]が指すpaneが存在すれば[block]を実行してその結果を返す。存在しなければnull。 */
    private fun <T> withPane(address: PaneAddress, block: (PaneState) -> T): T? = paneOrNull(address)?.let(block)

    // ── セッション操作(すべてPaneAddress指定。Task #13でtab-level互換APIは削除した)──
    // 画面分割時、両ペインは同時に見えるため「タブ指定」だけでは片方のペインの操作を
    // 表現できない(ステータスバーの再接続/切断/ログボタン・リサイズ・scrollback・キャンバスの
    // タップは常にそのペイン自身に向く)。UI([TerminalHostScreen])は常にペイン指定APIを使う。

    fun sendToPane(address: PaneAddress, bytes: ByteArray) = withPane(address) { it.session.send(bytes) }

    fun disconnectPane(address: PaneAddress) = withPane(address) { it.session.disconnect() }

    /** 自動再接続ループ(isReconnecting中)を中止する。 */
    fun cancelReconnectPane(address: PaneAddress) = withPane(address) { it.session.cancelReconnect() }

    fun resizePane(address: PaneAddress, cols: UInt, rows: UInt) = withPane(address) { it.session.resize(cols, rows) }

    fun scrollbackCellsForPane(address: PaneAddress, offset: Int, rows: Int): List<CellData>? =
        withPane(address) { it.session.scrollbackCells(offset, rows) }

    /** タスク#66: スクロールバック検索。対象ペインが無ければ(withPaneがnullを返す
     *  場合)空リストを返す——[TerminalSession.searchScrollback]自体の「未接続時は空
     *  リスト」という契約と揃える。 */
    fun searchScrollbackForPane(address: PaneAddress, query: String, caseSensitive: Boolean): List<ScrollbackSearchMatch> =
        withPane(address) { it.session.searchScrollback(query, caseSensitive) } ?: emptyList()

    /** タスク#13(OSC 133)「前のプロンプトへジャンプ」。既存のスクロールバック検索
     *  ([searchScrollbackForPane])とは独立した機能。 */
    fun jumpToPreviousPromptForPane(address: PaneAddress, fromScrollOffset: Int, fromShowingScrollback: Boolean) =
        withPane(address) { it.session.jumpToPreviousPrompt(fromScrollOffset, fromShowingScrollback) }

    fun jumpToNextPromptForPane(address: PaneAddress, fromScrollOffset: Int, fromShowingScrollback: Boolean) =
        withPane(address) { it.session.jumpToNextPrompt(fromScrollOffset, fromShowingScrollback) }

    fun clickToPromptCursorForPane(address: PaneAddress, row: Int, col: Int) =
        withPane(address) { it.session.clickToPromptCursor(row, col) }

    fun copyLastCommandOutputForPane(address: PaneAddress) =
        withPane(address) { it.session.copyLastCommandOutput() }

    fun trustUpdatedHostKeyForPane(address: PaneAddress) = withPane(address) { it.session.trustUpdatedHostKey() }

    fun dismissHostKeyWarningForPane(address: PaneAddress) = withPane(address) { it.session.dismissHostKeyWarning() }

    fun trustNewHostKeyForPane(address: PaneAddress) = withPane(address) { it.session.trustNewHostKey() }

    fun dismissNewHostKeyPromptForPane(address: PaneAddress) = withPane(address) { it.session.dismissNewHostKeyPrompt() }

    fun respondAgentSignRequestForPane(address: PaneAddress, approved: Boolean) =
        withPane(address) { it.session.respondAgentSignRequest(approved) }

    fun getSessionLogForPane(address: PaneAddress): String = withPane(address) { it.session.log.value } ?: ""

    // ── trzsz（Android ファイル I/O は executor 経由。ペインごとに二重起動防止）───

    fun trzszStartUploadForPane(address: PaneAddress, uri: Uri) {
        val pane = paneOrNull(address) ?: return
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

    fun trzszStartDownloadForPane(address: PaneAddress) {
        val pane = paneOrNull(address) ?: return
        if (pane.session.state.value.trzszState !is TrzszUiState.WaitingUser) return
        pane.session.trzszAcceptDownload()
    }

    fun trzszCancelForPane(address: PaneAddress) = withPane(address) { it.session.trzszCancel() }

    fun trzszDismissForPane(address: PaneAddress) = withPane(address) { it.session.trzszDismiss() }

    // ── ライフサイクル ──────────────────────────────────────────────

    override fun onCleared() {
        super.onCleared()
        RemoteLogger.i("IsekaiTerminalTabsVM", "TerminalTabsViewModel cleared")
        _tabs.value.forEach { tab -> tab.panes.forEach { closePaneSession(it) } }
        executor.unregisterNetworkCallbacks()
        executor.release()
    }
}
