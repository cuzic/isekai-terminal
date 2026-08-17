package tools.isekai.terminal.ui

import org.junit.Assert.assertEquals
import org.junit.Test

/**
 * [visibleRowRange]の`cellH`引数に渡す、実描画側([TerminalScreen.kt]の
 * `renderCellDims.second`)と同じ計算式`effectiveCanvasHeightPx(stableHeightPx,
 * liveHeightPx) / rows`をテストコードから使うための共有ヘルパー(コードレビュー指摘:
 * [TerminalResizeTest]と[tools.isekai.terminal.TerminalImeLayoutTest]の両方に同じ式が
 * 重複していたのをここへ集約)。`internal`にして両テストファイル(同じ`android/src/test`
 * ソースセット)から参照できるようにする。
 */
internal fun testCellH(stableHeightPx: Float, liveHeightPx: Float, rows: Int): Float =
    effectiveCanvasHeightPx(stableHeightPx, liveHeightPx) / rows

class TerminalResizeTest {

    // ── advanceResizeStability ───────────────────────────────────────

    @Test
    fun `IME not visible tracks the live height`() {
        val initial = ResizeStabilityState(hasObservedImeClosed = true, stableHeightPx = 999f, lastWidthPx = 400f)
        val next = advanceResizeStability(initial, isImeVisible = false, liveHeightPx = 480f, liveWidthPx = 400f)
        assertEquals(480f, next.stableHeightPx)
        assertEquals(true, next.hasObservedImeClosed)
    }

    @Test
    fun `IME visible freezes the previous stable height, ignoring the shrunk live height`() {
        // IME表示中はliveHeightPxが縮んでいても、以前の(IME非表示時点の)安定値を返し続ける。
        val initial = ResizeStabilityState(hasObservedImeClosed = true, stableHeightPx = 480f, lastWidthPx = 400f)
        val next = advanceResizeStability(initial, isImeVisible = true, liveHeightPx = 280f, liveWidthPx = 400f)
        assertEquals(480f, next.stableHeightPx)
    }

    @Test
    fun `IME closing then reopening tracks correctly across a full cycle`() {
        var state = ResizeStabilityState(hasObservedImeClosed = true, stableHeightPx = 480f, lastWidthPx = 400f)
        // 1. IME非表示: heightPx=480がそのまま安定値になる
        state = advanceResizeStability(state, isImeVisible = false, liveHeightPx = 480f, liveWidthPx = 400f)
        assertEquals(480f, state.stableHeightPx)
        // 2. IME表示: heightPxが280に縮むが、安定値は480のまま凍結される
        state = advanceResizeStability(state, isImeVisible = true, liveHeightPx = 280f, liveWidthPx = 400f)
        assertEquals(480f, state.stableHeightPx)
        // 3. IME非表示に戻る: heightPxが480に復元され、安定値もそれに追随する
        state = advanceResizeStability(state, isImeVisible = false, liveHeightPx = 480f, liveWidthPx = 400f)
        assertEquals(480f, state.stableHeightPx)
    }

    @Test
    fun `genuine rotation while IME is closed is tracked immediately`() {
        // 縦→横回転(IME非表示のまま)は即座に反映される。
        val initial = ResizeStabilityState(hasObservedImeClosed = true, stableHeightPx = 800f, lastWidthPx = 480f)
        val next = advanceResizeStability(initial, isImeVisible = false, liveHeightPx = 480f, liveWidthPx = 800f)
        assertEquals(480f, next.stableHeightPx)
    }

    @Test
    fun `rotation while IME is visible unfreezes immediately via the width signal`() {
        // IME表示中(=SshTerminalCanvasの描画高さもstableHeightPxに固定されている)に
        // 回転が起きた場合、高さだけを見ていると凍結されたままズレ続けてしまう。幅は
        // IMEの影響を受けないため、幅の変化を「本当のサイズ変化が起きた」signalとして
        // 使い、IME表示中でも即座に凍結を解除する。
        val initial = ResizeStabilityState(hasObservedImeClosed = true, stableHeightPx = 800f, lastWidthPx = 480f)
        val next = advanceResizeStability(initial, isImeVisible = true, liveHeightPx = 240f, liveWidthPx = 800f)
        assertEquals(240f, next.stableHeightPx)
        assertEquals(800f, next.lastWidthPx)
        // hasObservedImeClosedもfalseへ戻る: このサイズ変化より前の基準はもう無効なので、
        // 次に本当にIMEが閉じて新しい基準が確立するまでは「まだ信頼できる凍結基準が
        // 無い」状態として素直にliveHeightPxを追随し続ける必要がある。
        assertEquals(false, next.hasObservedImeClosed)
    }

