"""量「声色与音量稳不稳」：给若干条 wav，逐条报 F0 中位数 / 浊音帧数 / RMS / 峰值。

用法：`python3 scripts/measure-f0.py a.wav b.wav c.wav`

⚠️ **它原本是丢弃式探针，2026-09-16 才入库**——因为 `services/tts/backends.py`
与 `services/tts/make_voice.py` 都**引用它的数字当实测来源**
（「不归一时 RMS 差 4.54 倍」那条）。它当时躺在 `vendor/models/talk/` 下，
而那个目录**被 gitignore**：新克隆的仓库里没有它，**那些数字就没有可复现的来源**。
要么别引用它，要么把它入库——选了后者（jason 拍板）。

⚠️ 它要 numpy（本仓库非交互 shell 的 `python3` 是 3.9.6，**有** numpy 2.0.2）。


jason 的原话是「声量和语音发音的男生女声和声量的高低……都飘忽不定」，
所以这里就量那两件事，**不用需要下载模型的工具**（沙箱不让写 ~/Library/Caches）：

- **F0 中位数**：用自相关法估基频，按帧取中位数。女声大致 165–255 Hz、男声 85–155 Hz，
  所以它能捕捉「这一次是男声、下一次是女声」这种漂移。
  ⚠️ **F0 不等于音色身份**：它测的是音高/性别这一维，测不出「像不像同一个人」。
  要判后者得用说话人嵌入（`speech embed-speaker`），那需要联网下载模型，
  在本 sandbox 里跑不了——**这一点必须写清楚，不能拿 F0 冒充音色相似度**。
- **RMS 与峰值**：直接对应「声量高低」。
"""

import sys
import wave

import numpy as np

RATE = 48000
FRAME = 1024
HOP = 512
F0_MIN, F0_MAX = 60.0, 400.0


def read_wav(path):
    with wave.open(path, "rb") as w:
        rate = w.getframerate()
        data = np.frombuffer(w.readframes(w.getnframes()), dtype="<i2").astype(np.float64)
    return data / 32768.0, rate


def frame_f0(frame, rate):
    """自相关法估这一帧的 F0；不是浊音就返回 None。"""
    frame = frame - frame.mean()
    if np.sqrt(np.mean(frame**2)) < 0.01:  # 静音/太轻
        return None
    corr = np.correlate(frame, frame, mode="full")[len(frame) - 1 :]
    lo = int(rate / F0_MAX)
    hi = min(int(rate / F0_MIN), len(corr) - 1)
    if hi <= lo:
        return None
    seg = corr[lo:hi]
    peak = int(np.argmax(seg)) + lo
    if corr[peak] < 0.3 * corr[0]:  # 自相关太弱 → 不是浊音
        return None
    return rate / peak


def analyze(path):
    x, rate = read_wav(path)
    f0s = []
    for start in range(0, len(x) - FRAME, HOP):
        f0 = frame_f0(x[start : start + FRAME], rate)
        if f0:
            f0s.append(f0)
    rms = float(np.sqrt(np.mean(x**2)))
    peak = float(np.max(np.abs(x)))
    return {
        "f0": float(np.median(f0s)) if f0s else float("nan"),
        "f0_n": len(f0s),
        "rms": rms,
        "peak": peak,
    }


paths = sys.argv[1:]
stats = [analyze(p) for p in paths]
for p, s in zip(paths, stats):
    print(f"  {p.split('/')[-1]:16s} F0中位={s['f0']:6.1f}Hz 浊音帧={s['f0_n']:4d} RMS={s['rms']:.4f} 峰值={s['peak']:.3f}")

f0s = [s["f0"] for s in stats if not np.isnan(s["f0"])]
rmss = [s["rms"] for s in stats]
if len(f0s) > 1:
    print(f"\n  F0 中位数：{min(f0s):.1f}–{max(f0s):.1f}Hz，极差 {max(f0s)-min(f0s):.1f}Hz"
          f"（女声带 165–255 / 男声带 85–155）")
    spread = (max(f0s) - min(f0s)) / np.mean(f0s) * 100
    print(f"  相对极差 {spread:.1f}%")
    if max(f0s) > 155 and min(f0s) < 155:
        print("  ⚠️ 这几次生成横跨了男女声带 —— 就是「男生女声飘忽」")
print(f"  RMS：{min(rmss):.4f}–{max(rmss):.4f}，倍数 {max(rmss)/max(min(rmss),1e-9):.2f}×")
