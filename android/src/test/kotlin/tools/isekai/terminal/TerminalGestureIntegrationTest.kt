package tools.isekai.terminal

import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.size
import androidx.compose.ui.Modifier
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.semantics.SemanticsActions
import androidx.compose.ui.test.ExperimentalTestApi
import androidx.compose.ui.test.ScrollWheel
import androidx.compose.ui.test.assertIsDisplayed
import androidx.compose.ui.test.junit4.createComposeRule
import androidx.compose.ui.test.onNodeWithTag
import androidx.compose.ui.test.onNodeWithText
import androidx.compose.ui.test.performMouseInput
import androidx.compose.ui.test.performScrollTo
import androidx.compose.ui.test.performSemanticsAction
import androidx.compose.ui.test.performTouchInput
import androidx.compose.ui.test.swipeUp
import androidx.compose.ui.unit.dp
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Rule
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config
import uniffi.isekai_terminal_core.CursorShape
import uniffi.isekai_terminal_core.MouseButton
import uniffi.isekai_terminal_core.MouseEventKind
import uniffi.isekai_terminal_core.MouseReportingMode
import uniffi.isekai_terminal_core.NotifyKind
import uniffi.isekai_terminal_core.PanelKind
import uniffi.isekai_terminal_core.ScreenUpdate
import uniffi.isekai_terminal_core.TerminalKeyModifiers

/**
 * 項目6 Tier 3(ジェスチャー統合テスト): [tools.isekai.terminal.ui.MouseGestureArbiter]の
 * ピュア関数側は`MouseGestureArbiterTest`が守るが、それらの関数が実際に
 * `TerminalScreenBody`のCanvasジェスチャーハンドラから正しい順序・引数で呼ばれているか
 * (Compose境界での配線)は原理的に検出できない。ここでは`TerminalScreenBodyTest.kt`と
 * 同じ書式(`createComposeRule()` + `performTouchInput`)で、実際にマウス
 * press/drag/release・同一セル内motionの重複排除・2本指によるピンチへの引き継ぎ・
 * scrollback表示中の送出抑止・補助ドロワーの上スワイプ表示・ホイールスクロールを検証する。
 *
 * `pointerEventEncoderOverride`(テスト用シーム、`TerminalScreenBody`のdocstring参照)で
 * `terminalPointerEventBytes`(UniFFI経由のRustネイティブ呼び出し、`android/src/test`から
 * ロード不能)を差し替える。実際のSGR/legacy X10エンコードの正しさ自体は
 * rust-core側`terminal.rs::encode_pointer_event_bytes`のユニットテストに委ねる。
 */
@OptIn(ExperimentalTestApi::class)
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [28])
class TerminalGestureIntegrationTest {
    @get:Rule val composeTestRule = createComposeRule()

    private val viewportWidthDp = 400.dp
    private val viewportHeightDp = 300.dp

    /** [pointerEventEncoderOverride]へ渡す1回分の呼び出しの記録。 */
    private data class RecordedPointerCall(
        val kind: MouseEventKind,
        val button: MouseButton?,
    )

    // ── terminalPointerEventBytesのRust実装(terminal.rs::encode_pointer_event_bytes、
    // mouse_button_base_code、mouse_modifier_bits)を、テスト用シーム経由で差し替えるための
    // 参照実装。SGR形式(CSI < Cb ; Cx ; Cy M/m)のみこのテストでは使用する。 ──

    private fun mouseButtonBaseCode(button: MouseButton?): Int = when (button) {
        MouseButton.LEFT -> 0
        MouseButton.MIDDLE -> 1
        MouseButton.RIGHT -> 2
        null -> 3
        MouseButton.WHEEL_UP -> 64
        MouseButton.WHEEL_DOWN -> 65
        MouseButton.WHEEL_LEFT -> 66
        MouseButton.WHEEL_RIGHT -> 67
    }

    private fun mouseModifierBits(m: TerminalKeyModifiers): Int =
        (if (m.shift) 4 else 0) or (if (m.alt) 8 else 0) or (if (m.ctrl) 16 else 0)

