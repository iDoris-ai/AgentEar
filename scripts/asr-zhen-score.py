#!/usr/bin/env python3
"""T3.5.1：给 `asr-zhen-run.py` 的产出打分。不跑推理，随时可重算。

用法：
    python3 scripts/asr-zhen-score.py <manifest.json> <hyp 目录>

hyp 目录里的文件名形如 `<引擎>-r<k>.tsv`。同一引擎的多次运行分别打分，
报**中位数与极值**（不报区间），并检查几次运行的转写是否逐字一致。

## 口径

- **CER（M0 口径）**：照抄 `spike/cer.py` —— NFKC → 小写 → 去掉一切非 `\\w`
  与非 CJK 的字符（标点、**空白**全部去掉）→ 按字符算编辑距离。
  三组都报这一个数，所以英文部分在这个口径下是**字母级**的。
  中英混合组用它，是因为 ASCEND 的参考把缩写写成 `i s m`，而两个引擎都写 `ISM`：
  按词切会把这种纯排版差异算成三个错。
- **WER（仅英文组）**：NFKC → 小写 → 非 `[a-z0-9]` 一律换成空格（撇号直接删）
  → 按空白切词。
- 汇总是**语料级**（总编辑数 / 总参考长度），不是逐句 CER 的平均——
  后者会被短句放大。
- ⚠️ **数字写法不归一**：参考写 `25`、引擎写「二十五」（或反过来）照样记错。
  两个引擎都做 ITN，所以这一项对二者大致同向，但**不是零影响**。

## 配对比较

对每组、每一对引擎，用**第 1 次运行**的逐句编辑数做配对 bootstrap
（按句重抽样，4000 次，种子 20260925），报 CER 差值的 95% 区间。
区间跨过 0 = **未检出差异**，不等于「等效」（没有预设非劣界）。
"""
import json
import os
import random
import re
import sys
import unicodedata
from collections import defaultdict
from statistics import median


def norm_m0(s):
    s = unicodedata.normalize("NFKC", s).lower()
    return re.sub(r"[^\w一-鿿]", "", s)


def words_en(s):
    s = unicodedata.normalize("NFKC", s).lower().replace("'", "").replace("’", "")
    return re.sub(r"[^a-z0-9]+", " ", s).split()


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


def load_tsv(path):
    out = {}
    with open(path) as f:
        for line in f:
            key, wall, hyp = line.rstrip("\n").split("\t", 2)
            out[key] = (float(wall), hyp)
    return out


def main():
    if len(sys.argv) != 3:
        print(__doc__)
        return 2
    manifest = json.load(open(sys.argv[1]))
    hyp_dir = sys.argv[2]
    runs = defaultdict(list)  # engine -> [(k, path)]
    for fn in sorted(os.listdir(hyp_dir)):
        m = re.fullmatch(r"(.+)-r(\d+)\.tsv", fn)
        if m:
            runs[m.group(1)].append((int(m.group(2)), os.path.join(hyp_dir, fn)))

    sets = manifest["sets"]
    per_utt = {}  # (engine, set, metric) -> list of (edits, reflen) from run 1
    print("## 准确率（语料级，每格 = 中位数 [最小, 最大]，n = 运行次数）\n")
    print("| 引擎 | 组 | 条数 | 指标 | 值 | 几次运行转写是否逐字一致 |")
    print("|---|---|---|---|---|---|")
    timing = {}
    for eng in sorted(runs):
        tsvs = [(k, load_tsv(p)) for k, p in sorted(runs[eng])]
        for sname in sorted(sets):
            items = sets[sname]["items"]
            metrics = [("CER", norm_m0)]
            if sname == "en":
                metrics.append(("WER", words_en))
            identical = all(
                all(t[k][1] == tsvs[0][1][k][1] for k in items) for _, t in tsvs
            )
            for mname, fn in metrics:
                vals = []
                for ridx, (k, t) in enumerate(tsvs):
                    tot_e = tot_r = 0
                    utt = []
                    for key, it in sorted(items.items()):
                        r, h = fn(it["ref"]), fn(t[key][1])
                        e = edit(list(r), list(h))
                        tot_e += e
                        tot_r += len(r)
                        utt.append((e, len(r)))
                    vals.append(100.0 * tot_e / tot_r)
                    if ridx == 0:
                        per_utt[(eng, sname, mname)] = utt
                cell = f"{median(vals):.2f}% [{min(vals):.2f}, {max(vals):.2f}]"
                print(f"| {eng} | {sname} | {len(items)} | {mname} | {cell} (n={len(vals)}) | "
                      f"{'是' if identical else '**否**'} |")
            # 产品路径的墙钟：每条都是一次独立进程（含模型加载），所以这是
            # 「用户按一次键要等多久」的口径，不是纯推理 RTF
            audio = sum(it["duration_s"] for it in items.values())
            rtfs = [sum(t[k][0] for k in items) / audio for _, t in tsvs]
            walls = [median(t[k][0] for k in items) for _, t in tsvs]
            timing[(eng, sname)] = (rtfs, walls)

    print("\n## 产品路径墙钟（每条一次独立进程，含模型加载；中位数 [最小, 最大]，跨运行）\n")
    print("| 引擎 | 组 | 总墙钟 / 总音频时长 | 单条墙钟中位数（秒） |")
    print("|---|---|---|---|")
    for (eng, sname), (rtfs, walls) in sorted(timing.items()):
        print(f"| {eng} | {sname} | {median(rtfs):.3f} [{min(rtfs):.3f}, {max(rtfs):.3f}] | "
              f"{median(walls):.2f} [{min(walls):.2f}, {max(walls):.2f}] |")

    print("\n## 配对 bootstrap（第 1 次运行，4000 次，种子 20260925）：CER(A) − CER(B) 的 95% 区间\n")
    print("| 组 | 指标 | A | B | 点估计 | 95% 区间 | 判读 |")
    print("|---|---|---|---|---|---|---|")
    engines = sorted(runs)
    for sname in sorted(sets):
        for mname in (["CER", "WER"] if sname == "en" else ["CER"]):
            for i in range(len(engines)):
                for j in range(i + 1, len(engines)):
                    a, b = per_utt[(engines[i], sname, mname)], per_utt[(engines[j], sname, mname)]
                    n = len(a)

                    def diff(idx):
                        ea = sum(a[x][0] for x in idx)
                        eb = sum(b[x][0] for x in idx)
                        rl = sum(a[x][1] for x in idx)
                        return 100.0 * (ea - eb) / rl

                    point = diff(range(n))
                    rng = random.Random(20260925)
                    bs = sorted(diff([rng.randrange(n) for _ in range(n)]) for _ in range(4000))
                    lo, hi = bs[int(0.025 * 4000)], bs[int(0.975 * 4000) - 1]
                    verdict = "未检出差异" if lo <= 0 <= hi else (
                        f"{engines[j]} 更好" if lo > 0 else f"{engines[i]} 更好")
                    print(f"| {sname} | {mname} | {engines[i]} | {engines[j]} | {point:+.2f} | "
                          f"[{lo:+.2f}, {hi:+.2f}] | {verdict} |")
    sensitivity(sets, runs)
    return 0


