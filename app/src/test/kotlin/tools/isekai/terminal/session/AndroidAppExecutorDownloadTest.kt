package tools.isekai.terminal.session

import android.app.Application
import android.os.Environment
import androidx.test.core.app.ApplicationProvider
import kotlinx.coroutines.runBlocking
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config

/**
 * ダウンロード保存時のファイル名サニタイズ（タスク #63）の回帰テスト。
 *
 * サーバー由来のファイル名は現状常にリテラル "download" 固定
 * (`TerminalSession.kt` 参照) のため実際には到達しないが、将来
 * `suggested_name`/`file_name` を配線した場合に備えて
 * [AndroidAppExecutor.sanitizeDownloadFileName] とその適用箇所を検証する。
 */
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [33])
class AndroidAppExecutorDownloadTest {

    @Test
    fun pathTraversalIsReducedToBasename() {
        assertEquals("x", AndroidAppExecutor.sanitizeDownloadFileName("../../etc/x"))
        assertEquals(
            "passwd",
            AndroidAppExecutor.sanitizeDownloadFileName("../../../../etc/passwd"),
        )
    }

    @Test
    fun absolutePathIsReducedToBasename() {
        assertEquals("path", AndroidAppExecutor.sanitizeDownloadFileName("/abs/path"))
        assertEquals(
            "shadow",
            AndroidAppExecutor.sanitizeDownloadFileName("/etc/shadow"),
        )
    }

    @Test
    fun windowsStyleSeparatorsAreAlsoReducedToBasename() {
        assertEquals("bar", AndroidAppExecutor.sanitizeDownloadFileName("C:\\foo\\bar"))
        assertEquals("bar", AndroidAppExecutor.sanitizeDownloadFileName("..\\..\\bar"))
    }

    @Test
    fun blankNameFallsBackToDownload() {
        assertEquals("download", AndroidAppExecutor.sanitizeDownloadFileName(""))
        assertEquals("download", AndroidAppExecutor.sanitizeDownloadFileName("   "))
    }

    @Test
    fun bareDotAndDotDotFallBackToDownload() {
        assertEquals("download", AndroidAppExecutor.sanitizeDownloadFileName("."))
        assertEquals("download", AndroidAppExecutor.sanitizeDownloadFileName(".."))
    }

    @Test
    fun overlongNameIsTruncatedTo255Chars() {
        val longName = "a".repeat(1000)
        val sanitized = AndroidAppExecutor.sanitizeDownloadFileName(longName)
        assertEquals(255, sanitized.length)
    }

    @Test
    fun normalNameIsUnchanged() {
        assertEquals("report.txt", AndroidAppExecutor.sanitizeDownloadFileName("report.txt"))
    }

    // ── pre-Android-10（minSdk=28）の File 直書き経路の実体験テスト ──────────

    @Config(sdk = [28])
    @Test
    fun saveDownloadFileWithTraversalNameStaysInsideDownloadsDir_preQ() = runBlocking {
        val app = ApplicationProvider.getApplicationContext<Application>()
        val executor = AndroidAppExecutor(app)
        val downloadsDir = Environment.getExternalStoragePublicDirectory(
            Environment.DIRECTORY_DOWNLOADS
        )

        executor.saveDownloadFile("../../../../etc/passwd", byteArrayOf(1, 2, 3))

        // ダウンロードディレクトリの外(祖先ディレクトリ)には何も書き込まれていない。
        val escapedFile = java.io.File(downloadsDir.parentFile?.parentFile, "etc/passwd")
        assertFalse(escapedFile.exists())

        // サニタイズ後の basename でダウンロードディレクトリ内に書き込まれている。
        val expectedFile = java.io.File(downloadsDir, "passwd")
        assertTrue(expectedFile.exists())
        assertEquals(3, expectedFile.readBytes().size)
    }

    @Config(sdk = [28])
    @Test
    fun saveDownloadFileWithAbsolutePathNameStaysInsideDownloadsDir_preQ() = runBlocking {
        val app = ApplicationProvider.getApplicationContext<Application>()
        val executor = AndroidAppExecutor(app)
        val downloadsDir = Environment.getExternalStoragePublicDirectory(
            Environment.DIRECTORY_DOWNLOADS
        )

        executor.saveDownloadFile("/abs/path", byteArrayOf(9))

        assertFalse(java.io.File("/abs/path").exists())
        val expectedFile = java.io.File(downloadsDir, "path")
        assertTrue(expectedFile.exists())
    }

    @Config(sdk = [28])
    @Test
    fun saveDownloadFileWithBlankNameFallsBackToDownload_preQ() = runBlocking {
        val app = ApplicationProvider.getApplicationContext<Application>()
        val executor = AndroidAppExecutor(app)
        val downloadsDir = Environment.getExternalStoragePublicDirectory(
            Environment.DIRECTORY_DOWNLOADS
        )

        executor.saveDownloadFile("", byteArrayOf(5))

        val expectedFile = java.io.File(downloadsDir, "download")
        assertTrue(expectedFile.exists())
    }

    @Config(sdk = [28])
    @Test
    fun saveDownloadFileWithOverlongNameIsTruncated_preQ() = runBlocking {
        val app = ApplicationProvider.getApplicationContext<Application>()
        val executor = AndroidAppExecutor(app)
        val downloadsDir = Environment.getExternalStoragePublicDirectory(
            Environment.DIRECTORY_DOWNLOADS
        )
        val longName = "b".repeat(1000)

        executor.saveDownloadFile(longName, byteArrayOf(7))

        val filesInDir = downloadsDir.listFiles().orEmpty()
        val written = filesInDir.singleOrNull { it.name.all { c -> c == 'b' } }
        assertTrue(written != null)
        assertEquals(255, written!!.name.length)
    }
}
