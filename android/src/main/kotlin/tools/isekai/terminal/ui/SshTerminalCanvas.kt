package tools.isekai.terminal.ui

import android.graphics.Bitmap
import android.graphics.Paint
import android.graphics.PorterDuff
import android.graphics.PorterDuffXfermode
import android.graphics.Typeface
import androidx.compose.foundation.Canvas
import androidx.compose.foundation.background
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.asImageBitmap
import androidx.compose.ui.graphics.nativeCanvas
import androidx.compose.ui.graphics.toArgb
import androidx.compose.ui.unit.IntOffset
import androidx.compose.ui.unit.IntSize
import kotlin.math.ceil
import kotlinx.coroutines.delay
import tools.isekai.terminal.BuildConfig
import uniffi.isekai_terminal_core.CellData
import uniffi.isekai_terminal_core.CursorShape
import uniffi.isekai_terminal_core.ImagePlacement
import uniffi.isekai_terminal_core.ScreenUpdate
import uniffi.isekai_terminal_core.ScrollbackSearchMatch

/**
 * 1行内で背景色が [themeBgArgb](既定背景)以外の値で連続しているセル区間。
 * 背景の `drawRect` をセル単位ではなく区間単位にまとめてバッチ描画するために使う。
 *
 * 文字(`drawText`)はあえて同様にバッチ化していない: 各セルの `x` 位置はセル幅と
 * フォントの実測グリフ幅が厳密に一致する前提でまとめて描画すると、両者がわずかでも
 * 食い違う場合(このファイルのフォントサイズ調整は "M" の実測幅で近似しているだけで
 * 一致を強制していない)に複数文字目以降の位置が右へ流れてズレる。特に日本語の
 * 全角文字が混じる行で目立ちやすいリスクのため、安全側に倒してセル単位のまま
 * `drawText` している。
 */
internal data class BgRun(val startCol: Int, val endColExclusive: Int, val argb: Int)

/** カーソル描画矩形(ピクセル座標)。[computeCursorRect] の戻り値。 */
internal data class CursorRect(val left: Float, val top: Float, val right: Float, val bottom: Float)

/**
 * タスク#33: DECSCUSR(`CSI Ps SP q`)が選択したカーソル形状([shape]、Rust側`Terminal`
 * が真値を保持、rust-ssot)に応じたカーソル描画矩形をピュアに計算する。`(cx, cy)`は
 * カーソルセル左上のピクセル座標、`cellW`/`cellH`はセル寸法。block はセル全体、
 * underline/bar は最小太さ2px(iOS版`TerminalScreenView.swift`の`switch update.cursorShape`
 * と対称の太さ計算: underlineは`cellH * 0.12`、barは`cellW * 0.15`)を下限に描く。
 */
internal fun computeCursorRect(cx: Float, cy: Float, cellW: Float, cellH: Float, shape: CursorShape): CursorRect =
    when (shape) {
        CursorShape.BLOCK -> CursorRect(cx, cy, cx + cellW, cy + cellH)
        CursorShape.UNDERLINE -> {
            val thickness = (cellH * 0.12f).coerceAtLeast(2f)
            CursorRect(cx, cy + cellH - thickness, cx + cellW, cy + cellH)
        }
        CursorShape.BAR -> {
            val thickness = (cellW * 0.15f).coerceAtLeast(2f)
            CursorRect(cx, cy, cx + thickness, cy + cellH)
        }
    }

/**
 * underline/strikethrough(SGR 4/9)の装飾線描画矩形をピュアに計算する。`Paint.isUnderlineText`/
 * `isStrikeThruText`は、空白のみの文字列に対してRobolectric実描画でも実際には装飾線を描かない
 * ことが確認された(iOS版`NSAttributedString.underlineStyle`が空白セルでCoreTextにより
 * 描画されない実機バグ[コミット`5da238e`]と対称)ため、[computeCursorRect]と同じ手法で
 * Rectを直接計算し呼び出し元が`drawRect`で塗る。`(x, y)`はセル左上のピクセル座標。
 */
internal fun computeLineDecorationRects(
    x: Float,
    y: Float,
    cellW: Float,
    cellH: Float,
    underline: Boolean,
    strikethrough: Boolean,
): List<CursorRect> {
    val thickness = (cellH * 0.08f).coerceAtLeast(1f)
    val rects = mutableListOf<CursorRect>()
    if (underline) {
        rects.add(CursorRect(x, y + cellH - thickness, x + cellW, y + cellH))
    }
    if (strikethrough) {
        val midY = y + cellH * 0.5f
        rects.add(CursorRect(x, midY - thickness / 2f, x + cellW, midY + thickness / 2f))
    }
    return rects
}

/**
 * タスク#66: 検索バーの現在マッチ([match])の描画矩形をピュアに計算する。呼び出し元
 * ([SshTerminalCanvas])は`scrollOffset`が`match.row`と一致する場合にのみ[match]を渡す
 * (=表示中のscrollback合成画面の最終行(`row = rows - 1`)に必ず現れる、`scrollbackCells`
 * の`sb_idx = offset + (rows-1-r)`で`r = rows-1`のとき`sb_idx == offset`になることから
 * 導ける、`session.rs`の`scrollback_cells_orders_oldest_to_newest_top_to_bottom`テスト参照)
 * ため、この関数自体は行位置の判断を行わず矩形計算のみを担う。
 *
 * `match.col`/`match.len`は呼び出し時点のscrollbackスナップショットに基づく値([rows]/[cols]
 * が変化した後の古いマッチ等)であり得るため、`[0, cols]`へクランプする(iOS版
 * `TerminalScreenView.swift`の`min(...)`クランプと対称)。クランプ後に幅が0以下になる
 * (=マッチが完全に画面外にはみ出している)場合は`null`を返し、呼び出し元は描画を
 * スキップしてよい。
 */
