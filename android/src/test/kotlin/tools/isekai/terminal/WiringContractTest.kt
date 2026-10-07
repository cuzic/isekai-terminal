package tools.isekai.terminal

import android.app.Application
import android.net.Uri
import androidx.test.core.app.ApplicationProvider
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.ExperimentalCoroutinesApi
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.flow.update
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.test.TestCoroutineScheduler
import kotlinx.coroutines.test.UnconfinedTestDispatcher
import kotlinx.coroutines.test.resetMain
import kotlinx.coroutines.test.setMain
import kotlinx.coroutines.withTimeoutOrNull
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertTrue
import org.junit.Assert.fail
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config
import tools.isekai.terminal.data.ConnectionProfile
import tools.isekai.terminal.data.Repositories
import tools.isekai.terminal.session.AppExecutor
import tools.isekai.terminal.session.RebindFdSource
import tools.isekai.terminal.session.TerminalSession
import uniffi.isekai_terminal_core.ClipboardMimeKind
import uniffi.isekai_terminal_core.ClipboardPayload
import uniffi.isekai_terminal_core.DiagnosticCallback
import uniffi.isekai_terminal_core.DiagnosticEventQueueInterface
import uniffi.isekai_terminal_core.DiagnosticFrameMailboxInterface
import uniffi.isekai_terminal_core.DiagnosticHandleInterface
import uniffi.isekai_terminal_core.EventWakeListener
import uniffi.isekai_terminal_core.NotifyKind
import uniffi.isekai_terminal_core.OrchestratorCallback
import uniffi.isekai_terminal_core.SessionOrchestratorInterface
import uniffi.isekai_terminal_core.TransportPreference
import java.lang.reflect.Modifier

/**
 * `docs/adr/0020-unwired-callback-detection.md` Phase 1(§3(a′))の配線契約テスト(JVM/Robolectric側)。
 *
 * 1. **網羅**: 生成UniFFIバインディングの全interfaceメソッド・全トップレベル関数を反射で列挙し、
 *    [WiringContract]の分類表と過不足なく一致することをassertする。全数はUniFFIのchecksumシンボル
 *    (`IntegrityCheckingUniffiLib`、初期化しない`Class.forName`で列挙)と照合し、生成器の変更で
 *    反射の列挙が黙って縮むのを防ぐ。
 * 2. **名前規則**: `notify*`は`OsEvent`/`UiAction`/`NotUsedOnAndroid`のみ。
 * 3. **OS由来入口の到達**: 本物の[TerminalTabsViewModel]+[TerminalSession]を、Rust側だけを
 *    記録Proxy([RecordingOrchestrator])に差し替えて台本通りに動かし、`OsEvent`の全メソッドが
 *    **primaryとsplitの両pane**で記録されることをassertする(db3a6d87・7b10472a型の検出)。
 * 4. **callbackの到達**: `InjectedLambda`のcallbackが[TerminalSession]の注入lambdaへ届くこと。
 *
 * `UiAction`のCompose台本は[WiringContractComposeTest](UI側のflakeが配線の結果を隠さないよう別クラス)。
 * 待ち合わせはすべて記録Proxy/状態の[StateFlow]を`first { }`で待つ(sleep・ポーリング無し。
 * `withTimeoutOrNull`は失敗時に診断メッセージを出すための安全上限)。
 */
@OptIn(ExperimentalCoroutinesApi::class)
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [28])
class WiringContractTest {
    private lateinit var testScheduler: TestCoroutineScheduler
    private val panes = mutableListOf<RecordingOrchestrator>()
    private val loader: ClassLoader get() = WiringContractTest::class.java.classLoader!!

    @Before
    fun setup() {
        testScheduler = TestCoroutineScheduler()
        Dispatchers.setMain(UnconfinedTestDispatcher(testScheduler))
        val app = ApplicationProvider.getApplicationContext<Application>()
        Repositories.init(app)
        runBlocking { Repositories.profiles.getAll().forEach { Repositories.profiles.delete(it) } }
    }

    @After
    fun teardown() {
        Dispatchers.resetMain()
    }

    // ── 1. 網羅(反射 × checksum × 分類表) ─────────────────────────────

    private val checksumPrefix = "uniffi_isekai_terminal_core_checksum_"

