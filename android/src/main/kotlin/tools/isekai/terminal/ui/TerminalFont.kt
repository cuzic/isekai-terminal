package tools.isekai.terminal.ui

import android.content.Context
import android.content.SharedPreferences
import android.graphics.Typeface
import android.net.Uri
import java.io.File

/**
 * カスタム端末フォント(TTF/OTF)の永続化・読み込み。
 *
 * 配色テーマ([TerminalTheme])とは意図的に分離した独立設定(SharedPreferences
 * `isekai_terminal_ui` を共用しつつキーだけ別)として扱う。永続化方式は SAF の [Uri]
 * (`content://...`)をそのまま保存するのではなく、選択されたファイルの実体をアプリ内部
 * ストレージ(`filesDir/fonts/`)へコピーして保持する — SAF の一時パーミッションは
 * アプリ再起動やプロバイダ側の都合で失効しうるため、`Typeface.createFromFile()` で
 * 安定して読み込める自前コピーが必要。
 */
object TerminalFontSettings {
    /** `SharedPreferences("isekai_terminal_ui")` に保存するカスタムフォントのファイル名のキー。
     *  未設定(null)の場合は [Typeface.MONOSPACE] が既定のまま使われる。 */
    const val PREF_KEY = "terminal_font_filename"

    private const val FONT_DIR = "fonts"

    // sfnt(TTF/OTF)ファイル先頭4バイトの既知のマジックナンバー。
    // `Typeface.createFromFile()` は環境(実機のAndroidバージョン差・Robolectricの
    // ネイティブグラフィックス実装)によっては壊れた/フォントでないファイルに対しても
    // 例外を投げず既定フォントへ黙ってフォールバックすることがある(実測で確認済み — 単なる
    // 連番バイト列を渡しても例外にならないケースがあった)。そのため「フォントでないファイル」
    // の一次判定はこのマジックナンバー比較で決定的に行い、`Typeface.createFromFile()` の
    // 例外は補助的な二次防御として扱う。
    private val SFNT_MAGICS: List<ByteArray> = listOf(
        byteArrayOf(0x00, 0x01, 0x00, 0x00), // TrueType (glyf outlines)
        "OTTO".toByteArray(Charsets.US_ASCII), // OpenType (CFF outlines)
        "true".toByteArray(Charsets.US_ASCII), // 旧 Mac TrueType
        "typ1".toByteArray(Charsets.US_ASCII), // 旧 PostScript
        "ttcf".toByteArray(Charsets.US_ASCII), // TrueType Collection
    )

    private fun looksLikeSfntFont(file: File): Boolean {
        if (file.length() < 4) return false
        val header = ByteArray(4)
        file.inputStream().use { input ->
            var read = 0
            while (read < 4) {
                val n = input.read(header, read, 4 - read)
                if (n < 0) return false
                read += n
            }
        }
        return SFNT_MAGICS.any { it.contentEquals(header) }
    }

    private fun fontDir(context: Context): File =
        File(context.filesDir, FONT_DIR).apply { mkdirs() }

    /** 現在保存されているカスタムフォントファイル。未設定、またはファイルが実際には
     *  存在しない(何らかの理由で消えた)場合は null。 */
    fun currentFontFile(context: Context, prefs: SharedPreferences): File? {
        val name = prefs.getString(PREF_KEY, null) ?: return null
        val file = File(fontDir(context), name)
        return if (file.exists()) file else null
    }

    /**
     * 現在の設定から [Typeface] を解決する。未設定、ファイル欠損、または読み込み失敗
     * (壊れたファイル・フォントでないファイル)の場合は [Typeface.MONOSPACE] に
     * フォールバックする(フォント未選択時の既定挙動を壊さないための必須の安全策)。
     */
    fun loadTypeface(context: Context, prefs: SharedPreferences): Typeface {
        val file = currentFontFile(context, prefs) ?: return Typeface.MONOSPACE
        return try {
            Typeface.createFromFile(file)
        } catch (e: Exception) {
            Typeface.MONOSPACE
        } catch (e: Error) {
            // 一部端末/Robolectric環境では不正なフォントデータに対し Error 系
            // (例: RuntimeException のサブクラスではない native crash 相当)を投げることがあるため
            // 可能な範囲でフォールバックする。
            Typeface.MONOSPACE
        }
    }

