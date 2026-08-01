package tools.isekai.terminal.data

/**
 * 定型コマンド一覧画面の「テンプレートから追加」で選べる、アプリ同梱の雛形。
 * DB行ではなく静的データ。選んだ内容は通常の[Snippet]としてそのまま編集・保存できる。
 */
data class SnippetTemplate(
    val label: String,
    val command: String,
    val appendNewline: Boolean = true,
)

object SnippetTemplates {
    /**
     * tmux のセッション一覧を出して選んだセッションに attach する。fzf があれば絞り込み検索、
     * なければ bash 組み込みの `select` で番号選択にフォールバックする。
     *
     * ターミナルへは(スクリプトファイルとしてではなく)対話シェルへの逐次入力として送られる
     * ([SnippetCommands.toBytes] が各行末に CR を付けて送信するため)。そのため `exit` で
     * 接続自体を落とすような書き方や、シバン行は書かない。
     */
    val TMUX_SESSION_PICKER = SnippetTemplate(
        label = "tmuxセッション選択",
        command = """
            sessions=${'$'}(tmux list-sessions -F '#{session_name}' 2>/dev/null)
            if [ -z "${'$'}sessions" ]; then
              echo "tmux session not found"
            elif command -v fzf >/dev/null 2>&1; then
              s=${'$'}(printf '%s\n' "${'$'}sessions" | fzf) && tmux attach -t "${'$'}s"
            else
              PS3="session> "
              select s in ${'$'}sessions; do [ -n "${'$'}s" ] && tmux attach -t "${'$'}s"; break; done
            fi
        """.trimIndent(),
    )

    val ALL: List<SnippetTemplate> = listOf(TMUX_SESSION_PICKER)
}
