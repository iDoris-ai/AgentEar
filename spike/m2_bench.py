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
import pathlib
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

# 一级标签的定义与判别问题。**这是 M0 那 6/8 缺的东西**——
# 当时的提示词只有一行「归入其中一类，只输出类名」，没有任何定义，
# 模型只能按标签名的字面意思猜。完整论证见 docs/agent/label-taxonomy.md。
LABEL_RULES = """\
idea：一个还没决定要不要做的想法。判别：他承诺要做了吗？没有 → idea
task：一件确定要做的事，有交付物。判别：要我记下来以后做吗？是 → task
command：要系统现在执行的指令。判别：他在等系统立刻给反应吗？是 → command
note：一条知识、事实、结论。判别：一年后单独拿出来看还成立吗？成立 → note
journal：当天发生了什么、当时的状态。判别：离开「今天」这个语境还有意义吗？没有 → journal
question：一个待解答的疑问。判别：这句话的目的是求一个答案吗？是 → question
  ⚠️ 口语里疑问句**常常没有问号**（「泰语模型能不能在 Intel Mac 上跑」
  「现在几点了」都是问句）。不要靠标点判断。
reference：指向外部资源的指针。判别：主体是链接或出处吗？是 → reference
unknown：无法归类或内容无意义。以上都不像就选它，宁可 unknown 不要瞎猜
  ⚠️ 语气词、口头禅、附和（「嗯这个那个」「啊对对对」）一律 unknown，
  不要因为它可能暗示某种态度就归到 idea

两组最容易混的：
- note vs journal：看它离不离得开「今天」。「冷启动 0.2 秒」是事实(note)；
  「今天开会讨论了传输协议」离开时间就没信息量(journal)
- command vs task：看他等不等系统立刻反应。「帮我查日程」在等回答(command)；
  「记得给术语表加词」是让系统记下来(task)
"""

# 18 条，每类至少 2 条。**含 M0 判错的两条原样保留**，以及若干贴着边界的。
# `Q1` 标记的那条期望值待 jason 确认，见 label-taxonomy.md §2.1。
TAG_CASES = [
    ("我觉得可以给录音笔加个 ESP32 自动上传", "idea"),
    ("要是能用语音直接建任务就好了", "idea"),
    ("这个方案要不要做，我还没想好", "idea"),
    ("明天把 M2 的基准测试跑完", "task"),
    ("记得给术语表加上 Kubernetes", "task"),
    ("帮我查一下明天的日程", "command"),        # M0 判错的那条
    ("把刚才那段录音删掉", "command"),
    ("现在几点了", "question"),   # 原本归 command,是我应用定义时错了:它在求一个答案
    ("SenseVoice 的冷启动只要 0.2 秒", "note"),
    ("whisper.cpp 的 Metal 首次运行要多花几秒编译 shader", "note"),
    ("今天开会讨论了接入层的传输协议", "journal"),  # M0 判错的那条，期望值待确认(Q1)
    ("今天调了一天按键事件，有点累但总算通了", "journal"),
    ("为什么 SenseVoice 的内存比 Nano 低这么多？", "question"),
    ("泰语模型能不能在 Intel Mac 上跑", "question"),
    ("Ornith 那篇博客在 blog.mushroom.cv", "reference"),
    ("ADR-0004 里记了泰语选型的全部局限", "reference"),
    ("嗯这个那个", "unknown"),
    ("啊对对对", "unknown"),
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


def bench_tags(url, binary):
    """标签分类。

    **走生产二进制的 `--classify`，不自带提示词和解析器。**

    早先这里有一份自己的提示词和一个更宽松的解析器（删掉所有非 [a-z]
    字符再匹配），结果基准报 18/18 而生产路径报 17/18——差异稳定复现，
    排除了标签顺序、期望值、规则语义之后仍然查不出根因
    （docs/benchmarks-m2.md §9）。

    评测和产品共用同一段代码之后，这类疑问从根上就不会出现：
    基准报的就是产品的行为。
    """
    print("\n=== 2. 标签分类（走生产路径 --classify）===")
    if not binary:
        print("  ⚠️ 找不到 agentear 二进制，跳过。先 cargo build --release")
        return 0.0
    hit = 0
    got_all = []
    for text, want in TAG_CASES:
        try:
            got = subprocess.run(
                [binary, "--classify", text],
                capture_output=True, text=True, timeout=60,
            ).stdout.strip()
        except subprocess.TimeoutExpired:
            got = "(超时)"
        got_all.append(got)
        ok = got == want
        hit += ok
        print(f"  {'✅' if ok else '❌'} {text[:26]:28s} 期望 {want:10s} 得到 {got}")
    print(f"  命中 {hit}/{len(TAG_CASES)} = {hit/len(TAG_CASES)*100:.0f}%")
    per = {}
    for (text, want), got in zip(TAG_CASES, got_all):
        d = per.setdefault(want, [0, 0])
        d[1] += 1
        d[0] += (got == want)
    print("  按类：" + "  ".join(f"{k} {v[0]}/{v[1]}" for k, v in sorted(per.items())))
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
    ap.add_argument("--url", default="http://127.0.0.1:8793")
    # 标签评测走这个二进制的 --classify。默认找仓库的 release 产物；
    # 找不到就跳过标签那一节而不是退回自带实现——**宁可少一个数字,
    # 也不要一个和产品对不上的数字**。
    ap.add_argument(
        "--binary",
        default=str(pathlib.Path(__file__).resolve().parent.parent / "target/release/agentear"),
    )
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
    binary = args.binary if pathlib.Path(args.binary).exists() else None
    g = bench_tags(args.url, binary)
    bench_speed(args.url)

    print(f"\n=== 汇总 ===")
    print(f"  术语纠错 {t*100:.0f}%   标签分类 {g*100:.0f}%   RSS {rss:.2f} GiB"
          if rss else f"  术语纠错 {t*100:.0f}%   标签分类 {g*100:.0f}%")


if __name__ == "__main__":
    main()
