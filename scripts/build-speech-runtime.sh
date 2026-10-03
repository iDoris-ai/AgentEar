#!/usr/bin/env bash
# T3.5.9 方案 B-lite：从上游 speech-swift 的 PINNED 源码自己编 speech-server，
# 打上 patches/speech-swift-v0.0.28-agentear.patch（修「常驻越用越涨」那个内存
# 泄漏），然后把官方发布包里的 speech-server 换成我们编的这个，其余文件
# （speech CLI、mlx.metallib、各个 bundle）原样用官方的——不需要装 Metal
# Toolchain（我们自己的机器没装，mlx.metallib 要靠 Metal Toolchain 现编）。
#
# 产物就是「官方 tarball 的平替版」：同样的顶层布局，`src/qwen3.rs::unpack_runtime`
# 不用改一行就能解开它。
#
# 用法：scripts/build-speech-runtime.sh [输出目录]
#   WORK_DIR   克隆 + 编译用的临时目录（默认 mktemp，成功后保留以便排障，失败也保留）
#
# 产物：$OUT/speech-macos-arm64-v0.0.28-agentear.1.tar.gz（打印 sha256 / 大小 / 解包后大小）

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT="${1:-$ROOT/dist}"
PATCH="$ROOT/patches/speech-swift-v0.0.28-agentear.patch"

# 上游发布包（official，**不是**我们要发布的那个）：只借它的 mlx.metallib /
# speech CLI / bundle，sha 写死在这里——与 src/qwen3.rs 改之前的
# RUNTIME_URL / RUNTIME_SHA256 / RUNTIME_BYTES 一致。改这几行不会影响
# AgentEar 本身用哪个 URL（那是 src/qwen3.rs 的常量，由这个脚本的产物反过来驱动）。
UPSTREAM_TAG="v0.0.28"
# PR #106 评审非阻塞③：钉死完整 40 位 sha，不要只核对前缀——前缀匹配在上游
# 恰好有两个 commit 共享同一个短前缀时会把错的那个也放过去（概率很低，但
# 核对完整 sha 的代价是 0，没有理由省）。
UPSTREAM_COMMIT="231f8eb9f0971fee335fef49f42d2975e4fbf8bc"
UPSTREAM_REPO="https://github.com/soniqo/speech-swift"
OFFICIAL_URL="https://github.com/soniqo/speech-swift/releases/download/v0.0.28/speech-macos-arm64.tar.gz"
OFFICIAL_SHA256="cc144cac7985884f026a76281fdb504ce6e0fe2ad11a9b0a7901cf8b617b930a"
OFFICIAL_BYTES=99089736

# 我们发布的 agentear 补丁版的「修订号」。换补丁内容（不换上游版本）时只加这个数字；
# 换上游版本（SPEECH_VERSION）时连 UPSTREAM_TAG 一起改。
AGENTEAR_REV="1"
OUT_NAME="speech-macos-arm64-${UPSTREAM_TAG}-agentear.${AGENTEAR_REV}.tar.gz"

die() { echo "!! $*" >&2; exit 1; }
log() { echo "==> $*"; }

command -v git >/dev/null || die "缺少 git"
command -v swift >/dev/null || die "缺少 swift（装 Xcode 或 Swift toolchain）"
command -v shasum >/dev/null || die "缺少 shasum"
[ -f "$PATCH" ] || die "找不到补丁：$PATCH"

WORK_DIR="${WORK_DIR:-$(mktemp -d "${TMPDIR:-/tmp}/agentear-speech-runtime.XXXXXX")}"
mkdir -p "$WORK_DIR" "$OUT"
SRC="$WORK_DIR/speech-swift"

log "工作目录：${WORK_DIR}（成功或失败都会保留，自己清理）"

if [ ! -d "$SRC/.git" ]; then
  log "克隆 ${UPSTREAM_REPO} @ ${UPSTREAM_TAG}"
  git clone --depth 1 --branch "$UPSTREAM_TAG" "$UPSTREAM_REPO" "$SRC"
fi

cd "$SRC"
HEAD="$(git rev-parse HEAD)"
# 核对完整 40 位 sha，不是前缀——见上面 UPSTREAM_COMMIT 的注释。
[ "$HEAD" = "$UPSTREAM_COMMIT" ] \
  || die "pinned commit 不对：HEAD=${HEAD}，期望 ${UPSTREAM_COMMIT}（上游可能把这个 tag 重新打过？）"
log "pinned commit 核对通过：$HEAD"

if git diff --quiet; then
  log "打补丁：$(basename "$PATCH")"
  git apply "$PATCH"
else
  # 允许重跑（WORK_DIR 复用时补丁可能已经打过）：严格要求 diff 与补丁的
  # 「可应用性」一致，不去猜内容是不是完全相同，避免悄悄打两遍。
  git apply --check --reverse "$PATCH" \
    || die "工作区已有改动，且不是这个补丁打上去的；清空 $SRC 或换个 WORK_DIR 重来"
  log "补丁已经打过（工作区改动与它反向一致），跳过重复打补丁"
fi

log "swift build -c release --product speech-server（干净编译约 5.5 分钟）"
swift build -c release --product speech-server

