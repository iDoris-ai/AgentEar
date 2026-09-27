#!/usr/bin/env python3
"""最小 WebSocket 客户端（只用标准库）：带 Bearer 连 agent24d 的 GET /api/v1/events，
把每条文本帧原样追加一行写进 --out。收到 SIGTERM 退出。

只实现本脚本需要的子集：客户端握手、读服务端（不加掩码）的文本/关闭/ping 帧、回 pong。
"""
import argparse
import base64
import os
import signal
import socket
import struct
import sys


def recv_exact(s, n):
    buf = b""
    while len(buf) < n:
        chunk = s.recv(n - len(buf))
        if not chunk:
            raise EOFError
        buf += chunk
    return buf


def send_frame(s, opcode, payload=b""):
    # 客户端发出的帧必须加掩码（RFC 6455 §5.3）。
    mask = os.urandom(4)
    hdr = bytes([0x80 | opcode])
    n = len(payload)
    if n < 126:
        hdr += bytes([0x80 | n])
    elif n < 65536:
        hdr += bytes([0x80 | 126]) + struct.pack(">H", n)
    else:
        hdr += bytes([0x80 | 127]) + struct.pack(">Q", n)
    masked = bytes(b ^ mask[i % 4] for i, b in enumerate(payload))
    s.sendall(hdr + mask + masked)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, required=True)
    ap.add_argument("--token", required=True)
    ap.add_argument("--out", required=True)
    ap.add_argument("--ready-file", required=True)
    a = ap.parse_args()
    signal.signal(signal.SIGTERM, lambda *_: sys.exit(0))
    s = socket.create_connection(("127.0.0.1", a.port), timeout=None)
    key = base64.b64encode(os.urandom(16)).decode()
    req = (
        "GET /api/v1/events HTTP/1.1\r\n"
        "Host: 127.0.0.1:%d\r\n"
        "Upgrade: websocket\r\nConnection: Upgrade\r\n"
        "Sec-WebSocket-Key: %s\r\nSec-WebSocket-Version: 13\r\n"
        "Authorization: Bearer %s\r\n\r\n" % (a.port, key, a.token)
    )
    s.sendall(req.encode())
    head = b""
    while b"\r\n\r\n" not in head:
        c = s.recv(1)
        if not c:
            sys.exit("ws: 握手时连接被关闭")
        head += c
    status = head.split(b"\r\n", 1)[0]
    if b" 101 " not in status:
        sys.exit("ws: 握手失败：%s" % status.decode(errors="replace"))
    with open(a.ready_file, "w") as f:
        f.write("ok")
    out = open(a.out, "a", buffering=1)
    while True:
        b1, b2 = recv_exact(s, 2)
        op = b1 & 0x0F
        n = b2 & 0x7F
        if n == 126:
            n = struct.unpack(">H", recv_exact(s, 2))[0]
        elif n == 127:
            n = struct.unpack(">Q", recv_exact(s, 8))[0]
        mask = recv_exact(s, 4) if b2 & 0x80 else None
        data = recv_exact(s, n)
        if mask:
            data = bytes(b ^ mask[i % 4] for i, b in enumerate(data))
        if op == 0x1:
            out.write(data.decode("utf-8", errors="replace").replace("\n", " ") + "\n")
        elif op == 0x9:
            send_frame(s, 0xA, data)
        elif op == 0x8:
            return


if __name__ == "__main__":
    try:
        main()
    except EOFError:
        pass
