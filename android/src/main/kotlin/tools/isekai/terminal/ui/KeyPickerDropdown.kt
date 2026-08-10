package tools.isekai.terminal.ui

import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.ExposedDropdownMenuBox
import androidx.compose.material3.ExposedDropdownMenuDefaults
import androidx.compose.material3.MenuAnchorType
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.testTag
import tools.isekai.terminal.data.KeyEntry

/**
 * 登録済み鍵([KeyEntry])を選択する `ExposedDropdownMenuBox`。[ProfileEditScreen]で
 * 接続先本体/踏み台の2箇所にほぼ同一の実装があったものを統合した。
 */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun KeyPickerDropdown(
    label: String,
    keys: List<KeyEntry>,
    selectedId: Long?,
    onSelect: (Long) -> Unit,
    modifier: Modifier = Modifier,
    testTag: String? = null,
) {
    var expanded by remember { mutableStateOf(false) }
    val selectedLabel = keys.firstOrNull { it.id == selectedId }?.label ?: "鍵を選択"

    ExposedDropdownMenuBox(
        expanded = expanded,
        onExpandedChange = { expanded = it },
        modifier = modifier,
    ) {
        OutlinedTextField(
            value = selectedLabel,
            onValueChange = {},
            readOnly = true,
            label = { Text(label) },
            trailingIcon = {
                ExposedDropdownMenuDefaults.TrailingIcon(expanded = expanded)
            },
            modifier = Modifier
                .fillMaxWidth()
                .menuAnchor(MenuAnchorType.PrimaryNotEditable)
                .let { m -> testTag?.let { m.testTag(it) } ?: m },
        )
        ExposedDropdownMenu(
            expanded = expanded,
            onDismissRequest = { expanded = false },
        ) {
            if (keys.isEmpty()) {
                DropdownMenuItem(
                    text = { Text("登録された鍵がありません") },
                    onClick = { expanded = false },
                )
            } else {
                keys.forEach { key ->
                    DropdownMenuItem(
                        text = { Text(key.label) },
                        onClick = {
                            onSelect(key.id)
                            expanded = false
                        },
                    )
                }
            }
        }
    }
}
