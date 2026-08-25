import Foundation

/// 定型コマンド一覧画面の「テンプレートから追加」で選べる、アプリ同梱の雛形。
/// DB行ではなく静的データ。選んだ内容は通常の`Snippet`としてそのまま編集・保存できる
/// (Android版`data/SnippetTemplates.kt`の1:1移植、`ADR_IOS_PARITY_IMPLEMENTATION.md` §3.3)。
/// テンプレート定義自体はUI初期データであり接続/セッションの状態でも意思決定でもないため、
/// Rust共通層には置かない(D-1の「UI表示に閉じた状態」に該当、UniFFI regenサイクルを
/// 増やすだけで対称性の実利が無いという判断)。
public struct SnippetTemplate: Equatable {
    public let label: String
    public let command: String
    public let appendNewline: Bool

    public init(label: String, command: String, appendNewline: Bool = true) {
        self.label = label
        self.command = command
        self.appendNewline = appendNewline
    }
}

public enum SnippetTemplates {
    /// tmux のセッション一覧を出して選んだセッションに attach/switch する。fzf があれば絞り込み
    /// 検索、なければ bash 組み込みの `select` で番号選択にフォールバックする。
    ///
    /// ターミナルへは(スクリプトファイルとしてではなく)対話シェルへの逐次入力として送られる
    /// (`SnippetCommands.toBytes`が送信するため)。そのため `exit` で接続自体を落とすような
    /// 書き方や、シバン行は書かない。複数行のまま送ると、フォアグラウンドが必ずしもシェルとは
    /// 限らない(ページャ実行中等)場合に一部の行が意図しない形で解釈されうるため、`;` で
    /// 繋いだ1行にまとめている(adversarial review指摘、2026-08、Android版と同じ)。
    ///
    /// `select`/`PS3` は bash/ksh/zsh 限定(dash/fishでは動かない)——それ以外の行は
    /// 意図的にPOSIX風に書いているが、fzf不在時のフォールバック経路がこの制約を持つため
    /// 全体としてもこれら3シェルが既定ログインシェルであることが前提。
    public static let tmuxSessionPicker = SnippetTemplate(
        label: "tmuxセッション選択",
        command: "if ! command -v tmux >/dev/null 2>&1; then echo 'tmux: not installed'; "
            + "else "
            + "s=$(tmux list-sessions -F '#{session_name}' 2>/dev/null); "
            + "if [ -z \"$s\" ]; then echo 'tmux: no sessions'; "
            + "else "
            + "go() { if [ -n \"$TMUX\" ]; then tmux switch-client -t \"$1\"; else tmux attach -t \"$1\"; fi; }; "
            + "if command -v fzf >/dev/null 2>&1; then "
            + "p=$(printf '%s\\n' \"$s\" | fzf) && go \"$p\"; "
            + "else "
            + "oldifs=$IFS; IFS=$'\\n'; set -- $s; IFS=$oldifs; "
            + "PS3='session> '; select p in \"$@\"; do [ -n \"$p\" ] && go \"$p\"; break; done; "
            + "fi; fi; fi"
    )

    /// CPU/メモリ使用率が高い順にプロセスを並べて選択し、確認の上で `kill`(SIGTERM)する。
    /// `tmuxSessionPicker` と同じ fzf/`select` フォールバック構成・1行化の理由付けを踏襲。
    ///
    /// `kill`は破壊的操作のため、選んだ後も対象PIDを表示した上で `[y/N]` の確認を挟み、
    /// 既定(Enterのみ等)ではキャンセル扱いにする。シグナルは常にSIGTERM(既定)——
    /// プロセスに後始末の機会を与えるため。応答しないプロセスへの `-9` 等が必要な場合は
    /// 手動で打ち直すことを想定し、このテンプレートではシグナル選択までは提供しない。
    public static let killHighUsageProcess = SnippetTemplate(
        label: "CPU/メモリ使用率上位プロセスをkill",
        command: "list=$(ps -eo pid,%cpu,%mem,comm --sort=-%cpu,-%mem --no-headers | head -20); "
            + "if [ -z \"$list\" ]; then echo 'ps: no processes'; "
            + "else "
            + "if command -v fzf >/dev/null 2>&1; then "
            + "sel=$(printf '%s\\n' \"$list\" | fzf --prompt='kill> '); "
            + "else "
            + "oldifs=$IFS; IFS=$'\\n'; set -- $list; IFS=$oldifs; "
            + "PS3='process> '; select sel in \"$@\"; do [ -n \"$sel\" ] && break; done; "
            + "fi; "
            + "if [ -n \"$sel\" ]; then "
            + "set -- $sel; pid=$1; "
            + "printf 'kill PID %s? [y/N] ' \"$pid\"; read ans; "
            + "case \"$ans\" in [Yy]*) kill \"$pid\" && echo \"sent SIGTERM to $pid\";; *) echo cancelled;; esac; "
            + "fi; fi"
    )

