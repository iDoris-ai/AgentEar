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

# 裸 $VAR 后面紧跟非 ASCII 字节。
#
# ⚠️ 2026-10-03 发现：原来这里写的是 `grep -nP '...(?=[^\x00-\x7f])'`——
# **macOS 自带的 `/usr/bin/grep` 不认 `-P`**（没有 PCRE 支持），而这个脚本
# 是 `#!/usr/bin/env bash` 跑的非交互子进程，不会带上交互式 zsh 里那个把
# `grep` 转发到 ugrep 的 shell 函数——于是 `grep -P` 在真实的 `bash
# scripts/lint-shell.sh` 下会报错退出，`2>/dev/null` 又把这个错误吞掉，
# `if hits=...` 看到的是「没匹配」，**这条检查在普通 Mac 上一直是假绿的**
# （本机交互式手测会因为那个 ugrep 转发函数而看起来正常，这正是它藏了这么
# 久没被发现的原因）。改成不需要 `-P` 的写法。
#
# ⚠️ PR #106 评审非阻塞④：第一版改法用 `[^ -~]`（ASCII 可打印范围之外的
# 字节），但这个范围**连 tab/CR 之类的 ASCII 控制字符也算进去**——
# `echo "$VAR<tab>..."`（比如用 tab 对齐的表格）会被当成「全角字符坑」误报，
# 而那种情况下 bash 3.2 根本不会崩（`\t` 不是多字节字符的首字节）。
# 改成 `[^[:print:][:space:]]`：POSIX 字符类，**同时排除**可打印 ASCII
# 和所有空白类字符（含 tab/CR/LF），只留下「既不可打印也不是空白」的字节——
# 在 `LC_ALL=C` 下这正好就是 0x80 以上的非 ASCII 首字节。
PATTERN='\$[A-Za-z_][A-Za-z0-9_]*[^[:print:][:space:]]'

# 自检：这条正则该抓什么、不该抓什么——改坏了就先在这里炸，别拿一条
# 没验证过的正则去扫真文件（第一版的 `grep -P` 坑就是这么混进来的）。
_selftest() {
  local tmp
  tmp="$(mktemp "${TMPDIR:-/tmp}/lint-shell-selftest.XXXXXX")"
  printf 'echo "$VAR\there"\n' > "$tmp"
  if LC_ALL=C grep -nE "$PATTERN" "$tmp" >/dev/null 2>&1; then
    echo "!! lint-shell 自检失败：tab 被误报成全角字符坑（正则改坏了）" >&2
    rm -f "$tmp"
    exit 2
  fi
  printf 'echo "${VAR}好"\n' > "$tmp"
  if LC_ALL=C grep -nE "$PATTERN" "$tmp" >/dev/null 2>&1; then
    echo "!! lint-shell 自检失败：\${VAR}（已经加花括号）被误报（正则改坏了）" >&2
    rm -f "$tmp"
    exit 2
  fi
  # 全角括号的字面字节故意不直接写进这个脚本的源码——否则这一行本身就会被
  # 下面的扫描逻辑当成一个真的坑抓到（自检用例把自己也测进去了）。用 printf
  # 现场拼出来，源码里看到的只是 `$VAR%s`（`%` 是可打印 ASCII，不会误报）。
  local fw_paren
  fw_paren="$(printf '\xef\xbc\x88test\xef\xbc\x89')"
  printf 'echo "$VAR%s"\n' "$fw_paren" > "$tmp"
  if ! LC_ALL=C grep -nE "$PATTERN" "$tmp" >/dev/null 2>&1; then
    echo "!! lint-shell 自检失败：全角字符没被抓到（正则改坏了）" >&2
    rm -f "$tmp"
    exit 2
  fi
  rm -f "$tmp"
}
_selftest

bad=0
for f in "$ROOT"/scripts/*.sh; do
  if hits="$(LC_ALL=C grep -nE "$PATTERN" "$f" 2>/dev/null)"; then
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
