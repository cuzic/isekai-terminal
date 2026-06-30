package com.example.imespike

import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.safeDrawingPadding
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.material3.Button
import androidx.compose.material3.ButtonDefaults
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.runtime.snapshots.SnapshotStateList
import androidx.compose.runtime.mutableStateListOf
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.saveable.rememberSaveable
import com.example.imespike.data.ConnectionProfile
import com.example.imespike.data.Repositories
import com.example.imespike.util.RemoteLogger
import io.github.isseikz.kmpinput.InputMode
import io.github.isseikz.kmpinput.TerminalInputContainer
import io.github.isseikz.kmpinput.VirtualKey
import io.github.isseikz.kmpinput.rememberTerminalInputContainerState
import java.text.SimpleDateFormat
import java.util.Date
import java.util.Locale

class MainActivity : ComponentActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        // adb reverse tcp:9876 tcp:9876 で開発機にトンネル
        RemoteLogger.init("http://127.0.0.1:9876")
        Repositories.init(this)
        RemoteLogger.i("MainActivity", "app started")
        enableEdgeToEdge()
        setContent {
            MaterialTheme {
                Surface(
                    modifier = Modifier.fillMaxSize(),
                    color = MaterialTheme.colorScheme.background
                ) {
                    AppRoot()
                }
            }
        }
    }
}

@Composable
fun AppRoot() {
    var screen by rememberSaveable { mutableIntStateOf(0) }
    var selectedProfile by rememberSaveable { mutableStateOf<ConnectionProfile?>(null) }
    var selectedPassword by rememberSaveable { mutableStateOf<String?>(null) }
    var editingProfile by rememberSaveable { mutableStateOf<ConnectionProfile?>(null) }

    LaunchedEffect(screen) {
        val name = when (screen) {
            0 -> "ProfileList"
            5 -> "Terminal(profile='${selectedProfile?.label ?: "DEMO"}' host=${selectedProfile?.host ?: "hardcoded"})"
            6 -> if (editingProfile == null) "ProfileEdit(new)" else "ProfileEdit(id=${editingProfile?.id} '${editingProfile?.label}')"
            7 -> "KeyList"
            8 -> "KeyImport"
            else -> "screen$screen"
        }
        RemoteLogger.i("TsshNav", "→ $name")
    }

    when (screen) {
        0 -> ProfileListScreen(
            onConnect = { profile, password ->
                RemoteLogger.i("TsshNav", "ProfileList → Terminal via profile='${profile.label}' authType=${profile.authType}")
                selectedProfile = profile
                selectedPassword = password
                screen = 5
            },
            onAddProfile = { editingProfile = null; screen = 6 },
            onEditProfile = { profile -> editingProfile = profile; screen = 6 },
            onManageKeys = { screen = 7 },
        )
        5 -> TerminalScreen(
            profile = selectedProfile,
            config = if (selectedProfile == null) DEMO_SSH_CONFIG else null,
            password = selectedPassword,
            onBack = { screen = 0 },
        )
        6 -> ProfileEditScreen(
            profile = editingProfile,
            onSave = { screen = 0 },
            onCancel = { screen = 0 },
        )
        7 -> KeyListScreen(
            onImportKey = { screen = 8 },
            onBack = { screen = 0 },
        )
        8 -> KeyImportScreen(
            onSave = { screen = 7 },
            onCancel = { screen = 7 },
        )
    }
}

data class RecvLine(val ts: String, val len: Int, val hex: String, val str: String)

