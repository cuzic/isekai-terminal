package tools.isekai.terminal.filepreview

import org.junit.Assert.assertEquals
import org.junit.Test

class FilePreviewKindDetectorTest {

    @Test
    fun `md extension is markdown`() {
        assertEquals(FilePreviewKind.MARKDOWN, FilePreviewKindDetector.detect("README.md"))
    }

    @Test
    fun `markdown extension is markdown`() {
        assertEquals(FilePreviewKind.MARKDOWN, FilePreviewKindDetector.detect("notes.markdown"))
    }

    @Test
    fun `png jpg gif are images`() {
        assertEquals(FilePreviewKind.IMAGE, FilePreviewKindDetector.detect("photo.png"))
        assertEquals(FilePreviewKind.IMAGE, FilePreviewKindDetector.detect("photo.JPG"))
        assertEquals(FilePreviewKind.IMAGE, FilePreviewKindDetector.detect("anim.gif"))
    }

    @Test
    fun `csv and tsv are csv`() {
        assertEquals(FilePreviewKind.CSV, FilePreviewKindDetector.detect("data.csv"))
        assertEquals(FilePreviewKind.CSV, FilePreviewKindDetector.detect("data.tsv"))
    }

    @Test
    fun `unknown extensions fall back to text`() {
        assertEquals(FilePreviewKind.TEXT, FilePreviewKindDetector.detect("main.rs"))
        assertEquals(FilePreviewKind.TEXT, FilePreviewKindDetector.detect("noext"))
    }

    @Test
    fun `extension matching is case insensitive`() {
        assertEquals(FilePreviewKind.MARKDOWN, FilePreviewKindDetector.detect("README.MD"))
    }
}
