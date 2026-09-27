# 对 Agent24 main 的最终复验（A3 全部合并后）

- Agent24 源码：`604e1989757a34163493d836ee005c4f8f8622d5`（main，已含 #524/#526/#527/#529/#532）
- AgentEar：**v0.25.1 发布包**里的二进制（`dist/AgentEar.app/Contents/MacOS/AgentEar`，tag `v0.25.1` = `50bec33`）
- 日期：2026-09-27

## 结果：5 次运行，4 次 29/0，1 次 24/3

| # | 结果 | 备注 |
|---|---|---|
| 1 | 24 / 3 | 构建完后的第一次运行。失败：S5 turn idle、S6 speak 503 module_not_ready、S7 local.calls_ok=0 |
| 2 | 29 / 0 | |
| 3 | 29 / 0 | |
| 4 | 29 / 0 | |
| 5 | 29 / 0 | |

⚠️ **第 1 次失败的根因未确认**：那次没有保留临时目录（没开 `KEEP=1`），日志已随目录删除。
三处失败都指向「S5 那一轮的本地模型调用没成功」（没有 local 用量记录、这一轮没走到 idle，
AgentEar 在 S6 之前已断开）。**推测**是本地 oMLX 在长时间空闲后首次请求冷启动过慢，**未验证**。
后 4 次连续全过（其中 1 次 `KEEP=1` 的日志显示宿主推理 0.83 s、tier Local）。
把它记为已知的偶发不稳，不写成「稳定通过」。复现时请用 `KEEP=1` 保留现场。

**Agent24 侧已登记跟进**（agent24-13，2026-09-27）：复现「刚构建完第一次跑」并保留 daemon 日志定根因。Agent24 确认 daemon 侧**没有**「启动后首个 model/complete 被拒 / 健康表未就绪」这类已知行为；已知事实只有：内核 model/complete 超时 120 s、oMLX 8B 冷加载实测约 5 s、AgentEar talk_timeout 60 s。若那一轮拿到的是 `unavailable` 而非 `timeout`，更像 provider 探活失败而非超时——但那次没有日志，**无法区分**。

## 通过时各步（第 2–5 次相同）

S0 前置 ✅ · S1 计数桩（v4-mapped → Remote 层）✅ · S2 隔离 HOME 起 agent24d ✅ ·
S3 能力探测 `os attach list --json` + `os attach add --json`（name / digest `sha256:a8893e24…99d9` / socket / 无明文 token）✅ ·
S4 WS ✅ · S5 握手 offer `[_a24/events/, _a24/model/]`、transcript final、turn thinking/idle ✅ ·
S6 speak → 200 `{"result":{"accepted":true}}`、speech completed、同 command_id 幂等 ✅ ·
S7 usage local ≥1 / remote 0、计数桩 0、daemon 日志无转写原文 ✅ ·
S8 本地不可用 → 重启同 token 重连、error{unavailable}、turn failed、计数桩仍 0 ✅ ·
S9 revoke → 旧 token auth_failed ✅