    private fun referenceEncode(
        calls: MutableList<RecordedPointerCall>,
        kind: MouseEventKind,
        button: MouseButton?,
        row: UInt,
        col: UInt,
        modifiers: TerminalKeyModifiers,
        cols: UInt,
        rows: UInt,
        mode: MouseReportingMode,
        sgr: Boolean,
        @Suppress("UNUSED_PARAMETER") urxvt: Boolean,
    ): ByteArray? {
        calls.add(RecordedPointerCall(kind, button))
        val reportable = when (kind) {
            MouseEventKind.PRESS, MouseEventKind.RELEASE -> mode != MouseReportingMode.OFF
            MouseEventKind.MOTION -> when (mode) {
                MouseReportingMode.OFF, MouseReportingMode.NORMAL -> false
                MouseReportingMode.BUTTON_EVENT -> button != null
                MouseReportingMode.ANY_EVENT -> true
            }
        }
        if (!reportable) return null
        val base = mouseButtonBaseCode(button)
        val modifierBits = mouseModifierBits(modifiers)
        val motionBit = if (kind == MouseEventKind.MOTION) 0x20 else 0
        val clampedCol = col.toInt().coerceAtMost((cols.toInt() - 1).coerceAtLeast(0))
        val clampedRow = row.toInt().coerceAtMost((rows.toInt() - 1).coerceAtLeast(0))
        val terminator = if (kind == MouseEventKind.RELEASE) 'm' else 'M'
        require(sgr) { "このテストのハーネスはSGR(?1006)モードのみを想定している" }
        val cb = base + modifierBits + motionBit
        return "[<$cb;${clampedCol + 1};${clampedRow + 1}$terminator".toByteArray(Charsets.US_ASCII)
    }

    /** `CSI < Cb ; Cx ; Cy [Mm]`をパースする。マッチしなければnull。 */
    private data class ParsedSgr(val cb: Int, val col: Int, val row: Int, val isRelease: Boolean)
    private fun parseSgr(bytes: ByteArray): ParsedSgr? {
        val text = String(bytes, Charsets.US_ASCII)
        val m = Regex("""\[<(\d+);(\d+);(\d+)([Mm])""").find(text) ?: return null
        val (cb, col, row, term) = m.destructured
        return ParsedSgr(cb.toInt(), col.toInt(), row.toInt(), term == "m")
    }

    private fun minimalScreenUpdate(
        mouseReportingMode: MouseReportingMode = MouseReportingMode.OFF,
        sgrMouseMode: Boolean = true,
    ) = ScreenUpdate(
        0u, 80u, 24u, emptyList(), 0u, 0u, null, null, null, null, false, false, false,
        mouseReportingMode, sgrMouseMode, false, false, true, 0uL, 0uL, NotifyKind.INFO, "", "",
        0uL, PanelKind.NONE, "", "", emptyList(),
        CursorShape.BLOCK, true, emptyList(),
        emptyList(), 0u, null,
    )

