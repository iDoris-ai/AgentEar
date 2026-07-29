# 本地 ASR 选型报告（初版调研，结论已被推翻）

> ⚠️ **本文档的选型结论已作废。**
> 它是基于二手资料的初版调研，其中「主链路选 Fun-ASR-Nano」的结论**已被四模型实测横比推翻**。
> **当前有效的选型见 [`docs/decisions/0001-asr-model-selection.md`](decisions/0001-asr-model-selection.md)**（结论：SenseVoiceSmall q8）。
> 本文保留作为调研过程的历史记录，其中的架构分析（双工、路径分层）仍然有效。


调研日期：2026-07-29
目标机器：MacBook Pro，Apple M1 Max，10 核（8P+2E），**64 GB 统一内存**
约束（来自 jason）：常驻内存 ≤ 2 GB，模型规模 0.5B–1B 量级，中文为主，实时流式，最好支持双工

---

## 0. 结论先行

**主链路的候选优胜者是 `Fun-ASR-Nano-2512`，走 llama.cpp / GGUF 运行时。**

> ⚠️ **这是「暂定优胜」，不是「已锁定」。** 在第 5.1 节的基准测试跑完之前，本文所有性能与准确率数字都是二手资料，不能当作设计基础。已知本文内部就存在一处数字矛盾（见 2.1 节的体积问题）。

一句话理由：在**已调研到的**候选里，它是唯一同时满足「≤1B 参数 + 中文方言口音第一梯队 + 原生流式 + Apache 2.0 + 单二进制无 Python 运行时」这五条的模型，而最后一条对一个要 24/7 常驻的守护进程来说是决定性的。

> 「唯一」的限定是**已调研范围内**，不是穷尽。而且「第一梯队」目前只有二手数字支撑——§1 的 CER 数据自相矛盾，真实排序未知。

**双工这件事，需要先纠正一个前提**（详见第 4 节）：你想要的「他一边听我说一边理解、可以互相打断」不是 ASR 模型能提供的能力，那是 speech-to-speech 全双工模型（Moshi 那一类）的能力，而它们是 7B 级别、常驻远超 2 GB。在 2 GB 预算内做不到真全双工。

v1 走**打断式半双工**（流式 ASR + VAD 掐断 TTS）。**注意这不等于全双工，也不要说成「体感接近」**——它解决「说完才轮到我」，不解决「一边听一边想」，且需要回声消除才能成立。详见第 4 节。

---

## 1. 候选模型横评

| 模型 | 参数 | 量化后体积 | 中文 CER | 流式 | 运行时 | 许可 |
|---|---|---|---|---|---|---|
| **Fun-ASR-Nano-2512** | 800M | ~484 MB（GGUF）| 见下 | ✅ 原生 | FunASR / **llama.cpp** / vLLM | **Apache 2.0** |
| SenseVoice-Small | ~234M | ~250 MB (q8) | 7.81% | ❌ 分块离线 | ONNX / CPU | 官方标注**即将停止维护** |
| Paraformer | 最轻 | 最小 | 10.18% | ✅ 有流式版 | CPU | — |
| FireRedASR2-AED | 1.1B | 无 GGUF | **2.89%**（最强） | ✅ | PyTorch 为主 | — |
| Parakeet TDT | 0.6B | — | **不支持中文** | ✅ | MLX（parakeet-mlx）| CC-BY |
| Whisper（各尺寸） | — | — | ~20% | 需外挂改造 | whisper.cpp / MLX | MIT |

### 关于 CER 数字的一个诚实说明

上表的中文 CER **来自不同测试集，不能横向直接比**：

- funasr.com（厂商博客）自测口径：Fun-ASR-Nano 8.06%、SenseVoice 7.81%、Paraformer 10.18%、Whisper ~20%
- 公开学术基准口径：Fun-ASR-Nano 在 AISHELL-1 上 1.76% CER、AISHELL-2 上 2.80%；SenseVoice-Small 2.96%；FireRedASR2 在 4 个公开普通话基准上均值 2.89%(LLM) / 3.05%(AED)

厂商博客把自家 Nano 排在 SenseVoice 之后、学术基准却把它排在前面，这种矛盾本身就说明**不要迷信任何单一数字**。

能从这堆数字里安全推出的**只有一条**：**中文专用模型是唯一值得考虑的候选，Whisper 出局。** 而它们**彼此之间的排序是未知的**——同一厂商口径下 8.06% vs 20% 只有约 2.5 倍差距（不是一个数量级），学术基准口径下 1.76% vs 20% 才是一个数量级，两种口径不能混着用来支持同一个论断。

