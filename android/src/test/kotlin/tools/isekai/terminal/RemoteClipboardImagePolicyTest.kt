package tools.isekai.terminal

import android.content.ClipData
import android.content.ClipDescription
import android.net.Uri
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config

/**
 * [RemoteClipboardImagePolicy.isImageClip]/[RemoteClipboardImagePolicy.isValidPngPayload]
 * (Android依存の無い純粋な判定)を対象にする。`writeImageToClipData`/
 * `readImageFromClipData`(`FileProvider`のcontent:// URI発行・`BitmapFactory`の
 * デコード)は実機/エミュレータでの動作確認が別途必要(Robolectricでは代替しない)。
 */
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [28])
class RemoteClipboardImagePolicyTest {

    private fun clipDataWithMime(mime: String) =
        ClipData(ClipDescription("label", arrayOf(mime)), ClipData.Item(Uri.parse("content://example/1")))

    @Test
    fun `image png clip is recognized as an image`() {
        assertTrue(RemoteClipboardImagePolicy.isImageClip(clipDataWithMime("image/png")))
    }

    @Test
    fun `image jpeg clip is recognized as an image`() {
        assertTrue(RemoteClipboardImagePolicy.isImageClip(clipDataWithMime("image/jpeg")))
    }

    @Test
    fun `text plain clip is not an image`() {
        assertFalse(RemoteClipboardImagePolicy.isImageClip(clipDataWithMime("text/plain")))
    }

    @Test
    fun `null clip data is not an image`() {
        assertFalse(RemoteClipboardImagePolicy.isImageClip(null))
    }

    private val pngSignature =
        byteArrayOf(0x89.toByte(), 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A)

    @Test
    fun `data starting with the PNG signature within the size limit is valid`() {
        val data = pngSignature + ByteArray(100)
        assertTrue(RemoteClipboardImagePolicy.isValidPngPayload(data))
    }

    @Test
    fun `data without the PNG signature is rejected`() {
        val data = ByteArray(pngSignature.size + 100) { 0 }
        assertFalse(RemoteClipboardImagePolicy.isValidPngPayload(data))
    }

    @Test
    fun `data shorter than the PNG signature is rejected`() {
        assertFalse(RemoteClipboardImagePolicy.isValidPngPayload(pngSignature.copyOf(4)))
    }

    @Test
    fun `data larger than MAX_IMAGE_BYTES is rejected even with a valid signature`() {
        val data = pngSignature + ByteArray(RemoteClipboardImagePolicy.MAX_IMAGE_BYTES)
        assertFalse(RemoteClipboardImagePolicy.isValidPngPayload(data))
    }

    @Test
    fun `empty data is rejected`() {
        assertFalse(RemoteClipboardImagePolicy.isValidPngPayload(ByteArray(0)))
    }
}
