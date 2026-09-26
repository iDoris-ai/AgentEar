#!/usr/bin/env python3
"""录音提示音会不会把 ASR 带偏？——数字混音实测（v0.21.0，cue.rs）。

做法：用 `agentear --cue-wav` 拿到**守护进程播的同一段波形**（16 kHz），
按真实时序混进一段语音（开始音在 0 秒、双击「补一声」在 0.35 秒、
结束音贴在末尾），再走产品自己的 `agentear --transcribe`，
和不混提示音的基线逐字比对。每个组合跑 RUNS 次。

⚠️ 这是**数字混音**，不是「扬声器 → 空气 → 麦克风」：
真实路径上提示音会被房间和麦克风滤波、音量也不同。
这里混的是满幅 30% 的原始波形，比真实录进去的通常**更响**（保守方向），
但回声、失真、蓝牙耳机的延迟都不在这个测法里。

用法：
    scripts/cue-asr-check.py --agentear target/release/agentear --out docs/data/cue-asr-2026-09
依赖：只用标准库 + macOS 的 `say`。
"""

import argparse
import os
import struct
import subprocess
import sys
import tempfile
import wave

RATE = 16_000
RUNS = 3

# (语种标签, say 的声音, 文本, transcribe 额外参数)
SPEECH = [
    ("zh", "Tingting", "今天下午三点开会，记得带上电脑和充电器。", []),
    ("en", "Samantha", "Please remind me to call Alice tomorrow morning.", []),
    ("th", "Kanya", "พรุ่งนี้เช้าช่วยเตือนให้โทรหาแม่ด้วย", ["--lang", "th"]),
]


def read_pcm(path):
    with wave.open(path, "rb") as w:
        assert w.getnchannels() == 1 and w.getsampwidth() == 2, path
        assert w.getframerate() == RATE, f"{path}: {w.getframerate()} Hz"
        return list(struct.unpack(f"<{w.getnframes()}h", w.readframes(w.getnframes())))


def write_pcm(path, pcm):
    with wave.open(path, "wb") as w:
        w.setnchannels(1)
        w.setsampwidth(2)
        w.setframerate(RATE)
        w.writeframes(struct.pack(f"<{len(pcm)}h", *pcm))


def mix(base, overlay, at_ms):
    out = list(base)
    start = int(at_ms * RATE / 1000)
    need = start + len(overlay)
    if need > len(out):
        out.extend([0] * (need - len(out)))
    for i, v in enumerate(overlay):
        out[start + i] = max(-32768, min(32767, out[start + i] + v))
    return out


def silence(ms):
    return [0] * int(ms * RATE / 1000)


def run(cmd):
    r = subprocess.run(cmd, capture_output=True, text=True)
    if r.returncode != 0:
        sys.exit(f"失败（{r.returncode}）：{' '.join(cmd)}\n{r.stderr[-2000:]}")
    return r.stdout


