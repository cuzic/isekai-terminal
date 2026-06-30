#!/usr/bin/env -S python3 -u
"""
Android ログ受信サーバー。
Android アプリから POST されたログを標準出力に表示する。
Tailscale IP で listen するので、実機から直接届く。

使い方:
  python3 tools/logserver.py          # ポート 9876 でリッスン
  python3 tools/logserver.py 9999     # ポート指定
"""
import sys
import json
from http.server import BaseHTTPRequestHandler, HTTPServer
from datetime import datetime

PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 9876
RESET = "\033[0m"
RED   = "\033[31m"
YELLOW= "\033[33m"
CYAN  = "\033[36m"
GRAY  = "\033[90m"

LEVEL_COLOR = {"V": GRAY, "D": CYAN, "I": RESET, "W": YELLOW, "E": RED, "A": RED}

class LogHandler(BaseHTTPRequestHandler):
    def do_POST(self):
        length = int(self.headers.get("Content-Length", 0))
        body = self.rfile.read(length).decode("utf-8")
        self.send_response(200)
        self.end_headers()

        try:
            entries = json.loads(body)
            if not isinstance(entries, list):
                entries = [entries]
            for e in entries:
                level = e.get("level", "D")
                tag   = e.get("tag", "app")
                msg   = e.get("msg", "")
                ts    = e.get("ts", datetime.now().strftime("%H:%M:%S.%f")[:-3])
                color = LEVEL_COLOR.get(level, RESET)
                print(f"{GRAY}{ts}{RESET} {color}{level}/{tag}: {msg}{RESET}", flush=True)
        except Exception:
            # プレーンテキストとして表示
            print(body, flush=True)

    def log_message(self, format, *args):  # noqa: A002
        pass  # HTTP アクセスログを抑制

print(f"🟢 Log server listening on 0.0.0.0:{PORT}")
print(f"   Android から → http://100.100.45.36:{PORT}/log\n")
HTTPServer(("0.0.0.0", PORT), LogHandler).serve_forever()
