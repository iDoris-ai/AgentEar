#!/usr/bin/env bash
# AgentEar × 真 agent24d 端到端（A3 附着，P2 交付判据）。
#
# 规格：Agent24 docs/design/A3-ATTACHED-MODULE.md v2（@68c2412，#524）。本脚本**不 import 任何
# Agent24 代码**，只调它的二进制与公开端点；Agent24 端点/CLI 不存在时明确 FAIL，不静默通过。
#
# 全程隔离：Agent24 用临时 HOME（它的状态目录 = $HOME/.agent24），AgentEar 用临时 AGENTEAR_DATA；
# **不碰** ~/.agent24、~/.agentear，**不启动** AgentEar 守护进程（不截按键）。
#
# 用法：
#   scripts/e2e-agent24.sh
# 环境变量（都可省）：
#   AGENT24_BIN / AGENT24_CLI   已构建的 agent24d / agent24（缺省：$A24_BUILD/target/release/）
#   A24_BUILD                   Agent24 构建目录（缺省 /private/tmp/claude-502/a24build）
#   AGENT24_REPO + BUILD_A24=1  没有现成二进制时，从 $AGENT24_REPO 的 origin/main `git archive` 到
#                               $A24_BUILD/src 再构建（不在 Agent24 仓库里建 worktree、不改它）
#   AGENTEAR_BIN                缺省：本仓库 target/release/agentear
#   AGENTEAR_VENDOR             ASR 二进制与模型目录（缺省：本仓库 vendor/）
#   LOCAL_LLM_URL / LOCAL_LLM_KEY / LOCAL_MODEL
#                               本地 provider（缺省 oMLX http://127.0.0.1:8088、key xiaobao8088、
#                               Qwen3-0.6B-4bit），交给 agent24d 的 OMLX_URL / OMLX_API_KEY / DEFAULT_MODEL
#   （外部计数桩监听 127.0.0.1，但以 IPv4-mapped 地址 http://[::ffff:127.0.0.1]:<port> 交给
#    OLLAMA_URL——Agent24 路由器把 v4-mapped 判为非本地、落 Remote 层；流量实际不出本机，
#    也不依赖本机有 LAN 网卡。同 Agent24 rust/apps/agent24d/tests/me4_model_blackbox.rs 的做法）
#   PYTHON                      辅助脚本用的解释器（缺省自动挑 ≥3.9，只用标准库）
#   KEEP=1                      失败/结束后保留临时目录便于排查
# 退出码：0 全部通过；1 有断言失败（含「Agent24 侧还没有该能力」）；77 前置条件缺失（SKIP）。
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "${HERE}/.." && pwd)"
A24_BUILD="${A24_BUILD:-/private/tmp/claude-502/a24build}"
AGENT24_BIN="${AGENT24_BIN:-${A24_BUILD}/target/release/agent24d}"
AGENT24_CLI="${AGENT24_CLI:-${A24_BUILD}/target/release/agent24}"
AGENTEAR_BIN="${AGENTEAR_BIN:-${REPO}/target/release/agentear}"
AGENTEAR_VENDOR="${AGENTEAR_VENDOR:-${REPO}/vendor}"
LOCAL_LLM_URL="${LOCAL_LLM_URL:-http://127.0.0.1:8088}"
LOCAL_LLM_KEY="${LOCAL_LLM_KEY:-xiaobao8088}"
LOCAL_MODEL="${LOCAL_MODEL:-Qwen3-0.6B-4bit}"
MANIFEST="${REPO}/assets/agent24/domain-os.yml"
KEEP="${KEEP:-0}"

PASS=0
FAIL=0
STEP=""
T=""
PIDS=""

