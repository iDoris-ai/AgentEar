#!/usr/bin/env bash
# 备好「通话」这一档需要的两个模型与一个 Python 环境。
#
# 与 M2 的 scripts/setup-llm.sh 是同一个套路，但换了两件事：
#   1. LLM 换成面壁 MiniCPM5-2B 的 4bit MLX 档（2B，端侧）
#   2. TTS 从 macOS `say` 换成 VoxCPM2-4bit（ADR-0007 §6 唯一能说泰语的候选）
#
# 两个模型都**按需下载、不随包分发**（jason 2026-08-22 拍板的那条规矩）。
#
# 用法：scripts/setup-talk.sh [目录]     默认 ~/.agentear/talk
#   跑完用 scripts/serve-talk-llm.sh 和 scripts/serve-tts.sh 启动
#
# 环境变量：
#   AGENTEAR_TALK_VENV   复用哪个 venv（默认 ~/.agentear/llm/venv，见下）
#   AGENTEAR_TALK_DIR    模型与清单放哪（默认 ~/.agentear/talk）

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TALK_DIR="${1:-${AGENTEAR_TALK_DIR:-$HOME/.agentear/talk}}"
MODELS="$TALK_DIR/models"
LLM_MODEL_DIR="$MODELS/minicpm5-2b-4bit"
TTS_MODEL_DIR="$MODELS/voxcpm2-4bit"

LLM_REPO="${AGENTEAR_TALK_LLM_REPO:-mlx-community/MiniCPM5-2B-mlx-4Bit}"
TTS_REPO="${AGENTEAR_TALK_TTS_REPO:-mlx-community/VoxCPM2-4bit}"

die() { echo "!! $*" >&2; exit 1; }
note() { echo "==> $*"; }

# ---------------------------------------------------------------- Python 环境
#
# ⚠️ **优先复用 M2 那个 venv，不另起一套。**
# 实测（2026-09-14）~/.agentear/llm/venv 里同时有 mlx-lm 与 mlx-audio，
# 一个环境能同时跑 LLM 边车和 TTS 边车——这正是 ADR-0002 说的
# 「外部推理服务可以有 Python，但必须是独立进程」的落地形态。
#
# 找不到就新建一个。**不要 `uv venv` 撞运气**：setup-llm.sh 依赖 uv，
# 而这台机器上 uv 根本没装（2026-09-14 实测），照抄会让新用户第一步就失败。
VENV="${AGENTEAR_TALK_VENV:-$HOME/.agentear/llm/venv}"

venv_ok() {  # venv_ok <目录>
  [ -x "$1/bin/python" ] || return 1
  "$1/bin/python" -c 'import mlx_lm, mlx_audio' >/dev/null 2>&1
}

if venv_ok "$VENV"; then
  note "复用 Python 环境 $VENV"
else
  NEW_VENV="$TALK_DIR/venv"
  if ! venv_ok "$NEW_VENV"; then
    note "没有可用的环境，新建 $NEW_VENV"
    PY=""
    for candidate in python3.12 python3.13 python3.14 python3.11; do
      command -v "$candidate" >/dev/null && { PY="$candidate"; break; }
    done
    [ -n "$PY" ] || die "找不到 Python 3.11+（mlx-lm / mlx-audio 的最低要求）"
    mkdir -p "$TALK_DIR"
    [ -d "$NEW_VENV" ] || "$PY" -m venv "$NEW_VENV"
    "$NEW_VENV/bin/python" -m pip install -q --upgrade pip
    "$NEW_VENV/bin/python" -m pip install -q mlx-lm mlx-audio huggingface_hub
  fi
  VENV="$NEW_VENV"
fi

"$VENV/bin/python" -c 'import mlx_lm, mlx_audio' \
  || die "$VENV 里缺 mlx-lm 或 mlx-audio，自己装一下，或用 AGENTEAR_TALK_VENV 指一个装好的"

# ------------------------------------------------------------------ 下载
#
# ⚠️ **用 `env -u HF_ENDPOINT`，不要抄 setup-llm.sh 的 curl 逐文件那一套。**
# FU-15 已经记过：本机 huggingface_hub 报 LocalEntryNotFoundError 的根因
# 就是 HF_ENDPOINT 镜像变量，去掉它官方路径直接可用。两个模型各 1–2 GB，
# 不是 setup-llm.sh 那种 8 GB 分片，`hf download` 的续传够用。
#
# 但**仍然钉 revision**：模型目录必须能追溯到确切一次提交，
# 否则「同一份权重」这句话没有依据（照 setup-llm.sh 的理由）。
fetch_repo() {  # fetch_repo <repo> <目标目录> <必需文件名...>
  local repo="$1" dest="$2"; shift 2
  mkdir -p "$dest"
  echo "    $repo -> $dest"
  env -u HF_ENDPOINT HF_HUB_DISABLE_TELEMETRY=1 \
    "$VENV/bin/hf" download "$repo" --local-dir "$dest" >/dev/null \
    || die "$repo 下载失败（网络或 HF_ENDPOINT，确认能连 huggingface.co）"
  local missing=()
  for name in "$@"; do
    [ -s "$dest/$name" ] || missing+=("$name")
  done
  [ ${#missing[@]} -eq 0 ] || die "$repo 少了必需文件：${missing[*]}"
  # 记下提交号，别只写「已下载」
  local rev
  rev="$(curl -sfL --max-time 60 "https://huggingface.co/api/models/${repo}" \
    | "$VENV/bin/python" -c 'import json,sys; print(json.load(sys.stdin)["sha"])' 2>/dev/null || echo unknown)"
  printf 'repo=%s\nrevision=%s\nfetched=%s\n' "$repo" "$rev" "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
    > "$dest/.installed"
  echo "    revision ${rev}"
}

note "下载 LLM：面壁 MiniCPM5-2B（4bit MLX，HF 计数 1.4 GB / du 显示 1.3G）"
fetch_repo "$LLM_REPO" "$LLM_MODEL_DIR" \
  config.json model.safetensors tokenizer.json tokenizer_config.json

note "下载 TTS：VoxCPM2（4bit MLX，HF 计数 2.3 GB / du 显示 2.1G）"
fetch_repo "$TTS_REPO" "$TTS_MODEL_DIR" \
  config.json model.safetensors tokenizer.json tokenizer_config.json

# 体积守卫：中断留下的残缺目录不该被当成可用模型（照 setup-llm.sh）。
LLM_MB="$(du -sm "$LLM_MODEL_DIR" | cut -f1)"
TTS_MB="$(du -sm "$TTS_MODEL_DIR" | cut -f1)"
[ "$LLM_MB" -gt 1000 ] || die "LLM 目录只有 ${LLM_MB} MB，没下完"
[ "$TTS_MB" -gt 1500 ] || die "TTS 目录只有 ${TTS_MB} MB，没下完"

note "完成"
echo "    环境   $VENV"
echo "    LLM    $LLM_MODEL_DIR (${LLM_MB} MB)   $LLM_REPO"
echo "    TTS    $TTS_MODEL_DIR (${TTS_MB} MB)   $TTS_REPO"
echo
echo "    启动 LLM 边车：scripts/serve-talk-llm.sh"
echo "    启动 TTS 边车：scripts/serve-tts.sh"
echo "    端到端自测：  scripts/talk-e2e.sh --text '今天天气怎么样'"
