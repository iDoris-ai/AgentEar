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
- `--qwen-model`：产品里写死 `-m 1.7B`。要测 0.6B，这里在 PATH 最前面放一个
  `speech` 垫片，只把 `-m 1.7B` 换成指定值、其余参数原样转发——
  所以 0.6B 与 1.7B 走的是**同一条产品路径、同一个 context**，只差模型。
  垫片会把每次实际收到的参数追加进 `<输出 tsv>.argv`，用来核对替换真的发生了。

输出 TSV 列：key、墙钟秒数、转写文本（制表符与换行替换成空格）。
"""
import hashlib
import json
import os
import shutil
import stat
import subprocess
import sys
import tempfile
import time


def _sha16(path):
    with open(path, "rb") as f:
        return hashlib.sha256(f.read()).hexdigest()[:16]


def _make_shim(model, argv_log):
    real = shutil.which("speech")
    if not real:
        raise SystemExit("PATH 里找不到 speech")
    d = tempfile.mkdtemp(prefix="speech-shim-")
    shim = os.path.join(d, "speech")
    with open(shim, "w") as f:
        f.write(f"""#!/bin/bash
args=()
prev=""
for a in "$@"; do
  if [ "$prev" = "-m" ] && [ "$a" = "1.7B" ]; then a="{model}"; fi
  args+=("$a"); prev="$a"
done
printf '%s\\n' "${{args[*]}}" >> "{argv_log}"
exec "{real}" "${{args[@]}}"
""")
    os.chmod(shim, os.stat(shim).st_mode | stat.S_IEXEC)
    return d


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
    shim_dir = None
    if model:
        argv_log = out_tsv + ".argv"
        if os.path.exists(argv_log):
            os.remove(argv_log)
        shim_dir = _make_shim(model, os.path.abspath(argv_log))
        env["PATH"] = shim_dir + os.pathsep + env["PATH"]

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
        if shim_dir:
            shutil.rmtree(shim_dir, ignore_errors=True)

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
