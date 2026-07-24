package tools.isekai.terminal.filepreview

import java.io.ByteArrayOutputStream
import uniffi.isekai_terminal_core.FilePreviewOutcome
import uniffi.isekai_terminal_core.FilePreviewRequestKind

/**
 * タスク#17: `isekai-pipe ctl file cat`のチャンク読み取り(`--offset`/`--length`、
 * サーバー側は`ctl_file.rs::MAX_FILE_CAT_CHUNK_LEN`=8MiBでクランプする)を
 * `offset += 返ってきたlength`でページングしながら`eof:true`まで(または表示上限に
 * 達するまで)呼び続ける。ディレクトリブラウザ(`ls`)と違いこちらは複数回のRust
 * 呼び出しをまたぐ手続きなので、[FilePreviewSheet]のComposable本体から切り出して
 * 単体テスト可能にしている。
 */
object FilePreviewLoader {
    /** テキスト系ビューア(markdown/syntax-highlight/csv)で読み込む最大バイト数。
     *  超えたら打ち切り、[FileViewerUiState.truncated]をtrueにする(OOM・巨大文字列
     *  描画のもたつき防止)。 */
    const val MAX_TEXT_PREVIEW_BYTES = 2L * 1024 * 1024

    /** 画像ビューアで読み込む最大バイト数。 */
    const val MAX_IMAGE_PREVIEW_BYTES = 24L * 1024 * 1024

    /** 1回の`cat`呼び出しで要求するチャンク長。サーバー側の8MiB上限より小さくして、
     *  UIの進捗表示を細かく更新できるようにする。 */
    const val CHUNK_LEN = 512L * 1024

    /**
     * [path]の内容を[maxBytes]まで読み込む。読み込むたびに[onProgress]で
     * (これまでの累積バイト列, totalSize)を通知するので、呼び出し元はComposeの
     * StateFlow/mutableStateへ逐次反映して進捗表示できる。戻り値は
     * `(全バイト列, totalSize, truncated)`。取得中にエラーが起きた場合は例外ではなく
     * [FilePreviewLoadError]をthrowする(呼び出し元がエラーメッセージをそのまま
     * UI表示できるようにするため)。
     */
    suspend fun loadBytes(
        path: String,
        maxBytes: Long,
        requestFilePreview: suspend (FilePreviewRequestKind) -> FilePreviewOutcome,
        onProgress: suspend (loaded: Long, total: Long) -> Unit = { _, _ -> },
    ): Triple<ByteArray, Long, Boolean> {
        val buffer = ByteArrayOutputStream()
        var offset = 0L
        var totalSize = 0L
        var truncated = false
        while (true) {
            val remaining = maxBytes - offset
            if (remaining <= 0) { truncated = true; break }
            val length = minOf(CHUNK_LEN, remaining)
            when (val outcome = requestFilePreview(FilePreviewRequestKind.Cat(path, offset.toULong(), length.toULong()))) {
                is FilePreviewOutcome.Cat -> {
                    buffer.write(outcome.data)
                    totalSize = outcome.totalSize.toLong()
                    offset += outcome.length.toLong()
                    onProgress(offset, totalSize)
                    if (outcome.eof) break
                    // サーバーが要求より少ないバイト数しか返さなかった(例えば`length`が
                    // 0)場合に無限ループしないためのガード。
                    if (outcome.length == 0uL) { truncated = true; break }
                }
                is FilePreviewOutcome.Error -> throw FilePreviewLoadError(outcome.message)
                else -> throw FilePreviewLoadError("unexpected response for file cat")
            }
        }
        return Triple(buffer.toByteArray(), totalSize, truncated)
    }
}

class FilePreviewLoadError(message: String) : Exception(message)
