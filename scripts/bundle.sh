#!/usr/bin/env bash
# 把 release 二进制打成 macOS .app bundle。
#
# 为什么必须打 bundle：从终端直接跑的二进制会**继承终端的 TCC 权限**
# （麦克风、辅助功能），产生「我这儿能跑」的假象。分发给别的机器就会
# 因为没有 Info.plist 里的用途声明而被系统直接拒绝。
#
# 用法：scripts/bundle.sh [输出目录]  （默认 ./dist）

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT="${1:-$ROOT/dist}"
APP="$OUT/AgentEar.app"
VERSION="$(grep -m1 '^version' "$ROOT/Cargo.toml" | cut -d'"' -f2)"

echo "==> 构建 release"
cargo build --release --manifest-path "$ROOT/Cargo.toml"

echo "==> 组装 $APP"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"

cp "$ROOT/target/release/agentear" "$APP/Contents/MacOS/AgentEar"

# ASR 二进制与模型随 bundle 走，运行时从 Resources/vendor 读
if [ -d "$ROOT/vendor" ]; then
  cp -R "$ROOT/vendor" "$APP/Contents/Resources/vendor"
  cp "$ROOT/LICENSE" "$ROOT/NOTICE" "$APP/Contents/Resources/"
else
  echo "!! 缺少 vendor/，打出来的 app 跑不起来" >&2
  exit 1
fi

# 图标缺失不致命，但会在控制中心/权限列表里显示成空白方块
if [ -f "$ROOT/assets/AgentEar.icns" ]; then
  cp "$ROOT/assets/AgentEar.icns" "$APP/Contents/Resources/"
else
  echo "!! 缺少 assets/AgentEar.icns，图标会是空白方块（跑 scripts/make-icon.py 生成）" >&2
fi

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key>
    <string>AgentEar</string>
    <key>CFBundleDisplayName</key>
    <string>AgentEar</string>
    <key>CFBundleIdentifier</key>
    <string>ai.idoris.agentear</string>
    <key>CFBundleVersion</key>
    <string>$VERSION</string>
    <key>CFBundleShortVersionString</key>
    <string>$VERSION</string>
    <key>CFBundleExecutable</key>
    <string>AgentEar</string>
    <!-- 没有图标时，控制中心的麦克风占用面板、系统设置的权限列表里都只显示
         一个空白方块——「某个没有图标的东西在用你的麦克风」 -->
    <key>CFBundleIconFile</key>
    <string>AgentEar</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>LSMinimumSystemVersion</key>
    <string>11.0</string>

    <!-- 只在菜单栏出现，不占 Dock -->
    <key>LSUIElement</key>
    <true/>

    <!-- 麦克风用途说明。没有这一条，系统会直接拒绝而不是弹窗询问 -->
    <key>NSMicrophoneUsageDescription</key>
    <string>AgentEar 需要访问麦克风来录制你的语音并在本地转写成文字。录音和转写全部在本机完成，不会上传到任何服务器。</string>
</dict>
PLIST
echo '</plist>' >> "$APP/Contents/Info.plist"

# 签名。**必须用固定身份，不要用 ad-hoc（`-`）**。
#
# ad-hoc 签名没有稳定的代码标识：每次 codesign 都产生新的 cdhash，而 TCC 把
# 「辅助功能」授权钉死在 cdhash 上。后果是每次重新打包，授权都会静默失效——
# 系统设置里开关看着是开的、TCC 库里 auth_value=2，程序却报未授予。
# 详见 docs/m1-status.md 的「每次重新打包，辅助功能授权都会失效」。
#
# 用固定证书后，designated requirement 变成「identifier + 证书哈希」，
# 跨重建稳定，授权一次就一直有效。
# 证书用 scripts/make-signing-cert.sh 创建。
SIGN_ID="${AGENTEAR_SIGN_ID:-AgentEar Local Signing}"
if security find-identity -v -p codesigning 2>/dev/null | grep -qF "$SIGN_ID"; then
  # 必须写成 ${SIGN_ID}：macOS 自带 bash 3.2 会把紧跟其后的全角括号的
  # 首字节当成变量名的一部分，裸写 $SIGN_ID 会报 unbound variable
  echo "==> 签名（${SIGN_ID}）"
else
  echo "!! 找不到签名身份 \"$SIGN_ID\"，退回 ad-hoc" >&2
  echo "!! 后果：每次重新打包都要手动重新授权辅助功能" >&2
  echo "!! 修复：scripts/make-signing-cert.sh" >&2
  SIGN_ID="-"
fi
codesign --force --deep --sign "$SIGN_ID" "$APP" 2>&1 | sed 's/^/    /'

# 打印 designated requirement。它跨重建是否稳定，决定了要不要重新授权。
echo "    designated requirement:"
codesign -d -r- "$APP" 2>&1 | grep '^designated' | sed 's/^/      /'

# 架构一致性校验。ASR 运行时（FunASR 官方 macOS 版）只有 arm64，
# 所以整个 app 是 Apple Silicon only——做通用二进制没有意义，
# Intel 机器上 ASR 子进程照样跑不起来。
echo "==> 架构校验"
APP_ARCH="$(lipo -archs "$APP/Contents/MacOS/AgentEar")"
ASR_ARCH="$(lipo -archs "$APP/Contents/Resources/vendor/bin/llama-funasr-sensevoice")"
echo "    AgentEar:              $APP_ARCH"
echo "    llama-funasr-sensevoice: $ASR_ARCH"
if [ "$APP_ARCH" != "$ASR_ARCH" ]; then
  echo "!! 架构不一致，ASR 子进程会起不来" >&2
  exit 1
fi

# 冒烟测试：确认打出来的 app 能找到 bundle 内的 vendor 并跑通自检
echo "==> 冒烟测试"
# 先把输出收进变量再匹配。不要写成 `... | grep -q`：grep -q 命中即退出
# 并关闭管道，上游收到 SIGPIPE 返回非零，被 pipefail 判成整条失败。
SMOKE="$("$APP/Contents/MacOS/AgentEar" --diagnose 2>&1 || true)"
if grep -q "Resources/vendor" <<<"$SMOKE"; then
  echo "    ✅ bundle 内 vendor 解析正确"
else
  echo "!! bundle 找不到 Resources/vendor" >&2
  echo "$SMOKE" | sed 's/^/    /' >&2
  exit 1
fi

# 发布用压缩包。用 ditto 而非 zip，它会保留资源分支与签名
echo "==> 打包发布件"
ZIP="$OUT/AgentEar-$VERSION-macos-arm64.zip"
rm -f "$ZIP"
ditto -c -k --sequesterRsrc --keepParent "$APP" "$ZIP"
echo "    $ZIP ($(du -h "$ZIP" | cut -f1))"

echo "==> 完成: $APP"
echo
echo "首次运行需要授权两项权限（bundle 的权限与终端是分开的，要单独授予）："
echo "  1. 麦克风    —— 首次录音时会弹窗"
echo "  2. 辅助功能  —— 监听右 Command 所需"
echo "     系统设置 → 隐私与安全性 → 辅助功能，把 AgentEar 加进去"
echo
echo "启动：open $APP"
echo "看日志：$APP/Contents/MacOS/AgentEar"
