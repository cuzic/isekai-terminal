package com.example.imespike

import android.app.Application
import androidx.test.ext.junit.runners.AndroidJUnit4
import androidx.test.platform.app.InstrumentationRegistry
import com.example.imespike.data.ConnectionProfile
import com.example.imespike.data.Repositories
import com.example.imespike.session.TerminalSession
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.withTimeout
import org.junit.After
import org.junit.Assert.*
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith

@RunWith(AndroidJUnit4::class)
class TerminalViewModelTest {
    private lateinit var vm: TerminalViewModel
    private lateinit var fakeGateway: FakeSshGateway
    private lateinit var fakeHostKeyChecker: FakeHostKeyChecker

    @Before
    fun setup() {
        val app = InstrumentationRegistry.getInstrumentation()
            .targetContext.applicationContext as Application
        Repositories.init(app)
        runBlocking {
            Repositories.profiles.getAll().forEach { Repositories.profiles.delete(it) }
            Repositories.keys.getAll().forEach { Repositories.keys.delete(it) }
        }
        fakeGateway = FakeSshGateway()
        fakeHostKeyChecker = FakeHostKeyChecker()
        InstrumentationRegistry.getInstrumentation().runOnMainSync {
            vm = TerminalViewModel(app, TerminalSession(fakeGateway, fakeHostKeyChecker))
        }
    }

    @After
    fun teardown() {
        InstrumentationRegistry.getInstrumentation().runOnMainSync { vm.disconnect() }
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

    // ── 認証エラー（FakeSshGateway が接続前に検出）─────────────────

    @Test
    fun connectProfile_passwordAuth_emptyPassword_setsError() = runBlocking {
        val profile = ConnectionProfile(label = "test", host = "host", username = "user", authType = "password")
        vm.connectProfile(profile, "")
        val state = awaitError()
        assertEquals("パスワードが必要です", state.statusMsg)
        assertFalse("session should not be created on auth error", fakeGateway.session.connectCalled)
    }

    @Test
    fun connectProfile_passwordAuth_nullPassword_setsError() = runBlocking {
        val profile = ConnectionProfile(label = "test", host = "host", username = "user", authType = "password")
        vm.connectProfile(profile, null)
        val state = awaitError()
        assertEquals("パスワードが必要です", state.statusMsg)
        assertFalse(fakeGateway.session.connectCalled)
    }

    @Test
    fun connectProfile_keyAuth_noKeyId_setsError() = runBlocking {
        val profile = ConnectionProfile(label = "test", host = "host", username = "user", authType = "key", keyId = null)
        vm.connectProfile(profile, null)
        val state = awaitError()
        assertEquals("鍵IDが未設定です", state.statusMsg)
        assertFalse(fakeGateway.session.connectCalled)
    }

    @Test
    fun connectProfile_keyAuth_keyNotInDb_setsError() = runBlocking {
        val profile = ConnectionProfile(label = "test", host = "host", username = "user", authType = "key", keyId = 99999L)
        vm.connectProfile(profile, null)
        val state = awaitError()
        assertTrue("expected 鍵エラー but was ${state.statusMsg}", state.statusMsg.contains("鍵エラー"))
    }

    @Test
    fun connectProfile_unknownAuthType_setsError() = runBlocking {
        val profile = ConnectionProfile(label = "test", host = "host", username = "user", authType = "agent")
        vm.connectProfile(profile, null)
        val state = awaitError()
        assertTrue("expected 未知の認証タイプ but was ${state.statusMsg}", state.statusMsg.contains("未知の認証タイプ"))
        assertFalse(fakeGateway.session.connectCalled)
    }

    // ── 接続成功シミュレーション ───────────────────────────────────

    @Test
    fun connect_withFakeGateway_onConnected_setsConnectedState() = runBlocking {
        val profile = ConnectionProfile(label = "test", host = "192.168.1.1", username = "user", authType = "password")
        vm.connectProfile(profile, "pass")

        // FakeSshSession.connect() が呼ばれるのを待つ
        withTimeout(3000) {
            while (!fakeGateway.session.connectCalled) kotlinx.coroutines.delay(10)
        }

        // Rust 接続成功コールバックをシミュレート
        fakeGateway.session.simulateConnected()

        val state = awaitState { it.connected }
        assertTrue(state.connected)
        assertTrue(state.statusMsg.contains("192.168.1.1"))
    }

    @Test
    fun connect_withFakeGateway_onDisconnected_clearsConnectedState() = runBlocking {
        val profile = ConnectionProfile(label = "test", host = "192.168.1.1", username = "user", authType = "password")
        vm.connectProfile(profile, "pass")
        withTimeout(3000) { while (!fakeGateway.session.connectCalled) kotlinx.coroutines.delay(10) }

        fakeGateway.session.simulateConnected()
        awaitState { it.connected }

        fakeGateway.session.simulateDisconnected("server closed")

        val state = awaitState { !it.connected }
        assertFalse(state.connected)
        assertTrue(state.statusMsg.contains("server closed"))
        assertNull(state.screenUpdate)
    }

    @Test
    fun send_afterConnected_delegatesToFakeSession() = runBlocking {
        val profile = ConnectionProfile(label = "test", host = "192.168.1.1", username = "user", authType = "password")
        vm.connectProfile(profile, "pass")
        withTimeout(3000) { while (!fakeGateway.session.connectCalled) kotlinx.coroutines.delay(10) }
        fakeGateway.session.simulateConnected()
        awaitState { it.connected }

        val bytes = byteArrayOf(0x0D)
        vm.send(bytes)

        assertTrue(fakeGateway.session.sentBytes.any { it.contentEquals(bytes) })
    }

    // ── 切断 ──────────────────────────────────────────────────────

    @Test
    fun disconnect_whenNotConnected_setsDisconnectedMsg() {
        InstrumentationRegistry.getInstrumentation().runOnMainSync { vm.disconnect() }
        assertEquals("切断済み", vm.uiState.value.statusMsg)
        assertFalse(vm.uiState.value.connected)
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
