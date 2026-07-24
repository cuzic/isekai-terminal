package tools.isekai.terminal.filepreview

import androidx.compose.foundation.background
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.rememberScrollState
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.remember
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.AnnotatedString
import androidx.compose.ui.text.SpanStyle
import androidx.compose.ui.text.buildAnnotatedString
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontStyle
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.withStyle
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import tools.isekai.terminal.ui.AppColors

/**
 * タスク#17: `**bold**`/`*italic*`・`_italic_`・`` `code` ``のインライン装飾を
 * [AnnotatedString]へ変換する。[MarkdownParser]が担うブロック分割とは独立した
 * 純粋関数(テスト容易性のためComposable本体から切り出している)。
 */
fun renderInlineMarkdown(text: String): AnnotatedString = buildAnnotatedString {
    var i = 0
    while (i < text.length) {
        when {
            text.startsWith("**", i) -> {
                val end = text.indexOf("**", i + 2)
                if (end == -1) { append(text.substring(i)); i = text.length } else {
                    withStyle(SpanStyle(fontWeight = FontWeight.Bold)) { append(text.substring(i + 2, end)) }
                    i = end + 2
                }
            }
            text.startsWith("`", i) -> {
                val end = text.indexOf('`', i + 1)
                if (end == -1) { append(text.substring(i)); i = text.length } else {
                    withStyle(SpanStyle(fontFamily = FontFamily.Monospace, background = Color(0xFF2A2A3E))) {
                        append(text.substring(i + 1, end))
                    }
                    i = end + 1
                }
            }
            text[i] == '*' || text[i] == '_' -> {
                val marker = text[i]
                val end = text.indexOf(marker, i + 1)
                if (end == -1 || end == i + 1) { append(text[i]); i++ } else {
                    withStyle(SpanStyle(fontStyle = FontStyle.Italic)) { append(text.substring(i + 1, end)) }
                    i = end + 1
                }
            }
            else -> { append(text[i]); i++ }
        }
    }
}

@Composable
fun MarkdownViewer(source: String, modifier: Modifier = Modifier) {
    val blocks = remember(source) { MarkdownParser.parse(source) }
    LazyColumn(modifier = modifier.fillMaxWidth(), verticalArrangement = Arrangement.spacedBy(6.dp)) {
        items(blocks) { block -> MarkdownBlockView(block) }
    }
}

@Composable
private fun MarkdownBlockView(block: MarkdownBlock) {
    when (block) {
        is MarkdownBlock.Heading -> {
            val style = when (block.level) {
                1 -> MaterialTheme.typography.headlineSmall
                2 -> MaterialTheme.typography.titleLarge
                3 -> MaterialTheme.typography.titleMedium
                else -> MaterialTheme.typography.titleSmall
            }
            Text(renderInlineMarkdown(block.text), style = style, color = AppColors.MutedText)
        }
        is MarkdownBlock.Paragraph ->
            Text(renderInlineMarkdown(block.text), color = AppColors.MutedText, fontSize = 14.sp)
        is MarkdownBlock.CodeBlock ->
            Row(Modifier.fillMaxWidth().background(Color(0xFF1A1A2E)).padding(8.dp).horizontalScroll(rememberScrollState())) {
                Text(
                    block.code,
                    fontFamily = FontFamily.Monospace,
                    fontSize = 13.sp,
                    color = Color(0xFFCCCCCC),
                )
            }
        is MarkdownBlock.ListItem ->
            Row(Modifier.fillMaxWidth().padding(start = 8.dp)) {
                Text(if (block.ordered) "• " else "• ", color = AppColors.SecondaryText)
                Text(renderInlineMarkdown(block.text), color = AppColors.MutedText, fontSize = 14.sp)
            }
        is MarkdownBlock.BlockQuote ->
            Row(Modifier.fillMaxWidth().background(Color(0xFF1A1A2E)).padding(8.dp)) {
                Text(renderInlineMarkdown(block.text), color = AppColors.SecondaryText, fontStyle = FontStyle.Italic, fontSize = 14.sp)
            }
        MarkdownBlock.HorizontalRule ->
            Column(Modifier.fillMaxWidth().padding(vertical = 4.dp).height(1.dp).background(Color(0xFF444455))) {}
    }
}
