#!/usr/bin/env bash
# 启动 TTS 边车：mlx-community/VoxCPM2-4bit（48 kHz）。
#
# 与 services/tts 的 HTTP 契约不变（POST /speak → audio/wav），
# 换的只是后端：`--backend voxcpm2` 而不是 `--backend say`。
# 想退回零依赖的 macOS `say`：AGENTEAR_TTS_BACKEND=say scripts/serve-tts.sh
#
# ⚠️ **必须用带 mlx-audio 的 venv 启动**，不是 /usr/bin/python3。
# 系统 Python 是 3.9，装不了 mlx（mlx 要求 3.11+），而且这个服务
# 是在**自己进程里**常驻权重的——用错解释器会在启动时就报清楚，
# 不会静默降级成 `say`。

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TALK_DIR="${AGENTEAR_TALK_DIR:-$HOME/.agentear/talk}"
VENV="${AGENTEAR_TALK_VENV:-$HOME/.agentear/llm/venv}"
# 量化档。**默认 4bit**（权重 2.30 GB、峰值 RSS 约 2.4–2.5 GB）——
# 发布的普通人电脑内存没这么大。8bit（3.22 GB / 约 3.3 GB）要显式要：
#   AGENTEAR_TTS_QUANT=8bit scripts/serve-tts.sh
# ⚠️ 换档之后**必须重起边车**：模型是在它自己进程里常驻的，
# 改环境变量对已经在跑的那个没有任何影响。
QUANT="${AGENTEAR_TTS_QUANT:-4bit}"
case "$QUANT" in
  4bit|8bit) ;;
  *) echo "!! AGENTEAR_TTS_QUANT 只认 4bit / 8bit，收到：$QUANT" >&2; exit 2 ;;
esac
MODEL="${AGENTEAR_TTS_MODEL:-$TALK_DIR/models/voxcpm2-$QUANT}"
# 音色库。**必须给它一个目录**，否则参考音频为空 →
# VoxCPM2 是零样本克隆，没有参考就**每次生成随机换一个说话人**
# （边车自己会告警：实测 F0 极差 65%、音量差 4.5 倍）。
# ⚠️ v0.10.0 的句子级流水线让这件事更严重：一次回答会切成好几句，
# 每句各发一次请求 —— 没有参考音频就是**每句换一个人**在说话。
# 默认指数据目录（与 Rust 侧 `config.voices_dir()` 同一处），
# 目录不存在时不传这个参数，让边车的告警照常出现。
VOICES_DIR="${AGENTEAR_TTS_VOICES_DIR:-$TALK_DIR/voices}"
# 数据目录里还没有的（比如刚 clone 出来、还没跑 setup-talk.sh），
# 回退到仓库里那份随包的音色库——**没有它每句都会换说话人**。
if [ ! -d "$VOICES_DIR" ] || [ -z "$(ls -A "$VOICES_DIR" 2>/dev/null)" ]; then
  if [ -n "$(ls -A "$ROOT/assets/talk-voices" 2>/dev/null)" ]; then
    echo "数据目录还没有音色库，先用仓库里那份：$ROOT/assets/talk-voices"
    VOICES_DIR="$ROOT/assets/talk-voices"
  fi
fi
# 钉哪一条。默认 `female_zh_02`（实测挑的那条：F0 174.5 Hz、半音起伏 4.22）。
# 只有它真在库里时才钉——否则一钉就是 400，用户听到的是「没声音」。
VOICE="${AGENTEAR_TTS_VOICE:-female_zh_02}"
PORT="${AGENTEAR_TTS_PORT:-8765}"
BACKEND="${AGENTEAR_TTS_BACKEND:-voxcpm2}"

[ -x "$VENV/bin/python" ] || { echo "!! 环境没备好，先跑 scripts/setup-talk.sh" >&2; exit 1; }

if [ "$BACKEND" = "voxcpm2" ] && [ ! -f "$MODEL/config.json" ]; then
  echo "!! 找不到 VoxCPM2 模型 $MODEL（档位 ${QUANT}）" >&2
  # 报错要说清**下一步跑哪条命令**，而且要带上档位——只说「先跑 setup-talk.sh」
  # 会让人拿着 8bit 的目录去下 4bit，然后还是起不来。
  echo "   下这一档：AGENTEAR_TTS_QUANT=$QUANT scripts/setup-talk.sh" >&2
  echo "   或换回默认档：scripts/serve-tts.sh" >&2
  echo "   或零依赖兜底：AGENTEAR_TTS_BACKEND=say scripts/serve-tts.sh" >&2
  exit 1
fi

if lsof -nP -iTCP:"$PORT" -sTCP:LISTEN >/dev/null 2>&1; then
  echo "!! 端口 ${PORT} 已被占用：" >&2
  lsof -nP -iTCP:"$PORT" -sTCP:LISTEN | sed 's/^/   /' >&2
  exit 1
fi

# 组装可选参数：音色库在才传，要钉的那条也在才钉。
EXTRA=()
if [ "$BACKEND" = "voxcpm2" ] && [ -d "$VOICES_DIR" ]; then
  EXTRA+=(--voices-dir "$VOICES_DIR")
  if [ -f "$VOICES_DIR/$VOICE.wav" ] && [ -f "$VOICES_DIR/$VOICE.json" ]; then
    EXTRA+=(--voice "$VOICE")
    echo "音色库 $VOICES_DIR，钉住 $VOICE"
  else
    echo "⚠️ 音色库里没有 $VOICE（$VOICES_DIR），交给边车选默认那条" >&2
  fi
else
  echo "⚠️ 没有音色库（$VOICES_DIR）→ 每次生成会随机换说话人。" >&2
  echo "   造一条参考音频：services/tts/make_voice.py --help   或指定 AGENTEAR_TTS_VOICES_DIR" >&2
fi

# HF_HUB_OFFLINE=1：模型已经在本地目录里，别在启动时去连网校验。
# 这个服务的隐私前提是「不经过任何第三方」，启动路径上也不该有外呼。
# bash 3.2 的 `${EXTRA[@]}` 在 `set -u` 下会炸（空数组算未定义），所以要 `+` 兜底。
exec env PYTHONDONTWRITEBYTECODE=1 HF_HUB_OFFLINE=1 \
  "$VENV/bin/python" "$ROOT/services/tts/server.py" \
  --port "$PORT" --backend "$BACKEND" --model "$MODEL" ${EXTRA[@]+"${EXTRA[@]}"}
