#!/usr/bin/env python3
"""M2 理解层基准测试（丢弃式 spike）。

测三件事，对应 ADR-0002 §7 的待验证项：
  1. 术语纠错命中率 —— 用 M0 那段有 ground truth 的语料
  2. 标签分类准确率
  3. 常驻 RSS / tok/s / 首 token 延迟

前提：mlx-dspark serve 已在跑。
  mlx-dspark serve --model spike/models/ornith --port 8080
"""

import argparse
import json
import re
import subprocess
import sys
import time
import urllib.request

# ADR-0002 §4：M0 实测四个 ASR 模型全错的技术术语。
# 这就是 M2 存在的理由，先拿它当靶子。
TERM_CASES = [
    ("road目录", "raw"),
    ("ro的目录", "raw"),
    ("roll的目录", "raw"),
    ("闹铃是base", "knowledge base"),
    ("notice base", "knowledge base"),
    ("脑力士base", "knowledge base"),
    ("我的妈的book", "MacBook"),
    ("最好是mark mini", "Mac mini"),
    ("二四二运行", "24小时"),
    ("这是一个ID", "idea"),
    ("给你写个日报", "report"),
]

# ADR-0002 §3.1 的一级标签，封闭集合
LABELS = [
    "idea", "task", "note", "question",
    "reference", "journal", "command", "unknown",
]

TAG_CASES = [
    ("我觉得可以给录音笔加个 ESP32 自动上传", "idea"),
    ("明天把 M2 的基准测试跑完", "task"),
    ("今天开会讨论了接入层的传输协议", "note"),
    ("为什么 SenseVoice 的内存比 Nano 低这么多？", "question"),
    ("Ornith 那篇博客在 blog.mushroom.cv", "reference"),
    ("今天调了一天按键事件，有点累但总算通了", "journal"),
    ("帮我查一下明天的日程", "command"),
    ("嗯这个那个", "unknown"),
]

GLOSSARY = [
    "raw", "knowledge base", "MacBook", "Mac mini", "24小时", "idea",
    "report", "ASR", "VAD", "GGUF", "MLX", "SenseVoice", "Ornith",
    "AgentEar", "ESP32", "Raspberry Pi", "CER", "RTF", "MCP",
]


def chat(url, messages, max_tokens=512, timeout=300):
    """调 OpenAI 兼容的 /v1/chat/completions。返回 (文本, 首token延迟, 总耗时)。"""
    body = json.dumps({
        "messages": messages,
        "max_tokens": max_tokens,
        "temperature": 0.0,
        "stream": False,
    }).encode()
    req = urllib.request.Request(
        f"{url}/v1/chat/completions",
        data=body,
        headers={"Content-Type": "application/json"},
    )
    t0 = time.time()
    with urllib.request.urlopen(req, timeout=timeout) as r:
        data = json.load(r)
    dt = time.time() - t0
    text = data["choices"][0]["message"]["content"]
    usage = data.get("usage", {})
    return text, dt, usage.get("completion_tokens", 0)


def norm(s):
    return re.sub(r"[\s\-_]", "", s.lower())


def last_line(s):
    """取最后一行非空输出，即模型的最终答案。"""
    lines = [l.strip() for l in s.strip().splitlines() if l.strip()]
    return lines[-1] if lines else ""


def bench_terms(url):
    print("\n=== 1. 术语纠错 ===")
    glossary = "、".join(GLOSSARY)
    hit = 0
    for wrong, right in TERM_CASES:
        sent = f"然后把内容存到{wrong}里面。"
        prompt = (
            f"下面是一段语音转写文本，其中的技术术语可能被识别错了。\n"
            f"已知项目里常用的术语有：{glossary}\n\n"
            f"请只输出纠正后的文本，不要解释。\n\n"
            f"原文：{sent}"
        )
        out, dt, _ = chat(url, [{"role": "user", "content": prompt}], max_tokens=128)
        out = last_line(out)
        # 只看最终纠正后的句子。不能用「目标词出现在输出任意位置」做判据——
        # 模型如果吐推理过程，过程里自然会提到目标词，那是假阳性。
        # 「原错词不应残留」这道保险杠要用词边界，不能用裸子串：
        # norm("这是一个ID")="这是一个id" 是 norm("这是一个idea") 的前缀，
        # 裸子串判据会把纠对的案例误判成失败。
        residual = re.search(rf"{re.escape(norm(wrong))}(?![a-z])", norm(out))
        ok = norm(right) in norm(out) and not residual
        hit += ok
        print(f"  {'✅' if ok else '❌'} {wrong:14s} → 期望 {right:16s} | {out[:52]}")
    print(f"  命中 {hit}/{len(TERM_CASES)} = {hit/len(TERM_CASES)*100:.0f}%")
    return hit / len(TERM_CASES)


def bench_tags(url):
    print("\n=== 2. 标签分类 ===")
    labels = " / ".join(LABELS)
    hit = 0
    for text, want in TAG_CASES:
        prompt = (
            f"把下面这句话归入其中一类：{labels}\n"
            f"只输出类名，不要解释。\n\n{text}"
        )
        out, _, _ = chat(url, [{"role": "user", "content": prompt}], max_tokens=16)
        got = re.sub(r"[^a-z]", "", last_line(out).lower())
        ok = got == want
        hit += ok
        print(f"  {'✅' if ok else '❌'} {text[:26]:28s} 期望 {want:10s} 得到 {got}")
    print(f"  命中 {hit}/{len(TAG_CASES)} = {hit/len(TAG_CASES)*100:.0f}%")
    return hit / len(TAG_CASES)


def bench_speed(url):
    print("\n=== 3. 速度 ===")
    prompt = "用三句话解释什么是语音活动检测（VAD）。"
    out, dt, toks = chat(url, [{"role": "user", "content": prompt}], max_tokens=256)
    if toks:
        print(f"  生成 {toks} tokens，耗时 {dt:.2f}s → {toks/dt:.1f} tok/s")
    else:
        print(f"  耗时 {dt:.2f}s（服务未返回 usage，无法算 tok/s）")
    return dt


def rss_of(pattern):
    try:
        pid = subprocess.check_output(["pgrep", "-f", pattern], text=True).split()[0]
        kb = subprocess.check_output(["ps", "-o", "rss=", "-p", pid], text=True).strip()
        return int(kb) / 1048576
    except Exception:
        return None


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--url", default="http://127.0.0.1:8080")
    args = ap.parse_args()

    try:
        urllib.request.urlopen(f"{args.url}/v1/models", timeout=10)
    except Exception as e:
        print(f"连不上 {args.url}：{e}", file=sys.stderr)
        print("先启动：mlx-dspark serve --model spike/models/ornith --port 8080",
              file=sys.stderr)
        sys.exit(1)

    rss = rss_of("mlx-dspark")
    if rss:
        print(f"mlx-dspark 常驻 RSS: {rss:.2f} GiB")

    t = bench_terms(args.url)
    g = bench_tags(args.url)
    bench_speed(args.url)

    print(f"\n=== 汇总 ===")
    print(f"  术语纠错 {t*100:.0f}%   标签分类 {g*100:.0f}%   RSS {rss:.2f} GiB"
          if rss else f"  术语纠错 {t*100:.0f}%   标签分类 {g*100:.0f}%")


if __name__ == "__main__":
    main()
