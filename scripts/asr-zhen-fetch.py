#!/usr/bin/env python3
"""取 T3.5.1 中文 / 英文 / 中英混合 ASR 横比的固定评测子集。

三组，**采样都是确定性的**（等距下标 floor(i*N/n)，无随机数，覆盖全区间）：

| 组 | 数据源 | 许可 | 过滤 |
|---|---|---|---|
| `zh`    | `google/fleurs` `cmn_hans_cn` test | CC-BY-4.0 | 无 |
| `en`    | `google/fleurs` `en_us` test       | CC-BY-4.0 | 无 |
| `mixed` | `CAiRE/ASCEND` `main` test          | CC-BY-SA-4.0 | `language == "mixed"` 且时长 ≥ 2.0s |

ASCEND 过滤掉 2 秒以下的片段：它的混合语句里有大量「ok福建」「哦这么technical」
一类两三个 token 的碎片，单条错一个字 CER 就是 30–50%，噪声会盖过引擎差异。
**这是一个明确的口径选择**，结论只对「≥2 秒的混合语句」成立。

音频不入库（上游 parquet 共约 1.2 GB）。入库的是
`docs/data/asr-zh-en-2026-09/manifest.json`：每条的上游 id、参考文本、
上游压缩字节哈希、写出的 WAV 哈希，外加每组的子集指纹。
重跑时指纹对不上就**拒绝提升**（数字不可比），正式目录保持原样。

用法：
    python3 scripts/asr-zhen-fetch.py <输出目录> [每组条数，默认 60]

依赖 pyarrow、soundfile（`uv pip install pyarrow soundfile`）。
"""
import hashlib
import io
import json
import os
import shutil
import sys
import tempfile
import urllib.request

HF = "https://huggingface.co/api/datasets"
SETS = {
    "zh": {
        "url": f"{HF}/google/fleurs/parquet/cmn_hans_cn/test/0.parquet",
        "source": "google/fleurs cmn_hans_cn test, CC-BY-4.0",
        # FLEURS 的 `transcription` 是逐字加空格、去标点的；`raw_transcription`
        # 保留原文。M0 的 CER 口径自己做归一，所以用原文。
        "ref_key": "raw_transcription",
        "filter": lambda r: True,
    },
    "en": {
        "url": f"{HF}/google/fleurs/parquet/en_us/test/0.parquet",
        "source": "google/fleurs en_us test, CC-BY-4.0",
        "ref_key": "raw_transcription",
        "filter": lambda r: True,
    },
    "mixed": {
        "url": f"{HF}/CAiRE/ASCEND/parquet/main/test/0.parquet",
        "source": "CAiRE/ASCEND main test, CC-BY-SA-4.0",
        "ref_key": "transcription",
        "filter": lambda r: r["language"] == "mixed" and r["duration"] >= 2.0,
    },
}
MANIFEST = os.path.join(
    os.path.dirname(os.path.abspath(__file__)),
    "..", "docs", "data", "asr-zh-en-2026-09", "manifest.json",
)


def _sha16(b: bytes) -> str:
    return hashlib.sha256(b).hexdigest()[:16]


def _file_sha16(path: str) -> str:
    with open(path, "rb") as f:
        return _sha16(f.read())


def _download(url: str, dst: str) -> None:
    if os.path.exists(dst):
        return
    print(f"==> 下载 {url}")
    part = dst + ".part"
    try:
        urllib.request.urlretrieve(url, part)
        os.replace(part, dst)
    finally:
        if os.path.exists(part):
            os.remove(part)