internal fun computeSearchHighlightRect(match: ScrollbackSearchMatch, rows: Int, cols: Int, cellW: Float, cellH: Float): CursorRect? {
    val highlightRow = rows - 1
    val startCol = match.col.toInt().coerceIn(0, cols)
    val endCol = (startCol + match.len.toInt()).coerceIn(startCol, cols)
    if (startCol >= endCol) return null
    val y = highlightRow * cellH
    return CursorRect(startCol * cellW, y, endCol * cellW, y + cellH)
}

/**
 * SGR 2(dim)セルの前景色。色そのものを別値へ再計算するのではなく、ARGB の alpha
 * チャンネルを 0.6 倍に下げるだけに留める(iOS版`TerminalScreenView.swift`の
 * `withAlphaComponent(0.6)`と対称)。実際の減光は同じ Bitmap 上に既に描画済みの
 * 背景色との合成で表現される——`bg`側で別の実効色を計算し直す必要は無い。
 */
internal fun dimmedArgb(argb: Int): Int {
    val alpha = ((argb ushr 24) and 0xFF)
    val dimmedAlpha = (alpha * 0.6f).toInt().coerceIn(0, 255)
    return (argb and 0x00FFFFFF) or (dimmedAlpha shl 24)
}

/** [cells] のうち [rowStart] から始まる1行分(長さ [cols])を背景色の連続区間へ分割する。 */
internal fun computeBgRuns(cells: List<CellData>, rowStart: Int, cols: Int, themeBgArgb: Int): List<BgRun> {
    val runs = mutableListOf<BgRun>()
    var runStart = -1
    var runArgb = 0
    for (col in 0 until cols) {
        val bg = cells[rowStart + col].bg.toInt()
        if (bg == themeBgArgb) {
            if (runStart != -1) {
                runs += BgRun(runStart, col, runArgb)
                runStart = -1
            }
        } else if (runStart == -1) {
            runStart = col
            runArgb = bg
        } else if (bg != runArgb) {
            runs += BgRun(runStart, col, runArgb)
            runStart = col
            runArgb = bg
        }
    }
    if (runStart != -1) runs += BgRun(runStart, cols, runArgb)
    return runs
}

/**
 * セル寸法(cellW/cellH)・フォント種別ごとにフォントサイズ・ベースライン計算をキャッシュする。
 * `measureText`/`fontMetrics` は cellW/cellH/typeface が変わらない限り結果が変わらないが、
 * 以前は Canvas が再描画されるたび(＝Rust から ScreenUpdate 通知が来るたび)無条件に
 * 再計算していた。typeface はカスタム端末フォント切り替え時に変わるため、キャッシュキーに
 * 含めないと、フォント変更後も古い cellW/cellH のまま textSize/baseline が再計算されず、
 * 新しい Paint に正しくないサイズが残ってしまう。
 */
internal class FontFitCache {
    private var cellW = -1f
    private var cellH = -1f
    private var typeface: Typeface? = null
    var baseline = 0f
        private set

    /** [cellW]/[cellH]/[typeface] のいずれかが前回計算時と変わっていれば true。 */
    fun needsRefit(cellW: Float, cellH: Float, typeface: Typeface): Boolean =
        this.cellW != cellW || this.cellH != cellH || this.typeface !== typeface

    fun markFit(cellW: Float, cellH: Float, typeface: Typeface, baseline: Float) {
        this.cellW = cellW
        this.cellH = cellH
        this.typeface = typeface
        this.baseline = baseline
    }
}

/**
 * デバッグ専用: dirty_rows に基づく部分再描画([GridRenderPlan.Partial])を無視し、
 * 常に [GridRenderPlan.Full] にフォールバックさせるトグル(タスク#100)。
 *
 * dirty行の見落とし(=一部セルの表示が古いまま固まる)は原因の分かりにくい表示バグに
 * なるため、実機/CIで「常に全画面再描画」の旧経路へすぐ切り戻して新旧比較できるよう
 * 用意する。`BuildConfig.DEBUG` の外(release ビルド)では常に無視され、
 * [GridRenderCache.planRender] の通常の判定に一切影響しない。
 * `android/src/debug/kotlin/.../debug/FaultInjectionReceiver.kt` と同様の
 * adb broadcast 経由のトグルは `DirtyRowDebugReceiver`(debug ソースセット)が担う。
 */
internal object DirtyRowDebugFlags {
    @Volatile
    var forceFullRedraw: Boolean = false
}

/**
 * [GridRenderCache.planRender] が返す、次フレームでグリッド Bitmap をどう更新するかの決定。
 *
 * - [Reuse]: グリッドは前回描画分と同一。Bitmap を一切触らず貼り直すだけでよい。
 * - [Full]: グリッド全体を再描画する(初回・セル寸法/テーマ背景/フォント/blink位相の
 *   いずれかが変化した・`dirty_rows`が`None`=全画面dirtyのいずれか)。
 * - [Partial]: `dirty_rows` が指定した行([rows])だけを既存 Bitmap に部分再描画する。
 *   [rows]以外の行のピクセルは一切触らない(前回描画分をそのまま残す)。空リストは
 *   「グリッドセルに変化なし(非グリッドフィールドだけ変わった等)」を意味し、部分再描画も
 *   実質何もしない。
 */
