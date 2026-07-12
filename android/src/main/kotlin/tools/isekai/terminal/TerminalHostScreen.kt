package tools.isekai.terminal

import androidx.compose.animation.AnimatedVisibility
import androidx.compose.animation.fadeIn
import androidx.compose.animation.fadeOut
import androidx.compose.animation.slideInVertically
import androidx.compose.animation.slideOutVertically
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxHeight
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.IconButton
import androidx.compose.material3.ScrollableTabRow
import androidx.compose.material3.Tab
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.key
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.testTag
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import androidx.lifecycle.viewmodel.compose.viewModel
import kotlinx.coroutines.delay

/**
 * 上部バー(タブ行・[TerminalScreenBody]のステータス行)を、操作が無いまま自動で隠すまでの
 * 待ち時間。モバイルブラウザのアドレスバーと同様、画面いっぱいにターミナルを表示したい
 * という要望から導入 (ドラッグ操作で再表示 → この時間操作が無ければ再び隠れる)。
 */
private const val CHROME_AUTO_HIDE_DELAY_MS = 2500L

/**
 * 複数タブ (複数 SSH/QUIC セッション) を上部の [ScrollableTabRow] で切り替えるホスト画面。
 *
 * [TerminalTabsViewModel] は Application スコープで生成される想定 ([MainActivity.AppRoot] が
 * [IsekaiTerminalApplication] の ViewModelStoreOwner を使う限り、ナビゲーション遷移はもちろん
 * Activity の再生成をまたいでも同一インスタンスが使われ、バックグラウンドのタブは生き続ける)。
 *
 * 全タブ分の本体を常に composition に載せておき（スクロール位置・フォントスケール等の
 * ローカル状態を保持するため）、非アクティブなタブは [TerminalScreenBody] の `isActive = false`
 * で Canvas 描画をスキップする。
 *
 * 各タブは内部で画面分割(split pane)を持てる。1タブ=1ペインが既定で、`TerminalTabsViewModel`
 * の `splitPane`/`splitPaneWithExistingTab` を通じて水平/垂直の2分割まで可能
 * (バイナリツリー式の多段分割はスコープ外)。分割時、各ペインは完全に独立した
 * `TerminalSession` を持ち([TerminalTabsViewModel.PaneState])、キーボード入力・trzsz転送
 * シート・host key確認ダイアログ等の「1つしか存在しない」UIはフォーカス中のペインに対して
 * 表示する([TerminalScreenBody] の `hasFocus` パラメータ)。
 */
@Composable
fun TerminalHostScreen(
    onAllTabsClosed: () -> Unit,
    tabsVm: TerminalTabsViewModel = viewModel(),
) {
    val tabs by tabsVm.tabs.collectAsStateWithLifecycle()
    val activeTabId by tabsVm.activeTabId.collectAsStateWithLifecycle()

    if (tabs.isEmpty()) {
        onAllTabsClosed()
        return
    }

    // 上部バー(タブ行 + TerminalScreenBody のステータス行)の表示/非表示。「普段は画面いっぱいに
    // ターミナルを表示し、ドラッグ操作で見えるモバイルブラウザのアドレスバー」的な挙動にする要望から導入。
    // タブは常時コンポーズされている(非アクティブは0dpのプレースホルダ)ため、この状態はここで
    // 一元管理して両方の場所(タブ行/ステータス行)へ配る。
    var chromeVisible by remember { mutableStateOf(true) }
    var chromeRevealNonce by remember { mutableIntStateOf(0) }
    val revealChrome: () -> Unit = {
        chromeVisible = true
        chromeRevealNonce++
    }
    LaunchedEffect(chromeRevealNonce, chromeVisible) {
        if (chromeVisible) {
            delay(CHROME_AUTO_HIDE_DELAY_MS)
            chromeVisible = false
        }
    }

    Box(modifier = Modifier.fillMaxSize()) {
        Column(modifier = Modifier.fillMaxSize()) {
            val selectedIndex = tabs.indexOfFirst { it.tabId == activeTabId }.coerceAtLeast(0)
            AnimatedVisibility(
                visible = chromeVisible,
                enter = fadeIn() + slideInVertically(initialOffsetY = { -it }),
                exit = fadeOut() + slideOutVertically(targetOffsetY = { -it }),
            ) {
                ScrollableTabRow(
                    selectedTabIndex = selectedIndex,
                    containerColor = Color(0xFF1A1A2E),
                    contentColor = Color.White,
                    edgePadding = 4.dp,
                ) {
                    tabs.forEachIndexed { index, tab ->
                        Tab(
                            selected = index == selectedIndex,
                            onClick = { tabsVm.setActiveTab(tab.tabId) },
                            text = {
                                TabLabel(
                                    tabsVm = tabsVm,
                                    tab = tab,
                                    otherTabs = tabs.filterNot { it.tabId == tab.tabId },
                                    onClose = { tabsVm.closeTab(tab.tabId) },
                                )
                            },
                        )
                    }
                }
            }

            Box(modifier = Modifier.fillMaxSize()) {
                tabs.forEach { tab ->
                    key(tab.tabId) {
                        val isActive = tab.tabId == activeTabId
                        Box(
                            modifier = if (isActive) Modifier.fillMaxSize() else Modifier.size(0.dp),
                        ) {
                            TerminalTabScreen(
                                tab = tab,
                                tabsVm = tabsVm,
                                isActive = isActive,
                                onCloseTab = { tabsVm.closeTab(tab.tabId) },
                                chromeVisible = chromeVisible,
                                onUserActivity = revealChrome,
                            )
                        }
                    }
                }
            }
        }
    }
}

