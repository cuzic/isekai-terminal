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
import tools.isekai.terminal.data.ConnectionProfile

/**
 * 定型コマンド/打鍵列の「適用範囲」(全プロファイル共通 or 特定プロファイル専用)を選ぶ
 * `ExposedDropdownMenuBox`。[SnippetEditScreen]/[KeySequenceEditScreen]で
 * ほぼ同一の実装があったものを統合した。
 */
@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun ProfileScopeDropdown(
    profiles: List<ConnectionProfile>,
    selectedId: Long?,
    onSelect: (Long?) -> Unit,
    modifier: Modifier = Modifier,
) {
    var expanded by remember { mutableStateOf(false) }
    val selectedLabel = profiles.firstOrNull { it.id == selectedId }?.label ?: "全プロファイル共通"

    ExposedDropdownMenuBox(
        expanded = expanded,
        onExpandedChange = { expanded = it },
        modifier = modifier,
    ) {
        OutlinedTextField(
            value = selectedLabel,
            onValueChange = {},
            readOnly = true,
            label = { Text("プロファイル") },
            trailingIcon = {
                ExposedDropdownMenuDefaults.TrailingIcon(expanded = expanded)
            },
            modifier = Modifier
                .fillMaxWidth()
                .menuAnchor(MenuAnchorType.PrimaryNotEditable),
        )
        ExposedDropdownMenu(
            expanded = expanded,
            onDismissRequest = { expanded = false },
        ) {
            DropdownMenuItem(
                text = { Text("全プロファイル共通") },
                onClick = { onSelect(null); expanded = false },
            )
            profiles.forEach { p ->
                DropdownMenuItem(
                    text = { Text(p.label) },
                    onClick = { onSelect(p.id); expanded = false },
                )
            }
        }
    }
}
