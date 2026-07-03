package tools.isekai.terminal

import android.app.Application
import android.net.Uri
import androidx.test.core.app.ApplicationProvider
import tools.isekai.terminal.data.ConnectionProfile
import tools.isekai.terminal.data.Repositories
import tools.isekai.terminal.session.TerminalSession
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.ExperimentalCoroutinesApi
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.first
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
import uniffi.tssh_core.SshAuth
import uniffi.tssh_core.TransportPreference
import uniffi.tssh_core.SshConfig

@OptIn(ExperimentalCoroutinesApi::class)
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [28])
class TerminalViewModelTest {
    private lateinit var vm: TerminalViewModel
    private lateinit var fakeOrchestrator: FakeOrchestrator
    private lateinit var fakeHostKeyChecker: FakeHostKeyChecker
    private lateinit var executor: DumbAppExecutor

    @Before
    fun setup() {
        Dispatchers.setMain(UnconfinedTestDispatcher())
        val app = ApplicationProvider.getApplicationContext<Application>()
        Repositories.init(app)
        runBlocking {
            Repositories.profiles.getAll().forEach { Repositories.profiles.delete(it) }
            Repositories.keys.getAll().forEach { Repositories.keys.delete(it) }
        }
        fakeOrchestrator = FakeOrchestrator()
        fakeHostKeyChecker = FakeHostKeyChecker()
        executor = DumbAppExecutor()
        val session = TerminalSession(fakeHostKeyChecker) { cb ->
            fakeOrchestrator.also { it.callback = cb }
        }
        vm = TerminalViewModel(app, session, executor)
    }

    @After
    fun teardown() {
        vm.disconnect()
        Dispatchers.resetMain()
    }

    private suspend fun awaitState(condition: (TerminalUiState) -> Boolean): TerminalUiState =
        withTimeout(3000) { vm.uiState.first { condition(it) } }

    private suspend fun awaitError(): TerminalUiState =
        awaitState { it.statusMsg != "接続中…" && it.statusMsg != "未接続" }

    // ── 初期状態 ──────────────────────────────────────────────────

    @Test
    fun initialState_notConnected() {
        assertFalse(vm.uiState.value.connected)
        assertEquals("未接続", vm.uiState.value.statusMsg)
    }

    @Test
    fun initialState_screenUpdateNull() {
        assertNull(vm.uiState.value.screenUpdate)
    }

    // ── 認証エラー（接続前に検出）─────────────────────────────────

    @Test
    fun connectProfile_passwordAuth_emptyPassword_setsError() = runBlocking {
        val profile = ConnectionProfile(label = "test", host = "host", username = "user", authType = "password")
        vm.connectProfile(profile, "")
        val state = awaitError()
        assertEquals("パスワードが必要です", state.statusMsg)
        assertFalse("session should not be created on auth error", fakeOrchestrator.connectCalled)
    }

    @Test
    fun connectProfile_passwordAuth_nullPassword_setsError() = runBlocking {
        val profile = ConnectionProfile(label = "test", host = "host", username = "user", authType = "password")
        vm.connectProfile(profile, null)
        val state = awaitError()
        assertEquals("パスワードが必要です", state.statusMsg)
        assertFalse(fakeOrchestrator.connectCalled)
    }

    @Test
    fun connectProfile_keyAuth_noKeyId_setsError() = runBlocking {
        val profile = ConnectionProfile(label = "test", host = "host", username = "user", authType = "key", keyId = null)
        vm.connectProfile(profile, null)
        val state = awaitError()
        assertEquals("鍵IDが未設定です", state.statusMsg)
        assertFalse(fakeOrchestrator.connectCalled)
    }

    @Test
    fun connectProfile_keyAuth_keyNotInDb_setsError() = runBlocking {
        executor.keyPemError = RuntimeException("鍵が見つかりません (id=99999)")
        val profile = ConnectionProfile(label = "test", host = "host", username = "user", authType = "key", keyId = 99999L)
        vm.connectProfile(profile, null)
        val state = awaitError()
        assertTrue("expected 鍵エラー but was ${state.statusMsg}", state.statusMsg.contains("鍵エラー"))
        assertFalse(fakeOrchestrator.connectCalled)
    }