say_() { printf '%s\n' "$*"; }
step() { STEP="$1"; say_ ""; say_ "── ${STEP}"; }
ok() { PASS=$((PASS + 1)); say_ "  ✅ $*"; }
bad() { FAIL=$((FAIL + 1)); say_ "  ❌ $*"; }
skip_all() { say_ ""; say_ "SKIP（前置条件缺失）：$*"; exit 77; }
# Agent24 侧缺能力：记一条失败并立刻结束（后面的步骤都依赖它），明确指出卡在哪个 PR。
blocked() { bad "$*"; finish; }

cleanup() {
  local p
  for p in ${PIDS}; do
    kill -TERM "${p}" 2>/dev/null || true
  done
  sleep 1
  for p in ${PIDS}; do
    kill -KILL "${p}" 2>/dev/null || true
  done
  if [ -n "${T}" ] && [ -d "${T}" ]; then
    if [ "${KEEP}" = "1" ]; then
      say_ "（KEEP=1：临时目录保留在 ${T}）"
    else
      rm -rf "${T}"
    fi
  fi
}
trap cleanup EXIT

finish() {
  say_ ""
  say_ "== 结果：通过 ${PASS}，失败 ${FAIL}（最后一步：${STEP}）=="
  if [ "${FAIL}" -eq 0 ]; then exit 0; else exit 1; fi
}

# ---------- S0 前置 ----------
step "S0 前置条件"
pick_python() {
  local c
  for c in "${PYTHON:-}" python3.12 python3.11 python3 /usr/bin/python3; do
    [ -n "${c}" ] || continue
    command -v "${c}" >/dev/null 2>&1 || continue
    if "${c}" -c 'import sys; sys.exit(0 if sys.version_info >= (3, 9) else 1)' 2>/dev/null; then
      printf '%s' "${c}"
      return 0
    fi
  done
  return 1
}
PY="$(pick_python)" || skip_all "找不到 Python ≥3.9（辅助脚本只用标准库）"
say_ "  Python：${PY}"
for bin in say afconvert curl; do
  command -v "${bin}" >/dev/null 2>&1 || skip_all "缺 ${bin}"
done

if [ ! -x "${AGENT24_BIN}" ] || [ ! -x "${AGENT24_CLI}" ]; then
  if [ "${BUILD_A24:-0}" = "1" ] && [ -n "${AGENT24_REPO:-}" ]; then
    say_ "  构建 Agent24（origin/main → ${A24_BUILD}）……"
    rm -rf "${A24_BUILD}/src"
    mkdir -p "${A24_BUILD}/src"
    git -C "${AGENT24_REPO}" fetch -q origin main
    git -C "${AGENT24_REPO}" archive origin/main | tar -x -C "${A24_BUILD}/src"
    git -C "${AGENT24_REPO}" rev-parse origin/main >"${A24_BUILD}/SOURCE"
    (cd "${A24_BUILD}/src/rust" && CARGO_TARGET_DIR="${A24_BUILD}/target" cargo build --release -q -p agent24d -p agent24-cli)
  else
    skip_all "没有 agent24d/agent24 二进制（设 AGENT24_BIN/AGENT24_CLI，或 BUILD_A24=1 AGENT24_REPO=<Agent24 仓库>）"
  fi
fi
[ -x "${AGENTEAR_BIN}" ] || skip_all "没有 ${AGENTEAR_BIN}（先 cargo build --release）"
[ -d "${AGENTEAR_VENDOR}/bin" ] || skip_all "没有 ASR vendor 目录 ${AGENTEAR_VENDOR}（设 AGENTEAR_VENDOR）"
[ -f "${MANIFEST}" ] || skip_all "没有 ${MANIFEST}"
say_ "  agent24d：${AGENT24_BIN}（$("${AGENT24_CLI}" --version 2>/dev/null || echo '?')）"
if [ -f "${A24_BUILD}/SOURCE" ]; then say_ "  Agent24 源码：$(cat "${A24_BUILD}/SOURCE")"; fi
say_ "  agentear：${AGENTEAR_BIN}"