    /** `IntegrityCheckingUniffiLib`の`checksum_*`外部関数名(prefix除去済み)。objectを初期化しない(JNAをロードしない)。 */
    private fun checksumSymbols(): List<String> =
        Class.forName("uniffi.isekai_terminal_core.IntegrityCheckingUniffiLib", false, loader)
            .declaredMethods.map { it.name }.filter { it.startsWith(checksumPrefix) }.map { it.removePrefix(checksumPrefix) }

    /** checksumのobject名(UniFFIが小文字化したもの) → (生成Kotlin interface, 分類表のキー接頭辞)。 */
    private val uniffiObjects: Map<String, Pair<Class<*>, String>> = mapOf(
        "sessionorchestrator" to (SessionOrchestratorInterface::class.java to ""),
        "orchestratorcallback" to (OrchestratorCallback::class.java to ""),
        "diagnosticcallback" to (DiagnosticCallback::class.java to "DiagnosticCallback."),
        "diagnosticeventqueue" to (DiagnosticEventQueueInterface::class.java to "DiagnosticEventQueue."),
        "diagnosticframemailbox" to (DiagnosticFrameMailboxInterface::class.java to "DiagnosticFrameMailbox."),
        "diagnostichandle" to (DiagnosticHandleInterface::class.java to "DiagnosticHandle."),
        "eventwakelistener" to (EventWakeListener::class.java to "EventWakeListener."),
    )

    private fun interfaceMethodNames(c: Class<*>): Set<String> =
        c.methods.filter { !Modifier.isStatic(it.modifiers) && !it.isSynthetic }.map { WiringContract.kotlinName(it.name) }.toSet()

    @Test
    fun everyGeneratedMethodIsClassified_andMatchesUniffiChecksums() {
        val symbols = checksumSymbols()
        assertTrue("checksumシンボルが1つも見つからない(生成器の形が変わった?)", symbols.isNotEmpty())
        val errors = mutableListOf<String>()

        // (a) objectメソッド/callbackメソッド
        val methodSymbols = symbols.filter { it.startsWith("method_") }.map { it.removePrefix("method_") }
        val byObject = methodSymbols.groupBy({ it.substringBefore('_') }, { WiringContract.snakeToCamel(it.substringAfter('_')) })
        (byObject.keys - uniffiObjects.keys).forEach { errors += "未知のUniFFI object/callback interface `$it`: WiringContractTest.uniffiObjects と WiringContract の分類表に追加すること" }
        for ((obj, pair) in uniffiObjects) {
            val (iface, keyPrefix) = pair
            val fromChecksum = byObject[obj].orEmpty().toSet()
            val fromReflection = interfaceMethodNames(iface)
            if (fromChecksum != fromReflection) {
                errors += "${iface.simpleName}: 反射とchecksumが不一致 (反射のみ=${fromReflection - fromChecksum}, checksumのみ=${fromChecksum - fromReflection})"
            }
            val table = tableFor(obj)
            val classified = table.keys.filter { it.startsWith(keyPrefix) && (keyPrefix.isNotEmpty() || '.' !in it) }
                .map { it.removePrefix(keyPrefix) }.filter { it != "new" }.toSet()
            (fromReflection - classified).forEach { errors += "未分類: ${iface.simpleName}.$it (WiringContractに1行足して分類すること)" }
            (classified - fromReflection).forEach { errors += "分類表に存在しないメソッド: ${iface.simpleName}.$it" }
        }

        // (b) コンストラクタ
        val ctorKeys = symbols.filter { it.startsWith("constructor_") }.map { it.removePrefix("constructor_") }.map { sym ->
            val obj = sym.substringBefore('_')
            val prefix = uniffiObjects[obj]?.second ?: "<unknown:$obj>."
            prefix + WiringContract.snakeToCamel(sym.substringAfter('_'))
        }.toSet()
        val classifiedCtors = WiringContract.diagnostics.keys.filter { it.endsWith(".new") }.toSet()
        if (ctorKeys != classifiedCtors) errors += "コンストラクタの分類が不一致: checksum=$ctorKeys 分類表=$classifiedCtors"

        // (c) トップレベル関数(ファサード`Isekai_terminal_coreKt`の公開static。UniFFI関数でない生成物のヘルパーは固定の許可リストで除外)
        val fromChecksum = symbols.filter { it.startsWith("func_") }.map { WiringContract.snakeToCamel(it.removePrefix("func_")) }.toSet()
        val nonUniffiFacadeHelpers = setOf(
            "uniffiEnsureInitialized", "use", "getUniffiContinuationHandleMap",
            // 生成物のinternalヘルパー(トップレベルのinternal関数はJVM上publicのstaticになる)
            "uniffiRustCallAsync", "uniffiTraitInterfaceCall",
        )
        val facade = Class.forName("uniffi.isekai_terminal_core.Isekai_terminal_coreKt", false, loader)
        val fromFacade = facade.declaredMethods
            .filter { Modifier.isStatic(it.modifiers) && Modifier.isPublic(it.modifiers) && !it.isSynthetic }
            .map { WiringContract.kotlinName(it.name) }.toSet() - nonUniffiFacadeHelpers
        if (fromFacade != fromChecksum) {
            errors += "トップレベル関数: ファサードとchecksumが不一致 (ファサードのみ=${fromFacade - fromChecksum}, checksumのみ=${fromChecksum - fromFacade})"
        }
        (fromChecksum - WiringContract.topLevelFunctions.keys).forEach { errors += "未分類のトップレベル関数: $it" }
        (WiringContract.topLevelFunctions.keys - fromChecksum).forEach { errors += "分類表に存在しないトップレベル関数: $it" }

        assertTrue(errors.joinToString("\n", prefix = "配線契約の網羅性違反:\n"), errors.isEmpty())
        // ADR §3(a′)-4 執筆時点の全数(func 27 / method 63 / constructor 3)。増減したら上の分類で必ず捕まるので、
        // ここは「反射の列挙そのものが黙って縮んでいない」ことの下限チェックとしてだけ使う。
        assertTrue("checksum func数が想定より少ない: ${fromChecksum.size}", fromChecksum.size >= 27)
        assertTrue("checksum method数が想定より少ない: ${methodSymbols.size}", methodSymbols.size >= 63)
    }

