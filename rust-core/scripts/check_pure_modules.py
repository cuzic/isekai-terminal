#!/usr/bin/env python3
"""純粋モジュールのimport allowlist検査(docs/adr/0019-functional-core-effects.md §2.3 層2)。

rust-core/pure_modules.toml に登録された各モジュールが、
  * 未登録(=不純かもしれない)crate内モジュールの項目を `use`/インラインパスで参照していない
  * 許可リスト外の外部crate/stdパスを参照していない
  * 未登録モジュールからのglob importをしていない
  * 内部可変な `static` / `thread_local!` を持っていない
ことを確認する。`#[cfg(test)]` 付きアイテムとコメント・文字列リテラルは除外して検査する。

手書きの簡易字句解析なので、以下は検出できない(既知の死角):
  - `self::` / `super::` の相対パスの解決(無視する)
  - 許可済みモジュールを経由した再エクスポート(`pub use`)
  - マクロが生成するパス、`#[path]`属性
  - trait経由の動的呼び出し、許可リスト側モジュールに後から入った不純コード
[[interpreter]]登録関数(Step 7a)は、本体で`<effect>::`を`if let`/`let .. else`/`matches!`で
選り分けること、および`#[deny(clippy::wildcard_enum_match_arm)]`の欠落を検査する(`match`内の`_`はclippy側が検出)。
入れ子のグループimport(`use crate::{a::{b, c}, D}`)は括弧対応で展開して全項目を個別に判定する。

使い方: python3 scripts/check_pure_modules.py
        python3 scripts/check_pure_modules.py --self-test
"""
import os
import re
import sys
import tomllib

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))

PRIMITIVES = {
    "u8", "u16", "u32", "u64", "u128", "usize", "i8", "i16", "i32", "i64", "i128",
    "isize", "f32", "f64", "bool", "char", "str", "self", "super", "crate",
}


def strip_comments_and_strings(src: str) -> str:
    """コメントと文字列/文字リテラルの中身を空白に置換する(改行は保持)。"""
    out = []
    i, n = 0, len(src)
    while i < n:
        c = src[i]
        two = src[i:i + 2]
        if two == "//":
            while i < n and src[i] != "\n":
                i += 1
        elif two == "/*":
            depth = 1
            i += 2
            while i < n and depth:
                if src[i:i + 2] == "/*":
                    depth += 1
                    i += 2
                elif src[i:i + 2] == "*/":
                    depth -= 1
                    i += 2
                else:
                    if src[i] == "\n":
                        out.append("\n")
                    i += 1
            out.append(" ")
        elif c == "r" and re.match(r'r#*"', src[i:i + 12]) and (i == 0 or not (src[i - 1].isalnum() or src[i - 1] == "_")):
            m = re.match(r'r(#*)"', src[i:])
            end = '"' + m.group(1)
            j = src.find(end, i + len(m.group(0)))
            j = n if j < 0 else j + len(end)
            out.append('""' + "\n" * src[i:j].count("\n"))
            i = j
        elif c == '"':
            j = i + 1
            while j < n and src[j] != '"':
                j += 2 if src[j] == "\\" else 1
            j = min(j + 1, n)
            out.append('""' + "\n" * src[i:j].count("\n"))
            i = j
        elif c == "'":
            m = re.match(r"'(\\.[^']*|[^\\'])'", src[i:i + 12])
            if m:
                out.append("' '")
                i += len(m.group(0))
            else:  # lifetime
                out.append(c)
                i += 1
        else:
            out.append(c)
            i += 1
    return "".join(out)


def find_matching(s: str, start: int, open_c: str, close_c: str) -> int:
    depth = 0
    for k in range(start, len(s)):
        if s[k] == open_c:
            depth += 1
        elif s[k] == close_c:
            depth -= 1
            if depth == 0:
                return k
    return len(s) - 1


def remove_cfg_test_items(s: str) -> str:
    pat = re.compile(r"#\s*\[\s*cfg\s*\(\s*test\s*\)\s*\]")
    while True:
        m = pat.search(s)
        if not m:
            return s
        j = m.end()
        while True:  # 後続の属性を読み飛ばす
            m2 = re.match(r"\s*#\s*\[", s[j:])
            if not m2:
                break
            j = find_matching(s, j + m2.end() - 1, "[", "]") + 1
        k = j
        depth_p = 0
        while k < len(s):  # アイテム本体: 最初の `{`(括弧対応) か `;` まで
            ch = s[k]
            if ch in "([":
                depth_p += 1
            elif ch in ")]":
                depth_p -= 1
            elif ch == ";" and depth_p == 0:
                k += 1
                break
            elif ch == "{" and depth_p == 0:
                k = find_matching(s, k, "{", "}") + 1
                break
            k += 1
        s = s[:m.start()] + " " + s[k:]


