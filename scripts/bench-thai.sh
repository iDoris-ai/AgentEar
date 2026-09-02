#!/usr/bin/env bash
# ADR-0004 §3 的**性能**测量：体积 / RTF / 峰值 RSS。
#
# 范围声明：本脚本只复现**性能测量**这一步。它不复现模型下载、GGML 转换和
# 量化——那三步见 ADR-0004 §2，得自己跑完把 .bin 准备好。
# **准确率不归它管**，看 `scripts/cer-thai.py`。
#
# 语料是 macOS 内置泰语嗓音 Kanya 合成的（`scripts/thai-corpus.txt`）。
# ⚠️ **那 20 句是本项目自己编写的，没有母语者校对，不是任何标准语料库。**
# 用它测性能是可以的——但也别把这说死：内容会通过解码 token 数、温度回退
# 等路径影响 RTF（下面第 2 条就是被这个坑过），声学特征也可能有影响。
# **绝不能用来算 CER**——参考文本本身就没有权威性。真实准确率走 FLEURS。
#
# 两条测量纪律，都是踩坑换来的（ADR-0004 §3）：
#   1. **机器必须空闲。** 后台有下载/编译时数字会系统性偏慢，而且容易把
#      干净的那次误判成脏的。测完复现一遍再信。
#   2. **长度档必须由互不重复的内容构成。** 同一句重复拼接会触发 whisper 的
#      重复检测和温度回退，墙钟翻几倍而输出长度不变，RTF 整列作废。
#
# 用法：scripts/bench-thai.sh [--regenerate] <ggml模型> [...]
#   WHISPER_CPP  whisper.cpp 仓库路径（默认与本仓库同级）
#   THREADS      线程数，默认 4
#   AGENTEAR_BENCH_WORK  工作目录，默认 $TMPDIR/agentear-thai-bench

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WCPP="${WHISPER_CPP:-$ROOT/../whisper.cpp}"
CLI="$WCPP/build/bin/whisper-cli"
WORK="${AGENTEAR_BENCH_WORK:-${TMPDIR:-/tmp}/agentear-thai-bench}"
THREADS="${THREADS:-4}"
CORPUS_TXT="$ROOT/scripts/thai-corpus.txt"
C="$WORK/corpus"
REGEN=0
[ "${1:-}" = "--regenerate" ] && { REGEN=1; shift; }

die() { echo "!! $*" >&2; exit 1; }

# —— 依赖检查。缺哪个都要现在就说清楚，别让 set -e 在半路抛个看不懂的错 ——
for c in ffmpeg ffprobe bc say shasum; do
  command -v "$c" >/dev/null || die "缺少 $c"
