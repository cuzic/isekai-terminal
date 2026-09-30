package tools.isekai.terminal.filepreview

import android.graphics.Bitmap
import android.graphics.BitmapFactory
import androidx.compose.foundation.Image
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.runtime.getValue
import androidx.compose.runtime.produceState
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.asImageBitmap
import androidx.compose.ui.layout.ContentScale
import androidx.compose.ui.unit.dp
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import tools.isekai.terminal.ui.AppColors
import tools.isekai.terminal.util.BitmapSampling

/**
 * タスク#17: 画像ビューア。`isekai-pipe ctl file cat`のチャンク読み取りで組み立てた
 * バイト列([bytes]、呼び出し元[FilePreviewSheet]がpngとして完結するまで蓄積済み)を
 * `BitmapFactory`でデコードして表示する。デコード失敗(壊れたデータ・未対応形式)は
 * エラーメッセージ表示に落とす(クラッシュさせない)。
 */
@Composable
fun ImageViewer(bytes: ByteArray, modifier: Modifier = Modifier) {
    // AND-L9: 以前はサンプリング無しのフル解像度デコードをコンポジション中(メインスレッド)に
    // 行っておりjankの原因になっていた。表示には十分な[MAX_DECODE_PIXELS]以下へ縮小し、
    // バックグラウンドでデコードする。
    val result by produceState<DecodeResult>(DecodeResult.Loading, bytes) {
        value = withContext(Dispatchers.Default) { decodeSampled(bytes) }
    }
    val bitmap = (result as? DecodeResult.Done)?.bitmap
    Box(modifier = modifier.fillMaxSize(), contentAlignment = Alignment.Center) {
        if (result == DecodeResult.Loading) {
            CircularProgressIndicator()
        } else if (bitmap != null) {
            Image(
                bitmap = bitmap.asImageBitmap(),
                contentDescription = null,
                contentScale = ContentScale.Fit,
                modifier = Modifier.fillMaxSize().padding(8.dp),
            )
        } else {
            Text("画像を表示できません(未対応の形式か破損しています)", color = AppColors.Error, style = MaterialTheme.typography.bodyMedium)
        }
    }
}

private sealed class DecodeResult {
    object Loading : DecodeResult()
    class Done(val bitmap: Bitmap?) : DecodeResult()
}

/** 表示用デコードの画素数上限(ARGB_8888で約32MB)。 */
private const val MAX_DECODE_PIXELS = 8_000_000L

private fun decodeSampled(bytes: ByteArray): DecodeResult = DecodeResult.Done(
    runCatching {
        val bounds = BitmapFactory.Options().apply { inJustDecodeBounds = true }
        BitmapFactory.decodeByteArray(bytes, 0, bytes.size, bounds)
        val opts = BitmapFactory.Options().apply {
            inSampleSize = BitmapSampling.inSampleSizeFor(bounds.outWidth, bounds.outHeight, MAX_DECODE_PIXELS)
        }
        BitmapFactory.decodeByteArray(bytes, 0, bytes.size, opts)
    }.getOrNull(),
)