@Composable
private fun TabLabel(
    tabsVm: TerminalTabsViewModel,
    tab: TerminalTabsViewModel.TabState,
    otherTabs: List<TerminalTabsViewModel.TabState>,
    onClose: () -> Unit,
) {
    val uiState by tab.uiState.collectAsStateWithLifecycle(initialValue = TerminalUiState())
    val currentTheme by tab.currentTheme.collectAsStateWithLifecycle()
    val splitPane by tab.splitPane.collectAsStateWithLifecycle()
    var showThemeDialog by remember { mutableStateOf(false) }
    var showSplitDialog by remember { mutableStateOf(false) }

    Row(verticalAlignment = Alignment.CenterVertically) {
        Box(
            modifier = Modifier
                .size(8.dp)
                .clip(CircleShape)
                .background(
                    when {
                        uiState.connected -> Color(0xFF55FF55)
                        uiState.isConnecting -> Color.Yellow
                        else -> Color.Gray
                    },
                ),
        )
        Text(
            // リモートの OSC 0/2 タイトル変更があればそれを優先表示する(セッション/Rust側の
            // ScreenUpdate.title が SSOT)。tmux が横取りして届かない環境や、まだ何も
            // タイトルを送っていない接続直後は tab.label (プロファイル名) にフォールバックする
            // (ISEKAI_PIPE_DESIGN.md Epic M: 「tmuxに握りつぶされたときのフォールバック」の逆で、
            // ここは「OSCが届く環境ではそれを使う」通常経路)。
            text = uiState.screenUpdate?.title?.takeIf { it.isNotBlank() } ?: tab.label,
            modifier = Modifier.padding(start = 6.dp, end = 4.dp),
            maxLines = 1,
        )
        // 画面分割(split pane): 未分割なら分割メニューを開く、分割中なら解除する。
        IconButton(
            onClick = {
                if (splitPane != null) tabsVm.closeSplitPane(tab.tabId) else showSplitDialog = true
            },
            modifier = Modifier.size(20.dp).testTag("splitPaneButton"),
        ) {
            Text(if (splitPane != null) "⊟" else "⊞", fontSize = 12.sp, color = Color(0xFFAAAAAA))
        }
        // Phase 12 P2-1: このタブだけの配色テーマ変更(tab/session override)。
        IconButton(onClick = { showThemeDialog = true }, modifier = Modifier.size(20.dp)) {
            Text("🎨", fontSize = 12.sp)
        }
        IconButton(onClick = onClose, modifier = Modifier.size(20.dp).testTag("closeTabButton")) {
            Text("×", color = Color(0xFFAAAAAA), fontSize = 16.sp)
        }
    }

    if (showThemeDialog) {
        TerminalThemeDialog(
            current = currentTheme.name,
            onSelect = { theme -> tabsVm.setTabTheme(tab.tabId, theme) },
            onDismiss = { showThemeDialog = false },
        )
    }

    if (showSplitDialog) {
        SplitPaneDialog(
            otherTabs = otherTabs,
            onSplitNew = { direction ->
                tabsVm.splitPane(tab.tabId, direction)
                showSplitDialog = false
            },
            onSplitExisting = { direction, sourceTabId ->
                tabsVm.splitPaneWithExistingTab(tab.tabId, direction, sourceTabId)
                showSplitDialog = false
            },
            onDismiss = { showSplitDialog = false },
        )
    }
}

/**
 * 画面分割の方向・分割元(新規接続 or 既存タブの付け替え)を選ぶダイアログ。
 * `TerminalTabsViewModel.splitPane`/`splitPaneWithExistingTab` の2つの選択肢に対応する
 * (「同じ接続プロファイルで新規接続するか、既存タブのセッションを付け替えるか」)。
 */
