# AgentEar × Agent24 嵌入约定与联调流程

> 状态：实施约定草案。双方确认并把协议样例合入各自仓库后，才算冻结。  
> 范围：AgentEar 语音前端接入 Agent24；iDoris 是推理后端，不承载音频。  
> 配套设计：Agent24 `docs/design/INTEGRATION-AGENTEAR-IDORIS.md`（ADR-032）、[AgentEar ADR-0008](decisions/0008-voice-frontend-boundary.md)、[AgentEar ADR-0007](decisions/0007-realtime-voice-architecture.md)。

## 1. 目标和边界

用户可在 AgentEar 中说话，由 AgentEar 本机采集并转写；Agent24 组织对话、调用获准的模型和记忆、展示交互并执行获准的动作；AgentEar 将回复合成为语音并播放。两仓库保持独立，通过进程间协议集成，不互相导入对方源码或共享 Rust crate。

| 能力/数据 | 所有者 | 约定 |
|---|---|---|
| 热键、麦克风、TCC 授权、ASR、会话轮次、TTS、播放/停止 | AgentEar | 同一时刻只由 AgentEar 占用设备和热键；音频不离开本机 |
| Agent 生命周期、对话/工具流程、用户界面、记忆、提案审批与动作执行 | Agent24 | Agent24 是唯一审批与执行方；AgentEar 只转写、播报或提出提案 |
| 模型准入、隐私路由、预算、远程 provider 凭证与用量账本 | iDoris，经 Agent24 调用 | 附着模式的默认 `model_access` 为 `local_only`；远程访问必须由宿主策略显式授权 |

Agent24 不逐段中继音频，也不另建一套录音/播放状态机。AgentEar 不执行外部动作，不保存 Agent24 的长期会话记忆，不持有模型或业务凭证。语音文本只有在需要回答时才发给宿主；原始音频不发出 AgentEar。

## 2. 嵌入形态

采用 **附着式进程模块（A3）**：AgentEar 仍由用户安装/启动并持有 macOS 麦克风、辅助功能权限和全局热键；它可独立运行，也可向已运行的 Agent24 注册为语音模块。成功注册后，AgentEar 使用 Agent24 授予的 SDK capabilities；断开时按约定进入独立模式或显式显示“未连接”，不得悄悄把受 `local_only` 约束的请求改发远端。

附着注册协议是 Agent24 的前置交付，须先定义身份/token 发放与撤销、握手、模块 generation、重连/排空、单实例约束和反向命令路由。**不得把当前 OOP 模块的 fd 启动/EOF 退出规则直接套用到 A3**；两种生命周期不同。传输和握手以 Agent24 冻结的 `agent24-os-sdk`/协议为准，本文件不另造一套鉴权协议。

```text
麦克风/热键
    │
    ▼
AgentEar ── ASR 文本 / 模型请求 ──► Agent24 SDK ── OpenAI 兼容请求 ──► iDoris
    ▲                                  │                                  │
    └──── 播报/停止命令 ◄──────────────┘◄──────────── 回复文本 ────────────┘
```

## 3. 接口约定

### AgentEar → Agent24

- **模型**：使用 Agent24 SDK 的 `_a24/model/complete`，附着模式不直连 iDoris。请求按 Agent24 已冻结的 JSON-RPC schema 传 `messages`、`complexity`（默认 `simple`）及可选 `request_id`；第一阶段使用非流式回包。`model_access` 由已注册 manifest 决定，AgentEar 不得自行提高隐私级别。Agent24 当前回包包含 `text`、`model_id`、`tier` 和 `usage`；详细限制与错误以 `ME4-S2` 和 SDK 类型为准。
- **事件**：通过 SDK 的 `_a24/events/emit` 发布带 `agentear.event/1` 版本的事件。首批类型为 `turn`、`transcript`、`proposal`、`speech`、`error`。公共 envelope 至少含 `schema`、`event_id`、`session_id`、单调递增的 `seq`、`type`、`payload`。同一事件重试必须复用 `event_id`；宿主按 `(session_id, seq)` 去重并保持顺序。
- **转写**：`transcript.payload` 含 `text`、`lang`（BCP-47，如 `zh-CN`、`en-US`、`th-TH`）、`final: true`。当前只发最终转写，不伪造 partial/timestamp。不要把音频字节或原始音频路径放进事件、日志或审计。
- **提案**：`proposal.payload` 原样复用冻结的 `agentear.proposal/1` 对象；只允许 Agent24 显示、审批与执行。`needs_confirm: true` 时必须先由宿主确认；显示文本与执行输入必须来自同一对象。AgentEar 的 `--run-command` 不得由宿主调用。
- **记忆**：需要跨轮上下文时，由 AgentEar 调 Agent24 的 private memory capability；如该 capability 尚未授予或不可用，按单轮对话处理并说明，不自行建立第二份长期记忆。