def _select(rows, n):
    if n > len(rows):
        raise SystemExit(f"只有 {len(rows)} 条，取不到 {n} 条")
    return [rows[(i * len(rows)) // n] for i in range(n)]


def _to_wav16k(audio_bytes: bytes, path: str) -> float:
    import numpy as np
    import soundfile as sf

    data, sr = sf.read(io.BytesIO(audio_bytes), dtype="float32")
    if data.ndim > 1:
        data = data.mean(axis=1)
    if sr != 16000:
        # 两份数据源实测都是 16 kHz；出现别的采样率说明上游变了，不静默重采样
        raise SystemExit(f"意外采样率 {sr}（{path}）")
    sf.write(path, np.asarray(data), sr, subtype="PCM_16")
    return len(data) / sr


def main() -> int:
    if not 2 <= len(sys.argv) <= 3:
        print(__doc__)
        return 2
    out = sys.argv[1]
    n = int(sys.argv[2]) if len(sys.argv) == 3 else 60
    import pyarrow.parquet as pq

    os.makedirs(out, exist_ok=True)
    stage = tempfile.mkdtemp(prefix=".staging-", dir=out)
    try:
        manifest = {"n_per_set": n, "sampling": "floor(i*N/n)，排序键为 parquet 原始行序", "sets": {}}
        for name, spec in SETS.items():
            pq_path = os.path.join(out, f"{name}.parquet")
            _download(spec["url"], pq_path)
            rows = [r for r in pq.read_table(pq_path).to_pylist() if spec["filter"](r)]
            sel = _select(rows, n)
            wav_dir = os.path.join(stage, "wav", name)
            os.makedirs(wav_dir, exist_ok=True)
            items, total = {}, 0.0
            for i, r in enumerate(sel):
                key = f"{name}{i:03d}"
                wav_path = os.path.join(wav_dir, f"{key}.wav")
                dur = _to_wav16k(r["audio"]["bytes"], wav_path)
                total += dur
                items[key] = {
                    "upstream_id": str(r["id"]),
                    "audio_sha256_16": _sha16(r["audio"]["bytes"]),
                    "wav_sha256_16": _file_sha16(wav_path),
                    "ref": r[spec["ref_key"]],
                    "duration_s": round(dur, 3),
                }
            blob = json.dumps(
                [[items[k]["upstream_id"], items[k]["ref"], items[k]["audio_sha256_16"]]
                 for k in sorted(items)] + [len(items), len(rows)],
                ensure_ascii=False,
            ).encode()
            fp = _sha16(blob)
            manifest["sets"][name] = {
                "source": spec["source"], "pool_size_after_filter": len(rows),
                "total_s": round(total, 1), "subset_sha256_16": fp, "items": items,
            }
            print(f"{name}: 取 {len(items)} 条 / 候选 {len(rows)}，总时长 {total:.1f}s，指纹 {fp}")

        if os.path.exists(MANIFEST):
            ref = json.load(open(MANIFEST))
            for name in SETS:
                want = ref["sets"][name]["subset_sha256_16"]
                got = manifest["sets"][name]["subset_sha256_16"]
                if want != got:
                    print(f"!! {name} 子集与入库 manifest 不符（入库 {want}，本次 {got}）"
                          f"——数字不可比，本次取样不提升", file=sys.stderr)
                    return 1
            print("✓ 三组子集都与入库 manifest 一致")
        else:
            os.makedirs(os.path.dirname(MANIFEST), exist_ok=True)
            with open(MANIFEST, "w") as f:
                json.dump(manifest, f, ensure_ascii=False, indent=1, sort_keys=True)
            print(f"已写入 {os.path.normpath(MANIFEST)}")

        final_wav = os.path.join(out, "wav")
        if os.path.exists(final_wav):
            shutil.rmtree(final_wav)
        os.replace(os.path.join(stage, "wav"), final_wav)
        with open(os.path.join(out, "manifest.json"), "w") as f:
            json.dump(manifest, f, ensure_ascii=False, indent=1, sort_keys=True)
        print(f"✓ 已写入 {final_wav}")
        return 0
    finally:
        shutil.rmtree(stage, ignore_errors=True)


if __name__ == "__main__":
    sys.exit(main())