internal sealed interface GridRenderPlan {
    object Reuse : GridRenderPlan
    object Full : GridRenderPlan
    data class Partial(val rows: List<Int>) : GridRenderPlan
}

/**
 * 直近に描画したセル格子(背景+文字)を off-screen Bitmap にキャッシュする。
 *
 * `update`(UniFFI 生成の `ScreenUpdate`)は可変フィールド(`var`)を持つため Compose の
 * 安定性推論の対象にならず、このコンポーザブルは選択範囲のドラッグ操作や
 * `TerminalUiState` 内の無関係なフィールド変更(接続ステータス文言、エージェント署名
 * 確認ダイアログの表示状態等、複数の独立した UI 状態が1つの `StateFlow` にまとまって
 * いるため起きる)のたびに再実行される。このキャッシュは `update` の参照・セル寸法・
 * テーマ背景色・フォント種別が前回描画時と変わっていなければセル全走査(`drawRect`/
 * `drawText`)をスキップし、キャッシュ済み Bitmap を `drawImage` で貼り直すだけにする。
 *
 * 選択ハイライトとカーソルはこの Bitmap キャッシュの対象外とし、毎フレーム軽量に
 * 描画し直す(選択ハイライトは choice のあった行だけ、カーソルは1セル分の矩形のみ)。
 * こうすることで、カーソル色だけがテーマ間で異なる(背景色は同じ)ケースでもキャッシュキー
 * にカーソル色を含める必要がなくなる。
 */
internal class GridRenderCache {
    var bitmap: Bitmap? = null
    private var renderedUpdate: ScreenUpdate? = null
    private var renderedCellW = -1f
    private var renderedCellH = -1f
    private var renderedThemeBg = 0
    private var renderedTypeface: Typeface? = null
    private var renderedBlinkPhase = false
    /** 前回描画した [ScreenUpdate.updateSeq]。ギャップ検出([planRender])に使う。 */
    private var renderedSeq: UInt = 0u

    /**
     * 次フレームでグリッド Bitmap をどう更新すべきかを決める(タスク#97、行単位の
     * 部分再描画)。
     *
     * - セル寸法・テーマ背景色・フォント種別・blink位相のいずれかが前回描画時から
     *   変わっていれば、`dirty_rows` の内容にかかわらず [GridRenderPlan.Full]
     *   (全画面再描画)。これらは行を跨いだ描画結果(フォントサイズ・背景色・点滅)
     *   に影響するため、変化した行だけを描き直しても他の行が古いままになる。
     *   [blinkPhase] は SGR 5(blink)セルの点滅位相で、位相反転だけでも全走査が
     *   必要になる(位相をキーに含めないと「一度描かれたきり点滅しない」バグになる)。
     * - スタイルが不変で、かつ [update] が前回描画したのと同一インスタンスなら
     *   [GridRenderPlan.Reuse](何もしない)。
     * - スタイルが不変でも、`update.updateSeq` が前回描画した連番の次(wrapping)で
     *   なければ、配信チャネル([TerminalSession] の `Channel.CONFLATED`)が中間の発行を
     *   取りこぼしている。`dirty_rows` は「直前に発行した ScreenUpdate との差分」なので、
     *   取りこぼしが起きると欠落分の変化が載らず表示が化ける——この場合は `dirty_rows` を
     *   信用せず [GridRenderPlan.Full] にフォールバックする(Rust側 `update_seq` 追加の
     *   UI側対応。rust-ssot: `dirty_rows` 計算自体はRust、フレーム取りこぼし判定は
     *   UI/トランスポート固有の知識なのでKotlin側で持つ)。
     * - スタイルが不変で `update.dirtyRows` が `null`(=Rust側が全画面dirtyと判定)なら
     *   [GridRenderPlan.Full]。
     * - スタイルが不変で `dirty_rows` が非nullなら、その行だけを描き直す
     *   [GridRenderPlan.Partial]。画面範囲外の行番号(寸法変化直後の古い損傷レンジ等)は
     *   除外し、行番号は重複排除する。
     */
    fun planRender(
        update: ScreenUpdate,
        cellW: Float,
        cellH: Float,
        themeBgArgb: Int,
        typeface: Typeface,
        blinkPhase: Boolean,
    ): GridRenderPlan {
        val styleChanged = renderedCellW != cellW ||
            renderedCellH != cellH ||
            renderedThemeBg != themeBgArgb ||
            renderedTypeface !== typeface ||
            renderedBlinkPhase != blinkPhase
        if (styleChanged) return GridRenderPlan.Full
        if (renderedUpdate === update) return GridRenderPlan.Reuse
        // デバッグ専用トグル(タスク#100)。内容が不変(Reuse)ならそのまま何もしなくてよいが、
        // 何か描き直す必要がある場合は常に Partial ではなく Full を選ばせる。
        if (BuildConfig.DEBUG && DirtyRowDebugFlags.forceFullRedraw) return GridRenderPlan.Full
        // 配信チャネルでの取りこぼし検出。UInt の加算はモジュラなので wrapping も自動で正しい。
        // (初回は styleChanged 側で必ず Full になるためここには到達せず、renderedSeq は常に有効。)
        if (update.updateSeq != renderedSeq + 1u) return GridRenderPlan.Full
        val damage = update.dirtyRows ?: return GridRenderPlan.Full
        val rowCount = update.rows.toInt()
        val dirtyRowIndices = damage.asSequence()
            .map { it.line.toInt() }
            .filter { it in 0 until rowCount }
            .distinct()
            .toList()
        return GridRenderPlan.Partial(dirtyRowIndices)
    }

