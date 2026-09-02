#!/usr/bin/env bash
# 编一个**静态链接**的 whisper-cli，供泰语识别走 whisper 路径时调用。
#
# 为什么要自己编，不直接抄 whisper.cpp 的 build/bin/whisper-cli：
# 那个是动态链接的，依赖 6 个 @rpath dylib（libwhisper / libggml*）。
# 随 .app 分发就得连 dylib 一起搬、改 rpath、逐个签名——每一步都能在
# 别人的机器上静默失败。静态单二进制没有这些问题，也和主链路的
# llama-funasr-sensevoice 形态一致。
#
# 为什么开 Metal：ADR-0004 §3 的表头写着「CPU 后端」，但那**是错的**。
# 2026-09-02 实测同一份模型、同一段音频、同样的解码参数：
#
#   音频 33.2s   Metal 6.5s (RTF 0.20)   纯 CPU 13.0s (RTF 0.39)
#   音频  5.2s   Metal 0.94s (RTF 0.18)  纯 CPU 4.05s (RTF 0.78)
#
# 入库的中档 RTF 0.185 只和 Metal 那一列对得上——基准脚本调的是
# whisper.cpp 默认构建的 whisper-cli，而那个默认**带 Metal 且默认就用**
# （没传 `-ng`）。所以按字面关掉 Metal，等于把产品做慢 2–4 倍，
# 反而偏离了入库数据。
#
# ⚠️ **Metal 首次运行要编译 shader，实测约 19 秒**（之后由系统缓存，
# 降到 1 秒内）。这笔开销由下载后的加载冒烟吃掉——见 asr::smoke_thai。
#
# EMBED_LIBRARY 把 metallib 嵌进二进制，这样仍然是单文件，
# 不必额外分发 ggml-metal.metal。
#
# 用法：scripts/build-whisper-cli.sh [输出目录]
#   WHISPER_CPP  whisper.cpp 仓库路径（默认与本仓库同级）

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WCPP="${WHISPER_CPP:-$ROOT/../whisper.cpp}"
OUT="${1:-$ROOT/vendor/bin}"
# 独立的 build 目录：**不碰共享 checkout 的 build/**，那里是动态链接的
# 构建产物，性能基线脚本还在用它。
BUILD="${TMPDIR:-/tmp}/agentear-whisper-static"

die() { echo "!! $*" >&2; exit 1; }

[ -f "$WCPP/CMakeLists.txt" ] || die "找不到 whisper.cpp：$WCPP（设 WHISPER_CPP）"
command -v cmake >/dev/null || die "缺少 cmake"

echo "==> whisper.cpp commit $(cd "$WCPP" && git rev-parse --short HEAD)"
# 入库的性能与准确率数据都是在这个 commit 上测的（ADR-0004 §2/§3）。
# 换 commit 不是不行，但产出的就不是被测过的那个东西了。
EXPECT_COMMIT="7de8dd78"
GOT_COMMIT="$(cd "$WCPP" && git rev-parse --short=8 HEAD)"
[ "$GOT_COMMIT" = "$EXPECT_COMMIT" ] || \
  echo "!! 警告：当前是 $GOT_COMMIT，ADR-0004 的数据测的是 $EXPECT_COMMIT" >&2

cmake -S "$WCPP" -B "$BUILD" \
  -DCMAKE_BUILD_TYPE=Release \
  -DBUILD_SHARED_LIBS=OFF \
  -DGGML_METAL=ON \
  -DGGML_METAL_EMBED_LIBRARY=ON \
  -DGGML_ACCELERATE=ON \
  -DWHISPER_BUILD_TESTS=OFF \
  -DWHISPER_BUILD_SERVER=OFF \
  -DCMAKE_OSX_DEPLOYMENT_TARGET=11.0 \
  > "$BUILD.log" 2>&1 || { tail -20 "$BUILD.log"; die "cmake 配置失败（完整日志 $BUILD.log）"; }

cmake --build "$BUILD" --target whisper-cli -j "$(sysctl -n hw.ncpu)" \
  >> "$BUILD.log" 2>&1 || { tail -30 "$BUILD.log"; die "编译失败（完整日志 $BUILD.log）"; }

BIN="$BUILD/bin/whisper-cli"
[ -x "$BIN" ] || die "编出来了但找不到 $BIN"

# 静态得彻底才有意义：除了系统库，不该再有 @rpath 依赖。
# 漏一个就是「我这儿能跑」，装到别人机器上起不来。
LEFT="$(otool -L "$BIN" | tail -n +2 | grep -v '^\s*/usr/lib/' | grep -v '^\s*/System/' || true)"
[ -z "$LEFT" ] || die "还有非系统动态依赖，静态链接没生效：
$LEFT"

mkdir -p "$OUT"
cp "$BIN" "$OUT/whisper-cli"
chmod +x "$OUT/whisper-cli"

echo "==> $OUT/whisper-cli"
echo "    架构 $(lipo -archs "$OUT/whisper-cli")  体积 $(du -h "$OUT/whisper-cli" | cut -f1)"
echo "    sha256 $(shasum -a 256 "$OUT/whisper-cli" | cut -c1-12)"