@Composable
fun SpikeScreen() {
    val context = LocalContext.current
    val state = rememberTerminalInputContainerState()
    val logs = remember { mutableStateListOf<RecvLine>() }
    var mode by remember { mutableStateOf(InputMode.TEXT) }

    // ptyInputStream は handler がアタッチされて初めてデータを流す
    // Unit キーで一度だけ起動し、flow から受信し続ける
    LaunchedEffect(Unit) {
        state.ptyInputStream.collect { bytes ->
            logs.add(
                0, RecvLine(
                    ts = nowString(),
                    len = bytes.size,
                    hex = bytes.toHexString(),
                    str = bytes.decodeUtf8OrMark()
                )
            )
        }
    }

    Column(
        modifier = Modifier
            .fillMaxSize()
            .safeDrawingPadding()
            .padding(8.dp)
    ) {
        // モード切替
        Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            ModeButton(
                label = "RAW",
                selected = mode == InputMode.RAW,
                onClick = {
                    mode = InputMode.RAW
                    state.setInputMode(InputMode.RAW)
                }
            )
            ModeButton(
                label = "TEXT",
                selected = mode == InputMode.TEXT,
                onClick = {
                    mode = InputMode.TEXT
                    state.setInputMode(InputMode.TEXT)
                }
            )
            Text(
                text = "現在: $mode",
                modifier = Modifier.padding(top = 8.dp),
                style = MaterialTheme.typography.bodySmall
            )
        }

        Spacer(Modifier.height(4.dp))

        // inject ボタン（Q4b 検証用）
        Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            Button(
                onClick = { state.injectKey(VirtualKey.ESCAPE) },
                colors = ButtonDefaults.buttonColors(containerColor = Color(0xFF8B0000))
            ) { Text("Esc", color = Color.White) }
            Button(
                onClick = { state.injectString("") },
                colors = ButtonDefaults.buttonColors(containerColor = Color(0xFF8B0000))
            ) { Text("Ctrl+C", color = Color.White) }
            Button(
                onClick = { state.injectKey(VirtualKey.BACKSPACE) },
                colors = ButtonDefaults.buttonColors(containerColor = Color(0xFF444444))
            ) { Text("BS", color = Color.White) }
        }

        Spacer(Modifier.height(4.dp))

        // 入力エリア
        TerminalInputContainer(
            state = state,
            inputMode = mode,
            modifier = Modifier
                .fillMaxWidth()
                .height(100.dp)
                .background(Color(0xFFF0F0F0))
        ) {
            Box(
                modifier = Modifier
                    .fillMaxSize()
                    .padding(8.dp)
            ) {
                Text(
                    text = "ここをタップして入力",
                    color = Color.Gray,
                    style = MaterialTheme.typography.bodyMedium
                )
            }
        }

        Spacer(Modifier.height(8.dp))

        // 受信ログ（黒背景・等幅フォント）
        LazyColumn(
            modifier = Modifier
                .weight(1f)
                .fillMaxWidth()
                .background(Color(0xFF1A1A1A))
                .padding(4.dp)
        ) {
            items(logs) { line ->
                RecvRow(line)
            }
        }

        Spacer(Modifier.height(8.dp))

        // 操作ボタン
        Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
            Button(onClick = { logs.clear() }) {
                Text("ログclear")
            }
            Button(onClick = { copyLogsToClipboard(context, logs) }) {
                Text("ログをコピー")
            }
        }
    }
}

@Composable
fun ModeButton(label: String, selected: Boolean, onClick: () -> Unit) {
    Button(
        onClick = onClick,
        colors = ButtonDefaults.buttonColors(
            containerColor = if (selected) MaterialTheme.colorScheme.primary
            else MaterialTheme.colorScheme.surfaceVariant,
            contentColor = if (selected) MaterialTheme.colorScheme.onPrimary
            else MaterialTheme.colorScheme.onSurfaceVariant
        )
    ) {
        Text(label)
    }
}

@Composable
fun RecvRow(line: RecvLine) {
    Column(
        modifier = Modifier
            .fillMaxWidth()
            .padding(vertical = 2.dp)
    ) {
        Text(
            text = "${line.ts}  len=${line.len}",
            color = Color(0xFF888888),
            fontSize = 11.sp,
            fontFamily = FontFamily.Monospace
        )
        Text(
            text = "  hex: ${line.hex}",
            color = Color(0xFF00CC88),
            fontSize = 11.sp,
            fontFamily = FontFamily.Monospace
        )
        Text(
            text = "  str: ${line.str}",
            color = Color.White,
            fontSize = 13.sp,
            fontFamily = FontFamily.Monospace
        )
    }
}

private val timeFmt = SimpleDateFormat("HH:mm:ss.SSS", Locale.getDefault())

private fun nowString(): String = timeFmt.format(Date())

private fun ByteArray.toHexString(): String =
    joinToString(" ") { "%02x".format(it) }

private fun ByteArray.decodeUtf8OrMark(): String = try {
    val s = toString(Charsets.UTF_8)
    if (s.contains('�')) "⟨invalid UTF-8⟩ $s" else s
} catch (e: Exception) {
    "⟨decode error⟩"
}

private fun copyLogsToClipboard(context: Context, logs: SnapshotStateList<RecvLine>) {
    val text = logs.joinToString("\n\n") { line ->
        "${line.ts}  len=${line.len}\n  hex: ${line.hex}\n  str: ${line.str}"
    }
    val clipboard = context.getSystemService(Context.CLIPBOARD_SERVICE) as ClipboardManager
    clipboard.setPrimaryClip(ClipData.newPlainText("IME Spike Log", text))
}
