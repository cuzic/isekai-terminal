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
     * tmux のセッション一覧を出して選んだセッションに attach/switch する。fzf があれば絞り込み
     * 検索、なければ bash 組み込みの `select` で番号選択にフォールバックする。
     *
     * ターミナルへは(スクリプトファイルとしてではなく)対話シェルへの逐次入力として送られる
     * ([SnippetCommands.toBytes] が送信するため)。そのため `exit` で接続自体を落とすような
     * 書き方や、シバン行は書かない。複数行のまま送ると、フォアグラウンドが必ずしもシェルとは
     * 限らない(ページャ実行中等)場合に一部の行が意図しない形で解釈されうるため、`;` で
     * 繋いだ1行にまとめている(adversarial review指摘、2026-08)。
     *
     * `select`/`PS3` は bash/ksh/zsh 限定(dash/fishでは動かない)——それ以外の行は
     * 意図的にPOSIX風に書いているが、fzf不在時のフォールバック経路がこの制約を持つため
     * 全体としてもこれら3シェルが既定ログインシェルであることが前提。
     */
    val TMUX_SESSION_PICKER = SnippetTemplate(
        label = "tmuxセッション選択",
        command = "if ! command -v tmux >/dev/null 2>&1; then echo 'tmux: not installed'; " +
            "else " +
            "s=\$(tmux list-sessions -F '#{session_name}' 2>/dev/null); " +
            "if [ -z \"\$s\" ]; then echo 'tmux: no sessions'; " +
            "else " +
            // 既にtmux内(`$TMUX`が非空)から`attach`すると"sessions should be nested with
            // care"エラーになるため、内側なら`switch-client`、外側なら`attach`に分岐する。
            "go() { if [ -n \"\$TMUX\" ]; then tmux switch-client -t \"\$1\"; else tmux attach -t \"\$1\"; fi; }; " +
            "if command -v fzf >/dev/null 2>&1; then " +
            "p=\$(printf '%s\\n' \"\$s\" | fzf) && go \"\$p\"; " +
            "else " +
            // `IFS`を改行のみに切り替えてから`set --`で展開することで、セッション名に
            // スペースが含まれていても(tmuxは許容する)1セッション=1要素として扱う
            // ——`select s in $s`のように無quoteで直接渡すとword-splitで壊れるため。
            "oldifs=\$IFS; IFS=\$'\\n'; set -- \$s; IFS=\$oldifs; " +
            "PS3='session> '; select p in \"\$@\"; do [ -n \"\$p\" ] && go \"\$p\"; break; done; " +
            "fi; fi; fi",
    )

    /**
     * ディスク使用量を確認する簡単な例。tmux専用ではない汎用コマンドの例として、
     * 「テンプレート」機能が特定用途専用ではなく任意の定型コマンドを登録できる一般的な
     * 仕組みであることを示す([TMUX_SESSION_PICKER] だけだと「tmux接続専用機能」に
     * 見えてしまうため、design review指摘、2026-08)。
     */
    val DISK_USAGE = SnippetTemplate(
        label = "ディスク使用量確認",
        command = "df -h",
    )

    /**
     * CPU/メモリ使用率が高い順にプロセスを並べて選択し、確認の上で `kill`(SIGTERM)する。
     * [TMUX_SESSION_PICKER] と同じ fzf/`select` フォールバック構成・1行化の理由付けを踏襲。
     *
     * `ps`の出力はPID列が右詰め(先頭に空白)なので、選択した1行から先頭フィールドを
     * 取り出す際に `${sel%% *}` のような文字列操作を使うと空文字列になってしまう
     * (先頭の空白そのものが最初の"語"として扱われるため)。代わりに `set -- $sel` の
     * 無quote展開(標準的な語分割で先頭・連続する空白を自動的に読み飛ばす)で
     * `$1`として取り出している。
     *
     * `kill`は破壊的操作のため、選んだ後も対象PIDを表示した上で `[y/N]` の確認を挟み、
     * 既定(Enterのみ等)ではキャンセル扱いにする。シグナルは常にSIGTERM(既定)——
     * プロセスに後始末の機会を与えるため。応答しないプロセスへの `-9` 等が必要な場合は
     * 手動で打ち直すことを想定し、このテンプレートではシグナル選択までは提供しない。
     */
    val KILL_HIGH_USAGE_PROCESS = SnippetTemplate(
        label = "CPU/メモリ使用率上位プロセスをkill",
        command = "list=\$(ps -eo pid,%cpu,%mem,comm --sort=-%cpu,-%mem --no-headers | head -20); " +
            "if [ -z \"\$list\" ]; then echo 'ps: no processes'; " +
            "else " +
            "if command -v fzf >/dev/null 2>&1; then " +
            "sel=\$(printf '%s\\n' \"\$list\" | fzf --prompt='kill> '); " +
            "else " +
            "oldifs=\$IFS; IFS=\$'\\n'; set -- \$list; IFS=\$oldifs; " +
            "PS3='process> '; select sel in \"\$@\"; do [ -n \"\$sel\" ] && break; done; " +
            "fi; " +
            "if [ -n \"\$sel\" ]; then " +
            "set -- \$sel; pid=\$1; " +
            "printf 'kill PID %s? [y/N] ' \"\$pid\"; read ans; " +
            "case \"\$ans\" in [Yy]*) kill \"\$pid\" && echo \"sent SIGTERM to \$pid\";; *) echo cancelled;; esac; " +
            "fi; fi",
    )

    /**
     * Claude Code運用で頻出するコマンド4種。`claude --resume`/`claude --continue`は
     * シェルプロンプトで打つコマンド(前者は過去の会話一覧から選んで再開、後者は
     * カレントディレクトリの直近の会話をそのまま継続)。`/rewind`/`/clear`は逆に、
     * 既にclaudeの対話セッション内にいる状態で打つスラッシュコマンド(前者は直前の
     * チェックポイントへ巻き戻す、後者は現在の会話コンテキストを破棄する)——
     * どちらもシェルコマンドではないため、フォアグラウンドがclaude REPLでない
     * タイミングで送っても実行はされず、単に画面に文字列として現れるだけ(壊れはしない)。
     */
    val CLAUDE_RESUME = SnippetTemplate(
        label = "claude --resume(会話一覧から再開)",
        command = "claude --resume",
    )
    val CLAUDE_CONTINUE = SnippetTemplate(
        label = "claude --continue(直前の会話を継続)",
        command = "claude --continue",
    )
    val CLAUDE_REWIND = SnippetTemplate(
        label = "/rewind(直前のチェックポイントへ巻き戻す)",
        command = "/rewind",
    )
    val CLAUDE_CLEAR = SnippetTemplate(
        label = "/clear(会話コンテキストを破棄)",
        command = "/clear",
    )

    val ALL: List<SnippetTemplate> = listOf(
        TMUX_SESSION_PICKER,
        DISK_USAGE,
        KILL_HIGH_USAGE_PROCESS,
        CLAUDE_RESUME,
        CLAUDE_CONTINUE,
        CLAUDE_REWIND,
        CLAUDE_CLEAR,
    )
}