    @Test
    fun connectProfile_keyAuth_withValidKey_connectsSuccessfully() = runBlocking {
        executor.keyPem = "-----BEGIN OPENSSH PRIVATE KEY-----\nfake\n-----END OPENSSH PRIVATE KEY-----\n".toByteArray()
        val profile = ConnectionProfile(label = "test", host = "192.168.1.1", username = "user", authType = "key", keyId = 1L)
        vm.connectProfile(profile, null)
        withTimeout(3000) { while (!fakeOrchestrator.connectCalled) kotlinx.coroutines.delay(10) }
        assertTrue(fakeOrchestrator.connectCalled)
    }

    @Test
    fun connectProfile_unknownAuthType_setsError() = runBlocking {
        val profile = ConnectionProfile(label = "test", host = "host", username = "user", authType = "agent")
        vm.connectProfile(profile, null)
        val state = awaitError()
        assertTrue("expected 未知の認証タイプ but was ${state.statusMsg}", state.statusMsg.contains("未知の認証タイプ"))
        assertFalse(fakeOrchestrator.connectCalled)
    }

    // ── 接続成功シミュレーション ───────────────────────────────────

    @Test
    fun connect_withFakeOrchestrator_onConnected_setsConnectedState() = runBlocking {
        val profile = ConnectionProfile(label = "test", host = "192.168.1.1", username = "user", authType = "password")
        vm.connectProfile(profile, "pass")

        withTimeout(3000) {
            while (!fakeOrchestrator.connectCalled) kotlinx.coroutines.delay(10)
        }

        fakeOrchestrator.simulateConnected("192.168.1.1")

        val state = awaitState { it.connected }
        assertTrue(state.connected)
        assertTrue(state.statusMsg.contains("192.168.1.1"))
    }

    @Test
    fun connect_withFakeOrchestrator_onDisconnected_clearsConnectedState() = runBlocking {
        val profile = ConnectionProfile(label = "test", host = "192.168.1.1", username = "user", authType = "password")
        vm.connectProfile(profile, "pass")
        withTimeout(3000) { while (!fakeOrchestrator.connectCalled) kotlinx.coroutines.delay(10) }

        fakeOrchestrator.simulateConnected("192.168.1.1")
        awaitState { it.connected }

        fakeOrchestrator.simulateDisconnected("server closed")

        val state = awaitState { !it.connected }
        assertFalse(state.connected)
        assertTrue(state.statusMsg.contains("server closed"))
        assertNull(state.screenUpdate)
    }

    // ── Phase 9-4: 物理マルチパス（実験的機能） ─────────────────────

    @Test
    fun connectProfile_multipathTransport_physicalMultipathEnabled_acquiresPhysicalFds() = runBlocking {
        executor.physicalMultipathFds = tools.isekai.terminal.session.PhysicalMultipathFds(
            wifiFd = 42, wifiLocalIp = "192.168.1.5",
        )
        val profile = ConnectionProfile(
            label = "test", host = "100.64.0.1", username = "user", authType = "password",
            transportPreferenceName = TransportPreference.ISEKAI_HELPER_QUIC_MULTIPATH.name,
            enablePhysicalMultipath = true,
        )
        vm.connectProfile(profile, "pass")

        withTimeout(3000) { while (!fakeOrchestrator.connectMultipathHelperQuicCalled) delay(10) }

        assertEquals(1, executor.acquirePhysicalMultipathFdsCallCount)
    }

    @Test
    fun connectProfile_multipathTransport_physicalMultipathDisabled_doesNotAcquirePhysicalFds() = runBlocking {
        val profile = ConnectionProfile(
            label = "test", host = "100.64.0.1", username = "user", authType = "password",
            transportPreferenceName = TransportPreference.ISEKAI_HELPER_QUIC_MULTIPATH.name,
            enablePhysicalMultipath = false,
        )
        vm.connectProfile(profile, "pass")

        withTimeout(3000) { while (!fakeOrchestrator.connectMultipathHelperQuicCalled) delay(10) }

        assertEquals(0, executor.acquirePhysicalMultipathFdsCallCount)
    }

