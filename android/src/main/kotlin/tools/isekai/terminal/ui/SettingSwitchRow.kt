package tools.isekai.terminal.ui

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Switch
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp

/**
 * トグル(Switch)+ONのときだけ表示する説明文、の1ブロック。[ProfileEditScreen]の
 * agent forward/tmux通知/AIパネルの3設定で元々ほぼ同一の実装があったものを統合した。
 */
@Composable
fun SettingSwitchRow(
    title: String,
    checked: Boolean,
    onCheckedChange: (Boolean) -> Unit,
    explanation: String,
    modifier: Modifier = Modifier,
    explanationColor: Color = MaterialTheme.colorScheme.onSurfaceVariant,
    testTag: String? = null,
) {
    Column(modifier = modifier, verticalArrangement = Arrangement.spacedBy(4.dp)) {
        Row(
            modifier = Modifier.fillMaxWidth(),
            horizontalArrangement = Arrangement.SpaceBetween,
        ) {
            Text(title, modifier = Modifier.align(Alignment.CenterVertically))
            Switch(
                checked = checked,
                onCheckedChange = onCheckedChange,
                modifier = testTag?.let { Modifier.testTag(it) } ?: Modifier,
            )
        }
        if (checked) {
            Text(
                explanation,
                fontSize = 12.sp,
                color = explanationColor,
            )
        }
    }
}
