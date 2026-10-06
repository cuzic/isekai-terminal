#!/usr/bin/env python3
"""PreToolUse(Bash) hook rev3: エージェントからのGitHub書き込み系操作を許可リスト方式で制限する。

ADR_PARALLEL_AGENT_DELIVERY.md B1/NM1/NM2 対策。gh/git pushは「許可した形だけ通す」。
rev3: shlexで字句解析(引用符内/ヒアドキュメント本文の文字列は命令として扱わない)、env/timeout/絶対パス/
command等の前置ラッパーを剥がす、`--method=PUT`形式、リリースタグ(isekai-*-v*)のpushを拒否。
対象判定: stdinの agent_id/agent_type、または cwd が .claude/worktrees/ 配下。それ以外(リード)は対象外。
限界: 静的検査。変数展開・スクリプト経由・別ツール(curl等)・eval は防げない(多層防御の一層)。fail-open。
"""
import json, os, re, shlex, sys

STOP = "。停止してリードに報告すること(迂回しない)。"
def deny(reason):
    print(json.dumps({"hookSpecificOutput": {"hookEventName": "PreToolUse",
        "permissionDecision": "deny", "permissionDecisionReason": reason + STOP}}))
    sys.exit(0)

try:
    data = json.load(sys.stdin)
except Exception:
    sys.exit(0)
if os.environ.get("HOOK_DEBUG"):
    open("/tmp/deny-agent-merge.debug", "a").write(json.dumps({k: (v if k != "tool_input" else "...") for k, v in data.items()}) + "\n")
if data.get("tool_name") != "Bash":
    sys.exit(0)
is_agent = bool(data.get("agent_id") or data.get("agent_type")) or "/.claude/worktrees/" in (data.get("cwd") or "") + "/"
if not is_agent:
    sys.exit(0)
cmd = (data.get("tool_input") or {}).get("command") or ""

GH_ALLOW = {
    ("pr", "create"), ("pr", "view"), ("pr", "checks"), ("pr", "diff"), ("pr", "list"), ("pr", "status"),
    ("pr", "comment"), ("pr", "edit"),
    ("run", "list"), ("run", "view"), ("run", "watch"), ("run", "download"),
    ("workflow", "list"), ("workflow", "view"),
    ("repo", "view"), ("issue", "view"), ("issue", "list"), ("auth", "status"),
}
WRAPPERS = {"env", "command", "builtin", "exec", "nohup", "time", "nice", "sudo", "xargs", "stdbuf", "timeout", "doas"}
WRAPPER_ARG1 = {"timeout", "nice", "stdbuf"}  # 直後に1引数(秒数など)を取るもの
RELEASE_TAG = re.compile(r"^(?:refs/tags/)?(?:isekai-(?:ssh|pipe)-v.*|v\d.*)$")

def strip_heredocs(s):
    return re.sub(r"<<-?\s*(['\"]?)(\w+)\1[^\n]*\n.*?\n\s*\2\b", " ", s, flags=re.S)

def tokenize(s):
    lex = shlex.shlex(s, posix=True, punctuation_chars=";&|()\n")
    lex.whitespace = " \t\r"
    lex.whitespace_split = True
    return list(lex)

def split_commands(tokens):
    cur, out = [], []
    for t in tokens:
        if t and set(t) <= set(";&|()\n"):
            if cur: out.append(cur)
            cur = []
        else:
            cur.append(t)
    if cur: out.append(cur)
    return out

def unwrap(toks):
    """env VAR=1 timeout 5 /usr/bin/gh ... -> ['gh', ...]; bash -c '...' は別途再帰。"""
    i = 0
    while i < len(toks):
        t = toks[i]
        if re.fullmatch(r"\w+=.*", t):
            i += 1; continue
        base = os.path.basename(t)
        if base in WRAPPERS:
            i += 1
            while i < len(toks) and toks[i].startswith("-"):
                i += 1
            if base in WRAPPER_ARG1 and i < len(toks):
                i += 1
            continue
        break
    rest = toks[i:]
    if rest:
        rest = [os.path.basename(rest[0])] + rest[1:]
    return rest

def strip_flags(toks, with_arg):
    out, i = [], 0
    while i < len(toks):
        t = toks[i]
        if t in with_arg:
            i += 2; continue
        if any(t.startswith(f + "=") for f in with_arg if f.startswith("--")):
            i += 1; continue
        out.append(t); i += 1
    return out

def check_gh(toks):
    toks = strip_flags(toks, {"-R", "--repo", "--hostname"})
    if toks[1:2] == ["api"]:
        rest = toks[2:]
        for i, t in enumerate(rest):
            if t in ("-X", "--method") and i + 1 < len(rest) and rest[i + 1].upper() != "GET":
                deny("gh api の書き込み系メソッドは禁止")
            if t.startswith("--method=") and t.split("=", 1)[1].upper() != "GET":
                deny("gh api の書き込み系メソッドは禁止")
            if re.fullmatch(r"-X\w+", t) and t[2:].upper() != "GET":
                deny("gh api の書き込み系メソッドは禁止")
            if t in ("-f", "-F", "--field", "--raw-field", "--input") or t.startswith(("--field=", "--raw-field=", "--input=")):
                deny("gh api の -f/-F/--input は POST になるため禁止")
        return
    sub = tuple(toks[1:3])
    if sub == ("run", "rerun"):
        # 自分のPRの失敗したジョブの再実行だけ許可(--failed必須)。全体再実行/キャンセル等は不可。
        if "--failed" in toks[3:] and not any(t in toks[3:] for t in ("--debug", "--job")) :
            return
        deny("gh run rerun は --failed のみ許可(全体再実行や他の指定は禁止)")
    if sub not in GH_ALLOW:
        deny(f"許可リストにない gh 操作: gh {' '.join(sub)}")

def check_git(toks):
    toks = strip_flags(toks, {"-C", "-c", "--git-dir", "--work-tree"})
    if toks[1:2] != ["push"]:
        return
    args = toks[2:]
    opts = [a for a in args if a.startswith("-")]
    pos = [a for a in args if not a.startswith("-")]
    for o in opts:
        if o in ("--mirror", "--all", "--tags", "--follow-tags", "--delete", "-d", "--prune", "--force") \
           or (re.fullmatch(r"-[a-zA-Z]+", o) and "f" in o[1:]):
            deny(f"git push のオプション {o} は禁止(--force-with-lease のみ可)")
    for ref in pos[1:]:
        if ref.startswith("+"):
            deny("git push の +refspec(force)は禁止")
        dst = ref.split(":")[-1]
        if re.fullmatch(r"(?:refs/heads/)?(?:main|master)", dst) or dst.startswith(("refs/tags/", "tags/")) or RELEASE_TAG.match(dst):
            deny(f"git push の宛先 {dst} は禁止(main/master/タグ)")

def check(toks, depth=0):
    toks = unwrap(toks)
    if not toks:
        return
    head = toks[0]
    if head in ("bash", "sh", "zsh", "dash") and depth < 3:
        for i, t in enumerate(toks):
            if re.fullmatch(r"-\w*c", t) and i + 1 < len(toks):
                for sub in split_commands(tokenize(strip_heredocs(toks[i + 1]))):
                    check(sub, depth + 1)
                return
    elif head == "gh":
        check_gh(toks)
    elif head == "git":
        check_git(toks)

try:
    for c in split_commands(tokenize(strip_heredocs(cmd))):
        check(c)
except ValueError:
    pass  # 字句解析できない(未閉じ引用符等)は素通り(fail-open)
sys.exit(0)
