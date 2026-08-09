package tools.isekai.terminal.ui

import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.width
import androidx.compose.material3.Card
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp

/**
 * 一覧画面(プロファイル/定型コマンド/打鍵列)で共通の「タイトル+サブタイトル+キャプション+
 * 編集/削除ボタン」を持つカード。[ProfileListScreen]/[SnippetListScreen]/[KeySequenceListScreen]
 * の元々ほぼ同一だった `*Card` を統合したもの。
 */
@Composable
fun ListItemCard(
    title: String,
    subtitle: String,
    caption: String,
    onTap: () -> Unit,
    onEdit: () -> Unit,
    onDelete: () -> Unit,
    modifier: Modifier = Modifier,
    captionColor: Color = MaterialTheme.colorScheme.primary,
    editTestTag: String? = null,
    deleteTestTag: String? = null,
) {
    Card(
        modifier = modifier
            .fillMaxWidth()
            .clickable(onClick = onTap),
    ) {
        Row(
            modifier = Modifier
                .fillMaxWidth()
                .padding(12.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Column(modifier = Modifier.weight(1f)) {
                Text(
                    text = title,
                    fontWeight = FontWeight.Bold,
                    fontSize = 16.sp,
                )
                Spacer(Modifier.width(2.dp))
                Text(
                    text = subtitle,
                    fontFamily = FontFamily.Monospace,
                    fontSize = 13.sp,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                    maxLines = 1,
                )
                Text(
                    text = caption,
                    fontSize = 12.sp,
                    color = captionColor,
                )
            }
            TextButton(
                onClick = onEdit,
                modifier = editTestTag?.let { Modifier.testTag(it) } ?: Modifier,
            ) { Text("編集") }
            TextButton(
                onClick = onDelete,
                modifier = deleteTestTag?.let { Modifier.testTag(it) } ?: Modifier,
            ) { Text("削除") }
        }
    }
}
