package tools.isekai.terminal

import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.size
import androidx.compose.runtime.MutableState
import androidx.compose.runtime.mutableStateOf
import androidx.compose.ui.Modifier
import androidx.compose.ui.semantics.SemanticsActions
import androidx.compose.ui.test.assertIsDisplayed
import androidx.compose.ui.test.junit4.createComposeRule
import androidx.compose.ui.test.onNodeWithTag
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.performScrollTo
import androidx.compose.ui.test.performSemanticsAction
import androidx.compose.ui.test.performTouchInput
import androidx.compose.ui.test.swipeUp
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.dp
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config
import tools.isekai.terminal.ui.effectiveCanvasHeightPx
import tools.isekai.terminal.ui.isCursorRowVisible
import tools.isekai.terminal.ui.visibleRowRange
import uniffi.isekai_terminal_core.CursorShape
import uniffi.isekai_terminal_core.MouseReportingMode
import uniffi.isekai_terminal_core.NotifyKind
import uniffi.isekai_terminal_core.PanelKind
import uniffi.isekai_terminal_core.ScreenUpdate

/**
 * 項目6 Tier 1(IMEレイアウト回帰): PR#64(コミット`6673e34f`)の「接続直後の空バッファで
 * IMEを開くと凍結グリッド(タスク#19)の下端クリップでプロンプト行が画面外に追いやられる」
 * バグを、実際の[TerminalScreenBody]をRobolectric上でマウントして回帰確認する。
 *
 * `WindowInsets.isImeVisible`の実際のinsets配線はRobolectricの`createComposeRule()`からは
 * 安全に注入できない([TerminalScreenBody]のdocstring参照)ため、テスト用シーム
 * `imeVisibleOverride`でIME表示状態を注入し、ビューポートの実測サイズ自体は自前の
 * `Box(Modifier.size(...))`で外側から制御することでIME表示時の縮小を模擬する
 * (`.imePadding()`の実際の効果はRobolectric上では発生しないため、この2つを独立に
 * 制御する必要がある)。
 *
 * 純粋関数側の不変条件(`visibleRowRange`/`isCursorRowVisible`)は`TerminalResizeTest.kt`が
 * 担う。ここではComposeの実際の配線(`imeVisibleOverride` → `advanceResizeStability` →
 * `onResize`呼び出し、補助ドロワーの実際の到達可能性)を担う。
 */
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [28])
class TerminalImeLayoutTest {
    @get:Rule val composeTestRule = createComposeRule()

    // IME非表示時の全画面高さ相当。
    private val fullHeightDp = 800.dp

    // IME表示中に縮んだ実測ビューポート高さ相当(PR#64の実機再現に合わせ、全画面の半分以下。
    // 220dpは補助ドロワーの全ボタン(⌨/履歴▲▼/Wheel×4/Resize、PR#64時点で10個)がverticalScroll
    // 無しには収まらない高さ——スクロールパス自体を実際に踏ませるため意図的に小さくしてある)。
    private val shrunkHeightDp = 220.dp

    private val viewportWidthDp = 400.dp

    private fun minimalScreenUpdate() = ScreenUpdate(
        0u, 80u, 24u, emptyList(), 0u, 0u, null, null, null, null, false, false, false,
        MouseReportingMode.OFF, false, false, false, true, 0uL, 0uL, NotifyKind.INFO, "", "",
        0uL, PanelKind.NONE, "", "", emptyList(),
        CursorShape.BLOCK, true, emptyList(),
        emptyList(), 0u, null,
    )

