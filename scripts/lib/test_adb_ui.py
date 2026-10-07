#!/usr/bin/env python3
"""adb_ui.input_text_shell_arg の単体テスト(adb/実機は使わない)。

端末側で `adb shell "input text <arg>"` が `sh -c` 経由で解釈されるのと同じく、
ローカルの `sh -c` に通して `input` が受け取る引数(=`%s`変換後の値)をそのまま
取り出せるかを確かめる。

実行: python3 scripts/lib/test_adb_ui.py
"""
import subprocess
import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import adb_ui  # noqa: E402


def argv_seen_by_input(arg):
    """`sh -c "input text <arg>"` で `input` が受け取る引数列を返す(`input` の代わりに printf)。"""
    out = subprocess.run(
        ["sh", "-c", f"printf '%s\\n' {arg}"],
        check=True, capture_output=True, text=True,
    ).stdout
    return out.splitlines()


class InputTextShellArgTests(unittest.TestCase):
    def assert_round_trip(self, value):
        arg = adb_ui.input_text_shell_arg(value)
        self.assertEqual(argv_seen_by_input(arg), [value.replace(" ", "%s")])

    def test_plain_ascii(self):
        self.assert_round_trip("isekai-e2e-123")

    def test_spaces_become_percent_s_in_a_single_argument(self):
        self.assert_round_trip("isekai pipe ctl")

    def test_shell_metacharacters_are_not_interpreted(self):
        # 以前のエスケープ方式で取りこぼしていた文字(`#`以降のコメント化・グロブ展開等)を含む。
        self.assert_round_trip("a*b?c~d#e!f{g}h[i]j&k|l;m<n>o(p)q$HOME`id`\\r")

    def test_quotes(self):
        self.assert_round_trip("it's \"quoted\"")

    def test_newline_is_rejected(self):
        with self.assertRaises(SystemExit):
            adb_ui.input_text_shell_arg("line1\nline2")

    def test_literal_percent_s_is_rejected(self):
        with self.assertRaises(SystemExit):
            adb_ui.input_text_shell_arg("100%sure")

    def test_other_percent_is_allowed(self):
        self.assert_round_trip("100% done")


if __name__ == "__main__":
    unittest.main()
