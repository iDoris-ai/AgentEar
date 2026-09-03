# ADR-0003：知识库对接方案——双适配器 + 稳定投递契约

- **状态**：已接受（2026-07-30）
- **决策**：AgentEar 只定义一套投递契约，背后挂两个可自由切换的适配器：
  **个人档 = Markdown 文件树 + Git**；**组织档 = Memos 自托管服务**
- **前置**：[ADR-0002](0002-m2-understanding-layer.md) §5 已定「知识库是独立模块，经 MCP 对接」

---

## 1. GitHub 排行调研（2026-07-30 实测 star 数）

按 star 排序的主流知识库/笔记类仓库：

| star | 仓库 | 语言 | 许可 | 形态 | 多用户 |
|---|---|---|---|---|---|
| 70k | toeverything/AFFiNE | TypeScript | **NOASSERTION** | 本地优先 + 协作 | ✅ |
| **61k** | **usememos/memos** | **Go** | **MIT** | **自托管服务，单二进制** | ✅ |
| 45k | siyuan-note/siyuan | TypeScript | **AGPL-3.0** | 本地优先 + 可自托管 | ✅ |
| 44k | logseq/logseq | Clojure | **AGPL-3.0** | 本地 Markdown 文件 | ❌ |
| 39k | outline/outline | TypeScript | **NOASSERTION**(BSL) | 团队 wiki 服务 | ✅ |
| 38k | chatchat-space/Langchain-Chatchat | Python | Apache-2.0 | RAG 问答 | — |
| 37k | TriliumNext/Trilium | TypeScript | **AGPL-3.0** | 自托管服务 | 部分 |
| 17k | foambubble/foam | TypeScript | NOASSERTION | VSCode + 纯 Markdown | ❌ |
| 16k | suitenumerique/docs | Python | **MIT** | 协作文档（法国政府） | ✅ |
| 12k | codexu/note-gen | TypeScript | GPL-3.0 | 桌面笔记 | ❌ |
| 10k | chaitin/PandaWiki | TypeScript | **AGPL-3.0** | AI wiki | ✅ |
| 8k | reorproject/reor | JavaScript | **AGPL-3.0** | AI 笔记（本地 LLM） | ❌ |
| 7k | dendronhq/dendron | TypeScript | Apache-2.0 | Markdown（**2025-11 起停更**） | ❌ |
| 5k | silverbulletmd/silverbullet | TypeScript | **MIT** | 自托管 Markdown | 部分 |

### 1.1 MCP 生态现状

| MCP server | star | 语言 | 说明 |
|---|---|---|---|
| MarkusPfundstein/mcp-obsidian | 4.2k | Python | 最成熟，但依赖 Obsidian + local-rest-api 插件 |
| coddingtonbear/obsidian-local-rest-api | 2.7k | TypeScript | 上面那个的前置依赖 |
| entanglr/zettelkasten-mcp | 161 | Python | 纯 Markdown 卡片盒 |
| SiYuan 的若干社区实现 | 各 <100 | TS/Python | 数量多但都很小，且多为 AGPL |
| memos 的若干社区实现 | 各 ~20 | Python | 存在但不成熟 |

**结论：没有一个 MCP server 成熟到可以直接依赖。** 无论选哪个 KB，我们都得自己写一层薄的
MCP（或者干脆不用别人的 MCP，直接调 KB 的原生 API——见 §4.3）。

---

## 2. 关键发现：许可才是真约束，不是功能

jason 的规划里有一句关键的话：**「未来会抽象成产品和流程，为社区、公司提供可安装的服务」**。

这句话把**许可**从法务细节变成了架构约束：

- **AGPL-3.0**（siyuan / logseq / Trilium / PandaWiki / reor）：如果把它作为
  网络可访问服务的一部分分发给公司，**必须开放整个服务的源码**。
  这直接和「做成商业产品卖可安装服务」冲突。
- **NOASSERTION**（AFFiNE / Outline / foam）：非标准许可，需逐个读条款。
  Outline 用的是 BSL（Business Source License），**明确限制商业托管**。
- **MIT / Apache-2.0**（memos / suitenumerique-docs / silverbullet / dendron）：干净，可商用。

