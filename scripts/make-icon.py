#!/usr/bin/env python3
"""生成 AgentEar 的 app 图标（assets/AgentEar.icns）。

没有图标时，macOS 在控制中心的麦克风占用面板、通知、系统设置的权限列表里
都只会显示一个空白占位方块——用户看到「某个没有图标的东西在用麦克风」，
对一个隐私优先的工具来说是很糟的第一印象。

设计取向：极简，能在 16×16 读出来。一只耳朵的轮廓 + 声波，
呼应「Ear」而不是又一个话筒图标（菜单栏那个 🎙 已经是话筒了）。

用法：python3 scripts/make-icon.py
依赖：Pillow（图形），iconutil（macOS 自带，打包成 .icns）
"""

import math
import pathlib
import subprocess
import tempfile

from PIL import Image, ImageDraw

ROOT = pathlib.Path(__file__).resolve().parent.parent
OUT = ROOT / "assets" / "AgentEar.icns"

# 先用 4 倍尺寸画再缩小，等价于一次廉价的抗锯齿
S = 1024
SS = 4
N = S * SS

BG_TOP = (56, 78, 122)
BG_BOTTOM = (26, 34, 56)
FG = (255, 255, 255)
ACCENT = (255, 168, 76)


def rounded_mask(size: int, radius: int) -> Image.Image:
    m = Image.new("L", (size, size), 0)
    ImageDraw.Draw(m).rounded_rectangle([0, 0, size - 1, size - 1], radius, fill=255)
    return m


def gradient(size: int) -> Image.Image:
    g = Image.new("RGB", (1, size))
    px = g.load()
    for y in range(size):
        t = y / (size - 1)
        px[0, y] = tuple(int(a + (b - a) * t) for a, b in zip(BG_TOP, BG_BOTTOM))
    return g.resize((size, size))


def build() -> Image.Image:
    # macOS 的图标有留白，实际画布约占 824/1024
    pad = int(N * 0.10)
    inner = N - pad * 2

    canvas = Image.new("RGBA", (N, N), (0, 0, 0, 0))
    body = gradient(inner).convert("RGBA")
    body.putalpha(rounded_mask(inner, int(inner * 0.225)))
    canvas.paste(body, (pad, pad), body)

    d = ImageDraw.Draw(canvas)
    cx, cy = N // 2, N // 2
    r = inner * 0.30
    w = int(inner * 0.055)

    # 耳廓：一段接近整圆的粗弧，缺口朝右下，读起来像 ear/听
    d.arc(
        [cx - r, cy - r, cx + r, cy + r],
        start=200, end=110,
        fill=FG, width=w,
    )
    # 耳垂：把弧尾收进中心，避免在小尺寸下看着像个断开的 C
    ex = cx + r * math.cos(math.radians(110))
    ey = cy + r * math.sin(math.radians(110))
    d.line([ex, ey, cx - r * 0.15, cy + r * 0.72], fill=FG, width=w)

    # 耳道：一个实心点，给中心一个视觉落点
    dot = inner * 0.052
    d.ellipse([cx - dot, cy - dot, cx + dot, cy + dot], fill=ACCENT)

    # 声波：左侧两道弧，暗示「正在听」
    for i, k in enumerate((0.52, 0.72)):
        rr = r * (1 + k)
        d.arc(
            [cx - rr, cy - rr, cx + rr, cy + rr],
            start=142, end=218,
            fill=ACCENT if i == 0 else (*ACCENT, 150),
            width=int(w * 0.72),
        )

    return canvas.resize((S, S), Image.LANCZOS)


def main() -> None:
    icon = build()
    OUT.parent.mkdir(parents=True, exist_ok=True)

    with tempfile.TemporaryDirectory() as tmp:
        iconset = pathlib.Path(tmp) / "AgentEar.iconset"
        iconset.mkdir()
        # iconutil 要求的固定文件名集合，少一个都会报错
        for px in (16, 32, 64, 128, 256, 512):
            icon.resize((px, px), Image.LANCZOS).save(iconset / f"icon_{px}x{px}.png")
            icon.resize((px * 2, px * 2), Image.LANCZOS).save(
                iconset / f"icon_{px}x{px}@2x.png"
            )
        subprocess.run(
            ["iconutil", "-c", "icns", str(iconset), "-o", str(OUT)], check=True
        )

    print(f"✅ {OUT.relative_to(ROOT)}  ({OUT.stat().st_size // 1024} KB)")


if __name__ == "__main__":
    main()