done
[ -x /usr/bin/time ] || die "缺少 /usr/bin/time（本脚本依赖它的 -l 输出取峰值 RSS）"
[ -x "$CLI" ] || die "找不到 whisper-cli：${CLI}（设 WHISPER_CPP 指向 whisper.cpp 仓库）"
[ -f "$CORPUS_TXT" ] || die "找不到语料文本：$CORPUS_TXT"
[ $# -gt 0 ] || die "用法：$0 [--regenerate] <ggml模型> [...]"
[[ "$THREADS" =~ ^[1-9][0-9]*$ ]] || die "THREADS 必须是正整数，当前是 '$THREADS'"
# 不用 `say -v '?' | grep -q`：grep -q 命中即退出并关闭管道，say 收到 SIGPIPE
# 返回非零，pipefail 下会把「装了 Kanya」误判成「没装」
VOICES="$(say -v '?')"
grep -q '^Kanya' <<<"$VOICES" || die "缺泰语嗓音 Kanya（系统设置 → 辅助功能 → 朗读内容 → 系统声音里下载）"

# —— 语料。**指纹变了就重建**，不能只看某个文件在不在 ——
#
# 曾经只判断 len-long.wav 是否存在。那样的话：首次生成在 long 写出后、
# short/mid 写坏就会永久复用坏数据；改了 thai-corpus.txt 也不会重建。
STAMP="$C/.stamp"
# 生成逻辑变了就必须重建语料——只看文本 hash 不够。改了 say 参数、
# 改了 ffmpeg 编码、改了长度档的取句数，都要把这个版本号 +1。
CORPUS_FORMAT_VERSION=1
# 每个命令替换单独赋值并校验格式。塞在一个字符串里的话，赋值的退出状态取
# **最后一个**替换（grep）的状态，前面 shasum 失败会被后面的成功盖掉，
# 于是指纹变成空串——语料看起来「变了」而重建出来的又是同一份。
CORPUS_SHA="$(shasum -a 256 "$CORPUS_TXT" | cut -c1-16)" || die "语料哈希计算失败：$CORPUS_TXT"
[[ "$CORPUS_SHA" =~ ^[0-9a-f]{16}$ ]] || die "语料哈希格式异常：'$CORPUS_SHA'"
CORPUS_N="$(grep -c . "$CORPUS_TXT")" || die "统计语料行数失败：$CORPUS_TXT"
WANT="v=$CORPUS_FORMAT_VERSION corpus=$CORPUS_SHA voice=Kanya n=$CORPUS_N"
NEED=1
if [ "$REGEN" = 0 ] && [ -f "$STAMP" ] && [ "$(cat "$STAMP")" = "$WANT" ]; then
  NEED=0
  # 指纹对上还不够，三个档都得真的能读且时长为正
  for t in len-short len-mid len-long; do
    d="$(ffprobe -v error -show_entries format=duration -of csv=p=0 "$C/$t.wav" 2>/dev/null || echo 0)"
    [ "$(echo "$d > 0" | bc -l)" = 1 ] || { NEED=1; break; }
  done
fi

if [ "$NEED" = 1 ]; then
  echo "==> 合成语料到 $C"
  # 先在临时目录整个建好再替换，避免中途失败留下半套语料。
  #
  # **这不是原子替换**：`rm -rf` 与 `mv` 是两步，中间有空窗，
  # 两个实例并发跑还会互删。要真原子得用版本化目录 + symlink 切换。
  # 现状够用（基准是手动跑的单实例），但别在注释里吹成原子。
  TMP="$C.tmp.$$"; rm -rf "$TMP"; mkdir -p "$TMP"
  trap 'rm -rf "$TMP"' EXIT
  i=0; PARTS=()
  while IFS= read -r line; do
    [ -n "$line" ] || continue
    i=$((i+1)); n="$(printf 's%02d' "$i")"
    say -v Kanya -o "$TMP/$n.aiff" "$line"
    ffmpeg -y -i "$TMP/$n.aiff" -ar 16000 -ac 1 "$TMP/$n.wav" -loglevel error
    rm -f "$TMP/$n.aiff"
    PARTS+=("$TMP/$n.wav")
  done < "$CORPUS_TXT"
  [ "$i" -ge 6 ] || die "语料至少要 6 句，当前 $i 句"
  build() {  # $1=名字 $2=取前几句。显式构造列表，不解析 ls 的输出
    local out="$TMP/.concat"; : > "$out"
    local k=0
    for p in "${PARTS[@]}"; do
      k=$((k+1)); [ "$k" -le "$2" ] || break
      printf "file '%s'\n" "$p" >> "$out"
    done
    ffmpeg -y -f concat -safe 0 -i "$out" -ar 16000 -ac 1 "$TMP/$1.wav" -loglevel error
    rm -f "$out"
  }
  build len-short 2
  build len-mid 6
  build len-long "$i"
  echo "$WANT" > "$TMP/.stamp"
  rm -rf "$C"; mv "$TMP" "$C"   # 见上：非原子
  trap - EXIT
fi

dur() { ffprobe -v error -show_entries format=duration -of csv=p=0 "$1"; }

# 一次运行 → 结果写进全局 RUN_SECS / RUN_RSS。
#
# **结果不走 stdout，调用方也绝不能把它套进 `$(...)`。** 早先的版本是
# `read -r ts rs <<<"$(run ...)"`，那样 `die` 里的 `exit 1` 只结束命令替换
# 那个子 shell，主脚本继续跑：`read` 从空串里读到 ts="" rs=""，
# 后面 `echo "$ts/$DS" | bc -l` 拿到 "/12.3"（bc 报错、不输出、退出码仍是 0），
# 算术里的空串按 0 处理——于是打出一整行「0.000 0.000 0.000 0 0」并 exit 0。
# 一行凭空捏造的测量，混在正常行里看不出来。
#
# `/usr/bin/time -l` 和被测程序的 stderr 混在同一个流里，所以两个字段都要
# 锚定着取，并且校验解析结果确实是数字——解析失败若放任，会到 bc 那里才炸，
# 报出来的错和真实原因八竿子打不着。
RUN_SECS=""; RUN_RSS=""
run() {
  local o
  RUN_SECS=""; RUN_RSS=""
  o="$( { /usr/bin/time -l "$1" -m "$2" -f "$3" -l th -t "$THREADS" \
          -bo 1 -bs 1 -np -nt >/dev/null; } 2>&1 )" || {
    # 取**开头**几行：被测程序自己的报错在前，/usr/bin/time -l 的统计追加在后
    echo "-- whisper-cli 输出开头 --" >&2; head -5 <<<"$o" >&2
    die "whisper-cli 失败：模型 $(basename "$2") / 音频 $(basename "$3")"
  }
  RUN_SECS="$(awk '$2=="real"{print $1; exit}' <<<"$o")"
  RUN_RSS="$(awk '/maximum resident set size/{print $1; exit}' <<<"$o")"
  [[ "$RUN_SECS" =~ ^[0-9]+\.?[0-9]*$ ]] || die "解析墙钟失败（/usr/bin/time 输出格式变了？）：$(head -3 <<<"$o")"
  [[ "$RUN_RSS"  =~ ^[0-9]+$ ]]          || die "解析峰值 RSS 失败：$(grep -i resident <<<"$o" | head -1)"
}

# bc 语法错误时只往 stderr 写一行、stdout 为空、退出码仍是 0，所以结果得自己验。
rtf_of() {  # $1=墙钟秒 $2=音频时长秒 → 回写 RTF_OUT
  RTF_OUT="$(echo "$1/$2" | bc -l)"
  [[ "$RTF_OUT" =~ ^[0-9]*\.?[0-9]+$ ]] || die "RTF 计算结果不是数字：'$RTF_OUT'（墙钟=$1 时长=$2）"
}

DS="$(dur "$C/len-short.wav")"; DM="$(dur "$C/len-mid.wav")"; DL="$(dur "$C/len-long.wav")"
# ffprobe 可能成功退出却打出 N/A 或空串，那样 bc 会把 RTF 算成垃圾
for d in "$DS" "$DM" "$DL"; do
  [[ "$d" =~ ^[0-9]+\.?[0-9]*$ ]] && [ "$(echo "$d > 0" | bc -l)" = 1 ] \
    || die "语料时长解析失败：'$d'（$C 下的 wav 是不是坏了？）"
done
printf '语料 %.1fs / %.1fs / %.1fs（自编合成，仅供性能测量）｜线程 %s｜whisper.cpp %s\n\n' \
  "$DS" "$DM" "$DL" "$THREADS" "$(git -C "$WCPP" rev-parse --short HEAD 2>/dev/null || echo '?')"
printf '%-24s %7s %8s %8s %8s %8s %8s  %s\n' 模型 体积MB 短RTF 中RTF 长RTF 短RSS 长RSS sha256
# 跳过的模型必须在**stdout** 上留痕。只往 stderr 写的话，把输出重定向进
# 数据文件时，那一行就消失了——表里少一行，看起来和「本来就没测这个」
# 一模一样。收尾再以非零退出，让「部分失败」不会被当成成功。
SKIPPED=0
for m in "$@"; do
  tag="$(basename "$m" .bin)"
  [ -f "$m" ] || { printf '%-24s  !! 跳过：文件不存在\n' "$tag"; SKIPPED=$((SKIPPED+1)); continue; }
  sz="$(stat -f%z "$m")"
  # 转换失败留下的残件量化后仍只有几 MB。这只挡得住那一种坏法——
  # 截断到几百 MB、量化错档、架构不对都挡不住，所以下面把 sha256 一起打出来，
  # 出了问题至少能对上是哪个文件。
  [ "$sz" -gt 100000000 ] || {
    printf '%-24s  !! 跳过：只有 %d MB，多半是转换失败的残件\n' "$tag" $((sz/1000000))
    SKIPPED=$((SKIPPED+1)); continue; }
  # 三次运行都直接调用（不套 $(...)），任何一次失败都会当场终止整个脚本
  run "$CLI" "$m" "$C/len-short.wav"; ts="$RUN_SECS"; rs="$RUN_RSS"
  run "$CLI" "$m" "$C/len-mid.wav";   tm="$RUN_SECS"
  run "$CLI" "$m" "$C/len-long.wav";  tl="$RUN_SECS"; rl="$RUN_RSS"
  rtf_of "$ts" "$DS"; RS="$RTF_OUT"
  rtf_of "$tm" "$DM"; RM="$RTF_OUT"
  rtf_of "$tl" "$DL"; RL="$RTF_OUT"
  # 哈希也要单独算。塞进 printf 的参数里的话，shasum 失败只让那个命令替换
  # 非零，外层 printf 照样成功 —— 于是打出一行哈希为空的测量、SKIPPED 仍是 0、
  # 最终 exit 0。和最初那个「伪造整行」的坑是同一类。
  MSHA="$(shasum -a 256 "$m" | cut -c1-12)" || die "模型哈希计算失败：$m"
  [[ "$MSHA" =~ ^[0-9a-f]{12}$ ]] || die "模型哈希格式异常：'$MSHA'（${m}）"
  printf '%-24s %7d %8.3f %8.3f %8.3f %8d %8d  %s\n' "$tag" \
    $((sz/1000000)) "$RS" "$RM" "$RL" $((rs/1000000)) $((rl/1000000)) "$MSHA"
done

[ "$SKIPPED" = 0 ] || die "$SKIPPED 个模型被跳过，上面这张表不完整"
