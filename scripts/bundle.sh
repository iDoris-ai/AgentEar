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
else
  echo "!! 缺少 vendor/，打出来的 app 跑不起来" >&2
  exit 1
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

# 未签名的 app 在部分 macOS 版本上启动会被拦。做一次 ad-hoc 签名，
# 让它至少能在本机正常运行。正式分发需要开发者证书 + 公证。
echo "==> ad-hoc 签名"
codesign --force --deep --sign - "$APP" 2>&1 | sed 's/^/    /'

echo "==> 完成: $APP"
echo
echo "首次运行需要授权两项权限（bundle 的权限与终端是分开的，要单独授予）："
echo "  1. 麦克风    —— 首次录音时会弹窗"
echo "  2. 辅助功能  —— 监听右 Command 所需"
echo "     系统设置 → 隐私与安全性 → 辅助功能，把 AgentEar 加进去"
echo
echo "启动：open $APP"
echo "看日志：$APP/Contents/MacOS/AgentEar"
