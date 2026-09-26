#!/usr/bin/env bash
# 通话链路的端到端验收（T3.4.3）：一句问话进去，一段回答音频出来。
#
#   说话音频 → ASR → LLM（可换）→ TTS（VoxCPM2-4bit）→ 回答音频
#
# 用法：
#   scripts/talk-e2e.sh --text '今天天气怎么样'          # 用 say 合成问句，走完整链路
#   scripts/talk-e2e.sh --wav ask.wav --lang th          # 用真实录音
#   scripts/talk-e2e.sh --text '...' --play              # 顺便播出来（需要能出声）
#
# 前置：两个边车已经在跑
#   scripts/serve-talk-llm.sh     # LLM，默认 127.0.0.1:8794
#   scripts/serve-tts.sh          # TTS，默认 127.0.0.1:8796
#
# ⚠️ **天气那句话是本地写死的场景，不是真的天气接口**（ADR-0007 §4.6）。
# 它在整条链路里的作用是「让模型有东西可说」——证明 ASR→LLM→TTS 通了，
# 不是产品能力。接真实天气源是集成方的事（R3 外壳层）。

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
LLM_URL="${AGENTEAR_TALK_LLM_URL:-http://127.0.0.1:8794}"
TTS_URL="${AGENTEAR_TTS_URL:-http://127.0.0.1:8796}"
AGENTEAR_BIN="${AGENTEAR_BIN:-$ROOT/target/release/agentear}"
CITY="${AGENTEAR_TALK_CITY:-清迈}"
WEATHER_NOTE="${AGENTEAR_TALK_WEATHER_NOTE:-今天${CITY}多云转晴，最高 32 度，傍晚有阵雨，风不大。}"
OUT_DIR="${AGENTEAR_TALK_OUT:-$ROOT/vendor/models/talk/out/e2e}"

TEXT=""; WAV=""; LANG="zh"; PLAY=0
while [ $# -gt 0 ]; do
  case "$1" in
    --text) TEXT="$2"; shift 2 ;;
    --wav)  WAV="$2";  shift 2 ;;
    --lang) LANG="$2"; shift 2 ;;
    --play) PLAY=1;    shift ;;
    -h|--help) sed -n '2,20p' "$0"; exit 0 ;;
    *) echo "未知参数 $1" >&2; exit 2 ;;
  esac
done
[ -n "$TEXT" ] || [ -n "$WAV" ] || { echo "要么 --text，要么 --wav" >&2; exit 2; }
case "$LANG" in zh|en|th) ;; *) echo "--lang 只认 zh/en/th" >&2; exit 2 ;; esac

mkdir -p "$OUT_DIR"
step() { printf '\n\033[1m== %s\033[0m\n' "$*"; }

step "0. 边车自检"
for spec in "LLM $LLM_URL/health" "TTS $TTS_URL/health"; do
  name="${spec%% *}"; url="${spec#* }"
  if ! curl -sf --max-time 5 "$url" >/dev/null; then
    echo "!! $name 边车没在跑：$url" >&2
    [ "$name" = "LLM" ] && echo "   启动：scripts/serve-talk-llm.sh" >&2
    [ "$name" = "TTS" ] && echo "   启动：scripts/serve-tts.sh" >&2
    exit 1
  fi
  printf '   %s %s\n' "$name" "$(curl -sf --max-time 5 "$url" | head -c 200)"
done

# ⚠️ **问句文件名要带语言。** 曾经三种语言共用 `ask.wav`，
# 后一次运行会覆盖前一次的录音——于是拿一个泰语 wav 去跑 `--lang zh`
# 就得到一段乱码转写，而人只会以为「ASR 坏了」。
# 2026-09-14 实际踩到：`--talk-turn ask.wav --lang zh` 转出
# 「When你 I got bin right。」（那份 ask.wav 是上一轮泰语留下的）。
ASK="$OUT_DIR/ask_${LANG}.wav"
if [ -z "$WAV" ]; then
  step "1. 合成问句（say，只用来造输入音频）"
  case "$LANG" in
    zh) VOICE=Tingting ;; en) VOICE=Samantha ;; th) VOICE=Kanya ;;
  esac
  say -v "$VOICE" -o "$OUT_DIR/ask_${LANG}.aiff" "$TEXT"
  afconvert "$OUT_DIR/ask_${LANG}.aiff" "$ASK" -d LEI16@16000 -f WAVE -c 1
  echo "   $TEXT"
  echo "   -> $ASK"
