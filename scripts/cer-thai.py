#!/usr/bin/env python3
"""在 FLEURS 泰语 test 子集上跑推理并算 CER，输出**逐样本**结果。

只负责「跑一个模型、存下逐句 edits 与长度」。
置信区间和配对比较在 `cer-stats.py`，输入就是这里存的逐样本文件——
这样统计部分可以随时重算，不用重跑推理。

用法：
    python3 scripts/cer-thai.py [--allow-unverified-audio] <ggml模型> <whisper-cli> <数据目录> [输出目录]

数据目录由 `fleurs-thai-fetch.py` 生成（含 wav/ 与 refs.json）。

推理前会**逐条重算磁盘上 wav 的 SHA-256** 并与 refs.json 记录的比对，
比对结果聚合成 `refs_audio_sha256_16` 存进结果文件。只哈希 refs.json 里的
元数据是不够的 —— 换掉 `wav/f000.wav` 而不动 refs.json，两次跑出来的指纹
一模一样，`cer-stats.py` 会当成同一批录音接受配对。

refs.json 缺 `wav_sha256_16`（旧版 fetch 生成的）时**默认拒绝运行**，
要跑得显式加 `--allow-unverified-audio`，不静默降级。

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
    argv = [a for a in sys.argv[1:] if a != "--allow-unverified-audio"]
    allow_unverified = len(argv) != len(sys.argv) - 1
    if not 3 <= len(argv) <= 4:
        print(__doc__)
        return 2
    model, cli, data = argv[0], argv[1], argv[2]
    outdir = argv[3] if len(argv) == 4 else os.path.join(data, "results")
    for p in (model, cli, os.path.join(data, "refs.json")):
        if not os.path.exists(p):
            print(f"找不到 {p}", file=sys.stderr)
            return 1
    os.makedirs(outdir, exist_ok=True)

    refs = json.load(open(data + "/refs.json"))
    base = os.path.basename(model)
    # 去后缀，不是全局替换：`a.bin.quant.bin` 这种名字会被 replace 改成 `a.quant`
    tag = base[:-4] if base.endswith(".bin") else base

    # 先冒烟一条，模型坏掉要立刻知道，而不是跑完 80 条才发现全是空。
    # **只判退出码不够**：「能加载但吐空串」是另一种坏法，那样会安安静静
    # 跑满 80 条，产出一组 100% 删除错误的假 CER。
    first = sorted(refs)[0]
    if not transcribe(cli, model, f"{data}/wav/{first}.wav"):
        raise SystemExit(f"!! 冒烟失败：{first} 的转写为空。模型能加载但不产出文本，"
                         f"继续跑只会得到一组 100% 删除错误的假数字。")

    # 归一化后参考文本的指纹。cer-stats.py 拿它判断两份结果能不能配对——
    # **只比长度不够**：两段不同的文本完全可能等长，那样就会拿不同参考的
    # 结果做「配对」而检查不出来。
    ref_fp = _refs_fingerprint(refs)

    # **音频指纹要哈希真正送进 CLI 的那个文件，不是 refs.json 里的元数据。**
    # 只哈希元数据的话，把 wav/f000.wav 换掉而不动 refs.json，两次跑出来的
    # 指纹完全相同，cer-stats.py 会当成同一批录音接受配对。
    actual = {}
    for name in sorted(refs):
        p = os.path.join(data, "wav", f"{name}.wav")
        if not os.path.exists(p):
            print(f"!! 找不到 {p}", file=sys.stderr)
            return 1
        actual[name] = _file_sha16(p)
    stale = [n for n in sorted(refs)
             if refs[n].get("wav_sha256_16") and refs[n]["wav_sha256_16"] != actual[n]]
    if stale:
        print(f"!! {len(stale)} 条 wav 与 refs.json 记录的哈希不符"
              f"（{', '.join(stale[:5])}{'…' if len(stale) > 5 else ''}）"
              f"——磁盘上的音频被换过，数字不可比", file=sys.stderr)
        return 1
    unrecorded = [n for n in sorted(refs) if not refs[n].get("wav_sha256_16")]
    if unrecorded and not allow_unverified:
        print(f"!! refs.json 里有 {len(unrecorded)}/{len(refs)} 条没有 wav_sha256_16，"
              f"无法核对磁盘上的音频。\n"
              f"   用当前版本的 fleurs-thai-fetch.py 重新生成数据目录，"
              f"或显式加 --allow-unverified-audio。", file=sys.stderr)
        return 1
    if unrecorded:
        print(f"   注：{len(unrecorded)} 条未与 refs.json 对账（--allow-unverified-audio）",
              file=sys.stderr)
    # 指纹用**实测的**哈希，不是记录值
    audio_fp = _audio_fingerprint(actual)

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
        "audio_verified": not unrecorded,   # 是否逐条与 refs.json 对过账
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


def _audio_fingerprint(hashes: dict) -> str:
    """这批**实际音频文件**的指纹（名字 + 内容哈希，顺序也在内）。

    参考文本指纹只覆盖「念的是什么」，覆盖不到「哪一份录音」。
    """
    import hashlib
    blob = json.dumps([[k, hashes[k]] for k in sorted(hashes)],
                      ensure_ascii=False).encode()
    return hashlib.sha256(blob).hexdigest()[:16]


def _file_sha16(path: str) -> str:
    import hashlib
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()[:16]


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