> 这不是我替 jason 做的法务判断，而是指出：**选型必须先过许可这一关，否则做到一半
> 才发现不能商用，前面的集成工作全废。** 最终取舍是 jason 的决定。

**排除 AGPL 与 BSL 之后，第一梯队只剩 memos（61k / Go / MIT）。**

---

## 3. 决策：不选「一个 KB」，而是定一套契约 + 两个适配器

### 3.1 为什么不可能有单一方案

「个人用」和「公司用」的需求差异是结构性的，不是配置差异：

| | 个人 | 社区 / 公司 |
|---|---|---|
| 用户 | 1 | N，要认证与权限 |
| 存储 | 本机文件 | 服务端数据库 + 备份 |
| 可用性 | 开着就行 | 要 uptime |
| 隐私 | 全私有 | 分级可见性 |
| 分发 | 无 | jason 提到的**订阅服务** |

**指望一个工具同时做好这两端是不现实的。** 但**可以让 AgentEar 对两端无感**。

### 3.2 架构：投递契约在中间

ADR-0002 已经把 `routes/` 定为**本地权威记录**、MCP 定为投递边界。这个设计天然支持双适配器：

```
       AgentEar（Rust 守护进程）
  录音 → 转写 → 纠错 → 分类
                        ↓
                   routes/         ← 本地权威记录，任何适配器失败都不影响它
                        ↓
              ┌──── 投递契约 ────┐   ← 稳定接口，见 §4
              ↓                  ↓
      ┌───────────────┐  ┌──────────────────┐
      │ 个人档         │  │ 组织档            │
      │ Markdown+Git  │  │ Memos 自托管      │
      │ 零基础设施     │  │ Go 单二进制 + PG  │
      └───────────────┘  └──────────────────┘
              ↑                  ↑
      Obsidian/Logseq/    多用户 / 权限 /
      foam 都能直接读       REST+gRPC API
```

**切换 = 改一行配置。** 两个适配器写入同样的内容，只是落地形态不同。

### 3.3 个人档：Markdown 文件树 + Git

**不选任何具体 App，选它们的公约数。**

- Obsidian、Logseq、foam、silverbullet、dendron **全都读纯 Markdown 目录**
  —— 选文件就等于同时兼容所有这些，且不被任何一个锁定
- 零基础设施：没有服务、没有数据库、没有端口
- Git 提供版本历史与同步，顺带就是备份
- 和我们既有的存储语义天然衔接：`raw/audio` → `derived/transcripts` → `kb/`

文件布局：

```
kb/
  2026/07/30/
    143022-idea-录音笔加-esp32-自动上传.md
  index/
    tags.md          ← 二级标签的自动索引
```

每篇带 YAML front matter，承载 ADR-0002 §3 的标签体系：

```yaml
---
id: <content_hash 前 12 位>
created: 2026-07-30T14:30:22+08:00
label: idea                      # 一级标签，封闭集合
tags: [agentear, 录音笔, esp32]   # 二级标签，自由词表
source: raw/audio/<content_hash>.wav
transcript_raw: derived/transcripts/<hash>.txt   # 纠错前的原文
explicit_label: false            # 是否来自用户显式标记
---
```

**`source` 与 `transcript_raw` 两个字段是关键**：任何时候都能回溯到原始音频和
未纠错的转写。这是 ADR-0002 §4.3「纠错是有损操作，必须保留原文」的落地。

### 3.4 组织档：Memos

选它的理由，按重要性排序：

1. **MIT 许可** —— 商业化路径干净，这是排除 AGPL 后的第一梯队里唯一的选择
2. **Go 单二进制** —— 和我们「单二进制、无运行时」的取向完全一致
   （ASR 用 llama.cpp 单二进制，KB 用 Go 单二进制，只有 LLM 是 Python 边车）
3. **同一套软件覆盖两端** —— 个人用 SQLite，多用户换 PostgreSQL，**软件不换**
4. **REST + gRPC API** —— 好包，不依赖别人的半成品 MCP
5. **61k star，2026-07-29 仍在更新** —— 维护活跃
6. **设计中心正好是「快速捕获」** —— 语音笔记本质就是短内容速记，与 memos 的
   定位天然契合

