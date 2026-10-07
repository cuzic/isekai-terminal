package tools.isekai.terminal

import android.app.Application
import androidx.test.core.app.ApplicationProvider
import java.io.File
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.ExperimentalCoroutinesApi
import kotlinx.coroutines.delay
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.test.TestCoroutineScheduler
import kotlinx.coroutines.test.UnconfinedTestDispatcher
import kotlinx.coroutines.test.resetMain
import kotlinx.coroutines.test.setMain
import kotlinx.coroutines.withTimeout
import org.json.JSONObject
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertSame
import org.junit.Assert.assertTrue
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
import uniffi.isekai_terminal_core.ConnectionEdge
import uniffi.isekai_terminal_core.ConnectionPublicState
import uniffi.isekai_terminal_core.TransportPreference

/**
 * docs/adr/0019-functional-core-effects.md §6 Step 13: Rustが生成したcallback契約golden
 * (`rust-core/tests/golden/callback_contract/<scenario>.json`、生成・一致検査は
 * `rust-core/src/orchestrator/tests/callback_contract_golden.rs`)を、本物の転送経路
 * ([TerminalSession]の`OrchestratorCallback`実装 → `connectionEdges` → [TerminalTabsViewModel]の
 * `observeConnectionEdges`)へそのままreplayし、その結果の副作用を[DumbAppExecutor]越しに確かめる。
 *
 * Rust側のテストからは「Kotlin側が受け取ったエッジ1回につき対応処理を1回だけ呼ぶ(重複排除・
 * エッジ判定をしていない)」ことが見えないので、それをここで固定する。[FakeOrchestrator]の
 * `simulate*`(Kotlin側でRustの契約を模したもの)は使わず、golden の列を`callback`へ直接流す。
 *
 * goldenは`rust-core/`配下の1か所だけにあり(コピー無し)、gradleがsystem property
 * `isekai.callbackContractGoldenDir`で場所を渡す(`android/build.gradle.kts`、テストの入力としても宣言済み)。
 */
@OptIn(ExperimentalCoroutinesApi::class)
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [28])
class CallbackContractGoldenReplayTest {
    private lateinit var vm: TerminalTabsViewModel
    private lateinit var executor: DumbAppExecutor
    private val orchestrators = mutableListOf<FakeOrchestrator>()
    private lateinit var testScheduler: TestCoroutineScheduler

    @Before
    fun setup() {
        testScheduler = TestCoroutineScheduler()
        Dispatchers.setMain(UnconfinedTestDispatcher(testScheduler))
        val app = ApplicationProvider.getApplicationContext<Application>()
        Repositories.init(app)
        runBlocking { Repositories.profiles.getAll().forEach { Repositories.profiles.delete(it) } }
        executor = DumbAppExecutor()
        val sessionFactory: (AppExecutor, RebindFdSource, ConnectionProfile) -> TerminalSession = { _, _, _ ->
            val fake = FakeOrchestrator()
            orchestrators.add(fake)
            testTerminalSession(FakeHostKeyChecker(), orchestratorFactory = { cb -> fake.also { it.callback = cb } })
        }
        vm = TerminalTabsViewModel(app, executor, sessionFactory, UnconfinedTestDispatcher(testScheduler))
    }

    @After
    fun teardown() {
        Dispatchers.resetMain()
    }

    // ── golden の読み込み ─────────────────────────────────────────

    sealed interface GoldenEvent {
        data class State(val tag: String, val host: String?) : GoldenEvent
        data class Edge(val established: Boolean, val host: String?, val upstreamFailover: Boolean, val generation: Long) : GoldenEvent
        data class ForegroundResume(val didReconnect: Boolean) : GoldenEvent
    }

    data class Golden(val scenario: String, val events: List<GoldenEvent>) {
        val edges get() = events.filterIsInstance<GoldenEvent.Edge>()
        val establishedHosts get() = edges.filter { it.established }.map { it.host!! }
        val lostCount get() = edges.count { !it.established }
        /** `upstream_failover: true`を運ぶEstablishedの数(#175: Kotlinはその数だけupstream監視を登録する)。 */
        val upstreamFailoverEstablishedCount get() = edges.count { it.established && it.upstreamFailover }
        /** goldenの末尾でエッジが開いたまま(最後のエッジがEstablished)か。 */
        val edgeOpenAtEnd get() = edges.lastOrNull()?.established == true
        val lastState get() = events.filterIsInstance<GoldenEvent.State>().last()
    }

    private fun goldenDir(): File {
        val dir = System.getProperty(GOLDEN_DIR_PROPERTY)?.let(::File)
            ?: File("../rust-core/tests/golden/callback_contract")
        assertTrue("callback契約goldenのディレクトリが見つからない: ${dir.absolutePath}", dir.isDirectory)
        return dir
    }

