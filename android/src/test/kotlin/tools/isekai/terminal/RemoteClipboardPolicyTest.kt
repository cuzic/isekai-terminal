package tools.isekai.terminal

import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Test
import uniffi.isekai_terminal_core.ClipboardMimeKind
import uniffi.isekai_terminal_core.ClipboardPayload

class RemoteClipboardPolicyTest {

    private fun textPayload(text: String) =
        ClipboardPayload(ClipboardMimeKind.TEXT_PLAIN, text.toByteArray(Charsets.UTF_8))

    // `ClipboardPayload`はByteArrayプロパティを持つdata classで、Kotlinの自動生成
    // equals()はByteArrayを参照比較する(内容比較にならない)ため、`assertEquals`を
    // 直接使わずmime/data(文字列化)を個別に比較する。
    private fun assertPayloadIsText(expected: String, actual: ClipboardPayload?) {
        assertEquals(ClipboardMimeKind.TEXT_PLAIN, actual?.mime)
        assertArrayEquals(expected.toByteArray(Charsets.UTF_8), actual?.data)
    }

    // ── write ─────────────────────────────────────────────────────

    @Test
    fun `write is forwarded to the clipboard when opt-in is enabled`() {
        var written: ClipboardPayload? = null
        val policy = RemoteClipboardPolicy(
            isWriteAllowed = { true },
            isPullAllowed = { false },
            writeToClipboard = { payload -> written = payload },
            readFromClipboard = { null },
        )

        policy.onClipboardWriteRequested(textPayload("hello from remote"))

        assertPayloadIsText("hello from remote", written)
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

        policy.onClipboardWriteRequested(textPayload("should not reach the clipboard"))

        assertEquals(false, writeCalled)
    }

    // ── pull ──────────────────────────────────────────────────────

    @Test
    fun `pull returns the clipboard contents when opt-in is enabled`() {
        val policy = RemoteClipboardPolicy(
            isWriteAllowed = { false },
            isPullAllowed = { true },
            writeToClipboard = {},
            readFromClipboard = { textPayload("clipboard contents") },
        )

        assertPayloadIsText("clipboard contents", policy.onClipboardPullRequested())
    }

    @Test
    fun `pull returns null without touching the clipboard when opt-in is disabled (default)`() {
        var readCalled = false
        val policy = RemoteClipboardPolicy(
            isWriteAllowed = { false },
            isPullAllowed = { false },
            writeToClipboard = {},
            readFromClipboard = { readCalled = true; textPayload("should not be returned") },
        )

        assertNull(policy.onClipboardPullRequested())
        assertEquals(false, readCalled)
    }
}