    sealed class ImportResult {
        data class Success(val fileName: String) : ImportResult()
        data class Failure(val message: String) : ImportResult()
    }

    /**
     * [uri] が指すフォントファイルを内部ストレージへコピーし、実際に [Typeface] として
     * 読み込めるか検証してから設定として確定する。検証に失敗した場合はコピーした
     * ファイルを削除し、既存の設定にも一切影響を与えない([ImportResult.Failure])。
     *
     * 呼び出し元は Dispatchers.IO 等のバックグラウンドスレッドから呼ぶこと
     * (ファイル I/O・[Typeface.createFromFile] のデコードを含むため)。
     */
    fun importFont(
        context: Context,
        prefs: SharedPreferences,
        uri: Uri,
        displayName: String?,
    ): ImportResult {
        val dir = fontDir(context)
        val ext = displayName
            ?.substringAfterLast('.', "")
            ?.lowercase()
            ?.takeIf { it == "ttf" || it == "otf" }
            ?: "ttf"
        val destFileName = "custom_font.$ext"
        val dest = File(dir, destFileName)
        val tmp = File(dir, "$destFileName.importing")
        try {
            val copied = context.contentResolver.openInputStream(uri)?.use { input ->
                tmp.outputStream().use { output -> input.copyTo(output) }
                true
            } ?: false
            if (!copied) {
                tmp.delete()
                return ImportResult.Failure("ファイルを読み込めませんでした")
            }
            if (tmp.length() == 0L) {
                tmp.delete()
                return ImportResult.Failure("空のファイルです")
            }

            // 一次検証: sfnt マジックナンバーで TTF/OTF らしいファイルかを決定的に判定する
            // (Typeface.createFromFile の例外だけに頼ると、環境によっては壊れたファイルでも
            // 例外を投げず既定フォントへ黙ってフォールバックしてしまうことがあるため)。
            if (!looksLikeSfntFont(tmp)) {
                tmp.delete()
                return ImportResult.Failure("フォントファイルとして読み込めませんでした（壊れているか非対応の形式です）")
            }

            // 二次検証: マジックナンバーは正しくても内部構造が壊れているファイルは
            // Typeface.createFromFile が例外を投げることがあるため、引き続きここでも確認する。
            // 一部端末/Robolectric環境では不正なフォントデータに対しErrorを投げることも
            // あるため([loadTypeface]と同じ理由)、ここだけ狭くException/Error両方を拾う
            // (このtry全体をcatch(Error)で覆うと、無関係なファイルI/O由来の深刻なErrorまで
            // 握りつぶしてしまうため、検証呼び出しの直近だけに絞る)。
            try {
                Typeface.createFromFile(tmp)
            } catch (e: Throwable) {
                tmp.delete()
                return ImportResult.Failure("フォントファイルとして読み込めませんでした（壊れているか非対応の形式です）")
            }

            // 検証に成功したので確定パスへ差し替える(古いカスタムフォントファイルは削除)。
            val previousName = prefs.getString(PREF_KEY, null)
            if (previousName != null && previousName != destFileName) {
                File(dir, previousName).delete()
            }
            dest.delete()
            if (!tmp.renameTo(dest)) {
                tmp.copyTo(dest, overwrite = true)
                tmp.delete()
            }
            prefs.edit().putString(PREF_KEY, destFileName).apply()
            return ImportResult.Success(destFileName)
        } catch (e: Exception) {
            tmp.delete()
            return ImportResult.Failure("フォントファイルとして読み込めませんでした（壊れているか非対応の形式です）")
        }
    }

    /** カスタムフォントの設定を削除し、既定の [Typeface.MONOSPACE] に戻す。 */
    fun clearStoredFont(context: Context, prefs: SharedPreferences) {
        val name = prefs.getString(PREF_KEY, null)
        if (name != null) {
            File(fontDir(context), name).delete()
        }
        prefs.edit().remove(PREF_KEY).apply()
    }
}
