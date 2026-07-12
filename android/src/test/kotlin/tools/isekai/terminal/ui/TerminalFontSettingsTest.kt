package tools.isekai.terminal.ui

import android.content.Context
import android.content.SharedPreferences
import android.graphics.Typeface
import android.net.Uri
import androidx.test.core.app.ApplicationProvider
import java.io.File
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Assume.assumeTrue
import org.junit.Before
import org.junit.Test
import org.junit.runner.RunWith
import org.robolectric.RobolectricTestRunner
import org.robolectric.annotation.Config

/**
 * [TerminalFontSettings] のユニットテスト。カスタムフォント(TTF/OTF)のインポート・
 * 読み込み・削除の中核ロジックを検証する。
 *
 * 「壊れたフォント/フォントでないファイル」の判定は実際の [Typeface.createFromFile] の
 * 例外に依存するため、Robolectric のネイティブグラフィックス(RNG)経由で実際のフォント
 * パーサーが動く前提のテストになっている。「正常なフォントのインポートが成功する」テストは
 * このサンドボックス環境に存在するシステムフォントを使って検証しており、そのフォントが
 * 存在しない環境では [assumeTrue] によりスキップされる(CI環境の可搬性のため)。
 */
@RunWith(RobolectricTestRunner::class)
@Config(sdk = [33])
class TerminalFontSettingsTest {
    private lateinit var context: Context
    private lateinit var prefs: SharedPreferences

    @Before
    fun setUp() {
        context = ApplicationProvider.getApplicationContext()
        prefs = context.getSharedPreferences("isekai_terminal_ui_test", Context.MODE_PRIVATE)
        prefs.edit().clear().commit()
        File(context.filesDir, "fonts").deleteRecursively()
    }

    @Test
    fun loadTypeface_whenNoFontSet_fallsBackToMonospace() {
        val typeface = TerminalFontSettings.loadTypeface(context, prefs)
        assertEquals(Typeface.MONOSPACE, typeface)
    }

    @Test
    fun currentFontFile_whenNothingStored_returnsNull() {
        assertEquals(null, TerminalFontSettings.currentFontFile(context, prefs))
    }

    @Test
    fun currentFontFile_whenPrefSetButFileMissing_returnsNull() {
        // ファイルが何らかの理由(手動削除・アンインストール前の状態からの復元等)で
        // 実際には存在しないのに設定だけ残っているケースも安全にフォールバックすること。
        prefs.edit().putString(TerminalFontSettings.PREF_KEY, "does_not_exist.ttf").apply()
        assertNull(TerminalFontSettings.currentFontFile(context, prefs))
        assertEquals(Typeface.MONOSPACE, TerminalFontSettings.loadTypeface(context, prefs))
    }

    @Test
    fun importFont_corruptFile_failsAndDoesNotChangeSetting() {
        val garbage = File.createTempFile("bad-font", ".ttf")
        garbage.writeBytes(ByteArray(256) { it.toByte() })
        val uri = Uri.fromFile(garbage)

        val result = TerminalFontSettings.importFont(context, prefs, uri, "bad-font.ttf")

        assertTrue("壊れたフォントファイルはFailureになるべき", result is TerminalFontSettings.ImportResult.Failure)
        assertNull("設定は変更されないはず", prefs.getString(TerminalFontSettings.PREF_KEY, null))
        assertEquals(Typeface.MONOSPACE, TerminalFontSettings.loadTypeface(context, prefs))
    }

    @Test
    fun importFont_emptyFile_failsAsFileTooSmallToBeAFont() {
        val empty = File.createTempFile("empty-font", ".ttf")
        val uri = Uri.fromFile(empty)

        val result = TerminalFontSettings.importFont(context, prefs, uri, "empty-font.ttf")

        assertTrue(result is TerminalFontSettings.ImportResult.Failure)
        assertNull(prefs.getString(TerminalFontSettings.PREF_KEY, null))
    }

    @Test
    fun importFont_validTtf_succeedsAndTypefaceLoads() {
        // 実機に相当するフォントパーサー検証にはこのサンドボックスに存在する実際の TTF を
        // 使う(合成した偽フォントバイト列では現実のフォントパーサーの合否を再現できないため)。
        val systemFont = File("/usr/share/fonts/truetype/liberation/LiberationMono-Regular.ttf")
        assumeTrue("system test font not available in this environment", systemFont.exists())
        val uri = Uri.fromFile(systemFont)

        val result = TerminalFontSettings.importFont(context, prefs, uri, "LiberationMono-Regular.ttf")

        assertTrue("有効なTTFはSuccessになるべき", result is TerminalFontSettings.ImportResult.Success)
        assertEquals("custom_font.ttf", prefs.getString(TerminalFontSettings.PREF_KEY, null))
        val typeface = TerminalFontSettings.loadTypeface(context, prefs)
        assertNotEquals("カスタムフォントが読み込まれ、デフォルトのMONOSPACEとは異なるはず", Typeface.MONOSPACE, typeface)
    }

    @Test
    fun importFont_replacingWithDifferentExtension_deletesOldFile() {
        // destFileName は拡張子(ttf/otf)から決まるため、同じ拡張子で再インポートした場合は
        // 単なる上書きになる。「古いファイルの削除」経路を実際に踏むには拡張子が変わる
        // (ttf → otf)再インポートで検証する必要がある。
        val ttfFont = File("/usr/share/fonts/truetype/liberation/LiberationMono-Regular.ttf")
        assumeTrue("system test ttf font not available in this environment", ttfFont.exists())
        val otfFont = File("/usr/share/fonts/opentype/urw-base35/NimbusMonoPS-Regular.otf")
        assumeTrue("system test otf font not available in this environment", otfFont.exists())

        val firstResult = TerminalFontSettings.importFont(context, prefs, Uri.fromFile(ttfFont), "first.ttf")
        assertTrue(firstResult is TerminalFontSettings.ImportResult.Success)
        val firstFile = TerminalFontSettings.currentFontFile(context, prefs)
        assertTrue(firstFile != null && firstFile.exists())

        val secondResult = TerminalFontSettings.importFont(context, prefs, Uri.fromFile(otfFont), "second.otf")

        assertTrue(secondResult is TerminalFontSettings.ImportResult.Success)
        assertEquals("custom_font.otf", prefs.getString(TerminalFontSettings.PREF_KEY, null))
        assertFalse("古い拡張子(.ttf)のファイルは削除されているはず", firstFile!!.exists())
    }

    @Test
    fun clearStoredFont_removesPrefAndFile() {
        prefs.edit().putString(TerminalFontSettings.PREF_KEY, "custom_font.ttf").apply()
        val dir = File(context.filesDir, "fonts").apply { mkdirs() }
        val fontFile = File(dir, "custom_font.ttf").apply { writeBytes(byteArrayOf(1, 2, 3)) }
        assertTrue(fontFile.exists())

        TerminalFontSettings.clearStoredFont(context, prefs)

        assertNull(prefs.getString(TerminalFontSettings.PREF_KEY, null))
        assertFalse(fontFile.exists())
        assertEquals(Typeface.MONOSPACE, TerminalFontSettings.loadTypeface(context, prefs))
    }

    @Test
    fun clearStoredFont_whenNothingStored_isNoOp() {
        TerminalFontSettings.clearStoredFont(context, prefs)
        assertNull(prefs.getString(TerminalFontSettings.PREF_KEY, null))
    }
}