def split_top(s: str):
    parts, depth, cur = [], 0, ""
    for ch in s:
        if ch == "{":
            depth += 1
        elif ch == "}":
            depth -= 1
        if ch == "," and depth == 0:
            parts.append(cur)
            cur = ""
        else:
            cur += ch
    if cur.strip():
        parts.append(cur)
    return parts


def expand_use(tree: str, prefix=()):
    """`a::{b, c::{d, self}, *}` を (path_tuple, is_glob) の列へ展開する。"""
    tree = re.sub(r"\s+as\s+\w+\s*$", "", tree.strip())
    tree = re.sub(r"\s+", "", tree)
    if not tree:
        return []
    m = re.match(r"^(.*?)::\{(.*)\}$", tree)
    if m:
        head = tuple(x for x in m.group(1).split("::") if x)
        res = []
        for part in split_top(m.group(2)):
            res += expand_use(part, prefix + head)
        return res
    if tree.startswith("{") and tree.endswith("}"):
        res = []
        for part in split_top(tree[1:-1]):
            res += expand_use(part, prefix)
        return res
    segs = tuple(x for x in tree.split("::") if x)
    if segs and segs[-1] == "*":
        return [(prefix + segs[:-1], True)]
    if segs and segs[-1] == "self":
        return [(prefix + segs[:-1], False)]
    return [(prefix + segs, False)]


def seg_prefix(path, prefix):
    return len(path) >= len(prefix) and tuple(path[:len(prefix)]) == tuple(prefix)


class Config:
    def __init__(self, data):
        self.modules = data.get("module", [])
        ext = data.get("external", {})
        self.ext_allow = [tuple(p.split("::")) for p in ext.get("allow", [])]
        self.ext_deny = [tuple(p.split("::")) for p in ext.get("deny", [])]
        self.interpreters = data.get("interpreter", [])
        self.items = data.get("allow_item", [])
        for it in self.items:
            if not it.get("reason"):
                raise SystemExit(f"allow_item {it.get('path')} に reason が無い")
            if it["path"] in ("crate", "crate::*"):
                raise SystemExit("crate(root)のモジュール単位許可は禁止")

    def check_crate_path(self, package, path, glob):
        """path: ('crate', ...)。許可されていればNone、そうでなければ理由文字列。"""
        segs = list(path[1:])
        for m in self.modules:
            if m["package"] == package and seg_prefix(segs, tuple(m["module"].split("::"))):
                return None
        for it in self.items:
            if it["package"] == package and seg_prefix(path, tuple(it["path"].split("::"))):
                return None
        if glob:
            return "未登録モジュールからのglob importは禁止(項目単位の判定ができない)"
        return "未登録(不純の可能性がある)モジュール/項目を参照している。pure_modules.tomlへの登録か[[allow_item]]が必要"

    def check_external_path(self, path):
        for d in self.ext_deny:
            if seg_prefix(path, d):
                return "明示的に拒否された外部パス"
        for a in self.ext_allow:
            if seg_prefix(path, a):
                return None
        return "外部パスが許可リスト(pure_modules.toml [external].allow)に無い"


USE_RE = re.compile(r"(?<![\w'])use\s+([^;]+);")