    /**
     * グリッド全走査(背景+文字)の再描画が必要かどうか([GridRenderPlan.Reuse] 以外)。
     * [planRender] のBoolean版で、`update`参照/セル寸法/テーマ背景/typeface/blink位相の
     * いずれかが変わったか(または`dirty_rows`が全画面dirty)を判定する薄いラッパー。
     */
    fun needsRerender(
        update: ScreenUpdate,
        cellW: Float,
        cellH: Float,
        themeBgArgb: Int,
        typeface: Typeface,
        blinkPhase: Boolean,
    ): Boolean =
        planRender(update, cellW, cellH, themeBgArgb, typeface, blinkPhase) != GridRenderPlan.Reuse

    fun markRendered(
        update: ScreenUpdate,
        cellW: Float,
        cellH: Float,
        themeBgArgb: Int,
        typeface: Typeface,
        blinkPhase: Boolean,
    ) {
        renderedUpdate = update
        renderedCellW = cellW
        renderedCellH = cellH
        renderedThemeBg = themeBgArgb
        renderedTypeface = typeface
        renderedBlinkPhase = blinkPhase
        renderedSeq = update.updateSeq
    }

    /**
     * 次回の [planRender] を強制的に [GridRenderPlan.Full] にする(Bitmap を再確保した
     * ときに使う)。再確保直後の Bitmap は空(透明)なので、部分再描画ではなく必ず全画面
     * 再描画しなければならない——スタイルキーを無効値へ倒して [planRender] が Full を
     * 返すようにする(`renderedUpdate` のクリアだけでは部分再描画経路に落ちてしまう)。
     */
    fun invalidate() {
        renderedUpdate = null
        renderedCellW = -1f
    }
}

/**
 * Rust側`ImagePlacement.rgba`(RGBA8888、row-major)のバイト列を、Android
 * `Bitmap.setPixels`が要求するパックド`Int`(`0xAARRGGBB`、Android公式契約)の
 * 配列へ変換する。`copyPixelsFromBuffer`のような生バイトコピーとは違い、
 * チャンネル順を明示的に組み立てるため内部バイトレイアウトに依存しない。
 */
internal fun rgbaBytesToArgbInts(rgba: ByteArray): IntArray {
    val pixels = IntArray(rgba.size / 4)
    for (i in pixels.indices) {
        val o = i * 4
        val r = rgba[o].toInt() and 0xFF
        val g = rgba[o + 1].toInt() and 0xFF
        val b = rgba[o + 2].toInt() and 0xFF
        val a = rgba[o + 3].toInt() and 0xFF
        pixels[i] = (a shl 24) or (r shl 16) or (g shl 8) or b
    }
    return pixels
}

/**
 * Sixel(タスク#42)の`ImagePlacement.rgba`から作った`Bitmap`をid単位でキャッシュする。
 * `ScreenUpdate.images`はTerminal(rust-core)側で寿命管理された「現在アクティブな
 * 画像の全リスト」がそのまま渡ってくる(rust-ssot: どの画像がまだ生きているかの
 * 判断はRust側で完結している)ため、この層は判断ロジックを持たず「今回のリストに
 * 無いidのBitmapを捨て、まだキャッシュに無いidだけ新規デコードする」宣言的な
 * 反映のみを行う。
 */
internal class SixelBitmapCache {
    private val cache = mutableMapOf<ULong, Bitmap>()

    /**
     * Rust側`sixel.rs`の`MAX_SIXEL_DIM`/`MAX_SIXEL_AREA`と同じ上限をここでも二重に
     * 適用する。通常経路ではRust側で既に弾かれているはずだが、将来別の画像プロトコル
     * (#53等)が同じ`ImagePlacement`を再利用した場合や、寸法とバッファ長が矛盾する
     * 壊れたデータが来た場合に、巨大`Bitmap`確保やクラッシュへ直結させないための
     * 防御(codexレビュー指摘)。
     */
    private fun isSane(img: ImagePlacement, w: Int, h: Int): Boolean {
        if (w <= 0 || h <= 0) return false
        if (w > 4096 || h > 4096) return false
        if (w.toLong() * h.toLong() > 4_000_000L) return false
        return img.rgba.size.toLong() == w.toLong() * h.toLong() * 4L
    }

    fun bitmapsFor(images: List<ImagePlacement>): Map<ULong, Bitmap> {
        // 画像が0件になった場合(Rust側でclear_images()された等)も必ず呼ばれる想定。
        // liveIdsが空集合になり、retainAllで古いBitmapが全て解放される(codexレビュー指摘:
        // 呼び出し側がisNotEmpty()で早期returnしていると、寿命が尽きた画像のBitmapが
        // 解放されずに残り続けてしまう)。
        val liveIds = images.map { it.id }.toSet()
        cache.keys.retainAll(liveIds)
        for (img in images) {
            if (img.id !in cache) {
                val w = img.widthPx.toInt()
                val h = img.heightPx.toInt()
                if (!isSane(img, w, h)) continue
                // `img.rgba`はRust側(lib.rs `ImagePlacement.rgba`)がRGBA8888バイト順で
                // 詰めたバッファ。`Bitmap.copyPixelsFromBuffer`はバイト順の変換を一切せず
                // 生コピーするだけなので、`Config.ARGB_8888`の内部バイトレイアウトが実機・
                // OSバージョンによってRGBA順と一致しない場合(あるいはRobolectric等の
                // テスト環境)、赤/青チャンネルが入れ替わって描画されてしまう
                // (codex/Fableレビュー指摘)。`setPixels(IntArray, ...)`はAndroidが公式に
                // 契約している`0xAARRGGBB`のパックド`Int`表現を受け取るAPIなので、
                // 内部バイトレイアウトに依存せず正しい色で描画できる。
                val bmp = Bitmap.createBitmap(w, h, Bitmap.Config.ARGB_8888)
                bmp.setPixels(rgbaBytesToArgbInts(img.rgba), 0, w, 0, 0, w, h)
                cache[img.id] = bmp
            }
        }
        return cache
    }
}

