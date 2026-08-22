#!/usr/bin/env python3
"""取 FLEURS 泰语 test 的固定评测子集。

ADR-0004 §4 的准确率数字建立在这个子集上。**采样是确定性的**（等距，无随机数），
所以任何人重跑都能得到同一批句子。

数据源：`google/fleurs`，配置 `th_th`，split `test`，CC-BY-4.0。
音频不入库（几百 MB），入库的是 `scripts/fleurs-thai-manifest.json`——
里面有每条的 FLEURS id 与参考文本，可用来核对本脚本取出来的是不是同一批。

用法：
    python3 scripts/fleurs-thai-fetch.py <输出目录> [取样条数，默认 80]
"""
import hashlib
import io
import json
import os
import sys
import urllib.request

PARQUET_URL = (
    "https://huggingface.co/api/datasets/google/fleurs/parquet/th_th/test/0.parquet"
)
MANIFEST = os.path.join(os.path.dirname(os.path.abspath(__file__)), "fleurs-thai-manifest.json")


def main() -> int:
    if not 2 <= len(sys.argv) <= 3:
        print(__doc__)
        return 2
    out = sys.argv[1]
    want_n = int(sys.argv[2]) if len(sys.argv) == 3 else 80

    try:
        import pyarrow.parquet as pq
        import soundfile as sf
    except ImportError:
        print("需要 pyarrow 和 soundfile：pip install pyarrow soundfile", file=sys.stderr)
        return 1

    os.makedirs(out, exist_ok=True)
    pq_path = os.path.join(out, "test.parquet")
    if not os.path.exists(pq_path):
        print(f"==> 下载 {PARQUET_URL}")
        urllib.request.urlretrieve(PARQUET_URL, pq_path)

    rows = pq.read_table(pq_path).to_pylist()
    # 等距采样，**排序键是 parquet 的原始行序**（不是文件名、不是 id）。
    # 写清楚这一点：换个排序键会取到不同的句子，数字就对不上了。
    if want_n > len(rows):
        print(f"只有 {len(rows)} 条，取不到 {want_n} 条", file=sys.stderr)
        return 1
    step = len(rows) // want_n
    sel = [rows[i * step] for i in range(want_n)]

    wav_dir = os.path.join(out, "wav")
    os.makedirs(wav_dir, exist_ok=True)
    manifest, total = {}, 0.0
    for i, r in enumerate(sel):
        data, sr = sf.read(io.BytesIO(r["audio"]["bytes"]))
        if sr != 16000:
            print(f"意外采样率 {sr}", file=sys.stderr)
            return 1
        name = f"f{i:03d}"
        sf.write(os.path.join(wav_dir, f"{name}.wav"), data, sr, subtype="PCM_16")
        manifest[name] = {
            "fleurs_id": r["id"],
            "transcription": r["transcription"],
            "gender": r["gender"],
            "duration_s": round(len(data) / sr, 3),
        }
        total += len(data) / sr

    json.dump(manifest, open(os.path.join(out, "refs.json"), "w"),
              ensure_ascii=False, indent=1, sort_keys=True)

    # 和入库的 manifest 对账。对不上就是取到了不同的句子，数字不可比。
    blob = json.dumps({k: v["transcription"] for k, v in manifest.items()},
                      ensure_ascii=False, sort_keys=True).encode()
    digest = hashlib.sha256(blob).hexdigest()[:16]
    print(f"取样 {len(manifest)} 条（步长 {step}），总时长 {total:.1f}s，参考文本指纹 {digest}")

    if os.path.exists(MANIFEST):
        ref = json.load(open(MANIFEST))
        if ref.get("refs_sha256_16") != digest:
            print(f"!! 与入库 manifest 不符（入库 {ref.get('refs_sha256_16')}）——"
                  f"上游数据可能变了，ADR-0004 §4 的数字不可比", file=sys.stderr)
            return 1
        print("✓ 与入库 manifest 一致")
    else:
        json.dump({"n": len(manifest), "step": step, "refs_sha256_16": digest,
                   "source": "google/fleurs th_th test, CC-BY-4.0",
                   "sampling": "等距，排序键为 parquet 原始行序",
                   "items": manifest}, open(MANIFEST, "w"),
                  ensure_ascii=False, indent=1, sort_keys=True)
        print(f"已写入 {MANIFEST}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