    @Test
    fun `height growing without a width change does not unfreeze (known gap, Opus review)`() {
        // 上部バーの自動非表示や分割ペインの縦リサイズ等、幅は変わらず高さだけが増える
        // ケースは、あえて凍結解除のtriggerに含めない。`.imePadding()`との相互作用で
        // navigation barのinsetが一時的に0扱いになる端末があり(TerminalResize.ktの
        // advanceResizeStabilityドキュメント参照)、それだけでliveHeightPxが凍結値を
        // 一時的に上回ってしまう。高さ側もtriggerにすると、その1フレームの揺れだけで
        // IME表示セッション全体が「まだ基準が無い」状態に落ちてしまう(Opusレビュー指摘)。
        // このケース自体は、[TerminalScreen.kt]側が`effectiveCanvasHeightPx`
        // (`max(stableHeightPx, liveHeightPx)`)を使って描画の空白だけを防ぎ、tty側の
        // 凍結状態(=cols/rows)はIMEが実際に閉じるまで据え置く。
        val initial = ResizeStabilityState(hasObservedImeClosed = true, stableHeightPx = 280f, lastWidthPx = 400f)
        val next = advanceResizeStability(initial, isImeVisible = true, liveHeightPx = 320f, liveWidthPx = 400f)
        assertEquals(280f, next.stableHeightPx)
        assertEquals(true, next.hasObservedImeClosed)
    }

    // ── effectiveCanvasHeightPx ───────────────────────────────────────

    @Test
    fun `effectiveCanvasHeightPx uses the frozen height when the viewport is not taller`() {
        assertEquals(480f, effectiveCanvasHeightPx(stableHeightPx = 480f, liveHeightPx = 280f))
        assertEquals(480f, effectiveCanvasHeightPx(stableHeightPx = 480f, liveHeightPx = 480f))
    }

    @Test
    fun `effectiveCanvasHeightPx tracks the live height when it exceeds the frozen value`() {
        // advanceResizeStabilityが凍結解除しない一時的な高さ増加(nav bar insetの
        // 計算揺れ・幅を伴わない縦方向のみのリサイズ)の間も、描画側だけはビューポートより
        // 低くならないようにする(上端に空白が出ない)。
        assertEquals(320f, effectiveCanvasHeightPx(stableHeightPx = 280f, liveHeightPx = 320f))
    }

    @Test
    fun `IME visible with unchanged width keeps freezing as before`() {
        // 幅が変わっていない通常のIME開閉では、従来通り凍結し続ける(回帰防止)。
        val initial = ResizeStabilityState(hasObservedImeClosed = true, stableHeightPx = 800f, lastWidthPx = 480f)
        val next = advanceResizeStability(initial, isImeVisible = true, liveHeightPx = 500f, liveWidthPx = 480f)
        assertEquals(800f, next.stableHeightPx)
    }

    @Test
    fun `rotation while IME is visible re-establishes a fresh baseline once IME actually closes`() {
        // タスク#19の後追い修正で見つかった回帰の再発防止テスト: 回転直後に単純に
        // liveHeightPxを採用するだけだと、その後IMEが閉じたときに「回転より前の
        // (もう無効な)stableHeightPx」へ凍結してしまい、IMEを閉じた瞬間に不要な
        // resizeが飛ぶ。hasObservedImeClosedをfalseへ戻すことで、IMEが実際に閉じた
        // 時点の値を新しい基準として正しく再確立できる。
        var state = ResizeStabilityState(hasObservedImeClosed = true, stableHeightPx = 800f, lastWidthPx = 480f)
        // 1. IME表示中に回転(縦→横): 幅が変わり、即座にliveHeightPxへ追随する。
        state = advanceResizeStability(state, isImeVisible = true, liveHeightPx = 240f, liveWidthPx = 800f)
        assertEquals(240f, state.stableHeightPx)
        // 2. 回転後、IMEが実際に閉じる: 新しい向きでの正しい全高が新基準になる。
        state = advanceResizeStability(state, isImeVisible = false, liveHeightPx = 480f, liveWidthPx = 800f)
        assertEquals(480f, state.stableHeightPx)
        assertEquals(true, state.hasObservedImeClosed)
        // 3. IMEが再度表示される: 「回転より前」の800fではなく、新基準480fへ正しく凍結される。
        state = advanceResizeStability(state, isImeVisible = true, liveHeightPx = 240f, liveWidthPx = 800f)
        assertEquals(480f, state.stableHeightPx)
    }

