package tools.isekai.terminal.filepreview

import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.remember
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp

/**
 * タスク#17: シンタックスハイライト付きの生テキストビューア(Markdownビューアとは別系統
 * — こちらは常に等幅フォントで生テキストのまま表示する)。[fileName]の拡張子から
 * [SyntaxHighlighter.languageFor]で言語を推定する。
 */
@Composable
fun SyntaxHighlightedTextViewer(fileName: String, content: String, modifier: Modifier = Modifier) {
    val language = remember(fileName) { SyntaxHighlighter.languageFor(fileName) }
    val highlighted = remember(content, language) { SyntaxHighlighter.highlight(content, language) }
    Text(
        highlighted,
        modifier = modifier
            .fillMaxSize()
            .verticalScroll(rememberScrollState())
            .horizontalScroll(rememberScrollState())
            .padding(12.dp),
        fontFamily = FontFamily.Monospace,
        fontSize = 13.sp,
        color = Color(0xFFCCCCCC),
    )
}
