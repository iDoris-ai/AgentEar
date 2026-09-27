#!/usr/bin/env python3
"""外部计数桩：冒充一个「非回环地址上的 OpenAI 兼容 provider」（Agent24 判为 Remote 层）。

每收到一个请求就把计数写进 --count-file（原子替换），并回一个合法的 chat completion，
好让「如果真被路由到这里」的情况表现为成功而不是报错——这样计数 > 0 就是唯一信号，
不会被「请求失败后退回本地」掩盖。只用标准库。
"""
import argparse
import json
import os
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

LOCK = threading.Lock()
COUNT = 0


def bump(path):
    global COUNT
    with LOCK:
        COUNT += 1
        tmp = path + ".tmp"
        with open(tmp, "w") as f:
            f.write(str(COUNT))
        os.replace(tmp, path)


class H(BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def _reply(self, obj):
        body = json.dumps(obj).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        bump(self.server.count_file)
        self._reply({"object": "list", "data": [{"id": "remote-stub", "object": "model"}]})

    def do_POST(self):
        n = int(self.headers.get("Content-Length") or 0)
        self.rfile.read(n)
        bump(self.server.count_file)
        self._reply({
            "id": "stub", "object": "chat.completion", "model": "remote-stub",
            "choices": [{"index": 0, "finish_reason": "stop",
                         "message": {"role": "assistant", "content": "REMOTE-STUB-REPLY"}}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
        })


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--host", required=True)
    ap.add_argument("--port", type=int, default=0)
    ap.add_argument("--count-file", required=True)
    ap.add_argument("--port-file", required=True)
    a = ap.parse_args()
    with open(a.count_file, "w") as f:
        f.write("0")
    srv = ThreadingHTTPServer((a.host, a.port), H)
    srv.count_file = a.count_file
    with open(a.port_file, "w") as f:
        f.write(str(srv.server_address[1]))
    srv.serve_forever()


if __name__ == "__main__":
    main()