# IPv6 栈必须能把 [::ffff:127.0.0.1] 送到 IPv4 回环上的监听者，否则计数桩收不到任何东西，
# 「计数 = 0」就成了假绿。用一个临时 127.0.0.1 监听者实测一次。
if ! "${PY}" - <<'PYEOF' 2>/dev/null
import socket, sys
srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
srv.bind(("127.0.0.1", 0)); srv.listen(1)
port = srv.getsockname()[1]
c = socket.socket(socket.AF_INET6, socket.SOCK_STREAM)
c.settimeout(2)
try:
    c.connect(("::ffff:127.0.0.1", port))
except OSError:
    sys.exit(1)
srv.settimeout(2); srv.accept()
PYEOF
then
  skip_all "IPv6 栈不可用：连 [::ffff:127.0.0.1] 不通（远端计数桩靠 v4-mapped 地址被判成 Remote 层）"
fi

if ! curl -s -m 3 -o /dev/null -w '%{http_code}' -H "Authorization: Bearer ${LOCAL_LLM_KEY}" "${LOCAL_LLM_URL}/v1/models" | grep -q 200; then
  skip_all "本地 provider ${LOCAL_LLM_URL} 不可用"
fi
ok "前置齐全（本地 provider ${LOCAL_LLM_URL}/${LOCAL_MODEL}；IPv6 v4-mapped 可达）"

T="$(mktemp -d /private/tmp/claude-502/e2e-a24.XXXXXX 2>/dev/null || mktemp -d)"
A24_HOME="${T}/a24home"
AE_DATA="${T}/ae"
mkdir -p "${A24_HOME}" "${AE_DATA}"
# 对话 TTS 用 macOS say：端到端只验链路，不依赖 TTS 边车是否在跑。
printf '%s\n' '{"talk_tts_engine": "say", "talk_lang": "zh"}' >"${AE_DATA}/config.json"

# ---------- S1 外部计数桩 ----------
step "S1 起外部计数桩（监听 127.0.0.1，以 [::ffff:127.0.0.1] 交给 Agent24 → Remote 层）"
"${PY}" "${HERE}/e2e-agent24/count_stub.py" --host 127.0.0.1 \
  --count-file "${T}/stub.count" --port-file "${T}/stub.port" >"${T}/stub.log" 2>&1 &
PIDS="${PIDS} $!"
for _ in $(seq 1 50); do [ -s "${T}/stub.port" ] && break; sleep 0.1; done
[ -s "${T}/stub.port" ] || blocked "计数桩没起来：$(cat "${T}/stub.log")"
STUB_URL="http://[::ffff:127.0.0.1]:$(cat "${T}/stub.port")"
ok "计数桩 ${STUB_URL}"
stub_count() { cat "${T}/stub.count" 2>/dev/null || echo "?"; }

