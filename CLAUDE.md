# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## 当前状态：M0 已通过，**M1 开发中**（进度见 `docs/m1-status.md`）

**构建与测试**：

```bash
cargo build --release
cargo test                                    # 8 个测试：提交协议、崩溃语义、token 过滤
./target/release/agentear                     # 守护进程，Ctrl+Shift+R 开始/停止录音
./target/release/agentear --transcribe x.wav  # 离线转写，不占麦克风，用于验证 ASR 链路
```

数据落在 `~/.agentear/`（`AGENTEAR_DATA` 可覆盖）；ASR 二进制与模型在 `vendor/`（`AGENTEAR_VENDOR` 可覆盖，**不入库**）。

文档阅读顺序：`docs/milestones.md`（里程碑）→ `docs/decisions/`（决策记录，**选型结论以此为准**）→ `docs/benchmarks.md`（实测数据）→ `docs/ingest-design.md`（接入层设计）。`docs/asr-selection.md` 是初版调研，其中的选型结论**已被 ADR-0001 推翻**，仅作历史参考。

仓库在 GitHub 上（`git@github.com:jhfnetboy/AgentEar.git`，分支 `main`）。

### 已拍板（jason 2026-07-29 确认，不要重开讨论）

1. **M1 只做到转写** —— 快捷键 toggle 录音 → raw 落盘 → 转写 → 文字进剪贴板。不含 LLM、路由、TTS。
2. **常驻守护进程用 Rust** —— 单二进制、无运行时，契合 ASR 的选型理由。
3. **先做丢弃式 Python spike 跑 M0 基准** —— 「不用 Python」的约束针对常驻进程，不针对一次性测量脚本。spike 用完即删。

### 技术栈

- **ASR 已定为 `SenseVoiceSmall q8` + FSMN-VAD**，走 `llama-funasr-sensevoice` 单二进制。**决策依据见 `docs/decisions/0001-asr-model-selection.md`，不要重开选型讨论。**
- **Fun-ASR-Nano 已被推翻**：四模型实测横比后，它是唯一在 30 分钟音频就爆 2 GiB 预算的（16 分钟即破），且冷启动 11.45s。SenseVoice 常驻仅 419 MB（Nano 的 27%）、冷启动 0.2s、术语命中反超。
- **Qwen3-ASR-0.6B 指标最好但出局**：需 Python + MLX 运行时。若将来出现 GGUF/纯 Rust 路径，应重新评估。
- **Paraformer 出局**：完全不输出标点。
- **Whisper 不做主链路**：中文 CER 远差于中文专用模型。但不要重复「差一个数量级」这个说法——那是混用了两种测试集口径。
- **复用 `ququ/` 仅限配置与思路层面**（模型选择、中文后处理经验），**不继承其运行时**。常驻进程背一个内嵌 Python 环境不划算。`huniu/`、`whisper.cpp/` 同理，是参考不是迁移源。
- 目标机器 **M1 Max / 64 GB**，常驻内存预算 **≤2 GB**（jason 定的）。64 GB 的余量不是拿来放宽这个预算的。

### M0 实测产出的硬约束

**SenseVoiceSmall q8 实测关键数字**：权重 242 MiB + VAD 1.6 MiB、**冷启动 0.2s**、RSS 419 MB(2min) / 1.27 GiB(30min)、RTF 0.030–0.033。

1. **冷启动 0.2 秒 → M1 不需要常驻模型服务。** 每次录音直接 `Command::new()` 调 `llama-funasr-sensevoice` 子进程即可，无需链接 C++ 库、无需 server 模式。（这是换掉 Fun-ASR-Nano 的直接红利——Nano 冷启动要 11.45s，必须常驻。）
2. **长音频仍需分段送入 ASR，单段建议 ≤5 分钟。** RSS 随音频长度增长 0.52 MB/s，54 分钟破 2 GiB。影响 `ingest-design.md` 的**路径 A**；M1 的快捷键录音不受影响。
3. **特殊 token 需过滤。** `llama-funasr-sensevoice` 有 `--keep-tags` 开关，默认行为**需在 M1 中实测确认**，别让 `/sil` 之类粘进剪贴板。
4. **中英混杂技术术语不可靠。** 实测 `raw` 一词四个模型全错（row/road/ro/roll）。**术语纠错是 M2 的职责**，不要指望换 ASR 解决。

### M1 的两个红利，不要提前破坏

选择「M1 只到转写」让 M1 恰好绕开了两个最难的部分：

- **不需要 AEC**（没有 TTS 播放，麦克风不会收到自己的声音）
- **不需要流式 raw 语义**（快捷键的按下/再按天然给出段边界，每次录音就是一个有头有尾的文件对象，可直接用 `ingest-design.md` §3.3 的零丢失提交协议）

**不要在 M1 里提前引入 TTS 或无边界流**，那会把这两块难度一起拽进来。它们属于 M3。