/**
 * グリッド1行分(背景run + 文字 + underline/strikethrough装飾)を [canvas] に描画する。
 * [GridRenderCache] の全走査ループ本体を1行単位に切り出したもの——描画結果は
 * 従来のインラインループと完全に一致する(将来のdirty-row最適化で、変化した行だけを
 * 選択的に再描画できるようにするための純粋なリファクタ)。カーソル/選択ハイライトは
 * この経路の対象外(毎フレーム別途描画)である点は従来どおり。
 */
internal fun drawRow(
    canvas: android.graphics.Canvas,
    rowIndex: Int,
    cells: List<CellData>,
    cols: Int,
    cellW: Float,
    cellH: Float,
    baseline: Float,
    themeBgArgb: Int,
    effectiveBlinkPhase: Boolean,
    bgPaint: Paint,
    textPaint: Paint,
    typeface: Typeface,
    italicTypeface: Typeface,
) {
    val y = rowIndex * cellH
    val rowStart = rowIndex * cols

    // 背景(テーマの既定背景以外が連続する区間だけをバッチ描画)
    for (run in computeBgRuns(cells, rowStart, cols, themeBgArgb)) {
        bgPaint.color = run.argb
        canvas.drawRect(run.startCol * cellW, y, run.endColExclusive * cellW, y + cellH, bgPaint)
    }

    // 文字
    for (col in 0 until cols) {
        val cell = cells[rowStart + col]
        // invisible(SGR 8)はグリフを描かない。blink(SGR 5)は点滅位相が
        // 「消灯」側の間だけ同様にグリフを省く(背景は通常通り描く)。
        // reverse(SGR 7)はterminal.rs側でパース時にfg/bgへ実効色として
        // 解決済み(#21)なので、ここではcell.fg/bg をそのまま使うだけでよい。
        // 空白文字自体は本来 drawText 不要だが、underline/strikethrough
        // (SGR 4/9)が立っている空白セルは装飾線だけ描く必要があるため
        // isNotBlank() の早期スキップから除外する(codexレビュー指摘:
        // 装飾のみの空白セルが描かれないと下線/取り消し線が消えてしまう)。
        val blinkHidden = cell.blink && !effectiveBlinkPhase
        val hasLineDecoration = cell.underline || cell.strikethrough
        if (cell.ch.isNotEmpty() && (cell.ch.isNotBlank() || hasLineDecoration) &&
            !cell.invisible && !blinkHidden
        ) {
            val x = col * cellW
            val fgArgb = cell.fg.toInt()
            val resolvedFg = if (cell.dim) dimmedArgb(fgArgb) else fgArgb
            textPaint.color = resolvedFg
            textPaint.isFakeBoldText = cell.bold
            textPaint.typeface = if (cell.italic) italicTypeface else typeface
            canvas.drawText(cell.ch, x, y + baseline, textPaint)

            if (hasLineDecoration) {
                bgPaint.color = resolvedFg
                for (rect in computeLineDecorationRects(x, y, cellW, cellH, cell.underline, cell.strikethrough)) {
                    canvas.drawRect(rect.left, rect.top, rect.right, rect.bottom, bgPaint)
                }
            }
        }
    }
}

/**
 * タスク#97: `dirty_rows` が指定した [rows] だけを、既存の(前フレームを保持している)
 * Bitmap [canvas] に部分再描画する。各行は描画前に**行全幅**([0, bitmapWidthPx))を
 * [clearPaint](PorterDuff CLEAR = 透明化)でクリアしてから [drawRow] で描き直す。
 *
 * 列レンジ(`LineDamage.left`/`right`)ではなく必ず行全幅をクリアするのは、損傷レンジの
 * 外側に前フレームの背景色や、セル幅を超えて右へはみ出したグリフの残骸が残り得るため
 * (列レンジだけを消すと消し残しになる)。[rows]に含まれない行のピクセルは一切触らないので、
 * 変化していない行は前フレームの描画がそのまま保持される。
 */
internal fun redrawDirtyRows(
    canvas: android.graphics.Canvas,
    rows: List<Int>,
    bitmapWidthPx: Int,
    cells: List<CellData>,
    cols: Int,
    cellW: Float,
    cellH: Float,
    baseline: Float,
    themeBgArgb: Int,
    effectiveBlinkPhase: Boolean,
    clearPaint: Paint,
    bgPaint: Paint,
    textPaint: Paint,
    typeface: Typeface,
    italicTypeface: Typeface,
) {
    for (row in rows) {
        val y = row * cellH
        canvas.drawRect(0f, y, bitmapWidthPx.toFloat(), y + cellH, clearPaint)
        drawRow(
            canvas = canvas,
            rowIndex = row,
            cells = cells,
            cols = cols,
            cellW = cellW,
            cellH = cellH,
            baseline = baseline,
            themeBgArgb = themeBgArgb,
            effectiveBlinkPhase = effectiveBlinkPhase,
            bgPaint = bgPaint,
            textPaint = textPaint,
            typeface = typeface,
            italicTypeface = italicTypeface,
        )
    }
}

