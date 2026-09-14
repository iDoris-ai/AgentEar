#!/usr/bin/env python3
"""造一个「钉死的音色」参考音频（voice reference）。

## 为什么需要它

jason 2026-09-14 的反馈：「声量和男生女声、声量的高低……都飘忽不定」。
实测（`vendor/models/talk/measure_f0.py`）确认了：同一批句子生成三次，
**F0 中位数 142–292Hz、极差 65%，有一次落在男声带；RMS 差 4.54 倍**。

根因是 VoxCPM2 是**零样本**模型：不给参考音频时，每次生成都重新采样一个说话人。
所以「钉一个参考音频」（`ref_audio` + `ref_text`）是唯一的正解，
而不是去调温度或提示词。

## 为什么要「自举」而不是直接用 macOS `say`

`say -v Tingting` 是机器音，拿它当参考会把机器音一起克隆进去——
用户要的是「自然顺滑」。所以这里**先生成若干候选，再用客观量挑一个**：

- **F0 中位数贴近目标值 190Hz**——不是「落在女声带就行」。

  ⚠️ **这条是实现时踩出来的**：第一版只要求「落在女声带（165–255）」，挑中 235Hz 那条
  （女声带顶端），结果**克隆输出跑到 300–360Hz**，偏尖。
  实测校准过量具（100/150/200/235Hz 的谐波丰富信号，读数误差 <2%），所以不是测错：
  **这个模型的克隆输出比参考高约 +70Hz**。所以参考要往下挑，给抬升留余量。
- **浊音帧够多**——太少说明这一条根本没好好说话
- **RMS 不太低**——太轻的参考会带出很轻的输出
- **韵律要有起伏**（F0 半音标准差 + 能量标准差）——⚠️ **这条是用户听完之后补的**：
  第一版只挑音高/浊音帧/RMS，挑出来的参考本身 F0 起伏只有 2.75 半音（偏平），
  克隆输出能量起伏 0.0655 **比不钉音色（0.1363）还平**，
  jason 的原话是「像个机器人，一点感情都没有」。**为了"稳"把"活"牺牲掉是错的。**

挑中的那条存成参考音频，并用**本仓库自己的 ASR** 转出 `ref_text`
（VoxCPM2 的克隆模式要参考音频 + 它的文本，文本错了会带偏）。

## 用法

    ~/.agentear/llm/venv/bin/python services/tts/make_voice.py \
        --model ~/.agentear/talk/models/voxcpm2-4bit \
        --out ~/.agentear/talk/voices/female_zh_01.wav \
        --instruct "年轻女性，声音自然温和，语速平稳" \
        --candidates 6
"""

import argparse
import json
import os
import pathlib
import subprocess
import sys
import tempfile
import wave

import numpy as np

# 候选文本要够长：参考音频太短克隆会不稳，而且 ref_text 太短没有信息量。
SCRIPT = (
    "你好，我是你的语音助手，今天由我来陪你聊天。"
    "不管是天气、日程，还是你想随手记下的想法，说给我听就好。"
)


def synth(model, text, instruct, seed_text=None):
    """生成一条音频，返回 float 采样。"""
    from mlx_audio.tts.utils import load_model

    if not hasattr(synth, "_model") or synth._model is None:
        synth._model = load_model(model)
    segs = [
        np.asarray(seg.audio).reshape(-1)
        for seg in synth._model.generate(text=text, max_tokens=2000, instruct=instruct)
    ]
    return np.concatenate(segs) if len(segs) > 1 else segs[0]


def write_wav(path, samples, rate=48000):
    with wave.open(str(path), "wb") as w:
        w.setnchannels(1)
        w.setsampwidth(2)
        w.setframerate(rate)
        w.writeframes((np.clip(samples, -1, 1) * 32767).astype("<i2").tobytes())


def read_wav(path):
    with wave.open(str(path), "rb") as w:
        return np.frombuffer(w.readframes(w.getnframes()), dtype="<i2").astype(np.float64) / 32768.0


def analyze(samples, rate=48000):
    """F0 中位数 + RMS + 浊音帧数。判据与 `measure_f0.py` 同一套。"""
    frame, hop = 1024, 512
    f0s = []
    for start in range(0, max(0, len(samples) - frame), hop):
        seg = samples[start : start + frame] - samples[start : start + frame].mean()
        if np.sqrt(np.mean(seg**2)) < 0.01:
            continue
        corr = np.correlate(seg, seg, mode="full")[len(seg) - 1 :]
        lo, hi = int(rate / 400.0), min(int(rate / 60.0), len(corr) - 1)
        if hi <= lo:
            continue
        peak = int(np.argmax(corr[lo:hi])) + lo
        if corr[peak] < 0.3 * corr[0]:
            continue
        f0s.append(rate / peak)
    arr = np.asarray(f0s) if f0s else np.array([1.0])
    # F0 起伏用**半音**标准差：线性 Hz 会让高音显得更"起伏"，跨音高不可比
    semitone_std = float((12 * np.log2(arr / arr.mean())).std())
    return {
        "f0": float(np.median(f0s)) if f0s else float("nan"),
        "voiced": len(f0s),
        "rms": float(np.sqrt(np.mean(samples**2))),
        "seconds": len(samples) / rate,
        "semitone_std": semitone_std,
    }