# ---------- S2 agent24d ----------
start_daemon() { # $1 = OMLX_URL
  : >"${T}/daemon.out"
  env HOME="${A24_HOME}" OMLX_URL="$1" OMLX_API_KEY="${LOCAL_LLM_KEY}" DEFAULT_MODEL="${LOCAL_MODEL}" \
    OLLAMA_URL="${STUB_URL}" RUST_LOG=info \
    "${AGENT24_BIN}" serve --port 0 >"${T}/daemon.out" 2>"${T}/daemon.err" &
  DAEMON_PID=$!
  PIDS="${PIDS} ${DAEMON_PID}"
  local i
  for i in $(seq 1 150); do
    grep -q '"type":"ready"' "${T}/daemon.out" 2>/dev/null && break
    kill -0 "${DAEMON_PID}" 2>/dev/null || break
    sleep 0.1
  done
  grep -q '"type":"ready"' "${T}/daemon.out" 2>/dev/null || return 1
  local line
  line="$(grep -m1 '"type":"ready"' "${T}/daemon.out")"
  A24_PORT="$(printf '%s' "${line}" | "${PY}" -c 'import json,sys;print(json.load(sys.stdin)["port"])')"
  A24_TOKEN="$(printf '%s' "${line}" | "${PY}" -c 'import json,sys;print(json.load(sys.stdin)["token"])')"
  return 0
}
stop_daemon() {
  kill -TERM "${DAEMON_PID}" 2>/dev/null || true
  local i
  for i in $(seq 1 100); do kill -0 "${DAEMON_PID}" 2>/dev/null || return 0; sleep 0.1; done
  kill -KILL "${DAEMON_PID}" 2>/dev/null || true
}
api() { # $1 method $2 path [$3 body-file]
  if [ -n "${3:-}" ]; then
    curl -s -m 15 -o "${T}/api.body" -w '%{http_code}' -X "$1" -H "Authorization: Bearer ${A24_TOKEN}" \
      -H 'Content-Type: application/json' --data-binary @"$3" "http://127.0.0.1:${A24_PORT}$2"
  else
    curl -s -m 15 -o "${T}/api.body" -w '%{http_code}' -X "$1" -H "Authorization: Bearer ${A24_TOKEN}" \
      "http://127.0.0.1:${A24_PORT}$2"
  fi
}

step "S2 起 agent24d（隔离 HOME；OMLX_URL=本地，OLLAMA_URL=计数桩）"
start_daemon "${LOCAL_LLM_URL}" || blocked "agent24d 没打出 ready 行：$(tail -5 "${T}/daemon.err")"
ok "agent24d pid ${DAEMON_PID} 端口 ${A24_PORT}"

# ---------- S3 注册（A3 §3.2） ----------
step "S3 注册 AgentEar 为附着模块（agent24 os attach add --json）"
if ! env HOME="${A24_HOME}" "${AGENT24_CLI}" os attach --help >/dev/null 2>&1; then
  blocked "Agent24 CLI 没有 \`os attach\` 子命令 —— 等 Agent24 A3-2a（存储/REST/CLI）合并"
fi
if ! env HOME="${A24_HOME}" "${AGENT24_CLI}" os attach add "${MANIFEST}" --json >"${T}/attach.json" 2>"${T}/attach.err" </dev/null; then
  blocked "attach add 失败：$(cat "${T}/attach.err")"
fi
ATTACH_TOKEN="$("${PY}" -c 'import json,sys;print(json.load(open(sys.argv[1]))["token"])' "${T}/attach.json")"
ATTACH_SOCK="$("${PY}" -c 'import json,sys;print(json.load(open(sys.argv[1]))["socket_path"])' "${T}/attach.json")"
ATTACH_DIGEST="$("${PY}" -c 'import json,sys;print(json.load(open(sys.argv[1]))["manifest_digest"])' "${T}/attach.json")"
ATTACH_NAME="$("${PY}" -c 'import json,sys;print(json.load(open(sys.argv[1]))["name"])' "${T}/attach.json")"
printf '%s' "${ATTACH_TOKEN}" >"${T}/token"
chmod 600 "${T}/token"
LOCAL_DIGEST="sha256:$(shasum -a 256 "${MANIFEST}" | cut -d' ' -f1)"
[ "${ATTACH_NAME}" = "agentear" ] && ok "name = agentear" || bad "name = ${ATTACH_NAME}"
[ "${ATTACH_DIGEST}" = "${LOCAL_DIGEST}" ] && ok "digest 与本地 manifest 一致（${LOCAL_DIGEST}）" \
  || bad "digest 不一致：宿主 ${ATTACH_DIGEST} vs 本地 ${LOCAL_DIGEST}"
[ -S "${ATTACH_SOCK}" ] && ok "附着 socket 存在：${ATTACH_SOCK}" || bad "附着 socket 不存在：${ATTACH_SOCK}"
if grep -rq "${ATTACH_TOKEN}" "${A24_HOME}/.agent24" 2>/dev/null; then
  bad "明文 token 出现在 ${A24_HOME}/.agent24 下（C1：应只存哈希）"
