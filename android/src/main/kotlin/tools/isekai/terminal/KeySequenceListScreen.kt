package tools.isekai.terminal

import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.safeDrawingPadding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.material3.Button
import androidx.compose.material3.Card
import androidx.compose.material3.FloatingActionButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import androidx.lifecycle.viewmodel.compose.viewModel
import tools.isekai.terminal.data.KeySequence
import tools.isekai.terminal.data.KeySequencePackInstallation
import tools.isekai.terminal.input.KeyStep
import tools.isekai.terminal.input.previewText
import tools.isekai.terminal.input.shortLabel
import tools.isekai.terminal.pack.KeySequencePack
import tools.isekai.terminal.ui.DeleteConfirmDialog
import tools.isekai.terminal.util.RemoteLogger

@Composable
fun KeySequenceListScreen(
    onAddKeySequence: () -> Unit,
    onEditKeySequence: (KeySequence) -> Unit,
    onBack: () -> Unit,
) {
    val vm: KeySequenceListViewModel = viewModel()
    val keySequences by vm.keySequences.collectAsStateWithLifecycle()
    val deleteTarget by vm.deleteTarget.collectAsStateWithLifecycle()
    val globalInstallations by vm.globalInstallations.collectAsStateWithLifecycle()

    Scaffold(
        topBar = {
            Row(
                modifier = Modifier
                    .fillMaxWidth()
                    .safeDrawingPadding()
                    .padding(horizontal = 8.dp, vertical = 4.dp),
                horizontalArrangement = Arrangement.SpaceBetween,
            ) {
                Text(
                    "打鍵列",
                    fontWeight = FontWeight.Bold,
                    fontSize = 18.sp,
                    modifier = Modifier.align(Alignment.CenterVertically),
                )
                TextButton(onClick = onBack) { Text("戻る") }
            }
        },
        floatingActionButton = {
            FloatingActionButton(onClick = onAddKeySequence) {
                Text("＋", fontSize = 24.sp)
            }
        }
    ) { innerPadding ->
        Box(
            modifier = Modifier
                .fillMaxSize()
                .padding(innerPadding)
        ) {
            if (keySequences.isEmpty() && vm.packs.isEmpty()) {
                Text(
                    text = "「＋」をタップして打鍵列を追加",
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                    modifier = Modifier.align(Alignment.Center),
                )
            } else {
                LazyColumn(
                    modifier = Modifier
                        .fillMaxSize()
                        .padding(8.dp),
                    verticalArrangement = Arrangement.spacedBy(8.dp),
                ) {
                    if (vm.packs.isNotEmpty()) {
                        item {
                            Text(
                                "パック",
                                fontWeight = FontWeight.Bold,
                                fontSize = 14.sp,
                                modifier = Modifier.padding(vertical = 4.dp),
                            )
                        }
                        items(vm.packs, key = { it.id }) { pack ->
                            KeySequencePackCard(
                                pack = pack,
                                installation = globalInstallations[pack.id],
                                onActivate = { prefixChar -> vm.activatePack(pack, prefixChar) },
                                onDeactivate = { installation -> vm.deactivatePack(installation) },
                            )
                        }
                        item {
                            Text(
                                "打鍵列",
                                fontWeight = FontWeight.Bold,
                                fontSize = 14.sp,
                                modifier = Modifier.padding(vertical = 4.dp),
                            )
                        }
                    }
                    items(keySequences, key = { it.id }) { keySequence ->
                        KeySequenceCard(
                            keySequence = keySequence,
                            onEdit = {
                                RemoteLogger.i("IsekaiTerminalKeySequence", "edit: '${keySequence.label}' id=${keySequence.id}")
                                onEditKeySequence(keySequence)
                            },
                            onDelete = { vm.requestDelete(keySequence) },
                        )
                    }
                }
            }
        }
    }

    deleteTarget?.let { target ->
        DeleteConfirmDialog(
            title = "削除確認",
            message = "「${target.label}」を削除しますか？",
            onConfirm = { vm.confirmDelete(target) },
            onDismiss = { vm.dismissDelete() },
        )
    }
}

@Composable
private fun KeySequenceCard(
    keySequence: KeySequence,
    onEdit: () -> Unit,
    onDelete: () -> Unit,
) {
    Card(
        modifier = Modifier
            .fillMaxWidth()
            .clickable(onClick = onEdit),
    ) {
        Row(
            modifier = Modifier
                .fillMaxWidth()
                .padding(12.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Column(modifier = Modifier.weight(1f)) {
                Text(
                    text = keySequence.label,
                    fontWeight = FontWeight.Bold,
                    fontSize = 16.sp,
                )
                Spacer(Modifier.width(2.dp))
                Text(
                    text = keySequence.steps.previewText(),
                    fontFamily = FontFamily.Monospace,
                    fontSize = 13.sp,
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                    maxLines = 1,
                )
                Text(
                    text = if (keySequence.profileId == null) "全プロファイル共通" else "特定プロファイル専用",
                    fontSize = 12.sp,
                    color = MaterialTheme.colorScheme.primary,
                )
            }
            TextButton(onClick = onEdit) { Text("編集") }
            TextButton(onClick = onDelete) { Text("削除") }
        }
    }
}

/**
 * 打鍵列セット(パック)の有効化状態カード。MVPではグローバル有効化(profileId=null)のみを
 * この画面から操作できる。prefixキーの入力は[KeySequenceEditScreen]のCtrlチョード追加欄と
 * 同じ「1文字入力」方式(専用のキーキャプチャUIは将来検討)。
 */
@Composable
private fun KeySequencePackCard(
    pack: KeySequencePack,
    installation: KeySequencePackInstallation?,
    onActivate: (Char) -> Unit,
    onDeactivate: (KeySequencePackInstallation) -> Unit,
) {
    val currentPrefixChar = (installation?.paramValues?.get("prefix") as? KeyStep.CtrlChar)?.char
        ?: (pack.params.firstOrNull { it.name == "prefix" }?.default as? KeyStep.CtrlChar)?.char
    var prefixInput by remember(installation) { mutableStateOf(currentPrefixChar?.toString() ?: "") }

    Card(modifier = Modifier.fillMaxWidth()) {
        Column(modifier = Modifier.padding(12.dp)) {
            Row(verticalAlignment = Alignment.CenterVertically) {
                Text(pack.name, fontWeight = FontWeight.Bold, fontSize = 16.sp, modifier = Modifier.weight(1f))
                Text(
                    text = if (installation == null) "未有効化" else "有効",
                    fontSize = 12.sp,
                    color = if (installation == null) MaterialTheme.colorScheme.onSurfaceVariant else MaterialTheme.colorScheme.primary,
                )
            }
            Text(
                text = pack.sequences.joinToString(" / ") { it.label },
                fontSize = 12.sp,
                color = MaterialTheme.colorScheme.onSurfaceVariant,
            )
            Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                OutlinedTextField(
                    value = prefixInput,
                    onValueChange = { if (it.length <= 1) prefixInput = it },
                    label = { Text("Ctrl+ (1文字)") },
                    singleLine = true,
                    modifier = Modifier.width(120.dp),
                )
                Button(
                    onClick = { prefixInput.firstOrNull()?.let(onActivate) },
                    enabled = prefixInput.isNotEmpty(),
                ) { Text(if (installation == null) "有効化" else "更新") }
                if (installation != null) {
                    OutlinedButton(onClick = { onDeactivate(installation) }) { Text("無効化") }
                }
            }
        }
    }
}
