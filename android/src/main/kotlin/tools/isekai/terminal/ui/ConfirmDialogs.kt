package tools.isekai.terminal.ui

import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.semantics.testTagsAsResourceId
import tools.isekai.terminal.BuildConfig

/** 削除確認ダイアログ。`ProfileListScreen`/`KeyListScreen` 等で共通利用する。 */
@Composable
fun DeleteConfirmDialog(
    title: String,
    message: String,
    onConfirm: () -> Unit,
    onDismiss: () -> Unit,
    confirmColor: Color = Color.Unspecified,
) {
    AlertDialog(
        // AlertDialog は別ウィンドウ(Dialog)で描画されるため、呼び出し元(MainActivity)の
        // ルートに設定した testTagsAsResourceId はここには伝播しない(実機で確認済み)。
        modifier = Modifier.semantics { testTagsAsResourceId = BuildConfig.DEBUG },
        onDismissRequest = onDismiss,
        title = { Text(title) },
        text = { Text(message) },
        confirmButton = {
            TextButton(onClick = onConfirm, modifier = Modifier.testTag("deleteConfirmButton")) {
                Text("削除", color = confirmColor)
            }
        },
        dismissButton = {
            TextButton(onClick = onDismiss, modifier = Modifier.testTag("deleteCancelButton")) { Text("キャンセル") }
        },
    )
}