else
  ok "宿主状态目录里没有明文 token"
fi

# ---------- S4 事件订阅 ----------
# 当前事件文件（S5 用 events.ndjson，S8 用 events2.ndjson，互不覆盖）。
EVENTS="${T}/events.ndjson"

step "S4 订阅 GET /api/v1/events（WS）"
"${PY}" "${HERE}/e2e-agent24/ws_collect.py" --port "${A24_PORT}" --token "${A24_TOKEN}" \
  --out "${EVENTS}" --ready-file "${T}/ws.ready" >"${T}/ws.log" 2>&1 &
PIDS="${PIDS} $!"
for _ in $(seq 1 50); do [ -s "${T}/ws.ready" ] && break; sleep 0.1; done
[ -s "${T}/ws.ready" ] && ok "WS 已连上" || blocked "WS 连不上：$(cat "${T}/ws.log")"

# 统计 WS 里 agentear 事件。agent24d 的 WS 帧真实形状：
#   {"v":1,"seq":N,"ts":..,"type":"module",
#    "payload":{"module":"agentear","kind":"agentear.event","payload":<完整 agentear.event/1>}}
# $1 = 事件 type（transcript/turn/speech/error…）；其后可跟若干 `字段=值`，
# 按**解析后的对象**比对 agentear.event/1 的 payload 字段（值按 JSON 解析：true / "x" / 1；
# 解析不了就当字符串），不依赖序列化空格。
ws_has() {
  "${PY}" - "${EVENTS}" "$@" <<'PYEOF'
import json, sys
path, want, conds = sys.argv[1], sys.argv[2], sys.argv[3:]
def val(v):
    try:
        return json.loads(v)
    except ValueError:
        return v
pairs = [(c.split("=", 1)[0], val(c.split("=", 1)[1])) for c in conds]
n = 0
for line in open(path, encoding="utf-8", errors="replace"):
    try:
        m = json.loads(line)
    except ValueError:
        continue
    if m.get("type") != "module":
        continue
    outer = m.get("payload") or {}
    if outer.get("module") != "agentear" or outer.get("kind") != "agentear.event":
        continue
    ev = outer.get("payload") or {}
    if ev.get("schema") != "agentear.event/1" or ev.get("type") != want:
        continue
    body = ev.get("payload") or {}
    if all(body.get(k) == v for k, v in pairs):
        n += 1
print(n)
PYEOF
}
wait_ws() { # $1 超时秒；其余参数同 ws_has
  local secs="$1" i
  shift
  for i in $(seq 1 $((secs * 10))); do
    [ "$(ws_has "$@")" -gt 0 ] && return 0
    sleep 0.1
  done
  return 1
}

# ---------- S5 一轮对话 ----------
step "S5 AgentEar 附着跑一轮（--talk-turn --host a3，linger 25s 接反向命令）"
say -v Tingting -o "${T}/q.aiff" "今天天气怎么样" 2>/dev/null || say -o "${T}/q.aiff" "今天天气怎么样"
afconvert -f WAVE -d LEI16@16000 -c 1 "${T}/q.aiff" "${T}/q.wav"
env AGENTEAR_DATA="${AE_DATA}" AGENTEAR_VENDOR="${AGENTEAR_VENDOR}" AGENTEAR_A3_LINGER_SECS=25 \
  "${AGENTEAR_BIN}" --talk-turn "${T}/q.wav" --lang zh --host a3 \
  --a3-socket "${ATTACH_SOCK}" --a3-token-file "${T}/token" >"${T}/ae.out" 2>"${T}/ae.err" &