def check_file(cfg: Config, mod) -> list:
    pkg = mod["package"]
    with open(os.path.join(ROOT, mod["file"]), encoding="utf-8") as f:
        src = f.read()
    s = remove_cfg_test_items(strip_comments_and_strings(src))
    # 属性(`#[derive(..)]`/`#![deny(clippy::..)]`/`#[uniffi::export]`等)の中身は検査対象外
    # (clippy/uniffi等はコードの実行時依存ではない)。
    s = re.sub(r"#!?\s*\[", lambda m: m.group(0), s)
    out, i = [], 0
    for m in re.finditer(r"#!?\s*\[", s):
        if m.start() < i:
            continue
        end = find_matching(s, m.end() - 1, "[", "]") + 1
        out.append(s[i:m.start()])
        out.append("\n" * s[m.start():end].count("\n"))
        i = end
    out.append(s[i:])
    s = "".join(out)
    errs = []

    def lineno(pos):
        return s.count("\n", 0, pos) + 1

    imported_leaves = set()
    covered_spans = []
    for m in USE_RE.finditer(s):
        covered_spans.append((m.start(), m.end()))
        alias = re.search(r"\bas\s+(\w+)\s*$", m.group(1))
        if alias:
            imported_leaves.add(alias.group(1))
        for p, glob in expand_use(m.group(1)):
            if not p:
                continue
            if not glob:
                imported_leaves.add(p[-1])
            first = p[0]
            if first in ("self", "super"):
                continue  # 既知の死角
            if first == "crate":
                why = cfg.check_crate_path(pkg, p, glob)
            else:
                why = cfg.check_external_path(p)
                if why and glob and cfg.check_external_path(p + ("x",)) is None:
                    why = None  # 許可済みprefixのglob
            if why:
                errs.append(f"{mod['file']}:{lineno(m.start())}: `use {'::'.join(p)}{'::*' if glob else ''}`: {why}")

    def in_use(pos):
        return any(a <= pos < b for a, b in covered_spans)

    # インラインパス(`crate::..` と、小文字始まりの外部crate/stdパス)
    local_names = set(imported_leaves) | PRIMITIVES
    local_names |= set(re.findall(r"\b(?:mod|fn|struct|enum|trait|type|const|static|let(?:\s+mut)?)\s+(\w+)", s))
    for m in re.finditer(r"(?<![\w:.'])([a-z_][A-Za-z0-9_]*(?:\s*::\s*[A-Za-z_][A-Za-z0-9_]*)+)", s):
        if in_use(m.start()):
            continue
        segs = tuple(re.sub(r"\s+", "", m.group(1)).split("::"))
        first = segs[0]
        if first in ("self", "super"):
            continue
        if first == "crate":
            why = cfg.check_crate_path(pkg, segs, False)
        elif first in local_names:
            continue
        else:
            why = cfg.check_external_path(segs)
        if why:
            errs.append(f"{mod['file']}:{lineno(m.start())}: インラインパス `{'::'.join(segs)}`: {why}")

    # 内部可変なstatic / thread_local
    for m in re.finditer(r"\bthread_local\s*!", s):
        errs.append(f"{mod['file']}:{lineno(m.start())}: thread_local! は純粋モジュールで禁止")
    for m in re.finditer(r"(?<!')\bstatic\s+(mut\s+)?[A-Za-z_][A-Za-z0-9_]*\s*:([^=;]*)", s):
        if m.group(1) or re.search(r"Mutex|RwLock|Atomic|Lazy|Once|Cell", m.group(2)):
            errs.append(f"{mod['file']}:{lineno(m.start())}: 内部可変/可変なstaticは純粋モジュールで禁止")
    return errs


def check_interpreter(entry) -> list:
    """Effect interpreter関数(ADR §3-8)が、Effectを明示matcharm以外で選り分けていないか検査する。"""
    path = os.path.join(ROOT, entry["file"])
    if not os.path.exists(path):
        return [f"{entry['file']}: 登録されたinterpreterファイルが存在しない"]
    with open(path, encoding="utf-8") as f:
        src = f.read()
    s = remove_cfg_test_items(strip_comments_and_strings(src))
    name, eff = entry["fn"], re.escape(entry["effect"])
    m = re.search(r"((?:#\s*\[[^\]]*\]\s*)*)(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?fn\s+" + re.escape(name) + r"\b", s)
    if not m:
        return [f"{entry['file']}: interpreter関数 `{name}` が見つからない"]
    errs = []
    if "wildcard_enum_match_arm" not in m.group(1) or "deny" not in m.group(1):
        errs.append(f"{entry['file']}: `{name}` に #[deny(clippy::wildcard_enum_match_arm)] が無い")
    brace = s.find("{", m.end())
    body = s[brace:find_matching(s, brace, "{", "}") + 1]
    for pat, what in (
        (r"\bif\s+let\s+" + eff + r"\s*::", "if let"),
        (r"\blet\s+[^;=]*?\b" + eff + r"\s*::[^;]*?\belse\b", "let .. else"),
        (r"\bwhile\s+let\s+" + eff + r"\s*::", "while let"),
        (r"\bmatches\s*!\s*\([^;]*?" + eff + r"\s*::", "matches!"),
    ):
        for bm in re.finditer(pat, body):
            line = s.count("\n", 0, brace + bm.start()) + 1
            errs.append(f"{entry['file']}:{line}: interpreter `{name}` が {entry['effect']} を `{what}` で選り分けている(明示matcharmのみ許可、ADR §3-8)")
    return errs