真正的排序要用你自己的录音（你的口音、你的房间、你的录音笔麦克风）实测才算数。这是 v1 阶段第一件该做的事，见 5.1 节。

---

## 2. 为什么是 Fun-ASR-Nano，而不是分数更高的 FireRedASR2

FireRedASR2-AED 的中文准确率确实更好（2.89% vs 3.x%），在 WenetSpeech Meeting 这种真实嘈杂会议场景下 4.32% CER，明显强于其他模型。但它输在**工程形态**上：

- 没有 GGUF / llama.cpp 路径，主要是 PyTorch 部署 → 常驻就意味着常驻一个 Python + PyTorch 进程
- 1.1B 参数，没有量化路径的话内存吃得更多
- 许可条款不如 Apache 2.0 干净

对一个**跑一次转写一段音频**的批处理任务，FireRedASR2 是更好的选择。对一个**开机就起、跑一整年、随时接收麦克风流**的守护进程，Fun-ASR-Nano 的「单个自包含二进制、无 Python 运行时、内置 FSMN-VAD」是压倒性的优势 —— 这和 whisper.cpp 相对于 openai/whisper 的优势是同一个道理。

准确率差的那 1 个百分点，可以用第 5 节的「双模型分层」补回来。

### 2.1 Fun-ASR-Nano 的具体规格（含一处未解的数字矛盾）

- 800M 参数，架构是 SenseVoice encoder + Qwen3-0.6B decoder
- 31 种语言；中文含 7 大方言片（吴、粤、闽、客、赣、湘、晋）+ 26 种地方口音；另有英、日、韩等
- 预编译二进制覆盖 Linux / **macOS** / Windows
- 内置 FSMN-VAD，不需要自己再拼一个 VAD
- Apache 2.0
- 2025 年 12 月发布，训练数据为数千万小时真实语音

**⚠️ 体积数字对不上，必须实测澄清：**

资料里有两个互相矛盾的说法——「GGUF 量化后约 484 MB」和「q8 约为全量的一半」。但 800M 参数在 q8 下按每参数约 1 字节算应该是 **~800 MB**，不是 484 MB。484 MB 更像是 q4 级别的体积，或者对应的是另一个更小的变体。**在下载下来 `ls -l` 之前，不要采信任何一个数字。**

**内存估算也因此不可靠。** 常驻 RSS 除权重外还要算上：llama.cpp 的 mmap 行为、Metal 缓冲区、KV cache、decoder 状态、分配器开销、音频环形缓冲。原先「1–1.5 GB」的估算是基于 484 MB 权重推的，如果实际是 800 MB 权重，**≤2 GB 的预算会相当紧张**。这是必须实测的头号问题。

---

## 3. Apple Silicon 上的运行时选择

M1 Max 上有三条可选路径：

1. **llama.cpp / GGUF**（推荐）—— 官方提供 macOS 预编译二进制，Metal 加速。单文件、无依赖、最适合常驻。
2. **MLX** —— 生态很活跃：`mlx-audio` 支持 Whisper / Parakeet / Voxtral Realtime / Qwen3-ASR 并带流式和词级时间戳；`parakeet-mlx` 专门做 Apple Silicon 低延迟流式。**但 Parakeet 不支持中文**，这条路目前对你没用。可以留意 mlx-audio 里的 Qwen3-ASR。
3. **CoreML** —— `speech-swift` 这类工具链（Parakeet TDT 达到 32× 实时，WhisperKit 也是这条路）。性能最好但工程量最大，且同样卡在中文模型的可得性上。

**结论：v1 走 llama.cpp/GGUF。** MLX 作为 v2 备选观察，等中文流式模型在 mlx-audio 里成熟。

---

## 4. 双工：必须纠正的一个前提

你说的「我说的时候他在理解，他也可以打断我，我也可以打断他」——这是**全双工语音对话模型**的能力，代表作是 Kyutai 的 **Moshi**。它的做法是并行建模用户语音输入流和系统文本+语音输出流，理论延迟 160ms、实测约 200ms。同类还有 NVIDIA PersonaPlex（FullDuplexBench 上 100% 打断成功率、平均 205ms 延迟）、Qwen-Omni 系列（Thinker-Talker 结构）、Mini-Omni2。

**但它们和你的 2 GB 预算不兼容：**

- Moshi 是 **7B** 时序 transformer + 一个较小的 depth transformer + Mimi 流式音频编解码器
- PyTorch 版需要 ≥24 GB 显存
- `moshi_mlx` 确实支持 Apple Silicon，有 int4/int8/bf16 量化，`-q 4` 可以做 4-bit —— 这在你 64 GB 的 M1 Max 上**跑得动**，但常驻内存是 4–8 GB 量级，是你预算的 2–4 倍
- **中文支持不明确**。Moshi 官方是英语（Kyutai STT 是英/法），没有找到中文全双工的证据

