package tools.isekai.terminal.filepreview

import androidx.compose.ui.text.font.FontStyle
import androidx.compose.ui.text.font.FontWeight
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config

@RunWith(RobolectricTestRunner::class)
@Config(sdk = [33])
class RenderInlineMarkdownTest {

    @Test
    fun `plain text passes through unchanged`() {
        val result = renderInlineMarkdown("just plain text")
        assertEquals("just plain text", result.text)
        assertTrue(result.spanStyles.isEmpty())
    }

    @Test
    fun `bold text is wrapped in a bold span`() {
        val result = renderInlineMarkdown("this is **bold** text")
        assertEquals("this is bold text", result.text)
        val boldSpan = result.spanStyles.first { it.item.fontWeight == FontWeight.Bold }
        assertEquals("bold", result.text.substring(boldSpan.start, boldSpan.end))
    }

    @Test
    fun `italic with asterisks is wrapped in an italic span`() {
        val result = renderInlineMarkdown("this is *italic* text")
        assertEquals("this is italic text", result.text)
        val span = result.spanStyles.first { it.item.fontStyle == FontStyle.Italic }
        assertEquals("italic", result.text.substring(span.start, span.end))
    }

    @Test
    fun `italic with underscores is also recognized`() {
        val result = renderInlineMarkdown("this is _italic_ text")
        val span = result.spanStyles.first { it.item.fontStyle == FontStyle.Italic }
        assertEquals("italic", result.text.substring(span.start, span.end))
    }

    @Test
    fun `inline code is wrapped in a monospace span`() {
        val result = renderInlineMarkdown("run `cargo test` now")
        assertEquals("run cargo test now", result.text)
        assertTrue(result.spanStyles.any { it.item.fontFamily != null })
    }

    @Test
    fun `unterminated bold marker is left as literal text`() {
        val result = renderInlineMarkdown("**never closed")
        assertEquals("**never closed", result.text)
    }

    @Test
    fun `lone asterisk is preserved literally`() {
        val result = renderInlineMarkdown("2 * 3 = 6")
        assertEquals("2 * 3 = 6", result.text)
    }
}
