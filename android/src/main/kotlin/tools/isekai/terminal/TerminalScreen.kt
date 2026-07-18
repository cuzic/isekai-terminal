package tools.isekai.terminal

import android.content.ActivityNotFoundException
import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.content.Intent
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
import androidx.compose.ui.focus.FocusRequester
import androidx.compose.ui.focus.focusRequester
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.input.pointer.AwaitPointerEventScope
import androidx.compose.ui.input.pointer.PointerEventType
import androidx.compose.ui.input.pointer.pointerInput
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.compose.ui.viewinterop.AndroidView
import tools.isekai.terminal.data.KeySequence
import tools.isekai.terminal.data.Snippet
import tools.isekai.terminal.input.KeyStep
import tools.isekai.terminal.input.KeyboardLayoutMode
import tools.isekai.terminal.input.TerminalInputView
import tools.isekai.terminal.input.TerminalKeyEncoder
import tools.isekai.terminal.input.previewText
import tools.isekai.terminal.ui.AgentSignConfirmDialog
import tools.isekai.terminal.ui.AppColors
import tools.isekai.terminal.ui.HostKeyChangedDialog
import tools.isekai.terminal.ui.HostKeyUnknownDialog
import tools.isekai.terminal.ui.HyperlinkConfirmDialog
import tools.isekai.terminal.ui.ResizeStabilityState
import tools.isekai.terminal.ui.SelectionRange
import tools.isekai.terminal.ui.SshTerminalCanvas
import tools.isekai.terminal.ui.TerminalFontSettings
import tools.isekai.terminal.ui.TerminalThemes
import tools.isekai.terminal.ui.NormalGestureOutcome
import tools.isekai.terminal.ui.MouseTouchStep
import tools.isekai.terminal.ui.advanceResizeStability
import tools.isekai.terminal.ui.classifyNormalGesture
import tools.isekai.terminal.ui.computeResizeTargetColsRows
import tools.isekai.terminal.ui.decideMouseTouchStep
import tools.isekai.terminal.ui.isOpenableHyperlinkScheme
import tools.isekai.terminal.ui.isPointerReportingActive as arbiterIsPointerReportingActive
import tools.isekai.terminal.ui.linkUrlAtCell
import tools.isekai.terminal.ui.offsetToCellPos
import tools.isekai.terminal.ui.shouldUseMouseTouch
import tools.isekai.terminal.ui.wheelButtonForDelta
import tools.isekai.terminal.ui.reconstructSelectionText
import tools.isekai.terminal.ui.synthesizeDisplayUpdate
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
    /** 自動再接続ループ(isReconnecting中)を中止する。 */
    val onCancelReconnect: () -> Unit = {},
    val onBack: () -> Unit,
    val onSend: (ByteArray) -> Unit,
    val onResize: (UInt, UInt) -> Unit,
    val onScrollbackCells: (Int, Int) -> List<CellData>?,
    /** タスク#66: スクロールバック検索。マッチ計算は一切Kotlin側で行わず、Rust側
     *  `SessionCore::search_scrollback`(#37)の結果をそのまま返すだけ(rust-ssot)。 */
    val onSearchScrollback: (String, Boolean) -> List<ScrollbackSearchMatch> = { _, _ -> emptyList() },
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
    val onSendKeySequence: (List<KeyStep>) -> Unit = {},
    val onRespondAgentSignRequest: (Boolean) -> Unit,
    /** 画面分割(split pane)でこのペインがタップされた時に呼ぶ。フォーカスをこのペインへ
     *  切り替える(タブ横断の`TerminalTabsViewModel.setFocusedPane`への委譲)。分割していない
     *  単一ペインの場合は no-op のままでよい。 */
    val onRequestFocus: () -> Unit = {},
    val onNextTab: () -> Unit = {},
    val onPreviousTab: () -> Unit = {},
    /** #14: 「今すぐWiFiに戻す」。マルチパス以外のセッションでは呼んでもRust側で無視される。 */
    val onForceReturnToWifi: () -> Unit = {},
    /** #60: このペインの実効フォーカス状態(`isActive && hasFocus`)が変化するたびに
     *  そのまま呼ばれる。フォーカスレポーティング(`CSI ?1004`)が有効かどうかの判断は
     *  Rust側が持つため、ここでは生の値を渡すだけでよい。 */
    val onFocusChanged: (Boolean) -> Unit = {},
)

/**
 * タスク#66: 検索バーの現在マッチ([match])のうち、実際に[scrollOffset]の位置へ
 * ハイライトとして描画してよいものだけを返すピュア関数。
 *
 * `ScrollbackSearchMatch.row`は`scrollbackCells`と同じ規約("offset"がそのまま`row`)なので、
 * `scrollOffset`がその値と一致している間だけ実際に画面へ表示される。`scrollOffset == 0`は
 * 「ライブ画面表示」と「scrollback最新行(row=0)表示」の両方を指しうる(既存規約
 * [synthesizeDisplayUpdate]と[showingScrollback]参照)ため、`row == 0u`のマッチは
 * [showingScrollback]が真の間(=実際にscrollback最新行を表示中)だけハイライトを許可する
 * (タスク#79: それ以外[ライブ画面表示中]にrow=0のマッチを誤ってハイライトしないための
 * ガード。iOS版`TerminalView.swift`の`searchHighlight`計算と対称)。
 */