BUILT_BIN="$SRC/.build/release/speech-server"
[ -x "$BUILT_BIN" ] || die "没编出 $BUILT_BIN"
file "$BUILT_BIN" | grep -q "Mach-O" || die "$BUILT_BIN 看起来不是一个可执行文件"
log "编译完成：${BUILT_BIN}（$(du -h "$BUILT_BIN" | cut -f1)）"

OFFICIAL_TAR="$WORK_DIR/official.tar.gz"
if [ -f "$OFFICIAL_TAR" ] && [ "$(shasum -a 256 "$OFFICIAL_TAR" | awk '{print $1}')" = "$OFFICIAL_SHA256" ]; then
  log "官方 tarball 已缓存在 ${OFFICIAL_TAR}，跳过重新下载"
else
  log "下载官方 tarball：$OFFICIAL_URL"
  curl -fL -o "$OFFICIAL_TAR" "$OFFICIAL_URL"
fi
ACTUAL_BYTES="$(wc -c < "$OFFICIAL_TAR" | tr -d ' ')"
[ "$ACTUAL_BYTES" = "$OFFICIAL_BYTES" ] || die "官方 tarball 大小不对：实得 ${ACTUAL_BYTES}，期望 ${OFFICIAL_BYTES}"
ACTUAL_SHA="$(shasum -a 256 "$OFFICIAL_TAR" | awk '{print $1}')"
[ "$ACTUAL_SHA" = "$OFFICIAL_SHA256" ] || die "官方 tarball sha256 不对：实得 ${ACTUAL_SHA}，期望 ${OFFICIAL_SHA256}"
log "官方 tarball 校验通过（${OFFICIAL_BYTES} 字节，sha256 对得上）"

STAGE="$WORK_DIR/stage"
rm -rf "$STAGE"
mkdir -p "$STAGE"
log "解官方 tarball 到 ${STAGE}（保留原有顶层布局）"
tar -xzf "$OFFICIAL_TAR" -C "$STAGE"
for need in speech speech-server mlx.metallib; do
  [ -e "$STAGE/$need" ] || die "官方 tarball 里缺 ${need}，上游包结构可能变了，src/qwen3.rs::unpack_runtime 也要跟着改"
done

log "把 speech-server 换成我们自己编的那个（其余文件原样用官方的）"
cp "$BUILT_BIN" "$STAGE/speech-server"
chmod +x "$STAGE/speech-server"

PATCH_NOTE="$STAGE/AGENTEAR-PATCH.md"
cat > "$PATCH_NOTE" <<EOF
# 这个运行时包里改了什么

这是 soniqo/speech-swift ${UPSTREAM_TAG}（commit ${HEAD}）官方发布包的平替版：
**只换了 \`speech-server\` 这一个二进制**，其余文件（\`speech\` CLI、
\`mlx.metallib\`、各个 \`*.bundle\`）与官方 tarball 逐字节相同
（官方 sha256：\`${OFFICIAL_SHA256}\`）。

\`speech-server\` 是从上游同一个 pinned commit 的源码编的，只多打了一个补丁：
AgentEar 仓库里的 \`patches/speech-swift-v0.0.28-agentear.patch\`
（https://github.com/iDoris-ai/AgentEar/blob/main/patches/speech-swift-v0.0.28-agentear.patch）。

补丁内容一句话：原版 Qwen3-ASR 转写路径每次请求后不释放 MLX 的 Metal 缓冲区，
常驻服务长时间跑会涨到几十 GB；补丁加一个默认不生效的环境变量开关
（\`AGENTEAR_MLX_CACHE_MB\`），AgentEar 自己起 speech-server 时才会设它。
不设这个环境变量时，这个二进制与官方版本行为完全一致。

上游许可证（Apache License 2.0）随补丁的源码仓库一起适用，见
https://github.com/soniqo/speech-swift/blob/${UPSTREAM_TAG}/LICENSE。
按 Apache-2.0 §4(b)，被改过的源文件（\`Sources/AudioServer/AudioServer.swift\`）
已经在文件内标注了修改说明，补丁文件开头也有完整的改动说明。

构建脚本：\`scripts/build-speech-runtime.sh\`（AgentEar 仓库）。
EOF

cd "$STAGE"
log "打包到 ${OUT_NAME}（保持与官方 tarball 一致的顶层布局）"
rm -f "$OUT/$OUT_NAME"
tar -czf "$OUT/$OUT_NAME" .
cd "$ROOT"

FINAL_SHA="$(shasum -a 256 "$OUT/$OUT_NAME" | awk '{print $1}')"
FINAL_BYTES="$(wc -c < "$OUT/$OUT_NAME" | tr -d ' ')"
UNPACKED_BYTES="$(du -sk "$STAGE" | awk '{print $1 * 1024}')"

echo
echo "=== 产物 ==="
echo "文件     : $OUT/$OUT_NAME"
echo "sha256   : $FINAL_SHA"
echo "字节数   : $FINAL_BYTES"
echo "解包后   : 约 $UNPACKED_BYTES 字节"
echo
echo "把这三个数填进 src/qwen3.rs：RUNTIME_SHA256 / RUNTIME_BYTES / RUNTIME_UNPACKED_BYTES"
echo "RUNTIME_URL 指向发布这个 tarball 的 GitHub release asset（本脚本不负责发布）。"