def transcribe(agentear, wav, extra):
    out = run([agentear, "--transcribe", wav, *extra])
    # --transcribe 把转写结果打到 stdout；去掉首尾空白后整段就是结果。
    return out.strip()


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--agentear", required=True)
    ap.add_argument("--out", required=True)
    ap.add_argument("--langs", default="zh,en,th")
    args = ap.parse_args()
    os.makedirs(args.out, exist_ok=True)
    langs = set(args.langs.split(","))

    tmp = tempfile.mkdtemp(prefix="cue-asr-")
    cues = {}
    for name in ("start", "upgrade", "end", "start-conversation", "end-conversation"):
        p = os.path.join(tmp, f"{name}.wav")
        run([args.agentear, "--cue-wav", name, p, "--rate", str(RATE)])
        cues[name] = read_pcm(p)

    rows = []
    for lang, voice, text, extra in SPEECH:
        if lang not in langs:
            continue
        aiff = os.path.join(tmp, f"{lang}.wav")
        run(["say", "-v", voice, "-o", aiff, "--data-format=LEI16@16000", text])
        speech = read_pcm(aiff)
        # 录音的真实形状：按键 → 麦克风开 → 人反应 ~250ms 才开口 → 说完 → 停。
        body = silence(250) + speech + silence(300)
        variants = {
            "baseline": body,
            # 开始音：麦克风开的那一刻响，落在录音的最开头。
            "start@0": mix(body, cues["start"], 0),
            # 双击：第一下的开始音在 0，第二下「补一声」在 350ms——
            # 此时人可能已经开口（250ms），故意让它**压在语音上**。
            "double(start@0+upgrade@350)": mix(mix(body, cues["start"], 0), cues["upgrade"], 350),
            # 对话模式直接开始（两声）：压到 200ms。
            "start-conversation@0": mix(body, cues["start-conversation"], 0),
            # 结束音本该在关麦之后才响，这里假设它漏进了最后 70ms（最坏情况）。
            "end@tail": mix(body, cues["end"], len(body) * 1000 / RATE - 70),
            # 对照：同样长的纯静音、**没有提示音**。whisper（泰语路径）在纯静音上
            # 本身就会幻觉出字——没有这一格，就会把既有问题错算到提示音头上。
            "silence-only(no cue, control)": silence(1500),
            # 只有提示音、没人说话：不该转出任何字。
            "start-only(no speech)": mix(silence(1500), cues["start"], 0),
            "double-only(no speech)": mix(mix(silence(1500), cues["start"], 0), cues["upgrade"], 350),
        }
        for vname, pcm in variants.items():
            wav = os.path.join(tmp, f"{lang}-{vname.replace('/', '_')}.wav")
            write_pcm(wav, pcm)
            for k in range(RUNS):
                got = transcribe(args.agentear, wav, extra)
                rows.append((lang, vname, k + 1, got))
                print(f"{lang}\t{vname}\t#{k + 1}\t{got!r}", flush=True)

    tsv = os.path.join(args.out, "results.tsv")
    with open(tsv, "w", encoding="utf-8") as f:
        f.write("lang\tvariant\trun\ttranscript\n")
        for lang, vname, k, got in rows:
            f.write(f"{lang}\t{vname}\t{k}\t{got}\n")

    # 判定：带语音的变体 == 基线（逐字）；无语音的变体 == 空。
    base = {}
    for lang, vname, k, got in rows:
        if vname == "baseline":
            base.setdefault(lang, set()).add(got)
    lines = ["| 语种 | 变体 | 3 次结果一致 | 判定 | 转写 |", "|---|---|---|---|---|"]
    ok_all = True
    for lang in [s[0] for s in SPEECH if s[0] in langs]:
        control = [g for l, v, _, g in rows if l == lang and v.startswith("silence-only")]
        control_dirty = any(control)
        for vname in dict.fromkeys(v for l, v, _, _ in rows if l == lang):
            outs = [g for l, v, _, g in rows if l == lang and v == vname]
            stable = len(set(outs)) == 1
            if vname.startswith("silence-only"):
                verdict = "对照" + ("（纯静音本身就出字）" if control_dirty else "（为空）")
            elif "no speech" in vname:
                if all(o == "" for o in outs):
                    verdict = "✅ 为空"
                elif control_dirty:
                    # 纯静音已经会出字：提示音只是换了幻觉的内容，不是它引入的。
                    verdict = "⚠️ 出字，但对照组纯静音同样出字（既有问题）"
                else:
                    verdict = "❌ 提示音引入了文字"
                    ok_all = False
            else:
                if all(o in base[lang] for o in outs):
                    verdict = "✅ 与基线逐字相同"
                else:
                    verdict = "❌ 与基线不同"
                    ok_all = False
            shown = " / ".join(dict.fromkeys(repr(o) for o in outs))
            lines.append(f"| {lang} | {vname} | {'是' if stable else '否'} | {verdict} | {shown} |")
    lines.append("")
    lines.append(f"基线转写：" + "；".join(f"{l}={sorted(v)}" for l, v in base.items()))
    summary = os.path.join(args.out, "summary.md")
    with open(summary, "w", encoding="utf-8") as f:
        f.write("\n".join(lines) + "\n")
    print("\n".join(lines))
    print(f"\n→ {tsv}\n→ {summary}")
    sys.exit(0 if ok_all else 1)


if __name__ == "__main__":
    main()
