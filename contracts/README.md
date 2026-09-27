# contracts/ — AgentEar ↔ Agent24 的机器可读契约

依据：[`docs/agent24-embedding.md`](../docs/agent24-embedding.md)（§3 接口、§6.1 契约测试、§7.5 schema 归属）、
[ADR-0008](../docs/decisions/0008-voice-frontend-boundary.md)。Agent24 侧对应 `docs/design/INTEGRATION-AGENTEAR-IDORIS.md`（ADR-032）。

**为什么放在仓库根目录的 `contracts/`，不放 `docs/`**：这些是**被程序读取**的文件，
包括我们的 `cargo test`、Agent24 的 CI、将来的 fake host。它们和代码一起版本化、一起受测试守护。
`docs/` 放给人读的文字；放在那里，改文档时很容易顺手改坏 fixture，而且没人察觉。

## 内容

```
contracts/
├── schema/
│   ├── agentear.proposal.v1.schema.json   # 已冻结（ADR-0008 §3），= --match-command --json 的输出
│   ├── agentear.event.v1.schema.json      # AgentEar → Agent24（经 SDK _a24/events/emit）
│   └── agentear.command.v1.schema.json    # Agent24 → AgentEar（speak / stop_playback）
└── fixtures/
    ├── proposal/{valid,invalid}/*.json
    ├── event/{valid,invalid}/*.json
    ├── command/{valid,invalid}/*.json
    └── sequences/*.json                   # 宿主的去重/排序行为用例（由 Agent24 测）
```

Schema 用 JSON Schema draft 2020-12。`event` 的 `proposal` payload 用相对 `$ref` 引用 proposal schema，
所以三份文件要按各自的 `$id` 一起注册。

## 谁维护、怎么引用

| 契约 | 维护方 | 说明 |
|---|---|---|
| `agentear.proposal/1`、`agentear.event/1`、`agentear.command/1` + 本目录 fixtures | **AgentEar** | 生产方契约 |
| callback / SDK（`_a24/*` 方法、A3 注册与握手） | **Agent24** | 在 Agent24 仓库 |

- **Agent24 的 CI 按 commit hash 引用本目录**（例如 `git archive <sha> contracts/` 或 submodule 钉到某个 sha），
  并在合入记录里写明 fixture 用的是哪个 hash（embedding.md §6.1）。不要引用 `main` 的浮动头。
- **AgentEar 这边的守护**：`tests/contracts.rs` 检查 schema 能编译、合法 fixture 全部通过、非法 fixture 全部被拒；
  `src/main.rs` 的 `contract_tests` 用 `--match-command --json` 的**同一个函数** `proposal_json()`
  生成真实 proposal 去对照 schema，防止实现和契约漂移。

## 版本规则

- `schema` 字段里的 `/1` 是**不兼容版本号**。删字段、改语义、收紧取值范围，都要升到 `/2`，
  并与 Agent24 同步发布。收到未知版本一律拒绝（`fixtures/*/invalid/unknown_version.json`）。
- `/1` 之内**只允许增加可选字段**。schema 用 `additionalProperties: false`，这是**生产方**的约束：
  AgentEar 发出的内容必须严格符合它。**消费方**应当忽略自己不认识的字段，这样增字段不会把对方弄坏。
- 事件去重与顺序：同一事件重试时**复用** `event_id` 和 `seq`；`seq` 在同一个 `session_id` 内从 1 开始单调递增。
  宿主按 `(session_id, seq)` 去重并保持顺序。同一个 `(session_id, seq)` 出现两个不同的 `event_id` 属于协议违规
  （见 `fixtures/sequences/`）。
- 命令幂等：同一个 `command_id` 重复送达只执行一次；没有播放在进行时，`stop_playback` 也返回成功。
- **音频字节和原始音频路径永远不会出现在事件里**（`fixtures/event/invalid/transcript_leaks_audio_path.json`）。

## 与 agent24-13 的约定（2026-09-27）

以下是 AgentEar 提出的方案，agent24-13 已于 2026-09-27 **全部同意**，已写进 schema：

| # | 约定 | 在 schema 的位置 |
|---|---|---|
| B4 | AgentEar **按 wire 规格和共享 fixtures 自己实现**，不依赖 Agent24 的 `agent24-os-sdk` crate。A3 的 wire 规格由 Agent24 单独写成文档 | —（P1/P2 实现约束） |
| B6 | 只有**对话模式**的轮次才发 `transcript`，输入法模式（只上屏）不发；但输入法模式下命中指令表产生的 `proposal` 照样发 | `event` → `transcript` 的说明 |
| B7 | 本轮回答正在播放时，`speak` 排在它之后；`stop_playback` 和用户按键打断都会清空队列 | `command` → `speak` 的说明 |
| B8 | 新事件 **`confirm_reply`**：附着模式下 AgentEar 关掉本地的语音确认，把用户的答复交给宿主。payload = `proposal_event_id` + `reply`（confirm/reject）+ 可选原话 `text` | `event` → `confirm_reply` |
| B9 | `transcript.content_hash`（raw 音频的 sha256，可选）；`lang` 用 BCP-47，识别不出时填 `und` | `event` → `$defs.lang`、`transcript` |
| D | schema 归属按本文件「谁维护」一节；fixtures 就放在 `contracts/` | — |

**⚠️ 还没定的（待 jason 确认）**
- **B5 断连后的默认行为**：提案是「独立模式的推理走本机边车时，自动回到独立模式；否则停止监听并提示」。
  Agent24 会在它的 PR 里把这一条标出来请 jason 过目。这一条不影响任何 schema。
- `turn.phase`、`speech.state/reason`、`error.code` 的具体取值是 AgentEar 先定的，agent24-13 没有逐项评论。
  有异议就在 `/1` 冻结前提出来。
- **A3 附着协议本身**（注册、token、握手、generation、重连、反向命令入口）由 Agent24 冻结，这是 P2 的阻塞项。
  这件事要 jason 排期，**目前还没开始**。
