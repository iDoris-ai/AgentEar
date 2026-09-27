# E2E run6 —— 同 run5，加能力探测（S3 前 os attach list --json）

- 日期：2026-09-27
- Agent24 源码：`b111ac4`（同 run5）
- AgentEar：本 PR 的脚本（新增 S3 能力探测、AGENT24_REF 参数）+ a3_pair 能力探测
- 结果：**通过 29，失败 0**（多出的一项 = 能力探测）

```

── S0 前置条件
  Python：python3.12
  agent24d：<scratch>（agent24 0.3.0）
  Agent24 源码：b111ac4
  agentear：<agentear>/target/release/agentear
  ✅ 前置齐全（本地 provider http://127.0.0.1:8088/Qwen3-0.6B-4bit；IPv6 v4-mapped 可达）

── S1 起外部计数桩（监听 127.0.0.1，以 [::ffff:127.0.0.1] 交给 Agent24 → Remote 层）
  ✅ 计数桩 http://[::ffff:127.0.0.1]:<port>

── S2 起 agent24d（隔离 HOME；OMLX_URL=本地，OLLAMA_URL=计数桩）
  ✅ agent24d pid <pid> 端口 <port>

── S3 注册 AgentEar 为附着模块（agent24 os attach add --json）
  ✅ 能力探测：os attach list --json → {"modules":[...]}
  ✅ name = agentear
  ✅ digest 与本地 manifest 一致（sha256:a8893e2417ec3a5a7a060613c7bf2207fab3e8b67f767a6c0083752dc5ea99d9）
  ✅ 附着 socket 存在：<tmp>/a24home/.agent24/attach/agent24d.sock
  ✅ 宿主状态目录里没有明文 token

── S4 订阅 GET /api/v1/events（WS）
  ✅ WS 已连上

── S5 AgentEar 附着跑一轮（--talk-turn --host a3，linger 25s 接反向命令）
  ✅ 握手成功：已附着到 Agent24（offer：_a24/events/, _a24/model/）
  ✅ offer 含 _a24/model/
  ✅ WS 收到 transcript（final）
  ✅ WS 收到 turn thinking
  ✅ WS 收到 turn idle（本轮结束）

── S6 反向命令：POST /api/v1/os/agentear/commands/speak
  ✅ speak → 200 {"result":{"accepted":true}}
  ✅ WS 收到 speech completed（command_id=e2e-speak-1）
  ✅ 同一 command_id 重发 → 200（幂等，不应再播一次）
  ✅ agentear 正常退出

── S7 用量 tier=local、外部计数桩 = 0
  ✅ usage local.calls_ok = 1
  ✅ usage remote.calls_ok = 0
  ✅ 外部计数桩 = 0
  ✅ agent24d 日志里没有转写原文

── S8 负测：本地 provider 不可用 → unavailable 且计数桩仍 = 0
  ✅ daemon 重启后同一 token 重新握手成功
  ✅ WS 收到 error{code:unavailable}（本地不可用时失败关闭）
  ✅ WS 收到 turn failed
  ✅ 没有用到远端回答
  ✅ 外部计数桩仍 = 0

── S9 撤销（agent24 os attach revoke agentear）
  ✅ revoke 成功
  ✅ 撤销后旧 token 握手 auth_failed

== 结果：通过 29，失败 0（最后一步：S9 撤销（agent24 os attach revoke agentear））==
```
