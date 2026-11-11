#!/usr/bin/env bash
# 从源码复建 vendor/bin 里的两个 ASR 二进制（FU-24）：
#   llama-funasr-sensevoice  modelscope/FunASR runtime/llama.cpp（内置 ASR 主链路）
#   whisper-cli              ggml-org/whisper.cpp（泰语路径，静态链接 + Metal）
#
# 用法：scripts/build-vendor-asr.sh [输出目录]
#   输出目录默认 ./build/vendor-asr，**默认不碰 vendor/**；要覆盖 vendor/bin 请显式传 vendor/bin。
#   WHISPER_CPP  已有的 whisper.cpp checkout（默认与本仓库同级）；不存在就按钉死的 commit 克隆。
#   ONLY=sensevoice|whisper  只编其中一个
#
# 钉的东西（2026-11-11 核实）：
#   * llama-funasr-sensevoice：开发机 vendor 里那份与 FunASR release `runtime-llamacpp-v0.1.9`
#     的 funasr-llamacpp-macos-arm64.tar.gz 里的二进制**逐字节相同**（sha256 见 docs/upstream-pins.md），
#     即它是官方预编译产物。该 tag 指向 FunASR commit 9b918d74…，其 runtime/llama.cpp/CMakeLists.txt
#     用 FetchContent 拉 ggml-org/llama.cpp @ 8086439a…。官方 CI 的 macos-arm64 参数是 -DGGML_NATIVE=OFF。
#   * whisper-cli：ggml-org/whisper.cpp @ 7de8dd783f7b（ADR-0004 的测量 commit），参数见 build-whisper-cli.sh。
#
# ⚠️ 源码构建**不保证与 vendor 逐字节相同**（编译器版本、嵌入路径、时间戳不同）；
#    可复现性的判据是「钉死的源码 + 参数」+「行为冒烟一致」，不是 sha256 相等。
#    逐字节一致的获取方式是直接下官方 tarball 并核对 sha256（见 docs/upstream-pins.md §1.3）。

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT="${1:-$ROOT/build/vendor-asr}"
ONLY="${ONLY:-all}"
WORK="${TMPDIR:-/tmp}/agentear-vendor-asr"

FUNASR_REPO="https://github.com/modelscope/FunASR.git"
FUNASR_COMMIT="9b918d74d7090b38211a6414c6ee08893a34afd4"   # tag runtime-llamacpp-v0.1.9
FUNASR_LLAMA_COMMIT="8086439a4cea94c71a5dfb8fe4ad1546aebd640f"  # 该 commit 的 CMakeLists 里钉的 llama.cpp
WHISPER_REPO="https://github.com/ggml-org/whisper.cpp.git"
WHISPER_COMMIT="7de8dd783f7b2eab56bff6bbc5d3369e34f0e77f"

die() { echo "!! $*" >&2; exit 1; }
command -v cmake >/dev/null || die "缺少 cmake"
command -v git >/dev/null || die "缺少 git"
NCPU="$(sysctl -n hw.ncpu)"
mkdir -p "$OUT" "$WORK"

checkout_pinned() {  # <repo> <commit> <dir>
  local repo="$1" commit="$2" dir="$3"
  if [ ! -d "$dir/.git" ]; then
    git init -q "$dir"
    git -C "$dir" remote add origin "$repo"
  fi
  if [ "$(git -C "$dir" rev-parse HEAD 2>/dev/null || true)" != "$commit" ]; then
    git -C "$dir" fetch -q --depth 1 origin "$commit" || die "拉取 $repo @ $commit 失败"
    git -C "$dir" checkout -q --detach FETCH_HEAD
  fi
  [ "$(git -C "$dir" rev-parse HEAD)" = "$commit" ] || die "$dir 不在 $commit"
}

build_sensevoice() {
  local src="$WORK/FunASR" b="$WORK/funasr-build" llama="$WORK/llama.cpp"
  echo "==> FunASR @ ${FUNASR_COMMIT:0:12}"
  checkout_pinned "$FUNASR_REPO" "$FUNASR_COMMIT" "$src"
  grep -q "$FUNASR_LLAMA_COMMIT" "$src/runtime/llama.cpp/CMakeLists.txt" \
    || die "该 FunASR commit 钉的 llama.cpp 不是 ${FUNASR_LLAMA_COMMIT}，本脚本的钉子过期了"
  # 预先把 llama.cpp 钉到同一 commit 并浅取，避免 FetchContent 克隆整个历史。
  checkout_pinned "https://github.com/ggml-org/llama.cpp.git" "$FUNASR_LLAMA_COMMIT" "$llama"
  cmake -S "$src/runtime/llama.cpp" -B "$b" \
    -DCMAKE_BUILD_TYPE=Release -DGGML_NATIVE=OFF \
    -DFETCHCONTENT_SOURCE_DIR_LLAMA="$llama" \
    -DFETCHCONTENT_FULLY_DISCONNECTED=ON \
    > "$b.log" 2>&1 || { tail -20 "$b.log"; die "FunASR cmake 配置失败（$b.log）"; }
  cmake --build "$b" --target llama-funasr-sensevoice -j "$NCPU" \
    >> "$b.log" 2>&1 || { tail -30 "$b.log"; die "FunASR 编译失败（$b.log）"; }
  cp "$b/bin/llama-funasr-sensevoice" "$OUT/llama-funasr-sensevoice"
  chmod +x "$OUT/llama-funasr-sensevoice"
  echo "    $OUT/llama-funasr-sensevoice  sha256 $(shasum -a 256 "$OUT/llama-funasr-sensevoice" | cut -d' ' -f1)"
}

build_whisper() {
  local w="${WHISPER_CPP:-$ROOT/../whisper.cpp}"
  if [ ! -f "$w/CMakeLists.txt" ]; then
    w="$WORK/whisper.cpp"
    checkout_pinned "$WHISPER_REPO" "$WHISPER_COMMIT" "$w"
  fi
  local got
  got="$(git -C "$w" rev-parse HEAD)"
  [ "$got" = "$WHISPER_COMMIT" ] || die "whisper.cpp checkout 在 ${got}，要求 ${WHISPER_COMMIT}（ADR-0004 的测量 commit）；设 WHISPER_CPP 指向正确的 checkout，或删掉让脚本自己克隆"
  echo "==> whisper.cpp @ ${got:0:12}"
  WHISPER_CPP="$w" "$ROOT/scripts/build-whisper-cli.sh" "$OUT"
  echo "    sha256 $(shasum -a 256 "$OUT/whisper-cli" | cut -d' ' -f1)"
}

case "$ONLY" in
  all) build_sensevoice; build_whisper ;;
  sensevoice) build_sensevoice ;;
  whisper) build_whisper ;;
  *) die "ONLY 只认 all|sensevoice|whisper" ;;
esac
echo "完成。产物在 ${OUT}（未覆盖 vendor/）。"