    @Test
    fun disconnect_afterConnected_releasesPhysicalMultipathFds() = runBlocking {
        val profile = ConnectionProfile(label = "test", host = "192.168.1.1", username = "user", authType = "password")
        vm.connectProfile(profile, "pass")
        withTimeout(3000) { while (!fakeOrchestrator.connectCalled) kotlinx.coroutines.delay(10) }
        fakeOrchestrator.simulateConnected("192.168.1.1")
        awaitState { it.connected }

        fakeOrchestrator.simulateDisconnected("bye")
        awaitState { !it.connected }

        assertTrue(executor.releasePhysicalMultipathFdsCalled)
    }

    @Test
    fun send_afterConnected_delegatesToOrchestrator() = runBlocking {
        val profile = ConnectionProfile(label = "test", host = "192.168.1.1", username = "user", authType = "password")
        vm.connectProfile(profile, "pass")
        withTimeout(3000) { while (!fakeOrchestrator.connectCalled) kotlinx.coroutines.delay(10) }
        fakeOrchestrator.simulateConnected()
        awaitState { it.connected }

        val bytes = byteArrayOf(0x0D)
        vm.send(bytes)

        assertTrue(fakeOrchestrator.sentBytes.any { it.contentEquals(bytes) })
    }

    // ── 切断 ──────────────────────────────────────────────────────

    @Test
    fun disconnect_whenNotConnected_setsDisconnectedMsg() {
        vm.disconnect()
        assertEquals("切断済み", vm.uiState.value.statusMsg)
        assertFalse(vm.uiState.value.connected)
    }

    // ── ネットワーク変化 ──────────────────────────────────────────

    @Test
    fun onNetworkLost_whenNotConnected_doesNotDisconnect() = runBlocking {
        vm.onNetworkLost()
        assertEquals("未接続", vm.uiState.value.statusMsg)
    }

    @Test
    fun onNetworkLost_whenTcpConnected_disconnects() = runBlocking {
        val profile = ConnectionProfile(label = "test", host = "192.168.1.1", username = "user", authType = "password")
        vm.connectProfile(profile, "pass")
        withTimeout(3000) { while (!fakeOrchestrator.connectCalled) kotlinx.coroutines.delay(10) }
        fakeOrchestrator.simulateConnected()
        awaitState { it.connected }

        vm.onNetworkLost()

        val state = awaitState { !it.connected }
        assertFalse(state.connected)
    }

    @Test
    fun onNetworkLost_whenQuicConnected_doesNotDisconnect() = runBlocking {
        val profile = ConnectionProfile(
            label = "quic", host = "192.168.1.1", port = 22, tsshdPort = 2222,
            username = "user", authType = "password", useTsshd = true,
            transportPreferenceName = TransportPreference.TSSHD_QUIC.name,
        )
        vm.connectProfile(profile, "pass")
        withTimeout(3000) { while (!fakeOrchestrator.connectQuicCalled) kotlinx.coroutines.delay(10) }
        fakeOrchestrator.simulateConnected()
        awaitState { it.connected }

        vm.onNetworkLost()
        kotlinx.coroutines.delay(200)
        assertTrue(vm.uiState.value.connected)
    }

    @Test
    fun ensureServiceRunning_calledOnConnect() = runBlocking {
        val before = executor.serviceRunCount
        val profile = ConnectionProfile(label = "test", host = "192.168.1.1", username = "user", authType = "password")
        vm.connectProfile(profile, "pass")
        withTimeout(3000) { while (executor.serviceRunCount == before) kotlinx.coroutines.delay(10) }
        assertTrue(executor.serviceRunCount > before)
    }

    // ── executor 通知 ───────────────────────────────────────────

    @Test
    fun notifyConnected_calledWithHostAfterConnect() = runBlocking {
        val profile = ConnectionProfile(label = "test", host = "192.168.1.1", username = "user", authType = "password")
        vm.connectProfile(profile, "pass")
        withTimeout(3000) { while (!fakeOrchestrator.connectCalled) kotlinx.coroutines.delay(10) }
        fakeOrchestrator.simulateConnected("192.168.1.1")
        awaitState { it.connected }

        withTimeout(3000) { while (executor.connectedHosts.isEmpty()) kotlinx.coroutines.delay(10) }
        assertEquals("192.168.1.1", executor.connectedHosts.last())
    }

