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
import tools.isekai.terminal.data.BatteryGuidanceSettings
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
import tools.isekai.terminal.util.BatteryOptimization
import tools.isekai.terminal.util.ClientIdentity
import tools.isekai.terminal.util.RemoteLogger
import uniffi.isekai_terminal_core.BackgroundKillFacts
import uniffi.isekai_terminal_core.ClipboardMimeKind
import uniffi.isekai_terminal_core.ClipboardPayload
import uniffi.isekai_terminal_core.PlatformFd
import uniffi.isekai_terminal_core.ScrollbackSearchMatch
import uniffi.isekai_terminal_core.decideBatteryGuidance
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
 * 項目2(OEMバッテリー最適化への案内UI): 「予期しないkillの事実」から案内ダイアログを
 * 表示すべきかを判定するポリシー。本番実装はRust側`background_reliability_policy.rs`の
 * `decide_battery_guidance`(rust-ssot準拠のポリシー判断)へそのまま委譲するだけの
 * 薄いラッパーであり、この関数型インターフェース自体は[ReattachFreshnessPolicy]と同じ
 * 理由でテストがネイティブ呼び出し無しに差し替えるためだけに存在する。
 */
fun interface BatteryGuidancePolicy {
    fun shouldShow(facts: BackgroundKillFacts): Boolean
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
    private val sessionFactory: (AppExecutor, RebindFdSource, ConnectionProfile) -> TerminalSession,
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
    // 項目2: 「案内すべきか」の判定(既定はRust側`decide_battery_guidance`への委譲、
    // rust-ssot準拠)。[reattachFreshnessPolicy]と全く同じ理由でテストから差し替え可能に
    // しておく。
    private val batteryGuidancePolicy: BatteryGuidancePolicy = BatteryGuidancePolicy { facts ->
        decideBatteryGuidance(facts).shouldShow
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
        { executor, rebindFdSource, profile ->
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
                // `AI_INTEGRATION_DESIGN.md` §6.1/§11.1.4: ctlソケット経由のAI/汎用の
                // 注目通知(kindが`Waiting`/`Done`/`Info`)。判断(取りこぼし無く1回だけ
                // 発火させる`notifyGeneration`の単調増加チェック)は[TerminalSession]側で
                // 完結しており、ここでは実際の副作用注入のみを行う(`onBell`と同じ構成)。
                // 2026-07-25、tmux hook系kind(下の`onNotifyRequested`)と全く同じ
                // `TabAlertNotifier`経路へ配線した(バックグラウンドシステム通知)——
                // ただしこちらはRust側にフォアグラウンド/タブフォーカス抑制が無いため、
                // 該当タブを表示中でも通知が出うる(既知の差異、`TabAlertNotifier`の
                // クラスdocコメント参照)。「状態ドット」・通知タップでのタブジャンプは
                // tmux系kindも含めまだ未実装。
                onNotify = { kind, title, body ->
                    RemoteLogger.i("IsekaiTerminalNotify", "[$kind] $title: $body")
                    TabAlertNotifier.notify(
                        context = app,
                        tabId = profile.id.toString(),
                        profileLabel = profile.label,
                        kind = kind,
                        enabled = profile.enableTabNotifications,
                        message = title to body,
                    )
                },
                // タスク#57: tmux hook発火(kindが`Bell`/`Activity`/`Silence`/`JobDone`)。
                // 「見せるべきか」の判断はRust側で済んでおり、ここでは(a) このプロファイルの
                // opt-in設定、(b) 通知権限、の2つの確認と実際のpostだけを行う
                // (`TabAlertNotifier`参照)。通知は同じプロファイルの複数タブで1枠に集約する
                // (profile.idをキーにする、`rust-ssot.md`の対象外——UI表示上の判断)。
                onNotifyRequested = { kind ->
                    TabAlertNotifier.notify(
                        context = app,
                        tabId = profile.id.toString(),
                        profileLabel = profile.label,
                        kind = kind,
                        enabled = profile.enableTabNotifications,
                    )
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
     * 画面分割(split pane)導入前は「1タブ=1セッション」だった名残の後方互換プロパティ
     * (`session`/`preConnectError`/`uploadInProgress`/`snippets`/`keySequences`/
     * `installedPacks`/`pendingPostConnectBytes`/`postConnectSent`/
     * `upstreamFailoverEnabledForCurrentSession`)は、本番コードの呼び出し元が全て既に
     * [PaneState]を直接参照するよう置き換わっていたため削除した(セッション操作は
     * `pane: PaneState`を直接扱う設計に統一、`rust-ssot.md`と同じ「状態を複製しない」
     * 原則の一部)。
     */
    class TabState internal constructor(
        val tabId: String,
        internal val primaryPane: PaneState,
        val profile: ConnectionProfile?,
        val label: String,
        initialTheme: TerminalTheme,
        initialThemeIsOverridden: Boolean,
    ) {
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

        // ── tmux session group / ウィンドウ紐付け(タスク#60、primary paneのみ)──
        // Rust側(`SessionOrchestrator.ensureTmuxTabWindow`)が解決したウィンドウ情報の
        // うち、UIへ最小限反映してよい表示用ラベル(例: "win 2")だけを持つ。判断は
        // 一切ここで行わない(`.claude/rules/rust-ssot.md`) — 常にRustが返した値の
        // 素通しであり、Kotlin側で解釈・分岐はしない。まだ解決前(接続直後や
        // opportunisticな失敗時)はnullのままで、その場合はタブラベルに何も追加しない。
        internal val tmuxWindowLabel = MutableStateFlow<String?>(null)

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

    /**
     * [maybeEnsureTmuxTabWindow]の「同一プロファイルへの二重tmux連携」防止用。
     * `_tabs.value`を見た`tmuxWindowLabel != null`チェック(TOCTOU: 非同期RPCが
     * 完了して`tmuxWindowLabel`が書かれるまでの間は他のタブから見えない)だけでは
     * 同一プロファイルの2タブがほぼ同時に`connected`へ遷移した場合に両方すり抜け、
     * 互いに異なる`SessionOrchestrator`(=異なる`AppPaneId`)から同時に
     * `ensureTmuxTabWindow`を呼んでしまう(実機検証、2026-07-27。tmuxウィンドウの
     * 奪い合い自体に加え、`isekai-pipe ctl notify`が書き込むctl-socketパスの
     * 登録先app_pane_idと実際に接続しているapp_pane_idが食い違い、
     * `@isekai_ctl_sock`が永久に正しいウィンドウへ届かなくなる二次被害があった)。
     * `putIfAbsent`でコルーチン起動前に同期的に「予約」し、RPCが失敗した場合のみ
     * 解放して別タブに再挑戦の機会を残す。
     */
    private val tmuxClaimedProfileIds = java.util.concurrent.ConcurrentHashMap.newKeySet<Long>()

    private val _activeTabId = MutableStateFlow<String?>(null)
    val activeTabId: StateFlow<String?> = _activeTabId.asStateFlow()

    /** 項目2: コールドスタート時にOEMバッテリー最適化の案内ダイアログを自動表示すべきか。
     *  [MainActivity.AppRoot]がNavHost外で購読する(タスク#14の黙示的再アタッチによる
     *  自動画面遷移とは独立させるため)。 */
    private val _showBatteryGuidance = MutableStateFlow(false)
    val showBatteryGuidance: StateFlow<Boolean> = _showBatteryGuidance.asStateFlow()

    /** [showBatteryGuidance]を消費してダイアログを閉じる。「設定を開く」「閉じる」
     *  いずれを選んだ場合も呼ばれる(このセッション中の自動表示は最大1回)。 */
    fun dismissBatteryGuidance() {
        _showBatteryGuidance.value = false
    }

    // タブごとの監視コルーチン（通知集約の再計算・ダウンロード完了ファンアウト・接続状態遷移）。closeTab で cancel する。
    private val watchJobs = mutableMapOf<String, Job>()

    // トランスポート別connect_*呼び出しへの分岐・認証解決(Task #8 段階1でTerminalTabsViewModel
    // から切り出した)。テーマ反映・スニペット読み込みはこのViewModel側の責務のままコールバックで渡す。
    private val connectionCoordinator = ConnectionCoordinator(
        executor = executor,
        scope = viewModelScope,
        ioDispatcher = ioDispatcher,
        pushTheme = ::pushThemeToSession,
        loadPaneContent = ::loadPaneContent,
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
        // 実機検証(2026-07-28)でこれが未配線だったため、tmux通知(enableTabNotifications)が
        // バックグラウンド化しても常に抑制され続けるバグがあった(SessionOrchestrator側の
        // app_foregroundが起動時のtrueから変わらないため)。
        executor.registerLifecycleCallbacks(
            onBackground = { onAppBackgrounded() },
            onForeground = { onAppForegrounded() },
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
            val nowUnixSecs = System.currentTimeMillis() / 1000L
            // 項目2: 「新鮮なreattachレコードがあったか」を、このレコードが復元処理で
            // 消費される前に判定材料として使う(実際のclear()より先に呼ぶ必要はないが、
            // 意味的にこの位置が自然)。
            checkBatteryGuidance(
                hasFreshReattachRecord = records.any { reattachFreshnessPolicy.isFresh(it.savedAtUnixSecs, nowUnixSecs) },
            )
            if (records.isEmpty()) return@launch
            // 復元後は全レコードを新しいタブIDで作り直す([openTab]が[persistReattachRecord]
            // 経由で新しいレコードを書き戻す)ため、古いレコードは先にまとめて捨てる。
            reattachStore.clear()
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

    /**
     * 項目2(OEMバッテリー最適化への案内UI): 「新鮮なreattachレコードあり &&
     * clean-shutdownマーカー無し」を1回の予期しないkillとしてカウントし、Rust側の
     * `decide_battery_guidance`([batteryGuidancePolicy]経由)へ生の事実を渡して
     * 案内すべきか判断する。切断回数はトリガーにしない(ネットワーク起因の切断と
     * OEMによるプロセスkillは別事象でノイズが多すぎるため、`PLAN.md`参照)。
     *
     * [hasFreshReattachRecord]が`false`(新鮮なレコードが1件も無い)場合は「今回の
     * プロセス起動が予期しないkillからのものかどうか」を判定する材料が無いため、
     * カウンタの増減も判断も一切行わない——ただし[TerminalSessionService.
     * consumeCleanShutdownMarker]の呼び出しだけは常に行う(呼ばないとdirty-bitが
     * リセットされず、次回起動時に2世代前の"clean"痕跡を誤って読んでしまう、
     * `TerminalSessionService`のdoc参照)。
     */
    private fun checkBatteryGuidance(hasFreshReattachRecord: Boolean) {
        val context = getApplication<Application>()
        val wasCleanShutdown = TerminalSessionService.consumeCleanShutdownMarker(context)
        if (!hasFreshReattachRecord || wasCleanShutdown) return

        RemoteLogger.i(
            "IsekaiTerminalBattery",
            "unexpected process kill detected (fresh reattach record found without a clean-shutdown marker)",
        )
        val unexpectedKillCount = BatteryGuidanceSettings.incrementUnexpectedKillCount(context)

        val nowUnixSecs = System.currentTimeMillis() / 1000L
        val facts = BackgroundKillFacts(
            unexpectedKillCount = unexpectedKillCount.toUInt(),
            lastShownUnixSecs = BatteryGuidanceSettings.lastShownUnixSecs(context)?.toULong(),
            nowUnixSecs = nowUnixSecs.toULong(),
            isIgnoringBatteryOptimizations = BatteryOptimization.isIgnoringBatteryOptimizations(context),
            userOptedOut = BatteryGuidanceSettings.isOptedOut(context),
        )
        if (batteryGuidancePolicy.shouldShow(facts)) {
            BatteryGuidanceSettings.markShownNow(context, nowUnixSecs)
            _showBatteryGuidance.value = true
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

    /** 全タブ・全ペイン(画面分割side含む)を横断して[block]を実行する。プラットフォーム
     *  からの生イベント(ネットワーク経路変化・前景/背景遷移等)をRustへそのまま転送する
     *  ファンアウト処理が複数箇所で同じ`_tabs.value.flatMap { it.panes }.forEach`の
     *  形をしていたため、この局所ヘルパーへ切り出した。 */
    private inline fun forEachPane(block: (PaneState) -> Unit) {
        _tabs.value.flatMap { it.panes }.forEach(block)
    }

    /** internal にすることでテストから直接呼べる。split pane側にも同じ生イベントを転送する。 */
    internal fun onNetworkPathChanged(isSatisfied: Boolean) {
        forEachPane { pane -> forwardToRust("notifyNetworkPathChanged") { pane.session.notifyNetworkPathChanged(isSatisfied) } }
    }

    // ── アプリ全体のフォアグラウンド/バックグラウンド（全タブへファンアウト）──────

    /** [openTab]/[splitPane]が新規セッションを作った直後に現在の前景/背景状態を
     *  適用するための保持値(実機検証2026-07-28)。バックグラウンド中に新規セッションが
     *  作られた場合、Rust側の初期値`app_foreground=true`のまま取り残されるのを防ぐ。 */
    @Volatile private var appInForeground = true

    /** internal にすることでテストから直接呼べる。split pane側にも同じ生イベントを転送する。 */
    internal fun onAppBackgrounded() {
        appInForeground = false
        forEachPane { pane -> forwardToRust("notifyDidEnterBackground") { pane.session.notifyDidEnterBackground() } }
    }

    /** internal にすることでテストから直接呼べる。split pane側にも同じ生イベントを転送する。 */
    internal fun onAppForegrounded() {
        appInForeground = true
        forEachPane { pane -> forwardToRust("notifyWillEnterForeground") { pane.session.notifyWillEnterForeground() } }
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
        val primaryPane = PaneState(UUID.randomUUID().toString(), sessionFactory(executor, rebindFdSource, profile), rebindFdSource)
        // バックグラウンド中に新規タブが開かれた(黙示的復元等)場合、新しいセッションの
        // Rust側初期値`app_foreground=true`のまま取り残さないよう現在値を反映する。
        if (!appInForeground) primaryPane.session.notifyDidEnterBackground()
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
        // このタブがtmux連携を保有していた場合は解放する。他タブが同じプロファイルを
        // 参照し続けている場合に誤って解放してしまわないよう、profile自体が閉じられて
        // いる(このタブが最後の1枚だった)場合のみ解放する。
        tab.profile?.let { profile ->
            val remainingForProfile = _tabs.value.any { it.tabId != tabId && it.profile?.id == profile.id }
            if (!remainingForProfile) tmuxClaimedProfileIds.remove(profile.id)
        }

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
        val pane = PaneState(UUID.randomUUID().toString(), sessionFactory(executor, rebindFdSource, profile), rebindFdSource)
        if (!appInForeground) pane.session.notifyDidEnterBackground()
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
     * 接続状態遷移(Connected 立ち上がりでの自動実行コマンド送信・切断時の後始末)。
     * 非アクティブでも動き続ける。upstream フェイルオーバーの `NoViablePath` 検知は
     * `RebindManager`(Rust側)が既に同じイベントで反応するため、Kotlin側で
     * 二重に監視しない(`observeFailover`は撤去済み、`rust-ssot.md`参照)。
     * [watchJobs] は paneId(タブをまたいで一意)をキーにする — 分割ペインを付け替えても
     * ジョブの追跡が壊れないようにするため。
     */
    private fun watchPane(tab: TabState, pane: PaneState) {
        watchJobs[pane.paneId] = viewModelScope.launch {
            launch { observeSummary(pane) }
            launch { observeDownloads(pane) }
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
                // `AI_INTEGRATION_DESIGN.md` §3: このpaneのAIパネルopt-inを、接続の
                // たびに(再接続を含め)Rust側へ送り直す(`SessionCore.set_panel_enabled`
                // のdocコメント参照: 値を保持しないため毎回の送信が必須)。split pane
                // それぞれが独立した`Terminal`を持つため、tmux通知(primary paneのみ)
                // と異なり全paneに対して行う。
                tab.profile?.let { pane.session.setAiPanelEnabled(it.enableAiPanel) }
                // タスク#60: tmux session group ensure/attach + ウィンドウのcreate-or-select。
                // primary paneのみが対象(split paneはtmux非対応のMVP判断、
                // `rust-core/src/tmux_session.rs`のモジュールdoc参照)。
                if (pane.paneId == tab.primaryPane.paneId) {
                    maybeEnsureTmuxTabWindow(tab, pane)
                }
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
                // タスク#60: 切断中は古い`tmux:N`ラベルを表示し続けない(再接続後の
                // maybeEnsureTmuxTabWindowが新しいラベルで上書きするまでの間、
                // 実際にはもう繋がっていないウィンドウ番号が残るのを防ぐ)。
                if (pane.paneId == tab.primaryPane.paneId) {
                    tab.tmuxWindowLabel.value = null
                }
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
     * `UpstreamHealthMonitor`(ConnectivityManagerの`NET_CAPABILITY_VALIDATED`
     * 喪失、Rust側のQUICパスヘルスとは無関係な独自シグナル)が「WiFiは繋がっている
     * がupstreamが死んでいる」を検知した際に呼ばれる。判断・実際のrebind実行は
     * 一切せず、生イベントを`RebindManager`(Rust側)へそのまま転送するだけ
     * (`rust-ssot.md`準拠)。以前はここでKotlin側が独自にセルラーfdを取得して
     * `rebindToFd`を直接呼んでいたが、これは`RebindManager`が同じ`NoViablePath`
     * (Rust側のQUICパスヘルス検知経由)で既に行っている`PerformRebindToCellular`と
     * 完全に重複しており、同じセッションに対して独立に2本のセルラーfdを取得し
     * `rebind_abstract`を2回叩く二重rebindになっていた(実害あり、opusレビューで
     * 発見)。`notifyUpstreamHealthDegraded`はマルチパス以外のtransportや
     * `enableUpstreamFailover`が無効な場合はRust側で何もしない(`rust-ssot.md`の
     * 「判断ロジックを一箇所に集約」原則通り、Kotlin側では分岐しない)。
     */
    private fun onWifiUpstreamBroken(pane: PaneState) {
        forwardToRust("notifyUpstreamHealthDegraded") { pane.session.notifyUpstreamHealthDegraded() }
    }

    /**
     * OSからの生イベント転送(ConnectivityManagerコールバックスレッド・
     * ProcessLifecycleOwnerのメインルーパー経由)専用の防御的ラッパー
     * (クラッシュ観点レビュー、2026-07-31)。転送先のUniFFIメソッドは宣言上
     * 例外を投げない設計だが、生成バインディングは実際には`InternalException`
     * (Rust panic由来)や`IllegalStateException`(destroyed handle)を投げ得る。
     * これらのイベントは取りこぼしても致命的ではなく(例えば
     * [onWifiUpstreamBroken]はRust側の`RebindManager`が`NoViablePath`検知で
     * 冗長にカバーする、そのdocコメント参照)、OSコールバックスレッド上の
     * 未捕捉例外によるアプリクラッシュを防ぐ方が優先度が高いため、ログだけ
     * 残して握り潰す。ユーザー操作起因の呼び出し(`send`/`resize`等)には
     * 意図的に使わない——バグを隠す方向に広げないため、対象はOS生イベントの
     * 転送に限定する。
     */
    private inline fun forwardToRust(what: String, block: () -> Unit) {
        try {
            block()
        } catch (e: Exception) {
            RemoteLogger.w("IsekaiTerminalTabsVM", "forwardToRust($what) failed, dropping this event", e)
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

    // ── 定型コマンド/打鍵列/打鍵列セット(パック)の読み込み ──────────────────
    // 3つとも「対象リポジトリから読んで対応するPaneStateのStateFlowへ詰める」骨格が
    // 同一だったため1つにまとめた。[ConnectionCoordinator.connectPane]から接続開始時に
    // 呼ばれる。

    private fun loadPaneContent(pane: PaneState, profileId: Long?) {
        viewModelScope.launch(ioDispatcher) {
            pane.snippets.value = Repositories.snippets.getForProfile(profileId)
        }
        viewModelScope.launch(ioDispatcher) {
            pane.keySequences.value = Repositories.keySequences.getForProfile(profileId)
        }
        viewModelScope.launch(ioDispatcher) {
            pane.installedPacks.value = tools.isekai.terminal.pack.KeySequencePacks.ALL.mapNotNull { pack ->
                Repositories.keySequencePackInstallations.resolveInstallation(pack.id, profileId)?.let { pack to it }
            }
        }
    }

    fun sendSnippetToPane(address: PaneAddress, snippet: Snippet) {
        RemoteLogger.i("IsekaiTerminalSnippet", "send snippet '${snippet.label}' id=${snippet.id} tab=${address.tabId} pane=${address.paneId}")
        paneOrNull(address)?.session?.send(SnippetCommands.toBytes(snippet))
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

    // ── tmux session group / ウィンドウ紐付け(タスク#60)─────────────────

    /**
     * primary paneが接続完了した際に、tmux session groupのensure/attach + タブ用
     * ウィンドウのcreate-or-selectをRust側(`SessionOrchestrator.ensureTmuxTabWindow`)
     * へ依頼する。どのtmuxコマンドを実行するか・既存タグが見つかるか等の判断は一切
     * Kotlin側で行わない(`.claude/rules/rust-ssot.md`) — ここは
     * (1) Roomに永続化済みのタグがあれば読んで渡す
     * (2) Rustが返した`tag`をRoomへ書き戻す
     * (3) 表示用ラベルをタブへ反映する
     * だけの薄い配線。
     *
     * opportunisticな補助機能: 失敗してもタブ自体は通常のシェルとして使い続けられる
     * ため、ログのみ残して無視する(接続やUIをブロックしない)。プロファイルを
     * 持たない、または未保存(id=0、理論上のみ)のタブでは何もしない。
     *
     * `tmux_tab_locators`(Room)は`profile_id`単位でしかタグを永続化できない
     * (`TmuxTabLocator.kt`のdoc参照、タブ単位の永続識別子が現状無い設計)ため、
     * 同一プロファイルを複数タブで同時に開くと、後から開いたタブが先のタブと
     * 同じtmuxウィンドウを`select-window`で奪う・先のタブのタグを上書きする
     * 事故になり得る。これを避けるため、既に他のタブがこのプロファイルで
     * tmux連携済み(`tmuxWindowLabel`が非null)なら、このタブはtmux連携自体を
     * スキップする(通常のシェルタブとして使い続けられる、上のopportunistic方針
     * と同じ扱い)。
     *
     * この判定だけではTOCTOUレースが残る(`tmuxWindowLabel`は非同期RPCが完了する
     * まで書かれないため、同一プロファイルの2タブがほぼ同時に`connected`へ遷移
     * すると両方この判定をすり抜ける、実機検証2026-07-27)。[tmuxClaimedProfileIds]
     * への同期的な`add`(コルーチン起動前)で実際に排他する。
     */
    private fun maybeEnsureTmuxTabWindow(tab: TabState, pane: PaneState) {
        val profile = tab.profile ?: return
        if (profile.id == 0L) return
        if (_tabs.value.any { it.tabId != tab.tabId && it.profile?.id == profile.id && it.tmuxWindowLabel.value != null }) {
            RemoteLogger.i(
                "IsekaiTerminalTmux",
                "ensureTmuxTabWindow[${tab.tabId}]: skipped, another tab already owns tmux window for profile ${profile.id}",
            )
            return
        }
        if (!tmuxClaimedProfileIds.add(profile.id)) {
            RemoteLogger.i(
                "IsekaiTerminalTmux",
                "ensureTmuxTabWindow[${tab.tabId}]: skipped, another tab already claimed profile ${profile.id}",
            )
            return
        }
        viewModelScope.launch(ioDispatcher) {
            try {
                val profileIdentity = "profile:${profile.id}"
                val clientId = ClientIdentity.getOrCreate(getApplication())
                val existingTag = Repositories.tmuxTabLocators.findTagForProfile(profile.id)
                val info = pane.session.ensureTmuxTabWindow(profileIdentity, clientId, existingTag, profile.enableTabNotifications)
                Repositories.tmuxTabLocators.saveTag(profile.id, info.tag)
                tab.tmuxWindowLabel.value = "tmux:${info.windowIndex}"
                RemoteLogger.i(
                    "IsekaiTerminalTmux",
                    "ensureTmuxTabWindow[${tab.tabId}]: group=${info.groupName} session=${info.sessionName} " +
                        "window=${info.windowIndex} tag=${info.tag} isNew=${info.isNewWindow}",
                )
            } catch (e: Exception) {
                tmuxClaimedProfileIds.remove(profile.id)
                RemoteLogger.w("IsekaiTerminalTmux", "ensureTmuxTabWindow failed (non-fatal): ${e.message}")
            }
        }
    }

    private fun paneOrNull(address: PaneAddress): PaneState? = tabOrNull(address.tabId)?.paneOrNull(address.paneId)

    /** [address]が指すpaneが存在すれば[block]を実行してその結果を返す。存在しなければnull。 */
    private fun <T> withPane(address: PaneAddress, block: (PaneState) -> T): T? = paneOrNull(address)?.let(block)

    // ── セッション操作(すべてPaneAddress指定。Task #13でtab-level互換APIは削除した)──
    // 画面分割時、両ペインは同時に見えるため「タブ指定」だけでは片方のペインの操作を
    // 表現できない(ステータスバーの再接続/切断/ログボタン・リサイズ・scrollback・キャンバスの
    // タップは常にそのペイン自身に向く)。UI([TerminalHostScreen])は常にペイン指定APIを使う。
    //
    // 純粋な `withPane(address) { it.session.X(...) }` 転送のみのメソッド(send/disconnect/
    // resize/scrollbackCells/jump*Prompt/clickToPromptCursor/copyLastCommandOutput/
    // trust*HostKey/dismiss*HostKey*/respondAgentSignRequest/getSessionLog等)はここには置かず、
    // TerminalHostScreen側で`pane`(既にPaneAddressから解決済み)から直接`pane.session.X(...)`を
    // 呼ぶ(TerminalHostScreen.kt:576-585が元々5個の操作でこの直接呼び出しパターンを使っていた)。
    // 追加のロジック(nullフォールバック以上の何か)を持つメソッドだけをここに残す。

    /** タスク#66: スクロールバック検索。対象ペインが無ければ(withPaneがnullを返す
     *  場合)空リストを返す——[TerminalSession.searchScrollback]自体の「未接続時は空
     *  リスト」という契約と揃える。 */
    fun searchScrollbackForPane(address: PaneAddress, query: String, caseSensitive: Boolean): List<ScrollbackSearchMatch> =
        withPane(address) { it.session.searchScrollback(query, caseSensitive) } ?: emptyList()

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

    // ── ライフサイクル ──────────────────────────────────────────────

    override fun onCleared() {
        super.onCleared()
        RemoteLogger.i("IsekaiTerminalTabsVM", "TerminalTabsViewModel cleared")
        _tabs.value.forEach { tab -> tab.panes.forEach { closePaneSession(it) } }
        executor.unregisterNetworkCallbacks()
        executor.unregisterLifecycleCallbacks()
        executor.release()
    }
}
