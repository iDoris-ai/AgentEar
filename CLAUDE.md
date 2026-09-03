# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## 当前状态：M1 完成，**M2 代码完成并已发布（v0.4.0），默认关**

M2 = 术语纠错 + 一级标签识别 + `routes/` 落盘，需要一个本地 LLM 边车
（`scripts/setup-llm.sh` / `serve-llm.sh`，模型 7.8 GB **不随包分发**）。
边车的生命周期见 ADR-0002 §8：**连接优先、拉起兜底**，
`llm_autostart: false` 就是「只连不拉」的形态。

`routes/` 已经接上下游：**文件适配器**把每条记录渲染成 `kb/**/*.md`
（ADR-0003 §3.3 的 front matter），失败进 `routes/.pending/` 重试队列，
`--replay-kb` 可从 `routes/` 全量重建。**默认开**（`kb_enabled`，不需要任何外部依赖）。
ADR-0003 的**组织档适配器（memos）还没做**，等真有企业需求再定（ADR-0003 §6）。

**构建与测试**：

```bash
cargo build --release
cargo test                                    # 181 个测试：提交协议、崩溃语义、token 过滤、i18n、下载协议、知识库投递
./target/release/agentear                     # 守护进程，Ctrl+Shift+R 开始/停止录音
./target/release/agentear --transcribe x.wav  # 离线转写，不占麦克风，用于验证 ASR 链路
./target/release/agentear --diagnose          # 环境自检：权限、音频设备、ASR 依赖
./target/release/agentear --debug-keys        # 打印每个修饰键事件，排查按键问题
./target/release/agentear --fetch-thai        # 预下载泰语模型（574 MB），只装不改识别语言
./target/release/agentear --transcribe x.wav --lang th   # 不改配置试泰语链路
./target/release/agentear --classify "这是一个 idea"      # 给一段文字分类（评测脚本也走这条）
./target/release/agentear --replay-kb                    # 从 routes/ 全量重建 kb/，幂等，可反复跑
scripts/bundle.sh                             # 打 .app bundle → dist/
```

日志同时写 stderr 和 `~/.agentear/agentear.log`。

**macOS 上按键相关的三个坑**（症状都是「按了没反应」，见 `docs/m1-status.md`）：
主线程必须跑 AppKit/CFRunLoop 事件循环；`NSEvent` 全局监听在纯 CLI 二进制里回调
永不触发（用 `CGEventTap`）；修饰键的松开事件 keyCode 与按下相同，判据只能看
设备位 `NX_DEVICE_R_CMD (0x10)`。

**TCC 权限不会从终端带到 .app**：两者是独立主体，麦克风与辅助功能各自要授权一次。

数据落在 `~/.agentear/`（`AGENTEAR_DATA` 可覆盖）；ASR 二进制与模型在 `vendor/`（`AGENTEAR_VENDOR` 可覆盖，**不入库**）。

文档阅读顺序：`docs/milestones.md`（里程碑）→ `docs/decisions/`（决策记录，**选型结论以此为准**）→ `docs/benchmarks.md`（实测数据）→ `docs/ingest-design.md`（接入层设计）。`docs/asr-selection.md` 是初版调研，其中的选型结论**已被 ADR-0001 推翻**，仅作历史参考。

仓库在 GitHub 上（`git@github.com:jhfnetboy/AgentEar.git`，分支 `main`）。

### 已拍板（jason 2026-07-29 确认，不要重开讨论）

1. **M1 只做到转写** —— 快捷键 toggle 录音 → raw 落盘 → 转写 → 文字进剪贴板。不含 LLM、路由、TTS。
2. **常驻守护进程用 Rust** —— 单二进制、无运行时，契合 ASR 的选型理由。
3. **先做丢弃式 Python spike 跑 M0 基准** —— 「不用 Python」的约束针对常驻进程，不针对一次性测量脚本。spike 用完即删。

### 技术栈

