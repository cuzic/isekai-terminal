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
                "CPU/メモリ使用率上位プロセスをkill",
                "claude --resume(会話一覧から再開)",
                "claude --continue(直前の会話を継続)",
                "/rewind(直前のチェックポイントへ巻き戻す)",
                "/clear(会話コンテキストを破棄)",
                "直前のClaude応答をクリップボードへコピー",
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

    @Test
    fun claudeCopyLastReplyIsASingleLine() {
        assertFalse(SnippetTemplates.CLAUDE_COPY_LAST_REPLY.command.contains('\n'))
    }

    /**
     * claude REPL内で打つ前提のテンプレート。先頭`!`がREPLのbashモード切り替えで、
     * これが無いとシェルコマンドではなくclaudeへのプロンプトとして送られてしまう。
     */
    @Test
    fun claudeCopyLastReplyRunsAsAShellCommandInsideTheClaudeRepl() {
        assertTrue(SnippetTemplates.CLAUDE_COPY_LAST_REPLY.command.startsWith("!"))
    }

    /**
     * 切り詰め上限の64KiBは、ctl-socket経路の`MAX_CLIPBOARD_TEXT_DECODED_LEN`と、
     * `rust-core/src/terminal.rs`の`OSC_RAW_BUF_SIZE`(= `52;c;` + 64KiBのbase64長)の
     * **両方**にちょうど収まる値として選んである。溢れるとbase64のdecodeに失敗して
     * 何も起きない(サイレント失敗)ので、実際に計算して等号成立をピン留めする。
     * `terminal.rs`側を変える場合はこのテストも一緒に落ちるべき。
     */
    @Test
    fun claudeCopyLastReplyTruncatesToFitTheVteOscBuffer() {
        val command = SnippetTemplates.CLAUDE_COPY_LAST_REPLY.command
        val maxClipboardTextDecodedLen = 64 * 1024
        assertTrue(command.contains("d[:$maxClipboardTextDecodedLen]"))
        // `terminal.rs`の`OSC_RAW_BUF_SIZE`の実値(Rust側で実測)。上限いっぱいの
        // テキストを送ったときのOSCペイロード長がこれと一致する=1バイトも溢れない。
        val oscRawBufSize = 87389
        val oscPayloadLen = "52;c;".length + (maxClipboardTextDecodedLen + 2) / 3 * 4
        assertEquals(oscRawBufSize, oscPayloadLen)
    }

    /**
     * tmux配下では素のOSC 52はtmuxに横取りされて外側の端末まで届かない。DCS passthrough
     * でのラップと、tmux 3.3以降で既定offの`allow-passthrough`を有効化する手当ての両方が
     * 必要(実tmux 3.3aで、後者が無いと無音で捨てられることを確認済み)。
     */
    @Test
    fun claudeCopyLastReplyWrapsOsc52ForTmux() {
        val command = SnippetTemplates.CLAUDE_COPY_LAST_REPLY.command
        assertTrue(command.contains("Ptmux;"))
        assertTrue(command.contains("allow-passthrough on"))
    }

    /**
     * サブエージェントの会話は同じjsonlへ`isSidechain`付きで混ざって書かれるため、
     * 除外しないと「直前の応答」がサブエージェントの発言になり得る。
     */
    @Test
    fun claudeCopyLastReplyExcludesSidechainEntries() {
        assertTrue(SnippetTemplates.CLAUDE_COPY_LAST_REPLY.command.contains("not o.get(\"isSidechain\")"))
    }

    /**
     * mtime最新のjsonlだけに頼ると、同じプロジェクトディレクトリで複数のclaudeセッションを
     * 並行して動かしているときに他セッションのログを掴む(実際に再現済み)。
     * `$CLAUDE_CODE_SESSION_ID`優先・mtimeはフォールバック、の2段構えをピン留めする。
     */
    @Test
    fun claudeCopyLastReplyPrefersTheCurrentSessionIdOverMtime() {
        val command = SnippetTemplates.CLAUDE_COPY_LAST_REPLY.command
        assertTrue(command.contains("CLAUDE_CODE_SESSION_ID"))
        assertTrue(command.contains("os.path.getmtime"))
    }

    /**
     * 探索範囲を「直近の人間の発言以降」に区切らないと、tool_useのみのターンが続いたときに
     * 無制限に遡って文脈の異なる古い発言を掴む。範囲内に何も無ければ古いテキストで代用せず
     * エラーにする(誤って古い内容をコピーするより安全)。
     */
    @Test
    fun claudeCopyLastReplyScopesToTheMostRecentTurn() {
        val command = SnippetTemplates.CLAUDE_COPY_LAST_REPLY.command
        assertTrue(command.contains("R[(u[-1]+1 if u else 0):]"))
        assertTrue(command.contains("no text in the most recent turn"))
    }

    /**
     * 「人間の発言」判定の要。`type=="user"`はツール結果にも付く(実データでは`user`行の
     * 9割がこれ)ので`toolUseResult`で除外し、`<bash-input>`——**このスニペット自身の`!`実行が
     * まさにこれとして記録される**ため、除外しないと常に自分自身がアンカーになり機能が
     * 丸ごと壊れる——等の合成注入は`<`始まりでまとめて弾く。
     */
    @Test
    fun claudeCopyLastReplyDoesNotTreatToolResultsOrSyntheticInputAsAUserTurn() {
        val command = SnippetTemplates.CLAUDE_COPY_LAST_REPLY.command
        assertTrue(command.contains("not o.get(\"toolUseResult\")"))
        assertTrue(command.contains("not o.get(\"isMeta\")"))
        assertTrue(command.contains("c[:1]!=\"<\""))
    }
}
