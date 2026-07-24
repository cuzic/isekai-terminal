package tools.isekai.terminal.filepreview

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config

@RunWith(RobolectricTestRunner::class)
@Config(sdk = [33])
class SyntaxHighlighterTest {

    @Test
    fun `languageFor resolves known extensions`() {
        assertEquals("kt", SyntaxHighlighter.languageFor("Main.kt"))
        assertEquals("rs", SyntaxHighlighter.languageFor("lib.rs"))
        assertEquals("py", SyntaxHighlighter.languageFor("script.py"))
        assertEquals("json", SyntaxHighlighter.languageFor("data.json"))
    }

    @Test
    fun `languageFor is case insensitive`() {
        assertEquals("kt", SyntaxHighlighter.languageFor("Main.KT"))
    }

    @Test
    fun `languageFor returns null for unknown extensions`() {
        assertNull(SyntaxHighlighter.languageFor("file.unknown"))
        assertNull(SyntaxHighlighter.languageFor("noext"))
    }

    @Test
    fun `highlight preserves the original text content`() {
        val source = "fun main() { val x = 1 }"
        val result = SyntaxHighlighter.highlight(source, "kt")
        assertEquals(source, result.text)
    }

    @Test
    fun `highlight marks a keyword with the keyword color`() {
        val source = "fun main() {}"
        val result = SyntaxHighlighter.highlight(source, "kt")
        // "fun" が0..3の範囲でSpanStyleが付与されているはず
        val hasKeywordSpan = result.spanStyles.any { it.start == 0 && it.end == 3 }
        assertTrue("expected a span covering 'fun'", hasKeywordSpan)
    }

    @Test
    fun `highlight with no language still returns the plain text`() {
        val source = "some plain text 123"
        val result = SyntaxHighlighter.highlight(source, null)
        assertEquals(source, result.text)
    }

    @Test
    fun `highlight handles a hash comment for python`() {
        val source = "# a comment\nx = 1"
        val result = SyntaxHighlighter.highlight(source, "py")
        assertEquals(source, result.text)
    }

    @Test
    fun `highlight handles an unterminated string without infinite looping`() {
        val source = "let s = \"unterminated"
        val result = SyntaxHighlighter.highlight(source, "rs")
        assertEquals(source, result.text)
    }
}
