package tools.isekai.terminal

import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.graphics.Paint as AndroidPaint
import android.graphics.Typeface
import android.net.Uri
import android.view.inputmethod.InputMethodManager
import androidx.activity.compose.BackHandler
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.gestures.awaitEachGesture
import androidx.compose.foundation.gestures.awaitFirstDown
import androidx.compose.foundation.gestures.awaitLongPressOrCancellation
import androidx.compose.foundation.gestures.calculatePan
import androidx.compose.foundation.gestures.calculateZoom
// detectTransformGestures は awaitEachGesture ベースの手動実装に置き換えたため未使用
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.layout.*
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.compose.ui.viewinterop.AndroidView
import tools.isekai.terminal.data.Snippet
import tools.isekai.terminal.input.TerminalInputView
import tools.isekai.terminal.input.TerminalKeyEncoder
import tools.isekai.terminal.ui.AgentSignConfirmDialog
import tools.isekai.terminal.ui.AppColors
import tools.isekai.terminal.ui.HostKeyChangedDialog
import tools.isekai.terminal.ui.HostKeyUnknownDialog
import tools.isekai.terminal.ui.SelectionRange
import tools.isekai.terminal.ui.SshTerminalCanvas
import tools.isekai.terminal.ui.TerminalThemes
import tools.isekai.terminal.ui.offsetToCellPos
import tools.isekai.terminal.ui.reconstructSelectionText
import tools.isekai.terminal.util.RemoteLogger
import uniffi.isekai_terminal_core.*

/**
 * [TerminalScreenBody] が呼び出し元 (単一セッションの [TerminalScreen] か、複数タブの
 * `TerminalTabScreen` か) の違いを気にせず済むようにするための操作の束。
 *
 * すべての操作は最終的に [tools.isekai.terminal.session.TerminalSession] への薄い委譲。
 */
data class TerminalScreenActions(
    val onConnect: () -> Unit,
    val onDisconnect: () -> Unit,
    val onBack: () -> Unit,
    val onSend: (ByteArray) -> Unit,
    val onResize: (UInt, UInt) -> Unit,
    val onScrollbackCells: (Int, Int) -> List<CellData>?,
    val onTrustUpdatedHostKey: () -> Unit,
    val onDismissHostKeyWarning: () -> Unit,
    val onTrustNewHostKey: () -> Unit,
    val onDismissNewHostKeyPrompt: () -> Unit,
    val onTrzszStartUpload: (Uri) -> Unit,
    val onTrzszStartDownload: () -> Unit,
    val onTrzszCancel: () -> Unit,
    val onTrzszDismiss: () -> Unit,
    val onGetSessionLog: () -> String,
    val onSendSnippet: (Snippet) -> Unit,
    val onRespondAgentSignRequest: (Boolean) -> Unit,
    /** 画面分割(split pane)でこのペインがタップされた時に呼ぶ。フォーカスをこのペインへ
     *  切り替える(タブ横断の`TerminalTabsViewModel.setFocusedPane`への委譲)。分割していない
     *  単一ペインの場合は no-op のままでよい。 */
    val onRequestFocus: () -> Unit = {},
)

/**
 * ターミナル画面の本体。複数タブ UI の `TerminalTabScreen`、および画面分割(split pane)時の
 * 各ペイン(`TerminalPaneScreen`)から共有される。
 *
 * [isActive] が false の間は Canvas 描画・IME 入力欄を止める（Rust セッション自体は
 * 生きたまま）。スクロール位置・フォントスケール等のローカル状態は呼び出し側で
 * `key(tabId)`／`key(paneId)` により分離すること。
 *
 * [isActive] と [hasFocus] は別軸: 画面分割で両ペインが同時に見えている間は両方とも
 * [isActive] = true（Canvas・ステータスバーはどちらも描画する）だが、ソフトキーボード入力欄・
 * Ctrl キー行・trzsz転送シート・host key確認ダイアログ・定型コマンド一覧といった
 * 「タブ/ペインを跨いで1つしか存在しない」UIは [hasFocus] が true の側にだけ表示する
 * （「フォーカス中のペインに対して表示する」設計。未分割時は既定で isActive と同じ値になり、
 * 既存の挙動と変わらない）。
 */
