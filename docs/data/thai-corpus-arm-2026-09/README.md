# 泰语 code-switch 语料（Arm，2026-09-02 录制）

**这批语料解开的是 [ADR-0004](../../decisions/0004-thai-asr-engine.md) 的卡点**，原话：

> 选型卡在 code-switch 数据 —— FLEURS 没有夹英文，而那正是实际场景。

加上 Thonburian 的模型卡自承训练用过 FLEURS（比较不中立），换一份中立的
code-switch 语料正是泰语引擎复评需要的东西。

---

## 有什么

| 组 | 条数 | 时长 | 英文技术词 | 作用 |
|---|---|---|---|---|
| `devops` | 8 | 36.6s | 23 个 | code-switch 主力（deploy / Docker / Kubernetes / CI …） |
| `daily_collaboration` | 8 | 31.3s | 16 个 | code-switch 日常（meeting / version / Slack …） |
| `control_thai_only` | 6 | 24.5s | **0** | **对照组** |
| **合计** | **22** | **92.4s** | 39 个 | |
| `impromptu` | 1 | 184s | 未知 | **无参考文本**，见下 |

**对照组是这批数据最值钱的部分。** 有它才能把两种病分开：

- 纯泰语 CER 高 → **模型泰语本身就差**
- 纯泰语 CER 低、code-switch CER 高 → **一夹英文就崩**

没有对照组，一个高 CER 说明不了是哪一种，也就没法据此选模型。

## 文件

| 文件 | 是什么 |
|---|---|
| `reference.tsv` | **评分标准答案**。每行一条：take / 组 / 时长 / SNR / 英文词数 / sha256 前 16 位 / 文本 |
| `*.manifest.txt` | 录音工具的原始导出，含采集参数与质量指标 |
| `thai-corpus.sha256` | 25 个 wav 的**全量 sha256** |

## ⚠️ 音频不在仓库里

`.wav` 约 25 MB（22 条 8 MB + impromptu 17 MB），**刻意不入库**。

音频在 `/Users/jason/Dev/tools/wav/`（jason 的机器）。`thai-corpus.sha256`
是**确认「手里的文件就是这批」的唯一依据** —— 换机器、从别处拷回来，
先跑一次：

```bash
cd /path/to/wav && shasum -a 256 -c /path/to/thai-corpus.sha256
```

**参考文本和音频对不上，整份评测就没有意义**，所以这份校验和比音频本身更该留在版本控制里。

## 质量（录音工具实测，非请求值）

- 48 kHz 单声道 16-bit
- **AEC / NS / AGC 全部关闭** —— `getSettings()` 实报的 `aec=false ns=false agc=false`
- SNR **20.4 – 33.1 dB**，峰值 0.12 – 0.22（偏小但不削波）
- 丢帧一律 0.005s（`timeline:` 行）
- 单一说话人，安静房间，Chrome / macOS

## ⚠️ 样本量：够检出大差异，检不出小差异

**22 条 ≈ 92 秒。** 对比 ADR-0004 那轮 FLEURS 用的是 **n=80**。

- n=22 的置信区间会宽得多。如果几个模型的 CER 差在几个百分点以内，
  这份数据**大概率判不出高下** —— 那时结论应该写「**未检出差异**」，
  而不是「等效」（没有预定义非劣界）。
- **单一说话人**：结论的措辞只能是「Arm 这个说话人」，不能是「泰语用户」。
- 复评时应把 `control_thai_only` 与两个 code-switch 组**分别**算 CER，
  而不是混在一起给一个总分。

## 文本的来源与一处偏差

请求文档（[`../thai-recording-request.md`](../thai-recording-request.md)）明确
邀请 Arm 把非母语者写的泰语改写成自然泰语。实际情况：

- **20 条逐字照原文念**
- **2 条有小改动** —— 例如把 `release version ใหม่` 改成 `ปล่อย version ใหม่`
  （用泰语词替掉了英文词）

**后果**：材料里可能仍带着非母语者的措辞。但这**不影响 CER 的有效性** ——
`reference.tsv` 记的是他**实际念的**那一版，录音工具把两者绑在一起。
唯一的实际影响是那 2 条的英文密度略低于设计值，而英文密度正是被测的轴。

## `impromptu` 那 184 秒能干什么、不能干什么

- ❌ **算不了 CER** —— 没有参考文本。
- ✅ 可以测**长音频的 RTF 与内存增长**。ADR-0004 §3 明说 whisper 路径
  「98 秒之外是未验证」，这条能把验证范围翻倍 —— 但离 §3 要求的
  **5 / 15 / 30 / 60 分钟**还差得远，**不要拿它当那一项的结论**。

## 复评时怎么用

```bash
# 逐条转写（换 --lang / 换模型重复）
while IFS=$'\t' read -r take cat dur snr en sha text; do
  [ "$take" = take ] && continue
  agentear --transcribe "/path/to/wav/arm_thai_$cat/$take.wav" --lang th
done < reference.tsv
```

按 [`single-measurement-is-not-a-conclusion`] 的要求：**每个模型至少连跑 3 次**，
温度为 0 也不等于完全确定。
