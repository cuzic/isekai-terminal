package tools.isekai.terminal

import androidx.compose.ui.test.junit4.createComposeRule
import androidx.compose.ui.test.onNodeWithText
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config

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
}
