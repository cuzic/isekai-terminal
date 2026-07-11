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

    /** [clipData]の先頭itemが画像(mimeが"image/"で始まる)かどうかを判定する。 */
    fun isImageClip(clipData: ClipData?): Boolean {
        val description = clipData?.description ?: return false
        return (0 until description.mimeTypeCount).any { i -> description.getMimeType(i).startsWith("image/") }
    }

    /**
     * [data](PNGバイト列)をキャッシュディレクトリの一時ファイルへ書き出し、`FileProvider`
     * 経由のcontent:// URIを持つ[ClipData]を返す。書き込み前に既存の一時ファイルを
     * 全て削除する(`isekai-pipe-core::sweep_stale_sockets`と同じ「常駐GCなし、次回
     * 書き込み前に掃除する」パターン——1つの直近画像だけ保持できれば十分で履歴は不要)。
     */
    fun writeImageToClipData(context: Context, data: ByteArray): ClipData {
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
     * 画像でない、デコードに失敗した、または[MAX_IMAGE_BYTES]を超える場合は`null`。
     */
    fun readImageFromClipData(resolver: ContentResolver, clipData: ClipData?): ClipboardPayload? {
        if (!isImageClip(clipData)) return null
        val uri = clipData?.takeIf { it.itemCount > 0 }?.getItemAt(0)?.uri ?: return null
        val bitmap = resolver.openInputStream(uri)?.use { BitmapFactory.decodeStream(it) } ?: return null
        val encoded = ByteArrayOutputStream().use { out ->
            bitmap.compress(Bitmap.CompressFormat.PNG, 100, out)
            out.toByteArray()
        }
        if (encoded.size > MAX_IMAGE_BYTES) return null
        return ClipboardPayload(ClipboardMimeKind.IMAGE_PNG, encoded)
    }
}
