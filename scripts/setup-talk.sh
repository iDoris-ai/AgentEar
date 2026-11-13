#!/usr/bin/env bash
# 备好「通话」这一档需要的两个模型与一个 Python 环境。
#
# 与 M2 的 scripts/setup-llm.sh 是同一个套路，但换了两件事：
#   1. LLM 换成面壁 MiniCPM5-2B 的 4bit MLX 档（2B，端侧）
#   2. TTS 从 macOS `say` 换成 VoxCPM2-4bit（ADR-0007 §6 唯一能说泰语的候选）
#
# 两个模型都**按需下载、不随包分发**（jason 2026-08-22 拍板的那条规矩）。
#
# 用法：scripts/setup-talk.sh [目录] [--tts-quant 4bit|8bit]
#   跑完用 scripts/serve-talk-llm.sh 和 scripts/serve-tts.sh 启动
#
# 环境变量：
#   AGENTEAR_TALK_VENV   复用哪个 venv（默认 ~/.agentear/llm/venv，见下）
#   AGENTEAR_TALK_DIR    模型与清单放哪（默认 ~/.agentear/talk）
#   AGENTEAR_TTS_QUANT   TTS 量化档，默认 4bit（见下）
#
# ## ⚠️ 默认必须是 4bit，这不是随手定的
#
# 权重体积（HF 实测 2026-09-14）：**4bit 2.30 GB / 8bit 3.22 GB**；
# 边车进程峰值 RSS：**4bit 约 2.4–2.5 GB / 8bit 约 3.3 GB**。
# 发布的普通人电脑内存没这么大，**所以默认档只能是 4bit**。
# 8bit 是给「内存宽裕、自己显式要」的人用的：
#   AGENTEAR_TTS_QUANT=8bit scripts/setup-talk.sh
#   AGENTEAR_TTS_QUANT=8bit scripts/serve-tts.sh
# ⚠️ **实测没有检出 4bit 与 8bit 的输出质量差异**（见 ADR-0007 §6 与
# benchmarks-talk.md）：花掉的那 1 GB 内存**目前买不到可测的音质**。
# 想要更好的音色，先试「换参考音频」那条路（services/tts/make_voice.py）。
#
# ## ⚠️ 同时装一份**默认音色库**（这不是可选项）
#
# VoxCPM2 是**零样本克隆**：不给参考音频就**每次合成随机换一个说话人**
# （边车自己会告警：实测 F0 极差 65%、音量差 4.5 倍）。而 v0.10.0 的
# 句子级流水线让一次回答切成好几句、每句各发一次请求 ——
# 没有参考音频就是**一句话里换好几个人**在说。
# 所以这里把仓库里那两条实测挑过的参考音频（`assets/talk-voices/`，
# **VoxCPM2 自举生成的**，1.9 MB）装进数据目录，让开箱就有稳定音色。

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# 参数解析：位置参数是目录，`--tts-quant` 是档位。手写不引 getopts——
# 仓库里所有脚本都是这个形状，保持一致。
TALK_DIR_ARG=""
QUANT="${AGENTEAR_TTS_QUANT:-4bit}"
while [ "$#" -gt 0 ]; do
  case "$1" in
    --tts-quant) QUANT="${2:-}"; shift 2 ;;
    --tts-quant=*) QUANT="${1#--tts-quant=}"; shift ;;
    -h|--help) sed -n '2,30p' "$0"; exit 0 ;;
    -*) echo "!! 不认识的参数：$1" >&2; exit 2 ;;
    *) TALK_DIR_ARG="$1"; shift ;;
  esac
done
case "$QUANT" in
  4bit|8bit) ;;
  *) echo "!! --tts-quant 只认 4bit / 8bit，收到：$QUANT" >&2; exit 2 ;;
esac

TALK_DIR="${TALK_DIR_ARG:-${AGENTEAR_TALK_DIR:-$HOME/.agentear/talk}}"
MODELS="$TALK_DIR/models"
LLM_MODEL_DIR="$MODELS/minicpm5-2b-4bit"
TTS_MODEL_DIR="$MODELS/voxcpm2-$QUANT"

LLM_REPO="${AGENTEAR_TALK_LLM_REPO:-mlx-community/MiniCPM5-2B-mlx-4Bit}"
TTS_REPO="${AGENTEAR_TALK_TTS_REPO:-mlx-community/VoxCPM2-$QUANT}"

# ⚠️ T3.5.5：模型一律钉 commit（完整 40 位），不再下 main 的最新。
# 这几个 commit 是 jason 机器上实测验证过的那一份（2026-09-15 起用，
# 上游此后没有再动过，HF HEAD 与之一致，2026-11-10 复核）。
# 升级 = 改这里 + 重跑 talk-e2e + 更新 docs/upstream-pins.md。
LLM_REV_DEFAULT="86ad5182c5a8532a4681ab972011d34c3b89477a"
case "$QUANT" in
  4bit) TTS_REV_DEFAULT="dc9e5c187858da5f4a13dc4c247e297339216381" ;;
  8bit) TTS_REV_DEFAULT="d52725898a0675703f7f9ddc5a4d1a3cdbb99032" ;;
