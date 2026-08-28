#!/usr/bin/env python3
"""在 FLEURS 泰语 test 子集上跑推理并算 CER，输出**逐样本**结果。

只负责「跑一个模型、存下逐句 edits 与长度」。
置信区间和配对比较在 `cer-stats.py`，输入就是这里存的逐样本文件——
这样统计部分可以随时重算，不用重跑推理。

用法：
    python3 scripts/cer-thai.py <ggml模型> <whisper-cli> <数据目录> [输出目录]

数据目录由 `fleurs-thai-fetch.py` 生成（含 wav/ 与 refs.json）。

## 归一化口径

NFC → 小写 → 去全部空白 → 去 Unicode 标点（category `P*`）。

去空白：泰语词间不用空格，FLEURS 的空格是短语边界，各模型加不加、加在哪
都不同，计进去测的是排版不是识别。
**注意这条口径将来测 code-switch 时会掩盖英文词边界错误**，那时要另立口径。

`ฯ`（泰语缩略符，Unicode 类别 Lo 而非标点）**保留**，因为它是正字法的一部分，
不是排版符号。这是一个明确选择，不是遗漏。

## 两种切分粒度

- `cp`  —— 按 Unicode code point
- `bcm` —— base + combining marks：把泰文声调符/元音符并入前一个字符

⚠️ **`bcm` 不是 Unicode 字素簇，也不是泰语正字法音节。** 泰语的前置元音
（`เ`、`แ`…）和 Sara Am（`ำ`）它都处理不了：`เก่ง` → `['เ','ก่','ง']`，
`กำ` → `['ก','ำ']`。它只是一个「对组合符号不那么敏感」的辅助口径，
两个数字有差异只说明**结果对切分口径敏感**，不能据此断言错误集中在哪里
——要定位得看逐句对齐。
"""
import json
import os
import subprocess
import sys
import time
import unicodedata

# 泰文组合符号：U+0E31、U+0E34–U+0E3A、U+0E47–U+0E4E
THAI_COMBINING = frozenset(
    [0x0E31] + list(range(0x0E34, 0x0E3B)) + list(range(0x0E47, 0x0E4F))
)
# 保留的非标点符号：泰语缩略符是正字法的一部分
KEEP = frozenset("ฯ")


def norm(s: str) -> str:
    s = unicodedata.normalize("NFC", s).lower()
    return "".join(
        c for c in s
        if not c.isspace()
        and (c in KEEP or not unicodedata.category(c).startswith("P"))
    )


def base_plus_combining(s: str) -> list:
    """把泰文组合符号并入前一个字符。见模块文档里的局限声明。"""
    out: list = []
    for c in s:
        if out and ord(c) in THAI_COMBINING:
            out[-1] += c
        else:
            out.append(c)
    return out


def edit(a, b) -> int:
    """Levenshtein。a/b 可以是 str 也可以是 list。"""
    if len(a) < len(b):
        a, b = b, a
    prev = list(range(len(b) + 1))
    for i, ca in enumerate(a, 1):
        cur = [i]
        for j, cb in enumerate(b, 1):
            cur.append(min(prev[j] + 1, cur[j - 1] + 1, prev[j - 1] + (ca != cb)))
        prev = cur
    return prev[-1]


def transcribe(cli: str, model: str, wav: str) -> str:
    """跑一条。**任何非正常退出都当场失败，不返回空串。**

    静默把失败当成「模型输出为空」，会让加载失败、文件损坏、崩溃、超时
    统统被计成 100% 的删除错误——基础设施故障伪装成准确率差，
    这是实验正确性问题，不是健壮性细节。
    """
    try:
        p = subprocess.run(
            [cli, "-m", model, "-f", wav, "-l", "th", "-t", "4",
             "-bo", "1", "-bs", "1", "-np", "-nt"],
            capture_output=True, text=True, timeout=300,
        )
    except subprocess.TimeoutExpired as e:
        err = (e.stderr or b"").decode(errors="replace") if isinstance(e.stderr, bytes) else (e.stderr or "")
        raise SystemExit(f"!! 超时（300s）：{wav}\n{err[-2000:]}")
    except OSError as e:
        raise SystemExit(f"!! 无法执行 {cli}：{e}")
    if p.returncode != 0:
        raise SystemExit(
            f"!! whisper-cli 退出码 {p.returncode}：{wav}\n{p.stderr[-2000:]}"
        )
    return p.stdout.strip()