AE_PID=$!
PIDS="${PIDS} ${AE_PID}"
for _ in $(seq 1 100); do grep -q '已附着到 Agent24' "${T}/ae.out" 2>/dev/null && break; kill -0 "${AE_PID}" 2>/dev/null || break; sleep 0.1; done
if grep -q '已附着到 Agent24' "${T}/ae.out"; then
  ok "握手成功：$(grep -m1 '已附着到 Agent24' "${T}/ae.out")"
  grep -m1 '已附着到 Agent24' "${T}/ae.out" | grep -q '_a24/model/' && ok "offer 含 _a24/model/" \
    || bad "offer 不含 _a24/model/（宿主没授予模型，C2 正对照不成立）"
else
  blocked "握手失败：$(tail -5 "${T}/ae.err")"
fi
wait_ws 60 transcript final=true && ok "WS 收到 transcript（final）" || bad "60s 内 WS 没收到 transcript"
wait_ws 30 turn phase=thinking && ok "WS 收到 turn thinking" || bad "WS 没收到 turn thinking"
wait_ws 90 turn phase=idle && ok "WS 收到 turn idle（本轮结束）" || bad "WS 没收到 turn idle"
if grep -qE 'privacy_denied|tier.?=.?remote' "${T}/ae.err" "${T}/ae.out" 2>/dev/null; then
  bad "AgentEar 报了隐私违例/远端 tier"
fi

# ---------- S6 反向命令（A3 §6） ----------
step "S6 反向命令：POST /api/v1/os/agentear/commands/speak"
printf '%s\n' '{"schema":"agentear.command/1","command_id":"e2e-speak-1","type":"speak","payload":{"text":"这是 Agent24 让我说的一句话。","lang":"zh-CN"}}' >"${T}/speak.json"
CODE="$(api POST /api/v1/os/agentear/commands/speak "${T}/speak.json" || true)"
case "${CODE}" in
  200) ok "speak → 200 $(cat "${T}/api.body")" ;;
  404 | 405) bad "speak → ${CODE}：宿主没有反向命令路由 —— 等 Agent24 A3-3 合并" ;;
  *) bad "speak → ${CODE} $(cat "${T}/api.body" 2>/dev/null)" ;;
esac
if [ "${CODE}" = "200" ]; then
  wait_ws 40 speech state=completed command_id=e2e-speak-1 \
    && ok "WS 收到 speech completed（command_id=e2e-speak-1）" || bad "40s 内没收到 e2e-speak-1 的 speech completed"
  CODE2="$(api POST /api/v1/os/agentear/commands/speak "${T}/speak.json" || true)"
  [ "${CODE2}" = "200" ] && ok "同一 command_id 重发 → 200（幂等，不应再播一次）" || bad "重发 → ${CODE2}"
fi
for _ in $(seq 1 900); do kill -0 "${AE_PID}" 2>/dev/null || break; sleep 0.1; done
if wait "${AE_PID}"; then ok "agentear 正常退出"; else bad "agentear 退出码非 0：$(tail -3 "${T}/ae.err")"; fi

# ---------- S7 用量与隐私（C8） ----------
step "S7 用量 tier=local、外部计数桩 = 0"
CODE="$(api GET '/api/v1/usage?module=agentear' || true)"
if [ "${CODE}" = "200" ]; then
  LOCAL_OK="$("${PY}" -c 'import json,sys;print(json.load(open(sys.argv[1]))["by_served"]["local"]["calls_ok"])' "${T}/api.body")"
  REMOTE_OK="$("${PY}" -c 'import json,sys;print(json.load(open(sys.argv[1]))["by_served"]["remote"]["calls_ok"])' "${T}/api.body")"
  [ "${LOCAL_OK}" -ge 1 ] && ok "usage local.calls_ok = ${LOCAL_OK}" || bad "usage local.calls_ok = ${LOCAL_OK}（应 ≥1）"
  [ "${REMOTE_OK}" -eq 0 ] && ok "usage remote.calls_ok = 0" || bad "usage remote.calls_ok = ${REMOTE_OK}"
else
  bad "GET usage?module=agentear → ${CODE}"
