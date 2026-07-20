package tools.isekai.terminal.filepreview

import androidx.compose.foundation.background
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.rememberScrollState
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.remember
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp

/**
 * タスク#17: CSV/TSVビューア。[CsvParser]でパースした行を表形式(1行目をヘッダー扱い)で
 * 表示する。列数が揃っていない行(壊れたCSV)でも例外を投げずそのまま表示する
 * (`getOrNull`で欠けたセルは空文字扱い)。
 */
@Composable
fun CsvViewer(fileName: String, content: String, modifier: Modifier = Modifier) {
    val rows = remember(fileName, content) { CsvParser.parse(content, CsvParser.delimiterFor(fileName)) }
    if (rows.isEmpty()) {
        Text("空のファイルです", color = Color(0xFFAAAAAA), modifier = modifier.padding(12.dp))
        return
    }
    val columnCount = remember(rows) { rows.maxOf { it.size } }
    val horizontalScrollState = rememberScrollState()
    LazyColumn(modifier = modifier) {
        items(rows.size) { rowIndex ->
            val row = rows[rowIndex]
            Row(Modifier.fillMaxWidth().horizontalScroll(horizontalScrollState)) {
                for (col in 0 until columnCount) {
                    val cell = row.getOrNull(col) ?: ""
                    Text(
                        cell,
                        modifier = Modifier
                            .width(140.dp)
                            .background(if (rowIndex == 0) Color(0xFF2A2A3E) else Color.Transparent)
                            .padding(horizontal = 8.dp, vertical = 6.dp),
                        color = Color(0xFFCCCCCC),
                        fontWeight = if (rowIndex == 0) FontWeight.Bold else FontWeight.Normal,
                        fontSize = 13.sp,
                        maxLines = 1,
                    )
                }
            }
        }
    }
}