def run(config_path):
    with open(config_path, "rb") as f:
        cfg = Config(tomllib.load(f))
    if not cfg.modules:
        raise SystemExit("pure_modules.toml にモジュールが1つも登録されていない")
    errs = []
    for mod in cfg.modules:
        if not os.path.exists(os.path.join(ROOT, mod["file"])):
            errs.append(f"{mod['file']}: 登録されたファイルが存在しない")
            continue
        errs += check_file(cfg, mod)
    for entry in cfg.interpreters:
        errs += check_interpreter(entry)
    return errs


def self_test():
    import tempfile
    global ROOT
    cfg = Config({
        "module": [{"package": "p", "file": "x", "module": "good"}],
        "external": {"allow": ["std::time::Duration", "timed_fsm"], "deny": ["timed_fsm::clock"]},
        "allow_item": [{"package": "p", "path": "crate::Val", "reason": "r"}],
    })

    def check(code):
        global ROOT
        with tempfile.TemporaryDirectory() as d:
            old, ROOT = ROOT, d
            try:
                with open(os.path.join(d, "x"), "w") as f:
                    f.write(code)
                return check_file(cfg, {"package": "p", "file": "x", "module": "m"})
            finally:
                ROOT = old

    assert not check("use crate::good::A;\nuse crate::Val;\nuse std::time::Duration;\nuse timed_fsm::{Response};\n")
    assert check("use crate::bad::A;")
    assert check("use crate::RUNTIME;")
    assert check("use crate::bad::*;")
    assert not check("use crate::good::*;")
    assert check("use timed_fsm::clock::MonotonicClock;")
    assert check("use tokio::sync::Notify;")
    assert check("fn f(){ let _ = crate::bad::g(); }")
    assert check("fn f(){ let _ = std::time::Instant::now(); }")
    assert not check("fn f(){ let _ = u64::MAX; let _ = std::time::Duration::ZERO; }")
    assert not check("#[cfg(test)]\nmod t { use crate::bad::X; fn f(){ tokio::spawn(); } }\nfn ok(){}")
    assert not check('// use crate::bad::X;\nfn f(){ let _ = "crate::bad::X"; }')
    assert check("use crate::{good::A, bad::B};")
    assert check("static G: std::sync::Mutex<u8> = todo!();")
    assert check("thread_local!{ static X: u8 = 1; }")
    assert not check("static T: &'static str = \"a\"; fn f<'a>(x: &'a str) {}")
    def check_interp(code):
        global ROOT
        with tempfile.TemporaryDirectory() as d:
            old, ROOT = ROOT, d
            try:
                with open(os.path.join(d, "y"), "w") as f:
                    f.write(code)
                return check_interpreter({"file": "y", "fn": "run", "effect": "E"})
            finally:
                ROOT = old

    deny = "#[deny(clippy::wildcard_enum_match_arm)]\n"
    assert not check_interp(deny + "async fn run(x: Vec<E>) { for e in x { match e { E::A => {}, E::B { .. } => {} } } }")
    assert check_interp("fn run(x: Vec<E>) { for e in x { match e { E::A => {} } } }")
    assert check_interp(deny + "fn run(x: Vec<E>) { for e in x { if let E::A = e {} } }")
    assert check_interp(deny + "fn run(x: Vec<E>) { for e in x { let E::A = e else { continue }; } }")
    assert check_interp(deny + "fn run(x: Vec<E>) { let _ = matches!(x[0], E::A); }")
    assert check_interp(deny + "fn other() {}")
    assert not check_interp(deny + "fn run() { // if let E::A = e\n }")
    print("self-test ok")


if __name__ == "__main__":
    if "--self-test" in sys.argv:
        self_test()
        sys.exit(0)
    problems = run(os.path.join(ROOT, "pure_modules.toml"))
    if problems:
        print("純粋性検査に失敗しました(docs/adr/0019-functional-core-effects.md §2.3):", file=sys.stderr)
        for p in problems:
            print(f"::error::{p}")
        sys.exit(1)
    print("pure module import allowlist: ok")
