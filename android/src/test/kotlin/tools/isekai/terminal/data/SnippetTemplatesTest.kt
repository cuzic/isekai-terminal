package tools.isekai.terminal.data

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * [SnippetTemplates]は[SnippetCommands.toBytes]経由で対話シェルへ逐次キー入力として
 * 送られる([SnippetTemplates.TMUX_SESSION_PICKER]のKDoc参照)。ここでは実際のシェル実行
 * ではなく、その前提が壊れていないことだけを検証する。
 */
class SnippetTemplatesTest {
    @Test
    fun allContainsEveryDeclaredTemplate() {
        assertEquals(
            listOf(
                "tmuxセッション選択",
                "ディスク使用量確認",
                "CPU/メモリ使用率上位プロセスをkill",
                "claude --resume(会話一覧から再開)",
                "claude --continue(直前の会話を継続)",
                "/rewind(直前のチェックポイントへ巻き戻す)",
                "/clear(会話コンテキストを破棄)",
            ),
            SnippetTemplates.ALL.map { it.label },
        )
    }

    /**
     * 複数行のまま送ると、フォアグラウンドが必ずしもシェルとは限らない(ページャ実行中等)
     * 場合に一部の行が意図しない形で解釈されうる(adversarial review指摘、2026-08)。
     * 改行を含まない1行であることを直接ピン留めする。
     */
    @Test
    fun tmuxSessionPickerIsASingleLine() {
        assertFalse(SnippetTemplates.TMUX_SESSION_PICKER.command.contains('\n'))
    }

    @Test
    fun tmuxSessionPickerHandlesBeingAlreadyInsideTmux() {
        // 内側から`attach`すると"sessions should be nested with care"エラーになるため、
        // `$TMUX`(非空ならtmux内)を見て`switch-client`に分岐する必要がある。
        assertTrue(SnippetTemplates.TMUX_SESSION_PICKER.command.contains("switch-client"))
    }

    @Test
    fun killHighUsageProcessIsASingleLine() {
        assertFalse(SnippetTemplates.KILL_HIGH_USAGE_PROCESS.command.contains('\n'))
    }

    /** kill は破壊的操作のため、無条件実行ではなく確認ステップを経由することをピン留めする。 */
    @Test
    fun killHighUsageProcessAsksForConfirmationBeforeKilling() {
        val command = SnippetTemplates.KILL_HIGH_USAGE_PROCESS.command
        assertTrue(command.contains("[y/N]"))
        assertTrue(command.contains("case \"\$ans\" in [Yy]*)"))
    }
}
