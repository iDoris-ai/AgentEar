#!/usr/bin/env python3
"""取 FLEURS 泰语 test 的固定评测子集。

ADR-0004 §4 的准确率数字建立在这个子集上。**采样是确定性的**
（固定步长，无随机数），所以任何人重跑都能得到同一批录音。
注意是**固定步长**而非覆盖全区间的等距采样，尾部若干行抽不到，详见下方注释。

数据源：`google/fleurs`，配置 `th_th`，split `test`，CC-BY-4.0。
音频不入库（80 条约 32 MB，另有约 753 MB 的上游 parquet），
入库的是 `scripts/fleurs-thai-manifest.json`——
里面有每条的 FLEURS id 与参考文本，可用来核对本脚本取出来的是不是同一批。

用法：
    python3 scripts/fleurs-thai-fetch.py <输出目录> [取样条数，默认 80]
"""
import hashlib
import io
import json
import os
import shutil
import sys
import tempfile
import urllib.request

PARQUET_URL = (
    "https://huggingface.co/api/datasets/google/fleurs/parquet/th_th/test/0.parquet"
)
MANIFEST = os.path.join(os.path.dirname(os.path.abspath(__file__)), "fleurs-thai-manifest.json")


def _file_sha16(path: str) -> str:
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()[:16]


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
        # 先下到 .part 再改名。直接写目标路径的话，下到一半被 Ctrl-C 或断网，
        # 留下的半个文件在下次运行时会被 `os.path.exists` 当成「已下载」，
        # 于是拿一份截断的 parquet 去抽样——抽出来的子集指纹对不上，
        # 报的错却指向「上游数据变了」，排查方向整个是歪的。
        part = pq_path + ".part"
        try:
            urllib.request.urlretrieve(PARQUET_URL, part)
            os.replace(part, pq_path)
        finally:
            if os.path.exists(part):
                os.remove(part)

    rows = pq.read_table(pq_path).to_pylist()
    # 固定步长采样，**排序键是 parquet 的原始行序**（不是文件名、不是 id）。
    # 换个排序键会取到不同的句子，数字就对不上。
    #
    # ⚠️ 这是**固定步长**，不是覆盖全区间的等距采样：1021//80=12，
    # 只取到索引 0..948，**最后 72 行永远抽不到**。
    # 保持现状是为了不让已产出的数字失效；若将来重建评测集，
    # 应改成 floor(i * len(rows) / want_n)。
    if want_n > len(rows):
        print(f"只有 {len(rows)} 条，取不到 {want_n} 条", file=sys.stderr)
        return 1
    step = len(rows) // want_n
    sel = [rows[i * step] for i in range(want_n)]

    # 先全部写进暂存目录，**与入库 manifest 对上账之后才搬到正式位置**。
    # 直接写正式位置的话，一批对不上的数据会留在 wav/ 和 refs.json 里，
    # 而 cer-thai.py 只认路径不认指纹，下一步会照跑不误——
    # 拿着被拒绝的评测集算出一组看着正常、实则不可比的 CER。
    # 用 mkdtemp 而不是固定的 .staging：固定名字会在每次启动时无条件递归删除，
    # 撞上用户自己的同名目录或另一个并发实例就是直接删对方的数据。
    stage = tempfile.mkdtemp(prefix=".staging-", dir=out)
    try:
        return _fetch_into(stage, out, sel, step, pq_path)
    finally:
        # 只删自己这一次建的那个精确路径，且不吞错误——不然
        #「已删除本次取样」这句话不一定是真的
        shutil.rmtree(stage, ignore_errors=True)