    @Test
    fun `first composition while IME is already visible tracks the live height until IME is observed closed once`() {
        // タブがアクティブ化された直後など、この状態機械が初めて評価される時点で既に
        // IMEが表示中のケース(Codexレビュー指摘、タスク#19)。「凍結すべき正しい基準値」が
        // まだ無いため、hasObservedImeClosed=falseの間は素直にliveHeightPxを追随する。
        var state = ResizeStabilityState(hasObservedImeClosed = false, stableHeightPx = 280f, lastWidthPx = 400f)
        state = advanceResizeStability(state, isImeVisible = true, liveHeightPx = 280f, liveWidthPx = 400f)
        assertEquals(280f, state.stableHeightPx)
        assertEquals(false, state.hasObservedImeClosed)

        // さらにIMEが表示されたまま高さが変わっても(端末回転等)、まだ基準が無いので追随する。
        state = advanceResizeStability(state, isImeVisible = true, liveHeightPx = 250f, liveWidthPx = 400f)
        assertEquals(250f, state.stableHeightPx)
        assertEquals(false, state.hasObservedImeClosed)

        // 一度でもIMEが非表示になれば、以降は通常通り安定化が始まる。
        state = advanceResizeStability(state, isImeVisible = false, liveHeightPx = 480f, liveWidthPx = 400f)
        assertEquals(480f, state.stableHeightPx)
        assertEquals(true, state.hasObservedImeClosed)

        state = advanceResizeStability(state, isImeVisible = true, liveHeightPx = 280f, liveWidthPx = 400f)
        assertEquals(480f, state.stableHeightPx)
    }

    // ── visibleRowRange / isCursorRowVisible ─────────────────────────

    @Test
    fun `PR64 reproduction- fresh empty buffer prompt at row 0 is pushed off-screen when IME opens`() {
        // PR#64(コミット6673e34f)の実機再現配置: rows=24、凍結高さ=全画面(1000px)、
        // IME表示でliveHeightPxが45%(450px)まで縮む。cellHは
        // effectiveCanvasHeightPx/rows(=stableHeightPx/rows、キャンバスの実描画セル高)。
        // これは修正済みの挙動ではなく「手動Resizeボタンが唯一の脱出口」という
        // 既知の制約そのものを回帰確認として明示的にアサートする(TerminalScreen.kt
        // のドロワー「Resize」ボタン、TerminalImeLayoutTest参照)。
        val rows = 24
        val stableHeightPx = 1000f
        val liveHeightPx = stableHeightPx * 0.45f
        val cellH = testCellH(stableHeightPx, liveHeightPx, rows)

        val visible = visibleRowRange(stableHeightPx, liveHeightPx, cellH, rows)

        // 接続直後の空バッファのプロンプトはrow 0(グリッド最上段)にある。
        val cursorRow = 0
        assertEquals(
            "既知の制約: 接続直後の空バッファでIMEを開くとプロンプト行(row 0)は" +
                "画面外にクリップされる。脱出口は補助ドロワーのResizeボタン(TerminalScreen.kt)。",
            false,
            isCursorRowVisible(cursorRow, visible),
        )
    }

    @Test
    fun `visibleRowRange shows only the bottom rows when the frozen canvas is taller than the viewport`() {
        val visible = visibleRowRange(stableHeightPx = 480f, liveHeightPx = 240f, cellH = 20f, rows = 24)
        // liveHeightPx(240) / cellH(20) = 12行だけが、下端揃えのため下側12行(row 12..23)に見える。
        assertEquals(12..23, visible)
        assertEquals(false, isCursorRowVisible(0, visible))
        assertEquals(true, isCursorRowVisible(23, visible))
        assertEquals(true, isCursorRowVisible(12, visible))
        assertEquals(false, isCursorRowVisible(11, visible))
    }

