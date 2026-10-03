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
  # 裸 $VAR 后面紧跟非 ASCII 字节。
  #
  # ⚠️ 2026-10-03 发现：原来这里写的是 `grep -nP '...(?=[^\x00-\x7f])'`——
  # **macOS 自带的 `/usr/bin/grep` 不认 `-P`**（没有 PCRE 支持），而这个脚本
  # 是 `#!/usr/bin/env bash` 跑的非交互子进程，不会带上交互式 zsh 里那个把
  # `grep` 转发到 ugrep 的 shell 函数——于是 `grep -P` 在真实的 `bash
  # scripts/lint-shell.sh` 下会报错退出，`2>/dev/null` 又把这个错误吞掉，
  # `if hits=...` 看到的是「没匹配」，**这条检查在普通 Mac 上一直是假绿的**
  # （本机交互式手测会因为那个 ugrep 转发函数而看起来正常，这正是它藏了这么
  # 久没被发现的原因）。改成不需要 `-P` 的写法：用 POSIX 字符类
  # `[^ -~]`（ASCII 可打印范围之外的字节）直接匹配非 ASCII 的**首字节**，
  # `LC_ALL=C` 钉死按字节比较，不受系统 locale 影响。
  if hits="$(LC_ALL=C grep -nE '\$[A-Za-z_][A-Za-z0-9_]*[^ -~]' "$f" 2>/dev/null)"; then
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
