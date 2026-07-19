package tools.isekai.terminal.ui

/**
 * [computeResizeTargetColsRows]へ渡す「安定した高さ」の追跡状態。[TerminalScreen.kt]の
 * `BoxWithConstraints`内で`remember`され、フレームごとに[advanceResizeStability]で
 * 更新される(タスク#19)。
 */
data class ResizeStabilityState(
    /** これまでに一度でもIME非表示状態を観測したか([advanceResizeStability]のdoc参照)。 */
    val hasObservedImeClosed: Boolean,
    /** resize要求(cols/rows算出)に使う、IME開閉の影響を打ち消した高さ(px)。 */
    val stableHeightPx: Float,
)

/**
 * ソフトキーボード(IME)表示中はビューポートの実測高さ([liveHeightPx])が
 * `.imePadding()`分だけ縮むが、tty(Rust側`SessionCore::resize`)へ要求するcols/rowsの
 * 基準にはIMEが閉じていた時点の高さを使い続けたい(タスク#19: IME開閉のたびに
 * 不要なresize=SIGWINCH相当がvim等の実行中プログラムへ飛ぶのを防ぐ)。
 *
 * 当初は`heightPx + WindowInsets.ime.getBottom(density)`のように生のIME insetを
 * 足し戻して補正する実装だったが、`.navigationBarsPadding()`との相互作用(IME表示中に
 * navigation barのinsetが0扱いになる端末・OSバージョンがある)により正確な打ち消しが
 * 保証できない(Codexレビュー指摘、タスク#19)。そのため生のinset値を計算に使わず、
 * 「IMEが非表示の間だけ最新の高さを採用し、表示中は直近に非表示だった時点の値を
 * 凍結して使い続ける」方式にする。
 *
 * [hasObservedImeClosed]が false の間(=タブがアクティブ化された直後など、この
 * `BoxWithConstraints`がIME表示中に初めてcompositionされ、まだ一度もIME非表示状態を
 * 観測していない間)は「凍結すべき正しい基準値」がまだ存在しないため、素直に
 * `liveHeightPx`を採用し続ける(=このタスク以前と同じ挙動。一度でもIMEが閉じれば
 * それ以降は正しく安定化される。Codexレビュー指摘、タスク#19: 初回composition時に
 * IMEが既に表示中のケースへの対応)。
 *
 * 回転や実ウィンドウサイズ変化・ピンチズーム(cellW/cellHの変化)による本当の
 * サイズ変化は、IMEが非表示である限りそのまま`liveHeightPx`に反映されて追随する
 * (IME表示中に回転が起きた場合は、IMEが閉じるまで反映が遅れる既知のトレードオフ
 * ——回転そのものは表示ビューポートには即座に反映されるため実害は小さい)。
 */
fun advanceResizeStability(
    previous: ResizeStabilityState,
    isImeVisible: Boolean,
    liveHeightPx: Float,
): ResizeStabilityState {
    val hasObservedImeClosed = previous.hasObservedImeClosed || !isImeVisible
    val stableHeightPx = if (!hasObservedImeClosed || !isImeVisible) liveHeightPx else previous.stableHeightPx
    return ResizeStabilityState(hasObservedImeClosed, stableHeightPx)
}

/**
 * ビューポート寸法とセルサイズから、tty(Rust側`SessionCore::resize`)へ要求する
 * cols/rows を計算する(タスク#19)。[heightPx]には呼び出し側が
 * [advanceResizeStability]等で解決した「IME開閉の影響を除いた安定した高さ」を渡す
 * 責務を持つ——この関数自体はIMEを一切意識しない単純な pixel/cellサイズ の除算+
 * 下限クランプのみを行う。
 */
fun computeResizeTargetColsRows(
    widthPx: Float,
    heightPx: Float,
    cellW: Float,
    cellH: Float,
    minCols: Int = 10,
    minRows: Int = 5,
): Pair<Int, Int> {
    val cols = (widthPx / cellW).toInt().coerceAtLeast(minCols)
    val rows = (heightPx / cellH).toInt().coerceAtLeast(minRows)
    return Pair(cols, rows)
}
