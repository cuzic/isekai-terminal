package tools.isekai.terminal

import android.app.Application
import androidx.test.core.app.ApplicationProvider
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.ExperimentalCoroutinesApi
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.test.UnconfinedTestDispatcher
import kotlinx.coroutines.test.resetMain
import kotlinx.coroutines.test.setMain
import kotlinx.coroutines.withTimeout
import org.junit.After
import org.junit.Assert.*
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config
import tools.isekai.terminal.data.ConnectionProfile
import tools.isekai.terminal.session.TerminalSession

/**
 * 複数タブ (複数 SSH セッション) を横断する [TerminalTabsViewModel] のテスト。
 *
 * 各タブは独立した [FakeOrchestrator] にバインドされた [TerminalSession] を持つため、
 * タブ間の状態が混ざらないことを検証できる。
 */
@OptIn(ExperimentalCoroutinesApi::class)
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [28])
class TerminalTabsViewModelTest {
    private lateinit var vm: TerminalTabsViewModel
    private lateinit var executor: DumbAppExecutor
    // tabId ごとの FakeOrchestrator を、生成順に記録する。
    private val orchestrators = mutableListOf<FakeOrchestrator>()

    @Before
    fun setup() {
        Dispatchers.setMain(UnconfinedTestDispatcher())
        val app = ApplicationProvider.getApplicationContext<Application>()
        executor = DumbAppExecutor()
        val sessionFactory: () -> TerminalSession = {
            val fake = FakeOrchestrator()
            orchestrators.add(fake)
            TerminalSession(FakeHostKeyChecker()) { cb -> fake.also { it.callback = cb } }
        }
        vm = TerminalTabsViewModel(app, executor, sessionFactory)
    }

    @After
    fun teardown() {
        Dispatchers.resetMain()
    }

    private fun profile(label: String) = ConnectionProfile(
        label = label, host = "$label.example.com", username = "user", authType = "password",
    )

    private suspend fun awaitConnectCalled(o: FakeOrchestrator) =
        withTimeout(3000) { while (!o.connectCalled) kotlinx.coroutines.delay(10) }

    // ── タブ追加/削除でセッション生成・close が呼ばれる ────────────────────

    @Test
    fun openTab_createsSessionAndConnects() = runBlocking {
        val id = vm.openTab(profile("a"), "pass")
        awaitConnectCalled(orchestrators[0])

        assertEquals(1, vm.tabs.value.size)
        assertEquals(id, vm.activeTabId.value)
        assertTrue(orchestrators[0].connectCalled)
    }

    @Test
    fun closeTab_disconnectsAndClosesSession() = runBlocking {
        val id = vm.openTab(profile("a"), "pass")
        awaitConnectCalled(orchestrators[0])
        orchestrators[0].simulateConnected()

        vm.closeTab(id)

        assertTrue("disconnect() should reach the orchestrator", orchestrators[0].disconnectCalled)
        assertEquals(0, vm.tabs.value.size)
        assertNull(vm.activeTabId.value)
    }

    @Test
    fun closeTab_unknownId_isNoop() {
        vm.openTab(profile("a"), "pass")
        val before = vm.tabs.value.size
        vm.closeTab("does-not-exist")
        assertEquals(before, vm.tabs.value.size)
    }

    // ── ネットワーク断は全セッションへファンアウトされる ──────────────────

    @Test
    fun onNetworkLost_fansOutToAllConnectedTabs() = runBlocking {
        vm.openTab(profile("a"), "pass")
        vm.openTab(profile("b"), "pass")
        awaitConnectCalled(orchestrators[0])
        awaitConnectCalled(orchestrators[1])
        orchestrators[0].simulateConnected("host-a")
        orchestrators[1].simulateConnected("host-b")

        executor.simulateNetworkLost()

        assertTrue("tab a should be disconnected on network loss", orchestrators[0].disconnectCalled)
        assertTrue("tab b should be disconnected on network loss", orchestrators[1].disconnectCalled)
    }