    private fun tableFor(obj: String): Map<String, Wiring> = when (obj) {
        "sessionorchestrator" -> WiringContract.sessionOrchestrator
        "orchestratorcallback" -> WiringContract.orchestratorCallback
        else -> WiringContract.diagnostics
    }

    private val allEntries: List<Pair<String, Wiring>>
        get() = WiringContract.sessionOrchestrator.map { "SessionOrchestrator.${it.key}" to it.value } +
            WiringContract.orchestratorCallback.map { "OrchestratorCallback.${it.key}" to it.value } +
            WiringContract.diagnostics.map { it.key to it.value } +
            WiringContract.topLevelFunctions.map { "fn ${it.key}" to it.value }

    // ── 2. 名前規則・ref・呼び出し元シンボル ───────────────────────────

    @Test
    fun notifyMethods_areNeverClassifiedAsCommandOrQuery() {
        val violations = allEntries.filter { (key, w) ->
            key.substringAfterLast('.').substringAfter("fn ").startsWith("notify") &&
                w !is Wiring.OsEvent && w !is Wiring.UiAction && w !is Wiring.NotUsedOnAndroid
        }
        assertTrue("notify*はOsEvent/UiAction/NotUsedOnAndroidのみ(ADR R2-1): $violations", violations.isEmpty())
    }

    @Test
    fun everyRefPointsToACommitOrAMarkdownSection_andEveryCallerSymbolExists() {
        val errors = mutableListOf<String>()
        for ((key, w) in allEntries) {
            val ref = when (w) {
                is Wiring.NotUsedOnAndroid -> w.ref
                is Wiring.LogOnly -> w.ref
                is Wiring.UiAction -> w.composeExemptRef
                else -> null
            }
            if (ref != null && !WiringContract.isValidRef(ref)) errors += "$key: refはコミットハッシュか`.md`節であること(自由文不可): '$ref'"
            val caller = when (w) {
                is Wiring.Command -> w.caller
                is Wiring.Query -> w.caller
                else -> null
            }
            if (caller != null && !callerExists(caller)) errors += "$key: 本番の呼び出し元シンボルが見つからない: $caller"
        }
        assertTrue(errors.joinToString("\n"), errors.isEmpty())
    }