所以：

| | v1（现在做） | v2（以后再说） |
|---|---|---|
| 架构 | 流式 ASR + VAD + 本地 LLM + TTS，分段管线 | 端到端 speech-to-speech |
| 双工 | **打断式半双工**：VAD 检测到你开口 → 掐掉 TTS 输出 | 真全双工 |
| 常驻内存 | ASR 待实测 + LLM 按需 | 4–8 GB |
| 中文 | ✅ 方言口音都覆盖 | ❓ 无成熟中文方案 |

**不要把 v1 说成「体感接近全双工」——它不是。** 真全双工是「TTS 在放的同时模型仍在理解你说的话」；v1 是「检测到你开口就把 TTS 掐掉」。后者能解决「说完才轮到我」的憋屈感，但解决不了「一边听一边想」。这个差距在快速来回的对话里会被感知到。

**而且半双工有一个被忽略的硬需求：回声消除（AEC）。** TTS 从扬声器放出来会被麦克风收回去，ASR 会把系统自己的声音当成用户输入，触发误打断甚至自我对话循环。要么强制用耳机，要么必须引入 AEC（macOS 的 Voice Processing I/O 单元可以做，但要走 CoreAudio）。**这一项在选型报告里原先完全没考虑，是 v1 的真实工程量。**

**v1 的真正优势不在体感，在架构**：每一段都可换（ASR 换掉不影响 LLM），中文方案成熟，符合「隐私 + 成本 + 能自己攒」的初衷。端到端 7B 黑盒违背这个初衷。

**v1 半双工的验收门槛**（不达标就不能说做完了）：

| 指标 | 阈值 | 怎么测 |
|---|---|---|
| AEC 自触发率 | 外放 TTS 连续播放 10 分钟，**ASR 误识别次数 = 0** | 播一段合成语音，看有没有转写产出 |
| 打断响应时延 | 用户开口 → TTS 静音 **< 300 ms** | 打点计时，取 p95 |
| VAD 误打断率 | 咳嗽/键盘/环境噪声下 **< 1 次/10 分钟** | 放噪声样本，统计误掐断 |
| 打断后恢复 | 被掐掉的内容有明确处理策略（补说 / 丢弃 / 摘要），**行为一致可预测** | 人工验收 |

> 阈值是初始建议值，实测后可调，但**必须有数字**——「AEC 有效」这种描述不是验收标准。

---

## 5. 建议的技术栈

```
麦克风 / 录音笔
      ↓  16kHz PCM
┌──────────────────────────────────────────────┐
│  raw/audio/  ← 原始字节先落盘 + fsync         │
│  这一步早于 ASR，且不依赖任何下游步骤成功      │
└──────────────────────────────────────────────┘
      ↓（raw 已持久化之后才开始）
┌─────────────────────────────────────────┐
│ 常驻守护进程                              │
│  FSMN-VAD（Fun-ASR-Nano 内置）            │
│      ↓  切出语音段                        │
│  Fun-ASR-Nano-2512  GGUF                 │
│  via llama.cpp，Metal 加速                │
└─────────────────────────────────────────┘
      ↓
  derived/transcripts/   ← 模型输出，可重算、可覆盖
      ↓
  标签识别 → routes/ → idea / task / knowledge base / 调研 / 日程
```

### 存储语义（现在就定死，不要留给实现时决定）

| 目录 | 内容 | 可否重建 |
|---|---|---|
| `raw/audio/` | **ASR 之前的原始音频字节** | ❌ 丢了就没了 |
| `derived/transcripts/` | 转写结果 | ✅ 可从 raw 重算 |
| `routes/` | 分类与下游决策 | ✅ 可重算 |

**`raw` 的定义是「ASR 之前」，不是「处理流程末尾」。** 如果原始音频的持久化排在 ASR 之后，那么模型崩溃、VAD 切错段、或者转写进程 OOM，都会连带毁掉唯一一份忠实记录。README 的「先存后分流」原则，落到实现上就是这条线。

> **两条路径的持久化保证等级不同，不要混为一谈：**
> - **路径 A（文件导入）**：零丢失。走完整提交协议后才 ACK。
> - **路径 B（实时流）**：**有界丢失**。为了不牺牲实时性，音频以 tee 方式同时喂 ASR 和落盘，崩溃会丢掉最后一次 fsync 之后的部分。raw 对象按定长时间片切分，转写在 segment 提交前是 provisional。
>
> 完整定义见 `docs/ingest-design.md` §3.7。**下游路由只消费 committed 的转写。**

