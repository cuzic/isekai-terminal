package tools.isekai.terminal.filepreview

import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

class CsvParserTest {

    @Test
    fun `parses simple rows`() {
        val rows = CsvParser.parse("a,b,c\n1,2,3")
        assertEquals(listOf(listOf("a", "b", "c"), listOf("1", "2", "3")), rows)
    }

    @Test
    fun `handles quoted fields containing commas`() {
        val rows = CsvParser.parse("name,note\n\"Doe, John\",hello")
        assertEquals(listOf(listOf("name", "note"), listOf("Doe, John", "hello")), rows)
    }

    @Test
    fun `handles escaped double quotes inside a quoted field`() {
        val text = "a,b\n\"say \"\"hi\"\"\",2"
        val parsed = CsvParser.parse(text)
        assertEquals(listOf(listOf("a", "b"), listOf("say \"hi\"", "2")), parsed)
    }

    @Test
    fun `ignores carriage returns before newline`() {
        val rows = CsvParser.parse("a,b\r\n1,2\r\n")
        assertEquals(listOf(listOf("a", "b"), listOf("1", "2")), rows)
    }

    @Test
    fun `trailing newline does not create a phantom empty row`() {
        val rows = CsvParser.parse("a,b\n1,2\n")
        assertEquals(2, rows.size)
    }

    @Test
    fun `no trailing newline still captures the last row`() {
        val rows = CsvParser.parse("a,b\n1,2")
        assertEquals(listOf(listOf("a", "b"), listOf("1", "2")), rows)
    }

    @Test
    fun `empty input produces no rows`() {
        assertTrue(CsvParser.parse("").isEmpty())
    }

    @Test
    fun `delimiterFor returns tab for tsv and comma otherwise`() {
        assertEquals('\t', CsvParser.delimiterFor("data.tsv"))
        assertEquals(',', CsvParser.delimiterFor("data.csv"))
        assertEquals(',', CsvParser.delimiterFor("data.txt"))
    }

    @Test
    fun `parses tsv with tab delimiter`() {
        val rows = CsvParser.parse("a\tb\n1\t2", delimiter = '\t')
        assertEquals(listOf(listOf("a", "b"), listOf("1", "2")), rows)
    }
}