@Composable
private fun SplitPaneDialog(
    otherTabs: List<TerminalTabsViewModel.TabState>,
    onSplitNew: (SplitDirection) -> Unit,
    onSplitExisting: (SplitDirection, String) -> Unit,
    onDismiss: () -> Unit,
) {
    var direction by remember { mutableStateOf<SplitDirection?>(null) }

    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text(if (direction == null) "画面分割" else "分割元を選択") },
        text = {
            Column {
                val d = direction
                if (d == null) {
                    TextButton(onClick = { direction = SplitDirection.HORIZONTAL }) { Text("左右に分割") }
                    TextButton(onClick = { direction = SplitDirection.VERTICAL }) { Text("上下に分割") }
                } else {
                    TextButton(onClick = { onSplitNew(d) }) { Text("新規接続（同じプロファイル）") }
                    if (otherTabs.isNotEmpty()) {
                        Text(
                            "既存タブから移動",
                            color = Color(0xFFAAAAAA),
                            fontSize = 12.sp,
                            modifier = Modifier.padding(top = 8.dp, bottom = 2.dp),
                        )
                        otherTabs.forEach { t ->
                            TextButton(onClick = { onSplitExisting(d, t.tabId) }) { Text(t.label) }
                        }
                    }
                }
            }
        },
        confirmButton = {},
        dismissButton = { TextButton(onClick = onDismiss) { Text("キャンセル") } },
    )
}

/**
 * 1タブ分のペイン構成を描画する。未分割なら主ペイン1つ、分割中は
 * [TerminalTabsViewModel.TabState.splitDirection] に従い左右(HORIZONTAL)/上下(VERTICAL)に
 * 並べる。各ペインの操作はすべて [TerminalTabsViewModel] にタブID・paneIDを添えて委譲する。
 */
@Composable
private fun TerminalTabScreen(
    tab: TerminalTabsViewModel.TabState,
    tabsVm: TerminalTabsViewModel,
    isActive: Boolean,
    onCloseTab: () -> Unit,
    chromeVisible: Boolean,
    onUserActivity: () -> Unit,
) {
    val splitPane by tab.splitPane.collectAsStateWithLifecycle()
    val splitDirection by tab.splitDirection.collectAsStateWithLifecycle()
    val focusedPaneId by tab.focusedPaneId.collectAsStateWithLifecycle()

    val split = splitPane
    if (split == null) {
        TerminalPaneScreen(
            tab = tab,
            pane = tab.primaryPane,
            tabsVm = tabsVm,
            isActive = isActive,
            hasFocus = true,
            onCloseTab = onCloseTab,
            chromeVisible = chromeVisible,
            onUserActivity = onUserActivity,
        )
        return
    }

    when (splitDirection) {
        SplitDirection.VERTICAL ->
            Column(modifier = Modifier.fillMaxSize()) {
                Box(modifier = Modifier.weight(1f).fillMaxWidth()) {
                    TerminalPaneScreen(
                        tab = tab, pane = tab.primaryPane, tabsVm = tabsVm, isActive = isActive,
                        hasFocus = focusedPaneId == tab.primaryPane.paneId, onCloseTab = onCloseTab,
                        chromeVisible = chromeVisible, onUserActivity = onUserActivity,
                    )
                }
                Box(modifier = Modifier.fillMaxWidth().height(2.dp).background(Color(0xFF444444)))
                Box(modifier = Modifier.weight(1f).fillMaxWidth()) {
                    TerminalPaneScreen(
                        tab = tab, pane = split, tabsVm = tabsVm, isActive = isActive,
                        hasFocus = focusedPaneId == split.paneId, onCloseTab = onCloseTab,
                        onCloseSplit = { tabsVm.closeSplitPane(tab.tabId) },
                        chromeVisible = chromeVisible, onUserActivity = onUserActivity,
                    )
                }
            }
        else ->
            Row(modifier = Modifier.fillMaxSize()) {
                Box(modifier = Modifier.weight(1f).fillMaxHeight()) {
                    TerminalPaneScreen(
                        tab = tab, pane = tab.primaryPane, tabsVm = tabsVm, isActive = isActive,
                        hasFocus = focusedPaneId == tab.primaryPane.paneId, onCloseTab = onCloseTab,
                        chromeVisible = chromeVisible, onUserActivity = onUserActivity,
                    )
                }
                Box(modifier = Modifier.width(2.dp).fillMaxHeight().background(Color(0xFF444444)))
                Box(modifier = Modifier.weight(1f).fillMaxHeight()) {
                    TerminalPaneScreen(
                        tab = tab, pane = split, tabsVm = tabsVm, isActive = isActive,
                        hasFocus = focusedPaneId == split.paneId, onCloseTab = onCloseTab,
                        onCloseSplit = { tabsVm.closeSplitPane(tab.tabId) },
                        chromeVisible = chromeVisible, onUserActivity = onUserActivity,
                    )
                }
            }
    }
}