**诚实的短板**：memos 是**短内容导向**的（时间线式，类似推文）。
如果以后要写长文档、结构化 wiki、`report` 类产出，memos 会吃力。
届时的备选是 `suitenumerique/docs`（16k / Python / **MIT** / 协作文档）——
它是这次调研里唯一既 MIT 又面向长文档协作的候选。

---

## 4. 投递契约

### 4.1 一个 trait，两个实现

```rust
/// 知识库投递接口。routes/ 是权威记录，本接口只负责把内容送出去。
trait KbSink {
    /// 投递一条。幂等：同一个 id 重复投递不应产生重复条目。
    fn deliver(&self, entry: &KbEntry) -> Result<DeliveryId>;
    /// 健康检查，用于决定是否进重试队列。
    fn health(&self) -> Result<()>;
}
```

`KbEntry` 就是 §3.3 那份 front matter 的结构化形式——**两个适配器共享同一个数据模型**，
这是「自由切换」的前提。

### 4.2 失败不阻塞（沿用 ADR-0002 §5.2）

- 先写 `routes/`，再投递
- 投递失败进重试队列（`routes/.pending/`），用户照样拿到剪贴板文字
- 组织档尤其需要这个：远程 server 可能不在线、笔记本可能休眠

### 4.3 关于 MCP：分清「谁调谁」

ADR-0002 说「经 MCP 对接」，实测调研后需要精确化——**这里有两个方向，不要混淆**：

| 方向 | 用途 | 结论 |
|---|---|---|
| AgentEar **作为 MCP client** 调别人的 KB server | 投递 | ❌ **不用**。现有 KB 的 MCP server 全都不成熟（最大的 mcp-obsidian 还要装两个插件）。直接调 memos 的 REST 更可靠 |
| AgentEar **提供 MCP server** 让 Claude 等读它的数据 | 检索 | ✅ **要做**。这才是 MCP 的价值所在——让 AI 能查询你的语音笔记 |

**修订 ADR-0002 §5 的措辞**：投递走各适配器的原生接口（文件写入 / memos REST）；
MCP 用在**反方向**——AgentEar 暴露一个 MCP server，供 Claude Code 之类检索历史笔记。

这个纠正很重要：原来的设计把 MCP 用错了方向，会为了协议纯洁性去依赖一堆不成熟的
第三方 MCP server。

---

## 5. 迁移路径

个人档 → 组织档不是重写，是一次批量导入：

```
kb/**/*.md  ──(读 front matter)──→  memos REST API
```

因为两个适配器共享 `KbEntry` 模型，导入工具就是「文件适配器读 + memos 适配器写」，
几十行。**而且 `raw/audio/` 与 `routes/` 始终是权威记录，最坏情况可以从头重放。**

反向（组织档 → 个人档）同理。**这就是「自由切换」的实际含义。**

---

## 6. 决定与未决

**已定：**
- 个人档：Markdown + Git，兼容 Obsidian / Logseq / foam / silverbullet
- 组织档：Memos（MIT / Go 单二进制 / SQLite→PostgreSQL）
- 契约：`KbSink` trait + 共享 `KbEntry` 模型
- MCP 用于**检索**（AgentEar 作为 server），不用于投递

**已裁决（2026-09-03，原「留给 jason 的决定」两条）：**

- **AGPL/GPL 是红线 —— 是。** 依据不是法务偏好，而是两个已成立的事实：
  AgentEar 自身是 **Apache-2.0**，而 Apache→GPLv3 的兼容性是**单向**的
  （Apache 代码可以并进 GPLv3 项目，反过来不行）；加上「抽象成产品、
  为公司提供可安装服务」意味着**分发**，而分发正是 GPL 的触发点。
  两条叠加 ⇒ **任何 copyleft 组件都不得进入我们分发的产物**。
  所以 SiYuan 不重开评估。
  **但红线的位置要说准**：禁的是 fork / vendor / 打进 bundle，
  **不禁「用户自己装、和我们共享一个 `kb/` 目录」**——那不是衍生作品，
  也不是聚合分发。见 [ADR-0006](0006-openknowledge-as-personal-frontend.md) §3。
