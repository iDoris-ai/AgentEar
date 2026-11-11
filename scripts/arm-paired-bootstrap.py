#!/usr/bin/env python3
"""T3.5.2：Arm 泰语语料上两份逐条转写 TSV 的逐条 CER 与配对 bootstrap。

用法：
    python -I scripts/arm-paired-bootstrap.py <A.tsv> <B.tsv> [重采样次数=20000] [种子=20261110]

输入是 `category take reference hypothesis` 四列 TSV（qwen3-asr-2026-09-08.tsv 的格式）。
两份必须逐条同 category/take/reference，否则退出（不配对）。

CER 口径（RESULTS.md 第六节）：去掉所有空白后做字符级编辑距离，拉丁字母转小写，
不做泰语正规化；本脚本额外**不去标点**（RESULTS.md 没说去，保持字面）。
编辑距离按 Unicode code point。
语料级 CER = sum(edits)/sum(ref_len)（micro）；bootstrap 以**录音为单位**有放回重采样，
同一组索引同时算 A、B，输出 Δ = CER(A) − CER(B) 的点估计与百分位 95% 区间
（np 风格线性插值不用，取对称离散下标）。分三个集合：全部 22 条、
code-switch 16 条（daily_collaboration + devops）、纯泰语对照 6 条。
推断单位是录音，说话人只有 Arm 一人，结论只能说 Arm 这个说话人。
"""
import csv
import random
import sys
import unicodedata


def norm(s):
    return "".join(unicodedata.normalize("NFC", s).lower().split())


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


def load(path):
    rows = list(csv.DictReader(open(path, encoding="utf-8", newline=""), delimiter="\t", quoting=csv.QUOTE_NONE))
    out = []
    for r in rows:
        ref, hyp = norm(r["reference"]), norm(r["hypothesis"])
        out.append((r["category"], r["take"], r["reference"], edit(ref, hyp), len(ref)))
    return out


def cer(items, idx):
    return sum(items[i][3] for i in idx) / sum(items[i][4] for i in idx)


def main():
    if not 3 <= len(sys.argv) <= 5:
        print(__doc__)
        return 2
    A, B = load(sys.argv[1]), load(sys.argv[2])
    n_boot = int(sys.argv[3]) if len(sys.argv) > 3 else 20000
    seed = int(sys.argv[4]) if len(sys.argv) > 4 else 20261110
    if n_boot < 10000:
        raise SystemExit("重采样次数不得少于 10000")
    if [(a[0], a[1], a[2]) for a in A] != [(b[0], b[1], b[2]) for b in B]:
        raise SystemExit("!! 两份 TSV 的条目/参考文本不一一对应，不能配对")
    print(f"A = {sys.argv[1]}\nB = {sys.argv[2]}\n重采样 {n_boot} 次，种子 {seed}，Δ = CER(A) − CER(B)\n")
    sets = {
        "全部": list(range(len(A))),
        "code-switch(daily+devops)": [i for i, a in enumerate(A) if a[0] != "control_thai_only"],
        "纯泰语对照": [i for i, a in enumerate(A) if a[0] == "control_thai_only"],
    }
    for name, ids in sets.items():
        rnd = random.Random(seed)
        N = len(ids)
        diffs = []
        for _ in range(n_boot):
            idx = [ids[rnd.randrange(N)] for _ in range(N)]
            diffs.append(cer(A, idx) - cer(B, idx))
        diffs.sort()
        lo = int(0.025 * n_boot)
        pa, pb = cer(A, ids), cer(B, ids)
        l, h = diffs[lo], diffs[n_boot - 1 - lo]
        verdict = "区间含 0（未检出差异）" if (l <= 0 <= h) else "区间不含 0"
        if N < 10:
            # 退化情形：只有少数几条有差异时，自助法差值恒同号，区间机械地不含 0，
            # 不是校准过的显著性证据（对抗自审指出）。
            verdict += "；n<10，只作描述，不下显著性结论"
        print(f"{name:28s} n={N:2d}  A={pa*100:5.2f}%  B={pb*100:5.2f}%  "
              f"Δ={100*(pa-pb):+6.2f} pp  95%CI=[{100*l:+6.2f}, {100*h:+6.2f}] pp  {verdict}")
    print("\n注：「未检出差异」≠「无差异」≠「等效」；没有预定义非劣界，不做非劣结论。"
          "\n逐条 CER：")
    print("category\ttake\tA_edits\tB_edits\tref_len\tA_cer\tB_cer")
    # 第二份输入用同函数再算一遍逐条
    for a, b in zip(A, B):
        print(f"{a[0]}\t{a[1]}\t{a[3]}\t{b[3]}\t{a[4]}\t{a[3]/a[4]:.4f}\t{b[3]/b[4]:.4f}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
