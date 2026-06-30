package com.example.imespike

import android.net.Uri
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.*
import androidx.compose.material3.*
import androidx.compose.runtime.*
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

@Composable
fun KeyImportScreen(
    onSave: () -> Unit,
    onCancel: () -> Unit,
) {
    val context = LocalContext.current
    val scope = rememberCoroutineScope()

    var label by remember { mutableStateOf("") }
    var selectedUri by remember { mutableStateOf<Uri?>(null) }
    var selectedFileName by remember { mutableStateOf<String?>(null) }
    var errorMsg by remember { mutableStateOf<String?>(null) }
    var saving by remember { mutableStateOf(false) }

    val launcher = rememberLauncherForActivityResult(
        ActivityResultContracts.GetContent()
    ) { uri ->
        selectedUri = uri
        selectedFileName = uri?.lastPathSegment ?: uri?.let { "選択済み" }
        if (uri != null) RemoteLogger.i("TsshKey", "file selected via SAF: $selectedFileName uri=$uri")
        else RemoteLogger.i("TsshKey", "SAF file picker cancelled")
    }

    Column(
        modifier = Modifier
            .fillMaxSize()
            .background(Color(0xFF101018))
            .padding(16.dp),
        verticalArrangement = Arrangement.spacedBy(12.dp),
    ) {
        Text("秘密鍵をインポート", color = Color.White, fontSize = 18.sp)

        OutlinedTextField(
            value = label,
            onValueChange = { label = it },
            label = { Text("ラベル") },
            singleLine = true,
            modifier = Modifier.fillMaxWidth(),
        )

        Button(
            onClick = { launcher.launch("*/*") },
            modifier = Modifier.fillMaxWidth(),
        ) { Text("PEM ファイルを選択") }

        selectedFileName?.let {
            Text("選択中: $it", color = Color(0xFFAAAAAA), fontSize = 12.sp)
        }

        errorMsg?.let {
            Text(it, color = Color(0xFFFF6666), fontSize = 12.sp)
        }

        Spacer(Modifier.weight(1f))

        Row(horizontalArrangement = Arrangement.spacedBy(12.dp)) {
            OutlinedButton(
                onClick = onCancel,
                modifier = Modifier.weight(1f),
            ) { Text("キャンセル") }

            Button(
                onClick = {
                    val uri = selectedUri
                    if (uri == null) {
                        errorMsg = "PEM ファイルを選択してください"
                        return@Button
                    }
                    if (label.isBlank()) {
                        errorMsg = "ラベルを入力してください"
                        return@Button
                    }
                    errorMsg = null
                    saving = true
                    RemoteLogger.i("TsshKey", "import start: label='$label' file='$selectedFileName'")
                    scope.launch {
                        try {
                            val pemBytes = withContext(Dispatchers.IO) {
                                context.contentResolver.openInputStream(uri)?.use { it.readBytes() }
                                    ?: throw IllegalStateException("ファイルを読み込めませんでした")
                            }
                            RemoteLogger.i("TsshKey", "read PEM: ${pemBytes.size} bytes")
                            withContext(Dispatchers.IO) {
                                val path = KeyManager.saveEncryptedKey(context, pemBytes)
                                val hint = KeyManager.extractPublicKeyHint(pemBytes)
                                RemoteLogger.i("TsshKey", "encrypted key saved → $path")
                                Repositories.init(context)
                                val id = Repositories.keys.save(
                                    KeyEntry(
                                        label = label,
                                        publicKey = hint,
                                        encryptedPrivateKeyPath = path,
                                        kekAlias = KeyManager.KEK_ALIAS,
                                        createdAt = System.currentTimeMillis(),
                                    )
                                )
                                RemoteLogger.i("TsshKey", "key saved to DB: id=$id label='$label'")
                            }
                            onSave()
                        } catch (e: Exception) {
                            RemoteLogger.e("TsshKey", "import failed: ${e.message}", e)
                            errorMsg = "保存に失敗しました: ${e.message}"
                        } finally {
                            saving = false
                        }
                    }
                },
                enabled = !saving,
                modifier = Modifier.weight(1f),
            ) { Text(if (saving) "保存中…" else "保存") }
        }
    }
}
