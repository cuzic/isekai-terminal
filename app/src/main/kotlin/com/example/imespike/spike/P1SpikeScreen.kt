package com.example.imespike.spike

import androidx.compose.foundation.background
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.lazy.rememberLazyListState
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import com.example.imespike.util.RemoteLogger
import kotlinx.coroutines.launch
import uniffi.tssh_core.*

// スパイク専用鍵（authorized_keys に登録済み）
private val SPIKE_PRIVATE_KEY = """
-----BEGIN OPENSSH PRIVATE KEY-----
b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAMwAAAAtzc2gtZW
QyNTUxOQAAACBNaPPeQKkLg4/+hUVvT/zkbm2JQVDGiHwFu9T7gxbFLwAAAJiHO6aQhzum
kAAAAAtzc2gtZWQyNTUxOQAAACBNaPPeQKkLg4/+hUVvT/zkbm2JQVDGiHwFu9T7gxbFLw
AAAEBUVU4DNWj7Uk8pQQCJD3MPZNG94+cgGXwMamfL+zzhJ01o895AqQuDj/6FRW9P/ORu
bYlBUMaIfAW71PuDFsUvAAAAEnRzc2gtYW5kcm9pZC1zcGlrZQECAw==
-----END OPENSSH PRIVATE KEY-----
""".trimIndent()

private val SPIKE_CONFIG = SshConfig(
    host = "100.100.45.36",
    port = 22u,
    username = "cuzic",
    auth = SshAuth.PublicKey(privateKeyPem = SPIKE_PRIVATE_KEY.encodeToByteArray()),
    cols = 80u,
    rows = 24u,
)

@Composable
fun P1SpikeScreen(onBack: () -> Unit) {
    val logs = remember { mutableStateListOf<Pair<String, Color>>() }
    val listState = rememberLazyListState()
    val scope = rememberCoroutineScope()
    var session by remember { mutableStateOf<SshSession?>(null) }
    var connected by remember { mutableStateOf(false) }
    var inputText by remember { mutableStateOf("") }

    fun addLog(msg: String, color: Color = Color(0xFFCCCCCC)) {
        logs.add(Pair(msg, color))
        scope.launch { listState.animateScrollToItem(logs.size - 1) }
    }

    Column(
        modifier = Modifier
            .fillMaxSize()
            .background(Color.Black)
            .padding(8.dp)
    ) {
        // ヘッダー
        Row(
            modifier = Modifier.fillMaxWidth(),
            horizontalArrangement = Arrangement.SpaceBetween
        ) {
            Text("P1: SSH PTY スパイク", color = Color.Yellow, fontSize = 14.sp)
            TextButton(onClick = onBack) { Text("戻る", color = Color.Gray) }
        }
        Text(
            "→ ${SPIKE_CONFIG.host}:${SPIKE_CONFIG.port}  user=${SPIKE_CONFIG.username}",
            color = Color.Gray, fontSize = 11.sp
        )

        Spacer(Modifier.height(4.dp))

        // ログ表示
        LazyColumn(
            state = listState,
            modifier = Modifier
                .weight(1f)
                .fillMaxWidth()
                .background(Color(0xFF1A1A1A))
                .padding(4.dp)
        ) {
            items(logs) { (msg, color) ->
                Text(
                    text = msg,
                    color = color,
                    fontSize = 11.sp,
                    fontFamily = FontFamily.Monospace,
                )
            }
        }

        Spacer(Modifier.height(4.dp))

        // 入力行（接続後）
        if (connected) {
            Row(horizontalArrangement = Arrangement.spacedBy(4.dp)) {
                OutlinedTextField(
                    value = inputText,
                    onValueChange = { inputText = it },
                    modifier = Modifier.weight(1f),
                    textStyle = LocalTextStyle.current.copy(
                        color = Color.White,
                        fontFamily = FontFamily.Monospace,
                        fontSize = 13.sp,
                    ),
                    singleLine = true,
                    placeholder = { Text("コマンドを入力…", color = Color.Gray, fontSize = 12.sp) },
                )
                Button(onClick = {
                    val cmd = inputText + "\n"
                    session?.send(cmd.toByteArray())
                    addLog("> $inputText", Color(0xFF88FF88))
                    inputText = ""
                }) { Text("送信") }
            }
            Spacer(Modifier.height(4.dp))
            Row(horizontalArrangement = Arrangement.spacedBy(4.dp)) {
                OutlinedButton(onClick = {
                    session?.send(byteArrayOf(0x03))  // Ctrl+C
                    addLog("[Ctrl+C]", Color.Yellow)
                }) { Text("Ctrl+C", fontSize = 11.sp) }
                OutlinedButton(onClick = {
                    session?.disconnect()
                    connected = false
                    addLog("[disconnect]", Color.Yellow)
                }) { Text("切断", fontSize = 11.sp) }
            }
        } else {
            // 接続ボタン
            Button(
                onClick = {
                    addLog("接続中… ${SPIKE_CONFIG.host}:${SPIKE_CONFIG.port}", Color.Cyan)
                    val s = createSshSession(SPIKE_CONFIG)
                    session = s
                    try { s.connect(object : SessionCallback {
                        override fun onData(data: ByteArray) {
                            val text = data.toString(Charsets.UTF_8)
                            RemoteLogger.d("P1Spike", "rx ${data.size}B: ${text.take(80)}")
                            logs.add(Pair(text, Color.White))
                            scope.launch { listState.animateScrollToItem(logs.size - 1) }
                        }
                        override fun onHostKey(fingerprint: String): Boolean {
                            RemoteLogger.i("P1Spike", "host key: $fingerprint")
                            logs.add(Pair("[host key] $fingerprint", Color(0xFFAAAAAA)))
                            return true
                        }
                        override fun onConnected() {
                            RemoteLogger.i("P1Spike", "connected!")
                            connected = true
                            logs.add(Pair("[connected]", Color(0xFF88FF88)))
                        }
                        override fun onDisconnected(reason: String?) {
                            RemoteLogger.i("P1Spike", "disconnected: $reason")
                            connected = false
                            logs.add(Pair("[disconnected] $reason", Color.Yellow))
                        }
                        override fun onScreenUpdate(update: ScreenUpdate) {}
                        override fun onTrzszRequest(transferId: String, mode: String, suggestedName: String?, expectedSize: ULong?) {}
                        override fun onTrzszDownloadChunk(transferId: String, data: ByteArray, isLast: Boolean) {}
                        override fun onTrzszProgress(transferId: String, transferred: ULong, total: ULong?) {}
                        override fun onTrzszFinished(transferId: String, success: Boolean, message: String?) {}
                    }) } catch (e: SshException) {
                        RemoteLogger.e("P1Spike", "connect error", e)
                        addLog("[error] ${e.message}", Color.Red)
                    }
                },
                modifier = Modifier.fillMaxWidth()
            ) { Text("SSH 接続") }
        }
    }
}
