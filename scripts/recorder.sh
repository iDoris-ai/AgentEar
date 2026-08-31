#!/usr/bin/env bash
# 本地启动录音工具。
#
# 为什么要起个 HTTP 服务而不是直接双击 HTML：
#   `getUserMedia` 只在**安全上下文**里可用。`file://` 不算，
#   而 `http://127.0.0.1` 算（规范把 localhost 列为可信来源）。
#   直接开文件的话麦克风一定拿不到，而且报的错会指向权限，很难查。
#
# 用法：scripts/recorder.sh [端口]        端口默认 8899
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DOCS="$ROOT/docs"
PORT="${1:-8899}"

die() { echo "!! $*" >&2; exit 1; }

[[ "$PORT" =~ ^[0-9]+$ ]] && [ "$PORT" -ge 1024 ] && [ "$PORT" -le 65535 ] \
  || die "端口要是 1024–65535 的整数，当前是 '$PORT'"
command -v python3 >/dev/null || die "需要 python3（只用它的 http.server）"
for f in recorder.html recorder-core.js; do
  [ -f "$DOCS/$f" ] || die "缺文件：$DOCS/$f"
done

# 端口被占就直接说，别让人对着一个别人的页面纳闷。
# 用 python 而不是 nc：nc 不是每台机器都有，`command -v nc && nc -z ...`
# 在没有 nc 的机器上会**静默跳过整个检查** —— 一个不会失败的检查等于没检查。
# python3 上面已经确认存在，用它就没有这个口子。
if python3 - "$PORT" <<'PY'
import socket, sys
s = socket.socket()
try:
    s.bind(("127.0.0.1", int(sys.argv[1])))
except OSError:
    sys.exit(0)      # 绑不上 = 被占
finally:
    s.close()
sys.exit(1)          # 绑得上 = 空闲
PY
then
  die "端口 $PORT 已被占用。换一个：$(basename "$0") $((PORT+1))"
fi

URL="http://127.0.0.1:$PORT/recorder.html"
echo "==> 录音工具  $URL"
echo "    泰语语料页 http://127.0.0.1:$PORT/thai-recorder.html"
echo "    Ctrl-C 停止。音频只在本机，不上传任何地方。"
echo

# 只绑 127.0.0.1，不暴露到局域网
python3 -m http.server "$PORT" --bind 127.0.0.1 --directory "$DOCS" >/dev/null 2>&1 &
SRV=$!
# HUP 也要收：直接关掉终端窗口发的是 SIGHUP，不带它的话服务会留在后台，
# 下次再跑就撞「端口已被占用」，而人根本不知道是自己上次留下的
trap 'kill "$SRV" 2>/dev/null || true' EXIT INT TERM HUP

# 等它真的起来再开浏览器，否则会开出一个连接失败的页面
for _ in $(seq 1 40); do
  if curl -fsS -o /dev/null --max-time 1 "$URL" 2>/dev/null; then break; fi
  kill -0 "$SRV" 2>/dev/null || die "http.server 没起来"
  sleep 0.1
done
curl -fsS -o /dev/null --max-time 2 "$URL" 2>/dev/null || die "服务起来了但 $URL 打不开"

case "$(uname -s)" in
  Darwin) open "$URL" 2>/dev/null || true ;;
  Linux)  command -v xdg-open >/dev/null && xdg-open "$URL" >/dev/null 2>&1 || true ;;
esac

wait "$SRV"
