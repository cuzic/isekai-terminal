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
import androidx.compose.material3.CircularProgressIndicator
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
import tools.isekai.terminal.data.AuthType
import uniffi.isekai_terminal_core.ProgressState

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
 *
 * タブ内の「戻る」([onNavigateToProfileList])はプロファイル一覧へ遷移するだけで、タブ/
 * セッションは破棄しない(タブの破棄はタブ行の「×」[TerminalTabsViewModel.closeTab]のみ)。
 * 同一プロファイルへもう1セッション増やしたい場合は、プロファイル一覧に戻らずタブ行の
 * 「+」からその場で新規タブを開ける。
 */
@Composable
fun TerminalHostScreen(
    onAllTabsClosed: () -> Unit,
    onNavigateToProfileList: () -> Unit,
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
                // タブ数が変わる(特に増える)瞬間、ScrollableTabRowが「新しいタブがまだ計測されて
                // いないのにselectedTabIndexだけ先にその新タブを指す」状態を1フレーム経由することが
                // あり、Compose Material3側の内部position配列アクセスでIndexOutOfBoundsExceptionを
                // 起こす(タブ追加と同時にその新タブをアクティブ化するopenTab()の直後、既にマウント
                // 済みのScrollableTabRowへ「+」ボタンからタブを追加した際に実機クラッシュとして発見、
                // タスク#57フォローアップ)。tabs.sizeをkeyにしてタブ数が変わるたびにScrollableTabRow
                // 自体を作り直させる(内部のtabPositions等の派生状態を新しいタブ数に対して最初から
                // 組み立て直させる)ことで、この“成長途中の1フレーム”を経由させない。
                key(tabs.size) {
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
                                // タブ内の「戻る」はタブを閉じない(プロファイル一覧へ遷移するだけ、
                                // セッションはバックグラウンドで生き続ける)。タブの破棄はタブ行の
                                // 「×」(onClose、上のTabLabel参照)のみが行う。
                                onBack = onNavigateToProfileList,
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
    val tmuxWindowLabel by tab.tmuxWindowLabel.collectAsStateWithLifecycle()
    var showThemeDialog by remember { mutableStateOf(false) }
    var showSplitDialog by remember { mutableStateOf(false) }
    // splitPaneの「新規接続（同じプロファイル）」がパスワード認証プロファイルの場合、
    // SplitPaneDialogを閉じてこちらのpending方向を使ってPasswordDialogを表示する。
    var pendingSplitNewDirection by remember { mutableStateOf<SplitDirection?>(null) }
    // 「+」(同一プロファイルへの新規タブ追加)がパスワード認証プロファイルの場合のPasswordDialog表示。
    var showNewSessionPasswordDialog by remember { mutableStateOf(false) }

    Row(verticalAlignment = Alignment.CenterVertically) {
        Box(
            modifier = Modifier
                .size(8.dp)
                .clip(CircleShape)
                .background(
                    when {
                        uiState.connected -> Color(0xFF55FF55)
                        uiState.isConnecting -> Color.Yellow
                        uiState.isReconnecting -> Color(0xFFFF9800)
                        else -> Color.Gray
                    },
                ),
        )
        // リモートが Windows Terminal 互換の OSC 4;264(または isekai-pipe ctl
        // tab-color 経由の CtlMessage::SetTabColor)でタブ色を指定していれば、
        // 接続状態ドットの隣にアクセントドットとして表示する(Rust側 SSOT である
        // ScreenUpdate.tab_color を直接読むだけで、Kotlin側にミラー状態は作らない)。
        // タブの背景自体は染めない — claude-hookd の idle 通知色(常時点灯し得る
        // 具体的な暗色)を背景に使うとタブが常時グレーがかって見え、色の変化に
        // 意味を読み取りにくくなるため(2026-08 Opusレビュー指摘)。
        uiState.screenUpdate?.tabColor?.let { tabColor ->
            Box(
                modifier = Modifier
                    .padding(start = 3.dp)
                    .size(8.dp)
                    .clip(CircleShape)
                    .background(Color(red = tabColor.r.toInt(), green = tabColor.g.toInt(), blue = tabColor.b.toInt()))
                    .testTag("tabColorDot"),
            )
        }
        // isekai-ssh(ctl_forward.rs::osc_sequence_for)がConEmu/Windows Terminal互換の
        // OSC 9;4へ変換するのと同じ CtlMessage::SetProgress を、tabColor と同じ経路
        // (Rust側SSOTである ScreenUpdate.tab_progress を直接読むだけ)で反映する
        // (isekai-pipe ctl progress/ctl build 起点、2026-08)。タブの進捗インジケータは
        // 常時点灯するものではない(ビルド等の一時的な状態)ため、tabColorDot と違って
        // 背景着色との衝突は起きない。
        uiState.screenUpdate?.tabProgress?.let { tabProgress ->
            val indicatorColor = when (tabProgress.state) {
                ProgressState.ERROR -> Color(0xFFFF5252)
                ProgressState.WARNING -> Color(0xFFFFC107)
                else -> Color(0xFF64B5F6)
            }
            val indicatorModifier = Modifier
                .padding(start = 3.dp)
                .size(10.dp)
                .testTag("tabProgressIndicator")
            if (tabProgress.state == ProgressState.INDETERMINATE) {
                CircularProgressIndicator(
                    modifier = indicatorModifier,
                    color = indicatorColor,
                    strokeWidth = 1.5.dp,
                )
            } else {
                // ERROR/WARNINGはOSC 9;4の慣習上progress値が意味を持たない
                // (Rust側`ProgressState`のdocコメント参照)ため、0%表示で弧が消えて
                // トラック色だけが見える事故を避けるよう常に満円(1f)で塗る。
                // NORMALのみ実際の`progress`値(0-100)を反映する。
                val fraction = if (tabProgress.state == ProgressState.NORMAL) tabProgress.progress.toInt() / 100f else 1f
                CircularProgressIndicator(
                    progress = { fraction },
                    modifier = indicatorModifier,
                    color = indicatorColor,
                    strokeWidth = 1.5.dp,
                )
            }
        }
        Text(
            // リモートの OSC 0/2 タイトル変更があればそれを優先表示する(セッション/Rust側の
            // ScreenUpdate.title が SSOT)。tmux が横取りして届かない環境や、まだ何も
            // タイトルを送っていない接続直後は tab.label (プロファイル名) にフォールバックする
            // (ISEKAI_PIPE_DESIGN.md Epic M: 「tmuxに握りつぶされたときのフォールバック」の逆で、
            // ここは「OSCが届く環境ではそれを使う」通常経路)。
            // タスク#60: tmux session groupのウィンドウ紐付けが解決していれば
            // (`tmuxWindowLabel`、primary paneのみ)、末尾に最小限のサフィックスとして
            // 付け足す(独立した判断ロジックはRust側、ここは値の素通し表示のみ)。
            text = (uiState.screenUpdate?.title?.takeIf { it.isNotBlank() } ?: tab.label) +
                (tmuxWindowLabel?.let { " · $it" } ?: ""),
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
        // 同一プロファイルへの新規タブ追加(同じサーバーへもう1セッション増やしたい場合の
        // 最短経路。プロファイル一覧に戻ってタップし直す必要がない)。分割ペインの
        // 「新規接続（同じプロファイル）」と同じくtabsVm.openTabを直接呼ぶ。
        val profile = tab.profile
        if (profile != null) {
            IconButton(
                onClick = {
                    val needsPasswordPrompt = profile.authTypeEnum == AuthType.PASSWORD ||
                        (profile.usesJumpHost && profile.jumpAuthTypeEnum == AuthType.PASSWORD)
                    if (needsPasswordPrompt) {
                        showNewSessionPasswordDialog = true
                    } else {
                        tabsVm.openTab(profile)
                    }
                },
                modifier = Modifier.size(20.dp).testTag("addSessionButton"),
            ) {
                Text("+", color = Color(0xFFAAAAAA), fontSize = 14.sp)
            }
        }
        IconButton(onClick = onClose, modifier = Modifier.size(20.dp).testTag("closeTabButton")) {
            Text("×", color = Color(0xFFAAAAAA), fontSize = 16.sp)
        }
    }

    if (showNewSessionPasswordDialog) {
        val profile = tab.profile
        if (profile == null) {
            showNewSessionPasswordDialog = false
        } else {
            PasswordDialog(
                label = profile.label,
                showMainField = profile.authTypeEnum == AuthType.PASSWORD,
                jumpLabel = if (profile.usesJumpHost && profile.jumpAuthTypeEnum == AuthType.PASSWORD) profile.jumpHost else null,
                onDismiss = { showNewSessionPasswordDialog = false },
                onConfirm = { password, jumpPassword ->
                    tabsVm.openTab(profile, password, jumpPassword)
                    showNewSessionPasswordDialog = false
                },
            )
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
                showSplitDialog = false
                val profile = tab.profile
                val needsPasswordPrompt = profile != null &&
                    (profile.authTypeEnum == AuthType.PASSWORD || (profile.usesJumpHost && profile.jumpAuthTypeEnum == AuthType.PASSWORD))
                if (needsPasswordPrompt) {
                    pendingSplitNewDirection = direction
                } else {
                    tabsVm.splitPane(tab.tabId, direction)
                }
            },
            onSplitExisting = { direction, sourceTabId ->
                tabsVm.splitPaneWithExistingTab(tab.tabId, direction, sourceTabId)
                showSplitDialog = false
            },
            onDismiss = { showSplitDialog = false },
        )
    }

    pendingSplitNewDirection?.let { direction ->
        val profile = tab.profile
        if (profile == null) {
            pendingSplitNewDirection = null
        } else {
            PasswordDialog(
                label = profile.label,
                showMainField = profile.authTypeEnum == AuthType.PASSWORD,
                jumpLabel = if (profile.usesJumpHost && profile.jumpAuthTypeEnum == AuthType.PASSWORD) profile.jumpHost else null,
                onDismiss = { pendingSplitNewDirection = null },
                onConfirm = { password, jumpPassword ->
                    tabsVm.splitPane(tab.tabId, direction, password, jumpPassword)
                    pendingSplitNewDirection = null
                },
            )
        }
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
    onBack: () -> Unit,
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
            onBack = onBack,
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
                        hasFocus = focusedPaneId == tab.primaryPane.paneId, onBack = onBack,
                        chromeVisible = chromeVisible, onUserActivity = onUserActivity,
                    )
                }
                Box(modifier = Modifier.fillMaxWidth().height(2.dp).background(Color(0xFF444444)))
                Box(modifier = Modifier.weight(1f).fillMaxWidth()) {
                    TerminalPaneScreen(
                        tab = tab, pane = split, tabsVm = tabsVm, isActive = isActive,
                        hasFocus = focusedPaneId == split.paneId, onBack = onBack,
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
                        hasFocus = focusedPaneId == tab.primaryPane.paneId, onBack = onBack,
                        chromeVisible = chromeVisible, onUserActivity = onUserActivity,
                    )
                }
                Box(modifier = Modifier.width(2.dp).fillMaxHeight().background(Color(0xFF444444)))
                Box(modifier = Modifier.weight(1f).fillMaxHeight()) {
                    TerminalPaneScreen(
                        tab = tab, pane = split, tabsVm = tabsVm, isActive = isActive,
                        hasFocus = focusedPaneId == split.paneId, onBack = onBack,
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
    onBack: () -> Unit,
    onCloseSplit: (() -> Unit)? = null,
    chromeVisible: Boolean,
    onUserActivity: () -> Unit,
) {
    val tabId = tab.tabId
    val paneId = pane.paneId
    val address = PaneAddress(tabId, paneId)
    val uiState by pane.uiState.collectAsStateWithLifecycle(initialValue = TerminalUiState())
    val snippets by pane.snippets.collectAsStateWithLifecycle()
    val keySequences by pane.keySequences.collectAsStateWithLifecycle()
    val installedPacks by pane.installedPacks.collectAsStateWithLifecycle()

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
                keySequences = keySequences,
                installedPacks = installedPacks,
                chromeVisible = chromeVisible,
                onUserActivity = onUserActivity,
                actions = TerminalScreenActions(
                    onConnect = { tabsVm.reconnectPane(address) },
                    onDisconnect = { tabsVm.disconnectPane(address) },
                    onCancelReconnect = { tabsVm.cancelReconnectPane(address) },
                    onBack = onBack,
                    onSend = { bytes -> tabsVm.sendToPane(address, bytes) },
                    onResize = { cols, rows -> tabsVm.resizePane(address, cols, rows) },
                    onScrollbackCells = { offset, rows -> tabsVm.scrollbackCellsForPane(address, offset, rows) },
                    onSearchScrollback = { query, caseSensitive -> tabsVm.searchScrollbackForPane(address, query, caseSensitive) },
                    onJumpToPreviousPrompt = { fromScrollOffset, fromShowingScrollback ->
                        tabsVm.jumpToPreviousPromptForPane(address, fromScrollOffset, fromShowingScrollback)
                    },
                    onJumpToNextPrompt = { fromScrollOffset, fromShowingScrollback ->
                        tabsVm.jumpToNextPromptForPane(address, fromScrollOffset, fromShowingScrollback)
                    },
                    onClickToPromptCursor = { row, col -> tabsVm.clickToPromptCursorForPane(address, row, col) },
                    onCopyLastCommandOutput = { tabsVm.copyLastCommandOutputForPane(address) },
                    onTrustUpdatedHostKey = { tabsVm.trustUpdatedHostKeyForPane(address) },
                    onDismissHostKeyWarning = { tabsVm.dismissHostKeyWarningForPane(address) },
                    onTrustNewHostKey = { tabsVm.trustNewHostKeyForPane(address) },
                    onDismissNewHostKeyPrompt = { tabsVm.dismissNewHostKeyPromptForPane(address) },
                    onTrzszStartUpload = { uri -> tabsVm.trzszStartUploadForPane(address, uri) },
                    onTrzszStartDownload = { tabsVm.trzszStartDownloadForPane(address) },
                    onTrzszCancel = { tabsVm.trzszCancelForPane(address) },
                    onTrzszDismiss = { tabsVm.trzszDismissForPane(address) },
                    onGetSessionLog = { tabsVm.getSessionLogForPane(address) },
                    onSendSnippet = { snippet -> tabsVm.sendSnippetToPane(address, snippet) },
                    onSendKeySequence = { steps -> tabsVm.sendKeySequenceToPane(address, steps) },
                    onRespondAgentSignRequest = { approved -> tabsVm.respondAgentSignRequestForPane(address, approved) },
                    onRequestFocus = { tabsVm.setFocusedPane(address) },
                    // 物理キーボードの Ctrl+Tab / Ctrl+Shift+Tab によるタブ切替（TerminalInputView 経由）。
                    // 画面分割中でもタブ切替はタブ単位の操作なので、どちらのペインからでも同じ
                    // tabsVm.nextTab()/previousTab() を呼ぶ(ペイン固有の版は不要)。
                    onNextTab = { tabsVm.nextTab() },
                    onPreviousTab = { tabsVm.previousTab() },
                    onForceReturnToWifi = { pane.session.forceReturnToWifi() },
                    onFocusChanged = { focused -> pane.session.notifyFocusChange(focused) },
                    // タスク#17(ファイルプレビュー機能): `TerminalSession.filePreviewRequest`への
                    // 薄い委譲(パース/デコードはRust側で完結、Kotlin側は中継のみ)。
                    onFilePreviewRequest = { kind -> pane.session.filePreviewRequest(kind) },
                    // `AI_INTEGRATION_DESIGN.md` §6.2: `TerminalSession`への薄い委譲
                    // (フィールド値のJSON化・PTYへの書き戻し・パネルのdismissは全て
                    // TerminalSession.submitAiPanelForm/dismissAiPanel側で完結)。
                    onSubmitAiPanelForm = { values -> pane.session.submitAiPanelForm(values) },
                    onDismissAiPanel = { pane.session.dismissAiPanel() },
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
