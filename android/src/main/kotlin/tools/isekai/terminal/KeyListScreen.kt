package tools.isekai.terminal

import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import androidx.lifecycle.viewmodel.compose.viewModel
import tools.isekai.terminal.data.KeyEntry
import tools.isekai.terminal.ui.AppColors
import tools.isekai.terminal.ui.DeleteConfirmDialog
import tools.isekai.terminal.util.RemoteLogger
import java.text.SimpleDateFormat
import java.util.Date
import java.util.Locale

@Composable
fun KeyListScreen(
    onImportKey: () -> Unit,
    onBack: () -> Unit,
) {
    val context = LocalContext.current
    val vm: KeyListViewModel = viewModel()
    val keys by vm.keys.collectAsStateWithLifecycle()
    val deleteTarget by vm.deleteTarget.collectAsStateWithLifecycle()
    val generatedPubKey by vm.generatedPubKey.collectAsStateWithLifecycle()
    val isGenerating by vm.isGenerating.collectAsStateWithLifecycle()

    // key generation dialog state
    var showGenDialog by remember { mutableStateOf(false) }
    var genLabel by remember { mutableStateOf("") }
    var genError by remember { mutableStateOf<String?>(null) }

    val dateFmt = remember { SimpleDateFormat("yyyy-MM-dd HH:mm", Locale.getDefault()) }

    // Delete confirmation dialog
    deleteTarget?.let { key ->
        DeleteConfirmDialog(
            title = "鍵を削除",
            message = "「${key.label}」を削除しますか？この操作は元に戻せません。",
            onConfirm = { vm.confirmDelete(key) },
            onDismiss = { vm.dismissDelete() },
            confirmColor = AppColors.Error,
        )
    }

    // Key generation dialog
    if (showGenDialog) {
        AlertDialog(
            onDismissRequest = { if (!isGenerating) showGenDialog = false },
            title = { Text("ed25519 鍵を生成") },
            text = {
                Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
                    OutlinedTextField(
                        value = genLabel,
                        onValueChange = { genLabel = it; genError = null },
                        label = { Text("ラベル") },
                        singleLine = true,
                        modifier = Modifier.fillMaxWidth(),
                    )
                    genError?.let { Text(it, color = AppColors.Error, fontSize = 12.sp) }
                }
            },
            confirmButton = {
                TextButton(
                    onClick = {
                        if (genLabel.isBlank()) { genError = "ラベルを入力してください"; return@TextButton }
                        genError = null
                        vm.generateKey(
                            label = genLabel,
                            onError = { genError = it },
                            onSuccess = { showGenDialog = false; genLabel = "" },
                        )
                    },
                    enabled = !isGenerating,
                ) { Text(if (isGenerating) "生成中…" else "生成") }
            },
            dismissButton = {
                TextButton(onClick = { showGenDialog = false }, enabled = !isGenerating) {
                    Text("キャンセル")
                }
            },
        )
    }

    // Generated public key copy dialog
    generatedPubKey?.let { pubKey ->
        AlertDialog(
            onDismissRequest = { vm.dismissGeneratedPubKey() },
            title = { Text("鍵を生成しました") },
            text = {
                Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
                    Text("以下の公開鍵をサーバーの ~/.ssh/authorized_keys に追加してください。",
                        fontSize = 12.sp, color = AppColors.MutedText)
                    Text(
                        pubKey,
                        fontSize = 10.sp,
                        color = Color(0xFF55FFAA),
                        fontFamily = androidx.compose.ui.text.font.FontFamily.Monospace,
                    )
                }
            },
            confirmButton = {
                TextButton(onClick = {
                    val cm = context.getSystemService(Context.CLIPBOARD_SERVICE) as ClipboardManager
                    cm.setPrimaryClip(ClipData.newPlainText("public key", pubKey))
                    vm.dismissGeneratedPubKey()
                }) { Text("コピーして閉じる") }
            },
            dismissButton = {
                TextButton(onClick = { vm.dismissGeneratedPubKey() }) { Text("閉じる") }
            },
        )
    }

    Scaffold(
        floatingActionButton = {
            Column(
                horizontalAlignment = Alignment.End,
                verticalArrangement = Arrangement.spacedBy(8.dp),
            ) {
                SmallFloatingActionButton(
                    onClick = { genLabel = ""; genError = null; showGenDialog = true },
                    containerColor = MaterialTheme.colorScheme.secondaryContainer,
                ) {
                    Text("生成", fontSize = 11.sp)
                }
                FloatingActionButton(onClick = onImportKey) {
                    Text("＋", fontSize = 24.sp)
                }
            }
        },
        containerColor = AppColors.ScreenBackground,
    ) { padding ->
        Column(
            modifier = Modifier
                .fillMaxSize()
                .padding(padding),
        ) {
            Row(
                modifier = Modifier
                    .fillMaxWidth()
                    .padding(horizontal = 16.dp, vertical = 12.dp),
                horizontalArrangement = Arrangement.SpaceBetween,
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Text("鍵一覧", color = Color.White, fontSize = 18.sp)
                TextButton(onClick = onBack) { Text("戻る", color = AppColors.SecondaryText) }
            }

            if (keys.isEmpty()) {
                Box(
                    modifier = Modifier.fillMaxSize(),
                    contentAlignment = Alignment.Center,
                ) {
                    Text(
                        "「＋」でインポート、「生成」で新規作成",
                        color = Color.DarkGray,
                        fontSize = 14.sp,
                    )
                }
            } else {
                LazyColumn(
                    modifier = Modifier.fillMaxSize(),
                    contentPadding = PaddingValues(16.dp),
                    verticalArrangement = Arrangement.spacedBy(12.dp),
                ) {
                    items(keys, key = { it.id }) { key ->
                        KeyCard(
                            key = key,
                            createdAtText = dateFmt.format(Date(key.createdAt)),
                            onCopy = {
                                RemoteLogger.i("IsekaiTerminalKey", "copied public key: '${key.label}' id=${key.id}")
                                val cm = context.getSystemService(Context.CLIPBOARD_SERVICE) as ClipboardManager
                                cm.setPrimaryClip(ClipData.newPlainText("public key", key.publicKey))
                            },
                            onDelete = { vm.requestDelete(key) },
                        )
                    }
                }
            }
        }
    }
}

@Composable
private fun KeyCard(
    key: KeyEntry,
    createdAtText: String,
    onCopy: () -> Unit,
    onDelete: () -> Unit,
) {
    Column(
        modifier = Modifier
            .fillMaxWidth()
            .background(AppColors.CardBackground, shape = MaterialTheme.shapes.medium)
            .padding(12.dp),
        verticalArrangement = Arrangement.spacedBy(4.dp),
    ) {
        Text(key.label, color = Color.White, fontSize = 16.sp)
        Text(createdAtText, color = Color(0xFF888888), fontSize = 11.sp)
        Text(
            key.publicKey,
            color = AppColors.MutedText,
            fontSize = 11.sp,
        )
        Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            TextButton(onClick = onCopy) { Text("コピー", color = Color.Cyan, fontSize = 12.sp) }
            TextButton(onClick = onDelete) { Text("削除", color = AppColors.Error, fontSize = 12.sp) }
        }
    }
}
