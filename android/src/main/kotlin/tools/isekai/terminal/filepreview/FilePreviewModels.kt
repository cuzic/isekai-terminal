package tools.isekai.terminal.filepreview

import uniffi.isekai_terminal_core.FilePreviewEntry

/**
 * タスク#17(ファイルプレビュー機能): 拡張子から判定するビューア種別。ファイル内容を
 * 一切見ずファイル名だけで機械的に決まる純粋な判定であり、リモート/セッション状態には
 * 依存しないため`.claude/rules/rust-ssot.md`の対象外(UI表示専用の判断)。
 */
enum class FilePreviewKind { MARKDOWN, IMAGE, CSV, TEXT }

object FilePreviewKindDetector {
    private val markdownExtensions = setOf("md", "markdown", "mdown", "mkd")
    private val imageExtensions = setOf("png", "jpg", "jpeg", "gif", "bmp", "webp")
    private val csvExtensions = setOf("csv", "tsv")

    fun detect(fileName: String): FilePreviewKind {
        val ext = fileName.substringAfterLast('.', "").lowercase()
        return when (ext) {
            in markdownExtensions -> FilePreviewKind.MARKDOWN
            in imageExtensions -> FilePreviewKind.IMAGE
            in csvExtensions -> FilePreviewKind.CSV
            else -> FilePreviewKind.TEXT
        }
    }
}

/** ディレクトリブラウザ1画面分のUI状態(ナビゲーション状態、rust-ssot.md例外)。 */
data class FileBrowserUiState(
    val currentPath: String = "~",
    val entries: List<FilePreviewEntry> = emptyList(),
    val isLoading: Boolean = false,
    val errorMessage: String? = null,
)

/** 開いているファイルビューア1件分のUI状態。 */
data class FileViewerUiState(
    val path: String,
    val kind: FilePreviewKind,
    val isLoading: Boolean = true,
    val errorMessage: String? = null,
    val textContent: String? = null,
    val imageBytes: ByteArray? = null,
    val totalSize: Long = 0,
    val loadedBytes: Long = 0,
    /** 表示上限([FilePreviewLoader.MAX_TEXT_PREVIEW_BYTES]/[FilePreviewLoader.MAX_IMAGE_PREVIEW_BYTES])
     *  に達し、ファイル全体を読み切る前に打ち切った場合`true`。 */
    val truncated: Boolean = false,
) {
    override fun equals(other: Any?): Boolean {
        if (this === other) return true
        if (other !is FileViewerUiState) return false
        return path == other.path && kind == other.kind && isLoading == other.isLoading &&
            errorMessage == other.errorMessage && textContent == other.textContent &&
            (imageBytes?.contentEquals(other.imageBytes) ?: (other.imageBytes == null)) &&
            totalSize == other.totalSize && loadedBytes == other.loadedBytes && truncated == other.truncated
    }

    override fun hashCode(): Int {
        var result = path.hashCode()
        result = 31 * result + kind.hashCode()
        result = 31 * result + isLoading.hashCode()
        result = 31 * result + (errorMessage?.hashCode() ?: 0)
        result = 31 * result + (textContent?.hashCode() ?: 0)
        result = 31 * result + (imageBytes?.contentHashCode() ?: 0)
        result = 31 * result + totalSize.hashCode()
        result = 31 * result + loadedBytes.hashCode()
        result = 31 * result + truncated.hashCode()
        return result
    }
}

/** POSIXパスの単純な文字列結合/親ディレクトリ計算。リモートに問い合わせる必要のない
 *  純粋なUIナビゲーション操作のため、Rust側に置く必要はない(rust-ssot.mdの例外)。 */
object FilePreviewPaths {
    fun join(dir: String, name: String): String =
        if (dir.endsWith("/")) "$dir$name" else "$dir/$name"

    fun parent(path: String): String {
        val trimmed = path.trimEnd('/')
        if (trimmed.isEmpty() || !trimmed.contains('/')) return "/"
        val idx = trimmed.lastIndexOf('/')
        return if (idx == 0) "/" else trimmed.substring(0, idx)
    }
}