建议事件样例（最终字段以双方提交的 JSON Schema/SDK 类型为准）：

```json
{
  "schema": "agentear.event/1",
  "event_id": "evt_01J...",
  "session_id": "ses_01J...",
  "seq": 3,
  "type": "transcript",
  "payload": { "text": "今天天气怎么样？", "lang": "zh-CN", "final": true }
}
```

### Agent24 → AgentEar

宿主通过 AgentEar 注册的反向命令入口发送版本化命令。第一阶段只需要：`speak`（`text`、`lang`、可选 `voice`/`style`、`command_id`）和 `stop_playback`（`command_id`、可选 `session_id`）。重复的 `command_id` 必须幂等；停止无活动播放也应成功返回。AgentEar 通过 `speech`/`error` 事件回报 `started`、`completed`、`stopped` 或 `failed`。具体 URL/socket、HTTP 状态与认证由 A3 生命周期设计冻结，AgentEar 不监听任意外部网卡。

## 4. 错误、隐私和退出语义

- 模型超时、取消、`busy`、`unavailable`、隐私拒绝等错误按 Agent24 SDK 错误类型传递。AgentEar 可播报简短失败提示，但不得把失败转成成功回答。
- `local_only` 无本地 provider 时必须失败关闭；AgentEar 不做隐式云端 fallback。Agent24/iDoris 日志只记录必要元数据，不记录 transcript、prompt、完整回复、音频或凭证。
- 断连时停止新宿主调用并取消在途请求；已录到的音频按 AgentEar 既有本地保留策略处理。模块 generation 被撤销后，旧连接/旧命令必须拒绝。Agent24 退出不得让 AgentEar 失控占用麦克风或播放子进程。
- AgentEar 原有独立模式保留三种可配置推理路径：本机小模型、独立模式直连 iDoris、附着模式经 Agent24。切换来源须可观测；附着模式不在线时是否自动退回独立模式，需在设置中明确选择。

## 5. 分阶段开发与责任分工

| 阶段 | AgentEar 负责 | Agent24 负责 | 完成判据 |
|---|---|---|---|
| **P0 契约冻结** | 提交 `agentear.event/1`、`speak/stop_playback` 样例和 proposal 复用说明 | 冻结 A3 注册/授权/重连/撤销/命令路由；确认 SDK capability 和 endpoint | 两边各有同一组 schema fixtures；本表未决项有人认领；ADR-032 与本文件同步 |
| **P1 并行骨架** | 抽出模型 transport 与 Agent24 SDK adapter；加 fake host 测试替代真实服务 | 做附着注册 host 与 fake AgentEar 模块；路由事件和反向命令 | 双方各自可用假对端通过契约测试，不要求麦克风/模型 |
| **P2 单轮端到端** | 模块模式推送最终 transcript、接收 speak/stop、保留独立模式 | Agent24 接入模型、事件、展示及取消；默认 `local_only` | 真机说一句→收到转写→Agent24 得到本地模型回复→AgentEar 播放；无远端流量 |
| **P3 提案闭环** | 发布 proposal，不运行动作 | 宿主确认、执行受支持动作、保存回执 | 拒绝时零执行；确认内容与执行载荷同源；超出支持 action type 时安全拒绝 |
| **P4 流式优化** | 支持取消、分句播报和背压，不改 P0 语义 | 设计并实现流式 model callback 和生命周期取消 | 首字起播中位数 ≤3.0 秒，至少 10 次测量；取消后无残留播放/请求 |

