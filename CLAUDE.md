# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## 当前状态：选型已定，代码未写

这个仓库目前只有 `README.md`（中文构想稿）、`LICENSE`（Apache 2.0）和 `docs/asr-selection.md`（ASR 选型报告），**没有任何源码、manifest、lockfile 或构建脚本**。因此：

- 没有 build / test / lint 命令可用 —— 不要凭空编造，也不要假装某个命令存在。
- 仓库已经在 GitHub 上（`git@github.com:jhfnetboy/AgentEar.git`，分支 `main`），远程先于本地建立，符合上级 `tools/CLAUDE.md` 的新 idea 流程。

### 已定的技术栈（决策依据见 `docs/asr-selection.md`，不要在没读它之前重开选型讨论）

- **ASR：`Fun-ASR-Nano-2512`（800M，Apache 2.0），走 llama.cpp / GGUF 运行时**。选它不是因为准确率最高（FireRedASR2-AED 更高），而是因为它是唯一能做成「单个自包含二进制、无 Python 运行时、内置 FSMN-VAD」的中文流式模型 —— 对 24/7 常驻的守护进程，这条压倒准确率。
- **不要用 Whisper 做主链路**：中文 CER ~20%，比中文专用模型差一个数量级。
- **不要为了复用 `ququ/` 而引入内嵌 Python**：模型选型和中文后处理经验可以复用，运行时不要。常驻进程背一个 Python 环境不划算。
- 目标机器是 **M1 Max / 64 GB**，但常驻内存预算是 jason 定的 **≤2 GB**（Fun-ASR-Nano 实测预期 1–1.5 GB）。64 GB 的余量不是拿来放宽这个预算的。

### 三个尚未验证的前提（写代码前先验，别把它们当既成事实）

1. Fun-ASR-Nano 在 **jason 自己的录音**上的真实中文 CER。报告里所有厂商数字都不可信，且不同来源互相矛盾。
2. llama.cpp 在 M1 Max 上的实测常驻内存和 RTF。
3. 那支爱国者录音笔能否自动传输数据 —— 机器还没到手，**所有依赖「录音笔主动推送」的设计都是假设**。

### 「双工」的正确理解

jason 要的「边说边理解、可互相打断」是全双工 speech-to-speech 模型（Moshi 一类）的能力，7B 级别、常驻 4–8 GB、且无成熟中文方案 —— **在 2 GB 预算内做不到**。v1 的做法是「流式 ASR 持续吐部分结果 + VAD 检测到开口就掐掉 TTS」的**半双工**，体感接近而各段可换。不要在 v1 里试图引入端到端语音模型。

## 产品意图（读 README 才能拼出的全貌）

AgentEar 要做的是一条**端到端、全本地**的语音 → 文字 → AI 工作流管线，用来替代市面上的 AI 录音卡 / 录音助手。两个不可让步的设计约束，决定了后续所有技术选型：

1. **隐私**：录音和转写**不经过任何第三方服务器**。这排除了云端 ASR（Whisper API、各家语音服务）作为主链路 —— 转写必须跑在 jason 自己的机器上（本地模型）。
2. **成本**：硬件用便宜的二手设备（一支闲鱼淘的爱国者 8G 录音笔，几十块），而不是买成品 AI 录音卡。

### 设想中的数据链路

```
录音设备（爱国者录音笔 / 手机 / MacBook 麦克风）
   ↓  传输层：蓝牙 / WiFi / 互联网（长期可能挂一台 24h 运行的 Mac mini 中转）
本地机器（MacBook 优先）
   ↓  本地模型做 ASR
文字
   ↓  先无条件落一份 raw
raw 目录（原始留存，永不丢）
   ↓  按语音里带的"标签"路由到不同分支
下游动作：存入 knowledge base / 触发调研并出 report / 查日程并给回复 / 记 idea / 建任务
```

几个实现上要留意的点，都是 README 里已经定调的：

- **raw 优先**：任何分流之前必须先把原始数据存下来，分类失败也不能丢数据。
- **分支路由由语音内容里的标签驱动** —— 用户说"这是一个 idea"/"这是一个任务"，系统据此选下游分支。这意味着 ASR 之后需要一层意图/标签识别。
- **录音笔的可改造性未知**：那支爱国者录音笔还没到手，是否能加 WiFi、能否自动传输数据，都还没验证。任何依赖"录音笔能主动推送数据"的设计都是未经证实的假设；先做的是**手机 / MacBook 这条链路**（README 明确说了先完成这个）。
- **Mac mini 中转是后期选项**，不是第一版目标。

## 与工作区其他项目的关系

`ququ/`（FunASR 语音输入，Electron + 内嵌 Python）已经解决了"本地 ASR"这一段。做 AgentEar 的转写层之前，先去看 `ququ/` 的 `AGENTS.md` 和它的 Python 环境打包方式，能不能复用而不是重造。`huniu/`（本地语音助手）和 `whisper.cpp/` 也在同一片领域，选型时值得先扫一眼。
