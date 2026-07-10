package tools.isekai.terminal

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test

class RemoteClipboardPolicyTest {

    // ── write ─────────────────────────────────────────────────────

    @Test
    fun `write is forwarded to the clipboard when opt-in is enabled`() {
        var written: String? = null
        val policy = RemoteClipboardPolicy(
            isWriteAllowed = { true },
            isPullAllowed = { false },
            writeToClipboard = { text -> written = text },
            readFromClipboard = { null },
        )

        policy.onClipboardWriteRequested("hello from remote")

        assertEquals("hello from remote", written)
    }

    @Test
    fun `write is silently dropped when opt-in is disabled (default)`() {
        var writeCalled = false
        val policy = RemoteClipboardPolicy(
            isWriteAllowed = { false },
            isPullAllowed = { false },
            writeToClipboard = { writeCalled = true },
            readFromClipboard = { null },
        )

        policy.onClipboardWriteRequested("should not reach the clipboard")

        assertEquals(false, writeCalled)
    }

    // ── pull ──────────────────────────────────────────────────────

    @Test
    fun `pull returns the clipboard contents when opt-in is enabled`() {
        val policy = RemoteClipboardPolicy(
            isWriteAllowed = { false },
            isPullAllowed = { true },
            writeToClipboard = {},
            readFromClipboard = { "clipboard contents" },
        )

        assertEquals("clipboard contents", policy.onClipboardPullRequested())
    }

    @Test
    fun `pull returns null without touching the clipboard when opt-in is disabled (default)`() {
        var readCalled = false
        val policy = RemoteClipboardPolicy(
            isWriteAllowed = { false },
            isPullAllowed = { false },
            writeToClipboard = {},
            readFromClipboard = { readCalled = true; "should not be returned" },
        )

        assertNull(policy.onClipboardPullRequested())
        assertEquals(false, readCalled)
    }
}
