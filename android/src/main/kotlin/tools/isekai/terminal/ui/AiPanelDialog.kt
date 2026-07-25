package tools.isekai.terminal.ui

import androidx.compose.foundation.layout.*
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import tools.isekai.terminal.AiPanelUiState
import uniffi.isekai_terminal_core.PanelField
import uniffi.isekai_terminal_core.PanelFieldKind
import uniffi.isekai_terminal_core.PanelKind

/**
 * `AI_INTEGRATION_DESIGN.md` §6.2: リモートAPC経由で提示された構造化パネル
 * (presentDocument/presentForm)を表示するダイアログ。
 *
 * **信頼境界**: [panel]の内容(title/markdown/fields)はリモートの任意プロセスが
 * 偽造できるPTY上のin-bandデータであり、このダイアログはそれを**表示専用テキストとして
 * しか扱わない**——自動実行・シェルコマンド化・クリップボード書き込み等の副作用を
 * 一切引き起こさない(`ai_panel.rs`のモジュールdoc参照)。冒頭に「リモートから受信」
 * である旨を常に明示する。
 *
 * `presentForm`の送信結果は[onSubmit]経由でPTYへの通常のテキスト入力として
 * 返るだけで(`TerminalSession.submitAiPanelForm`参照)、専用の実行チャネルは無い。
 */
@Composable
fun AiPanelSheet(
    panel: AiPanelUiState,
    onSubmit: (Map<String, String>) -> Unit,
    onDismiss: () -> Unit,
) {
    val fieldValues = remember(panel) { mutableStateMapOf<String, String>() }

    AlertDialog(
        onDismissRequest = onDismiss,
        title = {
            Column {
                Text(
                    "📡 リモートから受信したパネル",
                    fontSize = 11.sp,
                    color = AppColors.SecondaryText,
                )
                Text(panel.title.ifBlank { "(無題)" })
            }
        },
        text = {
            Column(
                modifier = Modifier
                    .heightIn(max = 400.dp)
                    .verticalScroll(rememberScrollState()),
                verticalArrangement = Arrangement.spacedBy(12.dp),
            ) {
                when (panel.kind) {
                    PanelKind.DOCUMENT -> {
                        // MVPスコープ: Markdownの構文解釈はせず生テキストとして表示する
                        // (`AI_INTEGRATION_DESIGN.md` §6.2、最小スコープの明記通り)。
                        Text(panel.markdown)
                    }
                    PanelKind.FORM -> {
                        panel.fields.forEach { field ->
                            AiPanelFormField(
                                field = field,
                                value = fieldValues[field.id] ?: "",
                                onValueChange = { fieldValues[field.id] = it },
                            )
                        }
                    }
                    PanelKind.NONE -> {}
                }
            }
        },
        confirmButton = {
            if (panel.kind == PanelKind.FORM) {
                TextButton(onClick = { onSubmit(fieldValues.toMap()) }) { Text("送信") }
            } else {
                TextButton(onClick = onDismiss) { Text("閉じる") }
            }
        },
        dismissButton = {
            if (panel.kind == PanelKind.FORM) {
                TextButton(onClick = onDismiss) { Text("キャンセル") }
            }
        },
    )
}

@Composable
private fun AiPanelFormField(
    field: PanelField,
    value: String,
    onValueChange: (String) -> Unit,
) {
    Column(verticalArrangement = Arrangement.spacedBy(4.dp)) {
        Text(field.label, fontWeight = FontWeight.Medium, fontSize = 13.sp)
        when (field.kind) {
            PanelFieldKind.TEXT -> {
                OutlinedTextField(
                    value = value,
                    onValueChange = onValueChange,
                    modifier = Modifier.fillMaxWidth(),
                    singleLine = true,
                )
            }
            PanelFieldKind.CHOICE -> {
                Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                    field.options.forEach { option ->
                        FilterChip(
                            selected = value == option,
                            onClick = { onValueChange(option) },
                            label = { Text(option) },
                        )
                    }
                }
            }
        }
    }
}
