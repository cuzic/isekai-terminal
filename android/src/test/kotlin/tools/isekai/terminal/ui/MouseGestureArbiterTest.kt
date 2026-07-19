package tools.isekai.terminal.ui

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import uniffi.isekai_terminal_core.MouseButton
import uniffi.isekai_terminal_core.MouseReportingMode

/**
 * タスク#87(fableレビュー・グループD指摘): `TerminalScreen.kt`の`pointerInput`
 * コルーチンに直書きされていたマウスUI裁定ロジック(press/drag/releaseライフサイクル・
 * 2本指中断・scrollOffsetゲート・wheel経路)には単体テストが無かった。
 * `MouseGestureArbiter.kt`へ抽出したピュア関数を、Compose実行環境(Robolectric)
 * なしのプレーンJUnitで検証する。
 */
class MouseGestureArbiterTest {

    // ── isPointerReportingActive ────────────────────────────────────

    @Test
    fun `reporting active when mode is not off and live and not showing scrollback`() {
        assertTrue(
            isPointerReportingActive(
                scrollOffset = 0, showingScrollback = false,
                mouseReportingMode = MouseReportingMode.NORMAL,
            ),
        )
    }

    @Test
    fun `reporting inactive when mode is off`() {
        assertFalse(
            isPointerReportingActive(
                scrollOffset = 0, showingScrollback = false,
                mouseReportingMode = MouseReportingMode.OFF,
            ),
        )
    }

    @Test
    fun `reporting inactive when scrolled into scrollback`() {
        // 過去ログを表示中にライブ側のモードへ従ってポインタイベントを送ると、
        // 表示対象(スクロールバック)と入力対象(ライブセッション)が食い違う。
        assertFalse(
            isPointerReportingActive(
                scrollOffset = 3, showingScrollback = false,
                mouseReportingMode = MouseReportingMode.NORMAL,
            ),
        )
    }

    @Test
    fun `reporting inactive while showingScrollback even if scrollOffset is zero`() {
        // タスク#79: 検索ジャンプでscrollback最新行(row=0)を表示中は、
        // scrollOffset == 0のままでもライブ表示ではない。
        assertFalse(
            isPointerReportingActive(
                scrollOffset = 0, showingScrollback = true,
                mouseReportingMode = MouseReportingMode.NORMAL,
            ),
        )
    }

    // ── shouldUseMouseTouch ──────────────────────────────────────────

    @Test
    fun `uses mouse touch when reporting active and gesture starts with a single finger`() {
        assertTrue(shouldUseMouseTouch(pointerReportingActive = true, initialPointerCount = 1))
    }

    @Test
    fun `does not use mouse touch when reporting inactive`() {
        assertFalse(shouldUseMouseTouch(pointerReportingActive = false, initialPointerCount = 1))
    }

    @Test
    fun `does not use mouse touch when gesture already starts with two fingers`() {
        // ピンチを最優先するため、開始時点で既に2本指ならマウスタッチ経路は使わない。
        assertFalse(shouldUseMouseTouch(pointerReportingActive = true, initialPointerCount = 2))
    }

    // ── decideMouseTouchStep(2本指中断・タスク#80のピンチ引き継ぎ) ──────

    @Test
    fun `continues tracking while the tracked finger stays pressed alone`() {
        assertEquals(
            MouseTouchStep.CONTINUE,
            decideMouseTouchStep(trackedFingerPressed = true, pointerCount = 1),
        )
    }

    @Test
    fun `releases only when the tracked finger lifts without a second finger`() {
        assertEquals(
            MouseTouchStep.RELEASE_ONLY,
            decideMouseTouchStep(trackedFingerPressed = false, pointerCount = 1),
        )
    }

    @Test
    fun `releases and hands off to pinch when a second finger touches down`() {
        // タスク#80の回帰確認: 追跡中の指がまだ押されたままでも、2本目が触れた
        // 時点でreleaseし、同じジェスチャをピンチ/パンへ引き継ぐ。
        assertEquals(
            MouseTouchStep.RELEASE_AND_HANDOFF_TO_PINCH,
            decideMouseTouchStep(trackedFingerPressed = true, pointerCount = 2),
        )
    }