### 存储语义（已定，不要在实现时重新解释）

`raw/audio/` = **ASR 之前**的原始字节，丢了不可重建；`derived/transcripts/` = 模型输出，可重算；`routes/` = 下游决策，可重算。**原始音频的持久化不得依赖任何下游步骤成功**——这是 README「先存后分流」的执行点。

**两条接入路径的持久化保证等级不同，措辞上不要混用**：文件导入（路径 A）是**零丢失**，走完整提交协议后才 ACK；实时流（路径 B）是**有界丢失**——音频以 tee 同时喂 ASR 和落盘，崩溃会丢掉最后一次 fsync 之后的部分，raw 对象按定长时间片（非 VAD 边界）切分。**下游路由只消费 committed 的转写，不消费 provisional 的。** 详见 `docs/ingest-design.md` §3.7。

### 「双工」的正确理解

jason 要的「边说边理解、可互相打断」是全双工 speech-to-speech（Moshi 一类）的能力，7B 级别、常驻 4–8 GB、无成熟中文方案 —— **在 2 GB 预算内做不到**。v1 是**打断式半双工**：VAD 检测到开口就掐掉 TTS。

**不要把它描述成「体感接近全双工」**——它解决「说完才轮到我」，不解决「一边听一边想」，快速来回时能被感知到。

**v1 半双工有一个硬需求：回声消除（AEC）。** TTS 从扬声器出来会被麦克风收回去，ASR 会自触发。要么强制耳机，要么走 CoreAudio 的 Voice Processing I/O。这是真实工程量，不是细节。

### 未验证前提

1. SenseVoice 的准确率仅基于**单个样本**，样本量不足；且现有语料均为安静环境
2. 那支爱国者录音笔的能力 —— **机器没到手，`docs/ingest-design.md` §0 的「实时互斥」是条件式结论**。到手当天先做 §1.0 的刻画清单，它可能推翻整个坞站方案。
3. AEC 方案是否够用

## 产品意图（读 README 才能拼出的全貌）

AgentEar 要做的是一条**端到端、全本地**的语音 → 文字 → AI 工作流管线，用来替代市面上的 AI 录音卡 / 录音助手。两个不可让步的设计约束，决定了后续所有技术选型：

1. **隐私**：录音和转写**不经过任何第三方服务器**。这排除了云端 ASR（Whisper API、各家语音服务）作为主链路 —— 转写必须跑在 jason 自己的机器上（本地模型）。
2. **成本**：硬件用便宜的二手设备（一支闲鱼淘的爱国者 8G 录音笔，几十块），而不是买成品 AI 录音卡。

### 设想中的数据链路

```
录音设备（爱国者录音笔 / 手机 / MacBook 麦克风）
   ↓  传输层：WiFi 上的 HTTP/WebSocket（蓝牙仅用于配对与控制信令，不传数据）
本地机器（MacBook 优先）
   ↓
raw/audio/  ← 原始音频字节先落盘并 fsync，**早于 ASR**，不可重建
   ↓  本地模型做 ASR（失败可重试，不影响 raw）
derived/transcripts/  ← 文字，可从 raw 重算
   ↓  按语音里带的"标签"路由到不同分支
routes/ → 存入 knowledge base / 触发调研并出 report / 查日程并给回复 / 记 idea / 建任务
```

几个实现上要留意的点，都是 README 里已经定调的：

- **raw 优先**：原始**音频**必须在 ASR 之前就落盘，转写失败、VAD 切错段、进程 OOM 都不能毁掉唯一一份忠实记录。注意 README 的口述稿里 raw 是排在转写之后的，**以本文件为准**。
- **分支路由由语音内容里的标签驱动** —— 用户说"这是一个 idea"/"这是一个任务"，系统据此选下游分支。这意味着 ASR 之后需要一层意图/标签识别。
- **录音笔的可改造性未知**：那支爱国者录音笔还没到手，是否能加 WiFi、能否自动传输数据，都还没验证。任何依赖"录音笔能主动推送数据"的设计都是未经证实的假设；先做的是**手机 / MacBook 这条链路**（README 明确说了先完成这个）。
- **Mac mini 中转是后期选项**，不是第一版目标。

## 与工作区其他项目的关系

`ququ/`（FunASR 语音输入，Electron + 内嵌 Python）已经解决了"本地 ASR"这一段，`huniu/`（本地语音助手）和 `whisper.cpp/` 在同一片领域。

**这三个是参考资料，不是迁移源。** 可以复用的是**配置与思路层面**的东西：模型选择的经验、中文后处理、VAD 参数、踩过的坑。**不要继承它们的运行时** —— 尤其不要为了复用 ququ 而把内嵌 Python 环境搬进来，常驻守护进程背一个 Python 栈不划算（见上方技术栈一节）。任何"直接复用某段实现"的想法都要先过 `docs/benchmarks.md` 的实测。
