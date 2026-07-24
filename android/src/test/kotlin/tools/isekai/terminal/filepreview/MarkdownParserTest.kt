package tools.isekai.terminal.filepreview

import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

class MarkdownParserTest {

    @Test
    fun `parses a heading`() {
        val blocks = MarkdownParser.parse("# Title")
        assertEquals(listOf(MarkdownBlock.Heading(1, "Title")), blocks)
    }

    @Test
    fun `heading level matches the number of hashes`() {
        val blocks = MarkdownParser.parse("### Sub Title")
        assertEquals(listOf(MarkdownBlock.Heading(3, "Sub Title")), blocks)
    }

    @Test
    fun `consecutive non-blank lines merge into one paragraph`() {
        val blocks = MarkdownParser.parse("line one\nline two")
        assertEquals(listOf(MarkdownBlock.Paragraph("line one line two")), blocks)
    }

    @Test
    fun `blank line separates paragraphs`() {
        val blocks = MarkdownParser.parse("first\n\nsecond")
        assertEquals(listOf(MarkdownBlock.Paragraph("first"), MarkdownBlock.Paragraph("second")), blocks)
    }

    @Test
    fun `parses a fenced code block with language`() {
        val blocks = MarkdownParser.parse("```kotlin\nval x = 1\nprintln(x)\n```")
        assertEquals(listOf(MarkdownBlock.CodeBlock("kotlin", "val x = 1\nprintln(x)")), blocks)
    }

    @Test
    fun `parses a fenced code block without language`() {
        val blocks = MarkdownParser.parse("```\nplain\n```")
        assertEquals(listOf(MarkdownBlock.CodeBlock(null, "plain")), blocks)
    }

    @Test
    fun `unterminated code fence still captures everything to EOF`() {
        val blocks = MarkdownParser.parse("```\nabc")
        assertEquals(listOf(MarkdownBlock.CodeBlock(null, "abc")), blocks)
    }

    @Test
    fun `parses unordered list items with different markers`() {
        val blocks = MarkdownParser.parse("- one\n* two\n+ three")
        assertEquals(
            listOf(
                MarkdownBlock.ListItem(false, "one"),
                MarkdownBlock.ListItem(false, "two"),
                MarkdownBlock.ListItem(false, "three"),
            ),
            blocks,
        )
    }

    @Test
    fun `parses ordered list items`() {
        val blocks = MarkdownParser.parse("1. first\n2. second")
        assertEquals(
            listOf(MarkdownBlock.ListItem(true, "first"), MarkdownBlock.ListItem(true, "second")),
            blocks,
        )
    }

    @Test
    fun `parses a blockquote`() {
        val blocks = MarkdownParser.parse("> quoted text")
        assertEquals(listOf(MarkdownBlock.BlockQuote("quoted text")), blocks)
    }

    @Test
    fun `parses a horizontal rule`() {
        assertEquals(listOf(MarkdownBlock.HorizontalRule), MarkdownParser.parse("---"))
        assertEquals(listOf(MarkdownBlock.HorizontalRule), MarkdownParser.parse("***"))
    }

    @Test
    fun `mixed document parses in order`() {
        val source = "# Title\n\nSome text.\n\n- item one\n- item two\n\n```rs\nfn main() {}\n```"
        val blocks = MarkdownParser.parse(source)
        assertEquals(
            listOf(
                MarkdownBlock.Heading(1, "Title"),
                MarkdownBlock.Paragraph("Some text."),
                MarkdownBlock.ListItem(false, "item one"),
                MarkdownBlock.ListItem(false, "item two"),
                MarkdownBlock.CodeBlock("rs", "fn main() {}"),
            ),
            blocks,
        )
    }

    @Test
    fun `empty source produces no blocks`() {
        assertTrue(MarkdownParser.parse("").isEmpty())
    }
}
