package com.example.imespike

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
import com.example.imespike.data.KeyEntry
import com.example.imespike.data.Repositories
import com.example.imespike.util.RemoteLogger
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import java.io.File
import java.text.SimpleDateFormat
import java.util.Date
import java.util.Locale

@Composable
fun KeyListScreen(
    onImportKey: () -> Unit,
    onBack: () -> Unit,
) {
    val context = LocalContext.current
    val scope = rememberCoroutineScope()

    var keys by remember { mutableStateOf<List<KeyEntry>>(emptyList()) }
    var pendingDelete by remember { mutableStateOf<KeyEntry?>(null) }
    var reloadTrigger by remember { mutableStateOf(0) }

    // key generation dialog state
    var showGenDialog by remember { mutableStateOf(false) }
    var genLabel by remember { mutableStateOf("") }
    var genError by remember { mutableStateOf<String?>(null) }
    var generating by remember { mutableStateOf(false) }

    // show generated public key for copying
    var generatedPubKey by remember { mutableStateOf<String?>(null) }

    LaunchedEffect(reloadTrigger) {
        keys = withContext(Dispatchers.IO) {
            Repositories.init(context)
            val list = Repositories.keys.getAll()
            RemoteLogger.i("TsshKey", "loaded ${list.size} key(s): ${list.map { "'${it.label}'" }}")
            list
        }
    }

    val dateFmt = remember { SimpleDateFormat("yyyy-MM-dd HH:mm", Locale.getDefault()) }

    // Delete confirmation dialog
    pendingDelete?.let { key ->
        AlertDialog(
            onDismissRequest = { pendingDelete = null },
            title = { Text("鍵を削除") },
            text = { Text("「${key.label}」を削除しますか？この操作は元に戻せません。") },
            confirmButton = {
                TextButton(onClick = {
                    pendingDelete = null
                    scope.launch {
                        RemoteLogger.i("TsshKey", "deleting key id=${key.id} '${key.label}'")
                        withContext(Dispatchers.IO) {
                            Repositories.init(context)
                            Repositories.keys.delete(key)
                            runCatching { File(key.encryptedPrivateKeyPath).delete() }
                        }
                        reloadTrigger++
                    }
                }) { Text("削除", color = Color(0xFFFF6666)) }
            },
            dismissButton = {
                TextButton(onClick = { pendingDelete = null }) { Text("キャンセル") }
            },
        )
    }

    // Key generation dialog
    if (showGenDialog) {
        AlertDialog(
            onDismissRequest = { if (!generating) showGenDialog = false },
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
                    genError?.let { Text(it, color = Color(0xFFFF6666), fontSize = 12.sp) }
                }
            },
            confirmButton = {
                TextButton(
                    onClick = {
                        if (genLabel.isBlank()) { genError = "ラベルを入力してください"; return@TextButton }
                        generating = true
                        genError = null
                        scope.launch {
                            try {
                                val (pemBytes, pubKey) = withContext(Dispatchers.Default) {
                                    KeyManager.generateEd25519Pair()
                                }
                                RemoteLogger.i("TsshKey", "generated ed25519 key pair")
                                withContext(Dispatchers.IO) {
                                    val path = KeyManager.saveEncryptedKey(context, pemBytes)
                                    Repositories.init(context)
                                    val id = Repositories.keys.save(
                                        KeyEntry(
                                            label = genLabel,
                                            publicKey = pubKey,
                                            encryptedPrivateKeyPath = path,
                                            kekAlias = KeyManager.KEK_ALIAS,
                                            createdAt = System.currentTimeMillis(),
                                        )
                                    )
                                    RemoteLogger.i("TsshKey", "generated key saved id=$id '${genLabel}'")
                                }
                                showGenDialog = false
                                generatedPubKey = pubKey
                                genLabel = ""
                                reloadTrigger++
                            } catch (e: Exception) {
                                RemoteLogger.e("TsshKey", "key generation failed: ${e.message}", e)
                                genError = "生成失敗: ${e.message}"
                            } finally {
                                generating = false
                            }
                        }
                    },
                    enabled = !generating,
                ) { Text(if (generating) "生成中…" else "生成") }
            },
            dismissButton = {
                TextButton(onClick = { showGenDialog = false }, enabled = !generating) {
                    Text("キャンセル")
                }
            },
        )
    }

    // Generated public key copy dialog
    generatedPubKey?.let { pubKey ->
        AlertDialog(
            onDismissRequest = { generatedPubKey = null },
            title = { Text("鍵を生成しました") },
            text = {
                Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
                    Text("以下の公開鍵をサーバーの ~/.ssh/authorized_keys に追加してください。",
                        fontSize = 12.sp, color = Color(0xFFCCCCCC))
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
                    generatedPubKey = null
                }) { Text("コピーして閉じる") }
            },
            dismissButton = {
                TextButton(onClick = { generatedPubKey = null }) { Text("閉じる") }
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
        containerColor = Color(0xFF101018),
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
                TextButton(onClick = onBack) { Text("戻る", color = Color(0xFFAAAAAA)) }
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
                                RemoteLogger.i("TsshKey", "copied public key: '${key.label}' id=${key.id}")
                                val cm = context.getSystemService(Context.CLIPBOARD_SERVICE) as ClipboardManager
                                cm.setPrimaryClip(ClipData.newPlainText("public key", key.publicKey))
                            },
                            onDelete = { pendingDelete = key },
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
            .background(Color(0xFF1A1A2E), shape = MaterialTheme.shapes.medium)
            .padding(12.dp),
        verticalArrangement = Arrangement.spacedBy(4.dp),
    ) {
        Text(key.label, color = Color.White, fontSize = 16.sp)
        Text(createdAtText, color = Color(0xFF888888), fontSize = 11.sp)
        Text(
            key.publicKey,
            color = Color(0xFFCCCCCC),
            fontSize = 11.sp,
        )
        Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            TextButton(onClick = onCopy) { Text("コピー", color = Color.Cyan, fontSize = 12.sp) }
            TextButton(onClick = onDelete) { Text("削除", color = Color(0xFFFF6666), fontSize = 12.sp) }
        }
    }
}
