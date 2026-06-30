package com.example.imespike.spike

import android.content.Context
import android.content.Intent
import android.widget.Toast
import androidx.compose.foundation.layout.*
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.unit.dp
import com.example.imespike.util.RemoteLogger

/**
 * P0 スパイク選択画面。
 * 各スパイクを個別に起動して確認できる。
 */
@Composable
fun P0SpikeScreen(
    onShowCanvas: () -> Unit,
    onShowP1: () -> Unit = {},
    onShowP2: () -> Unit = {},
    onShowTerminal: () -> Unit = {},
) {
    val context = LocalContext.current

    Column(
        modifier = Modifier
            .fillMaxSize()
            .padding(16.dp),
        verticalArrangement = Arrangement.spacedBy(12.dp)
    ) {
        Text("スパイク検証", style = MaterialTheme.typography.headlineSmall)

        Text("── P0 ──", style = MaterialTheme.typography.labelMedium)

        Button(onClick = onShowCanvas, modifier = Modifier.fillMaxWidth()) {
            Text("Canvas 描画スパイク（80×24, 60fps）")
        }
        Button(
            onClick = { runKeystoreSpike(context) },
            modifier = Modifier.fillMaxWidth()
        ) {
            Text("Keystore KEK スパイク（暗号化→復号確認）")
        }
        Button(
            onClick = { startForegroundServiceSpike(context) },
            modifier = Modifier.fillMaxWidth()
        ) {
            Text("Foreground Service スパイク（通知バー確認）")
        }

        HorizontalDivider()
        Text("── P1 ──", style = MaterialTheme.typography.labelMedium)

        Button(onClick = onShowP1, modifier = Modifier.fillMaxWidth()) {
            Text("P1: SSH PTY スパイク（russh 生バイト）")
        }
        Button(onClick = onShowP2, modifier = Modifier.fillMaxWidth()) {
            Text("P2: SSH + VT100 Canvas 描画（vte + kmp-input）")
        }

        HorizontalDivider()
        Text("── ViewModel ──", style = MaterialTheme.typography.labelMedium)

        Button(onClick = onShowTerminal, modifier = Modifier.fillMaxWidth()) {
            Text("TerminalScreen（ViewModel + 回転対応）")
        }
    }
}

private fun runKeystoreSpike(context: Context) {
    RemoteLogger.i("KeystoreSpike", "starting self-test")
    val testData = "-----BEGIN OPENSSH PRIVATE KEY-----\nfake_key_data\n-----END OPENSSH PRIVATE KEY-----"
        .toByteArray()
    try {
        val ok = KeystoreKek.runSelfTest(testData)
        val msg = if (ok) "✅ KEK 暗号化・復号 OK" else "❌ KEK テスト失敗"
        RemoteLogger.i("KeystoreSpike", msg)
        Toast.makeText(context, msg, Toast.LENGTH_LONG).show()
    } catch (e: Exception) {
        RemoteLogger.e("KeystoreSpike", "exception", e)
        Toast.makeText(context, "❌ 例外: ${e.message}", Toast.LENGTH_LONG).show()
    }
}

private fun startForegroundServiceSpike(context: Context) {
    RemoteLogger.i("FgsSpike", "starting foreground service")
    try {
        val intent = Intent(context, TerminalSessionService::class.java).apply {
            putExtra(TerminalSessionService.EXTRA_SESSION_LABEL, "example.com (spike)")
        }
        context.startForegroundService(intent)
        RemoteLogger.i("FgsSpike", "startForegroundService() called OK")
        Toast.makeText(context, "Foreground Service 起動 → 通知バーを確認", Toast.LENGTH_LONG).show()
    } catch (e: Exception) {
        RemoteLogger.e("FgsSpike", "exception", e)
        Toast.makeText(context, "❌ 例外: ${e.message}", Toast.LENGTH_LONG).show()
    }
}
