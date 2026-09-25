#!/bin/bash
# T3.5.1：两个 ASR 引擎的冷启动 / RTF / 峰值 RSS。每格连跑 N 次（默认 3）。
#
# 直接调两个 CLI（参数与产品一致，见 src/asr.rs 与 src/engine.rs），
# 不经 agentear 包一层——要测的是**引擎进程自己**的 RSS。
# 每次都是一个新进程，所以每一行都含模型加载：这就是产品的实际形态
# （SenseVoice 每次录音 Command::new 一次；speech_swift 同样每次起一个 speech 进程）。
#
# 用法：
#   scripts/asr-zhen-perf.sh <vendor 目录> <输出文件> <wav>... [-- 次数]
#
# 输出每行：engine wav 时长s 墙钟s RTF 峰值RSS(MiB)
# 峰值 RSS 取自 /usr/bin/time -l 的 "maximum resident set size"（macOS 单位是字节）。
#
# ⚠️ 本仓库有一条前车之鉴：sandbox 里 Metal 着色器缓存写不进去，whisper 时延
# 因此慢了 20 倍（CLAUDE.md「M3 这轮的诚实边界」第 5 条）。本脚本不判断环境，
# 数字异常时要先怀疑环境，再怀疑模型。
set -euo pipefail

if [ $# -lt 3 ]; then
    sed -n 2,16p "$0"
    exit 2
fi
VENDOR=$1; OUT=$2; shift 2
REPS=3
WAVS=()
while [ $# -gt 0 ]; do
    if [ "$1" = "--" ]; then REPS=$2; shift 2; continue; fi
    WAVS+=("$1"); shift
done

SV_BIN="${VENDOR}/bin/llama-funasr-sensevoice"
SV_MODEL="${VENDOR}/models/sensevoice-small-q8.gguf"
SV_VAD="${VENDOR}/models/fsmn-vad.gguf"
for f in "$SV_BIN" "$SV_MODEL" "$SV_VAD"; do
    [ -e "$f" ] || { echo "缺 ${f}" >&2; exit 1; }
done
command -v speech >/dev/null || { echo "PATH 里没有 speech" >&2; exit 1; }

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

# 跑一次：$1=引擎名 $2=wav，其余是命令
measure() {
    local eng=$1 wav=$2; shift 2
    local dur t0 t1 wall rss rtf
    dur=$(afinfo "$wav" | awk '/estimated duration/ {print $3}')
    [ -n "$dur" ] || { echo "读不到时长：${wav}" >&2; exit 1; }
    t0=$(python3 -c 'import time;print(time.time())')
    /usr/bin/time -l "$@" >"$TMP/stdout" 2>"$TMP/stderr" || {
        echo "${eng} 在 ${wav} 上失败：" >&2; tail -20 "$TMP/stderr" >&2; exit 1; }
    t1=$(python3 -c 'import time;print(time.time())')
    rss=$(awk '/maximum resident set size/ {print $1}' "$TMP/stderr")
    [ -n "$rss" ] || { echo "time -l 没给出 RSS" >&2; exit 1; }
    # 结果必须非空：空转写说明引擎其实没干活，这一行时延不能算数
    if [ ! -s "$TMP/stdout" ]; then echo "${eng} 在 ${wav} 上 stdout 为空" >&2; exit 1; fi
    # 每个命令替换**单独赋值**再校验格式，不塞进 printf 的参数位：
    # 参数位里的失败不会触发 set -e，会打出一行空字段的「测量」（docs/data/README.md 记过）
    local base rss_mib
    wall=$(python3 -c "print(f'{$t1-$t0:.3f}')")
    rtf=$(python3 -c "print(f'{($t1-$t0)/$dur:.4f}')")
    rss_mib=$(python3 -c "print(f'{$rss/1048576:.0f}')")
    base=$(basename "$wav")
    for v in "$wall" "$rtf" "$rss_mib"; do
        [[ "$v" =~ ^[0-9]+(\.[0-9]+)?$ ]] || { echo "非数字测量值：${v}" >&2; exit 1; }
    done
    printf '%s\t%s\t%s\t%s\t%s\t%s\n' "$eng" "$base" "$dur" "$wall" "$rtf" "$rss_mib" | tee -a "$OUT"
}

printf 'engine\twav\tdur_s\twall_s\trtf\tpeak_rss_mib\n' > "$OUT"
for r in $(seq 1 "$REPS"); do
    for w in "${WAVS[@]}"; do
        measure sensevoice "$w" "$SV_BIN" -m "$SV_MODEL" --vad "$SV_VAD" -a "$w" --keep-tags
        for m in 1.7B 0.6B; do
            measure "qwen3-${m}" "$w" speech transcribe --engine qwen3 -m "$m" -- "$w"
        done
    done
done
echo "✓ ${OUT}" >&2
