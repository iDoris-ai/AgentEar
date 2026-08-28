#!/usr/bin/env python3
"""从 `cer-thai.py` 存的逐样本结果算置信区间与配对比较。

ADR-0004 §4 的 CI 和「检出/未检出差异」由本脚本产出。**种子固定**，
同一批逐样本文件重跑必然得到同一组数字。

用法：
    python3 scripts/cer-stats.py <结果目录> [重采样次数，默认 4000] [粒度 cp|bcm，默认 cp]

退出码：0 正常；1 输入有问题（样本集不一致、指纹不符、归一化口径不同）；
3 跑完了但有配对比较因缺结果没做——**输出不完整，别当成功用**。

## 方法

CER 是语料级 micro average：`sum(edits) / sum(reference_len)`。

**按录音自助法（cluster bootstrap）**：以录音为单位有放回重采样，
每次在样本内重算这个比值。这保留了句内字符错误的相关性，
比把每个字符当独立观测更合适。

**推断单位是录音（utterance）**，不是说话人、也不是「独立句子」——
评测集里有 4 条与其他条目共用 prompt。若要推广到「新说话人」应按说话人
重采样（FLEURS 子集没有说话人标注，做不到）；若要推广到「新文本内容」，
应按 `fleurs_id` 聚类重采样。

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
    # 百分位取离散下标（非插值），与 NumPy 默认的线性插值会有微小差异。
    # 上下两端取对称下标：int(0.975*n) 会比下端多留一个，差约 1e-4。
    lo = int(0.025 * n_boot)
    return vals[lo], vals[n_boot - 1 - lo]


def boot_paired(a, b, names, kind, n_boot, seed):
    """同一组索引同时算两个模型，返回差值 (a-b) 的 95% CI。"""
    rnd = random.Random(seed)
    N = len(names)
    diffs = []
    for _ in range(n_boot):
        idx = [names[rnd.randrange(N)] for _ in range(N)]
        diffs.append(cer(a, idx, kind) - cer(b, idx, kind))
    diffs.sort()
    lo = int(0.025 * n_boot)          # 与 boot_ci 同样取对称下标
    return diffs[lo], diffs[n_boot - 1 - lo]


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
SEED_CI, SEED_PAIRED = 20260822, 20260823
DEFAULT_KIND = "cp"


def main() -> int:
    if not 2 <= len(sys.argv) <= 4:
        print(__doc__)
        return 2
    d = sys.argv[1]
    n_boot = int(sys.argv[2]) if len(sys.argv) >= 3 else 4000
    # 粒度以前是写死的 "cp"，而逐样本文件里两种都存着 —— 想看 bcm 得改源码。
    # 它会影响所有结论，所以做成显式参数并打进表头。
    kind = sys.argv[3] if len(sys.argv) == 4 else DEFAULT_KIND
    if kind not in ("cp", "bcm"):
        print(f"粒度只能是 cp 或 bcm，当前是 '{kind}'", file=sys.stderr)
        return 1

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
    # **普查要把 baseline 自己算进去。** 之前拿 next(iter(res)) 当基线又把它
    # 排除在普查外，于是「6 份全缺」和「只有基线缺」打出一字不差的同一行；
    # 后一种情况点名的还恰恰是**有**指纹的那 5 份，把人指向健康的产物。
    no_text_fp = [t for t in sorted(res) if not res[t].get("refs_norm_sha256_16")]
    no_audio_fp = [t for t in sorted(res) if not res[t].get("refs_audio_sha256_16")]
    # 基线自己缺指纹的话，下面 `fa and fb` 恒假，其余结果彼此参考文本不同也
    # 查不出来 —— 正是那道守卫声称挡住的失败。所以优先挑一个有指纹的当基线。
    with_fp = [t for t in sorted(res) if res[t].get("refs_norm_sha256_16")]
    baseline_tag = with_fp[0] if with_fp else sorted(res)[0]
    baseline = res[baseline_tag]
    names = sorted(baseline["per_sample"])
    if not names:
        print("逐样本数据为空", file=sys.stderr)
        return 1
    for tag, r in res.items():
        if sorted(r["per_sample"]) != names:
            print(f"!! {tag} 的样本集与其他不一致，配对比较无效", file=sys.stderr)
            return 1
        # 配对的前提是两份结果用的是**同一批参考文本、同一套归一化**。
        # 首选比对参考指纹；只比长度是不够的——两段不同的文本可以等长。
        fa, fb = r.get("refs_norm_sha256_16"), baseline.get("refs_norm_sha256_16")
        if fa and fb:
            if fa != fb:
                print(f"!! {tag} 的参考指纹与 {baseline_tag} 不一致"
                      f"（{fa} vs {fb}），配对无效", file=sys.stderr)
                return 1
        # 缺字段的情况在上面已统一普查过，这里不再逐个收集
        # 参考文本一致不等于录音一致：上游换了音频、文字没动，上面那道全过。
        aa, ab = r.get("refs_audio_sha256_16"), baseline.get("refs_audio_sha256_16")
        if aa and ab:
            if aa != ab:
                print(f"!! {tag} 的**音频**指纹与 {baseline_tag} 不一致"
                      f"（{aa} vs {ab}）——参考文本相同但录音不同，配对无效",
                      file=sys.stderr)
                return 1

        if r.get("norm") != baseline.get("norm"):
            print(f"!! {tag} 的归一化口径与 {baseline_tag} 不同"
                  f"（{r.get('norm')} vs {baseline.get('norm')}）", file=sys.stderr)
            return 1
        for k in names:
            for fld in ("cp_len", "bcm_len"):
                if r["per_sample"][k][fld] != baseline["per_sample"][k][fld]:
                    print(f"!! {tag} 的 {fld} 与 {baseline_tag} 不一致（{k}）",
                          file=sys.stderr)
                    return 1

    # **写 stdout，不写 stderr。** thai-cer-stats.txt 是 stdout 重定向来的，
    # 写 stderr 的话这两行永远进不了产物 —— 同一个坑 bench-thai.sh 的注释里
    # 已经写死过一次，「缺结果」那条也已经改成 stdout + 退出码 3。
    if no_text_fp:
        print(f"!! {len(no_text_fp)}/{len(res)} 份结果缺 refs_norm_sha256_16，"
              f"对它们只能做长度校验（挡不住等长的不同参考）：{', '.join(no_text_fp)}")
    if no_audio_fp:
        print(f"!! {len(no_audio_fp)}/{len(res)} 份结果缺 refs_audio_sha256_16，"
              f"无法校验是否跑在同一批录音上：{', '.join(no_audio_fp)}")
    if no_text_fp:
        print(f"!! 基线取的是 {baseline_tag}"
              f"{'（它自己也没有指纹，参考一致性实际上未被校验）' if baseline_tag in no_text_fp else ''}")
    print()

    print(f"n={len(names)} 条录音，自助法 {n_boot} 次，粒度 {kind}，"
          f"种子 CI={SEED_CI}/配对={SEED_PAIRED}\n")
    print(f"{'模型':24s} {'CER':>8s}  {'95% CI':>20s}  {'空输出':>6s}  {'sha256':>12s}")
    for tag in sorted(res, key=lambda t: cer(res[t]["per_sample"], names, kind)):
        per = res[tag]["per_sample"]
        pt = cer(per, names, kind)
        lo, hi = boot_ci(per, names, kind, n_boot, SEED_CI)
        print(f"{tag:24s} {pt:8.4f}  [{lo:.4f}, {hi:.4f}]  "
              f"{res[tag]['empty_output']:>4d}/{len(names)}  {res[tag]['model_sha256_12']}")

    print("\n配对比较（事后固定的探索性比较组，未做多重校正；Δ = 左 − 右）：")
    missing = 0
    for a, b in COMPARISONS:
        if a not in res or b not in res:
            # 以前这里只打一行就 continue，最后照样 return 0 —— 把输出重定向进
            # 文件时，缺掉的比较和「本来就没这一组」长得一模一样。收尾要非零退出。
            print(f"  !! {a} / {b}：缺结果，未做这组比较")
            missing += 1
            continue
        lo, hi = boot_paired(res[a]["per_sample"], res[b]["per_sample"],
                             names, kind, n_boot, SEED_PAIRED)
        verdict = "检出差异" if (lo > 0 or hi < 0) else "未检出差异"
        print(f"  {a:22s} − {b:22s}  Δ95%CI=[{lo:+.4f}, {hi:+.4f}]  {verdict}")

    print("\n注：「未检出差异」≠「无差异」≠「等效」。要声称等效需先定非劣界，"
          "再看 CI 上界是否落在界内。")
    if missing:
        print(f"!! {missing} 组配对比较因缺结果没做，上面这份输出不完整",
              file=sys.stderr)
        return 3
    return 0


if __name__ == "__main__":
    sys.exit(main())
