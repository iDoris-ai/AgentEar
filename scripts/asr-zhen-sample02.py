#!/usr/bin/env python3
"""T3.5.1：用 M0 口径给 sample02（jason 口述 README）的转写打分，和 M0 对账。

M0 的做法（docs/benchmarks.md §3.1）：
- 参考是 `spike/ref.txt`——**另一个 ASR 的转写**，所以测的是系统间分歧率；
- 转写开头的「我们来测试一下这个录音功能。我来说一段话吧。」参考里没有，
  **计算时剔除**。这里按「从 `对，那以前` 起算」实现，找不到这个锚点就报错，
  不静默用全文（全文会多出十几个字的插入错误，数字就和 M0 不可比了）。
- 两个口径：原始、剔除语气词/助词（照抄 `spike/cer2.py`）。

用法：
    python3 scripts/asr-zhen-sample02.py spike/ref.txt docs/data/asr-zh-en-2026-09/sample02-*.txt
"""
import re
import sys
import unicodedata

ANCHOR = "对，那以前"


def norm(s, strip_filler=False):
    s = unicodedata.normalize("NFKC", s).lower()
    s = re.sub(r"[^\w一-鿿]", "", s)
    if strip_filler:
        s = re.sub(r"[嗯呃啊哦呀吧对了的]", "", s)
    return s


def edit(a, b):
    if len(a) < len(b):
        a, b = b, a
    prev = list(range(len(b) + 1))
    for i, ca in enumerate(a, 1):
        cur = [i]
        for j, cb in enumerate(b, 1):
            cur.append(min(prev[j] + 1, cur[j - 1] + 1, prev[j - 1] + (ca != cb)))
        prev = cur
    return prev[-1]


def main():
    if len(sys.argv) < 3:
        print(__doc__)
        return 2
    ref = open(sys.argv[1]).read()
    print("| 文件 | CER（含语气词） | CER（剔语气词/助词） |")
    print("|---|---|---|")
    for path in sys.argv[2:]:
        hyp = open(path).read()
        i = hyp.find(ANCHOR)
        if i < 0:
            print(f"{path} 里找不到锚点 {ANCHOR!r}，拒绝打分", file=sys.stderr)
            return 1
        hyp = hyp[i:]
        cells = []
        for sf in (False, True):
            r, h = norm(ref, sf), norm(hyp, sf)
            cells.append(f"{100.0 * edit(r, h) / len(r):.2f}%")
        print(f"| {path.rsplit('/', 1)[-1]} | {cells[0]} | {cells[1]} |")
    return 0


if __name__ == "__main__":
    sys.exit(main())