    @Test
    fun `hands off to pinch even if the tracked finger itself lifted at the same time as the second`() {
        // 追跡中の指の状態(離れた/離れていない)に関わらず、pointerCount > 1なら
        // 常にhandoffする(元実装の`handoffToPinch = pointerCount > 1`と対称)。
        assertEquals(
            MouseTouchStep.RELEASE_AND_HANDOFF_TO_PINCH,
            decideMouseTouchStep(trackedFingerPressed = false, pointerCount = 2),
        )
    }

    // ── classifyNormalGesture(長押し/タップ/ピンチの3択) ────────────────

    @Test
    fun `classifies as selection when long press succeeds with a single finger`() {
        assertEquals(
            NormalGestureOutcome.SELECTION,
            classifyNormalGesture(
                longPressSucceeded = true, pointerCount = 1, trackedFingerStillPressed = true,
            ),
        )
    }

    @Test
    fun `classifies as tap when long press fails and the finger already lifted`() {
        assertEquals(
            NormalGestureOutcome.TAP,
            classifyNormalGesture(
                longPressSucceeded = false, pointerCount = 0, trackedFingerStillPressed = false,
            ),
        )
    }

    @Test
    fun `classifies as pinch pan when two or more fingers are pressed even if long press succeeded`() {
        // awaitLongPressOrCancellationは2本指ピンチ中でもタイムアウトで非nullを
        // 返し得るため、実際に押されている指の本数を優先してピンチ扱いにする。
        assertEquals(
            NormalGestureOutcome.PINCH_PAN,
            classifyNormalGesture(
                longPressSucceeded = true, pointerCount = 2, trackedFingerStillPressed = true,
            ),
        )
    }

    @Test
    fun `classifies as pinch pan when long press fails but the finger is still down and moving`() {
        assertEquals(
            NormalGestureOutcome.PINCH_PAN,
            classifyNormalGesture(
                longPressSucceeded = false, pointerCount = 1, trackedFingerStillPressed = true,
            ),
        )
    }

    // ── shouldReportMouseMotion(タスク#88のセル単位dedup) ─────────────

    @Test
    fun `does not report motion when the new cell equals the last reported cell`() {
        // xtermは同一セル内でのマウス移動を重複報告しない。ドラッグ中に指がわずかに
        // 揺れて同じセル内へ戻ってきただけの場合は送信をスキップする。
        assertFalse(shouldReportMouseMotion(CellPos(3, 5), CellPos(3, 5)))
    }

    @Test
    fun `reports motion when the row changes`() {
        assertTrue(shouldReportMouseMotion(CellPos(3, 5), CellPos(4, 5)))
    }

    @Test
    fun `reports motion when the column changes`() {
        assertTrue(shouldReportMouseMotion(CellPos(3, 5), CellPos(3, 6)))
    }

    @Test
    fun `a burst of same-cell motion events after press collapses to a single report`() {
        // codexレビュー指摘: タスク#88の再現条件そのもの——`TerminalScreen.kt`の
        // ドラッグループが実際に行う「lastMotionCellを更新しながら抑止する」逐次処理を
        // ここで模倣し、120Hz相当で同じセル内へ複数回飛んできたMOTIONが1回も送信されず、
        // 実際にセルが変わった時だけ送信されることを検証する。
        val pressCell = CellPos(3, 5)
        val incomingMotionEvents = listOf(
            CellPos(3, 5), CellPos(3, 5), CellPos(3, 5), // pressと同じセル内での揺れ
            CellPos(4, 5), // 実際にセルが変わった
            CellPos(4, 5), CellPos(4, 5), // 新セル内でまた揺れる
        )
        var lastMotionCell = pressCell
        val reportedCells = mutableListOf<CellPos>()
        for (cell in incomingMotionEvents) {
            if (shouldReportMouseMotion(lastMotionCell, cell)) {
                lastMotionCell = cell
                reportedCells.add(cell)
            }
        }
        assertEquals(listOf(CellPos(4, 5)), reportedCells)
    }

    // ── wheelButtonForDelta ──────────────────────────────────────────

    @Test
    fun `zero delta yields no wheel button`() {
        assertNull(wheelButtonForDelta(0f))
    }

    @Test
    fun `positive delta yields wheel down`() {
        assertEquals(MouseButton.WHEEL_DOWN, wheelButtonForDelta(5f))
    }

    @Test
    fun `negative delta yields wheel up`() {
        assertEquals(MouseButton.WHEEL_UP, wheelButtonForDelta(-5f))
    }
}
