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

# ASR 二进制与模型随 bundle 走，运行时从 Resources/vendor 读。
#
# **泰语模型不在这里**——它 574 MB，按需下载到 ~/.agentear/models/。
# 往 bundle 里写会破坏代码签名（TCC 把辅助功能授权钉在 cdhash 上），
# 而且升级时整个 .app 被替换，下载的模型会直接消失。见 src/download.rs。
# 这里只带泰语**引擎**（whisper-cli，2.5 MB）。
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

    <!-- 麦克风用途说明。没有这一条，系统会直接拒绝而不是弹窗询问。
         这里是**英文兜底**，实际显示的文案由 Resources/*.lproj/InfoPlist.strings
         按系统语言挑（不是按 app 里的界面语言设置——这是系统弹窗）。 -->
    <key>NSMicrophoneUsageDescription</key>
    <string>AgentEar needs microphone access to record your voice and transcribe it locally. Recording and transcription happen entirely on this Mac; nothing is uploaded to any server.</string>

    <!-- 声明支持的本地化，否则系统只认 development region 那一种 -->
    <key>CFBundleDevelopmentRegion</key>
    <string>en</string>
    <key>CFBundleLocalizations</key>
    <array>
        <string>en</string>
        <string>zh-Hans</string>
        <string>th</string>
    </array>
</dict>
PLIST
echo '</plist>' >> "$APP/Contents/Info.plist"

# 权限弹窗的本地化文案。
#
# 注意这**不跟随 app 里的「界面语言」设置**：TCC 弹窗是系统画的，
# 系统按用户的 macOS 语言挑 .lproj，我们无从干预，也不该干预——
# 用户的系统是什么语言，系统弹窗就该是什么语言。
echo "==> 写入权限文案本地化"
write_infoplist_strings() {
  local lproj="$APP/Contents/Resources/$1.lproj"
  mkdir -p "$lproj"
  # 转义 `\` 和 `"`。顺序不能反——先转反斜杠，否则会把自己插入的
  # 转义符再转一遍。
  #
  # 反斜杠这条比引号更阴险：文案里写个 `\n`，.strings 解析器会把它当换行，
  # plutil -lint **照样通过**，只是显示出来的内容和你写的不一样了。
  local escaped="$2"
  escaped="${escaped//\\/\\\\}"
  escaped="${escaped//\"/\\\"}"
  # 无 BOM 的 UTF-8 就够（plutil -lint 把关）
  printf '%s\n' "\"NSMicrophoneUsageDescription\" = \"$escaped\";" > "$lproj/InfoPlist.strings"
  # 格式错了系统不会报错，只会静默退回 Info.plist 里的英文兜底——
  # 那种失败没人会发现，所以在这里挡住
  plutil -lint "$lproj/InfoPlist.strings" > /dev/null
}
write_infoplist_strings en \
  "AgentEar needs microphone access to record your voice and transcribe it locally. Recording and transcription happen entirely on this Mac; nothing is uploaded to any server."
write_infoplist_strings zh-Hans \
  "AgentEar 需要访问麦克风来录制你的语音并在本地转写成文字。录音和转写全部在本机完成，不会上传到任何服务器。"
write_infoplist_strings th \
  "AgentEar ต้องการเข้าถึงไมโครโฟนเพื่อบันทึกเสียงของคุณและถอดความในเครื่อง การบันทึกและถอดความทั้งหมดทำงานบน Mac เครื่องนี้ ไม่มีการอัปโหลดไปยังเซิร์ฟเวอร์ใด ๆ"

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
#
# 2026-09-02 复核过上游：`modelscope/FunASR` 从 runtime-llamacpp-v0.1.9 到
# v0.2.6，macOS 只发 `macos-arm64`，**从来没有过 macos-x64**
# （Linux/Windows 才有 x64）。所以这不是「暂时没做」，是上游没有。
echo "==> 架构校验"
APP_ARCH="$(lipo -archs "$APP/Contents/MacOS/AgentEar")"
# 逐个校验 vendor 里的**每一个**可执行文件，不是只看主 ASR 那一个。
# 泰语引擎是后加的，只校验一个的话，加进来一个 x86_64 的二进制
# 不会有任何提示——直到用户切到泰语才炸。
echo "    AgentEar: $APP_ARCH"
for bin in "$APP/Contents/Resources/vendor/bin/"*; do
  [ -f "$bin" ] || continue
  BIN_ARCH="$(lipo -archs "$bin" 2>/dev/null || echo '?')"
  echo "    $(basename "$bin"): $BIN_ARCH"
  if [ "$BIN_ARCH" != "$APP_ARCH" ]; then
    echo "!! $(basename "$bin") 架构是 ${BIN_ARCH}，与 app 的 $APP_ARCH 不一致，子进程会起不来" >&2
    exit 1
  fi
done

# 冒烟测试：确认打出来的 app 能找到 bundle 内的 vendor 并跑通自检
echo "==> 冒烟测试"
# 先把输出收进变量再匹配。不要写成 `... | grep -q`：grep -q 命中即退出
# 并关闭管道，上游收到 SIGPIPE 返回非零，被 pipefail 判成整条失败。
SMOKE="$("$APP/Contents/MacOS/AgentEar" --diagnose 2>&1 || true)"
if grep -q "Resources/vendor" <<<"$SMOKE"; then
  echo "    ✅ bundle 内 vendor 解析正确"
  # --diagnose 会打印泰语引擎那一行。缺了它泰语根本不能用，
  # 而主链路照常——那种「一半功能悄悄没了」的包不该发出去。
  if grep -q "✅ 引擎" <<<"$SMOKE"; then
    echo "    ✅ 泰语引擎已随包"
  else
    echo "!! bundle 里没有泰语引擎（跑 scripts/build-whisper-cli.sh）" >&2
    exit 1
  fi
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