@Composable
fun SshTerminalCanvas(
    update: ScreenUpdate,
    selection: SelectionRange? = null,
    // タスク#66: 検索バーで現在選択中のマッチ位置(`SessionCore::search_scrollback`、
    // #37が返した`ScrollbackSearchMatch`をそのまま受け取るだけ——マッチ計算自体は
    // 一切行わない、rust-ssot)。呼び出し元([TerminalScreen.kt])が`scrollOffset`と
    // マッチの`row`が一致している間だけ非nullを渡す設計(iOS版
    // `TerminalScreenRepresentable.searchHighlight`と対称)。
    searchHighlight: ScrollbackSearchMatch? = null,
    theme: TerminalTheme = TerminalThemes.DEFAULT_DARK,
    // カスタムフォント([TerminalFontSettings])。未指定時は既定の [Typeface.MONOSPACE]。
    typeface: Typeface = Typeface.MONOSPACE,
    modifier: Modifier = Modifier,
) {
    val textPaint = remember(typeface) {
        Paint().apply {
            isAntiAlias = true
            this.typeface = typeface
        }
    }
    // SGR 3(italic)用のフォントバリアント。ボールドは既存の isFakeBoldText を
    // 引き続き使う(実 Typeface を4種類持つより単純で、iOS版のような
    // BOLD_ITALIC バリアント確保が不要)。typeface が変わったときだけ作り直す。
    val italicTypeface = remember(typeface) { Typeface.create(typeface, Typeface.ITALIC) }
    val bgPaint = remember { Paint() }
    // タスク#97: 部分再描画で1行分を「透明にクリア」するための Paint。PorterDuff CLEAR は
    // 描画先ピクセルを src にかかわらず 0(透明)にするので、全画面再描画の
    // `bmp.eraseColor(TRANSPARENT)` を行スコープに縮めたのと等価になる。
    val clearPaint = remember { Paint().apply { xfermode = PorterDuffXfermode(PorterDuff.Mode.CLEAR) } }
    val cursorPaint = remember { Paint() }
    val selectionPaint = remember {
        Paint().apply { color = android.graphics.Color.argb(120, 255, 255, 255) }
    }
    // タスク#66: 検索マッチのハイライト(黄系、選択範囲の白系と混同しないよう別色にする。
    // iOS版`TerminalScreenView.swift`の`UIColor.systemYellow.withAlphaComponent(0.55)`と対称)。
    val searchHighlightPaint = remember {
        Paint().apply { color = android.graphics.Color.argb(140, 255, 213, 0) }
    }
    val fontFit = remember { FontFitCache() }
    val gridCache = remember { GridRenderCache() }
    val sixelCache = remember { SixelBitmapCache() }

    // blink属性(SGR 5)を持つセルが1つも無ければタイマー自体を回さない(codexレビュー
    // 指摘: 画面にblinkが無くても永続的に再コンポーズ/全セル走査が走っていた)。
    // `update`の参照が変わったときだけ再計算すればよいので、Canvas描画スコープの
    // 外側でremember(update)しておく(iOS版`TerminalScreenView.swift`の
    // `lastDisplayHasBlink`と対称)。
    val hasBlink = remember(update) { update.cells.any { it.blink } }

    // タスク#33: 点滅カーソル(DECSCUSRの偶数パラメータ、あるいはDECSET `?12`)を
    // 実際に点滅させる必要があるかどうか。「点滅させるべきかどうか」自体は
    // `update.cursorBlink`(Rust側`Terminal`が決定した真値、rust-ssot)をそのまま
    // 見るだけ——ここではその値と`cursorVisible`/画面範囲内かどうかを組み合わせて
    // 「点滅タイマーを回す必要があるか」というUI表示専用の判断のみ行う
    // (iOS版`TerminalScreenView.swift`の`lastDisplayCursorBlinks`/`cursorInBounds`と対称)。
    val cursorBlinks = remember(update) {
        update.cursorVisible && update.cursorBlink &&
            update.cursorRow < update.rows && update.cursorCol < update.cols
    }

    // SGR 5(blink)の点滅位相。UI表示にのみ閉じたアニメーション状態であり
    // rust-ssot の対象外(「blink属性が立っているかどうか」自体は`CellData.blink`
    // としてRustが決定した値をそのまま見るだけ、iOS版`TerminalScreenView.swift`の
    // `blinkPhaseVisible`と対称)。xterm既定に近い0.53秒間隔でトグルする。
    // 点滅カーソルもこの同じ位相を共有する(xtermも同じ位相を共有する、iOS版と対称)。
    //
    // タスク#86: `LaunchedEffect`のキーは`hasBlink`/`cursorBlinks`を別々に渡さず
    // `hasActiveBlink = hasBlink || cursorBlinks`という1つの値へ合成する(codexレビュー
    // 2次指摘)。別々のキーのままだと、既にSGR blinkが点滅中の状態へ点滅カーソルが
    // 追加された場合(`(true, false) → (true, true)`)のような「実質的には点滅状態が
    // 継続しているだけ」の遷移でもEffectが再起動され、下記のリセット処理が既存の
    // 点滅位相を不必要に巻き戻してしまう(iOS版`shouldResetBlinkPhase`が「既に
    // 点滅中なら新規遷移ではない」として区別しているのと不整合になる)。1つの
    // boolean値へ合成しておけば、Composeの`LaunchedEffect`キー比較自体が「無→有」
    // 遷移かどうかを自動的に判定してくれるため、iOS側のような明示的な状態比較なしに
    // 同じ意味論を実現できる。
    val hasActiveBlink = hasBlink || cursorBlinks
    var blinkPhaseVisible by remember { mutableStateOf(true) }
    LaunchedEffect(hasActiveBlink) {
        if (!hasActiveBlink) return@LaunchedEffect
        // このEffectが(再)起動される瞬間(=`hasActiveBlink`がfalse→trueへ新規遷移した
        // 瞬間)に`blinkPhaseVisible`を必ずtrueへ戻す。リセットしないと、前回のEffect
        // 実行が「消灯」側(false)で終わっていた場合、新しいblinkテキスト/点滅カーソルが
        // 最初から最大530ms不可視のまま表示されてしまう(fable/codexレビュー指摘、
        // iOS版`TerminalScreenView.swift`の`draw(_:)`内blink有無の新規遷移検知による
        // リセットと対称)。
        blinkPhaseVisible = true
        while (true) {
            delay(530)
            blinkPhaseVisible = !blinkPhaseVisible
        }
    }

    Canvas(modifier = modifier.background(theme.background)) {
        val cols = update.cols.toInt()
        val rows = update.rows.toInt()
        if (cols <= 0 || rows <= 0) return@Canvas

        val cellW = size.width / cols
        val cellH = size.height / rows
        val themeBgArgb = theme.background.toArgb()

        // blink属性(SGR 5)を持つセルが1つも無ければ、位相トグルのたびに
        // Bitmap キャッシュキーを変えて無駄な全走査を発生させない
        // (Fableレビュー2次: blink位相をキャッシュキーに含める対応。ただし
        // blinkが無い画面ではキーを固定値のままにしてキャッシュヒットを保つ)。
        // hasBlink自体はCanvas描画スコープの外(composable本体)で計算済み。
        val effectiveBlinkPhase = hasBlink && blinkPhaseVisible

        // フォントサイズをセル幅に収まるよう実測で調整
        // まず cellH ベースで設定し、M の実測幅が cellW を超えたら縮小
        // (cellW/cellH/typeface が変わらない限りキャッシュを使い回す)
        if (fontFit.needsRefit(cellW, cellH, typeface)) {
            textPaint.textSize = cellH * 0.75f
            val mWidth = textPaint.measureText("M")
            if (mWidth > cellW) {
                textPaint.textSize *= cellW / mWidth
            }
            val fm = textPaint.fontMetrics
            fontFit.markFit(cellW, cellH, typeface, baseline = -fm.top)
        }
        val baseline = fontFit.baseline

        // グリッド全体を描画する off-screen Bitmap。Canvas の実ピクセルサイズに合わせて
        // 確保し直す(回転・分割ペイン等でのリサイズ時のみ再確保が走る)。
        val pixelW = ceil(size.width).toInt().coerceAtLeast(1)
        val pixelH = ceil(size.height).toInt().coerceAtLeast(1)
        var bmp = gridCache.bitmap
        if (bmp == null || bmp.width != pixelW || bmp.height != pixelH) {
            bmp = Bitmap.createBitmap(pixelW, pixelH, Bitmap.Config.ARGB_8888)
            gridCache.bitmap = bmp
            gridCache.invalidate()
        }

        when (val plan = gridCache.planRender(update, cellW, cellH, themeBgArgb, typeface, effectiveBlinkPhase)) {
            GridRenderPlan.Reuse -> Unit // グリッドは前回と同一。Bitmap を触らず下の drawImage で貼り直すだけ。
            GridRenderPlan.Full -> {
                // 前回描画分をクリア(既定背景色以外を描いたセルが今回は既定背景に戻る
                // ケースがあるため、単純な上書きでは古いピクセルが残ってしまう)
                bmp.eraseColor(android.graphics.Color.TRANSPARENT)
                val bitmapCanvas = android.graphics.Canvas(bmp)
                for (row in 0 until rows) {
                    drawRow(
                        canvas = bitmapCanvas,
                        rowIndex = row,
                        cells = update.cells,
                        cols = cols,
                        cellW = cellW,
                        cellH = cellH,
                        baseline = baseline,
                        themeBgArgb = themeBgArgb,
                        effectiveBlinkPhase = effectiveBlinkPhase,
                        bgPaint = bgPaint,
                        textPaint = textPaint,
                        typeface = typeface,
                        italicTypeface = italicTypeface,
                    )
                }
                // 次回の描画でtypefaceが汚れたままにならないよう既定値へ戻す(このPaintは
                // グリッド以外(カーソル等)では使わないが、rememberで使い回されるインスタンス
                // なので明示的にリセットしておく)。`bgPaint.color`は次の背景run/装飾線描画の
                // たびに都度上書きされるためリセット不要。
                textPaint.typeface = typeface
                gridCache.markRendered(update, cellW, cellH, themeBgArgb, typeface, effectiveBlinkPhase)
            }
            is GridRenderPlan.Partial -> {
                if (plan.rows.isNotEmpty()) {
                    redrawDirtyRows(
                        canvas = android.graphics.Canvas(bmp),
                        rows = plan.rows,
                        bitmapWidthPx = pixelW,
                        cells = update.cells,
                        cols = cols,
                        cellW = cellW,
                        cellH = cellH,
                        baseline = baseline,
                        themeBgArgb = themeBgArgb,
                        effectiveBlinkPhase = effectiveBlinkPhase,
                        clearPaint = clearPaint,
                        bgPaint = bgPaint,
                        textPaint = textPaint,
                        typeface = typeface,
                        italicTypeface = italicTypeface,
                    )
                    textPaint.typeface = typeface
                }
                // 部分再描画(空リスト=グリッド無変化を含む)でもスナップショットは前進させる。
                gridCache.markRendered(update, cellW, cellH, themeBgArgb, typeface, effectiveBlinkPhase)
            }
        }

        drawImage(
            image = bmp.asImageBitmap(),
            dstOffset = IntOffset.Zero,
            dstSize = IntSize(pixelW, pixelH),
        )

        // Sixel画像(タスク#42)。テキストグリッドの上・カーソル/選択ハイライトの下に
        // 重ねる(実端末でも画像の上にカーソルが乗ることがあるのと同じ描画順)。
        // 配置(row/col/rows_span/cols_span)の判断は一切ここでは行わず、Rust側が
        // 決めた矩形へ`rgba`を引き伸ばして描くだけ(rust-ssot)。
        // update.imagesが空でも必ずbitmapsForを呼ぶ(寿命が尽きた画像のBitmapを
        // キャッシュから解放するため。isNotEmpty()で早期returnしていた旧実装は
        // 画像が0件になった後もBitmapを保持し続けるリークがあった。codexレビュー指摘)。
        run {
            val bitmaps = sixelCache.bitmapsFor(update.images)
            for (placement in update.images) {
                val src = bitmaps[placement.id] ?: continue
                drawImage(
                    image = src.asImageBitmap(),
                    dstOffset = IntOffset(
                        (placement.col.toInt() * cellW).toInt(),
                        (placement.row.toInt() * cellH).toInt(),
                    ),
                    dstSize = IntSize(
                        (placement.colsSpan.toInt() * cellW).toInt().coerceAtLeast(1),
                        (placement.rowsSpan.toInt() * cellH).toInt().coerceAtLeast(1),
                    ),
                )
            }
        }

        // カーソル(Bitmap キャッシュ対象外。テーマのカーソル色が変わってもキャッシュキーを
        // 増やす必要がないよう、選択ハイライトと同様に毎フレーム軽量に描画し直す)
        // DECTCEM(CSI ?25l/h)でカーソルが非表示状態のときはRust側がcursorVisible=falseを
        // 立てるので、描画自体をスキップする(rust-ssot: 可視判定はRust側で行い、Kotlin側は
        // フラグをそのまま反映するだけ)。タスク#33: DECSCUSR(`CSI Ps SP q`)が選択した
        // 形状は`update.cursorShape`(Rust側`Terminal`が真値を保持、rust-ssot)からそのまま
        // 読み、block/underline/barを描き分ける。点滅そのもの(`blinkPhaseVisible`という
        // 位相)はUIローカル状態(SGR blinkと同じタイマーを共有)だが、「点滅させるべきか
        // どうか」は`update.cursorBlink`(DECSCUSRの偶数/奇数パラメータ、DECSET `?12`の
        // どちらもRust側`Terminal`が解決済み)をそのまま見るだけで、Kotlin側では判断しない
        // (iOS版`TerminalScreenView.swift`の同名分岐と対称)。
        val cursorHidden = update.cursorBlink && !blinkPhaseVisible
        if (update.cursorVisible && !cursorHidden) {
            val cx = update.cursorCol.toInt() * cellW
            val cy = update.cursorRow.toInt() * cellH
            if (cx < size.width && cy < size.height) {
                cursorPaint.color = theme.cursor.copy(alpha = 0.7f).toArgb()
                val nCanvas = drawContext.canvas.nativeCanvas
                val rect = computeCursorRect(cx, cy, cellW, cellH, update.cursorShape)
                nCanvas.drawRect(rect.left, rect.top, rect.right, rect.bottom, cursorPaint)
            }
        }

        // 選択範囲のハイライト(行単位。Bitmap キャッシュとは独立に毎フレーム描画するので、
        // ドラッグ中でも背景・文字の再走査なしに追従できる)
        selection?.let { sel ->
            val startRow = sel.startRow.coerceIn(0, rows - 1)
            val endRow = sel.endRow.coerceIn(0, rows - 1)
            if (startRow <= endRow) {
                val nCanvas = drawContext.canvas.nativeCanvas
                for (row in startRow..endRow) {
                    val y = row * cellH
                    nCanvas.drawRect(0f, y, size.width, y + cellH, selectionPaint)
                }
            }
        }

        // タスク#66: 検索バーの現在マッチのハイライト。呼び出し元(`TerminalScreen.kt`)は
        // `scrollOffset`が`searchHighlight.row`と一致するときだけこの値を渡す(rust-ssot:
        // マッチの位置計算自体は一切ここでは行わず、Rust側が返した座標をそのまま描くだけ)。
        // 矩形計算は[computeSearchHighlightRect]にピュア関数として抽出済み。
        searchHighlight?.let { match ->
            computeSearchHighlightRect(match, rows, cols, cellW, cellH)?.let { rect ->
                val nCanvas = drawContext.canvas.nativeCanvas
                nCanvas.drawRect(rect.left, rect.top, rect.right, rect.bottom, searchHighlightPaint)
            }
        }
    }
}
