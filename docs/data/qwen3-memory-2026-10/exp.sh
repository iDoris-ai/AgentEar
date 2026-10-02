#!/bin/bash
# 用法：BIN=<speech-server> TAG=<名字> [AGENTEAR_MLX_CACHE_MB=256] exp.sh <wav 列表>
# 输出：每段 秒数 / 转写耗时 / 请求中峰值 / 转完后 footprint；文字存 $OUT/<TAG>/NN.txt
R=~/.agentear/models/qwen3; OUT=${OUT:-$(dirname "$0")/exp}; mkdir -p "$OUT/$TAG"
PORT=$(python3 -c 'import socket;s=socket.socket();s.bind(("127.0.0.1",0));print(s.getsockname()[1])')
QWEN3_ASR_CACHE_DIR=$R/cache /usr/bin/sandbox-exec -p '(version 1)(allow default)(deny network-outbound (remote tcp "*:*"))(allow network-outbound (remote unix-socket))' "$BIN" --host 127.0.0.1 --port $PORT >"$OUT/$TAG/server.log" 2>&1 &
P=$!
for i in $(seq 100); do curl -s -m1 -o /dev/null http://127.0.0.1:$PORT/health && break; sleep 0.2; done
fpmb() { footprint $P 2>/dev/null | awk '/Footprint:/{v=$(NF-5);u=$(NF-4); if(u=="GB")v*=1024; else if(u=="KB")v/=1024; printf "%d", v}'; }
i=0
while read -r f; do
  i=$((i+1)); n=$(printf %02d $i); d=$(afinfo "$f" | awk '/estimated duration/{printf "%.1f",$3}')
  : > "$OUT/$TAG/peak"; ( while :; do fpmb >> "$OUT/$TAG/peak"; echo >> "$OUT/$TAG/peak"; sleep 0.2; done ) & SP=$!
  t0=$(python3 -c 'import time;print(time.time())')
  curl -sS -m 300 -F "file=@$f" -F model=qwen3-asr-0.6b-mlx-int4 http://127.0.0.1:$PORT/v1/audio/transcriptions \
    | python3 -c 'import sys,json;print(json.load(sys.stdin).get("text",""))' > "$OUT/$TAG/$n.txt" 2>/dev/null
  t1=$(python3 -c 'import time;print(time.time())')
  kill $SP 2>/dev/null; wait $SP 2>/dev/null
  pk=$(sort -n "$OUT/$TAG/peak" | tail -1)
  printf '%s\t%s\t%.2f\t%s\t%s\n' $n $d $(echo "$t1-$t0"|bc) "$pk" "$(fpmb)"
done < "$1"
kill $P; wait $P 2>/dev/null; true
