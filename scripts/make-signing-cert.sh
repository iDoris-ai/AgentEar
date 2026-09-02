#!/usr/bin/env bash
# 创建一个自签的代码签名证书，供 scripts/bundle.sh 使用。
#
# ## 为什么需要这个
#
# ad-hoc 签名（`codesign --sign -`）没有稳定的代码标识：每次签名都产生新的
# cdhash。而 macOS 的 TCC 把「辅助功能」授权钉死在 cdhash 上，于是**每次重新
# 打包，授权都会失效**——而且症状很有欺骗性：系统设置里开关看着是开的、
# TCC 库里 auth_value=2，程序却报「未授予」并降级到 Ctrl+Shift+R。
#
# 换成固定证书后，designated requirement 变成
#   identifier "ai.idoris.agentear" and certificate leaf = H"<证书哈希>"
# 证书不变 → DR 不变 → 授权一次就一直有效。
#
# ## 这个证书能做什么、不能做什么
#
# 能：消除重复授权、让本机运行不被拦。
# **不能**：让别人下载后不被 Gatekeeper 拦。自签证书只在本机受信任，
# 对外分发仍然需要 Apple Developer ID + 公证（notarization）。
#
# ## 用法
#
#   scripts/make-signing-cert.sh          # 创建（已存在则跳过）
#   scripts/make-signing-cert.sh --force  # 重建（会让现有授权失效一次）
#
# 中途会弹一次系统对话框要登录密码——那是在修改证书信任设置，正常。

set -euo pipefail

NAME="${AGENTEAR_SIGN_ID:-AgentEar Local Signing}"
KEYCHAIN="$HOME/Library/Keychains/login.keychain-db"
FORCE=0
[ "${1:-}" = "--force" ] && FORCE=1

if [ "$FORCE" -eq 0 ] && security find-identity -v -p codesigning 2>/dev/null | grep -qF "$NAME"; then
  echo "✅ 签名身份「${NAME}」已存在，无需创建"
  security find-identity -v -p codesigning | grep -F "$NAME" | sed 's/^/   /'
  exit 0
fi

WORK="$(mktemp -d)"
# 私钥落在临时目录里，无论成功失败都要清掉
trap 'rm -rf "$WORK"' EXIT

# macOS 自带的是 LibreSSL，不支持 `req -addext`，扩展必须写进配置文件
cat > "$WORK/cert.cnf" <<CNF
[ req ]
distinguished_name = dn
x509_extensions    = v3_codesign
prompt             = no

[ dn ]
CN = $NAME
O  = AgentEar
C  = US

[ v3_codesign ]
basicConstraints     = critical,CA:FALSE
keyUsage             = critical,digitalSignature
extendedKeyUsage     = critical,codeSigning
subjectKeyIdentifier = hash
CNF

echo "==> 生成密钥与自签证书（有效期 10 年）"
openssl req -x509 -newkey rsa:2048 -sha256 -days 3650 -nodes \
  -config "$WORK/cert.cnf" -keyout "$WORK/key.pem" -out "$WORK/cert.pem" 2>/dev/null

echo "==> 导入登录钥匙串"
# -T /usr/bin/codesign：允许 codesign 使用这把私钥，否则每次签名都弹钥匙串授权
openssl pkcs12 -export -out "$WORK/id.p12" -inkey "$WORK/key.pem" -in "$WORK/cert.pem" \
  -passout pass:agentear -name "$NAME" 2>/dev/null
security import "$WORK/id.p12" -k "$KEYCHAIN" -P agentear \
  -T /usr/bin/codesign -T /usr/bin/security | sed 's/^/    /'

echo "==> 设为受信任的代码签名根（会弹窗要登录密码）"
# 不加 -d：只写当前用户的信任设置，不动系统域，不需要 sudo
security add-trusted-cert -r trustRoot -p codeSign -k "$KEYCHAIN" "$WORK/cert.pem"

echo
if security find-identity -v -p codesigning | grep -qF "$NAME"; then
  echo "✅ 完成。scripts/bundle.sh 会自动用它签名"
  security find-identity -v -p codesigning | grep -F "$NAME" | sed 's/^/   /'
  echo
  echo "下次打包后还需要**最后再授权一次**辅助功能（因为签名身份变了），"
  echo "之后就不用了："
  echo "  tccutil reset Accessibility ai.idoris.agentear"
else
  echo "!! 证书创建了但不受信任，检查上一步的弹窗是否被取消" >&2
  exit 1
fi
