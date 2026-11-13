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
VERSION="$(grep -m1 '^version' "$ROOT/Cargo.toml" | cut -d'"' -f2)"
BUILD_NUMBER="$VERSION"

# bundle id 可覆盖。MAS 沙箱原型必须用独立的 id（如 ai.idoris.agentear.mas），
# 否则和直接分发版共用 TCC 授权与沙箱容器。不设时与以前完全一致。
BUNDLE_ID="${AGENTEAR_BUNDLE_ID:-ai.idoris.agentear}"
CARGO_FEATURES=()
if [ "${AGENTEAR_DISTRIBUTION:-}" = "app-store" ]; then
  # App Store 版：独立的 .app 名与默认 bundle id，并只编 MAS 功能子集。
  APP="$OUT/AgentEar-MAS.app"
  # 正式 bundle id（2026-11-12 定）：上架后不可改，App Store Connect 里注册同一个。
  BUNDLE_ID="${AGENTEAR_BUNDLE_ID:-ai.idoris.agentear.mas}"
  # 沙箱探针（--mas-probe / --mas-probe-run，能在沙箱里跑任意命令）只给本机原型，提审构建不含。
  if [ "${AGENTEAR_MAS_PROBE:-}" = "1" ]; then
    CARGO_FEATURES=(--features mas-probe)
  else
    CARGO_FEATURES=(--features app-store)
  fi
  # 构建号（CFBundleVersion）：App Store 要求每次上传都比上一次大，同一个 0.x.y 重传也得换号，
  # 所以和版本号（CFBundleShortVersionString）分开。不设时退回版本号——只够本机原型用。
  if [ -n "${AGENTEAR_BUILD_NUMBER:-}" ]; then
    case "$AGENTEAR_BUILD_NUMBER" in
      *[!0-9.]*|.*|*.|*..*|*.*.*.*) echo "AGENTEAR_BUILD_NUMBER 只能是 1–3 段点分数字（Apple 的 CFBundleVersion 格式，如 42 或 29.0.3），收到：${AGENTEAR_BUILD_NUMBER}" >&2; exit 1 ;;
    esac
    BUILD_NUMBER="$AGENTEAR_BUILD_NUMBER"
  else
    echo "⚠️  未设 AGENTEAR_BUILD_NUMBER：CFBundleVersion 用版本号 ${VERSION}，只适合本机原型；上传 App Store 前必须设一个递增的构建号" >&2
  fi
else
  APP="$OUT/AgentEar.app"
fi


# vendor 检查（FU-30，2026-11-13 事故）：`cp -R` 遇到符号链接只拷链接本身，随后的签名会顺着链接
# 改写**包外**真实的 vendor 二进制（实际发生过：主 checkout 的 sensevoice 被改写后被系统 SIGTRAP 杀掉）。
# 选「报错退出」而不是 `cp -RL`：解引用拷贝会悄悄把任意包外文件打进发布件，
# 报错能逼人显式实拷（`cp -R <源>/vendor <此处>/vendor`），行为可预期。
# 包内的相对链接（不含 `..`、不是绝对路径）放行。
VENDOR_STATE=ok
if [ -L "$ROOT/vendor" ]; then
  VENDOR_STATE=symlink
elif [ -d "$ROOT/vendor" ]; then
  while IFS= read -r l; do
    [ -z "$l" ] && continue
    t="$(readlink "$l")"
    case "$t" in /*|*..*) VENDOR_STATE=external-symlink; VENDOR_BAD="$l -> $t"; break ;; esac
  done < <(find "$ROOT/vendor" -type l)
else
  VENDOR_STATE=missing
fi

# 干跑：不编译、不拷贝、不签名，只打印将要用的参数后退出（FU-29）。
# 构建号格式校验与 vendor 符号链接检查在上面已经照常生效。tests/script_defaults.rs 靠它做行为测试。
if [ "${AGENTEAR_BUNDLE_DRY_RUN:-}" = "1" ]; then
  if [ "${AGENTEAR_DISTRIBUTION:-}" != "app-store" ]; then PROBE_ASSERT=none
  elif [ "${AGENTEAR_MAS_PROBE:-}" = "1" ]; then PROBE_ASSERT=must-have
  else PROBE_ASSERT=must-not-have; fi
  echo "cargo_features=${CARGO_FEATURES[*]-}"
  echo "bundle_id=${BUNDLE_ID}"
  echo "build_number=${BUILD_NUMBER}"
  echo "short_version=${VERSION}"
  echo "app_name=$(basename "$APP")"
  echo "probe_assert=${PROBE_ASSERT}"
  echo "vendor=${VENDOR_STATE}"
fi
case "$VENDOR_STATE" in
  symlink|external-symlink)
    echo "!! vendor/ 是符号链接，或内含绝对路径 / 含 .. 的符号链接（${VENDOR_BAD:-$ROOT/vendor}）。判定从严：这类链接可能指向包外，cp -R 只会拷链接、签名会改写链接指向的真实文件；即使它其实指向包内也一律拒绝。请实拷：cp -R <源>/vendor $ROOT/vendor" >&2
    exit 1 ;;
esac
[ "${AGENTEAR_BUNDLE_DRY_RUN:-}" = "1" ] && exit 0
echo "==> 构建 release"
# 拷贝用 cargo **这次实际产出**的可执行文件路径，不写死 $ROOT/target/release（#135 评审 L1）：
# 设了 CARGO_TARGET_DIR 时，写死的路径会拷到 $ROOT/target 里上一次（可能带探针）的旧构建。
# json 走 stdout、编译诊断照常打到 stderr（json-render-diagnostics）。
BIN="$(cargo build --release ${CARGO_FEATURES[@]+"${CARGO_FEATURES[@]}"} --manifest-path "$ROOT/Cargo.toml" \
  --message-format=json-render-diagnostics \
  | sed -n 's|.*"executable":"\([^"]*/agentear\)".*|\1|p' | tail -1)"
