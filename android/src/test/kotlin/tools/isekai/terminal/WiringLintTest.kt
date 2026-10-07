package tools.isekai.terminal

import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test
import java.io.File

/**
 * `docs/adr/0020-unwired-callback-detection.md` §3(g)(Phase 2)の再発防止ガード(字句検査)。
 *
 * 1. **既定no-op lambdaの禁止**: 登録した宣言([noDefaultLambdas])の関数型パラメータ/プロパティに
 *    `= { ... }`の既定値を書かせない。ce214ef5(`onNotify`の既定no-opに吸われ、渡し忘れがコンパイルを
 *    通った)と同じ形の再発を止める。範囲はファイルではなく宣言単位(R2-4): 宣言の開き括弧から対応する
 *    閉じ括弧までをコメント除去・空白正規化してから照合するので、型注釈と`= {`が改行で分かれてもすり抜けない。
 * 2. **UniFFIの反射呼び出しの禁止**: Android本番ソースで`Class.forName("uniffi.`を拒否する
 *    (3965e322: 反射+無言`runCatching`で、どの失敗でも無言no-opになっていた)。
 *
 * ADRはこれを`scripts/check-wiring-lint.py`+新workflowとして提案したが、Androidの検査だけを
 * 先に入れる本PRでは、同じ規則をrequired `android-unit-test`内のJVMテストとして置く
 * (Pythonスクリプト・iOSの字句レポート・workflowは後続、PR本文参照)。`--self-test`相当は
 * [selfTest_detectsKnownBadShapes_andIgnoresSafeOnes]。
 */
class WiringLintTest {

    /** (ソースルートからの相対パス, 宣言の開始マーカー, 既定値を許す項目)。 */
    private data class Declaration(val path: String, val marker: String, val allow: Set<String> = emptySet())

    private val noDefaultLambdas = listOf(
        // orchestratorFactoryの既定値はno-opではなく本番の既定実装(createSessionOrchestrator)で、
        // 本番(TerminalTabsViewModel)が依存しているので明示的に許可する(ADR §3(g)、R2-4)。
        Declaration("tools/isekai/terminal/session/TerminalSession.kt", "class TerminalSession(", allow = setOf("orchestratorFactory")),
        Declaration("tools/isekai/terminal/session/NetworkPathMonitor.kt", "fun start("),
        Declaration("tools/isekai/terminal/TerminalScreen.kt", "data class TerminalScreenActions("),
    )

    private fun androidSrc(sub: String): File =
        listOf(File("src/$sub"), File("android/src/$sub")).firstOrNull { it.isDirectory }
            ?: error("android/src/$sub が見つからない(作業ディレクトリ=${File(".").absolutePath})")

    @Test
    fun registeredDeclarations_haveNoDefaultLambdas() {
        val root = androidSrc("main/kotlin")
        val violations = noDefaultLambdas.flatMap { d ->
            val file = File(root, d.path)
            assertTrue("登録した宣言のファイルが無い: ${file.path}", file.isFile)
            defaultLambdaParams(file.readText(), d.marker).filter { it !in d.allow }.map { "${d.path} `${d.marker}`: $it" }
        }
        assertTrue(
            "関数型パラメータに既定lambdaを書かないこと(渡し忘れが既定no-opに吸われる配線漏れの原因、ADR §3(g)):\n" +
                violations.joinToString("\n"),
            violations.isEmpty(),
        )
    }

    @Test
    fun androidSources_doNotCallUniffiThroughReflection() {
        val hits = listOf("main", "debug").map { androidSrc(it) }.flatMap { dir ->
            dir.walkTopDown().filter { it.isFile && it.extension == "kt" }.flatMap { f ->
                f.readLines().withIndex()
                    .filter { (_, line) -> line.contains("Class.forName(\"uniffi.") }
                    .map { (i, _) -> "${f.path}:${i + 1}" }
            }
        }
        assertTrue("UniFFIを反射で呼ばないこと(3965e322型の無言no-op、生成済みバインディングを直接呼ぶ):\n${hits.joinToString("\n")}", hits.isEmpty())
    }

