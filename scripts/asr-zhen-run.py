#!/usr/bin/env python3
"""T3.5.1：用**产品自己的转写入口**（`agentear --transcribe`）跑一遍评测子集。

走产品入口而不是直接调两个 CLI，是为了测到**用户实际拿到的东西**：
SenseVoice 的 `<|zh|>` 标记过滤、speech_swift 的 `Result:` 解析、
以及 speech_swift 默认带上的术语 `--context`（从数据目录的 terms.json 生成，
空数据目录下就是 `terms::load` 写出的**默认术语表**）都包含在内。

用法：
    python3 scripts/asr-zhen-run.py <agentear 二进制> <语料目录> <builtin|speech_swift> <输出 tsv> \
        [--qwen-model 0.6B]

- 语料目录由 `asr-zhen-fetch.py` 生成（wav/ + manifest.json）。
  推理前逐条重算 WAV 的 SHA-256 并与 manifest 对账，对不上就拒跑。
- 需要环境变量 `AGENTEAR_DATA`（建议指向一个空的临时目录，避免用户自己的
  terms.json 与 config.json 混进来）与 `AGENTEAR_VENDOR`。
- `--qwen-model`：speech_swift 用哪档，**默认 1.7B**（T3.5.1 当时的产品行为）。
  v0.23 起模型档位来自 `config.json` 的 `qwen3_model`（产品默认 0.6B），
  所以这里**把它写进 `$AGENTEAR_DATA/config.json`**，而不是像 T3.5.1 当时那样
  在 PATH 前面放一个替换 `-m 1.7B` 的垫片——垫片依赖「产品写死 1.7B」，
  那个前提已经不成立了（照旧跑会在新数据目录里**悄悄变成 0.6B**）。
  0.6B 与 1.7B 仍走同一条产品路径、同一个 context，只差模型。
- ⚠️ 运行时：`$AGENTEAR_DATA` 里**没装** AgentEar 自己下载的 Qwen3 时，走 PATH 上的
  `speech`（T3.5.1 用的是 brew 的 0.0.26）；装了就走钉死的 v0.0.28。两者不是同一个版本。

输出 TSV 列：key、墙钟秒数、转写文本（制表符与换行替换成空格）。
"""
import hashlib
import json
import os
import subprocess
import sys
import time


def _sha16(path):
    with open(path, "rb") as f:
        return hashlib.sha256(f.read()).hexdigest()[:16]


def _set_qwen3_model(model):
    """把 speech_swift 的模型档位写进数据目录的 config.json（保留其余字段）。"""
    os.makedirs(os.environ["AGENTEAR_DATA"], exist_ok=True)
    cfg_path = os.path.join(os.environ["AGENTEAR_DATA"], "config.json")
    cfg = {}
    if os.path.exists(cfg_path):
        with open(cfg_path) as f:
            cfg = json.load(f)
    cfg["qwen3_model"] = {"0.6b": "0.6b", "1.7b": "1.7b"}[model.lower()]
    cfg["qwen3_resident"] = False  # T3.5.1 测的是逐次调用
    with open(cfg_path, "w") as f:
        json.dump(cfg, f, ensure_ascii=False, indent=2)
    with open(cfg_path) as f:  # 读回核对：写没写进去不靠猜
        assert json.load(f)["qwen3_model"] == cfg["qwen3_model"]


def main():
    a = sys.argv[1:]
    model = None
    if "--qwen-model" in a:
        i = a.index("--qwen-model")
        model = a[i + 1]
        del a[i:i + 2]
    if len(a) != 4:
        print(__doc__)
        return 2
    binary, corpus, backend, out_tsv = a
    if backend not in ("builtin", "speech_swift"):
        raise SystemExit("后端只认 builtin / speech_swift")
    if model and backend != "speech_swift":
        raise SystemExit("--qwen-model 只对 speech_swift 有意义")
    for v in ("AGENTEAR_DATA", "AGENTEAR_VENDOR"):
        if not os.environ.get(v):
            raise SystemExit(f"要设 {v}")

    manifest = json.load(open(os.path.join(corpus, "manifest.json")))
    jobs = []
    for set_name, s in sorted(manifest["sets"].items()):
        for key, it in sorted(s["items"].items()):
            wav = os.path.join(corpus, "wav", set_name, f"{key}.wav")
            if _sha16(wav) != it["wav_sha256_16"]:
                raise SystemExit(f"{wav} 与 manifest 的哈希对不上，拒跑")
            jobs.append((key, wav))

    env = dict(os.environ)
    if backend == "speech_swift":
        _set_qwen3_model(model or "1.7b")

    cmd_base = [binary]
    if backend == "speech_swift":
        cmd_base += ["--asr-backend", "speech_swift"]
    rows = []
    try:
        for n, (key, wav) in enumerate(jobs, 1):
            t0 = time.perf_counter()
            p = subprocess.run(cmd_base + ["--transcribe", wav, "--lang", "auto"],
                               capture_output=True, text=True, env=env)
            wall = time.perf_counter() - t0
            if p.returncode != 0:
                raise SystemExit(f"{key} 失败 exit {p.returncode}:\n{p.stderr[-2000:]}")
            # 转写正文走 stdout，日志与「（语种 …）」走 stderr
            hyp = " ".join(p.stdout.split("\n")).replace("\t", " ").strip()
            rows.append((key, wall, hyp))
            print(f"[{n}/{len(jobs)}] {key} {wall:.2f}s {hyp[:60]}", file=sys.stderr)
    finally:
        pass

    os.makedirs(os.path.dirname(os.path.abspath(out_tsv)), exist_ok=True)
    tmp = out_tsv + ".part"
    with open(tmp, "w") as f:
        for key, wall, hyp in rows:
            f.write(f"{key}\t{wall:.3f}\t{hyp}\n")
    os.replace(tmp, out_tsv)
    # 读回核对，不信「写了」
    with open(out_tsv) as f:
        got = sum(1 for _ in f)
    if got != len(jobs):
        raise SystemExit(f"写出 {got} 行，应为 {len(jobs)}")
    print(f"✓ {out_tsv}：{got} 行", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
