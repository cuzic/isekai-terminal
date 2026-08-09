package tools.isekai.terminal

import androidx.compose.ui.test.junit4.createComposeRule
import androidx.compose.ui.test.longClick
import androidx.compose.ui.test.onNodeWithTag
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.performTouchInput
import androidx.compose.ui.test.swipeDown
import org.junit.Assert.assertTrue
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config
import uniffi.isekai_terminal_core.CursorShape
import uniffi.isekai_terminal_core.MouseReportingMode
import uniffi.isekai_terminal_core.NotifyKind
import uniffi.isekai_terminal_core.PanelKind
import uniffi.isekai_terminal_core.ScreenUpdate

/**
 * [TerminalScreenBody] のモーダルUI(host key/trzsz/agent forwarding確認ダイアログ)が
 * `isActive && hasFocus` の時だけ表示される、という設計([TerminalScreenBody]のdocstring
 * 参照)を検証する。split pane中の非フォーカス側ペインでも表示されてしまう不具合が
 * agent forwarding確認ダイアログにだけあった(host key/trzszは既にgateされていた)ため、
 * 3種のダイアログをまとめて回帰確認する。
 */
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [28])
class TerminalScreenBodyTest {
    @get:Rule val composeTestRule = createComposeRule()

    private val noopActions = TerminalScreenActions(
        onConnect = {},
        onDisconnect = {},
        onBack = {},
        onSend = {},
        onResize = { _, _ -> },
        onScrollbackCells = { _, _ -> null },
        onTrustUpdatedHostKey = {},
        onDismissHostKeyWarning = {},
        onTrustNewHostKey = {},
        onDismissNewHostKeyPrompt = {},
        onTrzszStartUpload = {},
        onTrzszStartDownload = {},
        onTrzszCancel = {},
        onTrzszDismiss = {},
        onGetSessionLog = { "" },
        onSendSnippet = {},
        onRespondAgentSignRequest = {},
    )

    private fun setScreen(uiState: TerminalUiState, hasFocus: Boolean) {
        composeTestRule.setContent {
            TerminalScreenBody(
                uiState = uiState,
                canReconnect = true,
                actions = noopActions,
                isActive = true,
                hasFocus = hasFocus,
            )
        }
        composeTestRule.waitForIdle()
    }

    @Test
    fun agentSignRequest_whenPaneHasFocus_showsConfirmDialog() {
        setScreen(
            uiState = TerminalUiState(connected = true, agentSignRequestFingerprint = "AA:BB:CC"),
            hasFocus = true,
        )
        composeTestRule.onNodeWithText("署名要求の確認").assertExists()
    }

    @Test
    fun agentSignRequest_whenPaneLacksFocus_doesNotShowConfirmDialog() {
        setScreen(
            uiState = TerminalUiState(connected = true, agentSignRequestFingerprint = "AA:BB:CC"),
            hasFocus = false,
        )
        composeTestRule.onNodeWithText("署名要求の確認").assertDoesNotExist()
    }

    @Test
    fun hostKeyChangedWarning_whenPaneLacksFocus_doesNotShowDialog() {
        setScreen(
            uiState = TerminalUiState(
                connected = true,
                hostKeyChangedWarning = HostKeyChangedWarning(
                    host = "example.com", port = 22,
                    oldFingerprint = "old", newFingerprint = "new",
                ),
            ),
            hasFocus = false,
        )
        composeTestRule.onNodeWithText("ホスト鍵が変わりました", substring = true).assertDoesNotExist()
    }

    // ── 単一指ジェスチャー回帰テスト(2026-07-27) ────────────────────────
    // 実機で「ヘッダー(ログ/ファイル/切断/戻る)を表示する単一指ドラッグが一切
    // 機能しない」バグを発見し、awaitLongPressOrDragCancellationへの置き換えで
    // 修正した。修正の過程でOpusレビューにより「catchする例外型の解決違いで
    // 長押し選択(SELECTION)が完全に死ぬ」というブロッカー級の退行も見つかった
    // (`MouseGestureArbiterTest`はclassifyNormalGestureという純粋関数しか見ておらず、
    // このクラスのバグ[Compose境界のライブラリ解決違い]を原理的に検出できない)。
    // 両方が実際のCompose環境で動くことをここで回帰確認する。

    private fun minimalScreenUpdate() = ScreenUpdate(
        0u, 80u, 24u, emptyList(), 0u, 0u, null, null, null, null, false, false, false,
        MouseReportingMode.OFF, false, false, false, true, 0uL, 0uL, NotifyKind.INFO, "", "",
        0uL, PanelKind.NONE, "", "", emptyList(),
        CursorShape.BLOCK, true, emptyList(),
        emptyList(), 0u, null)

    @Test
    fun longClick_onTerminalCanvas_entersSelectionMode() {
        setScreen(
            uiState = TerminalUiState(connected = true, screenUpdate = minimalScreenUpdate()),
            hasFocus = true,
        )

        composeTestRule.onNodeWithTag("terminalCanvas").performTouchInput { longClick() }
        composeTestRule.waitForIdle()

        // 選択中のみ表示されるフローティングツールバー(SelectionToolbar)の「コピー」ボタン。
        composeTestRule.onNodeWithText("コピー").assertExists()
    }

    @Test
    fun singleFingerDrag_onTerminalCanvas_triggersUserActivityCallback() {
        var userActivityCalls = 0
        composeTestRule.setContent {
            TerminalScreenBody(
                uiState = TerminalUiState(connected = true, screenUpdate = minimalScreenUpdate()),
                canReconnect = true,
                actions = noopActions,
                isActive = true,
                hasFocus = true,
                chromeVisible = false,
                onUserActivity = { userActivityCalls++ },
            )
        }
        composeTestRule.waitForIdle()

        composeTestRule.onNodeWithTag("terminalCanvas").performTouchInput { swipeDown() }
        composeTestRule.waitForIdle()

        // 単一指ドラッグはヘッダー表示のトリガーであるonUserActivity()を呼ぶべき
        // (`TerminalHostScreen`側でこのコールバックがchromeVisible=trueへ切り替える)。
        assertTrue(
            "single-finger drag on the terminal canvas should call onUserActivity()",
            userActivityCalls > 0,
        )
    }
}