def main() -> int:
    if not 4 <= len(sys.argv) <= 5:
        print(__doc__)
        return 2
    model, cli, data = sys.argv[1], sys.argv[2], sys.argv[3]
    outdir = sys.argv[4] if len(sys.argv) == 5 else os.path.join(data, "results")
    for p in (model, cli, os.path.join(data, "refs.json")):
        if not os.path.exists(p):
            print(f"找不到 {p}", file=sys.stderr)
            return 1
    os.makedirs(outdir, exist_ok=True)

    refs = json.load(open(data + "/refs.json"))
    tag = os.path.basename(model).replace(".bin", "")

    # 先冒烟一条，模型坏掉要立刻知道，而不是跑完 80 条才发现全是空
    first = sorted(refs)[0]
    transcribe(cli, model, f"{data}/wav/{first}.wav")

    # 归一化后参考文本的指纹。cer-stats.py 拿它判断两份结果能不能配对——
    # **只比长度不够**：两段不同的文本完全可能等长，那样就会拿不同参考的
    # 结果做「配对」而检查不出来。
    ref_fp = _refs_fingerprint(refs)
    # **音频指纹另立一格，不并进 ref_fp。** 并进去会让已产出的六份结果文件
    # 全部失配（它们的 ref_fp 是按旧口径算的）。
    # 少了这一格，「上游换了音频、文字没动」这种情况配对检查完全看不出来：
    # 两份结果参考文本一致、长度一致，却是在不同录音上跑出来的。
    audio_fp = _audio_fingerprint(refs)
    if audio_fp is None:
        print("   注：refs.json 缺 audio_sha256_16，本次结果不带音频指纹"
              "（cer-stats.py 将无法校验两份结果是否用了同一批录音）", file=sys.stderr)

    per, empty, t0 = {}, 0, time.time()
    for name in sorted(refs):
        hyp = transcribe(cli, model, f"{data}/wav/{name}.wav")
        if not hyp:
            empty += 1  # 进程成功退出且 stdout 确实为空，才算「空输出」
        r, h = norm(refs[name]["transcription"]), norm(hyp)
        rb, hb = base_plus_combining(r), base_plus_combining(h)
        per[name] = {
            "hyp": hyp,
            "cp_edits": edit(r, h), "cp_len": len(r),
            "bcm_edits": edit(rb, hb), "bcm_len": len(rb),
        }

    res = {
        "model": os.path.basename(model),  # 不写绝对路径，那是本机私有信息
        "model_sha256_12": _sha12(model),
        "cli_sha256_12": _sha12(cli),
        "norm": "NFC|lower|strip-space|strip-unicode-P|keep-ฯ",
        "refs_norm_sha256_16": ref_fp,
        "refs_audio_sha256_16": audio_fp,
        "n": len(per),
        "empty_output": empty,
        "elapsed_s": round(time.time() - t0, 1),
        "per_sample": per,
    }
    path = os.path.join(outdir, f"{tag}.json")
    json.dump(res, open(path, "w"), ensure_ascii=False, indent=1, sort_keys=True)

    cp = sum(v["cp_edits"] for v in per.values()) / sum(v["cp_len"] for v in per.values())
    bcm = sum(v["bcm_edits"] for v in per.values()) / sum(v["bcm_len"] for v in per.values())
    print(f"{tag:24s} CER_cp={cp:.4f}  CER_bcm={bcm:.4f}  "
          f"空输出={empty}/{len(per)}  耗时={res['elapsed_s']:.0f}s  → {path}")
    return 0


def _audio_fingerprint(refs: dict):
    """取到的是不是同一批**录音**的指纹。缺 audio_sha256_16 时返回 None。

    参考文本指纹只覆盖「念的是什么」，覆盖不到「谁在念、哪一段录音」。
    """
    import hashlib
    if not all(isinstance(v, dict) and v.get("audio_sha256_16") for v in refs.values()):
        return None
    blob = json.dumps([[k, refs[k]["audio_sha256_16"]] for k in sorted(refs)],
                      ensure_ascii=False).encode()
    return hashlib.sha256(blob).hexdigest()[:16]


def _refs_fingerprint(refs: dict) -> str:
    """归一化后参考序列的指纹（含样本名，所以顺序也在内）。"""
    import hashlib
    blob = json.dumps([[k, norm(refs[k]["transcription"])] for k in sorted(refs)],
                      ensure_ascii=False).encode()
    return hashlib.sha256(blob).hexdigest()[:16]


def _sha12(p: str) -> str:
    import hashlib
    h = hashlib.sha256()
    with open(p, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()[:12]


if __name__ == "__main__":
    sys.exit(main())