def _fetch_into(stage, out, sel, step, pq_path):
    import soundfile as sf
    wav_dir = os.path.join(stage, "wav")
    os.makedirs(wav_dir, exist_ok=True)
    manifest, total = {}, 0.0
    for i, r in enumerate(sel):
        data, sr = sf.read(io.BytesIO(r["audio"]["bytes"]))
        if sr != 16000:
            print(f"意外采样率 {sr}", file=sys.stderr)
            return 1
        name = f"f{i:03d}"
        wav_path = os.path.join(wav_dir, f"{name}.wav")
        sf.write(wav_path, data, sr, subtype="PCM_16")
        manifest[name] = {
            "fleurs_id": r["id"],
            # 上游压缩字节的哈希：判「取到的是不是同一批上游数据」
            "audio_sha256_16": hashlib.sha256(r["audio"]["bytes"]).hexdigest()[:16],
            # **写出来的 WAV 文件本身**的哈希：cer-thai.py 推理前拿它对账，
            # 判「送进 CLI 的确实是这一份」。上面那个哈希做不到这件事 ——
            # 它是压缩字节，和磁盘上的 PCM 文件对不上。
            "wav_sha256_16": _file_sha16(wav_path),
            "transcription": r["transcription"],
            "gender": r["gender"],
            "duration_s": round(len(data) / sr, 3),
        }
        total += len(data) / sr

    json.dump(manifest, open(os.path.join(stage, "refs.json"), "w"),
              ensure_ascii=False, indent=1, sort_keys=True)

    # 和入库的 manifest 对账。对不上就是取到了不同的句子，数字不可比。
    # 指纹要覆盖「取到的是不是同一批录音」，所以 id 和**音频字节**都得进去。
    # 只按参考文本算的话，上游换了音频、文字没动，就检测不出来。
    blob = json.dumps(
        [[manifest[k]["fleurs_id"], manifest[k]["transcription"], manifest[k]["audio_sha256_16"]]
         for k in sorted(manifest)] + [len(manifest), step],
        ensure_ascii=False,
    ).encode()
    digest = hashlib.sha256(blob).hexdigest()[:16]
    n_uniq = len({v["fleurs_id"] for v in manifest.values()})
    print(f"取样 {len(manifest)} 条录音（步长 {step}，唯一 fleurs_id {n_uniq} 个），"
          f"总时长 {total:.1f}s，指纹 {digest}")
    if n_uniq != len(manifest):
        print(f"   注：{len(manifest) - n_uniq} 条与其他条目共用 prompt（同文本的不同录音）")

    # 分块读：整块读入 753 MB 的 parquet 只为算一个哈希，内存上没必要
    _h = hashlib.sha256()
    with open(pq_path, "rb") as _f:
        for _chunk in iter(lambda: _f.read(1 << 20), b""):
            _h.update(_chunk)
    pq_sha = _h.hexdigest()

    if os.path.exists(MANIFEST):
        ref = json.load(open(MANIFEST))
        # 两个判断分开报，不要混成一个：
        #   子集指纹 —— 决定 CER 数字还可不可比（**这条不过就是硬错误**）
        #   parquet 全文件 sha —— 决定整个上游文件是否原样（只改了没抽中的行
        #                        时子集仍一致，那不影响可比性，所以只提示）
        if ref.get("refs_sha256_16") != digest:
            # 暂存目录由外层的 finally 删除，正式位置本来就还没被动过
            print(f"!! 评测子集与入库 manifest 不符（入库 {ref.get('refs_sha256_16')}，"
                  f"本次 {digest}）——ADR-0004 §4 的数字不可比。"
                  f"\n   本次取样不会被提升，{out} 下的正式数据保持原样。", file=sys.stderr)
            return 1
        print("✓ 评测子集与入库 manifest 一致")
        if ref.get("parquet_sha256") and ref["parquet_sha256"] != pq_sha:
            print("   注：上游 parquet 整体已变（但抽中的 80 条未变，数字仍可比）",
                  file=sys.stderr)
    else:
        json.dump({"n": len(manifest), "step": step, "refs_sha256_16": digest,
                   "unique_fleurs_ids": n_uniq,
                   "parquet_sha256": pq_sha,
                   "source": "google/fleurs th_th test, CC-BY-4.0",
                   "source_card": "https://huggingface.co/datasets/google/fleurs",
                   "sampling": "固定步长，排序键为 parquet 原始行序；尾部 72 行未覆盖",
                   "items": manifest}, open(MANIFEST, "w"),
                  ensure_ascii=False, indent=1, sort_keys=True)
        print(f"已写入 {MANIFEST}")

    # 对上账了才搬进正式位置。**wav/ 和 refs.json 要作为一组换上去。**
    # 原来是「删旧 wav → 提升新 wav → 提升 refs.json」，中间任一步失败就留下
    # 「新 WAV + 旧 refs」或者「有 WAV 没 refs」—— 正是 staging 想避免的那种
    # 正式目录不一致。严格的原子发布要版本目录 + symlink 切换；这里退一步：
    # **旧数据先挪开不删**，两步都成功才删；任一步失败就整组回滚。
    final_wav = os.path.join(out, "wav")
    final_refs = os.path.join(out, "refs.json")
    backup = tempfile.mkdtemp(prefix=".prev-", dir=out)
    moved = []
    try:
        if os.path.exists(final_wav):
            os.replace(final_wav, os.path.join(backup, "wav")); moved.append("wav")
        if os.path.exists(final_refs):
            os.replace(final_refs, os.path.join(backup, "refs.json")); moved.append("refs.json")
        os.replace(wav_dir, final_wav)
        os.replace(os.path.join(stage, "refs.json"), final_refs)
    except BaseException:
        # 回滚：先撤掉本次已经换上去的，再把旧数据原样搬回
        if "wav" in moved:
            shutil.rmtree(final_wav, ignore_errors=True)
            os.replace(os.path.join(backup, "wav"), final_wav)
        elif os.path.exists(final_wav):
            shutil.rmtree(final_wav, ignore_errors=True)
        if "refs.json" in moved:
            if os.path.exists(final_refs):
                os.remove(final_refs)
            os.replace(os.path.join(backup, "refs.json"), final_refs)
        elif os.path.exists(final_refs):
            os.remove(final_refs)
        shutil.rmtree(backup, ignore_errors=True)
        raise
    shutil.rmtree(backup, ignore_errors=True)
    print(f"✓ 已写入 {final_wav} 与 {final_refs}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
