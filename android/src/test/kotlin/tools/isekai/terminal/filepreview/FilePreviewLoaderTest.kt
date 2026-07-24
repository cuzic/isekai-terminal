package tools.isekai.terminal.filepreview

import kotlinx.coroutines.runBlocking
import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Assert.fail
import org.junit.Test
import uniffi.isekai_terminal_core.FilePreviewOutcome
import uniffi.isekai_terminal_core.FilePreviewRequestKind

class FilePreviewLoaderTest {

    /** テスト用の固定チャンクサイズを持つ`onRequest`フェイク。オフセットに応じて
     *  [fullContent]から切り出して返す(実際の`isekai-pipe ctl file cat`の
     *  offset/lengthページング契約を模す)。 */
    private fun fakeCatRequester(fullContent: ByteArray, chunkLen: Int = Int.MAX_VALUE): suspend (FilePreviewRequestKind) -> FilePreviewOutcome =
        { kind ->
            val cat = kind as FilePreviewRequestKind.Cat
            val offset = cat.offset.toInt()
            val requestedLength = (cat.length?.toInt() ?: (fullContent.size - offset))
            val length = minOf(requestedLength, chunkLen, fullContent.size - offset).coerceAtLeast(0)
            val data = fullContent.copyOfRange(offset, offset + length)
            FilePreviewOutcome.Cat(
                offset = offset.toULong(),
                length = length.toULong(),
                totalSize = fullContent.size.toULong(),
                eof = offset + length >= fullContent.size,
                data = data,
            )
        }

    @Test
    fun `loads a small file in a single request`() = runBlocking {
        val content = "hello world".toByteArray()
        val (bytes, totalSize, truncated) = FilePreviewLoader.loadBytes(
            "/tmp/f.txt", maxBytes = 1024, requestFilePreview = fakeCatRequester(content),
        )
        assertArrayEquals(content, bytes)
        assertEquals(11L, totalSize)
        assertFalse(truncated)
    }

    @Test
    fun `pages through multiple chunks until eof`() = runBlocking {
        val content = ByteArray(5000) { (it % 256).toByte() }
        val (bytes, totalSize, truncated) = FilePreviewLoader.loadBytes(
            "/tmp/f.bin", maxBytes = 1024L * 1024, requestFilePreview = fakeCatRequester(content, chunkLen = 700),
        )
        assertArrayEquals(content, bytes)
        assertEquals(5000L, totalSize)
        assertFalse(truncated)
    }

    @Test
    fun `stops and marks truncated once maxBytes is reached`() = runBlocking {
        val content = ByteArray(10_000) { 1 }
        val (bytes, _, truncated) = FilePreviewLoader.loadBytes(
            "/tmp/f.bin", maxBytes = 1000, requestFilePreview = fakeCatRequester(content, chunkLen = 300),
        )
        assertTrue(truncated)
        assertTrue("loaded at most maxBytes", bytes.size <= 1000)
    }

    @Test
    fun `propagates the server error message`(): Unit = runBlocking {
        val requester: suspend (FilePreviewRequestKind) -> FilePreviewOutcome =
            { FilePreviewOutcome.Error("No such file or directory") }
        try {
            FilePreviewLoader.loadBytes("/no/such/file", 1024, requester)
            fail("expected FilePreviewLoadError")
        } catch (e: FilePreviewLoadError) {
            assertEquals("No such file or directory", e.message)
        }
    }

    @Test
    fun `unexpected outcome type is reported as an error`(): Unit = runBlocking {
        val requester: suspend (FilePreviewRequestKind) -> FilePreviewOutcome =
            { FilePreviewOutcome.Ls(emptyList()) }
        try {
            FilePreviewLoader.loadBytes("/tmp/f.txt", 1024, requester)
            fail("expected FilePreviewLoadError")
        } catch (e: FilePreviewLoadError) {
            // メッセージの正確な文言までは検証しない(型不一致という事実だけを確認する)。
        }
    }

    @Test
    fun `empty file returns immediately with eof`() = runBlocking {
        val (bytes, totalSize, truncated) = FilePreviewLoader.loadBytes(
            "/tmp/empty.txt", maxBytes = 1024, requestFilePreview = fakeCatRequester(ByteArray(0)),
        )
        assertEquals(0, bytes.size)
        assertEquals(0L, totalSize)
        assertFalse(truncated)
    }
}