    private fun load(scenario: String): Golden {
        val json = JSONObject(File(goldenDir(), "$scenario.json").readText())
        assertEquals(scenario, json.getString("scenario"))
        val array = json.getJSONArray("events")
        val events = (0 until array.length()).map { i ->
            val e = array.getJSONObject(i)
            when (val method = e.getString("method")) {
                "on_connection_state_changed" ->
                    GoldenEvent.State(e.getString("state"), if (e.has("host")) e.getString("host") else null)
                "on_connection_edge" -> when (val edge = e.getString("edge")) {
                    "Established" ->
                        GoldenEvent.Edge(true, e.getString("host"), e.getBoolean("upstream_failover"), e.getLong("generation"))
                    "Lost" -> GoldenEvent.Edge(false, null, false, e.getLong("generation"))
                    else -> throw AssertionError("$scenario: 未知のedge '$edge'")
                }
                "on_foreground_resume" -> GoldenEvent.ForegroundResume(e.getBoolean("did_reconnect"))
                else -> throw AssertionError("$scenario: 未知のmethod '$method'")
            }
        }
        return Golden(scenario, events)
    }

    // ── replay ─────────────────────────────────────────────────

    /** 物理マルチパス+upstreamフェイルオーバー有効のプロファイル(接続/切断に伴うhandleの登録・closeが観測できる)。 */
    private fun goldenProfile() = ConnectionProfile(
        label = "golden", host = "example.com", username = "user", authType = "password",
    ).copy(
        transportPreferenceName = TransportPreference.ISEKAI_PIPE_QUIC_MULTIPATH.name,
        enablePhysicalMultipath = true,
        enableUpstreamFailover = true,
    )

    private fun tab(tabId: String) = vm.tabs.value.first { it.tabId == tabId }

    /** タブを開いて`connect_*`を呼ばせた後、goldenの列をRustの代わりに`OrchestratorCallback`へ流す。 */
    private suspend fun replay(golden: Golden): String {
        val id = vm.openTab(goldenProfile(), "pass")
        withTimeout(3000) { while (!orchestrators[0].connectMultipathIsekaiPipeQuicCalled) delay(10) }
        val callback = orchestrators[0].callback!!
        for (event in golden.events) {
            when (event) {
                is GoldenEvent.State -> callback.onConnectionStateChanged(event.toPublicState(golden.scenario))
                is GoldenEvent.Edge -> callback.onConnectionEdge(
                    if (event.established) ConnectionEdge.Established(event.host!!, event.upstreamFailover) else ConnectionEdge.Lost,
                    event.generation.toULong(),
                )
                is GoldenEvent.ForegroundResume -> callback.onForegroundResume(event.didReconnect)
            }
        }
        withTimeout(3000) {
            while (executor.connectedHosts.size < golden.establishedHosts.size ||
                executor.disconnectedCount < golden.lostCount
            ) delay(10)
        }
        testScheduler.advanceUntilIdle()
        return id
    }

    private fun GoldenEvent.State.toPublicState(scenario: String): ConnectionPublicState = when (tag) {
        "Connecting" -> ConnectionPublicState.Connecting
        "Connected" -> ConnectionPublicState.Connected(host!!)
        "Disconnected" -> ConnectionPublicState.Disconnected(null, null)
        "Reconnecting" -> ConnectionPublicState.Reconnecting(0u, 60u, null)
        "Error" -> ConnectionPublicState.Error("golden")
        else -> throw AssertionError("$scenario: 未知の状態タグ '$tag'")
    }

    /**
     * エッジに対応する処理が、届いたエッジ1回につき正確に1回だけ走り、handleが二重closeもリークもしないこと。
     * 高速な再接続の連続(旧`StateFlow`ミラーのconflationで取りこぼしえた形)でも同じ。
     */
    private fun replayAndAssertContract(scenario: String) = runBlocking {
        val golden = load(scenario)
        val id = replay(golden)
        val pane = tab(id).primaryPane

        assertEquals("$scenario: Establishedごとにnotify_connectedが1回", golden.establishedHosts, executor.connectedHosts)
        assertEquals("$scenario: Lostごとにnotify_disconnectedが1回", golden.lostCount, executor.disconnectedCount)

        // 物理マルチパスfdのhandleは接続開始時に1つだけ取得され、最初のLostで正確に1回closeされる。
        assertEquals("$scenario: 物理マルチパスhandleの取得数", 1, executor.physicalMultipathHandles.size)
        assertEquals(
            "$scenario: 物理マルチパスhandleのclose回数",
            if (golden.lostCount > 0) 1 else 0,
            executor.physicalMultipathHandles[0].closeCount,
        )

        // #175: upstream監視は、Rustが`upstream_failover: true`を載せたEstablishedごとに1回だけ登録する
        // (プロファイルは有効でも、Kotlin側はエッジの値だけに従う。ミラーフラグを持たない)。
        assertEquals(
            "$scenario: upstream_failover: trueのEstablishedごとにupstream監視を1回登録",
            golden.upstreamFailoverEstablishedCount,
            executor.upstreamFailoverHandles.size,
        )
        // upstream監視のhandleは二重closeせず、同時に開いているのは高々1つ。エッジが閉じて終わったら全て閉じている。
        executor.upstreamFailoverHandles.forEach {
            assertTrue("$scenario: ${it.label}が二重closeされた(${it.closeCount}回)", it.closeCount <= 1)
        }
        val open = executor.upstreamFailoverHandles.filter { !it.closed }
        assertTrue("$scenario: 開いたままのupstream監視handleが複数ある: $open", open.size <= 1)
        if (golden.edgeOpenAtEnd) {
            assertSame("$scenario: 開いているhandleはpaneが保持しているもの", open.firstOrNull(), pane.upstreamFailoverMonitorHandle)
        } else {
            assertTrue("$scenario: Lostで終わったのにupstream監視handleが開いたまま: $open", open.isEmpty())
            assertNull("$scenario: Lostで終わったのにpaneがupstream監視handleを保持している", pane.upstreamFailoverMonitorHandle)
            assertNull("$scenario: Lostで終わったのにpaneが物理マルチパスhandleを保持している", pane.physicalMultipathHandle)
        }

        // 状態公開も転送されている(UIは最後に届いた公開状態を反映する)。
        val last = golden.lastState
        withTimeout(3000) {
            while (pane.session.state.value.connected != (last.tag == "Connected")) delay(10)
        }
        assertEquals("$scenario: 最終のisReconnecting", last.tag == "Reconnecting", pane.session.state.value.isReconnecting)
    }

