#!/usr/bin/env bash
# ADR-0004 §3 的性能基线。复现 whisper 系泰语模型的体积 / RTF / 峰值 RSS。
#
# 前置：
#   1. whisper.cpp 已构建（build/bin/whisper-cli、build/bin/quantize）
#   2. GGML 模型已转好。转换见 ADR-0004 §2，注意 bf16 权重要先落成 fp32，
#      且 quantize **不校验输入**——转崩留下的残缺文件它照样量化成功。
#
# 用法：scripts/bench-thai.sh <ggml模型> [...]
#   环境变量 WHISPER_CPP 指定 whisper.cpp 仓库路径（默认与本仓库同级）
#
# ⚠️ 两条必须遵守的测量纪律，都是踩过坑换来的（ADR-0004 §3）：
#
#   1. **机器必须空闲。** 后台有下载/编译时测出的数字会系统性偏慢，
#      而且容易让人把干净的那次误判成脏的。测完复现一遍再信。
#   2. **长度档必须由互不重复的内容构成。** 同一句话重复拼接会触发 whisper
#      的重复检测和温度回退，墙钟翻几倍而输出长度不变，RTF 整列作废。
#      本脚本用 thai-corpus.txt 里 20 句不重复的句子拼长度档。
#
# 语料是 macOS 内置泰语嗓音 Kanya 合成的。**只能测性能，不能算 CER**——
# 没有口音、语速变化、噪声，准确率会显著优于真实表现。

set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WCPP="${WHISPER_CPP:-$ROOT/../whisper.cpp}"
CLI="$WCPP/build/bin/whisper-cli"
WORK="${AGENTEAR_BENCH_WORK:-${TMPDIR:-/tmp}/agentear-thai-bench}"
THREADS="${THREADS:-4}"

[ -x "$CLI" ] || { echo "找不到 whisper-cli：$CLI（设 WHISPER_CPP 指向 whisper.cpp 仓库）" >&2; exit 1; }
[ $# -gt 0 ] || { echo "用法：$0 <ggml模型> [...]" >&2; exit 1; }
command -v say >/dev/null || { echo "需要 macOS 的 say" >&2; exit 1; }
say -v '?' | grep -q '^Kanya' || { echo "缺泰语嗓音 Kanya（系统设置 → 辅助功能 → 语音里下载）" >&2; exit 1; }

C="$WORK/corpus"
if [ ! -f "$C/len-long.wav" ]; then
  echo "==> 合成语料到 $C"
  mkdir -p "$C"; i=0
  while IFS= read -r line; do
    [ -n "$line" ] || continue
    i=$((i+1)); n="$(printf 's%02d' $i)"
    say -v Kanya -o "$C/$n.aiff" "$line"
    ffmpeg -y -i "$C/$n.aiff" -ar 16000 -ac 1 "$C/$n.wav" -loglevel error
    rm -f "$C/$n.aiff"
  done < "$ROOT/scripts/thai-corpus.txt"
  build() {  # $1=名字 $2=取前几句
    ls "$C"/s*.wav | head -"$2" | sed "s|^|file '|;s|$|'|" > "$C/.concat"
    ffmpeg -y -f concat -safe 0 -i "$C/.concat" -ar 16000 -ac 1 "$C/$1.wav" -loglevel error
    rm -f "$C/.concat"
  }
  build len-short 2; build len-mid 6; build len-long "$i"
fi

dur() { ffprobe -v error -show_entries format=duration -of csv=p=0 "$1"; }
run() {  # $1=模型 $2=wav → "墙钟秒 峰值RSS字节"
  local o; o="$( { /usr/bin/time -l "$CLI" -m "$1" -f "$2" -l th -t "$THREADS" \
      -bo 1 -bs 1 -np -nt >/dev/null; } 2>&1 )"
  echo "$(awk '/ real/{print $1}' <<<"$o" | head -1) \
        $(grep -o '[0-9]*  *maximum resident' <<<"$o" | grep -o '^[0-9]*')"
}

DS=$(dur "$C/len-short.wav"); DM=$(dur "$C/len-mid.wav"); DL=$(dur "$C/len-long.wav")
printf '语料时长档：%.1fs / %.1fs / %.1fs，线程 %s\n\n' "$DS" "$DM" "$DL" "$THREADS"
printf '%-22s %7s %8s %8s %8s %9s %9s\n' 模型 体积MB 短-RTF 中-RTF 长-RTF 短-RSS 长-RSS
for m in "$@"; do
  [ -f "$m" ] || { echo "跳过（不存在）：$m" >&2; continue; }
  # 转换失败留下的残缺文件量化后仍是几 MB，挡在这里免得测出一堆没意义的数字
  sz=$(stat -f%z "$m")
  [ "$sz" -gt 100000000 ] || { echo "跳过（只有 $((sz/1000000)) MB，多半是转换失败的残件）：$m" >&2; continue; }
  read -r ts rs <<<"$(run "$m" "$C/len-short.wav")"
  read -r tm _  <<<"$(run "$m" "$C/len-mid.wav")"
  read -r tl rl <<<"$(run "$m" "$C/len-long.wav")"
  printf '%-22s %7d %8.3f %8.3f %8.3f %9d %9d\n' "$(basename "$m" .bin)" \
    $((sz/1000000)) "$(echo "$ts/$DS"|bc -l)" "$(echo "$tm/$DM"|bc -l)" \
    "$(echo "$tl/$DL"|bc -l)" $((rs/1000000)) $((rl/1000000))
done
