package com.example.imespike.spike

import android.graphics.Paint
import android.graphics.Typeface
import androidx.compose.foundation.Canvas
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.text.KeyboardActions
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.nativeCanvas
import androidx.compose.ui.graphics.toArgb
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.text.input.ImeAction
import androidx.compose.ui.text.input.TextFieldValue
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import com.example.imespike.data.HostKeyStatus
import com.example.imespike.data.Repositories
import com.example.imespike.ui.HostKeyChangedDialog
import com.example.imespike.ui.HostKeyUnknownDialog
import com.example.imespike.util.RemoteLogger
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import uniffi.tssh_core.*

// Kotlin/JVM GC から SshSession を保護するための GC root
// Compose の remember は GC が短時間で回収することがあるため top-level で保持する
private var _gcRootSession: SshSession? = null

private val P2_CONFIG = SshConfig(
    host = "100.100.45.36",
    port = 22u,
    username = "cuzic",
    auth = SshAuth.PublicKey(
        privateKeyPem = """
-----BEGIN OPENSSH PRIVATE KEY-----
b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAMwAAAAtzc2gtZW
QyNTUxOQAAACBNaPPeQKkLg4/+hUVvT/zkbm2JQVDGiHwFu9T7gxbFLwAAAJiHO6aQhzum
kAAAAAtzc2gtZWQyNTUxOQAAACBNaPPeQKkLg4/+hUVvT/zkbm2JQVDGiHwFu9T7gxbFLw
AAAEBUVU4DNWj7Uk8pQQCJD3MPZNG94+cgGXwMamfL+zzhJ01o895AqQuDj/6FRW9P/ORu
bYlBUMaIfAW71PuDFsUvAAAAEnRzc2gtYW5kcm9pZC1zcGlrZQECAw==
-----END OPENSSH PRIVATE KEY-----
""".trimIndent().encodeToByteArray()
    ),
    cols = 80u,
    rows = 24u,
)

private sealed class HostKeyDialogState {
    data class Unknown(val host: String, val port: Int, val fingerprint: String, val keyType: String) : HostKeyDialogState()
    data class Changed(val host: String, val port: Int, val fingerprint: String, val oldFingerprint: String) : HostKeyDialogState()
}

