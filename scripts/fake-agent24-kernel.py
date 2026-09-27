#!/usr/bin/env python3
"""假 Agent24 内核：**只按 Agent24 `docs/design/A3-ATTACHED-MODULE.md`（v2 @68c2412）§4/§6 写成**，
不 import 任何 Agent24 代码。用来在没有真 agent24d 的机器上跑

    agentear --talk-turn <wav> --host a3 --a3-socket <sock> --a3-token-file <file>

验证 AgentEar 的 A3 客户端端到端：握手 → 事件 → 模型回调 → 反向命令。

用法：
    scripts/fake-agent24-kernel.py --socket /tmp/fk.sock --token-file /tmp/fk.token \
        [--no-model] [--speak-after 1] [--reply 文字]

- 生成随机 token 写进 --token-file（0600），校验握手里的 auth_token / manifest_digest；
- `_a24/events/emit`：严格校验 params 只有 {kind, payload}、kind == agentear.event，逐条打印；
- `_a24/model/complete`：拒绝未知字段（含 request_id），回 tier=local 的固定回答；
- `--speak-after N`：收到第 N 个 transcript 事件后，下发一条 `_a24/command/invoke` speak，
  并校验应答是对象 `{"accepted": true}`；
- 结束时打印统计，退出码 0 = 全部校验通过。

⚠️ 这是**测试替身**，不是规格本身；规格以 Agent24 仓库的 A3 设计文档为准。
"""
import argparse
import hashlib
import json
import os
import secrets
import socket
import sys
import threading

MAX_FRAME = 1 << 20
ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
MANIFEST = os.path.join(ROOT, "assets", "agent24", "domain-os.yml")
MODEL_KEYS = {"messages", "complexity", "max_tokens", "response_format", "_meta"}

failures = []
stats = {"events": 0, "transcripts": 0, "model_calls": 0, "commands_ok": 0}


def fail(msg):
    failures.append(msg)
    print(f"✗ {msg}", flush=True)


def send(conn, lock, obj):
    data = json.dumps(obj, ensure_ascii=False).encode() + b"\n"
    with lock:
        conn.sendall(data)


def frames(conn):
    buf = b""
    while True:
        chunk = conn.recv(65536)
        if not chunk:
            return
        buf += chunk
        while b"\n" in buf:
            line, buf = buf.split(b"\n", 1)
            if len(line) > MAX_FRAME:
                fail("收到超过 1 MiB 的帧")
                return
            yield json.loads(line)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--socket", required=True)
    ap.add_argument("--token-file", required=True)
    ap.add_argument("--no-model", action="store_true")
    ap.add_argument("--speak-after", type=int, default=0)
    ap.add_argument("--reply", default="假内核的本地回答：今天天气不错。")
    a = ap.parse_args()

    digest = "sha256:" + hashlib.sha256(open(MANIFEST, "rb").read()).hexdigest()
    token = secrets.token_hex(16)
    fd = os.open(a.token_file, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    os.write(fd, token.encode())
    os.close(fd)
    if os.path.exists(a.socket):
        os.unlink(a.socket)
    srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    srv.bind(a.socket)
    os.chmod(a.socket, 0o600)
    srv.listen(1)
    print(f"假内核在 {a.socket} 等 AgentEar（digest {digest}）", flush=True)
    conn, _ = srv.accept()
    lock = threading.Lock()
    it = frames(conn)

    init = next(it)
    p = init.get("params", {})
    if init.get("method") != "initialize" or init.get("id") != "1":
        fail(f"首帧不是 initialize：{init}")
    if set(init) - {"jsonrpc", "id", "method", "params"}:
        fail("请求多了成员")
    if set(p) != {"protocol_versions", "module", "manifest_digest", "auth_token", "capabilities"}:
        fail(f"initialize params 字段不对：{sorted(p)}")
    if p.get("auth_token") != token:
        send(conn, lock, {"jsonrpc": "2.0", "id": "1", "error": {"code": -32000, "message": "auth", "data": {"kind": "auth_failed"}}})
        fail("token 不对")
        return 1
    if p.get("manifest_digest") != digest:
        send(conn, lock, {"jsonrpc": "2.0", "id": "1", "error": {"code": -32000, "message": "manifest mismatch", "data": {"kind": "manifest_mismatch"}}})
        fail("digest 不对")
        return 1
    provides = ["_a24/events/"] + ([] if a.no_model else ["_a24/model/"])
    send(conn, lock, {"jsonrpc": "2.0", "id": "1", "result": {"protocol_version": 1, "offer": {"provides": provides}}})
    print(f"✓ 握手通过，offer {provides}", flush=True)

    pending = {}
    kid = 0
    for f in it:
        if "method" not in f:
            want = pending.pop(f.get("id"), None)
            if want is None:
                fail(f"无主响应 {f}")
            elif f.get("result") != {"accepted": True}:
                fail(f"命令应答不是 {{accepted:true}}：{f}")
            else:
                stats["commands_ok"] += 1
                print(f"✓ 命令 {want} 被受理", flush=True)
            continue
        m, rid, params = f["method"], f.get("id"), f.get("params", {})
        if rid is None:
            print(f"  （模块通知 {m}：{params}）", flush=True)
            continue
        if m == "_a24/events/emit":
            if set(params) != {"kind", "payload"} or params["kind"] != "agentear.event":
                fail(f"emit params 不对：{sorted(params)}")
            ev = params["payload"]
            stats["events"] += 1
            print(f"← 事件 seq={ev.get('seq')} {ev.get('type')}: {json.dumps(ev.get('payload'), ensure_ascii=False)}", flush=True)
            send(conn, lock, {"jsonrpc": "2.0", "id": rid, "result": {}})
            if ev.get("type") == "transcript":
                stats["transcripts"] += 1
                if a.speak_after and stats["transcripts"] == a.speak_after:
                    kid += 1
                    k = f"k{kid}"
                    body = {"schema": "agentear.command/1", "command_id": f"cmd-{kid}", "type": "speak",
                            "payload": {"text": "这是宿主让我说的一句话。", "lang": "zh-CN"}}
                    pending[k] = "speak"
                    send(conn, lock, {"jsonrpc": "2.0", "id": k, "method": "_a24/command/invoke",
                                      "params": {"name": "speak", "body": body}})
        elif m == "_a24/model/complete":
            stats["model_calls"] += 1
            extra = set(params) - MODEL_KEYS
            if extra:
                fail(f"model/complete 多了字段 {sorted(extra)}")
                send(conn, lock, {"jsonrpc": "2.0", "id": rid, "error": {"code": -32602, "message": f"unknown fields {sorted(extra)}"}})
                continue
            if a.no_model:
                fail("没授予模型却收到了 model/complete")
            send(conn, lock, {"jsonrpc": "2.0", "id": rid, "result": {
                "text": a.reply, "model_id": "fake-local-2b", "tier": "local",
                "usage": {"prompt_tokens": 12, "completion_tokens": 9}}})
            print("✓ 回了一次本地模型回答", flush=True)
        else:
            send(conn, lock, {"jsonrpc": "2.0", "id": rid, "error": {"code": -32601, "message": "no such method"}})
    print(f"\n连接结束。统计：{stats}；失败 {len(failures)} 条", flush=True)
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