esac
# 改了仓库就不能再用上面的 commit（那是另一个仓库的提交号）：
# 必须同时用 AGENTEAR_TALK_*_REV 给出自己的 commit，不许退回下载最新。
LLM_REV="${AGENTEAR_TALK_LLM_REV:-}"
TTS_REV="${AGENTEAR_TALK_TTS_REV:-}"
if [ -z "$LLM_REV" ]; then
  [ "$LLM_REPO" = "mlx-community/MiniCPM5-2B-mlx-4Bit" ] \
    || { echo "!! 改了 AGENTEAR_TALK_LLM_REPO 就必须同时给 AGENTEAR_TALK_LLM_REV（完整 commit）" >&2; exit 2; }
  LLM_REV="$LLM_REV_DEFAULT"
fi
if [ -z "$TTS_REV" ]; then
  [ "$TTS_REPO" = "mlx-community/VoxCPM2-$QUANT" ] \
    || { echo "!! 改了 AGENTEAR_TALK_TTS_REPO 就必须同时给 AGENTEAR_TALK_TTS_REV（完整 commit）" >&2; exit 2; }
  TTS_REV="$TTS_REV_DEFAULT"
fi
for rev in "$LLM_REV" "$TTS_REV"; do
  case "$rev" in
    [0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f]*) [ "${#rev}" -eq 40 ] || { echo "!! revision 必须是完整 40 位 commit：$rev" >&2; exit 2; } ;;
    *) echo "!! revision 必须是完整 40 位 commit：$rev" >&2; exit 2 ;;
  esac
done

# pip 依赖钉版本（2026-11-10 取自实测在用的 ~/.agentear/llm/venv）。
# 不钉的话「重装一次环境」会悄悄换一套 mlx，而 mlx 的行为变化（stream、
# 缓存 API 弃用）这个仓库已经踩过。升级 = 改这里 + 重跑 talk-e2e。
PIP_PINS=(mlx==0.32.2 mlx-lm==0.31.3 mlx-audio==0.5.1 huggingface_hub==1.29.0)

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
    # ⚠️ **挑解释器要真的验版本，不能只看名字存不存在。**
    #
    # 踩点（jason 这台机器，2026-09-15）：他的默认 `python3` 是 **pyenv 的 3.11.9**
    # （`~/.pyenv/version`），但 **pyenv 只在交互式 shell 里生效**——
    # 从 launchd / GUI / 非交互 shell 启动时 `python3` 落到
    # `/usr/bin/python3` = **Xcode 的 3.9.6**，而 mlx 要 3.11+，
    # 那个解释器连 `import mlx_lm` 都做不到。
    # 所以：`python3` 也进候选，但**必须过版本闸**；
    # 而下面这些带版本号的名字也一律验一遍，名字不等于版本。
    #
    # ⚠️ 顺序保持原样（3.12 优先）。**3.14 在清单里但未验证**——
    # mlx 的 wheel 覆盖到哪一版要现查；我们实测在用的是 **3.11.15**
    # （`~/.agentear/llm/venv`，uv 装的）。真在只有 3.14 的机器上装失败，
    # 先怀疑这里，别怀疑模型。
    PY=""
    for candidate in python3.12 python3.13 python3.14 python3.11 python3; do
      command -v "$candidate" >/dev/null || continue
      # `python3 -c` 里判版本：避免依赖 `python3 -V` 的输出格式
      if "$candidate" -c 'import sys; raise SystemExit(0 if sys.version_info >= (3, 11) else 1)' 2>/dev/null; then
        PY="$candidate"
        break
      fi
      echo "   跳过 ${candidate}（$("$candidate" -V 2>&1) 低于 3.11）"
    done
    [ -n "$PY" ] || die "找不到 Python 3.11+（mlx-lm / mlx-audio 的最低要求）；
   注意 pyenv 之类只在交互式 shell 里生效——非交互环境下先 export PATH，或直接用绝对路径"
    mkdir -p "$TALK_DIR"
    [ -d "$NEW_VENV" ] || "$PY" -m venv "$NEW_VENV"
    # FU-24：pip 本身也钉版本（2026-11-13 取自本机实测在用的 26.1.1），不再 --upgrade 到最新。
    "$NEW_VENV/bin/python" -m pip install -q pip==26.1.1
    "$NEW_VENV/bin/python" -m pip install -q -c "$ROOT/scripts/pins/venv-constraints.txt" "${PIP_PINS[@]}"
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
fetch_repo() {  # fetch_repo <repo> <钉死的 revision> <目标目录> <清单> <清单对应的默认 revision> <必需文件名...>
  local repo="$1" rev="$2" dest="$3" manifest="$4" default_rev="$5"; shift 5
  mkdir -p "$dest"
  echo "    $repo @ ${rev} -> $dest"
  env -u HF_ENDPOINT HF_HUB_DISABLE_TELEMETRY=1 \
    "$VENV/bin/hf" download "$repo" --revision "$rev" --local-dir "$dest" >/dev/null \
    || die "$repo 下载失败（网络或 HF_ENDPOINT，确认能连 huggingface.co）"
  local missing=()
  for name in "$@"; do
    [ -s "$dest/$name" ] || missing+=("$name")
  done
  [ ${#missing[@]} -eq 0 ] || die "$repo 少了必需文件：${missing[*]}"
  # FU-24：逐文件 sha256 校验（清单 scripts/pins/*.sha256，大文件取自 HF API lfs.oid）。
  # 清单只对默认 revision 有效；用 AGENTEAR_TALK_*_REV 覆盖时无从校验，明说跳过。
  if [ "$rev" = "$default_rev" ]; then
    echo "    校验 sha256（$(grep -vc '^#' "$manifest") 个文件）"
    (cd "$dest" && grep -v '^#' "$manifest" | shasum -a 256 -c --quiet) \
      || die "$repo 的 sha256 与钉死的清单不符（镜像被改动或下载损坏）。删掉 $dest 重跑"
  else
    echo "!! $repo revision 被覆盖为 ${rev}，跳过 sha256 清单校验" >&2
  fi
  # 记下提交号（就是钉死的那个，不再事后去问 API——问到的是 HEAD，不一定是下的那份）
  printf 'repo=%s\nrevision=%s\nfetched=%s\n' "$repo" "$rev" "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
    > "$dest/.installed"
  echo "    revision ${rev}"
}

note "下载 LLM：面壁 MiniCPM5-2B（4bit MLX，HF 计数 1.4 GB / du 显示 1.3G）"
fetch_repo "$LLM_REPO" "$LLM_REV" "$LLM_MODEL_DIR" \
  "$ROOT/scripts/pins/minicpm5-2b-4bit.sha256" "$LLM_REV_DEFAULT" \
  config.json model.safetensors tokenizer.json tokenizer_config.json

note "下载 TTS：VoxCPM2（${QUANT} MLX）"
fetch_repo "$TTS_REPO" "$TTS_REV" "$TTS_MODEL_DIR" \
  "$ROOT/scripts/pins/voxcpm2-${QUANT}.sha256" "$TTS_REV_DEFAULT" \
  config.json model.safetensors tokenizer.json tokenizer_config.json

# 体积守卫：中断留下的残缺目录不该被当成可用模型（照 setup-llm.sh）。
LLM_MB="$(du -sm "$LLM_MODEL_DIR" | cut -f1)"
TTS_MB="$(du -sm "$TTS_MODEL_DIR" | cut -f1)"
[ "$LLM_MB" -gt 1000 ] || die "LLM 目录只有 ${LLM_MB} MB，没下完"
# 下界按档位定：4bit 权重 2.30 GB、8bit 3.22 GB（都按 du 计）。
# ⚠️ 这里**不能只写一个数**——8bit 的守卫要是沿用 4bit 的下界，
# 一个只下了一半的 8bit 目录（1.6 GB）会被当成完整的放过去。
case "$QUANT" in
  4bit) TTS_MIN_MB=1500 ;;
  8bit) TTS_MIN_MB=2400 ;;
