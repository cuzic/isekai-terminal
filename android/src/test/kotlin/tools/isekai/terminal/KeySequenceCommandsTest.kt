package tools.isekai.terminal

import org.junit.Assert.*
import org.junit.Test
import tools.isekai.terminal.input.KeyStep
import tools.isekai.terminal.input.TerminalKeyEncoder

class KeySequenceCommandsTest {

    // ── 各ステップ種別が TerminalKeyEncoder へ委譲されること ─────────

    @Test
    fun `CtrlChar delegates to TerminalKeyEncoder ctrlByte`() {
        val bytes = KeySequenceCommands.toBytes(listOf(KeyStep.CtrlChar('b')))
        assertArrayEquals(TerminalKeyEncoder.ctrlByte('b'.code), bytes)
        assertArrayEquals(byteArrayOf(0x02), bytes) // Ctrl+B
    }

    @Test
    fun `Special delegates to TerminalKeyEncoder specialKeyBytes`() {
        val bytes = KeySequenceCommands.toBytes(listOf(KeyStep.Special(TerminalKeyEncoder.KC_ENTER)))
        assertArrayEquals(byteArrayOf(0x0D), bytes)
    }

    @Test
    fun `Text delegates to TerminalKeyEncoder commitTextBytes without forcing trailing CR`() {
        // KeySequenceCommands は SnippetCommands と違い、単発キー入力に余計な CR を付与しない。
        val bytes = KeySequenceCommands.toBytes(listOf(KeyStep.Text("c")))
        assertArrayEquals("c".toByteArray(Charsets.UTF_8), bytes)
    }

    @Test
    fun `PlaceholderRef produces no bytes`() {
        val bytes = KeySequenceCommands.toBytes(listOf(KeyStep.PlaceholderRef("prefix")))
        assertEquals(0, bytes.size)
    }

    // ── 委譲元が null を返す場合はスキップされる ─────────────────

    @Test
    fun `invalid CtrlChar produces no bytes and does not throw`() {
        val bytes = KeySequenceCommands.toBytes(listOf(KeyStep.CtrlChar('1')))
        assertEquals(0, bytes.size)
    }

    @Test
    fun `unknown Special keyCode produces no bytes and does not throw`() {
        val bytes = KeySequenceCommands.toBytes(listOf(KeyStep.Special(9999)))
        assertEquals(0, bytes.size)
    }

    // ── 組み立て(複数ステップの連結) ──────────────────────────

    @Test
    fun `empty steps produces empty bytes`() {
        assertEquals(0, KeySequenceCommands.toBytes(emptyList()).size)
    }

    @Test
    fun `tmux new-window sequence concatenates prefix chord and literal c`() {
        // {prefix}=Ctrl+B, 'c' の想定(実際のパック解決は Task #18 側の責務)。
        val bytes = KeySequenceCommands.toBytes(
            listOf(KeyStep.CtrlChar('b'), KeyStep.Text("c")),
        )
        assertArrayEquals(byteArrayOf(0x02, 'c'.code.toByte()), bytes)
    }

    @Test
    fun `large text step is passed through unmodified aside from newline normalization`() {
        val big = "x".repeat(10_000)
        val bytes = KeySequenceCommands.toBytes(listOf(KeyStep.Text(big)))
        assertArrayEquals(big.toByteArray(Charsets.UTF_8), bytes)
    }

    // ── applicationCursorMode の伝播 ──────────────────────────

    @Test
    fun `arrow key without applicationCursorMode uses CSI form`() {
        val bytes = KeySequenceCommands.toBytes(
            listOf(KeyStep.Special(TerminalKeyEncoder.KC_DPAD_UP)),
            applicationCursorMode = false,
        )
        assertArrayEquals(byteArrayOf(0x1B, 0x5B, 0x41), bytes)
    }

    @Test
    fun `arrow key with applicationCursorMode uses SS3 form`() {
        val bytes = KeySequenceCommands.toBytes(
            listOf(KeyStep.Special(TerminalKeyEncoder.KC_DPAD_UP)),
            applicationCursorMode = true,
        )
        assertArrayEquals(byteArrayOf(0x1B, 0x4F, 0x41), bytes)
    }
}
