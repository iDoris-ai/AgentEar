#!/usr/bin/env python3
"""从 `cer-thai.py` 存的逐样本结果算置信区间与配对比较。

ADR-0004 §4 的 CI 和「显著/不显著」由本脚本产出。**种子固定**，
同一批逐样本文件重跑必然得到同一组数字。

用法：
    python3 scripts/cer-stats.py <结果目录> [重采样次数，默认 4000]

## 方法

CER 是语料级 micro average：`sum(edits) / sum(reference_len)`。

**按句自助法（cluster bootstrap）**：以句子为单位有放回重采样，
每次在样本内重算这个比值。这保留了句内字符错误的相关性，
比把每个字符当独立观测更合适。

**推断单位是句子**，不是说话人。若要推广到「新说话人」，
应按说话人重采样——当前 FLEURS 子集没有说话人标注，做不到。

**配对比较**用同一组重采样索引同时算两个模型，这样抵掉了句子难度带来的
共同波动。

## 读数字时的两条纪律

1. **CI 跨 0 只说明这批数据没检出差异，不等于「无差异」、更不等于「等效」。**
   要声称等效，得先预定义非劣界（比如「q5 相对 q8 的 CER 恶化不超过 0.5 个
   百分点」），再看 CI 上界是否落在界内。本脚本会报 CI 上界，但**不替你定界**。
2. **`COMPARISONS` 不是预注册的。** 它是在第一批结果出来之后才固定进代码的，
   作用只是让后续复算口径不漂移。所以这些比较是**探索性的**，
   存在多重比较问题，且未做校正。看到「检出差异」时要记着这一点。
"""
import json
import os
import random
import sys


def cer(per, keys, kind):
    e = sum(per[k][f"{kind}_edits"] for k in keys)
    n = sum(per[k][f"{kind}_len"] for k in keys)
    return e / n if n else 0.0


def boot_ci(per, names, kind, n_boot, seed):
    rnd = random.Random(seed)
    N = len(names)
    vals = []
    for _ in range(n_boot):
        idx = [names[rnd.randrange(N)] for _ in range(N)]
        vals.append(cer(per, idx, kind))
    vals.sort()
    # 百分位取离散下标（非插值）。与 NumPy 默认的线性插值会有微小差异。
    return vals[int(0.025 * n_boot)], vals[int(0.975 * n_boot)]


def boot_paired(a, b, names, kind, n_boot, seed):
    """同一组索引同时算两个模型，返回差值 (a-b) 的 95% CI。"""
    rnd = random.Random(seed)
    N = len(names)
    diffs = []
    for _ in range(n_boot):
        idx = [names[rnd.randrange(N)] for _ in range(N)]
        diffs.append(cer(a, idx, kind) - cer(b, idx, kind))
    diffs.sort()
    return diffs[int(0.025 * n_boot)], diffs[int(0.975 * n_boot)]


# ⚠️ **这不是预注册的比较集合。** 它是在结果已经产出之后固定下来的
# （commit c5f1e55），作用是让后续复算口径不漂移，**不能宣称「事前指定」**。
# 真要预注册，只能在下一轮新语料评测开始前先把方案定下来。
COMPARISONS = [
    ("ggml-medium-q8_0", "ggml-medium-q5_0"),
    ("ggml-distill-q8_0", "ggml-distill-q5_0"),
    ("ggml-turbo-q8_0", "ggml-turbo-q5_0"),
    ("ggml-medium-q8_0", "ggml-distill-q5_0"),
    ("ggml-distill-q5_0", "ggml-turbo-q5_0"),
    ("ggml-medium-q8_0", "ggml-turbo-q5_0"),
]
SEED_CI, SEED_PAIRED, KIND = 20260822, 20260823, "cp"


def main() -> int:
    if not 2 <= len(sys.argv) <= 3:
        print(__doc__)
        return 2
    d = sys.argv[1]
    n_boot = int(sys.argv[2]) if len(sys.argv) == 3 else 4000

    res = {}
    for f in sorted(os.listdir(d)):
        if f.endswith(".json"):
            res[f[:-5]] = json.load(open(os.path.join(d, f)))
    if not res:
        print(f"{d} 里没有结果文件", file=sys.stderr)
        return 1

    if n_boot <= 0:
        print("重采样次数必须为正", file=sys.stderr)
        return 1
    baseline_tag = next(iter(res))
    baseline = res[baseline_tag]
    names = sorted(baseline["per_sample"])
    if not names:
        print("逐样本数据为空", file=sys.stderr)
        return 1
    for tag, r in res.items():
        if sorted(r["per_sample"]) != names:
            print(f"!! {tag} 的样本集与其他不一致，配对比较无效", file=sys.stderr)
            return 1
        # 参考长度必须一致，否则是拿不同 normalization/reference 的结果在「配对」
        for k in names:
            if r["per_sample"][k]["cp_len"] != baseline["per_sample"][k]["cp_len"]:
                print(f"!! {tag} 的参考长度与 {baseline_tag} 不一致（{k}），"
                      f"归一化口径可能不同，配对无效", file=sys.stderr)
                return 1

    print(f"n={len(names)} 句，自助法 {n_boot} 次，粒度 {KIND}，"
          f"种子 CI={SEED_CI}/配对={SEED_PAIRED}\n")
    print(f"{'模型':24s} {'CER':>8s}  {'95% CI':>20s}  {'空输出':>6s}  {'sha256':>12s}")
    for tag in sorted(res, key=lambda t: cer(res[t]["per_sample"], names, KIND)):
        per = res[tag]["per_sample"]
        pt = cer(per, names, KIND)
        lo, hi = boot_ci(per, names, KIND, n_boot, SEED_CI)
        print(f"{tag:24s} {pt:8.4f}  [{lo:.4f}, {hi:.4f}]  "
              f"{res[tag]['empty_output']:>4d}/{len(names)}  {res[tag]['model_sha256_12']}")

    print("\n配对比较（事后固定的探索性比较组，未做多重校正；Δ = 左 − 右）：")
    for a, b in COMPARISONS:
        if a not in res or b not in res:
            print(f"  {a} / {b}：缺结果，跳过")
            continue
        lo, hi = boot_paired(res[a]["per_sample"], res[b]["per_sample"],
                             names, KIND, n_boot, SEED_PAIRED)
        verdict = "检出差异" if (lo > 0 or hi < 0) else "未检出差异"
        print(f"  {a:22s} − {b:22s}  Δ95%CI=[{lo:+.4f}, {hi:+.4f}]  {verdict}")

    print("\n注：「未检出差异」≠「无差异」≠「等效」。要声称等效需先定非劣界，"
          "再看 CI 上界是否落在界内。")
    return 0


if __name__ == "__main__":
    sys.exit(main())
