# 本地 ASR 选型报告

调研日期：2026-07-29
目标机器：MacBook Pro，Apple M1 Max，10 核（8P+2E），**64 GB 统一内存**
约束（来自 jason）：常驻内存 ≤ 2 GB，模型规模 0.5B–1B 量级，中文为主，实时流式，最好支持双工

---

## 0. 结论先行

**主链路选 `Fun-ASR-Nano-2512`，走 llama.cpp / GGUF 运行时。**

一句话理由：它是当前唯一同时满足「≤1B 参数 + 中文方言口音 SOTA 级 + 原生流式 + Apache 2.0 + 单二进制无 Python 运行时」这五条的模型，而最后一条对一个要 24/7 常驻的守护进程来说是决定性的。

**双工这件事，需要先纠正一个前提**（详见第 4 节）：你想要的「他一边听我说一边理解、可以互相打断」不是 ASR 模型能提供的能力，那是 speech-to-speech 全双工模型（Moshi 那一类）的能力，而它们是 7B 级别、常驻远超 2 GB。在 2 GB 预算内做不到真全双工。但用「流式 ASR + VAD 打断」可以做出体感上非常接近的半双工交互，v1 应该走这条路。

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

厂商博客把自家 Nano 排在 SenseVoice 之后、学术基准却把它排在前面，这种矛盾本身就说明**不要迷信任何单一数字**。唯一可信的结论是：**这几个中文专用模型全都把 Whisper 甩开一个数量级**（~3% vs ~20%），Whisper 在中文场景直接出局。

真正的排序要用你自己的录音（你的口音、你的房间、你的录音笔麦克风）实测才算数。这是 v1 阶段第一件该做的事。

---

## 2. 为什么是 Fun-ASR-Nano，而不是分数更高的 FireRedASR2

FireRedASR2-AED 的中文准确率确实更好（2.89% vs 3.x%），在 WenetSpeech Meeting 这种真实嘈杂会议场景下 4.32% CER，明显强于其他模型。但它输在**工程形态**上：

- 没有 GGUF / llama.cpp 路径，主要是 PyTorch 部署 → 常驻就意味着常驻一个 Python + PyTorch 进程
- 1.1B 参数，没有量化路径的话内存吃得更多
- 许可条款不如 Apache 2.0 干净

对一个**跑一次转写一段音频**的批处理任务，FireRedASR2 是更好的选择。对一个**开机就起、跑一整年、随时接收麦克风流**的守护进程，Fun-ASR-Nano 的「单个自包含二进制、无 Python 运行时、内置 FSMN-VAD」是压倒性的优势 —— 这和 whisper.cpp 相对于 openai/whisper 的优势是同一个道理。

准确率差的那 1 个百分点，可以用第 5 节的「双模型分层」补回来。

### Fun-ASR-Nano 的具体规格

- 800M 参数，架构是 SenseVoice encoder + Qwen3-0.6B decoder
- 31 种语言；中文含 7 大方言片（吴、粤、闽、客、赣、湘、晋）+ 26 种地方口音；另有英、日、韩等
- GGUF 量化后约 484 MB，q8 约为其一半，精度基本无损
- 预编译二进制覆盖 Linux / **macOS** / Windows
- 内置 FSMN-VAD，不需要自己再拼一个 VAD
- Apache 2.0
- 2025 年 12 月发布，训练数据为数千万小时真实语音

**内存估算**：484 MB 权重 + KV cache + 音频缓冲，常驻大约 1–1.5 GB。**符合你 ≤2 GB 的要求**，且在 64 GB 的机器上完全无压力。

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
| 双工 | **半双工**：VAD 检测到你开口 → 立刻掐掉 TTS 输出 | 真全双工 |
| 常驻内存 | ~1.5 GB（ASR）+ LLM 按需 | 4–8 GB |
| 中文 | ✅ 方言口音都覆盖 | ❓ 无成熟中文方案 |

**关键洞察**：v1 的「流式 ASR 持续吐部分结果 + VAD 随时打断 TTS」在体感上和全双工差别没有想象中大，而且它**每一段都可换**（ASR 换掉不影响 LLM），更符合你「隐私 + 成本 + 能自己攒」的初衷。端到端 7B 黑盒反而违背了这个初衷。

---

## 5. 建议的技术栈

```
麦克风 / 录音笔
      ↓  16kHz PCM 流
┌─────────────────────────────────────────┐
│ 常驻守护进程（≤2 GB）                     │
│                                          │
│  FSMN-VAD（Fun-ASR-Nano 内置）            │
│      ↓  切出语音段                        │
│  Fun-ASR-Nano-2512  GGUF/q8              │
│  via llama.cpp，Metal 加速                │
│      ↓  流式吐部分结果 + 最终结果          │
└─────────────────────────────────────────┘
      ↓
  raw 落盘（无条件，先存后分流）
      ↓
  标签识别 → 路由到 idea / task / knowledge base / 调研 / 日程
```

**分层兜底（可选，v1.5）**：实时链路用 Nano 保证低延迟；对重要录音（会议、长录音笔文件）再用 FireRedASR2-AED 离线跑一遍高精度转写，覆盖 raw 里的初版文本。低延迟和高精度就都拿到了，代价只是多一次离线批处理。

**待验证项**（不要在验证前写代码）：

1. 用你自己的录音实测 Fun-ASR-Nano 的中文 CER —— 厂商数字不可信
2. 实测 llama.cpp 在 M1 Max 上的常驻内存和 RTF
3. 那支爱国者录音笔到手后，确认它到底能不能自动传输 —— 目前所有「录音笔主动推送」的设计都是未验证假设

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