# FLEURS 中文参考里，外文名常以括号注出原文：「埃德·戴维 (Ed Davey)」。
# 朗读者通常不念，两个引擎也都不转，主口径里会给两边都记错。
# **只去掉括号内是纯拉丁文字的那种**：中文括号里的内容（「（甚至其他恒星也是）」）
# 是念出来的；英文组的括号（「(though not well)」）也是念出来的，所以英文组不动。
PAREN_LATIN = re.compile(r"\s*[（(][A-Za-z][A-Za-z .'\-]*[）)]")


def sensitivity(sets, runs):
    """敏感性分析：看结论是否被两处**参考文本的写法**左右。

    只用第 1 次运行（几次运行逐字一致时这没有损失，主表会报是否一致）。
    """
    variants = [
        ("主口径", lambda ref: ref, lambda ref: True),
        ("参考去掉括号里的拉丁原文注", lambda ref: PAREN_LATIN.sub("", ref), lambda ref: True),
        ("上一条 + 剔除含阿拉伯数字的句子", lambda ref: PAREN_LATIN.sub("", ref),
         lambda ref: not re.search(r"[0-9]", ref)),
    ]
    engines = sorted(runs)
    first = {e: load_tsv(sorted(runs[e])[0][1]) for e in engines}
    print("\n## 敏感性分析（第 1 次运行，M0 口径 CER；bootstrap 同上）\n")
    print("| 组 | 口径 | 条数 | " + " | ".join(engines) + " | 配对差值与 95% 区间（每一对） |")
    print("|---|---|---|" + "---|" * len(engines) + "---|")
    # 敏感性只对中文组做：英文组的括号是念出来的，混合组没有数字也没有括号
    for sname in [s for s in sorted(sets) if s == "zh"]:
        items = sets[sname]["items"]
        for vname, fix, keep in variants:
            keys = [k for k in sorted(items) if keep(items[k]["ref"])]
            utt = {}
            for e in engines:
                utt[e] = [(edit(list(norm_m0(fix(items[k]["ref"]))), list(norm_m0(first[e][k][1]))),
                           len(norm_m0(fix(items[k]["ref"])))) for k in keys]
            cells = [f"{100.0 * sum(x[0] for x in utt[e]) / sum(x[1] for x in utt[e]):.2f}%"
                     for e in engines]
            pairs = []
            n = len(keys)
            for i in range(len(engines)):
                for j in range(i + 1, len(engines)):
                    a, b = utt[engines[i]], utt[engines[j]]

                    def diff(idx):
                        return 100.0 * (sum(a[x][0] for x in idx) - sum(b[x][0] for x in idx)) \
                            / sum(a[x][1] for x in idx)

                    rng = random.Random(20260925)
                    bs = sorted(diff([rng.randrange(n) for _ in range(n)]) for _ in range(4000))
                    pairs.append(f"{engines[i]}−{engines[j]}：{diff(range(n)):+.2f} "
                                 f"[{bs[100]:+.2f}, {bs[3899]:+.2f}]")
            print(f"| {sname} | {vname} | {n} | " + " | ".join(cells) + " | " + "；".join(pairs) + " |")
        # 数字句对总字错的贡献（主口径）：说明中文组的平手里有多少来自数字写法
        digit_keys = {k for k in items if re.search(r"[0-9]", items[k]["ref"])}
        parts = []
        for e in engines:
            tot = dig = 0
            for k in sorted(items):
                ed = edit(list(norm_m0(items[k]["ref"])), list(norm_m0(first[e][k][1])))
                tot += ed
                dig += ed if k in digit_keys else 0
            parts.append(f"{e} {dig}/{tot}")
        print(f"\n{sname} 组含阿拉伯数字的句子 {len(digit_keys)} 条；"
              f"它们贡献的字错 / 总字错（主口径）：" + "，".join(parts))


if __name__ == "__main__":
    sys.exit(main())