P1 可由两边并行开发，但只能用 fake host/module 和暂定 fixtures；P2 真实联调必须等 P0 的 A3 传输、授权与生命周期冻结。**不要把 proposal 执行当作 P2 的阻塞条件**；Agent24 当前执行门的支持 action 集合仍需单独确认，先完成听说闭环。

## 6. 联调与发布验收

1. **契约测试**：双方使用相同 JSON fixtures，验证版本、必填字段、未知版本拒绝、重复事件去重、命令幂等和错误映射。Agent24 测宿主，AgentEar 测模块；合入前记录 fixture commit/hash。
2. **假宿主测试**：不启用麦克风，依次覆盖模块注册、transcript/proposal 事件、模型成功/失败/取消、播放/停止、断连和重连。
3. **macOS 真机验收**：确认麦克风/辅助功能授权仍属于 AgentEar；只出现一个 AgentEar 实例和一个热键所有者；确认系统睡眠、Agent24 重启和用户退出时无遗留录音/TTS 子进程。
4. **隐私负测**：把模型 provider 配成外部计数桩；`local_only` 场景计数必须为 0，缺本地 provider 时得到明确失败。`remote_allowed` 必须是显式策略，并能在宿主展示实际 tier。
5. **发布门**：Rust/SDK/模块测试通过；签名发行包中包含锁定版本的 AgentEar 模块及模型资产声明；首次安装、权限授予、升级、回滚和卸载有可复现步骤。未达流式门槛时，以 P2 非流式功能单独发布并标明延迟。

## 7. 冻结前必须关闭的事项

1. Agent24 A3 的附着协议与宿主命令入口（这是 P2 阻塞项）。
2. AgentEar 断开后采取“自动独立运行”还是“停听并提示”；默认建议停听，避免策略边界静默变化。
3. 对话 session 与 Agent24 run/memory 的关联及生命周期；第一版可单轮，但不得宣称多轮记忆已接通。
4. `proposal` 中三种 action 哪些由 Agent24 实际支持；不支持的必须拒绝执行。
5. `agentear.event/1` 与 command schema 的 JSON Schema 所属仓库和版本发布方式。建议 AgentEar 维护生产事件/proposal schema，Agent24 维护 callback/SDK schema；双方 CI 用同一份 fixtures 做兼容测试。

关闭后，将确认的状态、传输和兼容版本回填 AgentEar ADR-0008 与 Agent24 ADR-032。若实现偏离本约定，先更新 schema 和双方 ADR，再合并实现。

### §7 各项状态（2026-09-27 回填；只追加状态，不改上方约定正文）

1. **A3 附着协议与宿主命令入口** → ✅ 已冻结：Agent24 #524（`docs/design/A3-ATTACHED-MODULE.md` v2 @68c2412）；
   已实现：#526（A3-2a 存储/REST/CLI）、#527、#529（A3-2b 监听/生命周期）、#532（A3-3 反向命令，走同一连接 `_a24/command/invoke`）。
   AgentEar 侧：v0.25.0（#97）。端到端对 #532@`b111ac4` 29/29。
2. **断开后自动独立 vs 停听** → ✅ 已决（B5，jason 2026-09-27）：独立推理是本机边车 → 自动回独立模式；否则停听并提示；设置可改。
3. **session 与 Agent24 run/memory** → ⏳ 第一版**单轮**，未接多轮记忆（`memory` 能力未授予）；不宣称多轮已接通。
4. **proposal 三种 action 的宿主支持** → ⏳ 属 P3。现阶段宿主只展示 proposal，**`builtin` 标「已在 AgentEar 本地执行」、永不执行**；P3 执行门也排除 builtin。
5. **schema 所属与发布** → ✅ AgentEar 维护 `agentear.event/1`、`agentear.proposal/1`、`agentear.command/1`（本仓库 `contracts/`，基准 `522f9eb`）；
   Agent24 以 vendored 副本 + 记录 commit 的方式引用；error.code = wire ErrorKind 18 个 + AgentEar 自有 4 个。
