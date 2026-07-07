#!/usr/bin/env python3
"""uiautomator dump ベースの薄い UI 操作ヘルパー。

TESTING.md の手動確認手順のうち adb だけで完結する部分を
scripts/device_verify.sh からスクリプト化するために使う。
Espresso/Compose のセマンティクステスト(app/src/androidTest)は使えない
(実サーバー・実ネットワークに対する実機E2E検証のため)ので、
uiautomator dump の座標ベースでタップ/入力する。

同一目的のロジックを何度も再実装しないよう、tap/type/scroll の座標計算は
すべてここに集約する。
"""
import argparse
import re
import subprocess
import sys
import time
import xml.etree.ElementTree as ET

BOUNDS_RE = re.compile(r"\[(-?\d+),(-?\d+)\]\[(-?\d+),(-?\d+)\]")


def adb(device, *args, check=True):
    cmd = ["adb"]
    if device:
        cmd += ["-s", device]
    cmd += list(args)
    return subprocess.run(cmd, check=check, capture_output=True, text=True)


def dump_nodes(device):
    """端末上で uiautomator dump し、フラットな node 属性 dict のリスト(文書順)を返す。"""
    adb(device, "shell", "uiautomator", "dump", "/sdcard/isekai_e2e_dump.xml")
    out = adb(device, "shell", "cat", "/sdcard/isekai_e2e_dump.xml").stdout
    # uiautomator dump は内部で一時的に回転をロックし、終了時に "restore" するが、
    # その復元先は常に auto-rotate 有効(USER_ROTATION_FREE)になることを実機で確認した
    # (dumpsys window の UiAutomationConnection#restoreRotationStateLocked ログで確認)。
    # つまり dump 1回ごとに、こちらが設定した縦固定が毎回上書きされてしまう。
    # bounds は縦向き前提で計算するため、直後にタップ座標がずれないよう毎回再ロックする。
    adb(device, "shell", "settings", "put", "system", "accelerometer_rotation", "0")
    root = ET.fromstring(out)
    nodes = []
    for n in root.iter("node"):
        a: dict = dict(n.attrib)
        m = BOUNDS_RE.match(a.get("bounds", ""))
        if m:
            x1, y1, x2, y2 = map(int, m.groups())
            a["_bounds"] = (x1, y1, x2, y2)
            a["_center"] = ((x1 + x2) // 2, (y1 + y2) // 2)
        nodes.append(a)
    return nodes


def matches(node, text=None, contains=None, content_desc=None, cls=None, resource_id=None):
    if text is not None and node.get("text") != text:
        return False
    if contains is not None and contains not in (node.get("text") or ""):
        return False
    if content_desc is not None and node.get("content-desc") != content_desc:
        return False
    if cls is not None and node.get("class") != cls:
        return False
    if resource_id is not None and node.get("resource-id") != resource_id:
        return False
    return True


def find_all(device, **kw):
    return [n for n in dump_nodes(device) if matches(n, **kw)]


def find_with_retry(device, timeout, interval, nth, **kw):
    deadline = time.time() + timeout
    last_count = 0
    while time.time() < deadline:
        found = find_all(device, **kw)
        last_count = len(found)
        if found:
            try:
                return found[nth]
            except IndexError:
                pass
        time.sleep(interval)
    raise SystemExit(
        f"NOT FOUND after {timeout}s: {kw} (nth={nth}, matches so far={last_count})"
    )


def biased_point(node, x_bias, y_bias):
    """node の bounds 内で (x_bias, y_bias) の位置(0.0=左/上端, 1.0=右/下端, 0.5=中央)の座標を返す。

    ExposedDropdownMenuBox の読み取り専用フィールドは、見た目上は箱全体が
    クリック可能に見えるが、実際にタップが反応する領域が末尾のアイコン付近に
    偏っていて中央タップでは展開しないケースを実機で確認した。中央以外の
    位置を明示的に狙えるようにしておく。
    """
    x1, y1, x2, y2 = node["_bounds"]
    return (int(x1 + (x2 - x1) * x_bias), int(y1 + (y2 - y1) * y_bias))


def cmd_tap(args):
    node = find_with_retry(
        args.device, args.timeout, args.interval, args.nth,
        text=args.text, contains=args.contains, content_desc=args.content_desc,
        cls=args.class_name, resource_id=args.resource_id,
    )
    x, y = biased_point(node, args.x_bias, args.y_bias)
    adb(args.device, "shell", "input", "tap", str(x), str(y))
    print(f"tapped ({x},{y}) text={node.get('text')!r} resource-id={node.get('resource-id')!r}")


def find_field_near_label(device, label, timeout, interval):
    """label の TextView を探し、その y レンジを bounds に含む EditText を探す。

    ProfileEditScreen/KeyImportScreen の OutlinedTextField は
    EditText ノードの bounds がラベルの bounds を包含する形で
    (ラベルが先、フィールドが後ではなく、フィールドの bounds の中に
    ラベルが収まる形で)ダンプされることを実機で確認済み。
    """
    deadline = time.time() + timeout
    while time.time() < deadline:
        nodes = dump_nodes(device)
        label_nodes = [n for n in nodes if n.get("text") == label]
        edit_nodes = [n for n in nodes if n.get("class") == "android.widget.EditText"]
        for ln in label_nodes:
            _, ly1, _, ly2 = ln["_bounds"]
            lyc = (ly1 + ly2) // 2
            for en in edit_nodes:
                _, ey1, _, ey2 = en["_bounds"]
                if ey1 <= lyc <= ey2:
                    return en
        time.sleep(interval)
    raise SystemExit(f"NOT FOUND: field near label {label!r}")


def find_nearest_to_anchor(device, anchor_text, timeout, interval, **target_kw):
    """anchor_text と同じ行(カード)にある target ノードを、縦位置が最も近いもので特定する。

    ProfileCard/KeyCard は「ラベル … 編集/削除」のように同じ行に複数の
    カードが並ぶため、一覧に複数エントリがあっても正しい行の「削除」等を
    タップできるようにする。
    """
    deadline = time.time() + timeout
    while time.time() < deadline:
        nodes = dump_nodes(device)
        anchors = [n for n in nodes if n.get("text") == anchor_text]
        targets = [n for n in nodes if matches(n, **target_kw)]
        if anchors and targets:
            ay = anchors[0]["_center"][1]
            return min(targets, key=lambda t: abs(t["_center"][1] - ay))
        time.sleep(interval)
    raise SystemExit(f"NOT FOUND: anchor={anchor_text!r} target={target_kw}")


def cmd_tap_near(args):
    node = find_nearest_to_anchor(
        args.device, args.anchor, args.timeout, args.interval,
        text=args.text, contains=args.contains, resource_id=args.resource_id,
    )
    x, y = node["_center"]
    adb(args.device, "shell", "input", "tap", str(x), str(y))
    print(f"tapped near anchor={args.anchor!r} at ({x},{y}) text={node.get('text')!r} resource-id={node.get('resource-id')!r}")


def cmd_tap_near_label(args):
    node = find_field_near_label(args.device, args.label, args.timeout, args.interval)
    x, y = node["_center"]
    adb(args.device, "shell", "input", "tap", str(x), str(y))
    print(f"tapped field near label={args.label!r} at ({x},{y})")


def cmd_type(args):
    if not args.resource_id and not args.label:
        raise SystemExit("type: --resource-id か --label のどちらかを指定してください")
    if args.resource_id:
        node = find_with_retry(
            args.device, args.timeout, args.interval, 0, resource_id=args.resource_id,
        )
    else:
        node = find_field_near_label(args.device, args.label, args.timeout, args.interval)
    x, y = node["_center"]
    adb(args.device, "shell", "input", "tap", str(x), str(y))
    time.sleep(0.3)
    # フィールドに前回実行の残り値が入っている状態でタップ+入力すると、カーソル位置に
    # 挿入されるだけで置換されず、意図しない文字列(空欄判定をすり抜ける不正な結合文字列)
    # になることを実機で確認した。再実行の冪等性のため、末尾へ移動してから既存の内容を
    # 確実に削除してから入力する。
    adb(args.device, "shell", "input", "keyevent", "KEYCODE_MOVE_END")
    del_keys = ["input", "keyevent"] + ["67"] * 128  # KEYCODE_DEL x128 (十分に長いフィールドをカバー)
    adb(args.device, "shell", *del_keys)
    time.sleep(0.2)
    # adb shell input text は空白を %s に、他のシェル特殊文字はエスケープする必要がある。
    escaped = args.value.replace("\\", "\\\\").replace(" ", "%s")
    for ch in ("&", "(", ")", "<", ">", "|", ";", "'", '"', "`", "$"):
        escaped = escaped.replace(ch, f"\\{ch}")
    adb(args.device, "shell", f"input text {escaped}")
    # Compose 側の state 反映(IME commitText → recompose)が非同期のため、
    # 直後に別ノードをタップすると古い state のまま判定されることがある(実機で確認済み)。
    time.sleep(0.6)
    locator = f"resource-id={args.resource_id!r}" if args.resource_id else f"label={args.label!r}"
    print(f"typed {args.value!r} into field ({locator})")


def cmd_scroll_to(args):
    for i in range(args.max_swipes):
        found = find_all(args.device, text=args.text, contains=args.contains, resource_id=args.resource_id)
        if found:
            print(f"found {args.text or args.contains or args.resource_id!r} after {i} swipe(s)")
            return
        adb(args.device, "shell", "input", "swipe",
            str(args.x), str(args.y_from), str(args.x), str(args.y_to), "200")
        time.sleep(0.3)
    raise SystemExit(
        f"scroll-to: NOT FOUND within {args.max_swipes} swipes: "
        f"text={args.text!r} contains={args.contains!r} resource_id={args.resource_id!r}"
    )


def cmd_get_prefix(args):
    """text が prefix で始まる最初(nth指定可)のノードのテキストを出力する。

    鍵生成成功ダイアログはその瞬間 ssh-ed25519 行を1つしか表示しないため、
    ラベルでの絞り込み(ダイアログ自体にはラベルが表示されない)は不要。
    """
    deadline = time.time() + args.timeout
    while time.time() < deadline:
        found = [n for n in dump_nodes(args.device) if (n.get("text") or "").startswith(args.prefix)]
        if found:
            try:
                print(found[args.nth]["text"])
                return
            except IndexError:
                pass
        time.sleep(args.interval)
    raise SystemExit(f"get-prefix: NOT FOUND within {args.timeout}s: prefix={args.prefix!r}")


def cmd_exists(args):
    found = find_all(
        args.device, text=args.text, contains=args.contains,
        content_desc=args.content_desc, resource_id=args.resource_id,
    )
    print("yes" if found else "no")
    sys.exit(0 if found else 1)


def cmd_dump_text(args):
    for n in dump_nodes(args.device):
        if n.get("text") or n.get("content-desc") or n.get("resource-id"):
            print(
                f"{n.get('text')!r} | {n.get('content-desc')!r} | "
                f"resource-id={n.get('resource-id')!r} | {n.get('class')} | {n.get('bounds')}"
            )


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--device", default=None, help="adb -s SERIAL")
    sub = p.add_subparsers(dest="cmd", required=True)

    t = sub.add_parser("tap", help="text/content-desc/class/resource-id でノードを探してタップ")
    t.add_argument("--text")
    t.add_argument("--contains")
    t.add_argument("--content-desc")
    t.add_argument("--class-name")
    t.add_argument("--resource-id", help="Compose の testTag (testTagsAsResourceId 有効時、debugビルドのみ)")
    t.add_argument("--nth", type=int, default=0)
    t.add_argument("--x-bias", type=float, default=0.5, help="bounds内のタップ位置(0.0=左端〜1.0=右端、既定0.5=中央)")
    t.add_argument("--y-bias", type=float, default=0.5, help="bounds内のタップ位置(0.0=上端〜1.0=下端、既定0.5=中央)")
    t.add_argument("--timeout", type=float, default=8.0)
    t.add_argument("--interval", type=float, default=0.4)
    t.set_defaults(func=cmd_tap)

    tn = sub.add_parser("tap-near", help="anchorテキストと同じ行にあるtext/contains/resource-idノードをタップ")
    tn.add_argument("--anchor", required=True)
    tn.add_argument("--text")
    tn.add_argument("--contains")
    tn.add_argument("--resource-id", help="Compose の testTag")
    tn.add_argument("--timeout", type=float, default=8.0)
    tn.add_argument("--interval", type=float, default=0.4)
    tn.set_defaults(func=cmd_tap_near)

    tl = sub.add_parser("tap-near-label", help="ラベル直下(を包含する)EditTextをタップ(testTag未対応フィールド用)")
    tl.add_argument("--label", required=True)
    tl.add_argument("--timeout", type=float, default=8.0)
    tl.add_argument("--interval", type=float, default=0.4)
    tl.set_defaults(func=cmd_tap_near_label)

    ty = sub.add_parser("type", help="フィールドをタップしてテキスト入力(--resource-id 優先、無ければ --label で近傍探索)")
    ty.add_argument("--label", help="testTag未対応フィールド用: ラベルテキストで近傍のEditTextを探す")
    ty.add_argument("--resource-id", help="Compose の testTag。指定時はこちらを使う(--labelより堅牢)")
    ty.add_argument("--value", required=True)
    ty.add_argument("--timeout", type=float, default=8.0)
    ty.add_argument("--interval", type=float, default=0.4)
    ty.set_defaults(func=cmd_type)

    sc = sub.add_parser("scroll-to", help="指定テキスト/resource-idが見つかるまで下スワイプ")
    sc.add_argument("--text")
    sc.add_argument("--contains")
    sc.add_argument("--resource-id")
    sc.add_argument("--max-swipes", type=int, default=15)
    sc.add_argument("--x", type=int, default=540)
    sc.add_argument("--y-from", type=int, default=1800)
    sc.add_argument("--y-to", type=int, default=400)
    sc.set_defaults(func=cmd_scroll_to)

    gp = sub.add_parser("get-prefix", help="prefixで始まるテキストを持つノードの内容を出力")
    gp.add_argument("--prefix", required=True)
    gp.add_argument("--nth", type=int, default=0)
    gp.add_argument("--timeout", type=float, default=8.0)
    gp.add_argument("--interval", type=float, default=0.4)
    gp.set_defaults(func=cmd_get_prefix)

    ex = sub.add_parser("exists", help="単発チェック(存在すれば exit0/yes、なければexit1/no)")
    ex.add_argument("--text")
    ex.add_argument("--contains")
    ex.add_argument("--content-desc")
    ex.add_argument("--resource-id")
    ex.set_defaults(func=cmd_exists)

    dt = sub.add_parser("dump-text", help="デバッグ用: 現在の画面の全テキストを表示")
    dt.set_defaults(func=cmd_dump_text)

    args = p.parse_args()
    args.func(args)


if __name__ == "__main__":
    main()
