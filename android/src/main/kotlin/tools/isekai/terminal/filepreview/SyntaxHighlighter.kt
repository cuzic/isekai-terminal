package tools.isekai.terminal.filepreview

import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.AnnotatedString
import androidx.compose.ui.text.SpanStyle
import androidx.compose.ui.text.buildAnnotatedString
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.withStyle

/**
 * タスク#17: シンタックスハイライト付きテキストビューア用の、拡張子ベースの最小限
 * ハイライター。フル言語文法ではなく「コメント/文字列/数値/キーワード」の4種類だけを
 * 塗り分ける単純なトークナイザー(エディタの「素朴な」ハイライターと同じ設計)。
 * [MarkdownViewer]のレンダリング(HTML風のブロック表示)とは別系統で、こちらは常に
 * 生テキストを等幅フォントでそのまま表示する。
 */
object SyntaxHighlighter {
    private val keywordsByLanguage: Map<String, Set<String>> = mapOf(
        "kt" to setOf(
            "fun", "val", "var", "if", "else", "when", "for", "while", "do", "class", "object", "interface",
            "import", "package", "return", "is", "in", "null", "true", "false", "private", "public", "internal",
            "protected", "override", "companion", "data", "sealed", "suspend", "typealias", "throw", "try", "catch",
            "finally", "as", "this", "super", "vararg",
        ),
        "rs" to setOf(
            "fn", "let", "mut", "if", "else", "match", "for", "while", "loop", "struct", "enum", "impl", "trait",
            "use", "mod", "pub", "return", "true", "false", "self", "Self", "async", "await", "const", "static",
            "unsafe", "where", "dyn", "move",
        ),
        "py" to setOf(
            "def", "class", "if", "elif", "else", "for", "while", "import", "from", "return", "True", "False",
            "None", "try", "except", "finally", "with", "as", "lambda", "yield", "async", "await", "pass", "break",
            "continue", "raise", "not", "and", "or", "in", "is",
        ),
        "js" to setOf(
            "function", "const", "let", "var", "if", "else", "for", "while", "return", "class", "import", "export",
            "new", "this", "true", "false", "null", "undefined", "async", "await", "try", "catch", "finally",
            "typeof", "instanceof", "default", "extends", "static",
        ),
        "ts" to setOf(
            "function", "const", "let", "var", "if", "else", "for", "while", "return", "class", "import", "export",
            "new", "this", "true", "false", "null", "undefined", "async", "await", "interface", "type", "enum",
            "implements", "extends", "public", "private", "protected", "readonly", "static", "as",
        ),
        "java" to setOf(
            "public", "private", "protected", "class", "interface", "if", "else", "for", "while", "return", "new",
            "import", "package", "static", "final", "void", "true", "false", "null", "extends", "implements",
            "try", "catch", "finally", "throw", "throws", "this", "super",
        ),
        "go" to setOf(
            "func", "package", "import", "if", "else", "for", "range", "return", "var", "const", "type", "struct",
            "interface", "true", "false", "nil", "go", "defer", "chan", "select", "map", "switch", "case",
        ),
        "sh" to setOf(
            "if", "then", "else", "elif", "fi", "for", "do", "done", "while", "function", "echo", "export", "local",
            "return", "case", "esac", "in", "until",
        ),
        "c" to setOf(
            "int", "char", "float", "double", "void", "if", "else", "for", "while", "return", "struct", "typedef",
            "static", "const", "unsigned", "long", "short", "switch", "case", "break", "continue",
        ),
        "cpp" to setOf(
            "int", "char", "float", "double", "void", "if", "else", "for", "while", "return", "struct", "class",
            "namespace", "template", "public", "private", "protected", "new", "delete", "using", "const", "static",
            "virtual", "override",
        ),
        "toml" to emptySet(),
        "yaml" to emptySet(),
        "xml" to emptySet(),
        "css" to emptySet(),
        "json" to setOf("true", "false", "null"),
    )

    private val extensionToLanguage: Map<String, String> = mapOf(
        "kt" to "kt", "kts" to "kt",
        "rs" to "rs",
        "py" to "py",
        "json" to "json",
        "js" to "js", "jsx" to "js", "mjs" to "js", "cjs" to "js",
        "ts" to "ts", "tsx" to "ts",
        "java" to "java",
        "go" to "go",
        "sh" to "sh", "bash" to "sh", "zsh" to "sh",
        "c" to "c", "h" to "c",
        "cpp" to "cpp", "cc" to "cpp", "cxx" to "cpp", "hpp" to "cpp", "hxx" to "cpp",
        "toml" to "toml",
        "yaml" to "yaml", "yml" to "yaml",
        "xml" to "xml", "html" to "xml", "htm" to "xml",
        "css" to "css",
    )

    /** ファイル名から言語を推定する。未知の拡張子は`null`(キーワード無しの色分けのみ)。 */
    fun languageFor(fileName: String): String? =
        extensionToLanguage[fileName.substringAfterLast('.', "").lowercase()]

    private val hashCommentLanguages = setOf("py", "sh", "toml", "yaml")

    fun highlight(text: String, language: String?): AnnotatedString = buildAnnotatedString {
        val keywords = keywordsByLanguage[language] ?: emptySet()
        val commentPrefix = if (language in hashCommentLanguages) "#" else "//"
        var i = 0
        while (i < text.length) {
            val c = text[i]
            when {
                commentPrefix.isNotEmpty() && text.startsWith(commentPrefix, i) -> {
                    val end = text.indexOf('\n', i).let { if (it == -1) text.length else it }
                    withStyle(SpanStyle(color = CommentColor)) { append(text.substring(i, end)) }
                    i = end
                }
                c == '"' || c == '\'' -> {
                    val end = findStringEnd(text, c, i + 1)
                    withStyle(SpanStyle(color = StringColor)) { append(text.substring(i, minOf(end + 1, text.length))) }
                    i = end + 1
                }
                c.isDigit() -> {
                    var end = i
                    while (end < text.length && (text[end].isDigit() || text[end] == '.')) end++
                    withStyle(SpanStyle(color = NumberColor)) { append(text.substring(i, end)) }
                    i = end
                }
                c.isLetter() || c == '_' -> {
                    var end = i
                    while (end < text.length && (text[end].isLetterOrDigit() || text[end] == '_')) end++
                    val word = text.substring(i, end)
                    if (word in keywords) {
                        withStyle(SpanStyle(color = KeywordColor, fontWeight = FontWeight.Bold)) { append(word) }
                    } else {
                        append(word)
                    }
                    i = end
                }
                else -> { append(c); i++ }
            }
        }
    }

    private fun findStringEnd(text: String, quote: Char, from: Int): Int {
        var i = from
        while (i < text.length) {
            when {
                text[i] == '\\' -> i += 2
                text[i] == quote -> return i
                text[i] == '\n' -> return i - 1
                else -> i++
            }
        }
        return text.length
    }

    private val CommentColor = Color(0xFF6A9955)
    private val StringColor = Color(0xFFCE9178)
    private val NumberColor = Color(0xFFB5CEA8)
    private val KeywordColor = Color(0xFF569CD6)
}
