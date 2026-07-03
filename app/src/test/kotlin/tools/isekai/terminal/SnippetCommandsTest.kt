package tools.isekai.terminal

import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Test
import tools.isekai.terminal.data.Snippet

class SnippetCommandsTest {

    @Test
    fun toBytes_emptyCommand_returnsEmptyBytes() {
        assertArrayEquals(ByteArray(0), SnippetCommands.toBytes(""))
    }

    @Test
    fun toBytes_singleLine_appendNewlineTrue_appendsCr() {
        val bytes = SnippetCommands.toBytes("ls -la", appendNewline = true)
        assertEquals("ls -la\r", bytes.toString(Charsets.UTF_8))
    }

    @Test
    fun toBytes_singleLine_appendNewlineFalse_doesNotAppendCr() {
        val bytes = SnippetCommands.toBytes("ls -la", appendNewline = false)
        assertEquals("ls -la", bytes.toString(Charsets.UTF_8))
    }

    @Test
    fun toBytes_multiLine_normalizesInteriorNewlinesToCr() {
        val bytes = SnippetCommands.toBytes("cd /var/log\ntail -f syslog", appendNewline = false)
        assertEquals("cd /var/log\rtail -f syslog", bytes.toString(Charsets.UTF_8))
    }

    @Test
    fun toBytes_multiLine_appendNewlineTrue_appendsTrailingCrOnLastLineToo() {
        val bytes = SnippetCommands.toBytes("echo one\necho two", appendNewline = true)
        assertEquals("echo one\recho two\r", bytes.toString(Charsets.UTF_8))
    }

    @Test
    fun toBytes_crlfLineEndings_normalizedToCr() {
        val bytes = SnippetCommands.toBytes("a\r\nb", appendNewline = false)
        assertEquals("a\rb", bytes.toString(Charsets.UTF_8))
    }

    @Test
    fun toBytes_alreadyEndingInNewline_doesNotDoubleUpTrailingCr() {
        val bytes = SnippetCommands.toBytes("ls\n", appendNewline = true)
        assertEquals("ls\r", bytes.toString(Charsets.UTF_8))
    }

    @Test
    fun toBytes_isUtf8Encoded() {
        val bytes = SnippetCommands.toBytes("echo こんにちは", appendNewline = false)
        assertEquals("echo こんにちは", bytes.toString(Charsets.UTF_8))
    }

    @Test
    fun toBytes_fromSnippet_usesSnippetAppendNewlineFlag() {
        val snippet = Snippet(label = "l", command = "ls", appendNewline = false)
        assertEquals("ls", SnippetCommands.toBytes(snippet).toString(Charsets.UTF_8))

        val snippet2 = Snippet(label = "l", command = "ls", appendNewline = true)
        assertEquals("ls\r", SnippetCommands.toBytes(snippet2).toString(Charsets.UTF_8))
    }
}