    private fun callerExists(caller: String): Boolean {
        val cls = caller.substringBefore('#')
        val method = caller.substringAfter('#', missingDelimiterValue = "")
        if (method.isEmpty()) return false
        val c = runCatching { Class.forName(cls, false, loader) }.getOrNull() ?: return false
        return if (method == "<init>") c.declaredConstructors.isNotEmpty()
        else c.declaredMethods.any { WiringContract.kotlinName(it.name) == method }
    }

    // ── 3. OS由来入口の到達(ライフサイクル台本) ──────────────────────────

    /** [DumbAppExecutor]へ委譲しつつ、upstream failover監視の登録を待てるようにする。 */
    private class RecordingExecutor(val inner: DumbAppExecutor = DumbAppExecutor()) : AppExecutor by inner {
        val upstreamMonitorRegistrations = MutableStateFlow(0)
        override fun registerUpstreamFailoverMonitor(onWifiUpstreamBroken: () -> Unit): AutoCloseable =
            inner.registerUpstreamFailoverMonitor(onWifiUpstreamBroken).also { upstreamMonitorRegistrations.update { it + 1 } }
    }

    private fun newViewModel(executor: AppExecutor): TerminalTabsViewModel {
        val app = ApplicationProvider.getApplicationContext<Application>()
        val sessionFactory: (AppExecutor, RebindFdSource, ConnectionProfile) -> TerminalSession = { _, _, _ ->
            val recording = RecordingOrchestrator()
            panes.add(recording)
            testTerminalSession(FakeHostKeyChecker(), orchestratorFactory = { cb -> recording.fake.callback = cb; recording.proxy })
        }
        return TerminalTabsViewModel(app, executor, sessionFactory, UnconfinedTestDispatcher(testScheduler))
    }

    private suspend fun <T> StateFlow<T>.await(what: String, predicate: (T) -> Boolean) {
        withTimeoutOrNull(10_000) { first(predicate) } ?: fail("待機がタイムアウト: $what (現在値=$value)")
    }

    private suspend fun RecordingOrchestrator.awaitCalls(label: String, expected: Set<String>) {
        withTimeoutOrNull(10_000) { calls.first { it.toSet().containsAll(expected) } }
            ?: fail("$label: Rustへ到達しなかった入口=${expected - recorded()} (記録=${recorded()})")
    }

    /**
     * ADR §5 Phase 1の台本: 接続 → split pane → 背景 → 前景 → ネットワーク断/復帰 → upstream劣化 → 切断。
     * `notifyUpstreamHealthDegraded`は`ISEKAI_PIPE_QUIC_MULTIPATH`かつ`enableUpstreamFailover = true`の
     * プロファイルでしか監視が登録されない(`ConnectionCoordinator.connectPane`→`observeConnectionTransitions`、R2-2)。
     */
    @Test
    fun osEvents_reachRustOnBothPrimaryAndSplitPane() = runBlocking {
        val executor = RecordingExecutor()
        val vm = newViewModel(executor)
        val profile = ConnectionProfile(
            label = "wiring", host = "wiring.example.com", username = "user", authType = "password",
            transportPreferenceName = TransportPreference.ISEKAI_PIPE_QUIC_MULTIPATH.name,
            enablePhysicalMultipath = false,
            enableUpstreamFailover = true,
        )

        val tabId = vm.openTab(profile, "pass")
        val primary = panes[0]
        primary.awaitCalls("primary(接続)", setOf("connectMultipathIsekaiPipeQuic"))
        primary.fake.simulateConnected("host-primary")

        assertNotNull("split paneを作れること", vm.splitPane(tabId, SplitDirection.VERTICAL, "pass"))
        assertEquals("split paneのセッションが生成されること", 2, panes.size)
        val split = panes[1]
        split.awaitCalls("split(接続)", setOf("connectMultipathIsekaiPipeQuic"))
        split.fake.simulateConnected("host-split")
        executor.upstreamMonitorRegistrations.await("両paneのupstream failover監視登録") { it >= 2 }

        executor.inner.simulateAppBackgrounded()
        executor.inner.simulateAppForegrounded()
        executor.inner.simulateNetworkLost()
        executor.inner.simulateNetworkAvailable()
        executor.inner.simulateWifiUpstreamBroken(0)
        executor.inner.simulateWifiUpstreamBroken(1)

        val osEvents = WiringContract.sessionOrchestrator.filterValues { it is Wiring.OsEvent }.keys
        val observedCommands = WiringContract.sessionOrchestrator.filterValues { it is Wiring.Command && it.observedInLifecycle }.keys
        assertTrue("OsEventが1つも分類されていない", osEvents.isNotEmpty())
        primary.awaitCalls("primary pane", osEvents + observedCommands)
        split.awaitCalls("split pane", osEvents + observedCommands)

        vm.closeTab(tabId)
        primary.awaitCalls("primary(切断)", setOf("disconnect"))
        split.awaitCalls("split(切断)", setOf("disconnect"))
    }