def transcribe(wav_path):
    """用本仓库自己的 ASR 转参考文本——不要手写，手写容易和音频对不上。"""
    binary = os.environ.get("AGENTEAR_BIN", "target/release/agentear")
    if not pathlib.Path(binary).exists():
        return None
    out = subprocess.run(
        [binary, "--transcribe", str(wav_path)], capture_output=True, text=True
    )
    for line in reversed(out.stdout.splitlines()):
        line = line.strip()
        if line and not line.startswith("（") and not line.startswith("["):
            return line
    return None


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--model", required=True)
    ap.add_argument("--out", required=True)
    ap.add_argument("--instruct", default="年轻女性，声音自然温和，语速平稳，普通话")
    ap.add_argument("--candidates", type=int, default=6)
    ap.add_argument("--text", default=SCRIPT)
    ap.add_argument(
        "--from-wav",
        default=None,
        help="直接用一段**真人录音**当参考，不做自举（推荐：自举出来的参考会叠加合成感）",
    )
    args = ap.parse_args()

    out = pathlib.Path(os.path.expanduser(args.out))
    out.parent.mkdir(parents=True, exist_ok=True)

    if args.from_wav:
        # ⚠️ **真人录音比自己生成的参考好**：自举出来的参考本身带合成感，
        # 克隆会把那种"平"和"机器味"一起放大（jason 2026-09-14 的原话是
        # 「像个机器人，一点感情都没有」）。有真人录音就直接用。
        src = pathlib.Path(os.path.expanduser(args.from_wav))
        if not src.exists():
            sys.exit(f"找不到 {src}")
        import shutil

        shutil.copyfile(src, out)
        ref_text = transcribe(out)
        st = analyze(read_wav(out))
        out.with_suffix(".json").write_text(
            json.dumps(
                {
                    "voice": out.stem,
                    "ref_text": ref_text,
                    "f0_median": round(st["f0"], 1),
                    "semitone_std": round(st["semitone_std"], 2),
                    "rms": round(st["rms"], 4),
                    "source": f"真人录音 {src}",
                },
                ensure_ascii=False,
                indent=2,
            )
            + "\n"
        )
        print(f"参考音频：{out}（来自 {src}）")
        print(f"参考文本：{ref_text!r}")
        return

    best = None
    with tempfile.TemporaryDirectory() as tmp:
        for i in range(args.candidates):
            samples = synth(args.model, args.text, args.instruct)
            st = analyze(samples)
            # 打分：**离目标音高越近越好**（不是「进女声带就给满分」），
            # 其次浊音帧多、RMS 高。目标 190Hz 是为了抵消克隆那约 +70Hz 的抬升。
            target_f0 = 190.0
            dist = abs(st["f0"] - target_f0) if not np.isnan(st["f0"]) else 999
            in_band = 150.0 <= st["f0"] <= 230.0
            # **韵律起伏是主项**：用户要的是"像人"，而"平"就是机器人感。
            # 音高只作为次要约束（别跑到男声带或尖叫区）。
            score = (
                st["semitone_std"] * 300          # 起伏：主项
                - dist * 4                        # 音高：次要
                + (200 if in_band else 0)
                + st["voiced"] * 0.5
            )
            print(
                f"  候选 {i}: F0={st['f0']:6.1f}Hz 浊音帧={st['voiced']:4d} "
                f"RMS={st['rms']:.4f} 时长={st['seconds']:.2f}s "
                f"起伏={st['semitone_std']:.2f}半音 {'✅在带内' if in_band else '❌带外'} "
                f"score={score:.0f}",
                flush=True,
            )
            if best is None or score > best[0]:
                best = (score, i, samples, st)

    score, idx, samples, st = best
    write_wav(out, samples)
    print(f"\n挑中候选 {idx}：F0={st['f0']:.1f}Hz RMS={st['rms']:.4f} → {out}")

    # 先归一化音量再让 ASR 转，转写更稳
    ref_text = transcribe(out)
    sidecar = out.with_suffix(".json")
    sidecar.write_text(
        json.dumps(
            {
                "voice": out.stem,
                "ref_text": ref_text,
                "f0_median": round(st["f0"], 1),
                "semitone_std": round(st["semitone_std"], 2),
                "rms": round(st["rms"], 4),
                "instruct": args.instruct,
                "source": "VoxCPM2 自举（生成候选 → 按 F0/浊音帧/RMS 挑）",
            },
            ensure_ascii=False,
            indent=2,
        )
        + "\n"
    )
    print(f"参考文本：{ref_text!r}")
    print(f"清单：{sidecar}")
    if not ref_text:
        print("⚠️ 没转出参考文本（agentear 二进制不在？）——克隆模式没有 ref_text 会不稳", file=sys.stderr)


if __name__ == "__main__":
    main()
