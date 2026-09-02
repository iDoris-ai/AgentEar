#!/usr/bin/env bash
# 启动 M2 理解层的 LLM 边车。
#
# 三个参数都不是随便选的，是 M0 基准踩出来的（docs/benchmarks-m2.md §5）：
#
#   --mode lookup    默认模式要求注册过的 drafter，本地模型路径没有内置的，
#                    必须用 lookup（drafter-free）或 auto
#   --no-thinking    Ornith 默认会先吐 `Thinking Process:` 再给答案。
#                    不关的话标签分类取不到纯类名（实测 0%），
#                    而术语纠错会变成**假阳性**——推理过程里自然会提到目标词，
#                    「目标词出现在输出里」这个判据就永远成立
#   不用 --kv-bits   与本模型不兼容
#
# 端口：**默认换成 8793**，不再用 benchmarks-m2.md 里那个 8791。
# 2026-09-02 实测 8791 被本机另一个项目的 node 服务占着，而后果比
# 「起不来」严重得多——AgentEar 会**连上别人的服务**，把对方的业务回答
# 当成术语纠错的结果粘到用户光标处。所以下面既检查端口占用，
# 客户端那边也要校验对端身份（见 src/correct.rs）。

set -euo pipefail

ENV_DIR="${AGENTEAR_LLM_DIR:-$HOME/.agentear/llm}"
VENV="$ENV_DIR/venv"
MODEL_DIR="$ENV_DIR/models/ornith"
PORT="${AGENTEAR_LLM_PORT:-8793}"

[ -x "$VENV/bin/mlx-dspark" ] || { echo "!! 环境没备好，先跑 scripts/setup-llm.sh" >&2; exit 1; }
[ -f "$MODEL_DIR/config.json" ] || { echo "!! 找不到模型，先跑 scripts/setup-llm.sh" >&2; exit 1; }

# 端口被占就**明确失败**，不要让它悄悄起在别的端口上，
# 更不要让客户端连到占用者身上去。
if lsof -nP -iTCP:"$PORT" -sTCP:LISTEN >/dev/null 2>&1; then
  echo "!! 端口 ${PORT} 已被占用：" >&2
  lsof -nP -iTCP:"$PORT" -sTCP:LISTEN | sed 's/^/   /' >&2
  echo "   换一个：AGENTEAR_LLM_PORT=8794 scripts/serve-llm.sh" >&2
  exit 1
fi

exec "$VENV/bin/mlx-dspark" serve \
  --model "$MODEL_DIR" \
  --mode lookup \
  --port "$PORT" \
  --context-window 32768 \
  --no-thinking
