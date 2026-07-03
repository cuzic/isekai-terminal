package tools.isekai.terminal

import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.graphics.Paint as AndroidPaint
import android.graphics.Typeface
import android.view.inputmethod.InputMethodManager
import androidx.activity.compose.BackHandler
import androidx.compose.foundation.background
import androidx.compose.foundation.gestures.detectTransformGestures
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.rememberScrollState
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.compose.ui.viewinterop.AndroidView
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import androidx.lifecycle.viewmodel.compose.viewModel
import tools.isekai.terminal.data.ConnectionProfile
import tools.isekai.terminal.input.TerminalInputView
import tools.isekai.terminal.ui.AppColors
import tools.isekai.terminal.ui.HostKeyChangedDialog
import tools.isekai.terminal.ui.SshTerminalCanvas
import tools.isekai.terminal.util.RemoteLogger
import uniffi.tssh_core.*

@Composable
fun TerminalScreen(
    profile: ConnectionProfile? = null,
    password: String? = null,
    onBack: () -> Unit,
    vm: TerminalViewModel = viewModel(),
) {
    val context = LocalContext.current
    val uiState by vm.uiState.collectAsStateWithLifecycle()
    val connected = uiState.connected
    val statusMsg = uiState.statusMsg
    val screenUpdate = uiState.screenUpdate
    val scrollbackLen = uiState.scrollbackLen
    // スクロール位置は Compose local state — ViewModel を経由しない
    var scrollOffset by remember { mutableIntStateOf(0) }
    var showDisconnectDialog by remember { mutableStateOf(false) }

    BackHandler(enabled = connected) { showDisconnectDialog = true }

    if (showDisconnectDialog) {
        AlertDialog(
            onDismissRequest = { showDisconnectDialog = false },
            title = { Text("切断しますか？") },
            confirmButton = {
                TextButton(onClick = { vm.disconnect(); showDisconnectDialog = false; onBack() }) {
                    Text("切断")
                }
            },
            dismissButton = {
                TextButton(onClick = { showDisconnectDialog = false }) { Text("キャンセル") }
            },
        )
    }

    // Host key changed warning dialog
    uiState.hostKeyChangedWarning?.let { w ->
        HostKeyChangedDialog(
            warning = w,
            onAccept = { vm.trustUpdatedHostKey() },
            onReject = { vm.dismissHostKeyWarning() },
        )
    }

    // trzsz file transfer
    val trzszState = uiState.trzszState
    val transferActive = trzszState is TrzszUiState.WaitingUser || trzszState is TrzszUiState.InProgress
    if (trzszState != null) {
        TrzszTransferSheet(
            state = trzszState,
            onStartUpload = { uri -> vm.trzszStartUpload(uri) },
            onStartDownload = { vm.trzszStartDownload() },
            onCancel = { vm.trzszCancel() },
            onDismiss = { vm.trzszDismiss() },
        )
    }

    LaunchedEffect(Unit) {
        if (!connected && profile != null) {
            RemoteLogger.i("TsshSSH", "TerminalScreen: launch connectProfile '${profile.label}' ${profile.username}@${profile.host}:${profile.port}")
            vm.connectProfile(profile, password)
        }
    }

    Column(
        modifier = Modifier
            .fillMaxSize()
            .background(Color.Black)
            .navigationBarsPadding()
            .imePadding(),
    ) {
        // ステータスバー
        Row(
            modifier = Modifier
                .fillMaxWidth()
                .background(AppColors.CardBackground)
                .padding(horizontal = 8.dp, vertical = 4.dp),
            horizontalArrangement = Arrangement.SpaceBetween,
        ) {
            Text(
                statusMsg,
                color = if (connected) AppColors.Success else Color.Yellow,
                fontSize = 11.sp,
            )
            Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                if (!connected) {
                    TextButton(
                        onClick = {
                            if (profile != null) vm.connectProfile(profile, password)
                        },
                        contentPadding = PaddingValues(0.dp),
                    ) { Text("再接続", color = Color.Cyan, fontSize = 11.sp) }
                } else {
                    TextButton(
                        onClick = { vm.disconnect() },
                        contentPadding = PaddingValues(0.dp),
                    ) { Text("切断", color = Color.Gray, fontSize = 11.sp) }
                }
                if (connected) {
                    TextButton(
                        onClick = {
                            val log = vm.getSessionLog()
                            val cm = context.getSystemService(Context.CLIPBOARD_SERVICE) as ClipboardManager
                            cm.setPrimaryClip(ClipData.newPlainText("tssh log", log))
                        },
                        contentPadding = PaddingValues(0.dp),
                    ) { Text("ログ", color = AppColors.SecondaryText, fontSize = 11.sp) }
                }
                TextButton(
                    onClick = { vm.disconnect(); onBack() },
                    contentPadding = PaddingValues(0.dp),
                ) { Text("戻る", color = Color.Gray, fontSize = 11.sp) }
            }
        }

        // ターミナルキャンバス — font scale persisted via SharedPreferences
        val prefs = remember { context.getSharedPreferences("tssh_ui", android.content.Context.MODE_PRIVATE) }
        var fontScale by remember { mutableStateOf(prefs.getFloat("font_scale", 1f)) }
        val saveFontScale: (Float) -> Unit = remember {
            { scale -> prefs.edit().putFloat("font_scale", scale).apply() }
        }

        val update = screenUpdate
        if (update != null) {
            BoxWithConstraints(
                modifier = Modifier
                    .weight(1f)
                    .fillMaxWidth(),
            ) {
                val density = LocalDensity.current
                val widthPx = with(density) { maxWidth.toPx() }
                val heightPx = with(density) { maxHeight.toPx() }

                val cellDims = remember(density, fontScale) {
                    AndroidPaint().apply {
                        typeface = Typeface.MONOSPACE
                        textSize = 14f * density.density * fontScale
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
                    if (connected) vm.resize(cols.toUInt(), rows.toUInt())
                }

                // When scrolled into scrollback, synthesize a ScreenUpdate from the buffer
                val displayUpdate = remember(scrollOffset, rows, update) {
                    if (scrollOffset > 0) {
                        val sbCells = vm.scrollbackCells(scrollOffset, rows)
                        if (sbCells != null && sbCells.size == rows * cols) {
                            ScreenUpdate(
                                cols = update.cols,
                                rows = update.rows,
                                cells = sbCells,
                                cursorRow = update.rows,  // hide cursor (off-screen)
                                cursorCol = 0u,
                                title = update.title,
                                applicationCursorMode = update.applicationCursorMode,
                                bracketedPasteMode = update.bracketedPasteMode,
                            )
                        } else update
                    } else update
                }

                var panAccumY by remember { mutableStateOf(0f) }

                Box(modifier = Modifier.fillMaxSize()) {
                    SshTerminalCanvas(
                        update = displayUpdate,
                        modifier = Modifier
                            .fillMaxSize()
                            .pointerInput(cellDims) {
                                detectTransformGestures { _, pan, zoom, _ ->
                                    val newScale = (fontScale * zoom).coerceIn(0.5f, 3.0f)
                                    if (newScale != fontScale) { fontScale = newScale; saveFontScale(newScale) }
                                    panAccumY += pan.y
                                    val cellH = cellDims.second
                                    while (panAccumY < -cellH) {
                                        scrollOffset = (scrollOffset + 1).coerceIn(0, scrollbackLen)
                                        panAccumY += cellH
                                    }
                                    while (panAccumY > cellH) {
                                        scrollOffset = (scrollOffset - 1).coerceIn(0, scrollbackLen)
                                        panAccumY -= cellH
                                    }
                                }
                            },
                    )

                    // "Back to live" indicator when scrolled up
                    if (scrollOffset > 0) {
                        Button(
                            onClick = { scrollOffset = 0; panAccumY = 0f },
                            modifier = Modifier
                                .align(Alignment.BottomCenter)
                                .padding(bottom = 8.dp),
                            colors = ButtonDefaults.buttonColors(
                                containerColor = Color(0xCC1A1A2E),
                            ),
                        ) {
                            Text(
                                "↓ ライブへ戻る ($scrollOffset / $scrollbackLen)",
                                color = Color.Cyan,
                                fontSize = 11.sp,
                            )
                        }
                    }
                }
            }
        } else {
            Box(
                modifier = Modifier
                    .weight(1f)
                    .fillMaxWidth()
                    .background(Color.Black),
            ) {
                Text(
                    statusMsg,
                    color = Color.DarkGray,
                    fontSize = 12.sp,
                    modifier = Modifier.padding(16.dp),
                )
            }
        }

        // 入力エリア（キーボードの上に表示される）。転送中はキー入力が
        // trzsz バイナリストリームに混入するのを防ぐため無効化する。
        if (connected && !transferActive) {
            Column(
                modifier = Modifier
                    .fillMaxWidth()
                    .background(Color(0xFF111111))
                    .padding(horizontal = 8.dp, vertical = 4.dp),
                verticalArrangement = Arrangement.spacedBy(4.dp),
            ) {
                // InputConnection ベースの入力経路（state は Row より前に宣言）
                var composingText by remember { mutableStateOf("") }
                var inputView by remember { mutableStateOf<tools.isekai.terminal.input.TerminalInputView?>(null) }

                // Ctrl キー行
                Row(
                    horizontalArrangement = Arrangement.spacedBy(6.dp),
                    modifier = Modifier.horizontalScroll(rememberScrollState()),
                ) {
                    CtrlBtn("↵") { inputView?.commitComposing(); vm.send(byteArrayOf(0x0D)) }
                    CtrlBtn("Tab") { vm.send(byteArrayOf(0x09)) }
                    CtrlBtn("Esc") { vm.send(byteArrayOf(0x1B)) }
                    CtrlBtn("^C") { vm.send(byteArrayOf(0x03)) }
                    CtrlBtn("^D") { vm.send(byteArrayOf(0x04)) }
                    CtrlBtn("^Z") { vm.send(byteArrayOf(0x1A)) }
                    CtrlBtn("↑") { vm.send(byteArrayOf(0x1B, 0x5B, 0x41)) }
                    CtrlBtn("↓") { vm.send(byteArrayOf(0x1B, 0x5B, 0x42)) }
                    CtrlBtn("←") { vm.send(byteArrayOf(0x1B, 0x5B, 0x44)) }
                    CtrlBtn("→") { vm.send(byteArrayOf(0x1B, 0x5B, 0x43)) }
                }

                if (composingText.isNotEmpty()) {
                    Text(
                        "変換中: $composingText",
                        color = Color(0xFFFFFF88),
                        fontSize = 11.sp,
                        modifier = Modifier.padding(horizontal = 4.dp),
                    )
                }

                AndroidView(
                    factory = { ctx ->
                        tools.isekai.terminal.input.TerminalInputView(ctx).apply {
                            onSendBytes = { bytes -> vm.send(bytes) }
                            onComposingText = { text -> composingText = text }
                        }.also { inputView = it }
                    },
                    // update は view 生成後・recomposition のたびに呼ばれる。
                    // connected が true になった直後に呼ばれるので LaunchedEffect より確実。
                    update = { view ->
                        view.applicationCursorMode = screenUpdate?.applicationCursorMode ?: false
                        view.bracketedPasteMode = screenUpdate?.bracketedPasteMode ?: false
                        if (connected) {
                            view.post {
                                view.requestFocus()
                                view.context.getSystemService(InputMethodManager::class.java)
                                    ?.showSoftInput(view, 0)
                            }
                        }
                    },
                    modifier = Modifier
                        .fillMaxWidth()
                        .height(1.dp),
                )
            }
        }
    }
}

@Composable
private fun CtrlBtn(label: String, onClick: () -> Unit) {
    TextButton(
        onClick = onClick,
        contentPadding = PaddingValues(horizontal = 6.dp, vertical = 2.dp),
        modifier = Modifier.background(Color(0xFF2A2A2A), shape = MaterialTheme.shapes.small),
    ) {
        Text(label, color = AppColors.MutedText, fontSize = 11.sp)
    }
}
