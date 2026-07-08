package tools.isekai.terminal

import android.net.Uri
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.*
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import androidx.lifecycle.viewmodel.compose.viewModel
import tools.isekai.terminal.ui.AppColors
import tools.isekai.terminal.util.RemoteLogger

@Composable
fun KeyImportScreen(
    onSave: () -> Unit,
    onCancel: () -> Unit,
) {
    val vm: KeyImportViewModel = viewModel()
    val isSaving by vm.isSaving.collectAsStateWithLifecycle()
    val errorMsg by vm.errorMsg.collectAsStateWithLifecycle()

    var label by remember { mutableStateOf("") }
    var selectedUri by remember { mutableStateOf<Uri?>(null) }
    var selectedFileName by remember { mutableStateOf<String?>(null) }

    val launcher = rememberLauncherForActivityResult(
        ActivityResultContracts.GetContent()
    ) { uri ->
        selectedUri = uri
        selectedFileName = uri?.lastPathSegment ?: uri?.let { "選択済み" }
        if (uri != null) RemoteLogger.i("IsekaiTerminalKey", "file selected via SAF: $selectedFileName uri=$uri")
        else RemoteLogger.i("IsekaiTerminalKey", "SAF file picker cancelled")
    }

    Column(
        modifier = Modifier
            .fillMaxSize()
            .background(AppColors.ScreenBackground)
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
            Text("選択中: $it", color = AppColors.SecondaryText, fontSize = 12.sp)
        }

        errorMsg?.let {
            Text(it, color = AppColors.Error, fontSize = 12.sp)
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
                        vm.setError("PEM ファイルを選択してください")
                        return@Button
                    }
                    if (label.isBlank()) {
                        vm.setError("ラベルを入力してください")
                        return@Button
                    }
                    RemoteLogger.i("IsekaiTerminalKey", "import start: label='$label' file='$selectedFileName'")
                    vm.importKey(uri, label) { onSave() }
                },
                enabled = !isSaving,
                modifier = Modifier.weight(1f),
            ) { Text(if (isSaving) "保存中…" else "保存") }
        }
    }
}
