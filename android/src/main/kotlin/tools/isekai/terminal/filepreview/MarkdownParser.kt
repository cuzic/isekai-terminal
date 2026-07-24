package tools.isekai.terminal.filepreview

/**
 * タスク#17: Markdownビューア用の最小限のブロックレベルパーサー。Wave
 * Terminal/Tabby相当の「よく使う要素が読める」プレビューを目標としており、CommonMark
 * 完全準拠は狙わない(見出し・段落・フェンス付きコードブロック・箇条書き・番号付き
 * リスト・引用・水平線のみ)。インライン装飾(bold/italic/code)は[MarkdownParser]の
 * 対象外で、描画側([tools.isekai.terminal.filepreview.renderInlineMarkdown]、
 * `MarkdownViewer.kt`)が担当する。
 */
sealed class MarkdownBlock {
    data class Heading(val level: Int, val text: String) : MarkdownBlock()
    data class Paragraph(val text: String) : MarkdownBlock()
    data class CodeBlock(val language: String?, val code: String) : MarkdownBlock()
    data class ListItem(val ordered: Boolean, val text: String) : MarkdownBlock()
    data class BlockQuote(val text: String) : MarkdownBlock()
    object HorizontalRule : MarkdownBlock()
}

object MarkdownParser {
    private val headingRegex = Regex("^(#{1,6})\\s+(.*)$")
    private val unorderedListRegex = Regex("^\\s*[-*+]\\s+(.*)$")
    private val orderedListRegex = Regex("^\\s*\\d+\\.\\s+(.*)$")
    private val horizontalRuleRegex = Regex("^\\s*([-*_])\\1{2,}\\s*$")

    fun parse(source: String): List<MarkdownBlock> {
        val lines = source.replace("\r\n", "\n").split("\n")
        val blocks = mutableListOf<MarkdownBlock>()
        val paragraph = StringBuilder()

        fun flushParagraph() {
            if (paragraph.isNotEmpty()) {
                blocks.add(MarkdownBlock.Paragraph(paragraph.toString().trim()))
                paragraph.clear()
            }
        }

        var i = 0
        while (i < lines.size) {
            val line = lines[i]
            val heading = headingRegex.find(line)
            val unordered = unorderedListRegex.find(line)
            val ordered = orderedListRegex.find(line)
            when {
                line.startsWith("```") -> {
                    flushParagraph()
                    val language = line.removePrefix("```").trim().ifEmpty { null }
                    val code = StringBuilder()
                    i++
                    while (i < lines.size && !lines[i].startsWith("```")) {
                        if (code.isNotEmpty()) code.append('\n')
                        code.append(lines[i])
                        i++
                    }
                    blocks.add(MarkdownBlock.CodeBlock(language, code.toString()))
                    // ループ末尾のi++で閉じ```(またはEOF)を読み飛ばす
                }
                heading != null -> {
                    flushParagraph()
                    blocks.add(MarkdownBlock.Heading(heading.groupValues[1].length, heading.groupValues[2].trim()))
                }
                horizontalRuleRegex.matches(line) -> {
                    flushParagraph()
                    blocks.add(MarkdownBlock.HorizontalRule)
                }
                unordered != null -> {
                    flushParagraph()
                    blocks.add(MarkdownBlock.ListItem(ordered = false, text = unordered.groupValues[1].trim()))
                }
                ordered != null -> {
                    flushParagraph()
                    blocks.add(MarkdownBlock.ListItem(ordered = true, text = ordered.groupValues[1].trim()))
                }
                line.trimStart().startsWith(">") -> {
                    flushParagraph()
                    blocks.add(MarkdownBlock.BlockQuote(line.trimStart().removePrefix(">").trim()))
                }
                line.isBlank() -> flushParagraph()
                else -> {
                    if (paragraph.isNotEmpty()) paragraph.append(' ')
                    paragraph.append(line.trim())
                }
            }
            i++
        }
        flushParagraph()
        return blocks
    }
}