/**
 * 1ペイン分の [TerminalScreenBody]。すべての操作は [TerminalTabsViewModel] にタブID・paneIDを
 * 添えて委譲する。[hasFocus] が true の間だけキーボード入力・trzsz/host key等のモーダルUIを
 * 表示する(「フォーカス中のペインに対して表示する」設計)。[onCloseSplit] が非nullなら
 * (=このペインが分割側なら)ステータスバーに分割解除ボタンを出す。
 */
@Composable
private fun TerminalPaneScreen(
    tab: TerminalTabsViewModel.TabState,
    pane: PaneState,
    tabsVm: TerminalTabsViewModel,
    isActive: Boolean,
    hasFocus: Boolean,
    onCloseTab: () -> Unit,
    onCloseSplit: (() -> Unit)? = null,
    chromeVisible: Boolean,
    onUserActivity: () -> Unit,
) {
    val tabId = tab.tabId
    val paneId = pane.paneId
    val uiState by pane.uiState.collectAsStateWithLifecycle(initialValue = TerminalUiState())
    val snippets by pane.snippets.collectAsStateWithLifecycle()

    // スクロール位置・選択範囲・フォントスケール等のローカル状態(TerminalScreenBody内部の
    // remember)は paneId ごとに key() で分離する必要がある(同一タブの2ペインが同じ
    // remember スロットを共有してしまわないように)。
    key(paneId) {
        Box(modifier = Modifier.fillMaxSize()) {
            TerminalScreenBody(
                uiState = uiState,
                canReconnect = tab.profile != null,
                isActive = isActive,
                hasFocus = hasFocus,
                snippets = snippets,
                chromeVisible = chromeVisible,
                onUserActivity = onUserActivity,
                actions = TerminalScreenActions(
                    onConnect = { tabsVm.reconnectPane(tabId, paneId) },
                    onDisconnect = { tabsVm.disconnectPane(tabId, paneId) },
                    // タブ内の「戻る」/切断確認ダイアログはタブを閉じる（＝タブ行の × と同じ操作）。
                    // 全タブが閉じられると呼び出し側 (TerminalHostScreen) が自動でリストへ戻る。
                    onBack = onCloseTab,
                    onSend = { bytes -> tabsVm.sendToPane(tabId, paneId, bytes) },
                    onResize = { cols, rows -> tabsVm.resizePane(tabId, paneId, cols, rows) },
                    onScrollbackCells = { offset, rows -> tabsVm.scrollbackCellsForPane(tabId, paneId, offset, rows) },
                    onTrustUpdatedHostKey = { tabsVm.trustUpdatedHostKeyForPane(tabId, paneId) },
                    onDismissHostKeyWarning = { tabsVm.dismissHostKeyWarningForPane(tabId, paneId) },
                    onTrustNewHostKey = { tabsVm.trustNewHostKeyForPane(tabId, paneId) },
                    onDismissNewHostKeyPrompt = { tabsVm.dismissNewHostKeyPromptForPane(tabId, paneId) },
                    onTrzszStartUpload = { uri -> tabsVm.trzszStartUploadForPane(tabId, paneId, uri) },
                    onTrzszStartDownload = { tabsVm.trzszStartDownloadForPane(tabId, paneId) },
                    onTrzszCancel = { tabsVm.trzszCancelForPane(tabId, paneId) },
                    onTrzszDismiss = { tabsVm.trzszDismissForPane(tabId, paneId) },
                    onGetSessionLog = { tabsVm.getSessionLogForPane(tabId, paneId) },
                    onSendSnippet = { snippet -> tabsVm.sendSnippetToPane(tabId, paneId, snippet) },
                    onRespondAgentSignRequest = { approved -> tabsVm.respondAgentSignRequestForPane(tabId, paneId, approved) },
                    onRequestFocus = { tabsVm.setFocusedPane(tabId, paneId) },
                    // 物理キーボードの Ctrl+Tab / Ctrl+Shift+Tab によるタブ切替（TerminalInputView 経由）。
                    // 画面分割中でもタブ切替はタブ単位の操作なので、どちらのペインからでも同じ
                    // tabsVm.nextTab()/previousTab() を呼ぶ(ペイン固有の版は不要)。
                    onNextTab = { tabsVm.nextTab() },
                    onPreviousTab = { tabsVm.previousTab() },
                    onForceReturnToWifi = { pane.session.forceReturnToWifi() },
                ),
            )

            if (onCloseSplit != null) {
                IconButton(
                    onClick = onCloseSplit,
                    modifier = Modifier
                        .align(Alignment.TopEnd)
                        .padding(top = 28.dp, end = 4.dp)
                        .size(20.dp)
                        .testTag("closeSplitButton"),
                ) {
                    Text("✕", color = Color(0xFFAAAAAA), fontSize = 12.sp)
                }
            }
        }
    }
}
