#!/usr/bin/env python3
"""isekai-ssh の dead_code 警告の Linux ∩ Windows 積集合レポート(report-only)。

ADR_UNWIRED_CALLBACK_DETECTION.md §3(b) / U4: isekai-ssh は `native/` が cfg(windows) でしか
使われないため、単一プラットフォームでは dead_code を deny できない。Linux と Windows の
両ビルドで未使用と報告された項目(=本当に死んでいる候補)だけを報告する。失敗させない。

サブコマンド:
  extract --out FILE        stdin の `cargo ... --message-format=json` から isekai-ssh の
                            dead_code 警告(code == "dead_code")を抽出して JSON で保存する。
  report LINUX WINDOWS      両ファイルの積集合を Markdown で stdout に出す(常に exit 0)。
                            片方が無い/壊れている場合はその旨を出して続行する。
  --self-test               内蔵テスト。

項目の同一性キーは (正規化したファイルパス, 警告メッセージ)。行番号はキーに含めず参考表示のみ。
"""
import json
import os
import sys

CRATE_PREFIX = "isekai-ssh/"


def norm_path(p: str) -> str:
    p = p.replace("\\", "/")
    while p.startswith("./"):
        p = p[2:]
    # 絶対パスで来た場合は最後の "isekai-ssh/" 以降に揃える。
    idx = p.rfind("/" + CRATE_PREFIX)
    if idx >= 0:
        p = p[idx + 1:]
    return p


def extract(lines):
    """cargo の JSON 行から isekai-ssh の dead_code 警告を [{file,line,msg}] で返す。"""
    seen = {}
    for raw in lines:
        raw = raw.strip()
        if not raw.startswith("{"):
            continue
        try:
            obj = json.loads(raw)
        except ValueError:
            continue
        if obj.get("reason") != "compiler-message":
            continue
        msg = obj.get("message") or {}
        code = (msg.get("code") or {}).get("code")
        if code != "dead_code":
            continue
        spans = [s for s in msg.get("spans", []) if s.get("is_primary")]
        if not spans:
            continue
        f = norm_path(spans[0].get("file_name", ""))
        if not f.startswith(CRATE_PREFIX):
            continue
        item = {"file": f, "line": spans[0].get("line_start", 0), "msg": msg.get("message", "")}
        # lib/bin の双方に同じ項目が出たり、同一項目が重複報告されうるので畳む。
        seen.setdefault((item["file"], item["msg"], item["line"]), item)
    return sorted(seen.values(), key=lambda i: (i["file"], i["line"], i["msg"]))


def intersect(linux, windows):
    wkeys = {(i["file"], i["msg"]) for i in windows}
    return [i for i in linux if (i["file"], i["msg"]) in wkeys]


def render(linux, windows):
    out = ["## isekai-ssh dead_code: Linux ∩ Windows (report-only)", ""]
    if linux is None or windows is None:
        missing = [n for n, v in (("Linux", linux), ("Windows", windows)) if v is None]
        out.append(f"{'/'.join(missing)} 側の警告一覧が取得できなかったため積集合は算出できない(失敗させない)。")
        for n, v in (("Linux", linux), ("Windows", windows)):
            if v is not None:
                out.append(f"- {n}: {len(v)} 件")
        return "\n".join(out) + "\n"
    both = intersect(linux, windows)
    out.append(f"- Linux: {len(linux)} 件 / Windows: {len(windows)} 件 / **両方で未使用: {len(both)} 件**")
    out.append("")
    if not both:
        out.append("積集合は 0 件。`isekai-ssh` の dead_code deny を検討できる(ADR §10 U4)。")
    else:
        out.append("| file:line (Linux) | 警告 |")
        out.append("|---|---|")
        for i in both:
            out.append(f"| `{i['file']}:{i['line']}` | {i['msg']} |")
    return "\n".join(out) + "\n"


def load(path):
    try:
        with open(path, encoding="utf-8") as fh:
            data = json.load(fh)
        return data if isinstance(data, list) else None
    except (OSError, ValueError):
        return None


def self_test():
    def w(file, line, msg, code="dead_code", primary=True):
        return json.dumps({"reason": "compiler-message", "message": {
            "code": {"code": code}, "message": msg,
            "spans": [{"file_name": file, "line_start": line, "is_primary": primary}]}})

    lin = extract([
        w("isekai-ssh/src/a.rs", 1, "function `x` is never used"),
        w("isekai-ssh/src/a.rs", 1, "function `x` is never used"),  # 重複
        w("isekai-ssh/src/native/b.rs", 5, "function `y` is never used"),
        w("isekai-ssh/src/c.rs", 9, "unused import", code="unused_imports"),
        w("isekai-protocol/src/d.rs", 2, "function `z` is never used"),  # 他crate
        "not json", '{"reason":"build-finished"}',
    ])
    assert [i["msg"] for i in lin] == ["function `x` is never used", "function `y` is never used"], lin
    win = extract([w("isekai-ssh\\src\\a.rs", 1, "function `x` is never used"),
                   w("isekai-ssh/src/e.rs", 3, "constant `K` is never used")])
    assert win[0]["file"] == "isekai-ssh/src/a.rs", win
    both = intersect(lin, win)
    assert len(both) == 1 and both[0]["msg"].startswith("function `x`"), both
    assert norm_path("/home/r/rust-core/isekai-ssh/src/a.rs") == "isekai-ssh/src/a.rs"
    assert "両方で未使用: 1 件" in render(lin, win)
    assert "取得できなかった" in render(lin, None)
    assert "0 件" in render([], [])
    print("self-test OK")


def main(argv):
    if argv[:1] == ["--self-test"]:
        self_test()
        return 0
    if argv[:1] == ["extract"]:
        out = argv[argv.index("--out") + 1]
        items = extract(sys.stdin)
        os.makedirs(os.path.dirname(os.path.abspath(out)), exist_ok=True)
        with open(out, "w", encoding="utf-8") as fh:
            json.dump(items, fh, ensure_ascii=False, indent=1)
        print(f"extracted {len(items)} isekai-ssh dead_code warnings -> {out}")
        return 0
    if argv[:1] == ["report"] and len(argv) == 3:
        sys.stdout.write(render(load(argv[1]), load(argv[2])))
        return 0
    print(__doc__, file=sys.stderr)
    return 0  # report-only: 使い方の誤りでも CI を落とさない


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
