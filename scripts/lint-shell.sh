#!/usr/bin/env bash
# 挡住一个在这台机器上反复出现的坑：**macOS 自带的是 bash 3.2**，
# 它会把紧跟在 `$VAR` 后面的全角字符（中文括号、破折号、省略号……）
# 的首字节当成变量名的一部分，报 `unbound variable`。
#
# 症状极具迷惑性：脚本在 zsh 里手测正常，用 bash 跑就在一行 echo 上崩，
# 而报错指的是一个你根本没写过的变量名。
#
# 本仓库的注释和输出全是中文，所以这个坑**天然高频**——
# 2026-09-02 一天之内踩了两次。
#
# 用法：scripts/lint-shell.sh   （无输出 = 干净）

set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

bad=0
for f in "$ROOT"/scripts/*.sh; do
  # 裸 $VAR 后面紧跟非 ASCII 字节
  if hits="$(grep -nP '\$[A-Za-z_]\w*(?=[^\x00-\x7f])' "$f" 2>/dev/null)"; then
    echo "!! $(basename "$f") 有裸 \$VAR 紧跟全角字符（bash 3.2 会当成变量名的一部分）：" >&2
    echo "$hits" | sed 's/^/     /' >&2
    echo "   修法：写成 \${VAR}" >&2
    bad=1
  fi
  # 顺带做语法检查
  bash -n "$f" || bad=1
done
[ "$bad" = 0 ] && echo "shell 脚本检查通过"
exit "$bad"
