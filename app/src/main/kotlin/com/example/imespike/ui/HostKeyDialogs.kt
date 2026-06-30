package com.example.imespike.ui

import androidx.compose.foundation.layout.*
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp

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
    host: String,
    port: Int,
    newFingerprint: String,
    oldFingerprint: String,
    onAcceptUpdate: () -> Unit,
    onReject: () -> Unit,
) {
    AlertDialog(
        onDismissRequest = onReject,
        title = { Text("警告: ホスト鍵が変更されています", color = Color(0xFFCC0000)) },
        text = {
            Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
                Text(
                    "中間者攻撃 (MITM) の可能性があります！",
                    color = Color(0xFFCC0000),
                )
                Text("ホスト: $host:$port", fontFamily = FontFamily.Monospace, fontSize = 13.sp)
                Text("保存済み fingerprint:")
                Text(
                    oldFingerprint,
                    fontFamily = FontFamily.Monospace,
                    fontSize = 10.sp,
                    color = Color(0xFFCC0000),
                    modifier = Modifier.padding(start = 8.dp)
                )
                Text("現在の fingerprint:")
                Text(
                    newFingerprint,
                    fontFamily = FontFamily.Monospace,
                    fontSize = 10.sp,
                    color = MaterialTheme.colorScheme.primary,
                    modifier = Modifier.padding(start = 8.dp)
                )
                Text("サーバが正規に更新された場合のみ「更新して接続」を選んでください。", fontSize = 12.sp)
            }
        },
        confirmButton = {
            TextButton(onClick = onAcceptUpdate) { Text("更新して接続", color = Color(0xFFCC0000)) }
        },
        dismissButton = {
            TextButton(onClick = onReject) { Text("キャンセル（安全）") }
        }
    )
}
