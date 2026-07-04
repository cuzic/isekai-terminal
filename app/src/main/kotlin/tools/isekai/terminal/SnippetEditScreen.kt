package tools.isekai.terminal

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.safeDrawingPadding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.Button
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.ExposedDropdownMenuBox
import androidx.compose.material3.ExposedDropdownMenuDefaults
import androidx.compose.material3.MenuAnchorType
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Switch
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import androidx.lifecycle.viewmodel.compose.viewModel
import tools.isekai.terminal.data.Snippet
import tools.isekai.terminal.util.RemoteLogger

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun SnippetEditScreen(
    snippet: Snippet? = null,
    onSave: () -> Unit,
    onCancel: () -> Unit,
) {
    val vm: SnippetEditViewModel = viewModel()
    val profiles by vm.profiles.collectAsStateWithLifecycle()
    val isSaving by vm.isSaving.collectAsStateWithLifecycle()

    var label by remember { mutableStateOf(snippet?.label ?: "") }
    var command by remember { mutableStateOf(snippet?.command ?: "") }
    var appendNewline by remember { mutableStateOf(snippet?.appendNewline ?: true) }
    var profileId by remember { mutableStateOf(snippet?.profileId) }
    var profileMenuExpanded by remember { mutableStateOf(false) }

    val selectedProfileLabel = profiles.firstOrNull { it.id == profileId }?.label ?: "全プロファイル共通"
    val canSave = label.isNotBlank() && command.isNotBlank()

    Column(
        modifier = Modifier
            .fillMaxSize()
            .safeDrawingPadding()
            .verticalScroll(rememberScrollState())
            .padding(16.dp),
        verticalArrangement = Arrangement.spacedBy(12.dp),
    ) {
        Text(
            text = if (snippet == null) "定型コマンド追加" else "定型コマンド編集",
            fontWeight = FontWeight.Bold,
            fontSize = 20.sp,
        )

        OutlinedTextField(
            value = label,
            onValueChange = { label = it },
            label = { Text("ラベル") },
            singleLine = true,
            modifier = Modifier.fillMaxWidth(),
        )

        OutlinedTextField(
            value = command,
            onValueChange = { command = it },
            label = { Text("コマンド（複数行可）") },
            modifier = Modifier
                .fillMaxWidth()
                .heightIn(min = 100.dp),
        )

        Text(
            "注意: パスワードなどの機密情報をここに平文で書くと、保護されずデータベースに残ります。",
            color = MaterialTheme.colorScheme.error,
            fontSize = 12.sp,
        )

        Row(
            modifier = Modifier.fillMaxWidth(),
            horizontalArrangement = Arrangement.SpaceBetween,
        ) {
            Text(
                "末尾で Enter する",
                modifier = androidx.compose.ui.Modifier.align(androidx.compose.ui.Alignment.CenterVertically),
            )
            Switch(
                checked = appendNewline,
                onCheckedChange = { appendNewline = it },
                modifier = Modifier.testTag("appendNewlineSwitch"),
            )
        }

        Text("適用範囲")
        ExposedDropdownMenuBox(
            expanded = profileMenuExpanded,
            onExpandedChange = { profileMenuExpanded = it },
        ) {
            OutlinedTextField(
                value = selectedProfileLabel,
                onValueChange = {},
                readOnly = true,
                label = { Text("プロファイル") },
                trailingIcon = {
                    ExposedDropdownMenuDefaults.TrailingIcon(expanded = profileMenuExpanded)
                },
                modifier = Modifier
                    .fillMaxWidth()
                    .menuAnchor(MenuAnchorType.PrimaryNotEditable),
            )
            ExposedDropdownMenu(
                expanded = profileMenuExpanded,
                onDismissRequest = { profileMenuExpanded = false },
            ) {
                DropdownMenuItem(
                    text = { Text("全プロファイル共通") },
                    onClick = { profileId = null; profileMenuExpanded = false },
                )
                profiles.forEach { p ->
                    DropdownMenuItem(
                        text = { Text(p.label) },
                        onClick = { profileId = p.id; profileMenuExpanded = false },
                    )
                }
            }
        }

        Spacer(Modifier.height(8.dp))

        Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            Button(
                onClick = {
                    val saved = Snippet(
                        id = snippet?.id ?: 0,
                        label = label.trim(),
                        command = command,
                        sortOrder = snippet?.sortOrder ?: 0,
                        profileId = profileId,
                        appendNewline = appendNewline,
                    )
                    vm.save(saved) { onSave() }
                },
                enabled = canSave && !isSaving,
            ) { Text("保存") }
            OutlinedButton(onClick = {
                RemoteLogger.i("TsshSnippet", "cancelled snippet edit (${if (snippet == null) "new" else "id=${snippet.id} '${snippet.label}'"})")
                onCancel()
            }) { Text("キャンセル") }
        }
    }
}
