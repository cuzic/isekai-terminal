package tools.isekai.terminal.ui

import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.remember
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.semantics.testTagsAsResourceId
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import tools.isekai.terminal.BuildConfig

/**
 * 項目2(OEMバッテリー最適化への案内UI)のダイアログ。2つの経路から開かれる:
 *
 * 1. コールドスタート時、「予期しないkillが2回以上」等の条件を満たしたときに自動表示
 *    (`TerminalTabsViewModel`のkill検出→`decide_battery_guidance`呼び出し経由)。
 * 2. `ProfileListScreen`のオーバーフローメニュー「バックグラウンド動作」から、ユーザーが
 *    いつでも自分で開く(nagなしの恒常入口)。
 *
 * どちらの経路でも同じComposableを使う——文言・トグルの意味が変わらないため。
 * `ConfirmDialogs.kt`の`DeleteConfirmDialog`と同じく、別ウィンドウ(Dialog)描画のため
 * `testTagsAsResourceId`をこのComposable自身のmodifierにも設定する。
 */
@Composable
fun BackgroundReliabilityDialog(
    manufacturer: String,
    optedOut: Boolean,
    onOptOutChanged: (Boolean) -> Unit,
    onOpenSettings: () -> Unit,
    onDismiss: () -> Unit,
) {
    val copy = remember(manufacturer) { BatteryGuidanceCopy.forManufacturer(manufacturer) }

    AlertDialog(
        modifier = Modifier.semantics { testTagsAsResourceId = BuildConfig.DEBUG },
        onDismissRequest = onDismiss,
        title = { Text(copy.title) },
        text = {
            Column(verticalArrangement = Arrangement.spacedBy(10.dp)) {
                Text(copy.body, fontSize = 13.sp)
                copy.oemHint?.let { hint ->
                    Text(hint, fontSize = 12.sp, color = MaterialTheme.colorScheme.onSurfaceVariant)
                }
                SettingSwitchRow(
                    title = "今後表示しない",
                    checked = optedOut,
                    onCheckedChange = onOptOutChanged,
                    explanation = "オンにすると、この案内は予期しない切断が続いても自動的には表示" +
                        "されなくなります(メニューの「バックグラウンド動作」からいつでも" +
                        "設定画面を開けます)。",
                    testTag = "batteryGuidanceOptOutSwitch",
                )
            }
        },
        confirmButton = {
            TextButton(
                onClick = onOpenSettings,
                modifier = Modifier.testTag("batteryGuidanceOpenSettingsButton"),
            ) { Text("設定を開く") }
        },
        dismissButton = {
            TextButton(
                onClick = onDismiss,
                modifier = Modifier.testTag("batteryGuidanceDismissButton"),
            ) { Text("閉じる") }
        },
    )
}
