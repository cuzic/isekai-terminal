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

    /**
     * 直前のClaude Codeの応答を端末(Android)のクリップボードへコピーする。
     * [CLAUDE_REWIND]/[CLAUDE_CLEAR]と同じく「既にclaudeの対話REPL内にいる状態」で打つ
     * 前提だが、スラッシュコマンドではなく先頭`!`——claude REPLはこれをREPLから抜けずに
     * 生シェルコマンドとして実行する。REPL外(素のシェル)で誤って送った場合は
     * `!`始まりのコマンドとして解釈され`command not found`相当になるだけで壊れはしない。
     *
     * **会話ログの特定**: claudeは会話を`~/.claude/projects/<cwdの/を-に置換>/<uuid>.jsonl`
     * へ書く。`$CLAUDE_CODE_SESSION_ID`が現在のセッションのuuidそのものなので、これが
     * 立っていて対応するjsonlが実在すればそれを使う。無ければそのディレクトリ内で
     * **mtimeが最も新しい`*.jsonl`**へフォールバックする。この2段構えは必須で、mtime
     * だけに頼ると**同じプロジェクトディレクトリで複数のclaudeセッションを並行して
     * 動かしているとき、他人のセッションのログを掴む**(実際にこの手順で、無関係な
     * 並行セッションの発言をコピーする再現を確認済み)。
     *
     * **応答の抽出**: 各行が1 JSONオブジェクト。まず`isSidechain`が真の行を落とし
     * (サブエージェントの会話が同じjsonlへ混ざって書かれるため)、**直近の「人間の
     * 発言」以降**に限定して`type=="assistant"`の`message.content`から`type=="text"`
     * ブロックを連結し、その最後の非空のものを採る。範囲を区切らないと、tool_useのみで
     * テキストを伴わないターンが続いたときに無制限に遡って文脈の異なる古い発言を掴む。
     * 範囲内に非空テキストが無ければ、古いテキストで代用せずエラーで終わる。
     *
     * 「人間の発言」の判定が肝で、`type=="user"`だけでは全く足りない——
     * ツール結果も`type=="user"`として記録される(実データでは`user`行の9割がこれ)ので
     * `toolUseResult`キーを持つ行を除外し、さらに`<bash-input>`(**このスニペット自身の
     * `!`実行がまさにこれとして記録される**——除外しないと常に自分自身がアンカーになり
     * 機能が丸ごと壊れる)・`<task-notification>`・`<local-command-stdout>`等の合成
     * 注入も除外する(いずれも文字列contentが`<`始まりなのでまとめて弾く)。`isMeta`も同様。
     * アンカーは「これより古くは遡らない」下限にすぎず、範囲内では常に最新を採るので、
     * 判定を外して古い側に倒れても誤った新しい内容を掴むことはない。
     *
     * 書き込み途中の行を掴んでも壊れないよう、`{`で始まり`}`で終わる行だけをパース対象に
     * する(1行制約下でtry/exceptを書かずに済ませる意図も兼ねる)。
     *
     * **転送経路**: 主経路はOSC 52(`ESC]52;c;<base64>BEL`、`terminal.rs`がパースして
     * `on_clipboard_write`へ回す)。tmux配下(`$TMUX`が非空)では素のOSC 52はtmuxに
     * 横取りされ外側の端末まで届かないため、DCS passthrough(`ESC P tmux ;` + 本体のESCを
     * 2重化 + `ESC \`)でラップする。tmux 3.3以降は`allow-passthrough`が既定offで
     * ラップしただけでは無音で捨てられる(実tmux 3.3aで確認済み)ので、直前に
     * `tmux set -p allow-passthrough on`をpaneスコープで実行する——これはユーザーの
     * tmux設定を書き換える副作用だが、pane限定かつこの機能の動作に必須。3.2以前は
     * このオプション自体が無く(passthroughは常に許可)、`set`が失敗しても無視される。
     *
     * **サイズ上限**: 64KiB(65536バイト)へ切り詰めてから送る。これは
     * `isekai_protocol::MAX_CLIPBOARD_TEXT_DECODED_LEN`(ctl-socket経路のテキスト上限)
     * であり、同時に`rust-core/src/terminal.rs`の`OSC_RAW_BUF_SIZE`(vteのOSC生バッファを
     * 「64KiBのbase64 + `52;c;`」がちょうど収まる約85KiBに拡張したもの)の上限でもある
     * ——**2経路の上限を意図的に一致させてある**ので、どちらを通っても同じ64KiBで頭打ちに
     * なり、経路によって挙動が変わらない。切り詰めはUTF-8境界で壊れないよう
     * `decode("ignore")`で末尾の中途半端なバイトを落とす。
     *
     * vteのOSCバッファを溢れさせると、切り詰められたbase64はdecodeに失敗して
     * **何も起きない**(エラーにすらならない)——送る側で上限内に収めるのが必須なのは
     * このため(`terminal.rs`の`test_clipboard_write_osc_52_beyond_the_buffer_is_dropped`が
     * その挙動をピン留めしている)。OSC 52は複数回送っても連結されない(後勝ちの上書き)
     * のでチャンク分割による回避も不可。実際に何バイト送ったかはstderrへ
     * `copied 65536/200000 bytes via OSC 52`の形で出す。
     *
     * **補助経路**: `isekai-pipe`がPATHにあれば先に`isekai-pipe ctl clip push`
     * (tmux迂回control-plane、ISEKAI_PIPE_DESIGN.md Epic M)を試す。ソケットの解決
     * (`$ISEKAI_CTL_SOCK`またはtmux user-option `@isekai_ctl_sock`)は`isekai-pipe`自身が
     * 行うので、こちらは成否(exit status)だけを見て、失敗したらOSC 52へ黙って落ちる。
     * この機能はアプリ側設定「tmux迂回control-plane」が既定OFFのopt-inなので、あくまで
     * 「使えるなら使う」。両経路とも同じ切り詰め済みバイト列を送る——上限が同じである以上
     * 経路ごとに送る量を変える理由が無く、`ok`(ctl成功)でもstderrの表示が食い違わない。
     *
     * なお、どちらの経路でも実際にクリップボードへ書かれるのはアプリ設定
     * `allow_remote_clipboard_write`(既定false、opt-in)が有効な場合のみ。
     */
    val CLAUDE_COPY_LAST_REPLY = SnippetTemplate(
        label = "直前のClaude応答をクリップボードへコピー",
        command =
            // 先頭の`!`はclaude REPLのbashモード切り替え(REPLに食われ、以降が
            // シェルへ渡る)。ここから下がそのままシェルコマンド1行になる。
            "!if command -v python3 >/dev/null 2>&1; then python3 -c 'import os,sys,json,base64,glob,shutil,subprocess;" +
            "p=os.path.expanduser(\"~/.claude/projects/\")+os.getcwd().replace(\"/\",\"-\");" +
            "e=os.environ.get(\"CLAUDE_CODE_SESSION_ID\") or \"\";h=p+\"/\"+e+\".jsonl\";" +
            "g=[h] if e and os.path.exists(h) else sorted(glob.glob(p+\"/*.jsonl\"),key=os.path.getmtime);" +
            "sys.exit(\"isekai: no claude session log under \"+p) if not g else 0;" +
            "R=[o for o in map(json.loads,[l for l in open(g[-1],encoding=\"utf-8\",errors=\"replace\").read().splitlines() " +
            "if l[:1]==\"{\" and l[-1:]==\"}\" and (\"assistant\" in l or \"user\" in l)]) if not o.get(\"isSidechain\")];" +
            "u=[i for i,o in enumerate(R) for c in [((o.get(\"message\") or {}).get(\"content\") or \"\")] " +
            "if o.get(\"type\")==\"user\" and not o.get(\"toolUseResult\") and not o.get(\"isMeta\") " +
            "and (c[:1]!=\"<\" if isinstance(c,str) else not [k for k in c if isinstance(k,dict) and k.get(\"type\")==\"tool_result\"])];" +
            "S=[\"\".join(k.get(\"text\") or \"\" for k in ((o.get(\"message\") or {}).get(\"content\") or []) " +
            "if isinstance(k,dict) and k.get(\"type\")==\"text\") " +
            "for o in R[(u[-1]+1 if u else 0):] if o.get(\"type\")==\"assistant\"];" +
            "S=[s for s in S if s.strip()];" +
            "sys.exit(\"isekai: no text in the most recent turn (it was tool-calls only)\" if u " +
            "else \"isekai: no assistant text in \"+g[-1]) if not S else 0;" +
            "d=S[-1].encode(\"utf-8\");b=d[:65536].decode(\"utf-8\",\"ignore\").encode(\"utf-8\");" +
            "x=shutil.which(\"isekai-pipe\");" +
            "ok=bool(x) and subprocess.run([x,\"ctl\",\"clip\",\"push\",\"--mime\",\"text/plain\"]," +
            "input=b,capture_output=True).returncode==0;E=chr(27);" +
            "os.system(\"tmux set -p allow-passthrough on 2>/dev/null\") if os.environ.get(\"TMUX\") and not ok else 0;" +
            "q=E+\"]52;c;\"+base64.b64encode(b).decode()+chr(7);" +
            "q=E+\"Ptmux;\"+q.replace(E,E+E)+E+chr(92) if os.environ.get(\"TMUX\") else q;" +
            "sys.stdout.write(\"\" if ok else q);sys.stdout.flush();" +
            "sys.stderr.write(\"isekai: copied \"+str(len(b))+\"/\"+str(len(d))+" +
            "\" bytes via \"+(\"ctl-socket\" if ok else \"OSC 52\")+chr(10))';" +
            " else echo \"isekai: python3 not found (needed to read ~/.claude/projects/*.jsonl)\";" +
            " fi",
    )

    val ALL: List<SnippetTemplate> = listOf(
        TMUX_SESSION_PICKER,
        KILL_HIGH_USAGE_PROCESS,
        CLAUDE_RESUME,
        CLAUDE_CONTINUE,
        CLAUDE_REWIND,
        CLAUDE_CLEAR,
        CLAUDE_COPY_LAST_REPLY,
    )
}