if [ -z "$BIN" ] || [ ! -x "$BIN" ]; then
  echo "!! 没从 cargo 输出里拿到 agentear 可执行文件路径（收到：${BIN:-空}）" >&2
  exit 1
fi
echo "    可执行文件：${BIN}"

# ⚠️ **体积守卫：vendor/ 里不该出现推理用的模型权重。**
#
# 踩过（2026-09-14）：我把 VoxCPM2-8bit（2.3GB）下到了 `vendor/models/talk/`，
# 而下面这一步会把**整个 vendor/** 扫进 .app —— 于是安装包变成 4GB+，
# GitHub Release 直接拒收（`size must be less than 2147483648`）。
# 更糟的是**本地 `ls -lh` 只看到 240M 的旧 zip，不会注意**。
#
# 通话链路的模型按设计放在 `~/.agentear/talk/models/`（按需下载、不随包分发），
# 出现在 vendor 里一定是放错了。这里**直接拦住**，别让它悄悄打进包。
# **白名单 + 阈值**：只有这几个是**有意随包**的。
# 不用「一律拦 200MB 以上」——`sensevoice-small-q8.gguf`（254MB）本来就该随包，
# 那种守卫会误伤，而**误伤的守卫最后一定会被注释掉**，等于没有。
VENDOR_MODELS="$ROOT/vendor/models"
ALLOWED_BIG="sensevoice-small-q8.gguf fsmn-vad.gguf"
if [ -d "$VENDOR_MODELS" ]; then
  UNEXPECTED=""
  while IFS= read -r f; do
    [ -z "$f" ] && continue
    base="$(basename "$f")"
    case " $ALLOWED_BIG " in
      *" $base "*) ;;
      *) UNEXPECTED="$f" ;;
    esac
  done < <(find "$VENDOR_MODELS" -type f -size +200M 2>/dev/null)
  if [ -n "$UNEXPECTED" ]; then
    echo "!! vendor/models 里有不该随包的大文件：$UNEXPECTED" >&2
    echo "   通话链路的模型按设计放 ~/.agentear/talk/models/（按需下载、不随包分发）。" >&2
    echo "   实测教训：把 VoxCPM2-8bit 放在这里会让 .app 变成 4GB+，" >&2
    echo "   GitHub Release 直接拒收（size must be less than 2147483648）。" >&2
    exit 1
  fi
fi

echo "==> 组装 $APP"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"

cp "$BIN" "$APP/Contents/MacOS/AgentEar"

# 提审构建里不许有沙箱探针（能在沙箱里执行任意命令）；原型构建里必须有，否则复测步骤用不了。
# 对**拷进包里的那份**断言，而不是只信 feature 参数。
if [ "${AGENTEAR_DISTRIBUTION:-}" = "app-store" ]; then
  if grep -q -a -- "--mas-probe-run" "$APP/Contents/MacOS/AgentEar"; then HAS_PROBE=1; else HAS_PROBE=0; fi
  if [ "${AGENTEAR_MAS_PROBE:-}" = "1" ] && [ "$HAS_PROBE" = 0 ]; then
    echo "!! AGENTEAR_MAS_PROBE=1 但包里的二进制没有探针" >&2; exit 1
  fi
  if [ "${AGENTEAR_MAS_PROBE:-}" != "1" ] && [ "$HAS_PROBE" = 1 ]; then
    echo "!! 提审构建里混进了沙箱探针（--mas-probe-run），拒绝打包" >&2; exit 1
  fi
fi

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
    <string>$BUNDLE_ID</string>
    <key>CFBundleVersion</key>
    <string>$BUILD_NUMBER</string>
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
# App Store 包才带沙箱。直接安装的签名包不开：沙箱会把数据关进容器，
# 也会限制子进程，和现在这份本机安装包不是同一条分发路径。
# ⚠️ 这只是把签名分支备好；现有 bundle 还过不了 MAS 审核，见 docs/app-store.md。
if [ "${AGENTEAR_DISTRIBUTION:-}" = "app-store" ]; then
  echo "==> App Store 沙箱（assets/entitlements/AppStore.entitlements，bundle id ${BUNDLE_ID}）"
  # 随包可执行文件先逐个签：沙箱 + inherit（继承父进程的沙箱）。
  # 不能用 --deep：它会把嵌套二进制重签成不带 entitlements 的。
  for bin in "$APP/Contents/Resources/vendor/bin/"*; do
    [ -f "$bin" ] || continue
    codesign --force --options runtime \
      --entitlements "$ROOT/assets/entitlements/AppStoreHelper.entitlements" \
      --sign "$SIGN_ID" "$bin" 2>&1 | sed 's/^/    /'
  done
  codesign --force --options runtime \
    --entitlements "$ROOT/assets/entitlements/AppStore.entitlements" \
    --sign "$SIGN_ID" "$APP" 2>&1 | sed 's/^/    /'
else
  codesign --force --deep --sign "$SIGN_ID" "$APP" 2>&1 | sed 's/^/    /'
fi

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
# MAS 原型包只在本机沙箱验证用，不能和直接分发的发布件同名
[ "${AGENTEAR_DISTRIBUTION:-}" = "app-store" ] && ZIP="$OUT/AgentEar-MAS-$VERSION-macos-arm64-local.zip"
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
