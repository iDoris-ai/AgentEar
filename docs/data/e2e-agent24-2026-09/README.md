# AgentEar × 真 agent24d 端到端（A3 附着，P2 交付判据）

`scripts/e2e-agent24.sh` 的原样输出（去掉临时路径/端口/pid 噪声），**脚本原样输出，判定行未改**。

| 文件 | Agent24 源码 | 结果 |
|---|---|---|
| `run5-b111ac4.md` | `b111ac4`（#532 A3-3，叠在 #529 A3-2b 上） | 28 / 0 |
| `run6-b111ac4.md` | 同上 | 29 / 0（加 S3 能力探测） |
| `final-main-604e198.md` | **main `604e198`**（A3 全部合并后），v0.25.1 发布包 | 5 次：4 次 29/0、1 次 24/3（根因未确认，见文件） |

**分步断言**（脚本 S0–S9）：S0 前置（Python、本地 provider、IPv6 v4-mapped 可达）→ S1 外部计数桩 →
S2 隔离 HOME 起 agent24d → S3 能力探测 + `os attach add --json`（name / digest / socket / 状态目录无明文 token）→
S4 WS 订阅 → S5 `--talk-turn --host a3`（握手、offer 含 `_a24/model/`、transcript final、turn thinking/idle）→
S6 `POST /api/v1/os/agentear/commands/speak`（200、speech completed、同 command_id 幂等）→
S7 usage local ≥1 / remote 0、计数桩 0、daemon 日志无转写原文 →
S8 负测（本地 provider 不可用：daemon 重启同 token 重连、error{unavailable}、turn failed、计数桩仍 0）→
S9 revoke 后旧 token auth_failed。

✅ **已对 Agent24 main 复验**（`final-main-604e198.md`）；前两份是对未合并分支的预跑。

重跑：`AGENT24_REPO=<Agent24 仓库> BUILD_A24=1 AGENT24_REF=origin/<分支> AGENTEAR_VENDOR=<vendor> scripts/e2e-agent24.sh`
（或用 `AGENT24_BIN` / `AGENT24_CLI` 指现成二进制）。不启动 AgentEar 守护进程，不碰 `~/.agent24`、`~/.agentear`。
