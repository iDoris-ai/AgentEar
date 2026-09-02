# 数据模型与状态机

精确到能照着建文件、写解析。字段改动视为破坏性变更，要在 PR 里显式说明。

---

## 1. 术语表

### 位置与格式

`~/.agentear/terms.json`（数据目录内，可编辑，跨升级保留）。
随包提供一份默认表，首次启动时若文件不存在则写入默认表；**已存在则不覆盖**
（用户改过的不能被升级抹掉）。

```json
{
  "version": 1,
  "terms": [
    { "canonical": "raw",            "aliases": ["road", "row", "ro", "roll", "肉"] },
    { "canonical": "knowledge base", "aliases": ["闹铃是base", "notice base", "脑力士base"] },
    { "canonical": "MacBook",        "aliases": ["macbook", "我的妈book", "妈的book"] }
  ]
}
```

- `canonical`：正确写法，**大小写敏感**，输出时按此还原（`wifi` → `WiFi`）。
- `aliases`：ASR 已知会输出的错误形式。**可以为空**——空数组表示「这个词是对的，
  不要改它」，这类条目单独有用：它防止模型把正确的项目术语改成别的。
- 解析失败或文件损坏 → **退回默认表并记日志**，不让守护进程起不来
  （与 `config.rs` 的容错策略一致）。

### 注入方式

术语表拼进纠错提示词，作为「这些词是本项目的固定写法」的清单。
**不做逐字符替换**——那会误伤（用户真的在说 road 的时候）。让模型结合上下文决定，
术语表只是给它候选集合。

## 2. 标签

### 一级封闭集合（8 类，不可扩展）

| 标签 | 定义 | 与谁容易混，怎么分 |
|---|---|---|
| `idea` | 一个还没决定要不要做的想法 | 与 task 分：**没有承诺要做**就是 idea |
| `task` | 一件确定要做的事，有交付物 | 与 command 分：**要我记下来以后做**是 task |
| `command` | 要系统**现在**执行的指令 | 与 task 分：**要系统立刻响应**是 command |
| `note` | 一条知识、事实、结论 | 与 journal 分：**脱离时间仍然成立**是 note |
| `journal` | 当天发生了什么、当时的感受 | 与 note 分：**带时间与主观状态**是 journal |
| `question` | 一个待解答的疑问 | 与 note 分：句子的目的是**求答案** |
| `reference` | 指向外部资源的指针 | 与 note 分：**主体是链接或出处**是 reference |
| `unknown` | 无法归类，或内容无意义 | 兜底。**宁可 unknown 不要瞎猜** |

上面两列「怎么分」是本轮新增的判别依据，用来修 M0 基准里那两条判错
（「今天开会讨论了传输协议」→ 带时间与场景，是 **journal**；
「帮我查一下明天的日程」→ 要系统立刻响应，是 **command**）。

⚠️ **判别依据本身需要 jason 确认**。上表是按语义边界推的一个自洽方案，
但「开会讨论了什么」到底该进 note 还是 journal，属于产品决策。
实现时先按上表做，把这条列为待确认项，不擅自当成定论。

### 二级标签

自由词表，模型抽取，不做封闭。存进记录但**不用于路由**（路由只看一级）。

## 3. routes 记录

`routes/<yyyy-mm>/<content_hash>.json`，一次转写一条，只增不删。

```json
{
  "content_hash": "内容寻址的 sha256，与 raw/audio 和 transcripts 对齐",
  "created_at": "2026-09-02T15:04:05+07:00",
  "label": "idea",
  "label_source": "explicit",
  "confidence": null,
  "secondary": ["录音笔", "硬件"],
  "text": "纠正后的转写文本",
  "delivery": { "state": "pending", "attempts": 0, "last_error": null }
}
```

- `label_source`：`explicit`（用户在语音里明说）| `model`（模型推断）。
  **explicit 必须优先**，见架构边界 B5。
- `confidence`：模型推断时填，explicit 时为 null。
- `delivery.state`：`pending` → `delivered` | `failed`。
  投递是下一步的事，本轮只保证这个字段存在且初始为 pending。

### 显式标记的识别

用户可能说的形式（中英文）：「这是一个 idea」「这是个想法」「记一个任务」
「this is a task」。识别规则**必须能被单元测试覆盖**，不是模型判断——
显式的意思就是不靠猜。识别不到就落到模型推断。

## 4. 状态机：一次录音的完整生命周期

```
录音 → raw 提交(committed) → 转写 → [术语纠错] → [标签识别] → routes 落盘
                  │              │         │            │
                  │              │         └ 失败: 用未纠正文本继续
                  │              │                      └ 失败: label=unknown, 仍落盘
                  │              └ 失败: 停在这里, raw 完好, 报错
                  └ 失败: 整个流程中止, 不产生任何下游记录
```

**每一层的失败都不回滚上一层。** 已经落盘的 raw、已经写好的 transcript，
不因下游失败而删除或标记为无效。
