package tools.isekai.terminal.ui

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp

/**
 * SSH agent forwarding: 転送された鍵での署名要求を、要求ごとにユーザーへ確認するダイアログ。
 * 拒否（キャンセル含む）で応答すると Rust 側は SIGN_REQUEST に失敗応答を返す。
 */
@Composable
fun AgentSignConfirmDialog(
    fingerprint: String,
    onApprove: () -> Unit,
    onReject: () -> Unit,
) {
    AlertDialog(
        onDismissRequest = onReject,
        title = { Text("署名要求の確認") },
        text = {
            Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
                Text("接続先サーバーが、転送されたあなたの鍵での署名を要求しています。")
                Text("Fingerprint (SHA256):")
                Text(
                    fingerprint,
                    fontFamily = FontFamily.Monospace,
                    fontSize = 11.sp,
                    color = MaterialTheme.colorScheme.primary,
                )
                Text("信頼できる操作であることを確認したうえで許可してください。", fontSize = 13.sp)
            }
        },
        confirmButton = {
            TextButton(onClick = onApprove) { Text("許可") }
        },
        dismissButton = {
            TextButton(onClick = onReject) { Text("拒否") }
        },
    )
}
