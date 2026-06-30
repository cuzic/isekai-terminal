package com.example.imespike

import com.example.imespike.session.TerminalReducer
import org.junit.Assert.*
import org.junit.Test

class TerminalReducerTest {

    private val initial = TerminalUiState()

    // ── connecting ─────────────────────────────────────────────────

    @Test
    fun `connecting sets statusMsg`() {
        val next = TerminalReducer.connecting(initial)
        assertEquals("接続中…", next.statusMsg)
        assertFalse(next.connected)
    }

    @Test
    fun `connecting preserves scrollbackLen`() {
        val state = initial.copy(scrollbackLen = 10)
        val next = TerminalReducer.connecting(state)
        assertEquals(10, next.scrollbackLen)
    }

    // ── connected ──────────────────────────────────────────────────

    @Test
    fun `connected sets connected flag and host in statusMsg`() {
        val next = TerminalReducer.connected(initial, "example.com")
        assertTrue(next.connected)
        assertTrue(next.statusMsg.contains("example.com"))
    }

    @Test
    fun `connected stores currentHost`() {
        val next = TerminalReducer.connected(initial, "example.com")
        assertEquals("example.com", next.currentHost)
    }

    // ── disconnected ───────────────────────────────────────────────

    @Test
    fun `disconnected clears connected flag and screenUpdate`() {
        val state = initial.copy(connected = true, screenUpdate = null)
        val next = TerminalReducer.disconnected(state, "timeout")
        assertFalse(next.connected)
        assertNull(next.screenUpdate)
        assertTrue(next.statusMsg.contains("timeout"))
    }

    @Test
    fun `disconnected with null reason uses fallback text`() {
        val next = TerminalReducer.disconnected(initial, null)
        assertTrue(next.statusMsg.contains("不明"))
    }

    @Test
    fun `disconnected clears currentHost`() {
        val state = initial.copy(currentHost = "example.com")
        val next = TerminalReducer.disconnected(state, null)
        assertNull(next.currentHost)
    }

    // ── error ──────────────────────────────────────────────────────

    @Test
    fun `error sets statusMsg with prefix`() {
        val next = TerminalReducer.error(initial, "connection refused")
        assertTrue(next.statusMsg.contains("エラー"))
        assertTrue(next.statusMsg.contains("connection refused"))
    }

    // ── authError ──────────────────────────────────────────────────

    @Test
    fun `authError sets statusMsg verbatim`() {
        val next = TerminalReducer.authError(initial, "パスワードが必要です")
        assertEquals("パスワードが必要です", next.statusMsg)
    }

    // ── screenUpdated ──────────────────────────────────────────────

    @Test
    fun `screenUpdated sets update and scrollbackLen`() {
        val next = TerminalReducer.screenUpdated(initial, dummyScreenUpdate(), 50)
        assertNotNull(next.screenUpdate)
        assertEquals(50, next.scrollbackLen)
    }

    // ── trzsz state ────────────────────────────────────────────────

    @Test
    fun `trzszRequest sets WaitingUser state`() {
        val next = TerminalReducer.trzszRequest(initial, "tid1", "upload", "file.txt", 1024u)
        val s = next.trzszState as? TrzszUiState.WaitingUser
        assertNotNull(s)
        assertEquals("tid1", s!!.transferId)
        assertEquals("upload", s.mode)
        assertEquals("file.txt", s.suggestedName)
    }

    @Test
    fun `trzszFinished sets Done state`() {
        val state = initial.copy(
            trzszState = TrzszUiState.InProgress("tid1", "upload", "file.txt", 100u, 200u)
        )
        val next = TerminalReducer.trzszFinished(state, "tid1", true, null)
        val s = next.trzszState as? TrzszUiState.Done
        assertNotNull(s)
        assertTrue(s!!.success)
    }

    @Test
    fun `trzszProgress updates transferred amount when InProgress`() {
        val state = initial.copy(
            trzszState = TrzszUiState.InProgress("tid1", "upload", "file.txt", 0u, 1000u)
        )
        val next = TerminalReducer.trzszProgress(state, 500u, 1000u)
        val s = next.trzszState as? TrzszUiState.InProgress
        assertEquals(500uL, s!!.transferred)
    }

    @Test
    fun `trzszProgress transitions WaitingUser to InProgress`() {
        val state = initial.copy(
            trzszState = TrzszUiState.WaitingUser("tid1", "download", "file.txt", 1000u)
        )
        val next = TerminalReducer.trzszProgress(state, 500u, 1000u)
        val s = next.trzszState as? TrzszUiState.InProgress
        assertNotNull(s)
        assertEquals(500uL, s!!.transferred)
    }

    @Test
    fun `trzszProgress is no-op when trzszState is null`() {
        val next = TerminalReducer.trzszProgress(initial, 100u, null)
        assertNull(next.trzszState)
    }

    // ── host key ───────────────────────────────────────────────────

    @Test
    fun `hostKeyTrusted stores fingerprint`() {
        val next = TerminalReducer.hostKeyTrusted(initial, "sha256:abc")
        assertEquals("sha256:abc", next.lastFingerprint)
    }

    @Test
    fun `hostKeyChanged sets warning`() {
        val w = HostKeyChangedWarning("host.example.com", 22, "old-fp", "new-fp")
        val next = TerminalReducer.hostKeyChanged(initial, w, "new-fp")
        assertNotNull(next.hostKeyChangedWarning)
        assertEquals("new-fp", next.lastFingerprint)
    }

    // ── helpers ────────────────────────────────────────────────────

    private fun dummyScreenUpdate() = uniffi.tssh_core.ScreenUpdate(
        cols = 80u,
        rows = 24u,
        cells = emptyList(),
        cursorRow = 0u,
        cursorCol = 0u,
        title = null,
        applicationCursorMode = false,
        bracketedPasteMode = false,
    )
}