    /// Claude Code運用で頻出するコマンド4種。`claude --resume`/`claude --continue`は
    /// シェルプロンプトで打つコマンド(前者は過去の会話一覧から選んで再開、後者は
    /// カレントディレクトリの直近の会話をそのまま継続)。`/rewind`/`/clear`は逆に、
    /// 既にclaudeの対話セッション内にいる状態で打つスラッシュコマンド(前者は直前の
    /// チェックポイントへ巻き戻す、後者は現在の会話コンテキストを破棄する)——
    /// どちらもシェルコマンドではないため、フォアグラウンドがclaude REPLでない
    /// タイミングで送っても実行はされず、単に画面に文字列として現れるだけ(壊れはしない)。
    public static let claudeResume = SnippetTemplate(label: "claude --resume(会話一覧から再開)", command: "claude --resume")
    public static let claudeContinue = SnippetTemplate(label: "claude --continue(直前の会話を継続)", command: "claude --continue")
    public static let claudeRewind = SnippetTemplate(label: "/rewind(直前のチェックポイントへ巻き戻す)", command: "/rewind")
    public static let claudeClear = SnippetTemplate(label: "/clear(会話コンテキストを破棄)", command: "/clear")

    /// 直前のClaude Codeの応答を端末(iOS)のクリップボードへコピーする。Android版と同一の
    /// python one-linerを送る(挙動の詳細・転送経路・サイズ上限等の理由付けは
    /// Android版`data/SnippetTemplates.kt`の`CLAUDE_COPY_LAST_REPLY`ドキュメントを参照
    /// ——同じPythonコード自体がロジックの実体であり、プラットフォーム差分は無い)。
    public static let claudeCopyLastReply = SnippetTemplate(
        label: "直前のClaude応答をクリップボードへコピー",
        command:
            "!if command -v python3 >/dev/null 2>&1; then python3 -c 'import os,sys,json,base64,glob,shutil,subprocess;"
            + "p=os.path.expanduser(\"~/.claude/projects/\")+os.getcwd().replace(\"/\",\"-\");"
            + "e=os.environ.get(\"CLAUDE_CODE_SESSION_ID\") or \"\";h=p+\"/\"+e+\".jsonl\";"
            + "g=[h] if e and os.path.exists(h) else sorted(glob.glob(p+\"/*.jsonl\"),key=os.path.getmtime);"
            + "sys.exit(\"isekai: no claude session log under \"+p) if not g else 0;"
            + "R=[o for o in map(json.loads,[l for l in open(g[-1],encoding=\"utf-8\",errors=\"replace\").read().splitlines() "
            + "if l[:1]==\"{\" and l[-1:]==\"}\" and (\"assistant\" in l or \"user\" in l)]) if not o.get(\"isSidechain\")];"
            + "u=[i for i,o in enumerate(R) for c in [((o.get(\"message\") or {}).get(\"content\") or \"\")] "
            + "if o.get(\"type\")==\"user\" and not o.get(\"toolUseResult\") and not o.get(\"isMeta\") "
            + "and (c[:1]!=\"<\" if isinstance(c,str) else not [k for k in c if isinstance(k,dict) and k.get(\"type\")==\"tool_result\"])];"
            + "S=[\"\".join(k.get(\"text\") or \"\" for k in ((o.get(\"message\") or {}).get(\"content\") or []) "
            + "if isinstance(k,dict) and k.get(\"type\")==\"text\") "
            + "for o in R[(u[-1]+1 if u else 0):] if o.get(\"type\")==\"assistant\"];"
            + "S=[s for s in S if s.strip()];"
            + "sys.exit(\"isekai: no text in the most recent turn (it was tool-calls only)\" if u "
            + "else \"isekai: no assistant text in \"+g[-1]) if not S else 0;"
            + "d=S[-1].encode(\"utf-8\");b=d[:65536].decode(\"utf-8\",\"ignore\").encode(\"utf-8\");"
            + "x=shutil.which(\"isekai-pipe\");"
            + "ok=bool(x) and subprocess.run([x,\"ctl\",\"clip\",\"push\",\"--mime\",\"text/plain\"],"
            + "input=b,capture_output=True).returncode==0;E=chr(27);"
            + "os.system(\"tmux set -p allow-passthrough on 2>/dev/null\") if os.environ.get(\"TMUX\") and not ok else 0;"
            + "q=E+\"]52;c;\"+base64.b64encode(b).decode()+chr(7);"
            + "q=E+\"Ptmux;\"+q.replace(E,E+E)+E+chr(92) if os.environ.get(\"TMUX\") else q;"
            + "sys.stdout.write(\"\" if ok else q);sys.stdout.flush();"
            + "sys.stderr.write(\"isekai: copied \"+str(len(b))+\"/\"+str(len(d))+"
            + "\" bytes via \"+(\"ctl-socket\" if ok else \"OSC 52\")+chr(10))';"
            + " else echo \"isekai: python3 not found (needed to read ~/.claude/projects/*.jsonl)\";"
            + " fi"
    )

    public static let all: [SnippetTemplate] = [
        tmuxSessionPicker,
        killHighUsageProcess,
        claudeResume,
        claudeContinue,
        claudeRewind,
        claudeClear,
        claudeCopyLastReply,
    ]
}