    @Test
    fun `visibleRowRange shows all rows when the viewport is not shrunk (IME closed)`() {
        val visible = visibleRowRange(stableHeightPx = 480f, liveHeightPx = 480f, cellH = 20f, rows = 24)
        assertEquals(0..23, visible)
        assertEquals(true, isCursorRowVisible(0, visible))
        assertEquals(true, isCursorRowVisible(23, visible))
    }

    @Test
    fun `visibleRowRange shows all rows when the live viewport exceeds the frozen height`() {
        // effectiveCanvasHeightPxがliveHeightPxを採用するケース(Opusレビュー指摘の
        // 一時的な高さ増加)ではクリップは発生しない。
        val visible = visibleRowRange(stableHeightPx = 280f, liveHeightPx = 320f, cellH = 320f / 24, rows = 24)
        assertEquals(0..23, visible)
    }

    @Test
    fun `visibleRowRange returns an empty range for non-positive rows or cellH`() {
        assertEquals(IntRange.EMPTY, visibleRowRange(480f, 240f, 20f, rows = 0))
        assertEquals(IntRange.EMPTY, visibleRowRange(480f, 240f, cellH = 0f, rows = 24))
    }

    // ── computeResizeTargetColsRows ──────────────────────────────────

    @Test
    fun `simple pixel division`() {
        val (cols, rows) = computeResizeTargetColsRows(widthPx = 800f, heightPx = 480f, cellW = 10f, cellH = 20f)
        assertEquals(80, cols)
        assertEquals(24, rows)
    }

    @Test
    fun `genuine height change from rotation changes rows`() {
        val portrait = computeResizeTargetColsRows(widthPx = 480f, heightPx = 800f, cellW = 10f, cellH = 20f)
        val landscape = computeResizeTargetColsRows(widthPx = 800f, heightPx = 480f, cellW = 10f, cellH = 20f)
        assertEquals(Pair(48, 40), portrait)
        assertEquals(Pair(80, 24), landscape)
    }

    @Test
    fun `pinch zoom changing cell size changes cols and rows`() {
        val normal = computeResizeTargetColsRows(widthPx = 800f, heightPx = 480f, cellW = 10f, cellH = 20f)
        // ピンチズームでフォントが拡大 → セルサイズが大きくなる → cols/rowsは減る。
        val zoomedIn = computeResizeTargetColsRows(widthPx = 800f, heightPx = 480f, cellW = 20f, cellH = 40f)
        assertEquals(Pair(80, 24), normal)
        assertEquals(Pair(40, 12), zoomedIn)
    }

    @Test
    fun `result is clamped to configured minimums`() {
        val (cols, rows) = computeResizeTargetColsRows(widthPx = 5f, heightPx = 5f, cellW = 10f, cellH = 20f)
        assertEquals(10, cols)
        assertEquals(5, rows)
    }

    @Test
    fun `custom minimums are respected`() {
        val (cols, rows) = computeResizeTargetColsRows(
            widthPx = 5f, heightPx = 5f, cellW = 10f, cellH = 20f, minCols = 20, minRows = 8,
        )
        assertEquals(20, cols)
        assertEquals(8, rows)
    }

    // ── combined: advanceResizeStability feeding into computeResizeTargetColsRows ────

    @Test
    fun `IME opening does not change the resize target (task 19 regression guard)`() {
        val cellW = 10f
        val cellH = 20f
        val steady = ResizeStabilityState(hasObservedImeClosed = true, stableHeightPx = 480f, lastWidthPx = 800f)

        val closed = computeResizeTargetColsRows(
            widthPx = 800f,
            heightPx = advanceResizeStability(steady, isImeVisible = false, liveHeightPx = 480f, liveWidthPx = 800f).stableHeightPx,
            cellW = cellW, cellH = cellH,
        )
        val open = computeResizeTargetColsRows(
            widthPx = 800f,
            heightPx = advanceResizeStability(steady, isImeVisible = true, liveHeightPx = 280f, liveWidthPx = 800f).stableHeightPx,
            cellW = cellW, cellH = cellH,
        )
        assertEquals(closed, open)
    }
}
