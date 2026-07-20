package tools.isekai.terminal.ui

import android.graphics.Paint
import android.graphics.Typeface

/**
 * カスタム端末フォント([TerminalFontSettings])が持たないグリフ(絵文字・各種記号・
 * マイナーな Unicode ブロック等)を、セル単位でシステムのフォールバックフォントへ
 * 逃がすための小さな仕組み。
 *
 * 背景: 既定の [Typeface.MONOSPACE] は Android のシステムフォント・フォールバック
 * チェーン(絵文字用 NotoColorEmoji 等を含む)を自動的に引き継ぐため、欠落グリフも
 * 黙って別フォントで描かれる。ところがユーザーが取り込んだカスタムフォントは
 * `Typeface.createFromFile()` で生成されるため**このフォールバックチェーンを持たない** —
 * そのフォントに無いグリフは豆腐(.notdef)で描かれてしまう。
 *
 * 対策は「セルごとに、いま使う [Typeface] がそのグリフを持っているか([Paint.hasGlyph])
 * を確認し、無ければその1回の `drawText` だけフォールバック [Typeface] に差し替える」。
 * 端末は1セル=1 `drawText` で描く方針([SshTerminalCanvas] の `drawRow` 参照)なので、
 * この差し替えは複数セルをまとめた shaped run を作らずにそのまま成立する。
 */

/** [chooseGlyphTypeface] の結果。どちらの [Typeface] でそのセルを描くか。 */
internal enum class GlyphTypefaceChoice { PRIMARY, FALLBACK }

/**
 * あるグリフをどの [Typeface] で描くかを決めるピュア関数(実フォント描画に依存しないので
 * `hasGlyph` 相当の述語を差し替えて単体テストできる)。
 *
 * - primary が持っていれば primary(カスタムフォントの見た目を最優先)。
 * - primary が無く fallback が持っていれば fallback(システムのフォールバック経由)。
 * - どちらも持っていなければ PRIMARY。フォールバックへ差し替えても豆腐になるだけで
 *   得られるものが無く、primary のままにしておけば等幅メトリクスの一貫性を保てるため。
 */
internal fun chooseGlyphTypeface(
    ch: String,
    primaryHasGlyph: (String) -> Boolean,
    fallbackHasGlyph: (String) -> Boolean,
): GlyphTypefaceChoice {
    if (primaryHasGlyph(ch)) return GlyphTypefaceChoice.PRIMARY
    if (fallbackHasGlyph(ch)) return GlyphTypefaceChoice.FALLBACK
    return GlyphTypefaceChoice.PRIMARY
}

/**
 * `(Typeface, 文字)` → その [Typeface] がグリフを持つか、の結果を覚える有界 LRU キャッシュ。
 *
 * [Paint.hasGlyph] は毎フレーム全セル分を素で呼ぶと無視できないコストになる一方、
 * 端末画面に実際に現れる文字種は定常状態では小さいのでキャッシュがよく効く。ただし
 * 悪意ある/異常な出力(大量の異なるコードポイント)でも無制限に膨らまないよう
 * [maxEntries] で必ず上限を設ける。
 *
 * キーの [Typeface] は `equals` を持たず参照同一性で比較されるため、別インスタンスの
 * 取り違えは起きない。
 */
internal class GlyphCoverageCache(private val maxEntries: Int = 2048) {
    private val cache = object : LinkedHashMap<Pair<Typeface, String>, Boolean>(16, 0.75f, true) {
        override fun removeEldestEntry(eldest: MutableMap.MutableEntry<Pair<Typeface, String>, Boolean>): Boolean =
            size > maxEntries
    }

    /** 現在のエントリ数(テスト用)。 */
    val size: Int get() = cache.size

    fun coversGlyph(typeface: Typeface, ch: String, compute: () -> Boolean): Boolean {
        val key = typeface to ch
        cache[key]?.let { return it }
        val result = compute()
        cache[key] = result
        return result
    }
}

/**
 * `drawRow` から使う実体。[Paint.hasGlyph] 判定を [GlyphCoverageCache] 越しに行い、
 * セルの文字を描くのに使うべき [Typeface] を返す。
 *
 * @param fallbackTypeface 欠落グリフの逃がし先。既定は [Typeface.DEFAULT] — システムの
 *   標準フォールバックチェーン(絵文字を含む)を引き継ぐ最も広いカバレッジのため。
 *   [Typeface.MONOSPACE] でも同じチェーンを持つが、フォールバックで描かれるのは
 *   等幅フォントに無いような記号・絵文字が中心で、そこで等幅性に拘る必要は薄い。
 */
internal class GlyphFallbackResolver(
    private val fallbackTypeface: Typeface = Typeface.DEFAULT,
    maxEntries: Int = 2048,
) {
    private val cache = GlyphCoverageCache(maxEntries)
    // hasGlyph 判定専用の Paint。呼び出し元の描画用 Paint を汚さないよう分離する
    // (hasGlyph は cmap カバレッジだけを見るので textSize 等の設定は不要)。
    private val probePaint = Paint()

    fun resolve(ch: String, primary: Typeface): Typeface {
        val choice = chooseGlyphTypeface(
            ch,
            primaryHasGlyph = { cache.coversGlyph(primary, it) { hasGlyph(primary, it) } },
            fallbackHasGlyph = { cache.coversGlyph(fallbackTypeface, it) { hasGlyph(fallbackTypeface, it) } },
        )
        return when (choice) {
            GlyphTypefaceChoice.PRIMARY -> primary
            GlyphTypefaceChoice.FALLBACK -> fallbackTypeface
        }
    }

    private fun hasGlyph(typeface: Typeface, ch: String): Boolean {
        probePaint.typeface = typeface
        return probePaint.hasGlyph(ch)
    }
}
