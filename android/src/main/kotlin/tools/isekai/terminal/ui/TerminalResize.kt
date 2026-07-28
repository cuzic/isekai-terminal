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
    /**
     * 直近に観測した幅(px)。幅はIMEの影響を受けないため、前回からの変化は回転・分割
     * ペインのリサイズ等「本当のサイズ変化」の signal として使う([advanceResizeStability]
     * のdoc参照)。
     */
    val lastWidthPx: Float,
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
 * 回転や実ウィンドウサイズ変化による本当のサイズ変化は、IMEが非表示である限り
 * そのまま`liveHeightPx`/`liveWidthPx`に反映されて追随する(ピンチズームは
 * `liveWidthPx`/`liveHeightPx`自体を変えず`cellW`/`cellH`だけを変えるため、この
 * 関数の追随対象ではなく[computeResizeTargetColsRows]呼び出し側で直接反映される)。
 *
 * IME表示中に本当の横幅変化が起きた場合(回転・分割ペインのリサイズなど、[liveWidthPx]
 * ——IMEの影響を受けない値——が前回観測時から変わった場合)は例外的に即座に反映する。
 * この判定を「凍結値[ResizeStabilityState.stableHeightPx]より[liveHeightPx]が大きい」
 * という高さ側の条件でも行うことは意図的に避けている(Opusレビュー指摘): 端末・OS
 * バージョンによっては`.navigationBarsPadding()`との相互作用でIME表示アニメーション中に
 * navigation barのinsetが一時的に0扱いになることがあり(上のコメント参照)、その1フレームだけ
 * `liveHeightPx`が凍結値を上回ってしまう。高さ側もtriggerに含めると、この一時的な
 * inset計算の乱れだけで[hasObservedImeClosed]が`false`へ戻り、そのIME表示セッション
 * 全体が「まだ基準が無い」状態に落ちて`liveHeightPx`を追随し続けてしまう(=このPRが
 * 直そうとしているフォント潰れ症状と、tty側への不要なresizeの両方が復活する)。
 * 高さが一時的に凍結値を超えるケース自体は、[ResizeStabilityState.stableHeightPx]を
 * そのまま使わず呼び出し側([TerminalScreen.kt]の`.height()`/`renderCellDims`)が
 * `max(stableHeightPx, liveHeightPx)`を使うことで「凍結値を下回ることはない」を
 * 保ちながら吸収する(=描画上の空白は出ないが、状態自体は書き換えない)。
 *
 * このとき単に`liveHeightPx`を採用するだけでなく[hasObservedImeClosed]も`false`へ
 * 戻す——「まだ信頼できる凍結基準が無い」状態に戻すことで、次に本当にIMEが閉じて
 * 新しい基準が確立するまでは素直に`liveHeightPx`を追随し続ける。戻さないと、この
 * サイズ変化より前の(もう無効な)値へ次のIME開閉時に凍結してしまい、IMEを閉じた
 * 瞬間に改めてptyへresizeが飛ぶ——タスク#19が抑止したかった「IME開閉のたびの
 * 不要なresize」が、サイズ変化の直後だけ部分的に復活してしまう。
 *
 * 幅が変わらない縦方向のみの本当のサイズ変化(IME表示中の上部バー自動非表示・分割
 * ペインの縦リサイズ等)は、この関数では検知しない既知のギャップ(Opusレビュー指摘)。
 * 幅を伴わない高さ増加をtriggerに使うと上記の理由で誤検知するため、あえて対象外にした
 * ——このケースはIMEが閉じた時点で自己修復する。
 */
fun advanceResizeStability(
    previous: ResizeStabilityState,
    isImeVisible: Boolean,
    liveHeightPx: Float,
    liveWidthPx: Float,
): ResizeStabilityState {
    val realSizeChange = liveWidthPx != previous.lastWidthPx
    val hasObservedImeClosed = if (realSizeChange) !isImeVisible else (previous.hasObservedImeClosed || !isImeVisible)
    val stableHeightPx = if (!hasObservedImeClosed || !isImeVisible) liveHeightPx else previous.stableHeightPx
    return ResizeStabilityState(hasObservedImeClosed, stableHeightPx, liveWidthPx)
}

/**
 * [SshTerminalCanvas]の描画高さ・[TerminalScreen.kt]の`renderCellDims`に使う実効的な
 * 高さ(px)。[ResizeStabilityState.stableHeightPx](IME開閉の影響を除いた凍結値)を
 * そのまま使うと、[advanceResizeStability]が検知しない一時的な高さの増加
 * (nav bar insetの計算揺れ・幅を伴わない縦方向のみの本当のサイズ変化)の間、
 * ビューポートより低く描画されて上端に空白が出てしまう。[liveHeightPx]との
 * `max`を取ることで、凍結状態(=tty側cols/rowsへの反映)はそのままに、描画だけは
 * 「ビューポートより低くならない」を常に保証する。
 */
fun effectiveCanvasHeightPx(stableHeightPx: Float, liveHeightPx: Float): Float =
    stableHeightPx.coerceAtLeast(liveHeightPx)

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
