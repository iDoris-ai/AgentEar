# TTS 方言可行性摸底（T3.1.1）

日期：2026-09-02 · 状态：**调研中，尚无结论**
目的：回答**一个**问题 —— 台湾话（闽南语）、粤语的本地 TTS 到底**有没有**可用方案。
不回答「哪个更好」，那是选型，等有了「有」的答案再说。

约束来自 `docs/milestones.md` M3：输出语音要能说台湾话、广东话、香港话、英语、泰语，
可先做普通话或英语。以及全局的隐私红线：**任何需要把文本或音频送第三方服务器的方案直接出局**。

---

## 候选表

三栏不许留空。「本地可跑」指**在 M1 Max / Apple Silicon 上不依赖 CUDA 能跑**——
这是本项目的目标机器，不是泛指「能自托管」。

| 候选 | 方言覆盖 | 本地可跑（Apple Silicon） | 许可 | 证据来源 |
|---|---|---|---|---|
| [Qwen3-TTS](https://github.com/QwenLM/Qwen3-TTS) | 10 语言 + **仅北京话、四川话**两种中文方言 | ✅ **能** —— 官方 README 只给 CUDA，但 [mlx-audio](https://github.com/Blaizzy/mlx-audio) 已收录它，有 [macOS 实操记录](https://mybyways.com/blog/qwen3-tts-with-mlx-audio-on-macos) | **Apache-2.0** | 官方 README + mlx-audio |
| [MERaLiON-OmniVoice-Hokkien-TTS](https://huggingface.co/MERaLiON/MERaLiON-OmniVoice-Hokkien-TTS) | **新加坡闽南语**（非台湾腔） | ❓ 示例同样是 `device_map="cuda:0"` | `meralion-3-public-licence`（**自定义许可，非标准开源，再分发义务待核**） | HF 模型卡 |
| BreezyVoice-Taigi | **台湾闽南语**（台语） | ❓ 待查 | 待查 | [arXiv 2603.19259](https://arxiv.org/pdf/2503.19259)（Breeze Taigi 论文） |
| [CosyVoice 2](https://github.com/FunAudioLLM/CosyVoice) | 中英日韩粤等，粤语待核实 | ❓ 待查 | 待查（Apache-2.0 系？待核） | 上游仓库 |

## 目前浮现的格局（这是本次调研最要紧的一张图）

```
        能在 Apple Silicon 上跑          支持台湾话/粤语
              ↓                              ↓
      ┌───────────────┐              ┌───────────────┐
      │  Qwen3-TTS    │              │ BreezyVoice-  │
      │  (via mlx-    │   ← 交集？ →  │ Taigi         │
      │   audio)      │    目前是空    │ MERaLiON      │
      │  Kokoro 等    │               │ Hokkien       │
      └───────────────┘              └───────────────┘
        方言只有京/川                  推理示例全是 CUDA
```

**两个集合目前没有已确认的交集。** 这不是最终结论（T3.1.2 要去验证右边那些能不能在
Mac 上跑通），但它决定了 T3.1.2 该往哪使劲：**不是去比较谁质量好，
而是去验证「右边那两个到底能不能在 Apple Silicon 上跑起来」**。

`mlx-audio` 是最有希望的桥：它已经把 Qwen3-TTS、Kokoro、OmniVoice 等搬上了 MLX。
`MERaLiON-OmniVoice-Hokkien` 名字里就带 OmniVoice —— **它是不是 mlx-audio 已支持的
OmniVoice 的微调版？如果是，Mac 可跑性可能已经解决了。** 这是 T3.1.2 的第一优先项。

## 已经能说的三件事（证据充分，不是猜测）

### 1. 最干净的那个不覆盖需求

`Qwen3-TTS` 是候选里许可最省事的（Apache-2.0，和我们托管泰语模型时的经验一致，
再分发义务最轻）。但它公布的中文方言**只有北京话和四川话** ——
**粤语和闽南语都不在内**。许可干净解决不了覆盖不到的问题。

### 2. 台湾闽南语和新加坡闽南语不是一回事

`MERaLiON` 那个模型卡写的是 **Singapore Hokkien**，训练与评测都围绕新加坡语境
（还特别提到支持马来语/英语借词）。jason 要的是**台湾话**。
两者同属闽南语系但腔调、借词、常用词差异明显，**不能拿一个当另一个用**。
真正对口的是 `BreezyVoice-Taigi`（基于 CosyVoice 2，约 10000 小时合成台语数据）。

### 3. 所有候选都是 PyTorch + CUDA 取向，没有一个是「单二进制」形态

四个候选的推理示例清一色 `device_map="cuda:0"`，**没有任何一个提到 MLX、GGUF、
Apple Silicon 或纯 CPU**。这跟 ASR 侧的处境完全不同——那边有
`llama-funasr-sensevoice` 和 `whisper.cpp` 这类单二进制可用。

对本项目的含义：**TTS 大概率要走 ADR-0002 允许的「独立进程边车」路线**
（就像 M2 的 mlx-dspark），而不可能像 ASR 那样嵌成一个 Rust 调用的子进程。
这不违反约束，但它是**新增的一整套运行时**，成本要算进 M3 的工程量。

## 还没答的（T3.1.2 要做的）

1. **BreezyVoice-Taigi 的许可与可得性** —— 这是唯一对口台湾话的候选，
   它能不能用几乎决定了「有没有」这个问题的答案。
2. **CosyVoice 2 的粤语支持是否真实**，以及它在 Apple Silicon 上能不能跑。
3. ~~**有没有 MLX 移植**~~ —— **已查**：`mlx-audio` 支持 Qwen3-TTS / Kokoro /
   OmniVoice / Voxtral / CSM / Dia，Apple Silicon 原生。
   **剩下的关键问题变成**：`MERaLiON-OmniVoice-Hokkien` 是不是 mlx-audio 已支持的
   OmniVoice 的微调版？若是，闽南语的 Mac 可跑性可能已经有解。
4. 实测：挑 1–2 个最有希望的，真下下来合成一句，记冷启动 / RTF / 内存 / 产物可听性。
   **跑不通也是结论**。

## 一条要提前说清的话

如果 T3.1.2 的结论是「台湾话没有可在 Apple Silicon 上跑的方案」，
**那不等于 M3 失败**，而是要在三条路里选（这是 jason 的决定，不是我的）：

- 只做普通话 + 英语，方言推迟到有方案时再说
- 为 TTS 单独引入一个 CUDA 机器（与「全本地、便宜硬件」的初衷冲突）
- 推迟整个 M3，先做 M2b / M3.5

这三条的代价会在 `docs/decisions/0005-tts-selection.md`（T3.1.3）里逐条列清楚。