**分层兜底（可选，v1.5）**：实时链路用 Nano 保证低延迟；对重要录音（会议、长录音笔文件）再用 FireRedASR2-AED 离线跑一遍高精度转写，写入 `derived/` 的另一个版本（不覆盖，保留两版）。

### 5.1 基准测试门槛（**这是开始写产品代码的前置条件**）

在 M1 Max 上，用**真实的中文/口音录音**，跑通以下测量后才谈选型锁定：

| 指标 | 怎么测 | 门槛 |
|---|---|---|
| 中文 CER | jason 本人 + 目标场景的语料 | 与竞品横比，不看厂商数字 |
| 模型实际体积 | `ls -l` 下载下来的 GGUF | 澄清 484MB / 800MB 之争 |
| 常驻 RSS | 峰值 + 稳态，跑满 30–60 分钟 | **≤2 GB** |
| RTF | 实时率 | < 1.0 且有余量 |
| 长跑稳定性 | 连续流 30–60 分钟 | 无内存泄漏、无降频崩溃 |
| 热行为 | 持续负载下的降频 | 常驻场景必测 |

**横比对象**：Fun-ASR-Nano GGUF、Paraformer 流式、SenseVoice/ONNX、mlx-audio 里的 Qwen3-ASR、FireRedASR2（作为离线兜底）。

> 第 3 节对 MLX / CoreML 路线的排除，目前**只基于「Parakeet 不支持中文」这一条实测事实**；mlx-audio 里的 Qwen3-ASR 是被顺带略过的，没有实测理由。它应当进入上面的横比清单。

### 5.2 其他待验证项

1. 那支爱国者录音笔到手后的能力刻画 —— 见 `docs/ingest-design.md` 第 1.0 节的刻画清单
2. AEC 方案（CoreAudio Voice Processing I/O）是否够用，还是必须强制耳机

---

## 6. 与工作区其他项目的复用

`/Users/jason/Dev/tools/ququ/` 已经在用 FunASR（Electron + 内嵌 Python）。AgentEar 选 Fun-ASR-Nano 意味着**和 ququ 同源**，模型文件、VAD 配置、中文后处理经验都可以复用。

但**运行时不要复用** —— ququ 走的是内嵌 Python，AgentEar 应该走 llama.cpp 单二进制。常驻守护进程背一个 Python 环境是不划算的。

`whisper.cpp/` 的仓库结构和构建方式可以当作 llama.cpp 集成的参考样板。

---

## 来源

- [Best Open Speech Recognition (ASR) Models in 2026 — MarkTechPost](https://www.marktechpost.com/2026/07/23/best-open-speech-recognition-asr-models-in-2026-wer-languages-latency-and-license-compared/)
- [Best open source speech-to-text model in 2026 — Northflank](https://northflank.com/blog/best-open-source-speech-to-text-stt-model-in-2026-benchmarks)
- [FunAudioLLM/Fun-ASR-Nano-2512 — Hugging Face](https://huggingface.co/FunAudioLLM/Fun-ASR-Nano-2512)
- [Which FunASR Model? Nano vs MLT-Nano vs SenseVoice vs Paraformer — FunASR Blog](https://www.funasr.com/en/blog/which-funasr-model.html)
- [QwenAudio/Fun-ASR — GitHub](https://github.com/QwenAudio/Fun-ASR)
- [modelscope/FunASR — GitHub](https://github.com/modelscope/FunASR)
- [FireRedTeam/FireRedASR2S — GitHub](https://github.com/FireRedTeam/FireRedASR2S)
- [FireRedTeam/FireRedASR2-AED — Hugging Face](https://huggingface.co/FireRedTeam/FireRedASR2-AED)
- [FireRedASR2S 论文 — arXiv](https://arxiv.org/html/2603.10420v1)
- [kyutai-labs/moshi — GitHub](https://github.com/kyutai-labs/moshi)
- [moshi-mlx — PyPI](https://pypi.org/project/moshi-mlx/)
- [MLX Audio](https://blaizzy.github.io/mlx-audio/)
- [EliFuzz/parakeet-mlx — GitHub](https://github.com/EliFuzz/parakeet-mlx)
- [soniqo/speech-swift — GitHub](https://github.com/soniqo/speech-swift)
- [ASR in 2025-2026: A Deep Dive into Speech Recognition Technology Selection](https://ruoqijin.com/blog/asr-deep-dive-2025-2026)
