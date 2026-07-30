# 里程碑

日期：2026-07-29
决策依据：`docs/asr-selection.md`（ASR 选型）、`docs/ingest-design.md`（接入层设计）

## 已拍板的三件事

1. **M1 只做到转写**，不含 LLM、不含标签路由、不含语音输出
2. **常驻守护进程用 Rust**（单二进制、无运行时，契合 ASR 的选型理由）
3. **先做丢弃式 Python spike** 跑基准测试，验证通过再写正式代码
4. **ASR 用 SenseVoiceSmall q8**（M0 横比后推翻了 Fun-ASR-Nano，见 `docs/decisions/0001-asr-model-selection.md`）

---

## M0 · ASR 基准验证（丢弃式 spike）

> ✅ **已完成（2026-07-29）。** 结果见 `docs/benchmarks.md`，决策见 `docs/decisions/0001-asr-model-selection.md`。
> 横比了 Fun-ASR-Nano / SenseVoiceSmall / Paraformer / Qwen3-ASR 四家：**CER 相差不到 1 个百分点，
> 差异全在工程形态**。最终选定 **SenseVoiceSmall q8**，推翻了初版的 Fun-ASR-Nano。

| | |
|---|---|
| 语言 | Python（**用完即删**，不进主工程） |
| 位置 | `spike/`（`.gitignore` 掉产物，只提交脚本和结论） |
| 依赖 | jason 提供的真实中文语料（本人口音 + 目标场景，含至少一段嘈杂环境） |

**要横比的模型**：Fun-ASR-Nano GGUF、Paraformer 流式、SenseVoice/ONNX、mlx-audio 的 Qwen3-ASR、FireRedASR2（离线兜底组）

**验收标准**（产出 `docs/benchmarks.md`，表格填满才算过）：

| 指标 | 门槛 |
|---|---|
| 模型实际体积 | `ls -l` 实测，澄清 484MB vs 800MB 之争 |
| CER | 同一语料横比，选出真正的第一名 |
| 峰值 / 稳态 RSS | **≤ 2 GB** |
| RTF | < 1.0 且有余量 |
| 长跑稳定性 | 连续 30–60 分钟无泄漏、无崩溃 |
| 热行为 | 持续负载下的降频记录 |

**如果 Fun-ASR-Nano 没过线**：按 CER 排名换下一个候选，重跑。选型本来就是暂定的。

---

## M1 · 按键即录即转写

> 「我对着 Mac 说话，快捷键打开开始录音，关闭停止，然后转成文字给我。」

| | |
|---|---|
| 语言 | Rust |
| 形态 | 菜单栏常驻程序（无主窗口） |

```
按下全局快捷键（toggle 开）
  ↓
cpal 采集 16kHz 单声道 PCM
  ↓
再次按下快捷键（toggle 关）
  ↓
raw/audio/<content_hash>  ← 完整提交协议，零丢失
  ↓
llama-funasr-sensevoice 子进程转写（冷启动仅 0.2s）
  ↓
derived/transcripts/
  ↓
文字进剪贴板 + 悬浮窗显示
```

### M1 恰好绕开了两个最难的部分

- **不需要 AEC**：没有 TTS 播放，麦克风不会收到自己的声音。AEC 推迟到 M3。
- **不需要 §3.7 的流式 raw 语义**：快捷键的按下/再按天然给出了段边界，每次录音就是一个有头有尾的文件对象，可以直接用 §3.3 的零丢失提交协议。无边界流推迟到 M3。

**这是选择「M1 只到转写」的额外红利，不要在 M1 里提前引入这两样。**

### macOS 上的两个已知坑

1. **麦克风权限（TCC）**：从终端直接跑的二进制会继承终端的权限，容易产生「我这儿能跑」的假象。M1 必须打成正经的 `.app` bundle，`Info.plist` 里写 `NSMicrophoneUsageDescription`。
2. **全局快捷键**：用 Carbon `RegisterEventHotKey`（Rust 侧走 `global-hotkey` crate）注册**不需要**辅助功能权限；如果改用 `NSEvent` 全局监听则需要，别走错路。