    private fun noopActions(onResize: (UInt, UInt) -> Unit) = TerminalScreenActions(
        onConnect = {},
        onDisconnect = {},
        onBack = {},
        onSend = {},
        onResize = onResize,
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

    /** [heightState]/[imeState]を外側から操作できる形で[TerminalScreenBody]をマウントする。 */
    private fun setImeAwareScreen(
        heightState: MutableState<Dp>,
        imeState: MutableState<Boolean>,
        onResize: (UInt, UInt) -> Unit,
    ) {
        composeTestRule.setContent {
            Box(modifier = Modifier.size(viewportWidthDp, heightState.value)) {
                TerminalScreenBody(
                    uiState = TerminalUiState(connected = true, screenUpdate = minimalScreenUpdate()),
                    canReconnect = true,
                    actions = noopActions(onResize),
                    isActive = true,
                    hasFocus = true,
                    chromeVisible = false,
                    imeVisibleOverride = imeState.value,
                )
            }
        }
    }

    @Test
    fun imeVisible_freezesCanvasHeight_doesNotTriggerOnResize() {
        val rowsSeen = mutableListOf<UInt>()
        val heightState = mutableStateOf(fullHeightDp)
        val imeState = mutableStateOf(false)
        setImeAwareScreen(heightState, imeState) { _, rows -> rowsSeen.add(rows) }
        composeTestRule.waitForIdle()

        assertTrue("接続直後にonResizeが最低1回呼ばれているはず", rowsSeen.isNotEmpty())
        val callsBeforeIme = rowsSeen.size

        // IMEが開き、実測ビューポートが縮む(imePadding()相当)。
        composeTestRule.runOnIdle {
            imeState.value = true
            heightState.value = shrunkHeightDp
        }
        composeTestRule.waitForIdle()

        // タスク#19: IME表示中はheightPxが縮んでもtty側cols/rowsは凍結されたまま
        // ——新たなonResize呼び出しは発生しない。
        assertEquals(
            "IME表示中はキャンバス高さが凍結され、追加のonResizeは呼ばれないはず",
            callsBeforeIme,
            rowsSeen.size,
        )
    }

    @Test
    fun freshEmptyBuffer_promptRow0_isHiddenWhenImeOpens_knownConstraint() {
        // 上のテストと同じ遷移(fullHeightDp → shrunkHeightDp、IME表示)を実際に
        // TerminalScreenBody上で再現したうえで、その凍結状態が意味する帰結
        // ([visibleRowRange]/[isCursorRowVisible]、TerminalResizeTest.kt参照)を確認する。
        // 既知の制約(脱出口は補助ドロワーのResizeボタン)としての明示的なアサート。
        val rowsSeen = mutableListOf<UInt>()
        val heightState = mutableStateOf(fullHeightDp)
        val imeState = mutableStateOf(false)
        setImeAwareScreen(heightState, imeState) { _, rows -> rowsSeen.add(rows) }
        composeTestRule.waitForIdle()

        composeTestRule.runOnIdle {
            imeState.value = true
            heightState.value = shrunkHeightDp
        }
        composeTestRule.waitForIdle()

        val density = composeTestRule.density
        val stableHeightPx = with(density) { fullHeightDp.toPx() }
        val liveHeightPx = with(density) { shrunkHeightDp.toPx() }
        val rows = 24
        val cellH = effectiveCanvasHeightPx(stableHeightPx, liveHeightPx) / rows
        val visible = visibleRowRange(stableHeightPx, liveHeightPx, cellH, rows)

        // 接続直後の空バッファのプロンプトはrow 0(グリッド最上段)にある(minimalScreenUpdate
        // のcursorRow=0uと同じ規約)。
        assertFalse(
            "既知の制約: 接続直後の空バッファでIMEを開くとプロンプト行(row 0)は画面外に" +
                "クリップされる。脱出口は補助ドロワーのResizeボタン。",
            isCursorRowVisible(cursorRow = 0, visible = visible),
        )
    }

    @Test
    fun auxDrawerResizeButton_triggersOnResize_forTheShrunkViewport() {
        val rowsSeen = mutableListOf<UInt>()
        val heightState = mutableStateOf(fullHeightDp)
        val imeState = mutableStateOf(false)
        setImeAwareScreen(heightState, imeState) { _, rows -> rowsSeen.add(rows) }
        composeTestRule.waitForIdle()
        val rowsBeforeShrink = rowsSeen.last()

        composeTestRule.runOnIdle {
            imeState.value = true
            heightState.value = shrunkHeightDp
        }
        composeTestRule.waitForIdle()
        assertEquals("縮小直後は凍結されて追加callが無いはず", 1, rowsSeen.size)

        // 上向きスワイプで補助ドロワーを表示する(タスク#89、TerminalScreen.ktの
        // shouldRevealAuxDrawer)。
        composeTestRule.onNodeWithTag("terminalCanvas").performTouchInput { swipeUp() }
        composeTestRule.waitForIdle()

        // 手動Resizeボタン(PR#64): stableHeightPxを現在の(縮んだ)heightPxへ強制的に
        // 合わせ、既存のLaunchedEffect(cols, rows, connected)経由でonResizeが再度呼ばれる。
        composeTestRule.onNodeWithText("Resize").performScrollTo().performSemanticsAction(SemanticsActions.OnClick)
        composeTestRule.waitForIdle()

        assertTrue(
            "Resizeボタン押下後に新しいonResize呼び出しが発生するはず",
            rowsSeen.size > 1,
        )
        assertTrue(
            "縮んだviewport相当のrows($rowsBeforeShrink→${rowsSeen.last()})に減っているはず",
            rowsSeen.last() < rowsBeforeShrink,
        )
    }

    @Test
    fun auxDrawer_allButtonsIncludingTheLast_areReachableViaScroll_inShrunkViewport() {
        val heightState = mutableStateOf(shrunkHeightDp)
        val imeState = mutableStateOf(true)
        setImeAwareScreen(heightState, imeState) { _, _ -> }
        composeTestRule.waitForIdle()

        composeTestRule.onNodeWithTag("terminalCanvas").performTouchInput { swipeUp() }
        composeTestRule.waitForIdle()

        // 他のCtrlキー行(下部の常設バー)と重複しないラベルのみを対象にする(PgUp/PgDnは
        // 補助ドロワーと下部バーの両方に同名ボタンがあり曖昧になるため対象外)。末尾の
        // "Resize"(PR#64で追加、縦スクロールが導入されるきっかけになったボタン)まで
        // 到達できることを確認する。
        val auxDrawerOnlyLabels = listOf("⌨", "履歴▲", "履歴▼", "Wheel▲", "Wheel▼", "Wheel▲3x", "Wheel▼3x", "Resize")
        auxDrawerOnlyLabels.forEach { label ->
            composeTestRule.onNodeWithText(label).performScrollTo().assertIsDisplayed()
        }
    }

    @Test
    fun imeHidesAgain_resumesTrackingTheLiveHeight() {
        val rowsSeen = mutableListOf<UInt>()
        val heightState = mutableStateOf(fullHeightDp)
        val imeState = mutableStateOf(false)
        setImeAwareScreen(heightState, imeState) { _, rows -> rowsSeen.add(rows) }
        composeTestRule.waitForIdle()

        composeTestRule.runOnIdle {
            imeState.value = true
            heightState.value = shrunkHeightDp
        }
        composeTestRule.waitForIdle()
        assertEquals("凍結中は追加callが無いはず", 1, rowsSeen.size)

        // IMEが閉じ、実測ビューポートが(元の全画面とも縮んだ値とも異なる)新しい高さへ変わる
        // ——回転等による本当のサイズ変化が同時に起きたケースを模す。
        val resumedHeightDp = 500.dp
        composeTestRule.runOnIdle {
            imeState.value = false
            heightState.value = resumedHeightDp
        }
        composeTestRule.waitForIdle()

        assertTrue(
            "IME非表示に戻ったら追随が再開し、新しいonResize呼び出しが発生するはず",
            rowsSeen.size > 1,
        )
        assertTrue(
            "新しい実測高さに基づくrows数は、縮小時のrows数と異なるはず",
            rowsSeen.last() != rowsSeen[rowsSeen.size - 2],
        )
    }
}
