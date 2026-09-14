#!/usr/bin/env bash
# 启动「通话」这一档的 LLM 边车：面壁 MiniCPM5-2B（4bit MLX），OpenAI 兼容。
#
# ⚠️ **模型是可换的，换模型只改配置，不改代码。** 这个脚本读：
#   AGENTEAR_TALK_LLM_MODEL   模型目录或 HF repo id（默认 ~/.agentear/talk/models/minicpm5-2b-4bit）
#   AGENTEAR_TALK_LLM_PORT    端口（默认 8794）
#   AGENTEAR_TALK_VENV        venv（默认 ~/.agentear/llm/venv）
# AgentEar 那边只按 `talk_llm_url` 连，不关心谁把服务起起来的
# （ADR-0002 §8 / CLAUDE.md 的「连接优先、拉起兜底」）。
#
# 端口为什么是 8794 而不是 8793：8793 是 M2 理解层那个 9B 边车的，
# 两者不同模型、不同用途，端口撞了会出现「连上别人的服务还把回答当成
# 自己的结果」——serve-llm.sh 记过这个坑（2026-09-02 实测）。

set -euo pipefail

TALK_DIR="${AGENTEAR_TALK_DIR:-$HOME/.agentear/talk}"
VENV="${AGENTEAR_TALK_VENV:-$HOME/.agentear/llm/venv}"
MODEL="${AGENTEAR_TALK_LLM_MODEL:-$TALK_DIR/models/minicpm5-2b-4bit}"
PORT="${AGENTEAR_TALK_LLM_PORT:-8794}"

[ -x "$VENV/bin/mlx_lm.server" ] || { echo "!! 环境没备好，先跑 scripts/setup-talk.sh" >&2; exit 1; }
if [ ! -f "$MODEL/config.json" ] && [ ! -d "$MODEL" ]; then
  echo "!! 找不到模型 $MODEL，先跑 scripts/setup-talk.sh" >&2; exit 1
fi

# 端口被占就**明确失败**：悄悄换端口会让客户端连不上却以为边车没起；
# 更糟的是连到占用者身上，把别人的回答当成本地模型的输出。
if lsof -nP -iTCP:"$PORT" -sTCP:LISTEN >/dev/null 2>&1; then
  echo "!! 端口 ${PORT} 已被占用：" >&2
  lsof -nP -iTCP:"$PORT" -sTCP:LISTEN | sed 's/^/   /' >&2
  echo "   换一个：AGENTEAR_TALK_LLM_PORT=8795 scripts/serve-talk-llm.sh" >&2
  exit 1
fi

# ---------------------------------------------------------------------------
# `--chat-template-args '{"enable_thinking": false}'` 不是可选项。
#
# MiniCPM5 的 chat template 会在 assistant 开头插一个 `<think>` 块，
# 默认**先吐一段推理再给答案**（实测 2026-09-14：问「今天天气怎么样」，
# 输出先是「首先，我需要确认用户的问题是关于天气的。接下来……」）。
# 通话形态下这段话有三个害处：
#   1. 直接进 TTS 会被念出来，用户听到的是一段内心独白
#   2. 多几百毫秒到几秒的首字延迟，而 V1 的打断阈值是 300 ms
#   3. 它占掉 max-tokens 预算，答案可能被截断
# 模板里 `enable_thinking is false` 会写成空的 `<think>\n\n</think>\n\n`，
# 模型因此不进入推理模式。M2 那次对 Ornith 用的是 `--no-thinking`，
# 同一个道理，只是这个模型用模板参数表达。
# ---------------------------------------------------------------------------
exec "$VENV/bin/mlx_lm.server" \
  --model "$MODEL" \
  --host 127.0.0.1 \
  --port "$PORT" \
  --chat-template-args '{"enable_thinking": false}' \
  --temp "${AGENTEAR_TALK_TEMP:-0.3}" \
  --max-tokens "${AGENTEAR_TALK_MAX_TOKENS:-200}"
