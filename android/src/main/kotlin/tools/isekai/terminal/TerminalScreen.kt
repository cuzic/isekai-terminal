package tools.isekai.terminal

import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.graphics.Paint as AndroidPaint
import android.net.Uri
import android.view.inputmethod.InputMethodManager
import androidx.activity.compose.BackHandler
import androidx.compose.animation.AnimatedVisibility
import androidx.compose.animation.fadeIn
import androidx.compose.animation.fadeOut
import androidx.compose.animation.slideInVertically
import androidx.compose.animation.slideOutVertically
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
import tools.isekai.terminal.ui.TerminalFontSettings
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
    /** #14: 「今すぐWiFiに戻す」。マルチパス以外のセッションでは呼んでもRust側で無視される。 */
    val onForceReturnToWifi: () -> Unit = {},
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
    chromeVisible: Boolean = true,
    onUserActivity: () -> Unit = {},
) {
    val context = LocalContext.current
    val connected = uiState.connected

    // 未接続/切断中は再接続ボタンを隠したままにできないため、上部バーを強制的に再表示する。
    LaunchedEffect(connected, isActive) {
        if (isActive && !connected) onUserActivity()
    }
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
        // ステータスバー(ホスト名代わりの statusMsg・再接続/切断・ログ・戻る)。
        // 普段は画面いっぱいにターミナルを見せたいという要望から、ドラッグ操作時だけ表示する。
        AnimatedVisibility(
            visible = chromeVisible,
            enter = fadeIn() + slideInVertically(initialOffsetY = { -it }),
            exit = fadeOut() + slideOutVertically(targetOffsetY = { -it }),
        ) {
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
                    // #14: セルラーへフェイルオーバー中/WiFi復帰の静けさ待ち中だけ表示する。
                    // 表示可否の判定はRebindPublicState(Rust側が発火するcallback経由)だけを
                    // 見て行い、Kotlin側で推測状態は持たない(rust-ssot.md準拠)。
                    if (connected && uiState.rebindState != null && uiState.rebindState != RebindPublicState.ON_WIFI) {
                        TextButton(
                            onClick = { actions.onForceReturnToWifi() },
                            contentPadding = PaddingValues(0.dp),
                        ) { Text("今すぐWiFiに戻す", color = Color.Cyan, fontSize = 11.sp) }
                    }
                    TextButton(
                        onClick = { actions.onBack() },
                        contentPadding = PaddingValues(0.dp),
                    ) { Text("戻る", color = Color.Gray, fontSize = 11.sp) }
                }
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
        // カスタムフォント([TerminalFontSettings])もテーマと同じくグローバル設定として
        // ProfileListScreen 側で選択され、ここでは画面表示のたびに読み直すだけでよい。
        // 未選択、または壊れたフォントファイルの場合は既定の Typeface.MONOSPACE のまま
        // 動作する(TerminalFontSettings.loadTypeface 内でフォールバック済み)。
        val terminalTypeface = remember { TerminalFontSettings.loadTypeface(context, prefs) }

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

                val cellDims = remember(density, fontScale, terminalTypeface) {
                    AndroidPaint().apply {
                        typeface = terminalTypeface
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

                // pointerInput の key に cellDims/cols/rows を直接使うと、ピンチで
                // fontScale が変わる → cellDims/cols/rows が再計算される → key が変わり
                // ジェスチャー検出コルーチン自体がキャンセル&再起動される、という
                // 自己中断ループになってしまう(実機検証で確認: ピンチがほぼ常に
                // 完了前に中断されていた不具合の原因)。rememberUpdatedState 経由で
                // 最新値を読むことで、値が変わってもコルーチンを再起動せずに済ませる。
                val latestCellDims = rememberUpdatedState(cellDims)
                val latestCols = rememberUpdatedState(cols)
                val latestRows = rememberUpdatedState(rows)

                Box(modifier = Modifier.fillMaxSize()) {
                    SshTerminalCanvas(
                        update = displayUpdate,
                        selection = selection,
                        theme = terminalTheme,
                        typeface = terminalTypeface,
                        modifier = Modifier
                            .fillMaxSize()
                            .pointerInput(Unit) {
                                awaitEachGesture {
                                    // ジェスチャー開始時点の最新値を1回だけ読む
                                    // (ジェスチャー中は安定した値のまま扱えばよく、
                                    // ピンチ自体がこれらを変化させても再起動はされない)。
                                    val cellW = latestCellDims.value.first
                                    val cellH = latestCellDims.value.second
                                    val cols = latestCols.value
                                    val rows = latestRows.value
                                    val down = awaitFirstDown(requireUnconsumed = false)
                                    val longPress = awaitLongPressOrCancellation(down.id)
                                    // awaitLongPressOrCancellation は「指定した1本の指」の移動/リリースしか
                                    // 見ておらず、2本指が同時に押され続けている(=ピンチ操作中)場合でも
                                    // 長押しタイムアウト(既定 ~400ms)で非nullを返してしまう(実機ログで確認
                                    // 済み: 自然なピンチはほぼ確実にこの時間を超える)。そのため、ここで
                                    // 実際に押されている指の本数を見て、2本以上ならピンチ/パン優先で扱う
                                    // (単一指の本物の長押しだけを選択モードにする)。
                                    val pointerCount = currentEvent.changes.count { it.pressed }
                                    if (longPress != null && pointerCount < 2) {
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
                                        if (pointerCount < 2 && (stillDown == null || !stillDown.pressed)) {
                                            // (3) 単純タップ（長押し不成立かつ移動なしで指が離れた）→
                                            // 画面分割時はこのペインへフォーカスを切り替え、その上でIMEフォーカスを要求する
                                            // (このペインがまだフォーカス外の場合、入力欄自体がこの呼び出し時点では
                                            // 未生成[inputViewがnull]のため即時には効かないが、onRequestFocus()による
                                            // 再コンポジションでinputViewが生成された時点でその生成側のAndroidView.update
                                            // が改めてrequestFocus+showSoftInputを行う)。
                                            actions.onRequestFocus()
                                            requestImeFocus()
                                        } else {
                                            // (2) 2本指以上、または長押し不成立で移動 →
                                            // ピンチ拡縮+縦パンスクロール
                                            while (true) {
                                                val event = awaitPointerEvent()
                                                val zoom = event.calculateZoom()
                                                val pan = event.calculatePan()
                                                if (zoom != 1f || pan != Offset.Zero) {
                                                    // 上部バーを表示するトリガー(ドラッグ操作)。継続的にドラッグしている間は
                                                    // 呼ばれ続けるので、離れた後の自動非表示タイマーもその都度リセットされる。
                                                    onUserActivity()
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
                // 接続直後に1回だけソフトキーボードを自動表示するためのフラグ。この
                // ブロック全体は connected が false になると composition から外れて
                // remember 状態ごと破棄されるため、再接続のたびに自然に false へ戻る。
                // (以前は AndroidView の update ラムダで毎回無条件に showSoftInput() して
                // いたため、ターミナル出力がある(=screenUpdate が変わる)たびに再表示され、
                // ユーザーが手動で隠しても数百ms後には勝手に出てくる不具合になっていた。)
                var imeAutoShown by remember { mutableStateOf(false) }

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

                // F1〜F12 行（横スクロール、Ctrl キー行を圧迫しないよう別行にする）
                Row(
                    horizontalArrangement = Arrangement.spacedBy(6.dp),
                    modifier = Modifier.horizontalScroll(rememberScrollState()),
                ) {
                    CtrlBtn("F1") { actions.onSend(TerminalKeyEncoder.specialKeyBytes(TerminalKeyEncoder.KC_F1)!!) }
                    CtrlBtn("F2") { actions.onSend(TerminalKeyEncoder.specialKeyBytes(TerminalKeyEncoder.KC_F2)!!) }
                    CtrlBtn("F3") { actions.onSend(TerminalKeyEncoder.specialKeyBytes(TerminalKeyEncoder.KC_F3)!!) }
                    CtrlBtn("F4") { actions.onSend(TerminalKeyEncoder.specialKeyBytes(TerminalKeyEncoder.KC_F4)!!) }
                    CtrlBtn("F5") { actions.onSend(TerminalKeyEncoder.specialKeyBytes(TerminalKeyEncoder.KC_F5)!!) }
                    CtrlBtn("F6") { actions.onSend(TerminalKeyEncoder.specialKeyBytes(TerminalKeyEncoder.KC_F6)!!) }
                    CtrlBtn("F7") { actions.onSend(TerminalKeyEncoder.specialKeyBytes(TerminalKeyEncoder.KC_F7)!!) }
                    CtrlBtn("F8") { actions.onSend(TerminalKeyEncoder.specialKeyBytes(TerminalKeyEncoder.KC_F8)!!) }
                    CtrlBtn("F9") { actions.onSend(TerminalKeyEncoder.specialKeyBytes(TerminalKeyEncoder.KC_F9)!!) }
                    CtrlBtn("F10") { actions.onSend(TerminalKeyEncoder.specialKeyBytes(TerminalKeyEncoder.KC_F10)!!) }
                    CtrlBtn("F11") { actions.onSend(TerminalKeyEncoder.specialKeyBytes(TerminalKeyEncoder.KC_F11)!!) }
                    CtrlBtn("F12") { actions.onSend(TerminalKeyEncoder.specialKeyBytes(TerminalKeyEncoder.KC_F12)!!) }
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
                    // ただし update 自体は screenUpdate 等が変わるたびに何度も呼ばれるため、
                    // 自動表示は imeAutoShown で1回だけに制限する（手動で隠した後に
                    // 勝手に再表示されないようにするため）。
                    update = { view ->
                        view.applicationCursorMode = screenUpdate?.applicationCursorMode ?: false
                        view.bracketedPasteMode = screenUpdate?.bracketedPasteMode ?: false
                        view.ctrlArmed = ctrlArmed
                        view.onCtrlConsumed = { ctrlArmed = false }
                        if (connected && !imeAutoShown) {
                            imeAutoShown = true
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
