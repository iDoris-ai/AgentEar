# ADR-0002：M2 理解层——LLM、标签体系、知识库边界

- **状态**：已接受（2026-07-30）
- **决策**：Ornith-1.0-9B（MLX 6bit）常驻，经 mlx-dspark 提供 HTTP 服务；知识库作为独立模块经 MCP 对接
- **推翻**：`CLAUDE.md` 中「常驻内存 ≤2 GiB」与「常驻进程不背 Python 运行时」两条硬约束

---

## 1. 背景

M1 交付了「按键 → 录音 → 转写 → 剪贴板」。M2 要在转写之后加一层理解：

1. **术语纠错** —— M0 实测发现 `raw` 一词四个 ASR 模型全错（row / road / ro / roll），
   `knowledge base` 也被听成 `闹铃是base`。jason 的使用场景恰恰全是技术术语，
   ADR-0001 已把这项定为 M2 的职责，不指望换 ASR 解决。
2. **标签路由** —— README 里「说这是一个 idea / 这是一个任务，自动选分支」的那一段。
3. **下游投递** —— 把结果送进知识库。

---

## 2. 决策：LLM 选 Ornith-1.0-9B，MLX 6bit，常驻

参考 jason 的实测博文
[《Ornith-1.0：自改进 Coding Agent 模型，9B 打 35B，Mac mini 本地跑 60 t/s》](https://blog.mushroom.cv/blog/ornith-1-coding-agent-model-mac-mini-local-inference/)。

- 9B Dense（底座 Gemma 4），256K 上下文
- 博文实测：M4 Pro + 8bit，代码生成约 61 tok/s
- 强项是「有结构地组织输出」——这正是标签分类与术语纠错需要的能力

### 2.1 量化档：6bit

jason 最初选的是 **Q5_K_M**，但那是 GGUF 专有的量化档，而 mlx-dspark 只吃 MLX 格式，
两者不能组合。MLX 侧可选档位与体积：

| MLX 档 | 权重 | + KV cache 后峰值 |
|---|---|---|
| 4bit | 5.57 GiB | ~6.6 GiB |
| **6bit** ← 选定 | **7.65 GiB** | ~8.7 GiB |
| 8bit（博文实测档） | 9.74 GiB | ~11 GiB |

选 6bit 是对「要比 4bit 好一点的质量档」这个意图的忠实翻译。峰值 ~8.7 GiB 略超
8 GiB 目标，在 64 GB 机器上无实际影响。**做成可配置**，换 4bit 只改一行。

### 2.2 内存预算从 ≤2 GiB 改为 ≤9 GiB

这是本 ADR 最重要的一处变更，**它推翻了从 M0 一路贯穿至今的约束**：

```
常驻层
  Ornith-1.0-9B  MLX 6bit      7.65 GiB
  + KV cache（64K 上下文）      ~1 GiB
按需层
  SenseVoice 子进程（转写时）    ~0.4 GiB
──────────────────────────────────────
峰值                          ~9 GiB
```

原来的 2 GiB 约束是为「只做 ASR 的守护进程」定的。M2 引入常驻 LLM 之后它不再成立。
**但 ASR 侧的选型不因此重做**——SenseVoice 的优势（冷启动 0.2s、单二进制、
子进程即用即弃）在新预算下依然成立，而且省下的内存正好给 LLM。

### 2.3 运行时：mlx-dspark（Python），破「无 Python」约束

`CLAUDE.md` 原有硬约束「常驻守护进程不背 Python 运行时」，这也是 ADR-0001 里
判 Qwen3-ASR 出局的理由。**M2 明确推翻它**，理由：

- mlx-dspark 是 Apple Silicon 原生 MLX，带 speculative decoding，是 jason 实测过的路径
- 它以**独立服务进程**形式存在，Rust 守护进程通过 HTTP 与之通信 —— Python 并没有
  被塞进 Rust 进程内部，而是一个可以独立重启、独立崩溃的边车（sidecar）

**约束的精确措辞应改为**：Rust 守护进程自身不内嵌 Python 运行时；外部推理服务的
实现语言不受限制，但必须以独立进程 + 明确协议边界的形式接入。

> 遗留影响：ADR-0001 判 Qwen3-ASR 出局的理由之一（需 Python + MLX）在新措辞下
> 不再成立。但那不构成重开 ASR 选型的理由——SenseVoice 在**其余所有维度**
> （常驻内存、冷启动、术语命中）都不输，且已在 M1 中验证可用。

---

## 3. 标签体系

jason 明确交给我定义，并指出「这是可维护的、动态慢慢积累形成的」。因此设计原则是
**词表可扩展，但分类协议稳定**。

### 3.1 一级标签（意图）

| 标签 | 含义 | 下游动作 |
|---|---|---|
| `idea` | 想法、灵感、「我觉得可以……」 | 存入知识库，标记待孵化 |
| `task` | 待办、「我要去做……」 | 建任务 |
| `note` | 陈述性记录、见闻、会议纪要 | 存入知识库 |
| `question` | 疑问、「为什么……」「怎么……」 | 存入知识库，标记待解答 |
| `reference` | 提到的资料、链接、书名、人名 | 存入知识库，抽取实体 |
| `journal` | 情绪、状态、日记式记录 | 存入知识库，私有区 |
| `command` | 对助理的直接指令（「帮我查……」） | 触发对应动作 |
| `unknown` | 无法归类 | 只落 `routes/`，不投递下游 |

**`unknown` 是必须有的**：分类失败要有明确去处，而不是硬塞进某个标签。

### 3.2 二级标签（主题，自由词表）

一级标签是**封闭集合**，二级是**开放集合**——项目名、技术栈、人名、领域。
由 LLM 抽取，不预先枚举，随使用积累。这就是 jason 说的「动态慢慢积累形成」。

二级标签统一小写、连字符：`agentear`、`asr-selection`、`录音笔`。

### 3.3 显式标记优先于推断

用户说「这是一个 idea」时，那是**显式标记**，必须直接采信，不能被模型的推断覆盖。
只有没有显式标记时才走推断。

---

## 4. 术语纠错

### 4.1 为什么不能靠 ASR

M0 实测（`docs/benchmarks.md` §3.6）：

| 正确词 | 参考 | Nano | SenseVoice | Paraformer | Qwen3-ASR |
|---|---|---|---|---|---|
| **raw** | row ✗ | road ✗ | ro ✗ | roll ✗ | road ✗ |

**四个模型全错。** 这是中英混杂技术术语的固有难点，换模型无解。

### 4.2 做法

给 LLM 一份**项目术语表** + 转写文本，让它结合上下文纠正。术语表来自：

1. 手工维护的基础表（`raw`、`knowledge base`、`MacBook`、`Mac mini`、`24小时`……）
2. 从用户的仓库名、常用词逐步积累

### 4.3 必须保留原文

纠错结果写入 `derived/transcripts/`，**但原始转写要一并保留**。纠错是有损操作，
模型可能把对的改错。原始转写可从 `raw/audio/` 重算，但保留一份省一次推理。

---

## 5. 知识库边界：独立模块，经 MCP 对接

jason 说明：知识库是一个**独立的 24 小时运行的采集 agent**，采集全量数据并为他人
提供订阅服务；存在本机与远程 server；当下是个人的，未来会抽象成产品与流程，
为社区和公司提供可安装的服务。

**因此 AgentEar 不实现知识库**，只负责投递。边界如下：

```
AgentEar（本 ADR 范围）
  录音 → 转写 → 纠错 → 分类 → routes/
                                 │
                                 │  MCP
                                 ▼
                        知识库 agent（独立模块）
                          本机 + 远程 server
                          采集 / 订阅 / 分发
```

> ⚠️ **本节的 MCP 用法已由 [ADR-0003](0003-knowledge-base-adapters.md) §4.3 修正。**
> 调研发现现有 KB 的 MCP server 全都不成熟，投递改走各适配器的原生接口
> （文件写入 / memos REST）；**MCP 用在反方向**——AgentEar 提供 MCP server
> 供 Claude 等检索历史笔记。原来的设计把 MCP 用错了方向。

### 5.1 为什么用 MCP 而不是直接写文件或 HTTP

- 知识库未来要装到**别人的机器**上，接口必须是协议而非路径约定
- MCP 已经是 jason 工作区里的既有模式（`mempalace`、`seeder` 都是 MCP server）
- 工具化的接口天然带 schema，比自定义 HTTP 少一层文档负担

### 5.2 投递失败不能阻塞主链路

MCP 目标可能不在线（远程 server、笔记本休眠）。所以：

- `routes/` 是**本地权威记录**，先落盘再投递
- 投递失败进重试队列，不影响用户拿到转写文字
- 这与 `ingest-design.md` 的「先存后分流」是同一条原则

---

## 6. 对既有文档的影响

| 文档 | 影响 |
|---|---|
| `CLAUDE.md` | 内存预算 2 GiB → 9 GiB；「无 Python」约束改为「Rust 进程不内嵌 Python，外部服务不受限」 |
| `ADR-0001` | 判 Qwen3-ASR 出局的「需 Python」理由在新措辞下失效，但不重开 ASR 选型（见 §2.3） |
| `milestones.md` | M2 三项空白（LLM / 标签词表 / knowledge base 指向）由本 ADR 填上 |

## 7. 实测修订（2026-07-30，见 `docs/benchmarks-m2.md`）

| 项 | 本 ADR 原文 | 实测 |
|---|---|---|
| 术语纠错 | 待验证 | **11/11 = 100%** —— M2 的核心价值成立 |
| 标签分类 | 待验证 | 6/8 = 75%，两处失败均为**词表边界不清**，非模型问题 |
| 常驻 + 尖峰 | ~9 GiB | **7.54 GiB**（LLM 7.17 + ASR 子进程 0.41） |
| KV cache | ~1 GiB（按 64K 估） | 远低于此 —— Gemma 4 是混合线性注意力，**64 层只有 16 层持 KV** |
| 速度 | 博文 61 tok/s（M4 Pro 8bit + 投机解码） | 36 tok/s（M1 Max 6bit + lookup 模式） |

### 运行时参数（实测踩坑后确定）

```bash
mlx-dspark serve --model <path> --mode lookup --no-thinking --context-window 32768
```

- **必须 `--mode lookup`**：默认模式要求注册过的 drafter，本地模型路径没有
- **必须 `--no-thinking`**：Ornith 默认吐 `Thinking Process:`，会污染结构化输出
- **不能用 `--kv-bits`**：与混合线性注意力模型不兼容

### §3.1 标签定义需要修订

实测暴露两组边界重叠，**要改的是词表不是模型**：

- **note vs journal** → 判据改为「是否含主观状态」：陈述客观事实 = note；
  带情绪、体力、心境 = journal
- **command vs task** → 判据改为「谁来做」：让助理立刻执行 = command；
  记下来自己以后做 = task
- prompt 里补 few-shot，把这两组边界用例显式写进去

这正好印证 jason 说的「标签是可维护、动态积累形成的」——边界靠用例喂出来。

## 8. 待验证

- Ornith-9B 6bit 在 M1 Max 上的实测：常驻 RSS、tok/s、首 token 延迟
- 术语纠错的实际命中率（用 M0 那段 ground truth 语料测）
- 标签分类准确率
- MLX 与 SenseVoice 子进程同时跑时的内存尖峰