    @Test fun a_user_disconnect() = replayAndAssertContract("a_user_disconnect")
    @Test fun b_transport_error() = replayAndAssertContract("b_transport_error")
    @Test fun c_network_lost_debounce() = replayAndAssertContract("c_network_lost_debounce")
    @Test fun d_manual_connect_while_connected() = replayAndAssertContract("d_manual_connect_while_connected")
    @Test fun e_foreground_resume_reconnect() = replayAndAssertContract("e_foreground_resume_reconnect")
    @Test fun f_reconnect_loop_success() = replayAndAssertContract("f_reconnect_loop_success")
    @Test fun reconnect_gives_up() = replayAndAssertContract("reconnect_gives_up")
    @Test fun fast_reconnect_cycles() = replayAndAssertContract("fast_reconnect_cycles")
    @Test fun upstream_failover_reconnects() = replayAndAssertContract("upstream_failover_reconnects")

    /** goldenディレクトリの全シナリオを上のテストがreplayしている(Rust側でシナリオを足したらここにも足す)。 */
    @Test
    fun everyGoldenScenarioIsReplayed() {
        val files = goldenDir().listFiles()!!
            .map { it.name }
            .filter { it.endsWith(".json") && !it.endsWith(".actual.json") }
            .map { it.removeSuffix(".json") }
            .sorted()
        assertEquals(SCENARIOS.sorted(), files)
    }

    /**
     * #175(Step 13のreplayで発見): 以前は`onConnectionLost`がKotlin側のミラーフラグ
     * `upstreamFailoverEnabledForCurrentSession`をfalseに戻していたため、Rustの自動再接続ループ・
     * フォアグラウンド復帰(同じ接続設定で新しい世代が`Established`、`connectPane`を通らない)の後は
     * upstreamフェイルオーバー監視が再登録されなかった。いまはRustが`Established`ごとに
     * `upstream_failover`を載せ、Kotlinはそれをそのまま適用する。
     */
    @Test
    fun upstreamFailoverMonitor_isRegisteredForEveryEstablishedEdge_acrossAutomaticReconnects() = runBlocking {
        val golden = load("upstream_failover_reconnects")
        assertTrue("goldenの前提: 再接続を含む複数世代", golden.establishedHosts.size >= 3)
        assertEquals("goldenの前提: 全世代がupstream_failoverを運ぶ", golden.establishedHosts.size, golden.upstreamFailoverEstablishedCount)
        val id = replay(golden)
        assertEquals(
            "Establishedごとにupstream監視を登録するはず",
            golden.establishedHosts.size,
            executor.upstreamFailoverHandles.size,
        )
        val open = executor.upstreamFailoverHandles.filter { !it.closed }
        assertEquals("最後の世代のhandleだけが開いている", listOf(executor.upstreamFailoverHandles.last()), open)
        assertSame(open.single(), tab(id).primaryPane.upstreamFailoverMonitorHandle)
    }

    companion object {
        private const val GOLDEN_DIR_PROPERTY = "isekai.callbackContractGoldenDir"

        /** `rust-core/src/orchestrator/tests/callback_contract_golden.rs`の`SCENARIOS`と同じ一覧。 */
        private val SCENARIOS = listOf(
            "a_user_disconnect",
            "b_transport_error",
            "c_network_lost_debounce",
            "d_manual_connect_while_connected",
            "e_foreground_resume_reconnect",
            "f_reconnect_loop_success",
            "reconnect_gives_up",
            "fast_reconnect_cycles",
            "upstream_failover_reconnects",
        )
    }
}