    @Test
    fun selfTest_detectsKnownBadShapes_andIgnoresSafeOnes() {
        val src = """
            class Bad(
                private val a: () -> Unit = {},
                /** 型注釈と`= {`の間に改行やコメントがあってもすり抜けない */
                val b: (Int, Boolean) -> Unit =
                    { _, _ -> },
                val c: suspend (String) -> Int = // trailing comment
                    { 0 },
                private val ok1: Int = 0,
                val ok2: Map<String, Pair<Int, Int>> = emptyMap(),
                val ok3: () -> Unit,
                val ok4: String = "= { not a lambda }",
                val ok5: Boolean = { true }(),
            ) { fun start(x: () -> Unit = {}) {} }
        """.trimIndent()
        assertEquals(listOf("a", "b", "c"), defaultLambdaParams(src, "class Bad("))
        assertEquals(listOf("x"), defaultLambdaParams(src, "fun start("))
    }

    // ── 字句処理 ─────────────────────────────────────────────

    /** [marker]で始まる宣言のパラメータのうち、関数型(`->`を含む型)で既定値が`{`から始まるものの名前。 */
    private fun defaultLambdaParams(source: String, marker: String): List<String> {
        val code = stripCommentsAndStrings(source)
        val start = code.indexOf(marker)
        require(start >= 0) { "宣言が見つからない: $marker" }
        val open = start + marker.length - 1
        val close = matchingParen(code, open)
        return splitTopLevel(code.substring(open + 1, close)).mapNotNull { param ->
            val eq = topLevelAssign(param) ?: return@mapNotNull null
            val decl = param.substring(0, eq)
            val default = param.substring(eq + 1).trim()
            val name = Regex("""(\w+)\s*:""").find(decl)?.groupValues?.get(1) ?: return@mapNotNull null
            name.takeIf { "->" in decl && default.startsWith("{") }
        }
    }

    /** コメントを除去し、文字列リテラルの中身を空にする(`"= {"`のような文字列で誤検出しないため)。 */
    private fun stripCommentsAndStrings(s: String): String {
        val out = StringBuilder()
        var i = 0
        while (i < s.length) {
            when {
                s.startsWith("//", i) -> { while (i < s.length && s[i] != '\n') i++ }
                s.startsWith("/*", i) -> { val end = s.indexOf("*/", i + 2); i = if (end < 0) s.length else end + 2; out.append(' ') }
                s.startsWith("\"\"\"", i) -> { val end = s.indexOf("\"\"\"", i + 3); i = if (end < 0) s.length else end + 3; out.append("\"\"") }
                s[i] == '"' -> {
                    i++
                    while (i < s.length && s[i] != '"') { if (s[i] == '\\') i++; i++ }
                    i++
                    out.append("\"\"")
                }
                s[i] == '\'' && i + 2 < s.length && (s[i + 2] == '\'' || s[i + 1] == '\\') -> {
                    val end = s.indexOf('\'', i + 2)
                    i = if (end < 0) s.length else end + 1
                    out.append("' '")
                }
                else -> { out.append(s[i]); i++ }
            }
        }
        return out.toString()
    }

    private fun matchingParen(s: String, open: Int): Int {
        var depth = 0
        for (i in open until s.length) {
            when (s[i]) {
                '(' -> depth++
                ')' -> { depth--; if (depth == 0) return i }
            }
        }
        error("対応する閉じ括弧が無い")
    }

    /** 括弧・波括弧・角括弧・型引数(`->`の`>`は除く)の外側のカンマで分割する。 */
    private fun splitTopLevel(s: String): List<String> {
        val parts = mutableListOf<String>()
        var depth = 0
        var last = 0
        for (i in s.indices) {
            val c = s[i]
            when {
                c == '(' || c == '{' || c == '[' || c == '<' -> depth++
                c == ')' || c == '}' || c == ']' -> depth--
                c == '>' && (i == 0 || s[i - 1] != '-') -> depth--
                c == ',' && depth == 0 -> { parts += s.substring(last, i); last = i + 1 }
            }
        }
        parts += s.substring(last)
        return parts.filter { it.isNotBlank() }
    }

    /** トップレベルの代入`=`(`->`・`==`・`>=`等ではないもの)の位置。 */
    private fun topLevelAssign(param: String): Int? {
        var depth = 0
        for (i in param.indices) {
            val c = param[i]
            when {
                c == '(' || c == '{' || c == '[' || c == '<' -> depth++
                c == ')' || c == '}' || c == ']' -> depth--
                c == '>' && (i == 0 || param[i - 1] != '-') -> depth--
                c == '=' && depth == 0 -> {
                    val prev = if (i > 0) param[i - 1] else ' '
                    val next = if (i + 1 < param.length) param[i + 1] else ' '
                    if (next != '=' && next != '>' && prev !in "=!<>") return i
                }
            }
        }
        return null
    }
}