@Composable
fun P2SpikeScreen(onBack: () -> Unit) {
    val scope = rememberCoroutineScope()
    val context = LocalContext.current

    // SSH セッション状態
    var session by remember { mutableStateOf<SshSession?>(null) }
    var connected by remember { mutableStateOf(false) }
    var statusMsg by remember { mutableStateOf("未接続") }
    var textFieldValue by remember { mutableStateOf(TextFieldValue("")) }

    // TOFU state
    var hostKeyDialogState by remember { mutableStateOf<HostKeyDialogState?>(null) }

    // vte パース済みスクリーン（Rust側から届く）
    var screenUpdate by remember { mutableStateOf<ScreenUpdate?>(null) }

    // 接続処理
    fun connect() {
        statusMsg = "接続中…"
        val s = createSshSession(P2_CONFIG)
        _gcRootSession = s   // GC root に保持
        session = s
        try {
            s.connect(object : SessionCallback {
                override fun onData(data: ByteArray) {}
                override fun onHostKey(fingerprint: String): Boolean {
                    RemoteLogger.i("P2Spike", "host key: ${fingerprint.take(20)}…")
                    return true  // spike: always trust
                }
                override fun onConnected() {
                    RemoteLogger.i("P2Spike", "connected")
                    connected = true
                    statusMsg = "接続済み — ${P2_CONFIG.host}"
                }
                override fun onDisconnected(reason: String?) {
                    RemoteLogger.i("P2Spike", "disconnected: $reason")
                    connected = false
                    statusMsg = "切断: $reason"
                    _gcRootSession = null
                }
                override fun onScreenUpdate(update: ScreenUpdate) {
                    RemoteLogger.d("P2Spike", "onScreenUpdate: cursor(${update.cursorCol},${update.cursorRow})")
                    screenUpdate = update
                }
                override fun onTrzszRequest(transferId: String, mode: String, suggestedName: String?, expectedSize: ULong?) {}
                override fun onTrzszDownloadChunk(transferId: String, data: ByteArray, isLast: Boolean) {}
                override fun onTrzszProgress(transferId: String, transferred: ULong, total: ULong?) {}
                override fun onTrzszFinished(transferId: String, success: Boolean, message: String?) {}
            })
        } catch (e: SshException) {
            RemoteLogger.e("P2Spike", "connect error", e)
            statusMsg = "エラー: ${e.message}"
            _gcRootSession = null
        }
    }

    // TOFU ダイアログ表示
    when (val state = hostKeyDialogState) {
        is HostKeyDialogState.Unknown -> HostKeyUnknownDialog(
            host = state.host,
            port = state.port,
            fingerprint = state.fingerprint,
            onAccept = {
                hostKeyDialogState = null
                scope.launch(Dispatchers.IO) {
                    try {
                        Repositories.init(context)
                        Repositories.knownHosts.trust(state.host, state.port, state.keyType, state.fingerprint)
                        RemoteLogger.i("P2Spike", "host key trusted and saved")
                    } catch (e: Exception) {
                        RemoteLogger.w("P2Spike", "TOFU trust failed: ${e.message}")
                    }
                }
            },
            onReject = {
                hostKeyDialogState = null
                session?.disconnect()
                statusMsg = "接続を拒否しました (host key 未信頼)"
            }
        )
        is HostKeyDialogState.Changed -> HostKeyChangedDialog(
            host = state.host,
            port = state.port,
            newFingerprint = state.fingerprint,
            oldFingerprint = state.oldFingerprint,
            onAcceptUpdate = {
                hostKeyDialogState = null
                scope.launch(Dispatchers.IO) {
                    try {
                        Repositories.init(context)
                        Repositories.knownHosts.trust(state.host, state.port, "ssh-ed25519", state.fingerprint)
                        RemoteLogger.i("P2Spike", "host key updated and saved")
                    } catch (e: Exception) {
                        RemoteLogger.w("P2Spike", "TOFU update failed: ${e.message}")
                    }
                }
            },
            onReject = {
                hostKeyDialogState = null
                session?.disconnect()
                statusMsg = "接続を拒否しました (host key 変更)"
            }
        )
        null -> {}
    }

    Column(
        modifier = Modifier
            .fillMaxSize()
            .background(Color.Black)
            .imePadding(),
    ) {
        // ステータスバー
        Row(
            modifier = Modifier
                .fillMaxWidth()
                .background(Color(0xFF1A1A2E))
                .padding(horizontal = 8.dp, vertical = 4.dp),
            horizontalArrangement = Arrangement.SpaceBetween
        ) {
            Text(statusMsg, color = if (connected) Color(0xFF55FF55) else Color.Yellow, fontSize = 11.sp)
            Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                if (!connected) {
                    TextButton(onClick = { connect() }, contentPadding = PaddingValues(0.dp)) {
                        Text("接続", color = Color.Cyan, fontSize = 11.sp)
                    }
                } else {
                    TextButton(onClick = { session?.disconnect(); connected = false }, contentPadding = PaddingValues(0.dp)) {
                        Text("切断", color = Color.Gray, fontSize = 11.sp)
                    }
                }
                TextButton(onClick = onBack, contentPadding = PaddingValues(0.dp)) {
                    Text("戻る", color = Color.Gray, fontSize = 11.sp)
                }
            }
        }

        // ターミナル Canvas（メイン）
        val update = screenUpdate
        if (update != null) {
            BoxWithConstraints(
                modifier = Modifier.weight(1f).fillMaxWidth()
            ) {
                val density = LocalDensity.current
                val widthPx = with(density) { maxWidth.toPx() }
                val heightPx = with(density) { maxHeight.toPx() }

                val cellDims = remember(density) {
                    Paint().apply {
                        typeface = Typeface.MONOSPACE
                        textSize = 14f * density.density
                    }.let { paint ->
                        val cellW = paint.measureText("M")
                        val fm = paint.fontMetrics
                        val cellH = fm.bottom - fm.top
                        Pair(cellW, cellH)
                    }
                }

                val cols = (widthPx / cellDims.first).toInt().coerceAtLeast(10)
                val rows = (heightPx / cellDims.second).toInt().coerceAtLeast(5)

                LaunchedEffect(cols, rows, connected) {
                    if (connected) session?.resize(cols.toUInt(), rows.toUInt())
                }

                SshTerminalCanvas(
                    update = update,
                    modifier = Modifier.fillMaxSize()
                )
            }
        } else {
            Box(
                modifier = Modifier
                    .weight(1f)
                    .fillMaxWidth()
                    .background(Color.Black)
            ) {
                Text(
                    "「接続」をタップすると SSH セッションが始まります",
                    color = Color.DarkGray,
                    fontSize = 12.sp,
                    modifier = Modifier.padding(16.dp)
                )
            }
        }

        // 入力エリア（キーボードの上に表示される）
        if (connected) {
            Column(
                modifier = Modifier
                    .fillMaxWidth()
                    .background(Color(0xFF111111))
                    .padding(horizontal = 8.dp, vertical = 4.dp),
                verticalArrangement = Arrangement.spacedBy(4.dp),
            ) {
                // Ctrl キー行
                Row(horizontalArrangement = Arrangement.spacedBy(6.dp)) {
                    CtrlKey("Tab") { session?.send(byteArrayOf(0x09)) }
                    CtrlKey("Esc") { session?.send(byteArrayOf(0x1B)) }
                    CtrlKey("^C") { session?.send(byteArrayOf(0x03)) }
                    CtrlKey("^D") { session?.send(byteArrayOf(0x04)) }
                    CtrlKey("^Z") { session?.send(byteArrayOf(0x1A)) }
                    CtrlKey("↑") { session?.send(byteArrayOf(0x1B, 0x5B, 0x41)) }
                    CtrlKey("↓") { session?.send(byteArrayOf(0x1B, 0x5B, 0x42)) }
                }
                OutlinedTextField(
                    value = textFieldValue,
                    onValueChange = { new ->
                        val prev = textFieldValue
                        when {
                            new.composition != null -> {
                                textFieldValue = new
                            }
                            prev.composition != null -> {
                                if (new.text.isNotEmpty()) {
                                    session?.send(new.text.toByteArray(Charsets.UTF_8))
                                }
                                textFieldValue = TextFieldValue("")
                            }
                            new.text.length > prev.text.length -> {
                                val added = new.text.substring(prev.text.length)
                                session?.send(added.toByteArray(Charsets.UTF_8))
                                textFieldValue = TextFieldValue("")
                            }
                            new.text.isEmpty() && prev.text.isEmpty() -> {
                                session?.send(byteArrayOf(0x7F))
                            }
                            else -> textFieldValue = new
                        }
                    },
                    modifier = Modifier.fillMaxWidth(),
                    singleLine = true,
                    placeholder = {
                        Text("入力（ASCII は即送信 / 日本語は変換確定で送信）",
                            fontSize = 11.sp, color = Color(0xFF444444))
                    },
                    keyboardOptions = KeyboardOptions(imeAction = ImeAction.Send),
                    keyboardActions = KeyboardActions(
                        onSend = { session?.send(byteArrayOf(0x0D)) },
                    ),
                    colors = OutlinedTextFieldDefaults.colors(
                        focusedTextColor = Color.White,
                        unfocusedTextColor = Color.White,
                        focusedBorderColor = Color(0xFF4444AA),
                        unfocusedBorderColor = Color(0xFF333333),
                        focusedContainerColor = Color(0xFF1A1A1A),
                        unfocusedContainerColor = Color(0xFF1A1A1A),
                        cursorColor = Color.Cyan,
                    ),
                )
            }
        }
    }
}

