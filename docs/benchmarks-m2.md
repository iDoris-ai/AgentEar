# M2 理解层基准结果

日期：2026-07-30
机器：MacBook Pro，Apple M1 Max（8P+2E），64 GB
被测：`Ornith-1.0-9B` MLX **6bit**，经 `mlx-dspark serve --mode lookup --no-thinking`
决策依据：[ADR-0002](decisions/0002-m2-understanding-layer.md)

> **结论：三项全部达标，M2 可以开工。**

---

## 汇总

| 指标 | 结果 | 门槛 |
|---|---|---|
| **术语纠错** | **11/11 = 100%** | 这是 M2 存在的理由，必须高 |
| **标签分类** | 6/8 = 75% | 见 §3 的分析——两处失败都是词表定义问题 |
| **LLM 常驻 RSS** | **7.17 GiB** | — |
| **LLM + ASR 尖峰** | **7.54 GiB** | ≤ 9 GiB ✅ |
| 生成速度 | 36 tok/s | — |

---

## 1. 术语纠错：11/11

**靶子全部取自 M0 实测中四个 ASR 模型都答错的真实案例**，不是编的。

| ASR 的错误输出 | 正确词 | Ornith 纠正结果 |
|---|---|---|
| `road目录` | raw | ✅ 然后把内容存到 raw 目录里面。 |
| `ro的目录` | raw | ✅ 然后把内容存到 raw 的目录里面。 |
| `roll的目录` | raw | ✅ 然后把内容存到 raw 的目录里面。 |
| `闹铃是base` | knowledge base | ✅ 然后把内容存到 knowledge base 里面。 |
| `notice base` | knowledge base | ✅ |
| `脑力士base` | knowledge base | ✅ |
| `我的妈的book` | MacBook | ✅ 然后把内容存到我的 MacBook 里面。 |
| `最好是mark mini` | Mac mini | ✅ 然后把内容存到最好是 Mac mini 里面。 |
| `二四二运行` | 24小时 | ✅ 然后把内容存到 24 小时运行里面。 |
| `这是一个ID` | idea | ✅ 然后把内容存到这是一个 idea 里面。 |
| `给你写个日报` | report | ✅ |

**ADR-0001 §2.2 的核心判断得到验证**：`raw` 一词四个 ASR 模型全错（row/road/ro/roll），
**换 ASR 模型无解，但下游 LLM 结合术语表能 100% 纠回来**。M2 的核心价值成立。

---

## 2. 两处测量方法的错误（都是我这边的问题，值得记下）

第一轮跑出「术语纠错 100% + 标签分类 0%」，两个数字**都不可信**：

### 2.1 模型在输出推理过程

Ornith 默认会吐 `Thinking Process:` 再给答案。后果是双向的：

- **标签分类 0%** —— 不是模型不会分类，是输出里混着推理，取不到纯类名
- **术语纠错 100% 是假阳性** —— 我的判据是「目标词出现在输出任意位置」，
  而推理过程里自然会提到目标词

修法：`mlx-dspark serve --no-thinking`，判据改为只取**最后一行非空输出**。

### 2.2 保险杠判据撞上子串前缀

收紧后加了一道保险杠「原错词不应残留在输出中」，结果把 `这是一个ID → idea`
这个**纠对了的**案例判成失败：`norm("这是一个ID")` = `这是一个id`，
是 `norm("这是一个 idea")` = `这是一个idea` 的**前缀**，裸子串判据必然命中。

修法：残留检查加词边界 `(?![a-z])`。修完从 10/11 回到真实的 11/11。

> 教训与 M0 那次一样：**先怀疑测量方法，再怀疑被测对象。**

---

## 3. 标签分类：6/8，两处失败都是词表定义问题