esac
[ "$TTS_MB" -ge "$TTS_MIN_MB" ] || die "TTS（${QUANT}）目录只有 ${TTS_MB} MB（应 ≥ ${TTS_MIN_MB}），没下完"

# ------------------------------------------------------------ 默认音色库
#
# 已存在就**不覆盖**：用户可能自己造过更好的（make_voice.py），
# 升级时把他的音色顶掉是最讨厌的一类行为。
VOICES_DIR="$TALK_DIR/voices"
if [ -n "$(ls -A "$VOICES_DIR" 2>/dev/null)" ]; then
  note "音色库已存在，保持不动：$VOICES_DIR"
else
  note "装默认音色库（VoxCPM2 自举生成的参考音频，1.9 MB）"
  mkdir -p "$VOICES_DIR"
  for f in "$ROOT/assets/talk-voices"/*.wav "$ROOT/assets/talk-voices"/*.json; do
    [ -f "$f" ] && cp "$f" "$VOICES_DIR/"
  done
fi
VOICE_COUNT="$(ls "$VOICES_DIR"/*.wav 2>/dev/null | wc -l | tr -d ' ')"
[ "$VOICE_COUNT" -gt 0 ] || die "音色库里一条参考音频都没有——没有它每句都会换说话人"

note "完成"
echo "    环境   $VENV"
echo "    LLM    $LLM_MODEL_DIR (${LLM_MB} MB)   $LLM_REPO"
echo "    TTS    $TTS_MODEL_DIR (${TTS_MB} MB)   $TTS_REPO  [${QUANT}]"
echo "    音色   $VOICES_DIR (${VOICE_COUNT} 条)"
echo
echo "    启动 LLM 边车：scripts/serve-talk-llm.sh"
if [ "$QUANT" != "4bit" ]; then
  echo
  echo "    ⚠️ 启边车时也要指同一档，否则它会去找 4bit 的目录："
  echo "       AGENTEAR_TTS_QUANT=$QUANT scripts/serve-tts.sh"
fi
echo "    启动 TTS 边车：scripts/serve-tts.sh"
echo "    端到端自测：  scripts/talk-e2e.sh --text '今天天气怎么样'"
