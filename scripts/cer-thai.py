"""在 FLEURS 泰语 test 上算 CER。

归一化口径（先定死，否则数字不可比）：
  - Unicode NFC
  - 去掉**全部空白**。泰语词间不用空格，FLEURS 的空格是短语边界，
    而各模型加不加、加在哪都不一样，计进去测的是排版不是识别。
  - 拉丁字母转小写（FLEURS 的 transcription 字段已经是小写的）
  - 去掉标点

**两种粒度都报，不预设哪个是唯一正确答案**：
  - cp   : 按 Unicode code point
  - gc   : 按字素簇（泰文的声调符/元音符附着在辅音上，视觉上是一个字）
两者会给出不同的数字，差值本身能说明错误集中在组合符号上还是基字上。
"""
import json, sys, unicodedata, subprocess, os, time

THAI_COMBINING = set(range(0x0E31, 0x0E32)) | set(range(0x0E34, 0x0E3B)) | set(range(0x0E47, 0x0E4F))
PUNCT = set('.,!?;:"\'()[]{}<>—–-…"" '' `~@#$%^&*_+=|\\/๚๛ฯ')


def norm(s: str) -> str:
    s = unicodedata.normalize("NFC", s).lower()
    return "".join(c for c in s if not c.isspace() and c not in PUNCT)


def graphemes(s: str):
    """把泰文组合符号并入前一个基字。非泰文按单字符处理。"""
    out = []
    for c in s:
        if out and ord(c) in THAI_COMBINING:
            out[-1] += c
        else:
            out.append(c)
    return out


def edit(a, b) -> int:
    if len(a) < len(b):
        a, b = b, a
    prev = list(range(len(b) + 1))
    for i, ca in enumerate(a, 1):
        cur = [i]
        for j, cb in enumerate(b, 1):
            cur.append(min(prev[j] + 1, cur[j - 1] + 1, prev[j - 1] + (ca != cb)))
        prev = cur
    return prev[-1]


def main():
    sp = os.path.dirname(os.path.abspath(__file__))
    model, cli = sys.argv[1], sys.argv[2]
    refs = json.load(open(f"{sp}/fleurs/refs.json"))
    tot_cp = tot_cp_n = tot_gc = tot_gc_n = 0
    empty = 0
    t0 = time.time()
    dump = {}
    for name, r in sorted(refs.items()):
        wav = f"{sp}/fleurs/wav/{name}.wav"
        try:
            hyp = subprocess.run(
                [cli, "-m", model, "-f", wav, "-l", "th", "-t", "4",
                 "-bo", "1", "-bs", "1", "-np", "-nt"],
                capture_output=True, text=True, timeout=300,
            ).stdout.strip()
        except subprocess.TimeoutExpired:
            hyp = ""
        if not hyp:
            empty += 1
        dump[name] = hyp
        ref_n, hyp_n = norm(r["transcription"]), norm(hyp)
        tot_cp += edit(ref_n, hyp_n); tot_cp_n += len(ref_n)
        rg, hg = graphemes(ref_n), graphemes(hyp_n)
        tot_gc += edit(rg, hg); tot_gc_n += len(rg)
    tag = os.path.basename(model).replace(".bin", "")
    json.dump(dump, open(f"{sp}/fleurs/hyp-{tag}.json", "w"), ensure_ascii=False, indent=1)
    print(f"{tag:24s} CER_cp={tot_cp/tot_cp_n:.4f}  CER_gc={tot_gc/tot_gc_n:.4f}  "
          f"空输出={empty}/{len(refs)}  耗时={time.time()-t0:.0f}s")


main()
