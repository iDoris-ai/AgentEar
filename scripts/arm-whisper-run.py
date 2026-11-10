#!/usr/bin/env python3
"""T3.5.2：在 Arm 泰语 code-switch 语料（22 条）上用「配置 D」跑 whisper，
输出与 qwen3-asr-2026-09-08.tsv 同格式的逐条转写 TSV。

配置 D（RESULTS.md 第二节）：whisper-cli 默认 beam search（**不传** -bs/-bo，
即 beam 5 / best-of 5）+ `--prompt` = terms.json 里全部纯拉丁词（逗号分隔）。
其余参数同生产：-l th -t 4 -np -nt。

用法（用 uv/venv 里确定版本的解释器，不要裸调 python3）：
    python -I scripts/arm-whisper-run.py <whisper-cli> <ggml模型> <wav根目录> <terms.json> <输出.tsv>

wav 根目录里要有 arm_thai_<category>/<take>.wav，先跑
`shasum -a 256 -c thai-corpus.sha256` 确认是这批。
本脚本会自己再核对一遍（对着 thai-corpus.sha256），不符就退出，不静默继续。
任何 whisper 非零退出 / 超时 / 空输出都当场失败，不记成「空转写」。
"""
import csv
import hashlib
import os
import re
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
CORPUS = os.path.join(HERE, "..", "docs", "data", "thai-corpus-arm-2026-09")


def latin_prompt(terms_path):
    import json
    t = json.load(open(terms_path, encoding="utf-8"))
    out = []
    for x in t["terms"]:
        for w in [x["canonical"]] + x.get("aliases", []):
            # 与 src/engine.rs::is_pure_latin 同义：只含拉丁字母/数字/少数标点
            if re.search(r"[A-Za-z]", w) and re.fullmatch(r"[A-Za-z0-9 .+#\-]+", w) and w not in out:
                out.append(w)
    return ", ".join(out), len(out)


def sha256(p):
    h = hashlib.sha256()
    with open(p, "rb") as f:
        for b in iter(lambda: f.read(1 << 20), b""):
            h.update(b)
    return h.hexdigest()


def main():
    if len(sys.argv) != 6:
        print(__doc__)
        return 2
    cli, model, wavroot, terms, out = sys.argv[1:]
    # 核对音频
    for line in open(os.path.join(CORPUS, "thai-corpus.sha256"), encoding="utf-8"):
        if line.startswith("#") or not line.strip():
            continue
        digest, rel = line.split(None, 1)
        rel = rel.strip()
        if "impromptu" in rel:
            continue
        p = os.path.join(wavroot, rel)
        if not os.path.exists(p) or sha256(p) != digest:
            raise SystemExit(f"!! 音频与 thai-corpus.sha256 不符：{rel}")
    prompt, n = latin_prompt(terms)
    print(f"prompt: {n} 条纯拉丁词", file=sys.stderr)
    rows = list(csv.DictReader(open(os.path.join(CORPUS, "reference.tsv"), encoding="utf-8"), delimiter="\t"))
    ref_by = {(r["category"], r["take"]): r["text"] for r in rows}
    order = list(csv.DictReader(open(os.path.join(CORPUS, "qwen3-asr-2026-09-08.tsv"), encoding="utf-8"), delimiter="\t"))
    with open(out, "w", encoding="utf-8", newline="") as f:
        f.write("category\ttake\treference\thypothesis\n")
        for r in order:
            cat, take = r["category"], r["take"]
            ref = ref_by[(cat, take)]
            if ref != r["reference"]:
                raise SystemExit(f"!! 参考文本与 qwen3 tsv 不一致：{cat}/{take}")
            wav = os.path.join(wavroot, f"arm_thai_{cat}", f"{take}.wav")
            p = subprocess.run(
                [cli, "-m", model, "-f", wav, "-l", "th", "-t", "4", "-np", "-nt", "--prompt", prompt],
                capture_output=True, text=True, timeout=300)
            if p.returncode != 0:
                raise SystemExit(f"!! whisper-cli 退出码 {p.returncode}：{wav}\n{p.stderr[-1500:]}")
            hyp = " ".join(p.stdout.split())
            if not hyp:
                raise SystemExit(f"!! 空输出：{wav}")
            f.write(f"{cat}\t{take}\t{ref}\t{hyp}\n")
            print(cat, take, hyp, file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
