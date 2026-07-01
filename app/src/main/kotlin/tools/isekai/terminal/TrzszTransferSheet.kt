package tools.isekai.terminal

import android.net.Uri
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.layout.*
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun TrzszTransferSheet(
    state: TrzszUiState,
    onStartUpload: (Uri) -> Unit,
    onStartDownload: () -> Unit,
    onCancel: () -> Unit,
    onDismiss: () -> Unit,
) {
    ModalBottomSheet(onDismissRequest = { /* don't dismiss on swipe during transfer */ }) {
        Column(
            modifier = Modifier
                .fillMaxWidth()
                .padding(horizontal = 16.dp, vertical = 12.dp)
                .navigationBarsPadding(),
            verticalArrangement = Arrangement.spacedBy(12.dp),
        ) {
            when (state) {
                is TrzszUiState.WaitingUser -> {
                    if (state.mode == "upload") {
                        TrzszUploadWaiting(onFilePicked = onStartUpload, onCancel = onCancel)
                    } else {
                        Text("ファイルを受信", style = MaterialTheme.typography.titleMedium)
                        state.suggestedName?.let {
                            Text("ファイル名: $it", color = Color(0xFFAAAAAA), fontSize = 13.sp)
                        }
                        state.expectedSize?.let {
                            Text("サイズ: ${formatBytes(it.toLong())}", color = Color(0xFFAAAAAA), fontSize = 13.sp)
                        }
                        Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                            Button(onClick = onStartDownload, modifier = Modifier.weight(1f)) {
                                Text("受信開始")
                            }
                            OutlinedButton(onClick = onCancel, modifier = Modifier.weight(1f)) {
                                Text("キャンセル")
                            }
                        }
                    }
                }
                is TrzszUiState.InProgress -> {
                    val title = if (state.mode == "upload") "アップロード中" else "ダウンロード中"
                    Text(title, style = MaterialTheme.typography.titleMedium)
                    state.fileName?.let {
                        Text(it, color = Color(0xFFAAAAAA), fontSize = 13.sp)
                    }
                    val progress = state.total?.let { total ->
                        if (total > 0u) state.transferred.toFloat() / total.toFloat() else 0f
                    }
                    if (progress != null) {
                        LinearProgressIndicator(progress = { progress }, modifier = Modifier.fillMaxWidth())
                        Text(
                            "${formatBytes(state.transferred.toLong())} / ${formatBytes(state.total.toLong())}",
                            fontSize = 11.sp, color = Color(0xFFAAAAAA),
                        )
                    } else {
                        LinearProgressIndicator(modifier = Modifier.fillMaxWidth())
                        Text(formatBytes(state.transferred.toLong()), fontSize = 11.sp, color = Color(0xFFAAAAAA))
                    }
                    OutlinedButton(onClick = onCancel, modifier = Modifier.align(Alignment.End)) {
                        Text("キャンセル")
                    }
                }
                is TrzszUiState.Done -> {
                    val (label, color) = if (state.success)
                        "転送完了" to Color(0xFF55FF55)
                    else
                        "転送失敗" to MaterialTheme.colorScheme.error
                    Text(label, color = color, style = MaterialTheme.typography.titleMedium)
                    state.message?.let {
                        Text(it, fontSize = 13.sp, color = Color(0xFFAAAAAA))
                    }
                    Button(onClick = onDismiss, modifier = Modifier.align(Alignment.End)) {
                        Text("閉じる")
                    }
                }
            }
        }
    }
}

@Composable
private fun TrzszUploadWaiting(onFilePicked: (Uri) -> Unit, onCancel: () -> Unit) {
    val launcher = rememberLauncherForActivityResult(ActivityResultContracts.OpenDocument()) { uri ->
        uri?.let { onFilePicked(it) }
    }
    LaunchedEffect(Unit) { launcher.launch(arrayOf("*/*")) }
    Text("ファイルを選択してください", style = MaterialTheme.typography.titleMedium)
    OutlinedButton(onClick = onCancel) { Text("キャンセル") }
}

internal fun formatBytes(bytes: Long): String = when {
    bytes < 1024 -> "$bytes B"
    bytes < 1024 * 1024 -> "${"%.1f".format(bytes / 1024.0)} KB"
    bytes < 1024L * 1024 * 1024 -> "${"%.1f".format(bytes / (1024.0 * 1024))} MB"
    else -> "${"%.2f".format(bytes / (1024.0 * 1024 * 1024))} GB"
}
