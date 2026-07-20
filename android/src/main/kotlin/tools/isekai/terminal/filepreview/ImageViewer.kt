package tools.isekai.terminal.filepreview

import android.graphics.BitmapFactory
import androidx.compose.foundation.Image
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.remember
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.asImageBitmap
import androidx.compose.ui.layout.ContentScale
import androidx.compose.ui.unit.dp
import tools.isekai.terminal.ui.AppColors

/**
 * タスク#17: 画像ビューア。`isekai-pipe ctl file cat`のチャンク読み取りで組み立てた
 * バイト列([bytes]、呼び出し元[FilePreviewSheet]がpngとして完結するまで蓄積済み)を
 * `BitmapFactory`でデコードして表示する。デコード失敗(壊れたデータ・未対応形式)は
 * エラーメッセージ表示に落とす(クラッシュさせない)。
 */
@Composable
fun ImageViewer(bytes: ByteArray, modifier: Modifier = Modifier) {
    val bitmap = remember(bytes) {
        runCatching { BitmapFactory.decodeByteArray(bytes, 0, bytes.size) }.getOrNull()
    }
    Box(modifier = modifier.fillMaxSize(), contentAlignment = Alignment.Center) {
        if (bitmap != null) {
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
