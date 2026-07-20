package tools.isekai.terminal.filepreview

/**
 * タスク#17: CSV/TSVビューア用の最小限のRFC4180風パーサー。引用符囲み・エスケープ
 * された引用符(`""`)・区切り文字違い(.tsvはタブ)のみ対応する。
 */
object CsvParser {
    fun parse(text: String, delimiter: Char = ','): List<List<String>> {
        val rows = mutableListOf<List<String>>()
        var row = mutableListOf<String>()
        val field = StringBuilder()
        var inQuotes = false
        var i = 0
        var sawAnyContent = false
        while (i < text.length) {
            val c = text[i]
            when {
                inQuotes -> {
                    when {
                        c == '"' && i + 1 < text.length && text[i + 1] == '"' -> { field.append('"'); i++ }
                        c == '"' -> inQuotes = false
                        else -> field.append(c)
                    }
                }
                c == '"' -> { inQuotes = true; sawAnyContent = true }
                c == delimiter -> { row.add(field.toString()); field.clear(); sawAnyContent = true }
                c == '\r' -> {}
                c == '\n' -> {
                    row.add(field.toString())
                    field.clear()
                    rows.add(row)
                    row = mutableListOf()
                    sawAnyContent = false
                }
                else -> { field.append(c); sawAnyContent = true }
            }
            i++
        }
        if (sawAnyContent || field.isNotEmpty() || row.isNotEmpty()) {
            row.add(field.toString())
            rows.add(row)
        }
        return rows
    }

    /** ファイル名の拡張子から区切り文字を決める(`.tsv`はタブ、それ以外はカンマ)。 */
    fun delimiterFor(fileName: String): Char =
        if (fileName.substringAfterLast('.', "").lowercase() == "tsv") '\t' else ','
}