@Composable
private fun CtrlKey(label: String, onClick: () -> Unit) {
    TextButton(
        onClick = onClick,
        contentPadding = PaddingValues(horizontal = 6.dp, vertical = 2.dp),
        modifier = Modifier.background(Color(0xFF2A2A2A), shape = MaterialTheme.shapes.small)
    ) {
        Text(label, color = Color(0xFFCCCCCC), fontSize = 11.sp)
    }
}

@Composable
fun SshTerminalCanvas(update: ScreenUpdate, modifier: Modifier = Modifier) {
    val density = LocalDensity.current

    val textPaint = remember {
        Paint().apply {
            isAntiAlias = true
            typeface = Typeface.MONOSPACE
        }
    }
    val bgPaint = remember { Paint() }

    Canvas(modifier = modifier.background(Color.Black)) {
        val cols = update.cols.toInt()
        val rows = update.rows.toInt()

        val cellW = size.width / cols
        val cellH = size.height / rows

        // フォントサイズをセル幅に収まるよう実測で調整
        // まず cellH ベースで設定し、M の実測幅が cellW を超えたら縮小
        textPaint.textSize = cellH * 0.75f
        val mWidth = textPaint.measureText("M")
        if (mWidth > cellW) {
            textPaint.textSize *= cellW / mWidth
        }

        // ベースライン計算
        val fm = textPaint.fontMetrics
        val baseline = -fm.top

        val nCanvas = drawContext.canvas.nativeCanvas

        for (row in 0 until rows) {
            val y = row * cellH
            for (col in 0 until cols) {
                val x = col * cellW
                val cell = update.cells[row * cols + col]
                val bg = cell.bg.toInt()
                val fg = cell.fg.toInt()

                // 背景（デフォルト黒以外のみ描画）
                if (bg != android.graphics.Color.BLACK) {
                    bgPaint.color = bg
                    nCanvas.drawRect(x, y, x + cellW, y + cellH, bgPaint)
                }

                // 文字
                if (cell.ch.isNotBlank()) {
                    textPaint.color = fg
                    textPaint.isFakeBoldText = cell.bold
                    nCanvas.drawText(cell.ch, x, y + baseline, textPaint)
                }
            }
        }

        // カーソル
        val cx = update.cursorCol.toInt() * cellW
        val cy = update.cursorRow.toInt() * cellH
        if (cx < size.width && cy < size.height) {
            bgPaint.color = Color.White.copy(alpha = 0.7f).toArgb()
            nCanvas.drawRect(cx, cy, cx + cellW, cy + cellH, bgPaint)
        }
    }
}