fi
[ "$(stub_count)" = "0" ] && ok "外部计数桩 = 0" || bad "外部计数桩 = $(stub_count)（local_only 流量出了本机！）"
if grep -q '今天天气' "${T}/daemon.err" "${T}/daemon.out" 2>/dev/null; then
  bad "agent24d 日志里出现了转写原文（隐私：日志只记元数据）"
else
  ok "agent24d 日志里没有转写原文"
fi

# ---------- S8 负测：本地 provider 不可用 ----------
step "S8 负测：本地 provider 不可用 → unavailable 且计数桩仍 = 0"
stop_daemon
start_daemon "http://127.0.0.1:1" || blocked "负测用的 agent24d 没起来"
EVENTS="${T}/events2.ndjson"
: >"${EVENTS}"
"${PY}" "${HERE}/e2e-agent24/ws_collect.py" --port "${A24_PORT}" --token "${A24_TOKEN}" \
  --out "${EVENTS}" --ready-file "${T}/ws2.ready" >"${T}/ws2.log" 2>&1 &
PIDS="${PIDS} $!"
for _ in $(seq 1 50); do [ -s "${T}/ws2.ready" ] && break; sleep 0.1; done
# daemon 重启后 token 仍有效（A3 §5.4），socket 在同一路径重建。
for _ in $(seq 1 50); do [ -S "${ATTACH_SOCK}" ] && break; sleep 0.1; done
if env AGENTEAR_DATA="${AE_DATA}" AGENTEAR_VENDOR="${AGENTEAR_VENDOR}" \
  "${AGENTEAR_BIN}" --talk-turn "${T}/q.wav" --lang zh --host a3 \
  --a3-socket "${ATTACH_SOCK}" --a3-token-file "${T}/token" >"${T}/ae2.out" 2>"${T}/ae2.err"; then
  :
fi
grep -q '已附着到 Agent24' "${T}/ae2.out" && ok "daemon 重启后同一 token 重新握手成功" || bad "重启后握手失败：$(tail -3 "${T}/ae2.err")"
wait_ws 30 error code=unavailable && ok "WS 收到 error{code:unavailable}（本地不可用时失败关闭）" \
  || bad "WS 没收到 error{code:unavailable}（本地 provider 不可用时应失败关闭）"
wait_ws 30 turn phase=failed && ok "WS 收到 turn failed" || bad "WS 没收到 turn failed"
grep -q 'REMOTE-STUB-REPLY' "${T}/ae2.out" "${T}/ae2.err" && bad "AgentEar 念出了远端桩的回答！" || ok "没有用到远端回答"
[ "$(stub_count)" = "0" ] && ok "外部计数桩仍 = 0" || bad "外部计数桩 = $(stub_count)"

# ---------- S9 撤销 ----------
step "S9 撤销（agent24 os attach revoke agentear）"
if env HOME="${A24_HOME}" "${AGENT24_CLI}" os attach revoke agentear >"${T}/revoke.out" 2>&1 </dev/null; then
  ok "revoke 成功"
  if env AGENTEAR_DATA="${AE_DATA}" AGENTEAR_VENDOR="${AGENTEAR_VENDOR}" \
    "${AGENTEAR_BIN}" --talk-turn "${T}/q.wav" --lang zh --host a3 \
    --a3-socket "${ATTACH_SOCK}" --a3-token-file "${T}/token" >"${T}/ae3.out" 2>"${T}/ae3.err"; then
    bad "撤销后旧 token 仍能跑完一轮"
  fi
  grep -q 'auth_failed' "${T}/ae3.out" "${T}/ae3.err" && ok "撤销后旧 token 握手 auth_failed" \
    || bad "撤销后没看到 auth_failed：$(tail -2 "${T}/ae3.err")"
else
  bad "revoke 失败：$(cat "${T}/revoke.out")"
fi
stop_daemon
finish