| 输入 | 期望 | 实得 | 判断 |
|---|---|---|---|
| 我觉得可以给录音笔加个 ESP32 自动上传 | idea | idea | ✅ |
| 明天把 M2 的基准测试跑完 | task | task | ✅ |
| **今天开会讨论了接入层的传输协议** | note | **journal** | ❌ |
| 为什么 SenseVoice 的内存比 Nano 低这么多？ | question | question | ✅ |
| Ornith 那篇博客在 blog.mushroom.cv | reference | reference | ✅ |
| 今天调了一天按键事件，有点累但总算通了 | journal | journal | ✅ |
| **帮我查一下明天的日程** | command | **task** | ❌ |
| 嗯这个那个 | unknown | unknown | ✅ |

**两处失败都不该算模型的错，是我定的词表边界不清：**

1. **note vs journal** —— 「今天开会讨论了…」既是陈述性记录（note），
   又是当天的流水（journal）。ADR-0002 §3.1 里两者的定义确实重叠。
2. **command vs task** —— 「帮我查一下明天的日程」既是对助理的直接指令（command），
   也确实是一件待办（task）。

### 要改的是词表，不是模型

- **note / journal 的判据改为「是否含主观状态」**：陈述客观事实 → note；
  带情绪、体力、心境 → journal
- **command / task 的判据改为「谁来做」**：让助理立刻执行 → command；
  记下来自己以后做 → task
- prompt 里补 few-shot 示例，把这两组边界用例显式写进去

这条正好印证 jason 说的「标签是可维护、动态慢慢积累形成的」——
**边界要靠用例喂出来，不是一次性设计出来的。**

---

## 4. 内存：7.54 GiB 尖峰，在预算内

| 场景 | RSS |
|---|---|
| mlx-dspark 空转（模型未加载） | 0.02 GiB |
| **LLM 常驻（加载后）** | **7.17 GiB** |
| ASR 子进程单独峰值 | 0.41 GiB |
| **两者同时跑的合计尖峰** | **7.54 GiB** |

ADR-0002 估算 ~9 GiB，实测 **7.54 GiB**，✅ 在预算内且有 1.5 GiB 余量。

### 为什么比估算低：Gemma 4 的混合线性注意力

启动时带 `--kv-bits 8` 会被拒绝，报错信息给出了原因：

> `--kv-bits is unsupported for hybrid linear-attention targets
> (their recurrent-state caches are not KV caches; only 16 of 64 layers even hold KV)`

Ornith 底座是 Gemma 4，**64 层里只有 16 层持有真正的 KV cache**，其余是循环状态。
所以长上下文的 KV 内存远小于按标准 transformer 的估算——我在 ADR-0002 里
按 64K 上下文估的 ~1 GiB KV 偏高了。

---

## 5. 速度：36 tok/s

jason 博文实测是 M4 Pro + 8bit + dspark 投机解码约 61 tok/s。
本次是 M1 Max + 6bit + **lookup 模式**（无 drafter），36 tok/s 属于合理区间。

### 启动时踩的两个坑

1. **默认模式要求注册过的 drafter**：本地模型路径没有内置 drafter，
   必须用 `--mode lookup`（drafter-free）或 `--mode auto`。
2. **`--kv-bits` 与本模型不兼容**（见 §4）。

对 M2 的实际影响不大：术语纠错和标签分类都是短输出（几十到一百多 token），
36 tok/s 意味着单次推理 1–3 秒。

---

## 6. 对 ADR-0002 的修订

| 项 | ADR 原文 | 实测 |
|---|---|---|
| 常驻 + 尖峰 | ~9 GiB | **7.54 GiB** |
| KV cache | ~1 GiB（按 64K 估） | 远低于此，Gemma 4 只有 16/64 层持 KV |
| 运行时参数 | 未提 | 必须 `--mode lookup` + `--no-thinking`，不能用 `--kv-bits` |
| 标签体系 | 一级 8 类定义 | note/journal、command/task 边界需重新定义 + few-shot |

## 7. 复现

```bash
cd spike
source .venv/bin/activate
mlx-dspark serve --model "$PWD/models/ornith" --mode lookup \
  --port 8791 --context-window 32768 --no-thinking
python3 m2_bench.py --url http://127.0.0.1:8791
```

模型：`mlx-community/Ornith-1.0-9B-6bit`（7.7 GB，不入库）。