- **LLM 已定为 `Ornith-1.0-9B` MLX 6bit**，经 mlx-dspark 提供 HTTP 服务，**常驻**。见 `docs/decisions/0002-m2-understanding-layer.md`。注意 GGUF 的 Q5_K_M 与 MLX 格式不兼容，MLX 侧的对应档是 6bit。
- **ASR 已定为 `SenseVoiceSmall q8` + FSMN-VAD**，走 `llama-funasr-sensevoice` 单二进制。**决策依据见 `docs/decisions/0001-asr-model-selection.md`，不要重开选型讨论。**
- **Fun-ASR-Nano 已被推翻**：四模型实测横比后，它是唯一在 30 分钟音频就爆 2 GiB 预算的（16 分钟即破），且冷启动 11.45s。SenseVoice 常驻仅 419 MB（Nano 的 27%）、冷启动 0.2s、术语命中反超。
- **Qwen3-ASR-0.6B 指标最好但出局**：需 Python + MLX 运行时。若将来出现 GGUF/纯 Rust 路径，应重新评估。
- **Paraformer 出局**：完全不输出标点。
- **Whisper 不做主链路**：中文 CER 远差于中文专用模型。但不要重复「差一个数量级」这个说法——那是混用了两种测试集口径。
- **复用 `ququ/` 仅限配置与思路层面**（模型选择、中文后处理经验），**不继承其运行时**。常驻进程背一个内嵌 Python 环境不划算。`huniu/`、`whisper.cpp/` 同理，是参考不是迁移源。
- 目标机器 **M1 Max / 64 GB**。
  **常驻内存预算：M1 阶段 ≤2 GiB；M2 起放宽到 ≤9 GiB**（引入常驻 LLM，见 ADR-0002）。
  放宽只针对 LLM 这一项，ASR 侧仍按原标准要求。
- **「不背 Python 运行时」的精确措辞**（ADR-0002 修订）：**Rust 守护进程自身**不内嵌
  Python 运行时；**外部推理服务**的实现语言不受限，但必须是独立进程 + 明确协议边界
  （可独立重启、独立崩溃）。M2 的 mlx-dspark 就是这样的边车。

### M0 实测产出的硬约束

**SenseVoiceSmall q8 实测关键数字**：权重 242 MiB + VAD 1.6 MiB、**冷启动 0.2s**、RSS 419 MB(2min) / 1.27 GiB(30min)、RTF 0.030–0.033。

1. **冷启动 0.2 秒 → M1 不需要常驻模型服务。** 每次录音直接 `Command::new()` 调 `llama-funasr-sensevoice` 子进程即可，无需链接 C++ 库、无需 server 模式。（这是换掉 Fun-ASR-Nano 的直接红利——Nano 冷启动要 11.45s，必须常驻。）
2. **长音频仍需分段送入 ASR，单段建议 ≤5 分钟。** RSS 随音频长度增长 0.52 MB/s，54 分钟破 2 GiB。影响 `ingest-design.md` 的**路径 A**；M1 的快捷键录音不受影响。
3. **特殊 token 需过滤，但 `--keep-tags` 必须开着。** 已实测（2026-08-22）：转写走 stdout、日志走 stderr，多个 VAD 段拼在同一行，默认不泄漏 `/sil`。**`asr.rs` 靠 `<|zh|>`/`<|en|>` 标记的存在与否区分「转写结果」和「日志」，所以不能去掉 `--keep-tags`。** 绝不要退回「按有没有汉字判断」——那会把英/日/韩/泰的结果整段丢掉（已修，见 `docs/m1-status.md`）。
4. **中英混杂技术术语不可靠。** 实测 `raw` 一词四个模型全错（row/road/ro/roll）；Docker → `doocca`、Kubernetes → `cuubber needs`。**术语纠错是 M2 的职责**，不要指望换 ASR 解决。
5. **语种支持边界（实测）**：中文 ✅、英文 ✅、中英混合的中文部分 ✅。**泰语 ❌**。
   **`llama-funasr-sensevoice` 的语种集合里根本没有 `th`**（只有 zh/en/yue/ja/ko/nospeech），
   所以它永远不可能把音频标成泰语——两次实测分别误判成 `<|en|>` 和 `<|yue|>`。
   这排除了「拿 SenseVoice 的语种标记当泰语路由依据」这一条路线（**但推不出
   「只能用户显式选择」**——显式菜单是产品决策，不是实测结论，见 ADR-0004 §1）。
   `src/asr.rs::thai_is_not_a_sensevoice_language` 钉住这个事实。