### 验收标准

> ✅ **8/8 全部通过（2026-07-30）。** 实测数据见 `docs/m1-status.md`。

- [x] 快捷键 toggle 录音，状态在菜单栏可见（右 Command；`🎙` / `● Ns` / `◌ 转写中`）
- [x] 录音文件按提交协议落进 `raw/audio/`，**杀进程测试**：未提交的临时对象被正确清理
      （单元测试 + 一次真实事故双重验证）
- [x] 转写结果进剪贴板，可直接粘贴，且已过滤特殊 token
- [x] 转写失败时 raw 音频**仍然完好**（实测：把模型文件头部写坏 → 优雅报错不 panic，
      raw 的 sha256 前后一致）
- [x] 常驻内存 ≤ 2 GB（实测空闲 13 MB；bundle 42 MB，多的是 AppKit）
- [x] 按键到开始录音 < 200ms（实测 146 ms）
- [x] `.app` bundle 可分发（`scripts/bundle.sh`）
- [x] 端到端全程 7.8s（录音 7.4s + 提交 28ms + 转写 0.26s）

**不在 M1 范围内**：LLM、标签路由、TTS、录音笔、网络传输、AEC。

---

## M2 · 理解与路由

转写之后接上标签识别，分流到 `routes/`。这是 README 里「说这是一个 idea / 这是一个任务，自动选分支」的那一段。

**M2 多了一项 M0 发现的职责：技术术语纠错。** 实测 ASR 会把 `raw` 转成 `road`、`knowledge base` 转成 `闹铃是base`，而 jason 的场景全是技术术语。用本地 LLM 结合上下文纠正，比换 ASR 模型划算。

**开工前必须先定**（现在都是空白）：
- 用哪个本地 LLM，以及它和 ASR 怎么分 2 GB 预算（很可能要改成「按需加载、用完卸载」）
- 标签词表：idea / task / note / question / …
- 下游「knowledge base」到底指什么——`mempalace_rust/`？`Seeder/`？还是本地目录？

**约束**：下游路由只消费 committed 的转写。

---

## M3 · 语音输出与打断

> 「可以打开语音模式。」

加 TTS，并实现打断式半双工。**这是 AEC 和流式 raw 语义真正登场的地方**，也是 M1–M2 之外工程量最大的一块。

验收标准直接用 `docs/asr-selection.md` §4 的那张带阈值的表（AEC 自触发率 = 0、打断响应 < 300ms、误打断 < 1次/10分钟、打断后恢复行为一致）。

**TTS 尚未选型。**

---

## M3.5 · 录音笔能力刻画

**机器一到手就做，不排队、不等前面的里程碑。**

执行 `docs/ingest-design.md` §1.0 的 9 项清单。这一步的结果可能推翻 M4/M5 的全部设计（比如发现它带 USB Audio Class，或者有 line-out 口可以直接接线）。**高风险未知项前置。**

---

## M4 · 录音笔接入

1. 土办法：`launchd` 监听挂载 → 稳定性检测 → 导入 `raw/`
2. 协议化：抽出 HTTP `/ingest` + 鉴权 + 提交协议，让接入源可替换

具体设计见 `docs/ingest-design.md` §3。

---

## M5 · 独立硬件（二选一或都做）

- **A**：Pi Zero 2 W 坞站（批量归档路径）
- **B**：ESP32-S3 + I2S MEMS 麦克风（实时路径，比改造录音笔更接近最终目标）

方案对比见 `docs/ingest-design.md` §1.2 和 §2.2。

---

## 依赖关系

```
M0 基准验证 ──→ M1 按键转写 ──→ M2 理解路由 ──→ M3 语音输出
                                                    │
M3.5 录音笔刻画（到手即做，不阻塞主线）──→ M4 接入 ──→ M5 硬件
```

**M0 是唯一的硬门槛**，其余里程碑之间可以按兴趣调整顺序，但 M3.5 必须在 M4 之前。
