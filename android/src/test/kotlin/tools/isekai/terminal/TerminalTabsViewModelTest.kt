package tools.isekai.terminal

import android.app.Application
import androidx.test.core.app.ApplicationProvider
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.ExperimentalCoroutinesApi
import kotlinx.coroutines.delay
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.test.TestCoroutineScheduler
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
import tools.isekai.terminal.data.Repositories
import tools.isekai.terminal.data.Snippet
import tools.isekai.terminal.session.AppExecutor
import tools.isekai.terminal.session.TerminalSession
import uniffi.isekai_terminal_core.TransportPreference

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

    // viewModelScope (Dispatchers.Main.immediate) 上の delay() (接続後自動実行コマンドの
    // デバウンス) を進めるための仮想クロック。UnconfinedTestDispatcher() を素の runBlocking
    // から使うだけでは delay() が誰にも進めてもらえず永遠に止まるため、scheduler を明示的に
    // 保持し advanceUntilIdle() で駆動する。
    private lateinit var testScheduler: TestCoroutineScheduler

    @Before
    fun setup() {
        testScheduler = TestCoroutineScheduler()
        Dispatchers.setMain(UnconfinedTestDispatcher(testScheduler))
        val app = ApplicationProvider.getApplicationContext<Application>()
        Repositories.init(app)
        runBlocking {
            Repositories.profiles.getAll().forEach { Repositories.profiles.delete(it) }
            Repositories.snippets.getAll().forEach { Repositories.snippets.delete(it) }
        }
        executor = DumbAppExecutor()
        val sessionFactory: (AppExecutor) -> TerminalSession = {
            val fake = FakeOrchestrator()
            orchestrators.add(fake)
            TerminalSession(FakeHostKeyChecker(), orchestratorFactory = { cb -> fake.also { it.callback = cb } })
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

    /** connect呼び出し後、同じconnectTab()コルーチン内で続けて呼ばれるsetSessionThemeが
     *  実際に届くまで待つ(connectCalledとpushThemeToSessionの間には短いスケジューリング
     *  遅延があり得るため、Dispatchers.IOスレッドの実行が遅れがちな高負荷環境では
     *  awaitConnectCalledの直後に同期的読みするだけでは早すぎることがある)。 */
    private suspend fun awaitSetSessionThemeCalled(o: FakeOrchestrator) =
        withTimeout(3000) { while (o.setSessionThemeCalls.isEmpty()) kotlinx.coroutines.delay(10) }

    private fun tab(tabId: String) = vm.tabs.value.first { it.tabId == tabId }

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
    fun onNetworkPathChanged_fansOutToAllConnectedTabs() = runBlocking {
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
    fun onNetworkPathChanged_withNoTabs_doesNotThrow() {
        vm.onNetworkPathChanged(isSatisfied = false)
    }

    @Test
    fun onNetworkPathChanged_availableFansOutToAllTabsWithoutDisconnecting() = runBlocking {
        vm.openTab(profile("a"), "pass")
        vm.openTab(profile("b"), "pass")
        awaitConnectCalled(orchestrators[0])
        awaitConnectCalled(orchestrators[1])
        orchestrators[0].simulateConnected("host-a")
        orchestrators[1].simulateConnected("host-b")

        executor.simulateNetworkAvailable()

        assertFalse("tab a should not be disconnected on recovery", orchestrators[0].disconnectCalled)
        assertFalse("tab b should not be disconnected on recovery", orchestrators[1].disconnectCalled)
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

    // ── Phase 9-4: 物理マルチパス（実験的機能）─────────────────────────

    @Test
    fun connectTab_multipathTransport_physicalMultipathEnabled_acquiresPhysicalFds() = runBlocking {
        executor.physicalMultipathFds = tools.isekai.terminal.session.PhysicalMultipathFds(
            wifiFd = 42, wifiLocalIp = "192.168.1.5",
        )
        val p = profile("a").copy(
            transportPreferenceName = TransportPreference.ISEKAI_PIPE_QUIC_MULTIPATH.name,
            enablePhysicalMultipath = true,
        )
        vm.openTab(p, "pass")

        withTimeout(3000) { while (!orchestrators[0].connectMultipathIsekaiPipeQuicCalled) delay(10) }

        assertEquals(1, executor.acquirePhysicalMultipathFdsCallCount)
    }

    @Test
    fun connectTab_multipathTransport_physicalMultipathDisabled_doesNotAcquirePhysicalFds() = runBlocking {
        val p = profile("a").copy(
            transportPreferenceName = TransportPreference.ISEKAI_PIPE_QUIC_MULTIPATH.name,
            enablePhysicalMultipath = false,
        )
        vm.openTab(p, "pass")

        withTimeout(3000) { while (!orchestrators[0].connectMultipathIsekaiPipeQuicCalled) delay(10) }

        assertEquals(0, executor.acquirePhysicalMultipathFdsCallCount)
    }

    // ── Phase 10: STUN+SSHランデブー方式・relay経由のP2P ─────────────────

    @Test
    fun connectTab_stunP2pTransport_dispatchesToConnectIsekaiStunP2p() = runBlocking {
        val p = profile("a").copy(
            transportPreferenceName = TransportPreference.ISEKAI_STUN_P2P_QUIC.name,
            stunServer = "stun.example.com:3478",
        )
        vm.openTab(p, "pass")

        withTimeout(3000) { while (!orchestrators[0].connectIsekaiStunP2pCalled) delay(10) }

        assertTrue(orchestrators[0].connectIsekaiStunP2pCalled)
        assertFalse(orchestrators[0].connectCalled)
    }

    @Test
    fun connectTab_relayTransport_dispatchesToConnectIsekaiLinkRelay() = runBlocking {
        val p = profile("a").copy(
            transportPreferenceName = TransportPreference.ISEKAI_LINK_RELAY_QUIC.name,
            relayAddr = "relay.example.com:443",
            relaySni = "relay.example.com",
            relayJwt = "eyJhbGciOiJSUzI1NiJ9.test.sig",
        )
        vm.openTab(p, "pass")

        withTimeout(3000) { while (!orchestrators[0].connectIsekaiLinkRelayCalled) delay(10) }

        assertTrue(orchestrators[0].connectIsekaiLinkRelayCalled)
        assertFalse(orchestrators[0].connectCalled)
    }

    @Test
    fun disconnect_afterConnected_releasesPhysicalMultipathFds() = runBlocking {
        val id = vm.openTab(profile("a"), "pass")
        awaitConnectCalled(orchestrators[0])
        orchestrators[0].simulateConnected("host-a")
        withTimeout(3000) { while (!tab(id).session.state.value.connected) delay(10) }

        orchestrators[0].simulateDisconnected("bye")
        withTimeout(3000) { while (tab(id).session.state.value.connected) delay(10) }

        assertTrue(executor.releasePhysicalMultipathFdsCalled)
    }

    // ── 定型コマンド（スニペット）─────────────────────────────────

    @Test
    fun sendSnippet_appendNewlineTrue_sendsCommandFollowedByCr() = runBlocking {
        val id = vm.openTab(profile("a"), "pass")
        awaitConnectCalled(orchestrators[0])
        orchestrators[0].simulateConnected()
        withTimeout(3000) { while (!tab(id).session.state.value.connected) delay(10) }

        vm.sendSnippet(id, Snippet(label = "list", command = "ls -la", appendNewline = true))

        assertTrue(orchestrators[0].sentBytes.any { it.toString(Charsets.UTF_8) == "ls -la\r" })
    }

    @Test
    fun sendSnippet_appendNewlineFalse_sendsCommandWithoutTrailingCr() = runBlocking {
        val id = vm.openTab(profile("a"), "pass")
        awaitConnectCalled(orchestrators[0])
        orchestrators[0].simulateConnected()
        withTimeout(3000) { while (!tab(id).session.state.value.connected) delay(10) }

        vm.sendSnippet(id, Snippet(label = "partial", command = "echo hi", appendNewline = false))

        assertTrue(orchestrators[0].sentBytes.any { it.toString(Charsets.UTF_8) == "echo hi" })
    }

    @Test
    fun connectTab_loadsSnippetsForThatProfile() = runBlocking {
        val profileId = Repositories.profiles.save(profile("web"))
        Repositories.snippets.save(Snippet(label = "web-only", command = "tail -f log", profileId = profileId))
        val savedProfile = Repositories.profiles.findById(profileId)!!

        val id = vm.openTab(savedProfile, "pass")

        withTimeout(3000) { while (tab(id).snippets.value.isEmpty()) delay(10) }
        assertEquals(listOf("web-only"), tab(id).snippets.value.map { it.label })
    }

    // ── 接続後自動実行コマンド ────────────────────────────────────

    @Test
    fun connectTab_withPostConnectCommands_sendsThemOnceConnected() = runBlocking {
        val p = profile("a").copy(postConnectCommands = "echo hello\nls -la")
        val id = vm.openTab(p, "pass")
        awaitConnectCalled(orchestrators[0])
        orchestrators[0].simulateConnected()
        withTimeout(3000) { while (!tab(id).session.state.value.connected) delay(10) }
        testScheduler.advanceUntilIdle()

        withTimeout(3000) {
            while (orchestrators[0].sentBytes.none { it.toString(Charsets.UTF_8) == "echo hello\rls -la\r" }) {
                delay(20)
            }
        }
    }

    @Test
    fun connectTab_withoutPostConnectCommands_sendsNothingAutomatically() = runBlocking {
        val id = vm.openTab(profile("a"), "pass")
        awaitConnectCalled(orchestrators[0])
        orchestrators[0].simulateConnected()
        withTimeout(3000) { while (!tab(id).session.state.value.connected) delay(10) }

        delay(700) // 十分にデバウンス時間を超えて待つ
        assertTrue(orchestrators[0].sentBytes.isEmpty())
    }

    @Test
    fun postConnectCommands_internalResumeWithoutNewOpenTabCall_doesNotResend() = runBlocking {
        // セッション単位で1回だけ実行するフラグの検証:
        // Kotlin 側から openTab()/reconnect() を呼び直さずに Rust 側が内部的に
        // 切断→再接続（resume）した場合、post_connect_commands は再送されないべき。
        val p = profile("a").copy(postConnectCommands = "echo once")
        val id = vm.openTab(p, "pass")
        awaitConnectCalled(orchestrators[0])
        orchestrators[0].simulateConnected()
        withTimeout(3000) { while (!tab(id).session.state.value.connected) delay(10) }
        testScheduler.advanceUntilIdle()
        withTimeout(3000) {
            while (orchestrators[0].sentBytes.none { it.toString(Charsets.UTF_8) == "echo once\r" }) delay(20)
        }

        orchestrators[0].simulateDisconnected("network blip")
        withTimeout(3000) { while (tab(id).session.state.value.connected) delay(10) }
        orchestrators[0].simulateConnected()
        withTimeout(3000) { while (!tab(id).session.state.value.connected) delay(10) }
        testScheduler.advanceUntilIdle()
        delay(700)

        val matching = orchestrators[0].sentBytes.count { it.toString(Charsets.UTF_8) == "echo once\r" }
        assertEquals(1, matching)
    }

    @Test
    fun reconnect_calledAgainAfterDisconnect_resendsPostConnectCommandsForNewSession() = runBlocking {
        // 明示的な再接続（新しい reconnect() 呼び出し）は新セッション扱いなので、
        // 各セッションごとに1回ずつ実行されてよい。
        val p = profile("a").copy(postConnectCommands = "echo hi")
        val id = vm.openTab(p, "pass")
        awaitConnectCalled(orchestrators[0])
        orchestrators[0].simulateConnected()
        withTimeout(3000) { while (!tab(id).session.state.value.connected) delay(10) }
        testScheduler.advanceUntilIdle()
        withTimeout(3000) {
            while (orchestrators[0].sentBytes.none { it.toString(Charsets.UTF_8) == "echo hi\r" }) delay(20)
        }

        vm.disconnect(id)
        withTimeout(3000) { while (tab(id).session.state.value.connected) delay(10) }

        vm.reconnect(id, "pass")
        orchestrators[0].simulateConnected()
        withTimeout(3000) { while (!tab(id).session.state.value.connected) delay(10) }
        testScheduler.advanceUntilIdle()
        delay(700)

        val matching = orchestrators[0].sentBytes.count { it.toString(Charsets.UTF_8) == "echo hi\r" }
        assertEquals(2, matching)
    }

    // ── Phase 12 P2-1: per-session/per-hostのterminal theme ──────────────

    @Test
    fun openTab_withoutProfileTheme_appliesGlobalDefaultAndIsNotOverridden() = runBlocking {
        val id = vm.openTab(profile("a"), "pass")
        awaitConnectCalled(orchestrators[0])
        awaitSetSessionThemeCalled(orchestrators[0])

        assertFalse(tab(id).isThemeOverridden)
        assertEquals(1, orchestrators[0].setSessionThemeCalls.size)
    }

    @Test
    fun openTab_withProfileTheme_appliesItAndMarksOverridden() = runBlocking {
        val p = profile("a").copy(themeName = tools.isekai.terminal.ui.TerminalThemes.DRACULA.name)
        val id = vm.openTab(p, "pass")
        awaitConnectCalled(orchestrators[0])
        awaitSetSessionThemeCalled(orchestrators[0])

        assertTrue(tab(id).isThemeOverridden)
        assertEquals(tools.isekai.terminal.ui.TerminalThemes.DRACULA, tab(id).currentTheme.value)
        val (ansi16, fg, bg) = orchestrators[0].setSessionThemeCalls.last()
        assertEquals(tools.isekai.terminal.ui.TerminalThemes.DRACULA.ansi16Argb(), ansi16)
        assertEquals(tools.isekai.terminal.ui.TerminalThemes.DRACULA.foregroundArgb(), fg)
        assertEquals(tools.isekai.terminal.ui.TerminalThemes.DRACULA.backgroundArgb(), bg)
    }

    @Test
    fun setTabTheme_marksOverriddenAndPushesToSession() = runBlocking {
        val id = vm.openTab(profile("a"), "pass")
        awaitConnectCalled(orchestrators[0])
        val callsBefore = orchestrators[0].setSessionThemeCalls.size

        vm.setTabTheme(id, tools.isekai.terminal.ui.TerminalThemes.NORD)

        assertTrue(tab(id).isThemeOverridden)
        assertEquals(tools.isekai.terminal.ui.TerminalThemes.NORD, tab(id).currentTheme.value)
        assertEquals(callsBefore + 1, orchestrators[0].setSessionThemeCalls.size)
    }

    @Test
    fun applyGlobalThemeToNonOverriddenTabs_skipsOverriddenTabs() = runBlocking {
        val followingId = vm.openTab(profile("a"), "pass")
        awaitConnectCalled(orchestrators[0])
        val overriddenId = vm.openTab(profile("b"), "pass")
        awaitConnectCalled(orchestrators[1])
        vm.setTabTheme(overriddenId, tools.isekai.terminal.ui.TerminalThemes.DRACULA)

        vm.applyGlobalThemeToNonOverriddenTabs(tools.isekai.terminal.ui.TerminalThemes.SOLARIZED_DARK)

        assertEquals(tools.isekai.terminal.ui.TerminalThemes.SOLARIZED_DARK, tab(followingId).currentTheme.value)
        assertEquals(tools.isekai.terminal.ui.TerminalThemes.DRACULA, tab(overriddenId).currentTheme.value)
    }
}
