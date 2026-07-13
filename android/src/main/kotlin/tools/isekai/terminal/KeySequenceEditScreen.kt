package tools.isekai.terminal

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.safeDrawingPadding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.AssistChip
import androidx.compose.material3.Button
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.ExposedDropdownMenuBox
import androidx.compose.material3.ExposedDropdownMenuDefaults
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.MenuAnchorType
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateListOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import androidx.lifecycle.viewmodel.compose.viewModel
import tools.isekai.terminal.data.KeySequence
import tools.isekai.terminal.input.KeyStep
import tools.isekai.terminal.input.SPECIAL_KEY_CHOICES
import tools.isekai.terminal.input.shortLabel
import tools.isekai.terminal.util.RemoteLogger

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun KeySequenceEditScreen(
    keySequence: KeySequence? = null,
    onSave: () -> Unit,
    onCancel: () -> Unit,
) {
    val vm: KeySequenceEditViewModel = viewModel()
    val profiles by vm.profiles.collectAsStateWithLifecycle()
    val isSaving by vm.isSaving.collectAsStateWithLifecycle()

    var label by remember { mutableStateOf(keySequence?.label ?: "") }
    val steps = remember { mutableStateListOf<KeyStep>().apply { addAll(keySequence?.steps ?: emptyList()) } }
    var profileId by remember { mutableStateOf(keySequence?.profileId) }
    var profileMenuExpanded by remember { mutableStateOf(false) }

    var ctrlCharInput by remember { mutableStateOf("") }
    var textStepInput by remember { mutableStateOf("") }
    var specialKeyMenuExpanded by remember { mutableStateOf(false) }
    var selectedSpecialKeyLabel by remember { mutableStateOf(SPECIAL_KEY_CHOICES.first().first) }

    val selectedProfileLabel = profiles.firstOrNull { it.id == profileId }?.label ?: "全プロファイル共通"
    // steps.isNotEmpty() だけでは、Ctrl+1 のような変換不能な文字だけのstepでも保存できてしまい
    // 送信時に無音no-opになる(codexレビュー指摘)。実際にバイト列が出力されることまで確認する。
    val canSave = label.isNotBlank() && KeySequenceCommands.toBytes(steps).isNotEmpty()

    Column(
        modifier = Modifier
            .fillMaxSize()
            .safeDrawingPadding()
            .verticalScroll(rememberScrollState())
            .padding(16.dp),
        verticalArrangement = Arrangement.spacedBy(12.dp),
    ) {
        Text(
            text = if (keySequence == null) "打鍵列追加" else "打鍵列編集",
            fontWeight = FontWeight.Bold,
            fontSize = 20.sp,
        )

        OutlinedTextField(
            value = label,
            onValueChange = { label = it },
            label = { Text("ラベル（例: tmux 新規ウィンドウ）") },
            singleLine = true,
            modifier = Modifier.fillMaxWidth(),
        )

        Text(
            "注意: パスワードなどの機密情報をテキストステップに書くと、保護されずデータベースに残ります。",
            color = MaterialTheme.colorScheme.error,
            fontSize = 12.sp,
        )

        Text("打鍵列")
        if (steps.isEmpty()) {
            Text("まだステップがありません。下から追加してください。", fontSize = 12.sp)
        } else {
            Column(verticalArrangement = Arrangement.spacedBy(4.dp)) {
                steps.forEachIndexed { index, step ->
                    Row(
                        modifier = Modifier.fillMaxWidth(),
                        horizontalArrangement = Arrangement.spacedBy(6.dp),
                    ) {
                        AssistChip(onClick = {}, label = { Text(step.shortLabel()) })
                        OutlinedButton(onClick = { steps.removeAt(index) }) { Text("削除") }
                    }
                }
            }
        }

        Spacer(Modifier.height(4.dp))
        Text("ステップを追加", fontWeight = FontWeight.Bold, fontSize = 14.sp)

        // Ctrlチョード追加
        Row(verticalAlignment = androidx.compose.ui.Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            OutlinedTextField(
                value = ctrlCharInput,
                onValueChange = { if (it.length <= 1) ctrlCharInput = it },
                label = { Text("Ctrl+ (1文字)") },
                singleLine = true,
                modifier = Modifier.width(120.dp),
            )
            Button(
                onClick = {
                    val c = ctrlCharInput.firstOrNull()
                    if (c != null) {
                        steps.add(KeyStep.CtrlChar(c))
                        ctrlCharInput = ""
                    }
                },
                enabled = ctrlCharInput.isNotEmpty(),
            ) { Text("Ctrlを追加") }
        }

        // テキストステップ追加
        Row(verticalAlignment = androidx.compose.ui.Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            OutlinedTextField(
                value = textStepInput,
                onValueChange = { textStepInput = it },
                label = { Text("テキスト") },
                singleLine = true,
                modifier = Modifier.weight(1f),
            )
            Button(
                onClick = {
                    if (textStepInput.isNotEmpty()) {
                        steps.add(KeyStep.Text(textStepInput))
                        textStepInput = ""
                    }
                },
                enabled = textStepInput.isNotEmpty(),
            ) { Text("追加") }
        }

        // 特殊キー追加
        Row(verticalAlignment = androidx.compose.ui.Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            ExposedDropdownMenuBox(
                expanded = specialKeyMenuExpanded,
                onExpandedChange = { specialKeyMenuExpanded = it },
                modifier = Modifier.weight(1f),
            ) {
                OutlinedTextField(
                    value = selectedSpecialKeyLabel,
                    onValueChange = {},
                    readOnly = true,
                    label = { Text("特殊キー") },
                    trailingIcon = {
                        ExposedDropdownMenuDefaults.TrailingIcon(expanded = specialKeyMenuExpanded)
                    },
                    modifier = Modifier
                        .fillMaxWidth()
                        .menuAnchor(MenuAnchorType.PrimaryNotEditable),
                )
                ExposedDropdownMenu(
                    expanded = specialKeyMenuExpanded,
                    onDismissRequest = { specialKeyMenuExpanded = false },
                ) {
                    SPECIAL_KEY_CHOICES.forEach { (choiceLabel, _) ->
                        DropdownMenuItem(
                            text = { Text(choiceLabel) },
                            onClick = { selectedSpecialKeyLabel = choiceLabel; specialKeyMenuExpanded = false },
                        )
                    }
                }
            }
            Button(
                onClick = {
                    val keyCode = SPECIAL_KEY_CHOICES.firstOrNull { it.first == selectedSpecialKeyLabel }?.second
                    if (keyCode != null) steps.add(KeyStep.Special(keyCode))
                },
            ) { Text("追加") }
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
                    val saved = KeySequence.create(
                        label = label.trim(),
                        steps = steps.toList(),
                        sortOrder = keySequence?.sortOrder ?: 0,
                        profileId = profileId,
                        id = keySequence?.id ?: 0,
                    )
                    vm.save(saved) { onSave() }
                },
                enabled = canSave && !isSaving,
            ) { Text("保存") }
            OutlinedButton(onClick = {
                RemoteLogger.i(
                    "IsekaiTerminalKeySequence",
                    "cancelled key sequence edit (${if (keySequence == null) "new" else "id=${keySequence.id} '${keySequence.label}'"})",
                )
                onCancel()
            }) { Text("キャンセル") }
        }
    }
}
