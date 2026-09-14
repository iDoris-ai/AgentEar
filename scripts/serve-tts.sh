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
MODEL="${AGENTEAR_TTS_MODEL:-$TALK_DIR/models/voxcpm2-4bit}"
PORT="${AGENTEAR_TTS_PORT:-8765}"
BACKEND="${AGENTEAR_TTS_BACKEND:-voxcpm2}"

[ -x "$VENV/bin/python" ] || { echo "!! 环境没备好，先跑 scripts/setup-talk.sh" >&2; exit 1; }

if [ "$BACKEND" = "voxcpm2" ] && [ ! -f "$MODEL/config.json" ]; then
  echo "!! 找不到 VoxCPM2 模型 $MODEL，先跑 scripts/setup-talk.sh" >&2
  echo "   或者用零依赖兜底：AGENTEAR_TTS_BACKEND=say scripts/serve-tts.sh" >&2
  exit 1
fi

if lsof -nP -iTCP:"$PORT" -sTCP:LISTEN >/dev/null 2>&1; then
  echo "!! 端口 ${PORT} 已被占用：" >&2
  lsof -nP -iTCP:"$PORT" -sTCP:LISTEN | sed 's/^/   /' >&2
  exit 1
fi

# HF_HUB_OFFLINE=1：模型已经在本地目录里，别在启动时去连网校验。
# 这个服务的隐私前提是「不经过任何第三方」，启动路径上也不该有外呼。
exec env PYTHONDONTWRITEBYTECODE=1 HF_HUB_OFFLINE=1 \
  "$VENV/bin/python" "$ROOT/services/tts/server.py" \
  --port "$PORT" --backend "$BACKEND" --model "$MODEL"
