package tools.isekai.terminal.filepreview

import org.junit.Assert.assertEquals
import org.junit.Test

class FilePreviewPathsTest {

    @Test
    fun `join adds a single slash between dir and name`() {
        assertEquals("/home/user/file.txt", FilePreviewPaths.join("/home/user", "file.txt"))
    }

    @Test
    fun `join does not double the slash when dir already ends with one`() {
        assertEquals("/file.txt", FilePreviewPaths.join("/", "file.txt"))
    }

    @Test
    fun `parent strips the last path segment`() {
        assertEquals("/home/user", FilePreviewPaths.parent("/home/user/sub"))
    }

    @Test
    fun `parent of a top-level directory is root`() {
        assertEquals("/", FilePreviewPaths.parent("/home"))
    }

    @Test
    fun `parent of root is root`() {
        assertEquals("/", FilePreviewPaths.parent("/"))
    }

    @Test
    fun `parent tolerates a trailing slash`() {
        assertEquals("/home", FilePreviewPaths.parent("/home/user/"))
    }
}
