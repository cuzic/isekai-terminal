package tools.isekai.terminal

import android.content.ClipData
import android.content.ContentResolver
import android.content.Context
import android.graphics.Bitmap
import android.graphics.BitmapFactory
import androidx.core.content.FileProvider
import uniffi.isekai_terminal_core.ClipboardMimeKind
import uniffi.isekai_terminal_core.ClipboardPayload
import java.io.ByteArrayOutputStream
import java.io.File

/**
 * host↔deviceクリップボード画像同期(`ISEKAI_PIPE_DESIGN.md` §8 Epic M follow-up)の
 * Android固有I/O(`FileProvider`/`ContentResolver`/`BitmapFactory`)をまとめたもの。
 * OSC 52はテキスト専用プロトコルなので、画像は`isekai-pipe ctl`のtmux迂回チャンネル
 * (`CtlMessage::ClipboardPush`/`ClipboardPullResponse`)経由でのみやり取りされる——
 * その振り分け判断自体はRust側(`session.rs`)が行うので、ここは「今のClipDataが
 * 画像かどうか」の判定と、実際のファイルI/O・変換だけを担う。
 *
 * [isImageClip]は純粋関数(Robolectricでテスト可能)。[writeImageToClipData]/
 * [readImageFromClipData]は`FileProvider`のcontent:// URI発行・`BitmapFactory`の
 * デコードを伴うため、実機/エミュレータでの動作確認が別途必要。
 */
object RemoteClipboardImagePolicy {
    private const val AUTHORITY = "tools.isekai.terminal.fileprovider"
    private const val IMAGE_DIR_NAME = "clipboard-images"

    /** `isekai_protocol::ctl::MAX_CLIPBOARD_IMAGE_DECODED_LEN`(4MiB)と同じ上限。 */
    const val MAX_IMAGE_BYTES = 4 * 1024 * 1024

    /**
     * デコード前の画素数上限(概ね8000x5000相当)。`BitmapFactory.decodeStream`は
     * デコード後のピクセルバッファをそのまま確保するため、圧縮後は小さくても
     * 展開後サイズが巨大な画像(decompression bomb)を弾いてからデコードする。
     */
    private const val MAX_IMAGE_PIXELS = 40_000_000L

    private val PNG_SIGNATURE = byteArrayOf(0x89.toByte(), 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A)

    /** [clipData]の先頭itemが画像(mimeが"image/"で始まる)かどうかを判定する。 */
    fun isImageClip(clipData: ClipData?): Boolean {
        val description = clipData?.description ?: return false
        return (0 until description.mimeTypeCount).any { i -> description.getMimeType(i).startsWith("image/") }
    }

    /**
     * リモートから受け取った[data]が、サイズ上限内かつPNGシグネチャを持つ妥当な
     * ペイロードかどうかを判定する。壊れた/悪意あるリモートが`ClipboardMime::ImagePng`
     * と偽って任意バイト列を送ってきた場合に、`FileProvider`経由でファイルへ書き出す
     * 前に弾くための最小限の検証(中身が本当にデコード可能かまでは保証しない)。
     */
    fun isValidPngPayload(data: ByteArray): Boolean =
        data.size in PNG_SIGNATURE.size..MAX_IMAGE_BYTES &&
            data.copyOfRange(0, PNG_SIGNATURE.size).contentEquals(PNG_SIGNATURE)

    /**
     * [data](PNGバイト列)をキャッシュディレクトリの一時ファイルへ書き出し、`FileProvider`
     * 経由のcontent:// URIを持つ[ClipData]を返す。書き込み前に既存の一時ファイルを
     * 全て削除する(`isekai-pipe-core::sweep_stale_sockets`と同じ「常駐GCなし、次回
     * 書き込み前に掃除する」パターン——1つの直近画像だけ保持できれば十分で履歴は不要)。
     * [data]が[isValidPngPayload]を満たさない場合は書き出さず`null`を返す。
     */
    fun writeImageToClipData(context: Context, data: ByteArray): ClipData? {
        if (!isValidPngPayload(data)) return null
        val dir = File(context.cacheDir, IMAGE_DIR_NAME)
        dir.mkdirs()
        dir.listFiles()?.forEach { it.delete() }
        val file = File(dir, "${System.nanoTime()}.png")
        file.writeBytes(data)
        val uri = FileProvider.getUriForFile(context, AUTHORITY, file)
        return ClipData.newUri(context.contentResolver, "isekai-terminal (remote)", uri)
    }

    /**
     * 現在のプライマリクリップ([clipData])が画像なら、[resolver]経由で読み出し
     * `BitmapFactory`でデコードした上でPNGへ再エンコードして返す(コピー元がJPEG等
     * でも、ワイヤー上は`ClipboardMime::ImagePng`のみをサポートするため常にPNG化する)。
     * 画素数が[MAX_IMAGE_PIXELS]を超える画像はデコード前(`inJustDecodeBounds`)に弾く。
     * 画像でない、デコードに失敗した、または[MAX_IMAGE_BYTES]を超える場合は`null`。
     */
    fun readImageFromClipData(resolver: ContentResolver, clipData: ClipData?): ClipboardPayload? {
        if (!isImageClip(clipData)) return null
        val uri = clipData?.takeIf { it.itemCount > 0 }?.getItemAt(0)?.uri ?: return null
        val bounds = BitmapFactory.Options().apply { inJustDecodeBounds = true }
        resolver.openInputStream(uri)?.use { BitmapFactory.decodeStream(it, null, bounds) } ?: return null
        if (bounds.outWidth <= 0 || bounds.outHeight <= 0) return null
        if (bounds.outWidth.toLong() * bounds.outHeight.toLong() > MAX_IMAGE_PIXELS) return null
        val bitmap = resolver.openInputStream(uri)?.use { BitmapFactory.decodeStream(it) } ?: return null
        val encoded = ByteArrayOutputStream().use { out ->
            bitmap.compress(Bitmap.CompressFormat.PNG, 100, out)
            out.toByteArray()
        }
        if (encoded.size > MAX_IMAGE_BYTES) return null
        return ClipboardPayload(ClipboardMimeKind.IMAGE_PNG, encoded)
    }
}
