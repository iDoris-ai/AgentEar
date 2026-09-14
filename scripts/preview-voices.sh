#!/usr/bin/env bash
# 音色/语气候选集：生成一批音频，**依次播放给你听，你挑一条**。
#
# ## 为什么要有这个脚本
#
# 「像不像机器人、有没有感情」**没有客观判据**——我试过 F0 起伏（半音）、能量起伏，
# 结果发现**run-to-run 的波动和配置间差异一样大**：同一个配置两次实验给出
# 5.57 / 4.55 半音（n=2~3）。用这种量去"调优"音色等于在噪声里找信号。
#
# 而且这本来就是这个仓库定的规矩：**音色与自然度只能人耳验收**
# （`docs/benchmarks-m3.md` §6.3）。所以这里不替你判断，只把候选摆出来。
#
# ## 用法
#
#   scripts/preview-voices.sh              # 生成 + 逐个播放
#   scripts/preview-voices.sh --no-play    # 只生成，自己用 afplay 听
#
# 听完告诉我编号，我把它设成默认（改 `~/.agentear/config.json` 或服务启动参数）。

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TTS_URL="${AGENTEAR_TTS_URL:-http://127.0.0.1:8765}"
OUT="${AGENTEAR_PREVIEW_OUT:-$ROOT/vendor/models/talk/voices/preview}"
PLAY=1
[ "${1:-}" = "--no-play" ] && PLAY=0

mkdir -p "$OUT"
TS="$(date +%H%M%S)"
TEXT="${AGENTEAR_PREVIEW_TEXT:-今天天气怎么样？今天清迈天气还不错，最高三十二度，傍晚可能有阵雨。}"

# 候选 = 音色 × 语气 × 语系。**每组都标注清楚**，否则听完记不住哪个是哪个。
declare -a CASES=(
  "01_female_zh_02_warm_zh|female_zh_02|zh|warm"
  "02_female_zh_02_calm_zh|female_zh_02|zh|calm"
  "03_female_zh_02_lively_zh|female_zh_02|zh|lively"
  "04_female_zh_01_warm_zh|female_zh_01|zh|warm"
  "05_female_zh_02_warm_yue|female_zh_02|yue|warm"
  "06_female_zh_02_warm_henan|female_zh_02|henan|warm"
  "07_female_zh_02_warm_engb|female_zh_02|en-gb|warm"
  "08_no_voice_baseline||zh|warm"
)

echo "== 生成候选（文本：$TEXT）=="
for spec in "${CASES[@]}"; do
  IFS='|' read -r name voice style tone <<<"$spec"
  body=$(NAME="$name" VOICE="$voice" STYLE="$style" TONE="$tone" TEXT="$TEXT" python3 - <<'PY'
import json, os
p = {"text": os.environ["TEXT"], "lang": "zh", "style": os.environ["STYLE"], "tone": os.environ["TONE"]}
if os.environ["VOICE"]:
    p["voice"] = os.environ["VOICE"]
print(json.dumps(p, ensure_ascii=False))
PY
)
  file="$OUT/${TS}_${name}.wav"
  if curl -s --max-time 120 -o "$file" "$TTS_URL/speak" -H 'Content-Type: application/json' -d "$body"; then
    printf '  ✅ %-30s %s\n' "$name" "$file"
  else
    printf '  ❌ %-30s 失败（服务在跑吗？scripts/serve-tts.sh）\n' "$name"
  fi
done

echo
echo "== 怎么听 =="
echo "  目录：$OUT"
if [ "$PLAY" = "1" ]; then
  for f in "$OUT/${TS}"_*.wav; do
    echo "── $(basename "$f")"
    afplay "$f" || true
    sleep 0.4
  done
  echo
  echo "  听完了？告诉我编号（例如「03」），我把它设成默认。"
  echo "  ⚠️ 每条只生成一次，模型本身每次都在变——**某个候选难听也可能只是这一次运气差**，"
  echo "     拿不准就重跑一遍同一个编号再听。"
else
  echo "  没播放（--no-play）。逐个听："
  echo "    for f in $OUT/${TS}_*.wav; do afplay \"\$f\"; done"
fi