else
  ASK="$WAV"
  step "1. 用现成音频"
  echo "   -> $ASK"
fi

step "2. ASR（走现有引擎，语言显式指定 $LANG）"
if [ ! -x "$AGENTEAR_BIN" ]; then
  echo "!! 找不到 $AGENTEAR_BIN，先 cargo build --release" >&2
  exit 1
fi
# ⚠️ `--lang` 只认 th / auto：中英走 SenseVoice 的默认路径，传 `zh` 会被
# 参数解析直接拒掉（2026-09-14 实测 exit 1），所以按语言决定加不加这个参数。
if [ "$LANG" = "th" ]; then
  ASR_ARGS=(--lang th)
else
  ASR_ARGS=()
fi
# `${ASR_ARGS[@]+...}` 这层壳是给 macOS 自带的 bash 3.2 用的：
# 空数组配 `set -u` 在 3.2 上会报 "unbound variable"（bash 4.4+ 才修）。
ASR_RAW="$("$AGENTEAR_BIN" --transcribe "$ASK" ${ASR_ARGS[@]+"${ASR_ARGS[@]}"} 2>&1)"
HEARD="$(printf '%s\n' "$ASR_RAW" | grep -v '^\[[0-9]' | grep -v '^（' | tail -1)"
[ -n "$HEARD" ] || { echo "!! ASR 没出文字：" >&2; printf '%s\n' "$ASR_RAW" >&2; exit 1; }
echo "   听到：$HEARD"

step "3. LLM（$LLM_URL，模型可换）"
SYS="你是 AgentEar 的语音助手，正在打电话。回答必须短：一到两句、不超过 40 个字，直接给结论，不要 markdown，不要列举。已知本地事实：$WEATHER_NOTE 用户问天气时就用这条事实回答。"
BODY="$(LANG="$LANG" SYS="$SYS" HEARD="$HEARD" python3 -c '
import json, os
print(json.dumps({
    "messages": [
        {"role": "system", "content": os.environ["SYS"]},
        {"role": "user", "content": os.environ["HEARD"]},
    ],
    "max_tokens": 160,
    "temperature": 0.3,
}, ensure_ascii=False))')"
REPLY="$(curl -sf --max-time 120 "$LLM_URL/v1/chat/completions" \
  -H 'Content-Type: application/json' -d "$BODY" \
  | python3 -c 'import json,sys; print(json.load(sys.stdin)["choices"][0]["message"]["content"].strip())')"
[ -n "$REPLY" ] || { echo "!! LLM 没给回答" >&2; exit 1; }
echo "   回答：$REPLY"

step "4. TTS（VoxCPM2-4bit）"
REPLY_WAV="$OUT_DIR/reply_${LANG}.wav"
TTS_BODY="$(LANG="$LANG" REPLY="$REPLY" python3 -c '
import json, os
print(json.dumps({"text": os.environ["REPLY"], "lang": os.environ["LANG"]}, ensure_ascii=False))')"
curl -sf --max-time 180 -o "$REPLY_WAV" "$TTS_URL/speak" \
  -H 'Content-Type: application/json' -d "$TTS_BODY" \
  || { echo "!! TTS 请求失败" >&2; exit 1; }
python3 - "$REPLY_WAV" <<'PY'
import sys, wave
with wave.open(sys.argv[1], "rb") as w:
    print(f"   {w.getframerate()} Hz / {w.getnchannels()} ch / {w.getnframes()/w.getframerate():.2f}s -> {sys.argv[1]}")
PY

if [ "$PLAY" = "1" ]; then
  step "5. 播放"
  afplay "$REPLY_WAV"
fi

step "结果"
printf '   问：%s\n   听成：%s\n   答：%s\n   语音：%s\n' "$TEXT$WAV" "$HEARD" "$REPLY" "$REPLY_WAV"
