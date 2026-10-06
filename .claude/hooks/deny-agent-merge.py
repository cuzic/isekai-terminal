#!/usr/bin/env python3
"""PreToolUse(Bash) hook rev2: エージェントからのGitHub書き込み系操作を許可リスト方式で制限する。

ADR_PARALLEL_AGENT_DELIVERY.md B1/NM1/NM2 対策。拒否リストはバイパスが多い(gh -R, git -C, タグpush,
gh release/secret, gh auth token+curl 等)ため、gh/git pushは「許可した形だけ通す」。
対象判定: (a) stdinに agent_id/agent_type があればエージェント、(b) cwd が .claude/worktrees/ 配下。
どちらでもなければ(=リード)対象外。※ (a)のフィールドの有無は導入時に実測で確認する(HOOK_DEBUG=1)。
限界: コマンド文字列の静的検査。変数展開/スクリプト経由/別ツール(curl等)は防げない(多層防御の一層)。
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
    sys.exit(0)  # fail-open
if os.environ.get("HOOK_DEBUG"):
    open("/tmp/deny-agent-merge.debug", "a").write(json.dumps({k: (v if k != "tool_input" else "...") for k, v in data.items()}) + "\n")
if data.get("tool_name") != "Bash":
    sys.exit(0)

is_agent = bool(data.get("agent_id") or data.get("agent_type")) or "/.claude/worktrees/" in (data.get("cwd") or "") + "/"
if not is_agent:
    sys.exit(0)

cmd = (data.get("tool_input") or {}).get("command") or ""

GH_ALLOW = {  # (group, sub) -- 読み取り系 + 自分のPRの作成/更新
    ("pr", "create"), ("pr", "view"), ("pr", "checks"), ("pr", "diff"), ("pr", "list"), ("pr", "status"),
    ("pr", "comment"), ("pr", "edit"),
    ("run", "list"), ("run", "view"), ("run", "watch"), ("run", "download"),
    ("workflow", "list"), ("workflow", "view"),
    ("repo", "view"), ("issue", "view"), ("issue", "list"), ("auth", "status"),
}
SEGMENT_SPLIT = re.compile(r"(?:&&|\|\||;|\||\n|\$\(|`|\()")

def strip_globals(toks, flags_with_arg):
    out, i = [], 0
    while i < len(toks):
        t = toks[i]
        if t in flags_with_arg:
            i += 2; continue
        if any(t.startswith(f + "=") for f in flags_with_arg if f.startswith("--")):
            i += 1; continue
        out.append(t); i += 1
    return out

def check_gh(toks):
    toks = strip_globals(toks, {"-R", "--repo", "--hostname"})
    sub = tuple(toks[1:3]) if len(toks) >= 3 else tuple(toks[1:2])
    if toks[1:2] == ["api"]:
        rest = toks[2:]
        for i, t in enumerate(rest):
            if t in ("-X", "--method") and i + 1 < len(rest) and rest[i + 1].upper() != "GET":
                deny("gh api の書き込み系メソッドは禁止")
            if t.upper().startswith("-X") and len(t) > 2 and t[2:].upper() != "GET":
                deny("gh api の書き込み系メソッドは禁止")
            if t in ("-f", "-F", "--field", "--raw-field", "--input") or t.startswith(("--field=", "--raw-field=", "--input=")):
                deny("gh api の -f/-F/--input は POST になるため禁止")
        return
    if sub not in GH_ALLOW:
        deny(f"許可リストにない gh 操作: gh {' '.join(sub)}")

def check_git(toks):
    toks = strip_globals(toks, {"-C", "-c", "--git-dir", "--work-tree"})
    if toks[1:2] != ["push"]:
        return
    args = [t for t in toks[2:]]
    opts = [a for a in args if a.startswith("-")]
    pos = [a for a in args if not a.startswith("-")]
    for o in opts:
        if o in ("--mirror", "--all", "--tags", "--follow-tags", "--delete", "-d", "--prune") or o == "--force" or (o.startswith("-") and not o.startswith("--") and "f" in o[1:] and o not in ("-u",)):
            deny(f"git push のオプション {o} は禁止(--force-with-lease のみ可)")
    for ref in pos[1:]:  # pos[0] はremote
        dst = ref.split(":")[-1].lstrip("+")
        if dst in ("main", "master") or dst.startswith(("refs/heads/main", "refs/heads/master", "refs/tags/", "tags/")) or re.fullmatch(r"v\d[\w.\-]*", dst):
            deny(f"git push の宛先 {dst} は禁止(main/master/タグ)")
        if ref.startswith("+"):
            deny("git push の +refspec(force)は禁止")
    if len(pos) < 2 and not any(o in ("-u", "--set-upstream") for o in opts) and False:
        pass

for seg in SEGMENT_SPLIT.split(cmd):
    s = seg.strip()
    if not s:
        continue
    # bash -c '...' / sh -c "..." の中身も再帰的に検査
    m = re.match(r"^(?:env\s+\S+=\S+\s+)*(?:bash|sh|zsh)\s+-\w*c\s+(.+)$", s)
    if m:
        try:
            inner = shlex.split(m.group(1))[0]
        except Exception:
            inner = m.group(1)
        for sub_seg in SEGMENT_SPLIT.split(inner):
            s2 = sub_seg.strip()
            if s2:
                try: toks = shlex.split(s2)
                except ValueError: toks = s2.split()
                if toks[:1] == ["gh"]: check_gh(toks)
                if toks[:1] == ["git"]: check_git(toks)
        continue
    try: toks = shlex.split(s)
    except ValueError: toks = s.split()
    toks = [t for t in toks if not re.fullmatch(r"\w+=\S*", t)] if toks and re.fullmatch(r"\w+=\S*", toks[0]) else toks
    if toks[:1] == ["gh"]:
        check_gh(toks)
    elif toks[:1] == ["git"]:
        check_git(toks)
    elif re.search(r"\bgh\s+auth\s+token\b", s):
        deny("gh auth token の取得は禁止")
sys.exit(0)
