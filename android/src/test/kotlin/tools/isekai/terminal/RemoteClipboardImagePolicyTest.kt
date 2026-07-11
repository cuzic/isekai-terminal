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
 * [RemoteClipboardImagePolicy.isImageClip]のみを対象にする。`writeImageToClipData`/
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
}
