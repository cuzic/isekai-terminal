package tools.isekai.terminal

import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.material3.IconButton
import androidx.compose.material3.ScrollableTabRow
import androidx.compose.material3.Tab
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.key
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

/**
 * 複数タブ (複数 SSH/QUIC セッション) を上部の [ScrollableTabRow] で切り替えるホスト画面。
 *
 * [TerminalTabsViewModel] は Activity スコープで生成される想定 (呼び出し元の `viewModel()`
 * が Activity の ViewModelStoreOwner を使う限り、ナビゲーション遷移をまたいでも同一インスタンス
 * が使われ、バックグラウンドのタブは生き続ける)。
 *
 * 全タブ分の本体を常に composition に載せておき（スクロール位置・フォントスケール等の
 * ローカル状態を保持するため）、非アクティブなタブは [TerminalScreenBody] の `isActive = false`
 * で Canvas 描画をスキップする。
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

    Box(modifier = Modifier.fillMaxSize()) {
        Column(modifier = Modifier.fillMaxSize()) {
            val selectedIndex = tabs.indexOfFirst { it.tabId == activeTabId }.coerceAtLeast(0)
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
                                onClose = { tabsVm.closeTab(tab.tabId) },
                            )
                        },
                    )
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
    onClose: () -> Unit,
) {
    val uiState by tab.uiState.collectAsStateWithLifecycle(initialValue = TerminalUiState())
    val currentTheme by tab.currentTheme.collectAsStateWithLifecycle()
    var showThemeDialog by remember { mutableStateOf(false) }

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
}

/**
 * 1タブ分の [TerminalScreenBody]。すべての操作は [TerminalTabsViewModel] にタブIDを添えて委譲する。
 */
@Composable
private fun TerminalTabScreen(
    tab: TerminalTabsViewModel.TabState,
    tabsVm: TerminalTabsViewModel,
    isActive: Boolean,
    onCloseTab: () -> Unit,
) {
    val tabId = tab.tabId
    val uiState by tab.uiState.collectAsStateWithLifecycle(initialValue = TerminalUiState())
    val snippets by tab.snippets.collectAsStateWithLifecycle()

    TerminalScreenBody(
        uiState = uiState,
        canReconnect = tab.profile != null,
        isActive = isActive,
        snippets = snippets,
        actions = TerminalScreenActions(
            onConnect = { tabsVm.reconnect(tabId) },
            onDisconnect = { tabsVm.disconnect(tabId) },
            // タブ内の「戻る」/切断確認ダイアログはタブを閉じる（＝タブ行の × と同じ操作）。
            // 全タブが閉じられると呼び出し側 (TerminalHostScreen) が自動でリストへ戻る。
            onBack = onCloseTab,
            onSend = { bytes -> tabsVm.send(tabId, bytes) },
            onResize = { cols, rows -> tabsVm.resize(tabId, cols, rows) },
            onScrollbackCells = { offset, rows -> tabsVm.scrollbackCells(tabId, offset, rows) },
            onTrustUpdatedHostKey = { tabsVm.trustUpdatedHostKey(tabId) },
            onDismissHostKeyWarning = { tabsVm.dismissHostKeyWarning(tabId) },
            onTrustNewHostKey = { tabsVm.trustNewHostKey(tabId) },
            onDismissNewHostKeyPrompt = { tabsVm.dismissNewHostKeyPrompt(tabId) },
            onTrzszStartUpload = { uri -> tabsVm.trzszStartUpload(tabId, uri) },
            onTrzszStartDownload = { tabsVm.trzszStartDownload(tabId) },
            onTrzszCancel = { tabsVm.trzszCancel(tabId) },
            onTrzszDismiss = { tabsVm.trzszDismiss(tabId) },
            onGetSessionLog = { tabsVm.getSessionLog(tabId) },
            onSendSnippet = { snippet -> tabsVm.sendSnippet(tabId, snippet) },
            onRespondAgentSignRequest = { approved -> tabsVm.respondAgentSignRequest(tabId, approved) },
            // 物理キーボードの Ctrl+Tab / Ctrl+Shift+Tab によるタブ切替（TerminalInputView 経由）。
            onNextTab = { tabsVm.nextTab() },
            onPreviousTab = { tabsVm.previousTab() },
        ),
    )
}