6. **泰语引擎见 `docs/decisions/0004-thai-asr-engine.md`（已临时选定 `distill` q5_0，
   v0.3.1 落地）**。⚠️ 是「先用起来、拿到语料再复评」的默认值，不是终局；
   换模型 = 改 `src/download.rs` 四个常量 + 重跑 `scripts/build-thai-model.sh`。
   已完成：
   三个 Whisper 系泰语微调（Thonburian medium / Thonburian distil-large-v3 /
   typhoon-whisper-turbo）已转 GGML 并量化，跑在现有 whisper.cpp 上、不引入新运行时。
   FLEURS 泰语 test 上实测 CER（n=80 条录音，含自助法 CI）：Thonburian 两个约
   6.1–6.5%、typhoon-turbo 约 9.5%。六组**事后固定的探索性比较**里只有两组
   检出差异，都是 Thonburian 优于 turbo；**但 Thonburian 的模型卡声明训练用过
   FLEURS，比较不中立**。q5_0 与 q8_0 **未检出**准确率差异
   （「未检出」不是「等效」——没有预定义非劣界）。
   f16 档因峰值 1.84–1.90 GB 出局。**选型卡在 code-switch 数据**——FLEURS 没有夹英文，
   而那正是实际场景。模型**按需下载**不随包分发（jason 2026-08-22 拍板），
   q5_0 约 540–575 MB。
7. **⚠️ 「长音频必须分段、单段 ≤5 分钟」是 SenseVoice 的约束，在 whisper 路径上
   状态是「未验证」，不是「不适用」。** SenseVoice 的 RSS 每秒涨 0.52 MB；
   whisper.cpp 在 11–98 秒区间只涨 18–40 MB，但**98 秒外推不到几十分钟**，
   要测 5/15/30/60 分钟才能下结论。见 ADR-0004 §3。

### M1 的两个红利，不要提前破坏

选择「M1 只到转写」让 M1 恰好绕开了两个最难的部分：

- **不需要 AEC**（没有 TTS 播放，麦克风不会收到自己的声音）
- **不需要流式 raw 语义**（快捷键的按下/再按天然给出段边界，每次录音就是一个有头有尾的文件对象，可直接用 `ingest-design.md` §3.3 的零丢失提交协议）

**不要在 M1 里提前引入 TTS 或无边界流**，那会把这两块难度一起拽进来。它们属于 M3。

### 存储语义（已定，不要在实现时重新解释）

`raw/audio/` = **ASR 之前**的原始字节，丢了不可重建；`derived/transcripts/` = 模型输出，可重算；`routes/` = 下游决策，可重算；`kb/` = 人读的 Markdown 文档层，**可从 `routes/` 全量重放**（`--replay-kb`）。

**分层的分界线是「能不能从语音重算」，不是「存在哪」**（ADR-0003 §7）：L0 事实层（raw+derived+routes，音频不可重建）→ L1 文档层（`kb/`，可重放）→ L2 索引层（还没做，可重建）→ **L3 行动层（任务/日程，带用户后来改的状态，不可重放）**。L3 不能塞进 Markdown 文件树，否则重放会覆盖用户改过的状态。**原始音频的持久化不得依赖任何下游步骤成功**——这是 README「先存后分流」的执行点。

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