@Composable
fun TerminalScreenBody(
    uiState: TerminalUiState,
    canReconnect: Boolean,
    actions: TerminalScreenActions,
    snippets: List<Snippet> = emptyList(),
    isActive: Boolean = true,
    hasFocus: Boolean = isActive,
) {
    val context = LocalContext.current
    val connected = uiState.connected
    val statusMsg = uiState.statusMsg
    val screenUpdate = uiState.screenUpdate
    val scrollbackLen = uiState.scrollbackLen
    // スクロール位置・選択範囲は Compose local state — ViewModel を経由しない
    // (.claude/rules/rust-ssot.md の「UI 表示だけに閉じた状態」の例外)
    var scrollOffset by remember { mutableIntStateOf(0) }
    var showDisconnectDialog by remember { mutableStateOf(false) }
    var selection by remember { mutableStateOf<SelectionRange?>(null) }
    var showSnippetSheet by remember { mutableStateOf(false) }
    // Canvas のタップジェスチャーから IME フォーカスを要求するために、
    // 入力用 AndroidView への参照をここで保持する（入力欄自体は下部に描画）。
    var inputView by remember { mutableStateOf<tools.isekai.terminal.input.TerminalInputView?>(null) }

    BackHandler(enabled = connected && isActive && hasFocus) { showDisconnectDialog = true }

    if (showDisconnectDialog) {
        AlertDialog(
            onDismissRequest = { showDisconnectDialog = false },
            title = { Text("切断しますか？") },
            confirmButton = {
                TextButton(onClick = { actions.onDisconnect(); showDisconnectDialog = false; actions.onBack() }) {
                    Text("切断")
                }
            },
            dismissButton = {
                TextButton(onClick = { showDisconnectDialog = false }) { Text("キャンセル") }
            },
        )
    }

    // Host key changed warning dialog（非アクティブ、またはフォーカス外のペインでは表示しない）
    if (isActive && hasFocus) {
        uiState.hostKeyChangedWarning?.let { w ->
            HostKeyChangedDialog(
                warning = w,
                onAccept = { actions.onTrustUpdatedHostKey() },
                onReject = { actions.onDismissHostKeyWarning() },
            )
        }
    }

    // 初回接続(Unknown host key)確認ダイアログ（非アクティブ、またはフォーカス外のペインでは表示しない）
    if (isActive && hasFocus) {
        uiState.newHostKeyPrompt?.let { prompt ->
            HostKeyUnknownDialog(
                host = prompt.host,
                port = prompt.port,
                fingerprint = prompt.fingerprint,
                onAccept = { actions.onTrustNewHostKey() },
                onReject = { actions.onDismissNewHostKeyPrompt() },
            )
        }
    }

    // SSH agent forwarding: 署名要求ごとの確認ダイアログ
    uiState.agentSignRequestFingerprint?.let { fingerprint ->
        AgentSignConfirmDialog(
            fingerprint = fingerprint,
            onApprove = { actions.onRespondAgentSignRequest(true) },
            onReject = { actions.onRespondAgentSignRequest(false) },
        )
    }

    // trzsz file transfer
    val trzszState = uiState.trzszState
    val transferActive = trzszState is TrzszUiState.WaitingUser || trzszState is TrzszUiState.InProgress
    if (isActive && hasFocus && trzszState != null) {
        TrzszTransferSheet(
            state = trzszState,
            onStartUpload = { uri -> actions.onTrzszStartUpload(uri) },
            onStartDownload = { actions.onTrzszStartDownload() },
            onCancel = { actions.onTrzszCancel() },
            onDismiss = { actions.onTrzszDismiss() },
        )
    }

    // 定型コマンド（スニペット）一覧
    if (isActive && hasFocus && showSnippetSheet) {
        SnippetPickerSheet(
            snippets = snippets,
            onPick = { snippet ->
                actions.onSendSnippet(snippet)
                showSnippetSheet = false
            },
            onDismiss = { showSnippetSheet = false },
        )
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
                            if (canReconnect) actions.onConnect()
                        },
                        contentPadding = PaddingValues(0.dp),
                    ) { Text("再接続", color = Color.Cyan, fontSize = 11.sp) }
                } else {
                    TextButton(
                        onClick = { actions.onDisconnect() },
                        contentPadding = PaddingValues(0.dp),
                    ) { Text("切断", color = Color.Gray, fontSize = 11.sp) }
                }
                if (connected) {
                    TextButton(
                        onClick = {
                            val log = actions.onGetSessionLog()
                            val cm = context.getSystemService(Context.CLIPBOARD_SERVICE) as ClipboardManager
                            cm.setPrimaryClip(ClipData.newPlainText("isekai-terminal log", log))
                        },
                        contentPadding = PaddingValues(0.dp),
                    ) { Text("ログ", color = AppColors.SecondaryText, fontSize = 11.sp) }
                }
                TextButton(
                    onClick = { actions.onBack() },
                    contentPadding = PaddingValues(0.dp),
                ) { Text("戻る", color = Color.Gray, fontSize = 11.sp) }
            }
        }

        // ターミナルキャンバス — font scale / 配色テーマは SharedPreferences 経由で永続化
        val prefs = remember { context.getSharedPreferences("isekai_terminal_ui", android.content.Context.MODE_PRIVATE) }
        var fontScale by remember { mutableStateOf(prefs.getFloat("font_scale", 1f)) }
        val saveFontScale: (Float) -> Unit = remember {
            { scale -> prefs.edit().putFloat("font_scale", scale).apply() }
        }
        // 配色テーマの選択自体は ProfileListScreen 側で行う（グローバル設定）。
        // ここでは画面表示のたびに最新の永続化値を読み直すだけでよい。
        val terminalTheme = remember {
            TerminalThemes.byName(prefs.getString(TerminalThemes.PREF_KEY, null))
        }

        val update = screenUpdate
        if (isActive && update != null) {
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

                // タブがアクティブ化された直後にも、このタブの実際のビューポート寸法で
                // 確実に resize() が送られる（cols/rows は非アクティブ中は計算されないため）。
                LaunchedEffect(cols, rows, connected) {
                    if (connected) actions.onResize(cols.toUInt(), rows.toUInt())
                }

                // When scrolled into scrollback, synthesize a ScreenUpdate from the buffer
                val displayUpdate = remember(scrollOffset, rows, update) {
                    if (scrollOffset > 0) {
                        val sbCells = actions.onScrollbackCells(scrollOffset, rows)
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

                // IME フォーカス要求（単純タップ用）。AndroidView 生成前は no-op。
                val requestImeFocus: () -> Unit = {
                    inputView?.let { view ->
                        view.post {
                            view.requestFocus()
                            view.context.getSystemService(InputMethodManager::class.java)
                                ?.showSoftInput(view, 0)
                        }
                    }
                }

                Box(modifier = Modifier.fillMaxSize()) {
                    SshTerminalCanvas(
                        update = displayUpdate,
                        selection = selection,
                        theme = terminalTheme,
                        modifier = Modifier
                            .fillMaxSize()
                            .pointerInput(cellDims, cols, rows) {
                                val cellW = cellDims.first
                                val cellH = cellDims.second
                                awaitEachGesture {
                                    val down = awaitFirstDown(requireUnconsumed = false)
                                    val longPress = awaitLongPressOrCancellation(down.id)
                                    if (longPress != null) {
                                        // (1) 長押し成立 → 選択モード。選択中はスクロールに触れない
                                        // (= スクロール位置ロック)。以降のドラッグで head を更新する。
                                        val startCell = offsetToCellPos(
                                            longPress.position.x, longPress.position.y,
                                            cellW, cellH, cols, rows,
                                        )
                                        selection = SelectionRange(startCell, startCell)
                                        while (true) {
                                            val event = awaitPointerEvent()
                                            val change = event.changes.firstOrNull { it.id == down.id } ?: break
                                            if (!change.pressed) {
                                                change.consume()
                                                break
                                            }
                                            change.consume()
                                            val cell = offsetToCellPos(
                                                change.position.x, change.position.y,
                                                cellW, cellH, cols, rows,
                                            )
                                            selection = selection?.copy(head = cell)
                                        }
                                    } else {
                                        val stillDown = currentEvent.changes.firstOrNull { it.id == down.id }
                                        if (stillDown == null || !stillDown.pressed) {
                                            // (3) 単純タップ（長押し不成立かつ移動なしで指が離れた）→
                                            // 画面分割時はこのペインへフォーカスを切り替え、その上でIMEフォーカスを要求する
                                            // (このペインがまだフォーカス外の場合、入力欄自体がこの呼び出し時点では
                                            // 未生成[inputViewがnull]のため即時には効かないが、onRequestFocus()による
                                            // 再コンポジションでinputViewが生成された時点でその生成側のAndroidView.update
                                            // が改めてrequestFocus+showSoftInputを行う)。
                                            actions.onRequestFocus()
                                            requestImeFocus()
                                        } else {
                                            // (2) 長押し不成立で移動 → 従来のピンチ拡縮+縦パンスクロール相当
                                            while (true) {
                                                val event = awaitPointerEvent()
                                                val zoom = event.calculateZoom()
                                                val pan = event.calculatePan()
                                                if (zoom != 1f || pan != Offset.Zero) {
                                                    val newScale = (fontScale * zoom).coerceIn(0.5f, 3.0f)
                                                    if (newScale != fontScale) { fontScale = newScale; saveFontScale(newScale) }
                                                    panAccumY += pan.y
                                                    while (panAccumY < -cellH) {
                                                        scrollOffset = (scrollOffset + 1).coerceIn(0, scrollbackLen)
                                                        panAccumY += cellH
                                                    }
                                                    while (panAccumY > cellH) {
                                                        scrollOffset = (scrollOffset - 1).coerceIn(0, scrollbackLen)
                                                        panAccumY -= cellH
                                                    }
                                                    event.changes.forEach { it.consume() }
                                                }
                                                if (event.changes.all { !it.pressed }) break
                                            }
                                        }
                                    }
                                }
                            },
                    )

                    // 選択中のフローティングツールバー（コピー／キャンセル）
                    selection?.let { sel ->
                        Row(
                            modifier = Modifier
                                .align(Alignment.TopCenter)
                                .padding(top = 8.dp)
                                .background(Color(0xCC1A1A2E), shape = MaterialTheme.shapes.small)
                                .padding(horizontal = 8.dp, vertical = 4.dp),
                            horizontalArrangement = Arrangement.spacedBy(4.dp),
                        ) {
                            TextButton(onClick = {
                                val text = reconstructSelectionText(displayUpdate, sel)
                                if (text.isNotEmpty()) {
                                    val cm = context.getSystemService(Context.CLIPBOARD_SERVICE) as ClipboardManager
                                    cm.setPrimaryClip(ClipData.newPlainText("isekai-terminal selection", text))
                                }
                                selection = null
                            }) { Text("コピー", color = Color.Cyan, fontSize = 12.sp) }
                            TextButton(onClick = { selection = null }) {
                                Text("キャンセル", color = Color.Gray, fontSize = 12.sp)
                            }
                        }
                    }

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
        } else if (isActive) {
            Box(
                modifier = Modifier
                    .weight(1f)
                    .fillMaxWidth()
                    .background(terminalTheme.background),
            ) {
                Text(
                    statusMsg,
                    color = Color.DarkGray,
                    fontSize = 12.sp,
                    modifier = Modifier.padding(16.dp),
                )
            }
        } else {
            // 非アクティブタブ: Canvas 描画をスキップ（Rust セッションは生かしたまま）。
            // 幅0のプレースホルダにすることで上の remember 状態 (scrollOffset 等) は保持しつつ、
            // 描画コストとキーボードのポップアップだけを避ける。
            Box(modifier = Modifier.weight(1f).fillMaxWidth())
        }

        // 入力エリア（キーボードの上に表示される）。転送中はキー入力が
        // trzsz バイナリストリームに混入するのを防ぐため無効化する。非アクティブタブ・
        // フォーカス外のペインではソフトキーボードを誤って呼び出さないよう表示しない。
        if (isActive && hasFocus && connected && !transferActive) {
            Column(
                modifier = Modifier
                    .fillMaxWidth()
                    .background(Color(0xFF111111))
                    .padding(horizontal = 8.dp, vertical = 4.dp),
                verticalArrangement = Arrangement.spacedBy(4.dp),
            ) {
                // InputConnection ベースの入力経路（state は Row より前に宣言）
                // inputView 自体は Canvas のタップジェスチャーからも参照するため画面トップレベルで保持している。
                var composingText by remember { mutableStateOf("") }
                // トグル式 Ctrl キーの武装状態。UI 表示に閉じたローカル状態（rust-ssot.md の例外）。
                var ctrlArmed by remember { mutableStateOf(false) }

                // Ctrl キー行
                Row(
                    horizontalArrangement = Arrangement.spacedBy(6.dp),
                    modifier = Modifier.horizontalScroll(rememberScrollState()),
                ) {
                    CtrlBtn("Ctrl", active = ctrlArmed) { ctrlArmed = !ctrlArmed }
                    CtrlBtn("↵") { inputView?.commitComposing(); actions.onSend(byteArrayOf(0x0D)) }
                    CtrlBtn("Tab") { actions.onSend(byteArrayOf(0x09)) }
                    CtrlBtn("Esc") { actions.onSend(byteArrayOf(0x1B)) }
                    CtrlBtn("^C") { actions.onSend(byteArrayOf(0x03)) }
                    CtrlBtn("^D") { actions.onSend(byteArrayOf(0x04)) }
                    CtrlBtn("^Z") { actions.onSend(byteArrayOf(0x1A)) }
                    CtrlBtn("↑") { actions.onSend(byteArrayOf(0x1B, 0x5B, 0x41)) }
                    CtrlBtn("↓") { actions.onSend(byteArrayOf(0x1B, 0x5B, 0x42)) }
                    CtrlBtn("←") { actions.onSend(byteArrayOf(0x1B, 0x5B, 0x44)) }
                    CtrlBtn("→") { actions.onSend(byteArrayOf(0x1B, 0x5B, 0x43)) }
                    CtrlBtn("貼付") {
                        val cm = context.getSystemService(Context.CLIPBOARD_SERVICE) as ClipboardManager
                        val text = cm.primaryClip?.takeIf { it.itemCount > 0 }
                            ?.getItemAt(0)?.coerceToText(context)?.toString()
                        if (!text.isNullOrEmpty()) {
                            actions.onSend(TerminalKeyEncoder.commitTextBytes(text, screenUpdate?.bracketedPasteMode ?: false))
                        }
                    }
                    CtrlBtn("定型") { showSnippetSheet = true }
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
                            onSendBytes = { bytes -> actions.onSend(bytes) }
                            onComposingText = { text -> composingText = text }
                        }.also { inputView = it }
                    },
                    // update は view 生成後・recomposition のたびに呼ばれる。
                    // connected が true になった直後に呼ばれるので LaunchedEffect より確実。
                    update = { view ->
                        view.applicationCursorMode = screenUpdate?.applicationCursorMode ?: false
                        view.bracketedPasteMode = screenUpdate?.bracketedPasteMode ?: false
                        view.ctrlArmed = ctrlArmed
                        view.onCtrlConsumed = { ctrlArmed = false }
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
private fun CtrlBtn(label: String, active: Boolean = false, onClick: () -> Unit) {
    TextButton(
        onClick = onClick,
        contentPadding = PaddingValues(horizontal = 6.dp, vertical = 2.dp),
        modifier = Modifier.background(
            if (active) Color(0xFF4A6A4A) else Color(0xFF2A2A2A),
            shape = MaterialTheme.shapes.small,
        ),
    ) {
        Text(label, color = if (active) Color.White else AppColors.MutedText, fontSize = 11.sp)
    }
}

@OptIn(ExperimentalMaterial3Api::class)
@Composable
private fun SnippetPickerSheet(
    snippets: List<Snippet>,
    onPick: (Snippet) -> Unit,
    onDismiss: () -> Unit,
) {
    ModalBottomSheet(onDismissRequest = onDismiss) {
        Column(
            modifier = Modifier
                .fillMaxWidth()
                .padding(horizontal = 16.dp, vertical = 12.dp)
                .navigationBarsPadding(),
            verticalArrangement = Arrangement.spacedBy(4.dp),
        ) {
            Text("定型コマンド", style = MaterialTheme.typography.titleMedium)
            if (snippets.isEmpty()) {
                Text(
                    "登録された定型コマンドがありません。プロファイル一覧の「定型」から追加できます。",
                    color = Color(0xFFAAAAAA),
                    fontSize = 13.sp,
                    modifier = Modifier.padding(vertical = 12.dp),
                )
            } else {
                Column(
                    modifier = Modifier
                        .fillMaxWidth()
                        .heightIn(max = 360.dp)
                        .verticalScroll(rememberScrollState()),
                ) {
                    snippets.forEach { snippet ->
                        Row(
                            modifier = Modifier
                                .fillMaxWidth()
                                .clickable { onPick(snippet) }
                                .padding(vertical = 10.dp),
                            verticalAlignment = Alignment.CenterVertically,
                        ) {
                            Column(modifier = Modifier.weight(1f)) {
                                Text(snippet.label, color = Color.White, fontSize = 15.sp)
                                Text(
                                    snippet.command.lineSequence().firstOrNull() ?: "",
                                    color = Color(0xFF888888),
                                    fontSize = 11.sp,
                                    fontFamily = FontFamily.Monospace,
                                    maxLines = 1,
                                )
                            }
                        }
                    }
                }
            }
        }
    }
}