    @Test
    fun onNetworkLost_withNoTabs_doesNotThrow() {
        vm.onNetworkLost()
    }

    // ── 最後のタブを閉じた時のみ FGS 停止 ────────────────────────────────

    @Test
    fun closingTabs_stopsServiceOnlyWhenLastTabCloses() = runBlocking {
        val idA = vm.openTab(profile("a"), "pass")
        val idB = vm.openTab(profile("b"), "pass")
        awaitConnectCalled(orchestrators[0])
        awaitConnectCalled(orchestrators[1])
        orchestrators[0].simulateConnected("host-a")
        orchestrators[1].simulateConnected("host-b")

        assertEquals(0, executor.serviceStoppedCount)

        vm.closeTab(idA)
        assertEquals(
            "closing one of two tabs must not stop the shared foreground service",
            0, executor.serviceStoppedCount,
        )
        assertEquals(1 to 1, executor.lastSessionsSummary)

        vm.closeTab(idB)
        assertEquals(
            "closing the last tab must signal the service that it may stop",
            1, executor.serviceStoppedCount,
        )
    }

    @Test
    fun openTab_onlyEnsuresServiceRunning_doesNotStopIt() = runBlocking {
        vm.openTab(profile("a"), "pass")
        assertEquals(1, executor.serviceRunCount)
        assertEquals(0, executor.serviceStoppedCount)
    }

    // ── アクティブ切替で他タブの状態が壊れない ────────────────────────────

    @Test
    fun setActiveTab_doesNotAffectOtherTabsSessionState() = runBlocking {
        val idA = vm.openTab(profile("a"), "pass")
        val idB = vm.openTab(profile("b"), "pass")
        awaitConnectCalled(orchestrators[0])
        awaitConnectCalled(orchestrators[1])
        orchestrators[0].simulateConnected("host-a")
        // tab b intentionally left in "connecting" state

        vm.setActiveTab(idA)
        vm.setActiveTab(idB)
        vm.setActiveTab(idA)

        val tabA = vm.tabs.value.first { it.tabId == idA }
        val tabB = vm.tabs.value.first { it.tabId == idB }
        assertTrue("tab a must remain connected regardless of active-tab switches", tabA.session.state.value.connected)
        assertFalse("tab b must not have been connected as a side effect", tabB.session.state.value.connected)
        assertEquals(idA, vm.activeTabId.value)
    }

    @Test
    fun send_isRoutedToTheCorrectTabsOrchestratorOnly() = runBlocking {
        val idA = vm.openTab(profile("a"), "pass")
        val idB = vm.openTab(profile("b"), "pass")
        awaitConnectCalled(orchestrators[0])
        awaitConnectCalled(orchestrators[1])
        orchestrators[0].simulateConnected()
        orchestrators[1].simulateConnected()

        vm.send(idA, byteArrayOf(0x41))

        assertTrue(orchestrators[0].sentBytes.any { it.contentEquals(byteArrayOf(0x41)) })
        assertTrue("tab b's orchestrator must not receive tab a's bytes", orchestrators[1].sentBytes.isEmpty())
    }

    @Test
    fun closeTab_activatesRemainingTab_whenActiveTabIsClosed() = runBlocking {
        val idA = vm.openTab(profile("a"), "pass")
        val idB = vm.openTab(profile("b"), "pass")
        assertEquals(idB, vm.activeTabId.value)

        vm.closeTab(idB)

        assertEquals(idA, vm.activeTabId.value)
    }

    @Test
    fun onCleared_closesAllSessionsAndReleasesExecutor() = runBlocking {
        vm.openTab(profile("a"), "pass")
        vm.openTab(profile("b"), "pass")
        awaitConnectCalled(orchestrators[0])
        awaitConnectCalled(orchestrators[1])

        TerminalTabsViewModel::class.java
            .getDeclaredMethod("onCleared")
            .apply { isAccessible = true }
            .invoke(vm)

        assertTrue(orchestrators[0].disconnectCalled)
        assertTrue(orchestrators[1].disconnectCalled)
        assertTrue(executor.released)
    }
}