    private fun noopActions(onSend: (ByteArray) -> Unit) = TerminalScreenActions(
        onConnect = {},
        onDisconnect = {},
        onBack = {},
        onSend = onSend,
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

    private fun setScreen(
        mouseReportingMode: MouseReportingMode,
        calls: MutableList<RecordedPointerCall>,
        sentBytes: MutableList<ByteArray>,
        scrollbackLen: Int = 0,
    ) {
        composeTestRule.setContent {
            Box(modifier = Modifier.size(viewportWidthDp, viewportHeightDp)) {
                TerminalScreenBody(
                    uiState = TerminalUiState(
                        connected = true,
                        screenUpdate = minimalScreenUpdate(mouseReportingMode = mouseReportingMode),
                        scrollbackLen = scrollbackLen,
                    ),
                    canReconnect = true,
                    actions = noopActions { bytes -> sentBytes.add(bytes) },
                    isActive = true,
                    hasFocus = true,
                    chromeVisible = false,
                    pointerEventEncoderOverride = { kind, button, row, col, modifiers, cols, rows, mode, sgr, urxvt ->
                        referenceEncode(calls, kind, button, row, col, modifiers, cols, rows, mode, sgr, urxvt)
                    },
                )
            }
        }
        composeTestRule.waitForIdle()
    }

    @Test
    fun mouseReporting_pressDragRelease_sendsWellFormedSgrBytes() {
        val calls = mutableListOf<RecordedPointerCall>()
        val sentBytes = mutableListOf<ByteArray>()
        setScreen(MouseReportingMode.BUTTON_EVENT, calls, sentBytes)

        composeTestRule.onNodeWithTag("terminalCanvas").performTouchInput {
            down(center)
            // 複数セルにまたがる程度の距離を動かし、少なくとも1回はMOTIONを発生させる。
            moveTo(center + Offset(60f, 0f))
            up()
        }
        composeTestRule.waitForIdle()

        assertTrue("press/drag/releaseで最低2回(press+release)は呼ばれるはず", calls.size >= 2)
        assertEquals("最初はPRESSのはず", MouseEventKind.PRESS, calls.first().kind)
        assertEquals("最後はRELEASEのはず", MouseEventKind.RELEASE, calls.last().kind)
        assertTrue("押している間はLEFTボタンのはず", calls.all { it.button == MouseButton.LEFT })

        val parsed = sentBytes.map { requireNotNull(parseSgr(it)) { "SGR形式でパースできるはず: ${String(it)}" } }
        assertEquals("送信したバイト列の件数はcalls(reportableなもの)と一致するはず", calls.size, parsed.size)
        // press: cb=0(LEFT, 修飾無し, motionビット無し)、'M'終端。
        assertEquals(0, parsed.first().cb)
        assertTrue("pressは'M'終端のはず", !parsed.first().isRelease)
        // release: cb=0、'm'終端。
        assertEquals(0, parsed.last().cb)
        assertTrue("releaseは'm'終端のはず", parsed.last().isRelease)
        // 中間(あれば)はMOTION: cb=32(motionビットのみ、LEFTのbase=0)、'M'終端。
        parsed.subList(1, parsed.size - 1).forEach { motion ->
            assertEquals("MOTIONはmotionビット(32)だけが立つはず", 32, motion.cb)
            assertTrue(!motion.isRelease)
        }
    }

    @Test
    fun mouseReporting_microDrag_withinSameCell_doesNotDuplicateMotion() {
        val calls = mutableListOf<RecordedPointerCall>()
        val sentBytes = mutableListOf<ByteArray>()
        setScreen(MouseReportingMode.BUTTON_EVENT, calls, sentBytes)

        composeTestRule.onNodeWithTag("terminalCanvas").performTouchInput {
            down(center)
            // 複数回、1セル(通常十数px以上)よりずっと小さいサブピクセル単位の移動を送る
            // (同一セル内に留まる複数の生ポインタイベントが実際に発生することを保証し、
            // shouldReportMouseMotionによる重複排除を意味のある形で検証する)。
            repeat(5) { i -> moveTo(center + Offset(i * 0.1f, 0f)) }
            up()
        }
        composeTestRule.waitForIdle()

        assertEquals(
            "同一セル内に留まる複数回の微小ドラッグはMOTIONを重複送出しないはず(shouldReportMouseMotion)",
            0,
            calls.count { it.kind == MouseEventKind.MOTION },
        )
        // press + releaseの2回だけ。
        assertEquals(listOf(MouseEventKind.PRESS, MouseEventKind.RELEASE), calls.map { it.kind })
    }

    @Test
    fun mouseReporting_secondFingerDuringDrag_handsOffToPinch() {
        val calls = mutableListOf<RecordedPointerCall>()
        val sentBytes = mutableListOf<ByteArray>()
        setScreen(MouseReportingMode.BUTTON_EVENT, calls, sentBytes)

        composeTestRule.onNodeWithTag("terminalCanvas").performTouchInput {
            down(0, center)
            down(1, center + Offset(40f, 0f))
            moveTo(0, center + Offset(5f, 5f))
            moveTo(1, center + Offset(45f, 5f))
            up(0)
            up(1)
        }
        composeTestRule.waitForIdle()

        // タスク#80: マウスドラッグ中に2本目の指が触れたら、直前のpressに対応するreleaseを
        // 送って打ち切り、以降はピンチ/パン(sendPointerEventを一切呼ばない)へ引き継ぐ。
        assertEquals(
            "press→(2本目の指の検出による)release で打ち切られ、以降のsendPointerEvent呼び出しは無いはず",
            listOf(MouseEventKind.PRESS, MouseEventKind.RELEASE),
            calls.map { it.kind },
        )
    }

    @Test
    fun scrollbackDisplayed_suppressesMouseDispatch() {
        val calls = mutableListOf<RecordedPointerCall>()
        val sentBytes = mutableListOf<ByteArray>()
        setScreen(MouseReportingMode.BUTTON_EVENT, calls, sentBytes, scrollbackLen = 10_000)

        // 補助ドロワーを表示し、「履歴▲」でscrollOffsetを0より大きくする(タスク#89)。
        composeTestRule.onNodeWithTag("terminalCanvas").performTouchInput { swipeUp() }
        composeTestRule.waitForIdle()
        composeTestRule.onNodeWithText("履歴▲").performScrollTo().performSemanticsAction(SemanticsActions.OnClick)
        composeTestRule.waitForIdle()

        // ここまでの操作(上スワイプ検出中の1本指ドラッグ)がmouseモード経由でcallsを
        // 汚しうるため、以降の検証対象だけを見るためにクリアする。
        calls.clear()

        composeTestRule.onNodeWithTag("terminalCanvas").performTouchInput {
            down(center)
            up()
        }
        composeTestRule.waitForIdle()

        assertTrue(
            "scrollback表示中(scrollOffset>0)はisPointerReportingActiveがfalseになり、" +
                "マウスpress/drag/releaseは一切送出されないはず",
            calls.isEmpty(),
        )
    }

    @Test
    fun upwardSwipe_revealsAuxDrawer() {
        val calls = mutableListOf<RecordedPointerCall>()
        val sentBytes = mutableListOf<ByteArray>()
        // マウスレポーティングOFF: shouldRevealAuxDrawer検出は別系統のpointerInputであり
        // マウスモードの影響を受けないため、ここではOFFにしてネイティブ呼び出しの
        // 経路自体を通らないようにする。
        setScreen(MouseReportingMode.OFF, calls, sentBytes)

        composeTestRule.onNodeWithText("⌨").assertDoesNotExist()

        composeTestRule.onNodeWithTag("terminalCanvas").performTouchInput { swipeUp() }
        composeTestRule.waitForIdle()

        composeTestRule.onNodeWithText("⌨").performScrollTo().assertIsDisplayed()
    }

    @Test
    fun wheelScroll_verticalAndHorizontal_sendCorrectlyPairedButtonCodes() {
        val calls = mutableListOf<RecordedPointerCall>()
        val sentBytes = mutableListOf<ByteArray>()
        setScreen(MouseReportingMode.BUTTON_EVENT, calls, sentBytes)

        composeTestRule.onNodeWithTag("terminalCanvas").performMouseInput {
            moveTo(center)
            scroll(1f, ScrollWheel.Vertical)
        }
        composeTestRule.waitForIdle()
        assertEquals(1, calls.size)
        assertEquals(MouseEventKind.PRESS, calls.single().kind)
        val verticalButtonA = calls.single().button
        assertTrue(
            "縦ホイールはWHEEL_UP/WHEEL_DOWNのどちらかのはず",
            verticalButtonA == MouseButton.WHEEL_UP || verticalButtonA == MouseButton.WHEEL_DOWN,
        )
        calls.clear()

        composeTestRule.onNodeWithTag("terminalCanvas").performMouseInput {
            moveTo(center)
            scroll(-1f, ScrollWheel.Vertical)
        }
        composeTestRule.waitForIdle()
        assertEquals(1, calls.size)
        val verticalButtonB = calls.single().button
        assertTrue(
            "正負が逆の縦スクロールは、対になる逆方向のボタン(WHEEL_UP⇔WHEEL_DOWN)を送るはず" +
                "(wheelButtonForDelta、MouseGestureArbiter.kt)",
            (verticalButtonA == MouseButton.WHEEL_UP && verticalButtonB == MouseButton.WHEEL_DOWN) ||
                (verticalButtonA == MouseButton.WHEEL_DOWN && verticalButtonB == MouseButton.WHEEL_UP),
        )
        calls.clear()

        composeTestRule.onNodeWithTag("terminalCanvas").performMouseInput {
            moveTo(center)
            scroll(1f, ScrollWheel.Horizontal)
        }
        composeTestRule.waitForIdle()
        assertEquals(1, calls.size)
        val horizontalButtonA = calls.single().button
        assertTrue(
            "横ホイールはWHEEL_LEFT/WHEEL_RIGHTのどちらかのはず",
            horizontalButtonA == MouseButton.WHEEL_LEFT || horizontalButtonA == MouseButton.WHEEL_RIGHT,
        )
        calls.clear()

        composeTestRule.onNodeWithTag("terminalCanvas").performMouseInput {
            moveTo(center)
            scroll(-1f, ScrollWheel.Horizontal)
        }
        composeTestRule.waitForIdle()
        assertEquals(1, calls.size)
        val horizontalButtonB = calls.single().button
        assertTrue(
            "正負が逆の横スクロールは、対になる逆方向のボタン(WHEEL_LEFT⇔WHEEL_RIGHT)を送るはず" +
                "(wheelButtonForHorizontalDelta、MouseGestureArbiter.kt)",
            (horizontalButtonA == MouseButton.WHEEL_LEFT && horizontalButtonB == MouseButton.WHEEL_RIGHT) ||
                (horizontalButtonA == MouseButton.WHEEL_RIGHT && horizontalButtonB == MouseButton.WHEEL_LEFT),
        )
    }
}
