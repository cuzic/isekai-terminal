package tools.isekai.terminal.ui

import androidx.compose.foundation.layout.*
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import tools.isekai.terminal.HostKeyChangedWarning

// 初回接続: fingerprint 確認ダイアログ
@Composable
fun HostKeyUnknownDialog(
    host: String,
    port: Int,
    fingerprint: String,
    onAccept: () -> Unit,
    onReject: () -> Unit,
) {
    AlertDialog(
        onDismissRequest = onReject,
        title = { Text("ホスト鍵の確認") },
        text = {
            Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
                Text("初めて接続するホストです。")
                Text("ホスト: $host:$port", fontFamily = FontFamily.Monospace, fontSize = 13.sp)
                Text("Fingerprint (SHA256):")
                Text(
                    fingerprint,
                    fontFamily = FontFamily.Monospace,
                    fontSize = 11.sp,
                    color = MaterialTheme.colorScheme.primary,
                    modifier = Modifier.padding(start = 8.dp)
                )
                Text("この fingerprint を信頼して接続しますか？", fontSize = 13.sp)
            }
        },
        confirmButton = {
            TextButton(onClick = onAccept) { Text("信頼して接続") }
        },
        dismissButton = {
            TextButton(onClick = onReject) { Text("キャンセル") }
        }
    )
}

// fingerprint 変化: 強い警告ダイアログ (MITM の可能性)
@Composable
fun HostKeyChangedDialog(
    warning: HostKeyChangedWarning,
    onAccept: () -> Unit,
    onReject: () -> Unit,
) {
    AlertDialog(
        onDismissRequest = {},  // force explicit choice
        title = { Text("⚠ ホスト鍵が変わりました", color = Color(0xFFFF6666)) },
        text = {
            Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
                Text(
                    "${warning.host}:${warning.port} のホスト鍵が変更されています。MITM攻撃の可能性があります。",
                    fontSize = 13.sp,
                )
                Text(
                    "保存済み: ${warning.oldFingerprint.take(20)}…",
                    fontSize = 11.sp,
                    color = Color(0xFFAAAAAA),
                )
                Text(
                    "今回:     ${warning.newFingerprint.take(20)}…",
                    fontSize = 11.sp,
                    color = Color(0xFFFF9944),
                )
            }
        },
        confirmButton = {
            TextButton(onClick = onAccept) { Text("更新して接続", color = Color(0xFFFF9944)) }
        },
        dismissButton = {
            TextButton(onClick = onReject) { Text("切断", color = Color(0xFFFF6666)) }
        },
    )
}
