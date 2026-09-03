# ADR-0006：OpenKnowledge —— 记录为未来参考，**现阶段不采纳**

- **状态**：已接受（2026-09-03）
- **决策**：`inkeep/open-knowledge` **不进入 M2/M3 的实现范围**。它是 `kb/` 目录的一个
  可选消费端，用户想用自己装即可；AgentEar **不 fork、不 vendor、不打包分发**它。
- **关系**：细化 [ADR-0003](0003-knowledge-base-adapters.md) §3.3「个人档 = Markdown 文件树」的消费端选择

---

## 1. 它是什么（2026-09-03 实测）

| | 实测值 |
|---|---|
| 仓库 | [inkeep/open-knowledge](https://github.com/inkeep/open-knowledge) |
| star / 年龄 | 3,961 star；**2026-06-03 建库**，最近提交 2026-09-02 |
| 许可 | **GPL-3.0-or-later** |
| 栈 | TypeScript / Node 24+ / pnpm / Turbo 单体仓库；桌面 app + 本地 web UI |
| 存储 | **它自己没有存储**。`ok start` 打开一个本地 markdown/mdx 文件夹，同步靠 git/GitHub |
| 对外接口 | CLI（`ok init` / `ok start`）+ 自带 MCP server 与 skills，供 agent 检索/写入 |

**最关键的一条事实：它是编辑器，不是可投递的知识库服务。** 没有 ingest API、没有数据库、
没有多租户。它读的就是磁盘上的一堆 `.md`。

所以套回 ADR-0003 §3.2 那张图，它**不在「组织档」那一格**，它在个人档的**消费端**——
和 Obsidian / Logseq / foam / silverbullet 并列，是第五个「能直接读我们 `kb/` 目录的东西」。

## 2. 为什么现在不做

它带来的增量能力是**文档编辑的封装**加**给 agent 提供检索/写入**。这两样：

- 编辑：`kb/` 是纯 Markdown，Obsidian 等已经能读。AgentEar 的定位是**产生**这些文档，
  不是提供编辑体验。
- agent 检索：确实有价值（能省掉 ADR-0003 §4.3 里「AgentEar 自己暴露一个 MCP server」
  那个模块），**但那个模块本来就还没做**。省一个尚未开工的模块不构成现在动手的理由。

**结论：对当前仓库定位没有增量。记录在案，等真的需要「让 Claude 检索历史语音笔记」时再评估。**

## 3. 许可：担心的方向对，但这里的解法是「根本不接触」

先厘清两点常被混淆的事实：

1. 它是 **GPL-3.0**，不是 AGPL。GPL 的触发点是**分发**，不是「提供网络服务」。
2. AgentEar 本身是 **Apache-2.0**。兼容性是**单向**的：Apache 代码可以并进 GPLv3 项目，
   **反过来不行**。

于是：

| 做法 | 后果 |
|---|---|
| ❌ fork 它 / vendor 它 / 打进 `scripts/bundle.sh` 产出的 .app 一起发 | AgentEar 整体必须转 GPL-3.0，ADR-0003 §2 说的「卖可安装服务」这条路被封死 |
| ✅ 文档里写一句「可以用 OpenKnowledge 打开 `kb/`」，用户自己 `npm i -g` | 两个独立程序共享一个文件夹。不是衍生作品，也不是聚合分发。**零法律接触面** |

**规则一句话：可以推荐，不要 fork，不要进 bundle。**

注意这条规则不是针对 OpenKnowledge 的特例，而是**所有 GPL/AGPL 系知识库消费端的通则**
（SiYuan、Logseq、Trilium 同理）——只要我们只写文件、不链接代码，它们的许可就与我们无关。
这正是 ADR-0003 选「文件树而不是某个 App」的价值。

## 4. 风险与它为什么不构成风险

3 个月新库、商业公司（inkeep）的开源产品，存在改协议 / 加云端 / 停更的可能。

但**我们只依赖「它能读 markdown 目录」这一个事实**，它跑了我们损失为零——因为
AgentEar 侧一行代码都没有为它写。这是「契约在文件树上」的直接红利。

## 5. 顺带确立的分层：分界线是「能不能从语音重算」

评估过程中澄清了一个更重要的问题——知识库该怎么分层。**分界线不是「存在哪」，
而是「能不能从语音重算」**：

| 层 | 内容 | 可重建？ | 归属 |
|---|---|---|---|
| **L0 事实层** | `raw/audio/` + `derived/transcripts/` + `routes/` | ❌ 音频不可重建 | 永远在 `~/.agentear`，第三方碰不到 |
| **L1 文档层** | 一条语音 = 一个 `.md`，front matter 带 label/tags/source | ✅ 可从 L0 全量重放 | **唯一的人类可读权威面**，第三方的接入点 |
| **L2 索引层** | 标签索引、全文检索、实体图、wiki link | ✅ **必须**能从 L1 全量重建 | 实现可随便换、随便删 |
| **L3 行动层** | 任务、日程、report | ❌ **带用户后来改的状态** | 下游系统（Seeder 等） |

**L3 和前三层性质不同，这是最容易出事的地方**：任务有「已完成/已取消」，是用户在下游
改的，不是从语音算出来的。如果把它也塞进 Markdown 文件树、哪天再从 L0 重放，
**会把用户的状态覆盖掉**。所以 L3 必须走**单向「创建」语义**（投一次，之后不再同步），
且投递目标不是知识库。

T2.4.3 做的是 **L0 → L1**。L2 排在 L1 之后（rusqlite + SQLite FTS5，不上向量库）。
L3 等到真有下游任务系统时再开。

## 来源

- GitHub API 元数据与 README，2026-09-03 实测
- [inkeep/open-knowledge](https://github.com/inkeep/open-knowledge) · <https://openknowledge.ai/docs>