internal fun searchHighlightMatch(
    match: ScrollbackSearchMatch?,
    scrollOffset: Int,
    showingScrollback: Boolean,
): ScrollbackSearchMatch? =
    match?.takeIf { scrollOffset == it.row.toInt() && (it.row != 0u || showingScrollback) }

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
@OptIn(androidx.compose.foundation.layout.ExperimentalLayoutApi::class)
@Composable
fun TerminalScreenBody(
    uiState: TerminalUiState,
    canReconnect: Boolean,
    actions: TerminalScreenActions,
    snippets: List<Snippet> = emptyList(),
    keySequences: List<KeySequence> = emptyList(),
    installedPacks: List<Pair<tools.isekai.terminal.pack.KeySequencePack, tools.isekai.terminal.data.KeySequencePackInstallation>> = emptyList(),
    isActive: Boolean = true,
    hasFocus: Boolean = isActive,
    chromeVisible: Boolean = true,
    onUserActivity: () -> Unit = {},
) {
    val context = LocalContext.current
    val connected = uiState.connected
    val isReconnecting = uiState.isReconnecting

    // 未接続/切断中(自動再接続中を含む)は再接続/中止ボタンを隠したままにできないため、
    // 上部バーを強制的に再表示する。
    LaunchedEffect(connected, isActive) {
        if (isActive && !connected) onUserActivity()
    }
    // #60: フォーカスレポーティング(`CSI ?1004`)。「タブ/split paneを跨いでこのペインが
    // 今まさに入力を受け付けている状態」(isActive && hasFocus)の変化をそのままRust側へ
    // 転送する(モバイル版の「ターミナルウィンドウのフォーカス」の実効的な定義 —
    // タブ切替・split pane切替のたびに発火する)。有効/無効の判断はRust側が持つ
    // (rust-ssot)ので、ここでは生の可視性/フォーカス状態を渡すだけでよい。
    LaunchedEffect(isActive, hasFocus) {
        actions.onFocusChanged(isActive && hasFocus)
    }
    val statusMsg = uiState.statusMsg
    val screenUpdate = uiState.screenUpdate
    val scrollbackLen = uiState.scrollbackLen
    // スクロール位置・選択範囲は Compose local state — ViewModel を経由しない
    // (.claude/rules/rust-ssot.md の「UI 表示だけに閉じた状態」の例外)
    var scrollOffset by remember { mutableIntStateOf(0) }
    // タスク#79: `scrollOffset == 0`は従来「ライブ画面表示」を意味する唯一の条件として
    // 使われてきたが、これだと検索結果の`row == 0`(scrollbackの最新履歴行)へジャンプする
    // 際、`scrollOffset`を0にしてもライブ表示に横取りされて到達不能になっていた
    // (旧実装は`jumpToCurrentSearchMatch`でこの理由からrow=0へのジャンプ自体を明示的に
    // 諦めていた)。「ユーザーが明示的にscrollback表示へ入っているか」を`scrollOffset`の
    // 値そのものとは独立したフラグとして持つことで、`scrollOffset == 0`のまま
    // scrollback最新行を表示できるようにする(iOS版`TerminalView.swift`の
    // `showingScrollback`と対称。「UI表示だけに閉じた状態」として`scrollOffset`と
    // 同じくrust-ssot.mdの例外)。
    var showingScrollback by remember { mutableStateOf(false) }
    var showDisconnectDialog by remember { mutableStateOf(false) }
    var selection by remember { mutableStateOf<SelectionRange?>(null) }
    var showSnippetSheet by remember { mutableStateOf(false) }
    var showKeySequenceSheet by remember { mutableStateOf(false) }
    // タスク#66: スクロールバック検索バーの開閉・クエリ・大小文字区別・マッチ結果一覧・
    // 「今何件目を見ているか」。いずれも「UI表示だけに閉じた状態」(rust-ssot.mdの例外、
    // scrollOffset/selectionと同じ扱い)であり、マッチの計算自体(部分一致検索・combining
    // character境界・大小文字無視)は一切ここでは行わず、actions.onSearchScrollback経由で
    // Rust側search_scrollback(#37)に全面委譲する。iOS版`TerminalView.swift`の
    // `showSearchBar`等と対称(タスク#67)。
    var showSearchBar by remember { mutableStateOf(false) }
    var searchQuery by remember { mutableStateOf("") }
    var searchCaseSensitive by remember { mutableStateOf(false) }
    var searchMatches by remember { mutableStateOf<List<ScrollbackSearchMatch>>(emptyList()) }
    var currentSearchMatchIndex by remember { mutableIntStateOf(0) }

    /** `searchMatches`を`actions.onSearchScrollback`で最新化する。`ScrollbackSearchMatch.row`は
     *  「呼び出し時点のスナップショットに対してのみ有効」で長期キャッシュしない運用を前提
     *  とするドキュメント([ScrollbackSearchMatch]、Fableレビュー2次指摘(b)、iOS版
     *  `TerminalView.swift`の`refreshMatches()`と対称)のため、ジャンプ操作(次/前)の
     *  たびに必ず再検索してから移動する。空クエリの場合は呼び出し自体を省く。 */
    val refreshSearchMatches: () -> Boolean = refresh@{
        if (searchQuery.isEmpty()) {
            searchMatches = emptyList()
            currentSearchMatchIndex = 0
            return@refresh false
        }
        searchMatches = actions.onSearchScrollback(searchQuery, searchCaseSensitive)
        if (currentSearchMatchIndex !in searchMatches.indices) currentSearchMatchIndex = 0
        searchMatches.isNotEmpty()
    }
    // `scrollOffset`を現在のマッチの`row`へ合わせ、そのマッチが画面に映るようにする。
    //
    // タスク#79: scrollback最新行(`row == 0u`)は、`scrollOffset == 0`が「ライブ画面表示」を
    // 兼ねる既存の規約([synthesizeDisplayUpdate])と衝突するため、`scrollOffset`の値
    // だけでは表示できない(`scrollbackCells`の`sb_idx = offset + (rows-1-r)`は
    // `offset == 0`のとき`sb_idx == 0`[=scrollback最新行]に一致するが、この`offset == 0`は
    // 従来ライブ表示に横取りされていた)。[showingScrollback]を真にすることで
    // `scrollOffset == 0`のままscrollback最新行の合成表示へ切り替える(下の`displayUpdate`
    // 計算・[searchHighlightMatch]・「ライブへ戻る」ボタンの表示条件も同じフラグを見る)。
    val jumpToCurrentSearchMatch: () -> Unit = {
        searchMatches.getOrNull(currentSearchMatchIndex)?.let { match ->
            scrollOffset = match.row.toInt()
            showingScrollback = true
        }
    }
    val runSearch: () -> Unit = {
        currentSearchMatchIndex = 0
        if (refreshSearchMatches()) jumpToCurrentSearchMatch()
    }
    val goToNextSearchMatch: () -> Unit = {
        if (refreshSearchMatches()) {
            currentSearchMatchIndex = (currentSearchMatchIndex + 1) % searchMatches.size
            jumpToCurrentSearchMatch()
        }
    }
    val goToPreviousSearchMatch: () -> Unit = {
        if (refreshSearchMatches()) {
            currentSearchMatchIndex = (currentSearchMatchIndex - 1 + searchMatches.size) % searchMatches.size
            jumpToCurrentSearchMatch()
        }
    }
    // 検索バーを閉じる。「ライブへ戻る」ボタンとは独立に扱う(検索結果を見ながら手動で
    // スクロールしている場合、バーを閉じただけでライブへ戻す挙動は驚きが大きいため
    // scrollOffset自体は変更しない——iOS版`closeSearchBar()`と対称)。
    val closeSearchBar: () -> Unit = {
        showSearchBar = false
        searchQuery = ""
        searchCaseSensitive = false
        searchMatches = emptyList()
        currentSearchMatchIndex = 0
    }
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

    // モーダルUI(host key/trzsz/agent forwarding/スニペット一覧確認)は「フォーカス中の
    // ペインに対してだけ表示する」設計(このComposableのdocstring参照)。表示条件を
    // TerminalModalHost 1箇所に集約し、個々のダイアログ呼び出し側でisActive/hasFocusの
    // gatingを繰り返し書かない(繰り返しの一箇所が漏れてバグになった実例があったため)。
    TerminalModalHost(
        uiState = uiState,
        actions = actions,
        snippets = snippets,
        keySequences = keySequences,
        installedPacks = installedPacks,
        showSnippetSheet = showSnippetSheet,
        onDismissSnippetSheet = { showSnippetSheet = false },
        showKeySequenceSheet = showKeySequenceSheet,
        onDismissKeySequenceSheet = { showKeySequenceSheet = false },
        visible = isActive && hasFocus,
    )

    val trzszState = uiState.trzszState
    val transferActive = trzszState is TrzszUiState.WaitingUser || trzszState is TrzszUiState.InProgress

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
                    color = when {
                        connected -> AppColors.Success
                        isReconnecting -> Color.Yellow
                        else -> AppColors.SecondaryText
                    },
                    fontSize = 11.sp,
                )
                Row(horizontalArrangement = Arrangement.spacedBy(8.dp)) {
                    if (isReconnecting) {
                        // 自動再接続中(Rust側のreconnectループ)は「再接続」ではなく、
                        // それを中止する操作を出す(手動での二重接続を避けるため
                        // connectPane側でも既にガードしているが、UI上もループが
                        // 動いている間は「接続する」ボタンではなく「中止する」ボタンを見せる)。
                        TextButton(
                            onClick = { actions.onCancelReconnect() },
                            contentPadding = PaddingValues(0.dp),
                        ) { Text("中止", color = Color.Yellow, fontSize = 11.sp) }
                    } else if (!connected) {
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
        // JIS/US配列モードの選択も ProfileListScreen 側のメニューで行う（グローバル設定）。
        val keyboardLayoutMode = remember {
            KeyboardLayoutMode.fromPrefValue(prefs.getString(KeyboardLayoutMode.PREF_KEY, null))
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
                // ソフトキーボード(IME)表示中は親Columnの`.imePadding()`がこの分だけ
                // `heightPx`(=ここのBoxWithConstraintsの実測高さ)を圧縮する。IME開閉
                // そのものはtty実サイズを変える理由にしたくない(タスク#19: IME開閉・回転・
                // ピンチズームのたびに不要なresize=SIGWINCH相当がvim等の実行中プログラムへ
                // 飛ぶのを防ぐ)ため、resize先のcols/rowsには「IMEが非表示だった直近の
                // 高さ」を凍結して使う(生のIME insetを足し戻して補正しない理由・初回
                // composition時の扱いは advanceResizeStability のdoc参照)。
                val isImeVisible = WindowInsets.isImeVisible
                var resizeStability by remember {
                    mutableStateOf(ResizeStabilityState(hasObservedImeClosed = !isImeVisible, stableHeightPx = heightPx))
                }
                resizeStability = advanceResizeStability(resizeStability, isImeVisible, heightPx)
                val stableHeightPx = resizeStability.stableHeightPx

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

                val (cols, rows) = computeResizeTargetColsRows(
                    widthPx = widthPx,
                    heightPx = stableHeightPx,
                    cellW = cellDims.first,
                    cellH = cellDims.second,
                )

                // タブがアクティブ化された直後にも、このタブの実際のビューポート寸法で
                // 確実に resize() が送られる（cols/rows は非アクティブ中は計算されないため）。
                LaunchedEffect(cols, rows, connected) {
                    if (connected) actions.onResize(cols.toUInt(), rows.toUInt())
                }

                // When scrolled into scrollback, synthesize a ScreenUpdate from the buffer.
                // 合成ロジック本体は tools.isekai.terminal.ui.synthesizeDisplayUpdate へ抽出済み
                // (タスク#46: iOS版`TerminalScrollback.swift`と対称にし、ユニットテスト可能にする)。
                // スクロールバック行の要求は必ず update.rows(ライブの行数)で行う——Compose層が
                // 独自計算したビューポート由来の rows/cols を使うと、リサイズ中の過渡状態で
                // Rust側の実際の行幅とズレて displayUpdate の cols/rows と cells 件数が
                // 食い違いうる(Codexレビュー: タスク#46、synthesizeDisplayUpdate側のdocを参照)。
                // タスク#79: `scrollOffset > 0`だけでなく、scrollback最新行(row=0)への
                // 検索ジャンプで`showingScrollback`が真の場合も合成表示へ切り替える。
                val displayUpdate = remember(scrollOffset, showingScrollback, rows, update) {
                    if (scrollOffset > 0 || showingScrollback) {
                        val sbCells = actions.onScrollbackCells(scrollOffset, update.rows.toInt())
                        synthesizeDisplayUpdate(update, scrollOffset, sbCells, showingScrollback)
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

                // ジェスチャーのヒットテスト(タップ/選択/ピンチ)に使うセル寸法・cols/rows
                // は、SshTerminalCanvas が実際の描画に使う値(実ピクセル領域を
                // displayUpdate.cols/rows へ均等割りしたセルサイズ)に厳密に合わせる。
                // 上の cellDims(フォント計測ベース)・cols/rows(resize要求用、IME非表示時
                // 想定の"安定"サイズ)をそのまま使うと、IME表示中(タスク#19の変更でtty側
                // cols/rowsを据え置くため、実ビューポート寸法と一致しなくなる)や
                // resize飛行中にタップ位置が実際のセルとズレる(Codexレビュー指摘:
                // TerminalScreen.kt既存の不整合。#46でのdisplayUpdate合成側の修正と同種)。
                val renderCols = displayUpdate.cols.toInt().coerceAtLeast(1)
                val renderRows = displayUpdate.rows.toInt().coerceAtLeast(1)
                val renderCellDims = Pair(widthPx / renderCols, heightPx / renderRows)

                // pointerInput の key に renderCellDims/renderCols/renderRows を直接使うと、
                // ピンチで fontScale が変わる → resize要求 → 新しい displayUpdate が届く →
                // renderCellDims/renderCols/renderRows が再計算される → key が変わり
                // ジェスチャー検出コルーチン自体がキャンセル&再起動される、という
                // 自己中断ループになってしまう(実機検証で確認: ピンチがほぼ常に
                // 完了前に中断されていた不具合の原因)。rememberUpdatedState 経由で
                // 最新値を読むことで、値が変わってもコルーチンを再起動せずに済ませる。
                val latestCellDims = rememberUpdatedState(renderCellDims)
                val latestCols = rememberUpdatedState(renderCols)
                val latestRows = rememberUpdatedState(renderRows)
                // タップ判定(OSC 8リンクのhit-test、タスク#52)は毎フレーム変わる画面内容を
                // 見る必要があるため、上と同じ理由でrememberUpdatedState経由で読む
                // (pointerInput(Unit)はkeyが変わらない限りコルーチンを再起動しないため、
                // 素の val をそのままクロージャに捕まえると古いスクリーン内容のまま固定される)。
                val latestDisplayUpdate = rememberUpdatedState(displayUpdate)

                // タスク#50: マウスレポーティング(`?1000`/`?1002`/`?1003`、rust-core タスク#36)
                // 有効時にポインタイベントをエンコードして送る共通処理。エンコード自体は
                // `terminalPointerEventBytes`(rust-core `terminal_pointer_event_bytes`、タスク#36/#51)
                // がRust側で行い、ここでは座標とジェスチャ種別を生のまま渡すだけ(rust-ssot: 「今
                // どのマウスモードか」「このイベントを報告すべきか」の判断はRust側の値
                // [latestDisplayUpdate.value.mouseReportingMode/sgrMouseMode]をそのまま見るだけで、
                // Kotlin側にミラー状態を作らない — iOS版TerminalScreenView.swift`sendMouseEvent`と対称)。
                // 報告対象外のイベント種別(モードOff・Normalモードでのmotion等)は
                // `terminalPointerEventBytes`が`null`を返すので、その判断もRust側に委ねてよい。
                //
                // (codexレビュー指摘: 修飾キー無しの`TerminalKeyModifiers`をドラッグ/ホイールの
                // たびに毎回アロケートしていたのを1つのremember済みインスタンスに統一)
                val noPointerModifiers = remember {
                    TerminalKeyModifiers(shift = false, alt = false, ctrl = false, meta = false)
                }
                // (codexレビュー指摘: タッチ経路・ホイール経路の両方で「マウスレポーティングが
                // 実際に有効か」の判定[scrollOffset==0 && mode!=Off]が重複していたのを1箇所へ
                // 集約。iOS版`isPointerReportingActive`と同じ役割)
                //
                // タスク#87: 判断ロジック自体は`MouseGestureArbiter.kt`のピュア関数
                // (`arbiterIsPointerReportingActive`としてimport)へ抽出済み。ここでは
                // 現在のComposable状態(scrollOffset/showingScrollback/mouseReportingMode)を
                // そのまま渡すだけ。
                val isPointerReportingActive: () -> Boolean = {
                    // タスク#79: `showingScrollback`が真の間(scrollback最新行への検索
                    // ジャンプ中)は`scrollOffset == 0`でもライブ表示ではないため、
                    // タッチ/ホイールをRustへ渡さない(表示対象と入力対象の食い違いを
                    // 避ける、下の`runPinchAndPan`のコメントと同じ理由)。
                    arbiterIsPointerReportingActive(
                        scrollOffset = scrollOffset,
                        showingScrollback = showingScrollback,
                        mouseReportingMode = latestDisplayUpdate.value.mouseReportingMode,
                    )
                }
                val sendPointerEvent: (MouseEventKind, MouseButton?, Int, Int) -> Unit = { kind, button, row, col ->
                    val u = latestDisplayUpdate.value
                    val bytes = terminalPointerEventBytes(
                        kind = kind,
                        button = button,
                        row = row.toUInt(),
                        col = col.toUInt(),
                        modifiers = noPointerModifiers,
                        cols = u.cols,
                        rows = u.rows,
                        mouseReportingMode = u.mouseReportingMode,
                        sgrMouseMode = u.sgrMouseMode,
                    )
                    if (bytes != null) actions.onSend(bytes)
                }
                // (codexレビュー指摘: 座標→セル変換[offsetToCellPos]+送出が press/motion/release/
                // wheelの4箇所で重複していたのを1つのヘルパーへ集約)
                val sendPointerEventAt: (MouseEventKind, MouseButton?, Float, Float, Float, Float, Int, Int) -> Unit =
                    { kind, button, x, y, cellW, cellH, cols, rows ->
                        val cell = offsetToCellPos(x, y, cellW, cellH, cols, rows)
                        sendPointerEvent(kind, button, cell.row, cell.col)
                    }

                // タップされたセルがOSC 8リンクを指していた場合の確認待ちURL(タスク#52)。
                // 「UI表示だけに閉じた状態」としてComposeローカルで保持する
                // (選択範囲・スクロール位置と同じ扱い。rust-ssot原則の対象外)。
                var pendingHyperlinkUrl by remember { mutableStateOf<String?>(null) }

                // タスク#66: 検索バーの現在マッチのハイライト。行位置の判断自体は
                // [searchHighlightMatch]にピュア関数として抽出済み(このComposable本体は
                // 現在マッチと現在のscrollOffsetをそのまま渡すだけ)。マッチの位置計算
                // (col/len)は一切ここでは行わず、Rust側`search_scrollback`が返した座標を
                // そのまま`SshTerminalCanvas`へ渡すだけ(rust-ssot)。
                val currentSearchMatch = searchMatches.getOrNull(currentSearchMatchIndex)
                val searchHighlight = searchHighlightMatch(currentSearchMatch, scrollOffset, showingScrollback)

                Box(modifier = Modifier.fillMaxSize()) {
                    SshTerminalCanvas(
                        update = displayUpdate,
                        selection = selection,
                        searchHighlight = searchHighlight,
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
                                    // タスク#50: マウスレポーティング有効(かつスクロールバック表示中でない
                                    // ——scrollOffset==0)の間は、単一指のタッチを選択(longPress)・スクロール
                                    // バックパン(pinch/pan)へ渡さず、代わりにマウスのpress/drag/releaseとして
                                    // Rustへ送る。scrollOffset>0を除外するのは、表示しているのが過去ログの
                                    // 合成表示である一方でライブ側のモードに従ってポインタイベントを送ると、
                                    // 表示対象(スクロールバック)と入力対象(ライブセッション)が食い違う
                                    // ため(iOS版`isPointerReportingActive`と同じ理由・同じ判断)。
                                    val initialPointerCount = currentEvent.changes.count { it.pressed }
                                    val mouseModeActive = shouldUseMouseTouch(isPointerReportingActive(), initialPointerCount)
                                    // タスク#80(codexレビュー指摘): ピンチ拡縮+縦パンスクロールの
                                    // イベントループ本体を、下の(2)通常経路とマウスモード経由の両方から
                                    // 呼べるよう共通化しておく(マウスモード時に2本目の指が検出された
                                    // 場合、以前はreleaseを送って`return@awaitEachGesture`するだけで
                                    // 同じジェスチャをピンチへ継続できず、マウスモード有効時はピンチが
                                    // 実質使えなかった — iOS版はpinch recognizerをマウスモード時も
                                    // 抑止対象にしていないため、Android側だけの不整合だった)。
                                    val runPinchAndPan: suspend AwaitPointerEventScope.() -> Unit = {
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
                                                    // タスク#79: 手動でライブ方向へパンし0まで戻したら、
                                                    // 検索ジャンプ由来の`showingScrollback`も解除する
                                                    // (「ライブへ戻る」ボタンと同じ扱い)。
                                                    if (scrollOffset == 0) showingScrollback = false
                                                    panAccumY -= cellH
                                                }
                                                event.changes.forEach { it.consume() }
                                            }
                                            if (event.changes.all { !it.pressed }) break
                                        }
                                    }
                                    if (mouseModeActive) {
                                        down.consume()
                                        onUserActivity()
                                        // codexレビュー指摘: 画面分割時、他ペインがフォーカス中のまま
                                        // このペインへマウスpress/dragを送ってしまうと、送信先(このペイン)と
                                        // 実際にフォーカス(=IME/物理キーボード入力・focus reportingの宛先)が
                                        // 食い違う。下のタップ分岐(単純タップ)と同じくペインフォーカスを
                                        // 切り替える(IMEは要求しない — 長押し選択と同じく、マウスモードの
                                        // タッチはソフトキーボードを呼び出す操作ではないため)。
                                        actions.onRequestFocus()
                                        sendPointerEventAt(MouseEventKind.PRESS, MouseButton.LEFT, down.position.x, down.position.y, cellW, cellH, cols, rows)
                                        var handoffToPinch = false
                                        while (true) {
                                            val event = awaitPointerEvent()
                                            val change = event.changes.firstOrNull { it.id == down.id } ?: break
                                            change.consume()
                                            // 2本目以降の指が触れてきた場合、単一指ドラッグとしては扱えないため
                                            // 直前のpressに対応するreleaseを送って打ち切る(iOS版`touchesBegan`の
                                            // 「2本目の指が触れたら追跡中のタッチのreleaseを送る」処理と同じ理由
                                            // ——releaseを送らないとリモート側でボタンが押されっぱなしに見える)。
                                            //
                                            // タスク#87: このステップの裁定自体は`decideMouseTouchStep`
                                            // (`MouseGestureArbiter.kt`)へ抽出済み。2本指中断+ピンチ引き継ぎ
                                            // (タスク#80)の回帰は`MouseGestureArbiterTest`でCompose非依存に
                                            // 検証する。
                                            val pointerCount = event.changes.count { it.pressed }
                                            when (decideMouseTouchStep(trackedFingerPressed = change.pressed, pointerCount = pointerCount)) {
                                                MouseTouchStep.CONTINUE ->
                                                    sendPointerEventAt(MouseEventKind.MOTION, MouseButton.LEFT, change.position.x, change.position.y, cellW, cellH, cols, rows)
                                                MouseTouchStep.RELEASE_ONLY -> {
                                                    sendPointerEventAt(MouseEventKind.RELEASE, MouseButton.LEFT, change.position.x, change.position.y, cellW, cellH, cols, rows)
                                                    handoffToPinch = false
                                                    break
                                                }
                                                MouseTouchStep.RELEASE_AND_HANDOFF_TO_PINCH -> {
                                                    sendPointerEventAt(MouseEventKind.RELEASE, MouseButton.LEFT, change.position.x, change.position.y, cellW, cellH, cols, rows)
                                                    // タスク#80: releaseの原因が「2本目の指が触れた」ことによる
                                                    // ものであれば、同じジェスチャをそのままピンチ/パン処理へ
                                                    // 引き継ぐ(単に指が離れただけ[2本目なし]の場合は継続しない)。
                                                    handoffToPinch = true
                                                    break
                                                }
                                            }
                                        }
                                        if (handoffToPinch) {
                                            runPinchAndPan()
                                        }
                                        return@awaitEachGesture
                                    }
                                    val longPress = awaitLongPressOrCancellation(down.id)
                                    // awaitLongPressOrCancellation は「指定した1本の指」の移動/リリースしか
                                    // 見ておらず、2本指が同時に押され続けている(=ピンチ操作中)場合でも
                                    // 長押しタイムアウト(既定 ~400ms)で非nullを返してしまう(実機ログで確認
                                    // 済み: 自然なピンチはほぼ確実にこの時間を超える)。そのため、ここで
                                    // 実際に押されている指の本数を見て、2本以上ならピンチ/パン優先で扱う
                                    // (単一指の本物の長押しだけを選択モードにする)。
                                    val pointerCount = currentEvent.changes.count { it.pressed }
                                    val stillDown = currentEvent.changes.firstOrNull { it.id == down.id }
                                    // タスク#87: 長押し/タップ/ピンチの3択の裁定自体は`classifyNormalGesture`
                                    // (`MouseGestureArbiter.kt`)へ抽出済み。以下の3分岐は判断結果に応じた
                                    // 副作用(選択ループ・hit-test・ピンチ委譲)のみを行う。
                                    when (
                                        classifyNormalGesture(
                                            longPressSucceeded = longPress != null,
                                            pointerCount = pointerCount,
                                            trackedFingerStillPressed = stillDown?.pressed == true,
                                        )
                                    ) {
                                        NormalGestureOutcome.SELECTION -> {
                                            // (1) 長押し成立 → 選択モード。選択中はスクロールに触れない
                                            // (= スクロール位置ロック)。以降のドラッグで head を更新する。
                                            // (`classifyNormalGesture`がSELECTIONを返すのは
                                            // longPressSucceeded[= longPress != null]が真の場合のみ)。
                                            val longPressResult = requireNotNull(longPress)
                                            val startCell = offsetToCellPos(
                                                longPressResult.position.x, longPressResult.position.y,
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
                                        }
                                        NormalGestureOutcome.TAP -> {
                                            // (3) 単純タップ（長押し不成立かつ移動なしで指が離れた）→
                                            // まずタップ位置がOSC 8リンク(タスク#52)を指しているか
                                            // hit-testする。hit-test自体は表示中のセル配列を読むだけの
                                            // UI表示に閉じた判断であり、rust-ssot原則の対象外
                                            // (linkId/linkTableは既にRust側がintern済みで公開している)。
                                            // リンクがあり、かつスキームがhttp/httpsの場合のみ確認
                                            // ダイアログへ回す(intent://等を無条件でACTION_VIEWへ渡さない
                                            // ——タスク#52 Fableレビュー2次のセキュリティ要件)。IMEを
                                            // 誤って開かないよう、この場合はrequestImeFocusを呼ばない。
                                            val tapCell = offsetToCellPos(
                                                down.position.x, down.position.y,
                                                cellW, cellH, cols, rows,
                                            )
                                            val tappedUrl = linkUrlAtCell(
                                                latestDisplayUpdate.value, tapCell.row, tapCell.col,
                                            )
                                            if (tappedUrl != null && isOpenableHyperlinkScheme(tappedUrl)) {
                                                pendingHyperlinkUrl = tappedUrl
                                            } else {
                                                // 画面分割時はこのペインへフォーカスを切り替え、その上でIMEフォーカスを要求する
                                                // (このペインがまだフォーカス外の場合、入力欄自体がこの呼び出し時点では
                                                // 未生成[inputViewがnull]のため即時には効かないが、onRequestFocus()による
                                                // 再コンポジションでinputViewが生成された時点でその生成側のAndroidView.update
                                                // が改めてrequestFocus+showSoftInputを行う)。
                                                actions.onRequestFocus()
                                                requestImeFocus()
                                            }
                                        }
                                        NormalGestureOutcome.PINCH_PAN -> {
                                            // (2) 2本指以上、または長押し不成立で移動 →
                                            // ピンチ拡縮+縦パンスクロール(ループ本体はマウスモード
                                            // 経由のピンチ引き継ぎ[タスク#80]と共通の`runPinchAndPan`)
                                            runPinchAndPan()
                                        }
                                    }
                                }
                            }
                            // タスク#50(Fableレビュー2次: 「scrollOffset==0かつmouse mode時のwheel→
                            // Rust送出」の裁定): 外付けマウス/トラックパッドのホイールスクロール
                            // (`PointerEventType.Scroll`、Android実機ではBluetoothマウス・Chromebook等
                            // 経由でのみ発生し、通常のタッチスクロールは別経路)を、マウスレポーティング
                            // 有効かつscrollOffset==0の間はscrollback移動ではなくRustへのwheel
                            // up/downイベントとして送る。上のタッチジェスチャ用pointerInputとは
                            // イベント系統が異なる(ホイールはボタン押下を伴わない`Scroll`型イベントで
                            // 届くため`awaitFirstDown`では捕捉できない)ため、別のpointerInputで待ち受ける。
                            //
                            // 対象外(Fableレビューで明示を求められたスコープ判断): alt-screenでの
                            // wheel→矢印キー変換(xterm `?1007` Alternate Scroll Mode相当)は実装しない。
                            // rust-core(タスク#36)は`?1007`のモード状態を保持しておらず、`ScreenUpdate`も
                            // 「現在alt screenかどうか」を公開していない — この判断はターミナル状態の
                            // SSOTであるRust側に持たせるべきで(rust-ssot)、Kotlin側で代替の判定
                            // (例えばESCシーケンスの目視パース)を持つのは避ける。実装するならまず
                            // rust-core側に`?1007`状態とalt-screen可視性を追加する別タスクが必要。
                            .pointerInput(Unit) {
                                awaitPointerEventScope {
                                    while (true) {
                                        val event = awaitPointerEvent()
                                        if (event.type != PointerEventType.Scroll) continue
                                        if (!isPointerReportingActive()) continue
                                        val change = event.changes.firstOrNull() ?: continue
                                        val deltaY = change.scrollDelta.y
                                        // Composeのスクロール系API(Modifier.scrollable等)と同じ符号規約:
                                        // 正のdeltaY = コンテンツを上へ送る(=下方向へスクロール、xtermの
                                        // wheel down/button 65)。実機(Bluetoothマウス等)未検証のため、
                                        // 符号が逆であれば実機確認時に反転させる。判定自体は
                                        // `wheelButtonForDelta`(タスク#87、`MouseGestureArbiter.kt`)へ抽出済み
                                        // ——`deltaY == 0f`の場合は`null`が返るのでcontinueする。
                                        val button = wheelButtonForDelta(deltaY) ?: continue
                                        change.consume()
                                        sendPointerEventAt(
                                            MouseEventKind.PRESS, button, change.position.x, change.position.y,
                                            latestCellDims.value.first, latestCellDims.value.second,
                                            latestCols.value, latestRows.value,
                                        )
                                    }
                                }
                            },
                    )

                    // 選択範囲のコピー。フローティングツールバーの「コピー」ボタンと物理キーボードの
                    // Ctrl+Shift+C / Meta+C ショートカット（下の inputView.onCopyRequested）の両方から
                    // 呼ばれる共通実装。選択が無ければ何もしない。
                    val performCopy: () -> Unit = copy@{
                        val sel = selection ?: return@copy
                        val text = reconstructSelectionText(displayUpdate, sel)
                        if (text.isNotEmpty()) {
                            val cm = context.getSystemService(Context.CLIPBOARD_SERVICE) as ClipboardManager
                            cm.setPrimaryClip(ClipData.newPlainText("isekai-terminal selection", text))
                        }
                        selection = null
                    }
                    // 物理キーボードショートカットは BoxWithConstraints スコープの外（入力エリアの
                    // AndroidView）から呼ばれるため、常に最新の performCopy クロージャ（selection/
                    // displayUpdate の現在値を捕捉したもの）を inputView 側へ反映しておく。
                    SideEffect { inputView?.onCopyRequested = performCopy }

                    // 選択中のフローティングツールバー（コピー／キャンセル）
                    selection?.let {
                        SelectionToolbar(
                            onCopy = performCopy,
                            onCancel = { selection = null },
                            modifier = Modifier.align(Alignment.TopCenter).padding(top = 8.dp),
                        )
                    }

                    // タスク#66: スクロールバック検索バー。開いている間は常に画面上部に表示する
                    // (選択ツールバーと同じ`Alignment.TopCenter`だが、選択中と検索中が同時に
                    // 起きることは操作上想定していないため重なりは考慮していない)。
                    if (showSearchBar) {
                        ScrollbackSearchBar(
                            query = searchQuery,
                            onQueryChange = { searchQuery = it; runSearch() },
                            caseSensitive = searchCaseSensitive,
                            onToggleCaseSensitive = { searchCaseSensitive = !searchCaseSensitive; runSearch() },
                            matchCount = searchMatches.size,
                            currentIndex = currentSearchMatchIndex,
                            onPrevious = goToPreviousSearchMatch,
                            onNext = goToNextSearchMatch,
                            onClose = closeSearchBar,
                            modifier = Modifier.align(Alignment.TopCenter),
                        )
                    }

                    // "Back to live" indicator when scrolled up (タスク#79:
                    // `showingScrollback`が真の間[scrollOffset==0のscrollback最新行表示]も
                    // 表示する — このボタン以外にライブへ戻る手段が無いため)
                    if (scrollOffset > 0 || showingScrollback) {
                        Button(
                            onClick = { scrollOffset = 0; showingScrollback = false; panAccumY = 0f },
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

                    // OSC 8リンク(タスク#52)タップ確認ダイアログ。URLはリモートホスト出力
                    // 由来の信頼できない入力のため、ここで全文を見せてユーザーが明示的に
                    // 「開く」を押した場合のみACTION_VIEWへ渡す。スキームは
                    // isOpenableHyperlinkScheme(http/httpsのみ)でタップ時点で既に
                    // 絞り込み済み。
                    pendingHyperlinkUrl?.let { url ->
                        HyperlinkConfirmDialog(
                            url = url,
                            onOpen = {
                                pendingHyperlinkUrl = null
                                // リモートホスト出力由来のURLなので、開けるアプリが無い端末・
                                // 制限プロファイル等でも例外でクラッシュしないようにする
                                // (codexレビュー指摘、タスク#52)。
                                try {
                                    context.startActivity(Intent(Intent.ACTION_VIEW, Uri.parse(url)))
                                } catch (e: ActivityNotFoundException) {
                                    RemoteLogger.w("Hyperlink", "no activity found to open $url", e)
                                }
                            },
                            onDismiss = { pendingHyperlinkUrl = null },
                        )
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
                    color = if (isReconnecting) Color.Yellow else Color.DarkGray,
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

                // クリップボードからの貼り付け。「貼付」ボタンと物理キーボードの Ctrl+Shift+V /
                // Meta+V ショートカット（下の inputView.onPasteRequested）の両方から呼ばれる。
                val performPaste: () -> Unit = {
                    val cm = context.getSystemService(Context.CLIPBOARD_SERVICE) as ClipboardManager
                    val text = cm.primaryClip?.takeIf { it.itemCount > 0 }
                        ?.getItemAt(0)?.coerceToText(context)?.toString()
                    if (!text.isNullOrEmpty()) {
                        actions.onSend(TerminalKeyEncoder.commitTextBytes(text, screenUpdate?.bracketedPasteMode ?: false))
                    }
                }

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
                    // 矢印ボタンはDECCKM(applicationCursorMode)に従ってSS3/CSI形式を切り替える
                    // 必要があるため、固定バイト列ではなくspecialKeyBytes経由にする(タスク#30、
                    // 以前はDECCKMを無視する既存バグだった。vim等のアプリケーションカーソルモード
                    // 中でもこのボタンから正しいシーケンスが送られるようにする)。
                    CtrlBtn("↑") { actions.onSend(TerminalKeyEncoder.specialKeyBytes(TerminalKeyEncoder.KC_DPAD_UP, screenUpdate?.applicationCursorMode ?: false)!!) }
                    CtrlBtn("↓") { actions.onSend(TerminalKeyEncoder.specialKeyBytes(TerminalKeyEncoder.KC_DPAD_DOWN, screenUpdate?.applicationCursorMode ?: false)!!) }
                    CtrlBtn("←") { actions.onSend(TerminalKeyEncoder.specialKeyBytes(TerminalKeyEncoder.KC_DPAD_LEFT, screenUpdate?.applicationCursorMode ?: false)!!) }
                    CtrlBtn("→") { actions.onSend(TerminalKeyEncoder.specialKeyBytes(TerminalKeyEncoder.KC_DPAD_RIGHT, screenUpdate?.applicationCursorMode ?: false)!!) }
                    CtrlBtn("貼付", onClick = performPaste)
                    CtrlBtn("定型") { showSnippetSheet = true }
                    CtrlBtn("打鍵") { showKeySequenceSheet = true }
                    // タスク#66: スクロールバック検索バーを開く。
                    CtrlBtn("検索") { showSearchBar = true }
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
                        view.applicationKeypadMode = screenUpdate?.applicationKeypadMode ?: false
                        view.bracketedPasteMode = screenUpdate?.bracketedPasteMode ?: false
                        view.keyboardLayoutMode = keyboardLayoutMode
                        view.ctrlArmed = ctrlArmed
                        view.onCtrlConsumed = { ctrlArmed = false }
                        // コピー(onCopyRequested)は BoxWithConstraints 内の SideEffect が selection/
                        // displayUpdate の最新値を捕捉した performCopy で常に上書きするため、ここでは
                        // 配線しない(先に設定してしまうと BoxWithConstraints 側の SideEffect 実行前は
                        // 空実装のままになり得るが、後続のコンポジションで確実に上書きされる)。
                        view.onPasteRequested = performPaste
                        view.onNextTabRequested = actions.onNextTab
                        view.onPreviousTabRequested = actions.onPreviousTab
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

/**
 * [TerminalScreenBody] の「タブ/ペインを跨いで1つしか存在しない」モーダルUI
 * (host key変更警告・初回接続確認・agent forwarding署名確認・trzsz転送シート・
 * 定型コマンド一覧)をまとめて表示するホスト。[visible] が false の間は何も表示しない
 * ([TerminalScreenBody]の`isActive && hasFocus`をそのまま渡す想定)。
 *
 * 個々のダイアログ呼び出しごとに`if (isActive && hasFocus)`を繰り返し書かないための
 * 抽出(繰り返しの一箇所が漏れて非フォーカス側ペインにダイアログが出るバグになった実例が
 * あったため)。
 */
@Composable
private fun TerminalModalHost(
    uiState: TerminalUiState,
    actions: TerminalScreenActions,
    snippets: List<Snippet>,
    keySequences: List<KeySequence>,
    installedPacks: List<Pair<tools.isekai.terminal.pack.KeySequencePack, tools.isekai.terminal.data.KeySequencePackInstallation>>,
    showSnippetSheet: Boolean,
    onDismissSnippetSheet: () -> Unit,
    showKeySequenceSheet: Boolean,
    onDismissKeySequenceSheet: () -> Unit,
    visible: Boolean,
) {
    if (!visible) return

    uiState.hostKeyChangedWarning?.let { w ->
        HostKeyChangedDialog(
            warning = w,
            onAccept = { actions.onTrustUpdatedHostKey() },
            onReject = { actions.onDismissHostKeyWarning() },
        )
    }

    uiState.newHostKeyPrompt?.let { prompt ->
        HostKeyUnknownDialog(
            host = prompt.host,
            port = prompt.port,
            fingerprint = prompt.fingerprint,
            onAccept = { actions.onTrustNewHostKey() },
            onReject = { actions.onDismissNewHostKeyPrompt() },
        )
    }

    uiState.agentSignRequestFingerprint?.let { fingerprint ->
        AgentSignConfirmDialog(
            fingerprint = fingerprint,
            onApprove = { actions.onRespondAgentSignRequest(true) },
            onReject = { actions.onRespondAgentSignRequest(false) },
        )
    }

    uiState.trzszState?.let { trzszState ->
        TrzszTransferSheet(
            state = trzszState,
            onStartUpload = { uri -> actions.onTrzszStartUpload(uri) },
            onStartDownload = { actions.onTrzszStartDownload() },
            onCancel = { actions.onTrzszCancel() },
            onDismiss = { actions.onTrzszDismiss() },
        )
    }

    if (showSnippetSheet) {
        SnippetPickerSheet(
            snippets = snippets,
            onPick = { snippet ->
                actions.onSendSnippet(snippet)
                onDismissSnippetSheet()
            },
            onDismiss = onDismissSnippetSheet,
        )
    }

    if (showKeySequenceSheet) {
        KeySequencePickerSheet(
            keySequences = keySequences,
            installedPacks = installedPacks,
            onSendSteps = { steps ->
                actions.onSendKeySequence(steps)
                onDismissKeySequenceSheet()
            },
            onDismiss = onDismissKeySequenceSheet,
        )
    }
}

/** 選択範囲がある間、コピー／キャンセル操作を出すフローティングツールバー。 */
@Composable
private fun SelectionToolbar(onCopy: () -> Unit, onCancel: () -> Unit, modifier: Modifier = Modifier) {
    Row(
        modifier = modifier
            .background(Color(0xCC1A1A2E), shape = MaterialTheme.shapes.small)
            .padding(horizontal = 8.dp, vertical = 4.dp),
        horizontalArrangement = Arrangement.spacedBy(4.dp),
    ) {
        TextButton(onClick = onCopy) { Text("コピー", color = Color.Cyan, fontSize = 12.sp) }
        TextButton(onClick = onCancel) { Text("キャンセル", color = Color.Gray, fontSize = 12.sp) }
    }
}

/**
 * タスク#66: スクロールバック検索バー。クエリ入力・大小文字区別トグル・前後マッチへの
 * ジャンプ・件数表示・閉じるボタンをまとめる。マッチの計算(部分一致・combining character
 * 境界・大小文字無視)は一切ここでは行わず、呼び出し元([TerminalScreenBody])が
 * `actions.onSearchScrollback`(Rust側`search_scrollback`、#37)経由で計算した結果を
 * そのまま受け取って表示するだけ(rust-ssot)。iOS版`TerminalView.swift`の`searchBar`と対称。
 */
@Composable
private fun ScrollbackSearchBar(
    query: String,
    onQueryChange: (String) -> Unit,
    caseSensitive: Boolean,
    onToggleCaseSensitive: () -> Unit,
    matchCount: Int,
    currentIndex: Int,
    onPrevious: () -> Unit,
    onNext: () -> Unit,
    onClose: () -> Unit,
    modifier: Modifier = Modifier,
) {
    val focusRequester = remember { FocusRequester() }
    // バーが開いた直後にクエリ欄へフォーカスし、ソフトキーボードを自動表示する。
    LaunchedEffect(Unit) { focusRequester.requestFocus() }
    Row(
        modifier = modifier
            .fillMaxWidth()
            .background(Color(0xE6000000))
            .padding(horizontal = 8.dp, vertical = 6.dp),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(4.dp),
    ) {
        TextField(
            value = query,
            onValueChange = onQueryChange,
            modifier = Modifier
                .weight(1f)
                .focusRequester(focusRequester),
            singleLine = true,
            placeholder = { Text("検索", fontSize = 13.sp) },
            colors = TextFieldDefaults.colors(
                focusedContainerColor = Color(0xFF1A1A1A),
                unfocusedContainerColor = Color(0xFF1A1A1A),
                focusedTextColor = Color.White,
                unfocusedTextColor = Color.White,
            ),
        )
        Text(
            if (matchCount == 0) "0/0" else "${currentIndex + 1}/$matchCount",
            color = Color.White,
            fontSize = 11.sp,
            modifier = Modifier.padding(horizontal = 2.dp),
        )
        CtrlBtn("Aa", active = caseSensitive, onClick = onToggleCaseSensitive)
        CtrlBtn("▲", onClick = onPrevious)
        CtrlBtn("▼", onClick = onNext)
        CtrlBtn("✕", onClick = onClose)
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

@OptIn(ExperimentalMaterial3Api::class)
@Composable
private fun KeySequencePickerSheet(
    keySequences: List<KeySequence>,
    installedPacks: List<Pair<tools.isekai.terminal.pack.KeySequencePack, tools.isekai.terminal.data.KeySequencePackInstallation>>,
    onSendSteps: (List<KeyStep>) -> Unit,
    onDismiss: () -> Unit,
) {
    ModalBottomSheet(onDismissRequest = onDismiss) {
        Column(
            modifier = Modifier
                .fillMaxWidth()
                .padding(horizontal = 16.dp, vertical = 12.dp)
                .navigationBarsPadding()
                .heightIn(max = 480.dp)
                .verticalScroll(rememberScrollState()),
            verticalArrangement = Arrangement.spacedBy(4.dp),
        ) {
            Text("打鍵列", style = MaterialTheme.typography.titleMedium)
            if (keySequences.isEmpty() && installedPacks.isEmpty()) {
                Text(
                    "登録された打鍵列がありません。プロファイル一覧の「打鍵列」から追加できます。",
                    color = Color(0xFFAAAAAA),
                    fontSize = 13.sp,
                    modifier = Modifier.padding(vertical = 12.dp),
                )
            } else {
                keySequences.forEach { keySequence ->
                    KeySequencePickerRow(
                        label = keySequence.label,
                        preview = keySequence.steps.previewText(),
                        onClick = { onSendSteps(keySequence.steps) },
                    )
                }
                installedPacks.forEach { (pack, installation) ->
                    Text(
                        pack.name,
                        color = Color(0xFFAAAAAA),
                        fontSize = 12.sp,
                        modifier = Modifier.padding(top = 8.dp),
                    )
                    val resolved = tools.isekai.terminal.pack.KeySequencePackResolver.resolve(pack, installation.paramValues)
                    resolved.forEach { seq ->
                        KeySequencePickerRow(
                            label = seq.label,
                            preview = seq.steps.previewText(),
                            onClick = { onSendSteps(seq.steps) },
                        )
                    }
                }
            }
        }
    }
}

@Composable
private fun KeySequencePickerRow(label: String, preview: String, onClick: () -> Unit) {
    Row(
        modifier = Modifier
            .fillMaxWidth()
            .clickable(onClick = onClick)
            .padding(vertical = 10.dp),
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Column(modifier = Modifier.weight(1f)) {
            Text(label, color = Color.White, fontSize = 15.sp)
            Text(
                preview,
                color = Color(0xFF888888),
                fontSize = 11.sp,
                fontFamily = FontFamily.Monospace,
                maxLines = 1,
            )
        }
    }
}