    /**
     * Compose台本から除外した`UiAction`(`composeExemptRef`付き)は、VMの公開APIから到達を確かめる。
     * 除外を増やしたらここに台本を足すこと(足さずに除外だけ増やすと落ちる)。
     */
    @Test
    fun composeExemptUiActions_reachRustThroughTheViewModel() = runBlocking {
        val exempt = WiringContract.sessionOrchestrator.filterValues { it is Wiring.UiAction && it.composeExemptRef != null }.keys
        assertEquals("Compose台本から除外したUiActionにはこのテストの台本が要る", setOf("trzszAcceptUpload"), exempt)

        val vm = newViewModel(DumbAppExecutor())
        val tabId = vm.openTab(
            ConnectionProfile(label = "upload", host = "upload.example.com", username = "user", authType = "password"),
            "pass",
        )
        val pane = panes[0]
        pane.awaitCalls("接続", setOf("connect"))
        pane.fake.simulateConnected()
        pane.fake.simulateTrzszRequest("u1", "upload", null, null)
        val tab = vm.tabs.value.first { it.tabId == tabId }
        tab.primaryPane.session.state.await("trzszのWaitingUser") { it.trzszState is TrzszUiState.WaitingUser }

        vm.trzszStartUploadForPane(PaneAddress(tabId, tab.primaryPane.paneId), Uri.parse("content://wiring/upload.txt"))

        pane.awaitCalls("upload", setOf("trzszAcceptUpload", "trzszSendChunk"))
    }

    // ── 4. callback(Rust → platform)の到達 ─────────────────────────────

    @Test
    fun injectedLambdaCallbacks_reachTheInjectedLambda() {
        val fired = mutableSetOf<String>()
        var callback: OrchestratorCallback? = null
        val session = TerminalSession(
            FakeHostKeyChecker(),
            orchestratorFactory = { cb -> callback = cb; FakeOrchestrator().also { it.callback = cb } },
            onClipboardWriteRequested = { fired += "onClipboardWriteRequested" },
            onClipboardPullRequested = { fired += "onClipboardPullRequested"; null },
            acquireWifiFd = { fired += "acquireWifiFd"; null },
            acquireCellularFd = { fired += "acquireCellularFd"; null },
            onBell = { fired += "onBell" },
            onNotify = { _, _, _ -> fired += "onNotify" },
            onNotifyRequested = { fired += "onNotifyRequested" },
        )
        val cb = callback!!
        val injected = WiringContract.orchestratorCallback.mapNotNull { (name, w) -> (w as? Wiring.InjectedLambda)?.let { name to it.lambda } }
        assertTrue("InjectedLambdaが1つも分類されていない", injected.isNotEmpty())
        val errors = mutableListOf<String>()
        for ((name, lambda) in injected) {
            fired.clear()
            when (name) {
                "onClipboardWrite" -> cb.onClipboardWrite(ClipboardPayload(ClipboardMimeKind.TEXT_PLAIN, "x".toByteArray()))
                "onClipboardPullRequest" -> cb.onClipboardPullRequest()
                "onRequestWifiFd" -> cb.onRequestWifiFd()
                "onRequestCellularFd" -> cb.onRequestCellularFd()
                "onNotify" -> cb.onNotify(NotifyKind.INFO)
                else -> { errors += "$name: このテストに駆動手順が無い(InjectedLambdaを足したらwhenに1行足すこと)"; continue }
            }
            if (lambda !in fired) errors += "OrchestratorCallback.$name が注入lambda `$lambda` へ届かない (発火=$fired)"
        }
        session.close()
        assertTrue(errors.joinToString("\n"), errors.isEmpty())
    }
}