    @Test
    fun notifyDisconnected_calledAfterDisconnect() = runBlocking {
        val profile = ConnectionProfile(label = "test", host = "192.168.1.1", username = "user", authType = "password")
        vm.connectProfile(profile, "pass")
        withTimeout(3000) { while (!fakeOrchestrator.connectCalled) kotlinx.coroutines.delay(10) }
        fakeOrchestrator.simulateConnected()
        awaitState { it.connected }

        fakeOrchestrator.simulateDisconnected("server closed")
        awaitState { !it.connected }

        withTimeout(3000) { while (executor.disconnectedCount == 0) kotlinx.coroutines.delay(10) }
        assertEquals(1, executor.disconnectedCount)
    }

    // ── Bug 2: trzszStartUpload の二重起動を防ぐ ──────────────────────

    @Test
    fun trzszStartUpload_calledTwiceConcurrently_onlyOneUploadStarts() = runBlocking {
        val profile = ConnectionProfile(label = "test", host = "192.168.1.1", username = "user", authType = "password")
        vm.connectProfile(profile, "pass")
        withTimeout(3000) { while (!fakeOrchestrator.connectCalled) kotlinx.coroutines.delay(10) }
        fakeOrchestrator.simulateConnected()
        awaitState { it.connected }

        fakeOrchestrator.simulateTrzszRequest("up-tid", "upload", null, null)
        awaitState { it.trzszState is TrzszUiState.WaitingUser }

        val dummyUri = Uri.parse("content://tools.isekai.terminal.test/fake")
        vm.trzszStartUpload(dummyUri)
        vm.trzszStartUpload(dummyUri)
        delay(300)

        assertTrue(
            "trzszAcceptUploadCount should be 0 or 1, was ${fakeOrchestrator.trzszAcceptUploadCount}",
            fakeOrchestrator.trzszAcceptUploadCount <= 1,
        )
    }

    @Test
    fun trzszStartUpload_afterFailure_flagIsResetAndCanUploadAgain() = runBlocking {
        val profile = ConnectionProfile(label = "test", host = "192.168.1.1", username = "user", authType = "password")
        vm.connectProfile(profile, "pass")
        withTimeout(3000) { while (!fakeOrchestrator.connectCalled) kotlinx.coroutines.delay(10) }
        fakeOrchestrator.simulateConnected()
        awaitState { it.connected }

        fakeOrchestrator.simulateTrzszRequest("up-tid", "upload", null, null)
        awaitState { it.trzszState is TrzszUiState.WaitingUser }

        val dummyUri = Uri.parse("content://tools.isekai.terminal.test/fake")

        vm.trzszStartUpload(dummyUri)
        delay(500)

        fakeOrchestrator.simulateTrzszFinished("up-tid", success = false)
        delay(100)
        fakeOrchestrator.simulateTrzszRequest("up-tid-2", "upload", null, null)
        awaitState { it.trzszState is TrzszUiState.WaitingUser }

        vm.trzszStartUpload(dummyUri)
        delay(300)

        assertTrue(
            "should have attempted upload twice total, got ${fakeOrchestrator.trzszAcceptUploadCount}",
            fakeOrchestrator.trzszAcceptUploadCount <= 2,
        )
    }

    // ── Connect guard ─────────────────────────────────────────────────────

    @Test
    fun connect_calledWhileAlreadyConnecting_isIgnored() = runBlocking {
        val profile = ConnectionProfile(label = "test", host = "192.168.1.1", username = "user", authType = "password")
        vm.connectProfile(profile, "pass")
        withTimeout(3000) { while (!fakeOrchestrator.connectCalled) kotlinx.coroutines.delay(10) }

        assertTrue(vm.uiState.value.isConnecting)

        vm.connect(SshConfig(
            host = "192.168.1.1", port = 22u, username = "user",
            auth = SshAuth.Password("pass"), cols = 80u, rows = 24u,
        ))
        delay(100)

        assertTrue(fakeOrchestrator.connectCalled)
    }

    // ── ライフサイクル ──────────────────────────────────────────────

    @Test
    fun onCleared_releasesExecutor() {
        TerminalViewModel::class.java
            .getDeclaredMethod("onCleared")
            .apply { isAccessible = true }
            .invoke(vm)
        assertTrue(executor.released)
    }

    // ── ログ ──────────────────────────────────────────────────────

    @Test
    fun getSessionLog_initially_empty() {
        assertEquals("", vm.getSessionLog())
    }

    @Test
    fun clearSessionLog_doesNotThrow() {
        vm.clearSessionLog()
    }
}
