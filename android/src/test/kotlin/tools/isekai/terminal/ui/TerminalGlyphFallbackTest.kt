package tools.isekai.terminal.ui

import android.graphics.Typeface
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config

/**
 * [chooseGlyphTypeface](どの Typeface でグリフを描くかのピュアな選択ロジック)と
 * [GlyphCoverageCache](hasGlyph 判定結果の有界 LRU キャッシュ)の単体テスト。
 *
 * どちらも実フォント描画に依存しない(選択関数は `hasGlyph` 述語を差し替え可能、
 * キャッシュは compute ラムダを差し替え可能)ため、Robolectric のフォントサブシステム
 * (`Paint.hasGlyph` の実挙動は環境依存でスタブされ得る)に頼らずに検証できる。
 */
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [28])
class TerminalGlyphFallbackTest {

    @Test
    fun `primary has glyph uses primary`() {
        val choice = chooseGlyphTypeface(
            "A",
            primaryHasGlyph = { true },
            fallbackHasGlyph = { throw AssertionError("primary が持つならフォールバックは評価しない") },
        )
        assertEquals(GlyphTypefaceChoice.PRIMARY, choice)
    }

    @Test
    fun `primary lacks but fallback has glyph uses fallback`() {
        val choice = chooseGlyphTypeface(
            "😀", // 😀
            primaryHasGlyph = { false },
            fallbackHasGlyph = { true },
        )
        assertEquals(GlyphTypefaceChoice.FALLBACK, choice)
    }

    @Test
    fun `neither has glyph falls back to primary as last resort`() {
        val choice = chooseGlyphTypeface(
            "􏿿", // 私用領域末端など、どのフォントも持たない想定
            primaryHasGlyph = { false },
            fallbackHasGlyph = { false },
        )
        assertEquals(GlyphTypefaceChoice.PRIMARY, choice)
    }

    @Test
    fun `cache computes once then serves repeated lookups from cache`() {
        val cache = GlyphCoverageCache(maxEntries = 16)
        var computeCount = 0
        val tf = Typeface.MONOSPACE

        repeat(5) {
            val has = cache.coversGlyph(tf, "A") { computeCount++; true }
            assertTrue(has)
        }
        assertEquals("同一キーの compute は初回1回だけ実行される", 1, computeCount)
        assertEquals(1, cache.size)
    }

    @Test
    fun `cache distinguishes different characters and typefaces`() {
        val cache = GlyphCoverageCache(maxEntries = 16)
        var computeCount = 0
        val mono = Typeface.MONOSPACE
        val sans = Typeface.SANS_SERIF

        cache.coversGlyph(mono, "A") { computeCount++; true }
        cache.coversGlyph(mono, "B") { computeCount++; true } // 文字違い → 別キー
        cache.coversGlyph(sans, "A") { computeCount++; true } // typeface 違い → 別キー
        cache.coversGlyph(mono, "A") { computeCount++; true } // 既出 → キャッシュヒット

        assertEquals(3, computeCount)
        assertEquals(3, cache.size)
    }

    @Test
    fun `cache is bounded and evicts least-recently-used entries`() {
        val cache = GlyphCoverageCache(maxEntries = 3)
        val tf = Typeface.MONOSPACE

        // 4種の異なる文字を投入 → 上限3を超えるので最古(まだ触っていない最初のキー)が退避される。
        cache.coversGlyph(tf, "a") { true }
        cache.coversGlyph(tf, "b") { true }
        cache.coversGlyph(tf, "c") { true }
        cache.coversGlyph(tf, "d") { true }

        assertEquals("エントリ数は上限を超えない", 3, cache.size)

        // "a" は退避済みのはずなので再度 compute が走る(=キャッシュに残っていない証拠)。
        var recomputed = false
        cache.coversGlyph(tf, "a") { recomputed = true; true }
        assertTrue("退避された最古のキー \"a\" は再計算される", recomputed)
    }

    @Test
    fun `cache access refreshes recency so bound never grows`() {
        val cache = GlyphCoverageCache(maxEntries = 2)
        val tf = Typeface.MONOSPACE

        cache.coversGlyph(tf, "x") { true }
        cache.coversGlyph(tf, "y") { true }
        // "x" にアクセスして最近利用に更新 → 次の挿入で退避されるのは "y" 側。
        cache.coversGlyph(tf, "x") { throw AssertionError("キャッシュヒットのはず") }
        cache.coversGlyph(tf, "z") { true }

        assertEquals(2, cache.size)
        // "x" は生き残っている(compute されない)。
        cache.coversGlyph(tf, "x") { throw AssertionError("\"x\" は最近利用でキャッシュに残るはず") }
    }
}
