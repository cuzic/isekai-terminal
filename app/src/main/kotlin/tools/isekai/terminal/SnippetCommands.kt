package tools.isekai.terminal

import tools.isekai.terminal.data.Snippet

/**
 * スニペット／接続後自動実行コマンドの文字列をターミナルへ送信するバイト列に変換する純粋関数群。
 * Android/Rust いずれにも依存しないため単体テストが容易。
 */
object SnippetCommands {
    /**
     * [command] の各行区切り (`\n`, `\r\n`) を `\r` (CR) に正規化して UTF-8 バイト列にする。
     * [appendNewline] が true かつ末尾がまだ `\r` で終わっていなければ、末尾にも `\r` を追加する
     * （＝最後の行も Enter される）。false なら最後の行は Enter されずに残る。
     */
    fun toBytes(command: String, appendNewline: Boolean = true): ByteArray {
        if (command.isEmpty()) return ByteArray(0)
        val normalized = command.replace("\r\n", "\n").replace('\n', '\r')
        val withTrailing =
            if (appendNewline && !normalized.endsWith('\r')) normalized + '\r' else normalized
        return withTrailing.toByteArray(Charsets.UTF_8)
    }

    fun toBytes(snippet: Snippet): ByteArray = toBytes(snippet.command, snippet.appendNewline)
}