- **长文档需求 —— 现在不构成选型压力，组织档整体推迟。**
  `report` 类长产出属于 §7 分层里的 **L3 行动层**，而 L3 要等真有下游系统才开工。
  在那之前把组织档定成 memos 还是 `suitenumerique/docs` 是**空转**——
  两者共享同一个 `KbEntry` 模型，届时切换就是 §5 那个几十行的导入工具。
  **组织档的实现推迟到有真实企业需求时再定**，本 ADR 的 §3.4 保留为候选而非承诺。

**待验证：**
- memos REST API 的幂等语义（重复投递同一 id 的行为）
- memos 多用户下的可见性模型能否表达 ADR-0002 §3.1 里 `journal` 的「私有区」要求

## 7. 分层：分界线是「能不能从语音重算」

§3.3 只说了「个人档写 Markdown 文件树」，没说这棵树在整体里的位置。补上：

| 层 | 内容 | 可重建？ | 归属 |
|---|---|---|---|
| **L0 事实层** | `raw/audio/` + `derived/transcripts/` + `routes/` | ❌ 音频不可重建 | 永远在 `~/.agentear` |
| **L1 文档层** | `kb/**/*.md`，front matter 见 §3.3 | ✅ 可从 L0 全量重放 | 本 ADR 的两个适配器都作用在这一层 |
| **L2 索引层** | 标签索引、全文检索、实体图 | ✅ **必须**能从 L1 全量重建 | 实现可换可删 |
| **L3 行动层** | 任务、日程、report | ❌ **带用户后来改的状态** | 下游系统，不是知识库 |

**L3 的性质和前三层不同**：任务有「已完成/已取消」，是用户在下游改的，
不是从语音算出来的。塞进 Markdown 文件树、再从 L0 重放，**会覆盖掉用户的状态**。
所以 L3 走**单向「创建」语义**，投一次，之后不再同步。

`KbSink` 只服务 L1。ADR-0002 §3.1 里 `task` 的下游动作「建任务」属于 L3，
在 L3 落地之前 `task` 仍然投进 `kb/`（否则它在下游彻底消失），
但**这是过渡安排，不是终局**。

### 7.1 对 §3.3 文件布局的两处修订（T2.4.3 实现时确定）

1. **`journal` 走 `kb/private/` 子树。** ADR-0002 §3.1 要求 `journal` 进「私有区」。
   在文件适配器上，「私有」的可执行含义就是**可以被单独排除在 git / 分享之外**，
   所以它必须是一棵独立子树，而不是靠 front matter 标记。
2. **`unknown` 与 `command` 不投递。** 前者是 ADR-0002 §3.1 的明文规定
   （只落 `routes/`）；后者是「触发动作」而非「存入知识库」，在动作层落地之前
   投进 `kb/` 只会污染知识库。两者的记录都完整保留在 `routes/` 里，
   将来可用 `--replay-kb` 重放。

3. **`id` 用完整的 `content_hash`，不是 §3.3 写的「前 12 位」。**
   这个字段是**身份**，去重和删除都按它判断，而 12 位十六进制只有 48 bit——
   拿它当删除判据等于「撞前缀就删对方」。文件名末尾另带 16 位哈希，
   那一段才是显示/防撞名用的。
4. **文件名带哈希后缀**：`103022-idea-给录音笔加-wifi-<hash16>.md`。
   没有它，同一秒里标签相同、正文前 32 字符也相同的两条不同记录会 rename
   到同一个路径，**后一条静默覆盖前一条**。

`kb/index/tags.md`（§3.3 提到的二级标签索引）**推迟到 L2**：二级标签抽取尚未实现
（`Route::secondary` 恒为空），现在生成的索引会是一个空文件。

## 来源

- GitHub Search API，star 数与许可为 2026-07-30 实测
- [usememos/memos](https://github.com/usememos/memos) · [Memos Features](https://usememos.com/features)
- [siyuan-note/siyuan](https://github.com/siyuan-note/siyuan) · [PurpleLiu/siyuan-mcp](https://github.com/PurpleLiu/siyuan-mcp)
- [MarkusPfundstein/mcp-obsidian](https://github.com/MarkusPfundstein/mcp-obsidian)
- [suitenumerique/docs](https://github.com/suitenumerique/docs)
